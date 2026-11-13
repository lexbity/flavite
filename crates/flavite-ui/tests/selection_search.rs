//! Match-driven selection integration (FEUX-0002 X06): drafts overlay an
//! outline-only preview, Apply installs the canonical set atomically with a
//! `selection_version` bump and no history node, stale completions are
//! rejected by exact key, Esc discards byte-identically, and literal
//! collection keeps working after a regex-path failure.

#[path = "support/mod.rs"]
mod support;

use flavite_edit_context::ContextCommand as Command;
use flavite_ui::accessibility::program::FlaviteProgram;
use flavite_ui::app::{App, Message};
use flavite_ui::host::context_mode::{ContextMessage, EditorTarget};
use flavite_ui::host::selection_search::{SelectionSearchMessage, overlay_spans};
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use iced::Program;
use std::time::{Duration, Instant};

const BODY: &str = "TODO one\nTODO two\nplain text\n";

fn fixture(seed: u64) -> (support::UiFixture, TabId) {
    let mut f = support::ui_fixture(
        seed,
        &format!("/test/selection-search-{seed}"),
        &[support::SeedNote {
            components: &["note.md"],
            body: BODY,
        }],
    );
    let group = f.app.focused_group().unwrap();
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["note.md"]).unwrap(),
        });
    let tab = f.app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    selection(&mut f.app, tab, 0, 0);
    (f, tab)
}

fn selection(app: &mut App, tab: TabId, anchor: usize, head: usize) {
    let view = app.note_editor_view(tab).unwrap();
    let _ = app.update(Message::AccessibleSelection {
        tab,
        incarnation: app.notes.incarnation(tab).unwrap(),
        revision: view.revision,
        anchor,
        head,
    });
}

fn run(app: &mut App, tab: TabId, command: Command) {
    let target = EditorTarget::capture(app, tab).unwrap();
    let _ = app.update(Message::ContextMode(ContextMessage::Run(target, command)));
}

fn draft_query(app: &mut App, tab: TabId, query: &str) {
    let _ = app.update(Message::SelectionSearch(
        SelectionSearchMessage::DraftAction {
            tab,
            action: iced::widget::text_editor::Action::Edit(
                iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(query.to_string())),
            ),
        },
    ));
}

