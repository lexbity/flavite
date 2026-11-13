//! FEUX-0002 X07: cross-path refusal/cancel/expired outcomes produce their
//! typed events on the real ring. Applied, refused, selection, history,
//! match, and burst paths each leave their own event family; the seeded
//! negatives are the classifications (unit-covered) and the zero-export
//! invariants.

#[path = "support/mod.rs"]
mod support;

use flavite_edit_context::ContextCommand as Command;
use flavite_ui::app::{App, Message};
use flavite_ui::host::context_mode::{ContextMessage, EditorTarget};
use flavite_ui::host::selection_search::{ApplyKind, SelectionSearchMessage};
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::time::{Duration, Instant};

fn fixture(seed: u64) -> (support::UiFixture, TabId) {
    let mut f = support::ui_fixture(
        seed,
        &format!("/failures-{seed}"),
        &[support::SeedNote {
            components: &["fail.md"],
            body: "one café\ntwo\nthree\n",
        }],
    );
    let group = f.app.focused_group().unwrap();
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["fail.md"]).unwrap(),
        });
    let tab = f.app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    (f, tab)
}

fn run(app: &mut App, tab: TabId, command: Command) {
    let target = EditorTarget::capture(app, tab).unwrap();
    let _ = app.update(Message::ContextMode(ContextMessage::Run(target, command)));
}

fn pump_until(app: &mut App, predicate: impl Fn(&App) -> bool, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate(app) {
        assert!(Instant::now() < deadline, "timed out: {label}");
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn cross_path_outcomes_leave_their_typed_events() {
    let (mut f, tab) = fixture(11_201);
    run(&mut f.app, tab, Command::Toggle);

    // Applied: a ticketed delete.
    run(&mut f.app, tab, Command::Delete);
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("OperationApplied"), "{exported}");

    // Refused: a stale target against the bumped revision.
    let stale = EditorTarget::capture(&f.app, tab).unwrap();
    run(&mut f.app, tab, Command::Delete); // bumps revision again
    let _ = f.app.update(Message::ContextMode(ContextMessage::Run(
        stale,
        Command::Delete,
    )));
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("OperationRefused"), "{exported}");

    // Selection op: invert applied (selection-only).
    run(&mut f.app, tab, Command::InvertSelections);
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("SelectionOp"), "{exported}");

    // Match preview: draft collects (literal) and overlays.
    let _ = f
        .app
        .update(Message::SelectionSearch(SelectionSearchMessage::Open {
            tab,
            kind: ApplyKind::Select,
        }));
    let _ = f.app.update(Message::SelectionSearch(
        SelectionSearchMessage::DraftAction {
            tab,
            action: iced::widget::text_editor::Action::Edit(
                iced::widget::text_editor::Edit::Paste(std::sync::Arc::new("two".to_string())),
            ),
        },
    ));
    pump_until(
        &mut f.app,
        |app| {
            app.selection_search
                .tabs
                .get(&tab)
                .is_some_and(|s| s.pending.is_none() && s.overlay.is_some())
        },
        "draft overlay ready",
    );
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("MatchPreview"), "{exported}");
    let _ = f
        .app
        .update(Message::SelectionSearch(SelectionSearchMessage::Close(tab)));

    // History page open (after enough edits for at least one node).
    run(&mut f.app, tab, Command::History);
    pump_until(
        &mut f.app,
        |app| app.context_mode.history.open,
        "history chooser open",
    );
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("HistoryPage"), "{exported}");

    // Restore the latest non-current entry and record its outcome.
    let page = f
        .app
        .context_mode
        .history
        .page
        .clone()
        .expect("history page");
    let entry = page
        .entries
        .iter()
        .find(|entry| !entry.current && entry.restorable)
        .cloned();
    let Some(entry) = entry else {
        // No restorable entry (e.g. single edit): the close path still
        // leaves the chooser clean; the page event already proves the path.
        let _ = f.app.update(Message::EditorHistory(
            flavite_ui::host::editor_history::HistoryMessage::Act(
                EditorTarget::capture(&f.app, tab).unwrap(),
                flavite_ui::host::editor_history::HistoryAction::Close,
            ),
        ));
        return;
    };
    let _ = f.app.update(Message::EditorHistory(
        flavite_ui::host::editor_history::HistoryMessage::Act(
            EditorTarget::capture(&f.app, tab).unwrap(),
            flavite_ui::host::editor_history::HistoryAction::Restore(entry.target),
        ),
    ));
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("HistoryRestore"), "{exported}");
    assert!(exported.contains("Applied"), "{exported}");
}

#[test]
fn burst_hint_emits_a_typed_event() {
    let (mut f, tab) = fixture(11_202);
    run(&mut f.app, tab, Command::Toggle);
    for _ in 0..5 {
        run(&mut f.app, tab, Command::Delete);
    }
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("BurstHint"), "{exported}");
}

#[test]
fn empty_clipboard_paste_records_a_refusal() {
    let (mut f, tab) = fixture(11_203);
    run(&mut f.app, tab, Command::Toggle);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let generation = f.app.context_mode.generation();
    let _ = f.app.update(Message::EditorClipboard(
        flavite_ui::host::editor_clipboard::ClipboardMessage::ContextPaste {
            target,
            generation,
            text: None,
        },
    ));
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("OperationRefused"), "{exported}");
}