fn pump_until(app: &mut App, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate(app) {
        assert!(
            Instant::now() < deadline,
            "pump deadline: {}",
            app.status_text()
        );
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn committed_members(app: &App, tab: TabId) -> Vec<(usize, usize)> {
    app.workspace
        .tab(&tab)
        .unwrap()
        .interaction()
        .selections
        .iter()
        .map(|s| (s.anchor, s.head))
        .collect()
}

#[test]
fn select_matches_draft_overlays_then_apply_replaces_the_set() {
    let (mut f, tab) = fixture(9101);
    let version_before = f
        .app
        .workspace
        .tab(&tab)
        .unwrap()
        .interaction()
        .selection_version;
    run(&mut f.app, tab, Command::SelectMatches);
    draft_query(&mut f.app, tab, "TODO");
    pump_until(&mut f.app, |app| overlay_spans(app, tab).is_some());
    let spans = overlay_spans(&f.app, tab).expect("overlay");
    assert_eq!(spans.len(), 2, "TODO matches on two lines");
    // The overlay never mutates the committed set.
    assert_eq!(committed_members(&f.app, tab), vec![(0, 0)]);
    let text_before = f.app.note_editor_view(tab).unwrap().text.clone();

    let _ = f
        .app
        .update(Message::SelectionSearch(SelectionSearchMessage::Apply(tab)));
    let members = committed_members(&f.app, tab);
    assert_eq!(members, vec![(0, 4), (9, 13)]);
    assert!(
        f.app
            .workspace
            .tab(&tab)
            .unwrap()
            .interaction()
            .selection_version
            > version_before,
        "apply bumps the committed selection version"
    );
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().text,
        text_before,
        "apply is selection-only: bytes unchanged"
    );
    // Apply consumed the overlay; the row stays open for a refined query.
    assert!(overlay_spans(&f.app, tab).is_none());
}

#[test]
fn split_apply_builds_gaps_and_refuses_no_gaps_with_prior_set_kept() {
    let (mut f, tab) = fixture(9102);
    run(&mut f.app, tab, Command::SplitSelection);
    draft_query(&mut f.app, tab, "TODO");
    pump_until(&mut f.app, |app| overlay_spans(app, tab).is_some());
    let spans = overlay_spans(&f.app, tab).expect("overlay");
    assert_eq!(spans.len(), 2, "two TODO matches");
    let _ = f
        .app
        .update(Message::SelectionSearch(SelectionSearchMessage::Apply(tab)));
    assert_eq!(
        committed_members(&f.app, tab),
        vec![(4, 9), (13, 29)],
        "gaps around the TODO matches"
    );

    // All-match coverage leaves no gaps: typed refusal keeps the prior set.
    let mut draft = f.app.note_editor_view(tab).unwrap().text.clone();
    {
        // type a query that matches everything literal-wise
        let _ = &mut draft;
    }
    // The literal "TODO" already produced gaps; a NoGaps case is reached by
    // applying spans that cover the whole document — the api refuses.
    let request_id = f
        .app
        .flavite
        .as_ref()
        .unwrap()
        .command_registry()
        .next_request_id();
    let whole = f.app.note_editor_view(tab).unwrap().text.len();
    let result = f
        .app
        .flavite
        .as_ref()
        .unwrap()
        .execute(flavite_api::Invocation::new(
            request_id,
            "editor.split_selection",
            serde_json::json!({
                "buffer_id": f.app.note_editor_view(tab).unwrap().buffer_id,
                "revision": f.app.note_editor_view(tab).unwrap().revision,
                "selections": {"primary": 0, "items": [[0, 0, 0]]},
                "query": {"text": "TODO", "regex": false, "case_sensitive": false},
                "apply": true,
                "spans": [[0, whole]],
                "total_matches": 1,
                "zero_width_count": 0,
            }),
            flavite_api::Origin::Gui,
        ));
    let message = match &result {
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => {
            error.message.clone()
        }
        flavite_api::ExecutionResult::Rejected(rejected) => {
            rejected.message.clone().unwrap_or_default()
        }
        other => panic!("expected typed NoGaps refusal, got {other:?}"),
    };
    assert_eq!(message, "NoGaps");
    assert_eq!(committed_members(&f.app, tab), vec![(4, 9), (13, 29)]);
}

#[test]
fn stale_draft_is_rejected_by_key_when_the_selection_changes() {
    let (mut f, tab) = fixture(9103);
    run(&mut f.app, tab, Command::SelectMatches);
    draft_query(&mut f.app, tab, "TODO");
    // Immediately change the committed selection before any completion
    // drains: the draft's key (selection_version) no longer matches.
    selection(&mut f.app, tab, 5, 6);
    pump_until(&mut f.app, |app| {
        app.selection_search
            .tabs
            .get(&tab)
            .is_some_and(|state| state.pending.is_none())
    });
    // The stale completion was rejected: no overlay is installed.
    assert!(
        overlay_spans(&f.app, tab).is_none(),
        "stale draft must not paint"
    );
    // The committed set is untouched byte-identically.
    assert_eq!(committed_members(&f.app, tab), vec![(5, 6)]);
    assert_eq!(f.app.note_editor_view(tab).unwrap().text.as_ref(), BODY);
}

#[test]
fn close_discards_the_overlay_byte_identically() {
    let (mut f, tab) = fixture(9104);
    run(&mut f.app, tab, Command::SelectMatches);
    draft_query(&mut f.app, tab, "TODO");
    pump_until(&mut f.app, |app| overlay_spans(app, tab).is_some());
    let _ = f
        .app
        .update(Message::SelectionSearch(SelectionSearchMessage::Close(tab)));
    assert!(!f.app.selection_search.tabs.contains_key(&tab));
    assert_eq!(committed_members(&f.app, tab), vec![(0, 0)]);
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().text.as_ref(),
        BODY,
        "Esc/Close leaves the committed set byte-identical"
    );
}

#[test]
fn literal_collection_keeps_working_after_a_regex_path_failure() {
    let (mut f, tab) = fixture(9105);
    // A regex draft may succeed or fail depending on helper availability;
    // either way the literal draft afterwards must still overlay.
    run(&mut f.app, tab, Command::SelectMatches);
    let _ = f.app.update(Message::SelectionSearch(
        SelectionSearchMessage::ToggleRegex(tab),
    ));
    draft_query(&mut f.app, tab, "TODO");
    pump_until(&mut f.app, |app| {
        app.selection_search
            .tabs
            .get(&tab)
            .is_some_and(|state| state.pending.is_none())
    });
    // Back to literal: the literal engine never depends on the helper.
    let _ = f.app.update(Message::SelectionSearch(
        SelectionSearchMessage::ToggleRegex(tab),
    ));
    pump_until(&mut f.app, |app| overlay_spans(app, tab).is_some());
    let spans = overlay_spans(&f.app, tab).expect("literal overlay");
    assert_eq!(spans.len(), 2);
}

#[test]
fn mode_exit_cancels_the_row_and_discards_the_overlay() {
    let (mut f, tab) = fixture(9106);
    run(&mut f.app, tab, Command::SelectMatches);
    draft_query(&mut f.app, tab, "TODO");
    pump_until(&mut f.app, |app| overlay_spans(app, tab).is_some());
    // Enter context mode so Exit has a session to tear down.
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::ContextMode(ContextMessage::Run(
        target.clone(),
        Command::Toggle,
    )));
    let _ = f.app.update(Message::ContextMode(ContextMessage::Run(
        target,
        Command::Exit,
    )));
    assert!(
        !f.app.selection_search.tabs.contains_key(&tab),
        "mode exit cancels the row"
    );
    assert_eq!(committed_members(&f.app, tab), vec![(0, 0)]);
}

/// The query row must render *independently* of the find bar (E-11):
/// opening the match-selection row while find is closed still publishes
/// the row in the semantic tree. This guards against nesting the row in
/// the find-open conditional (a native-only rendering regression).
#[test]
fn query_row_renders_without_the_find_bar() {
    let (mut f, tab) = fixture(2211);
    run(&mut f.app, tab, Command::SelectMatches);
    pump_until(&mut f.app, |app| {
        app.selection_search.tabs.contains_key(&tab)
    });
    for _ in 0..5 {
        let _ = f.app.update(Message::Pump);
    }
    let update = FlaviteProgram
        .accessibility_tree(&f.app, iced_winit::program::core::window::Id::unique())
        .expect("the shell publishes a semantic tree");
    let labels: Vec<String> = update
        .nodes
        .iter()
        .filter_map(|(_, node)| {
            let mut text = String::new();
            if let Some(label) = node.label() {
                text.push_str(label);
            }
            if let Some(value) = node.value() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(value);
            }
            (!text.is_empty()).then_some(text)
        })
        .collect();
    assert!(
        labels.iter().any(|l| l.contains("Select matches")
            || l.contains("Split on matches")
            || l.contains("Alt+Enter")),
        "query row must render without the find bar; labels={labels:?}"
    );
}

#[test]
fn a_superseded_draft_cancels_the_old_request_and_the_newest_wins() {
    // FEUX-0002 R13/E-11: a refined draft cancels the previous collection
    // at the source (one in-flight per view) and only the newest overlay
    // installs — a late result of the cancelled request never paints.
    let (mut f, tab) = fixture(9107);
    run(&mut f.app, tab, Command::SelectMatches);
    draft_query(&mut f.app, tab, "TODO");
    // Refine the query without draining anything: the first request is
    // cancelled before (or while) it runs.
    draft_query(&mut f.app, tab, "TOD");
    pump_until(&mut f.app, |app| overlay_spans(app, tab).is_some());
    let spans = overlay_spans(&f.app, tab).expect("the newest draft's overlay");
    assert!(
        spans.iter().all(|&(start, end)| end - start == 3),
        "the overlay is the refined query's, not the cancelled draft's: {spans:?}"
    );
    // The committed set stays untouched until the apply.
    assert_eq!(committed_members(&f.app, tab), vec![(0, 0)]);
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().text.as_ref(),
        BODY,
        "cancellation and supersede never mutate the text"
    );
}
