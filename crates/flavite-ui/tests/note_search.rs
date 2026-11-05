//! Literal note Find/Replace (RFC-0006 S01): per-view state, revision-keyed
//! match pages, display-only find navigation, and core-driven replacement
//! through `text.prepare_replace` + `text.apply_replace`.

#[path = "../../flavite-api/tests/support/links.rs"]
mod support;
use flavite_ui::{
    app::{App, Message},
    host::{note_search, reading},
};
use flavite_workspace::{Arrangement, LayoutAction, NoteLocator, TabId};
use std::time::{Duration, Instant};

use note_search::NoteSearchMessage;

fn wait(app: &mut App, done: impl Fn(&App) -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = app.update(Message::Pump);
        if done(app) {
            return;
        }
        assert!(Instant::now() < until, "{}", app.status_text());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn fixture(notes: &[(&str, &str)]) -> App {
    App::from_flavite(support::fixture(notes).flavite)
}

fn open(app: &mut App, locator: &NoteLocator, group: Option<flavite_workspace::GroupId>) -> TabId {
    let before: Vec<TabId> = app.workspace.tabs().map(|(tab, _)| *tab).collect();
    let _ = app.update(Message::OpenNote {
        locator: locator.clone(),
        group,
    });
    wait(app, |a| !a.notes.any_pending());
    app.workspace
        .tabs()
        .map(|(tab, _)| *tab)
        .find(|tab| !before.contains(tab))
        .unwrap_or_else(|| app.workspace.focused_tab())
}

/// Open the same note twice via a split: SplitGroup duplicates the active
/// note view into a second group sharing the same buffer (W03).
fn duplicate(app: &mut App, locator: &NoteLocator) -> (TabId, TabId) {
    let first = open(app, locator, None);
    let group = app.focused_group().unwrap();
    let _ = app.update(Message::Layout(LayoutAction::SplitGroup {
        expected_revision: app.workspace.revision(),
        group,
        arrangement: Arrangement::SideBySide,
    }));
    wait(app, |a| !a.notes.any_pending());
    let second = app
        .workspace
        .tabs()
        .map(|(tab, _)| *tab)
        .find(|tab| {
            *tab != first
                && app.notes.locator_of(*tab).as_ref() == Some(locator)
                && app
                    .notes
                    .state(tab)
                    .is_some_and(|state| state.buffer.is_some())
        })
        .expect("duplicate view opened");
    (first, second)
}

fn spans(app: &App, tab: TabId) -> Vec<(usize, usize)> {
    note_search::matches(app, tab).map_or_else(Vec::new, |page| page.spans)
}

fn find(app: &mut App, tab: TabId, query: &str) {
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindOpen(tab)));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindQuery {
        tab,
        query: query.to_owned(),
    }));
}

#[test]
fn duplicate_views_keep_independent_find_state_and_rescan_after_edits() {
    let mut app = fixture(&[("a.md", "alpha beta alpha gamma alpha\n")]);
    let locator = NoteLocator::from_components(["a.md"]).unwrap();
    let (first, second) = duplicate(&mut app, &locator);
    assert_eq!(
        app.note_editor_view(first).unwrap().buffer_id,
        app.note_editor_view(second).unwrap().buffer_id,
        "both views share one buffer"
    );
    // The legacy Message::Find* variants route into the same host.
    let _ = app.update(Message::FindOpen(first));
    let _ = app.update(Message::FindQuery {
        tab: first,
        query: "alpha".into(),
    });
    find(&mut app, second, "beta");
    assert_eq!(spans(&app, first).len(), 3);
    assert_eq!(spans(&app, second).len(), 1);
    assert_eq!(app.note_search.finds[&first].index, 0);
    assert_eq!(app.note_search.finds[&second].index, 0);
    // Navigation indices move independently per view.
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindNext {
        tab: first,
        forward: true,
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindNext {
        tab: first,
        forward: true,
    }));
    assert_eq!(app.note_search.finds[&first].index, 2);
    assert_eq!(app.note_search.finds[&second].index, 0);
    // An edit from one view bumps the shared revision; the other view's
    // next scan invalidates its revision-keyed page and reflects the text.
    let view = app.note_editor_view(second).unwrap();
    let api = app.flavite.as_ref().unwrap();
    assert!(
        api.execute(flavite_api::Invocation::new(
            api.command_registry().next_request_id(),
            "apply_edits",
            serde_json::json!({
                "buffer_id": view.buffer_id,
                "revision": view.revision,
                "ops": [{ "range": [0, 0], "replacement": "xalpha " }],
            }),
            flavite_api::Origin::Gui,
        ))
        .is_success()
    );
    assert_eq!(spans(&app, first).len(), 4, "first view rescans new text");
    assert_eq!(spans(&app, second).len(), 1, "second view unchanged");
}

#[test]
fn match_page_reports_230_matches_once_and_cycles_the_index() {
    let mut app = fixture(&[("many.md", "ab ".repeat(230).as_str())]);
    let tab = open(
        &mut app,
        &NoteLocator::from_components(["many.md"]).unwrap(),
        None,
    );
    find(&mut app, tab, "ab");
    let page = note_search::matches(&app, tab).expect("find page present");
    assert_eq!(page.spans.len(), 230);
    assert!(!page.truncated);
    let starts: std::collections::BTreeSet<usize> = page.spans.iter().map(|(at, _)| *at).collect();
    assert_eq!(starts.len(), 230, "each occurrence exactly once");
    assert_eq!(page.spans[0], (0, 2));
    assert_eq!(page.spans[229], (687, 689));
    // FindQuery selects the first match for display; 230 Next presses wrap
    // back to the start of the cycle.
    for _ in 0..230 {
        let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindNext {
            tab,
            forward: true,
        }));
    }
    assert_eq!(app.note_search.finds[&tab].index, 0);
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindNext {
        tab,
        forward: true,
    }));
    assert_eq!(app.note_search.finds[&tab].index, 1);
    // Backwards navigation wraps through the end of the page.
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindNext {
        tab,
        forward: false,
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindNext {
        tab,
        forward: false,
    }));
    assert_eq!(app.note_search.finds[&tab].index, 229);
}

#[test]
fn find_navigation_sets_display_selection_without_editor_edits() {
    let mut app = fixture(&[("a.md", "one two one\n")]);
    let tab = open(
        &mut app,
        &NoteLocator::from_components(["a.md"]).unwrap(),
        None,
    );
    let view = app.note_editor_view(tab).unwrap();
    let revision = view.revision;
    let text = view.text.clone();
    find(&mut app, tab, "one");
    // The find selection is display state on the workspace interaction.
    let selection = app
        .workspace
        .tab(&tab)
        .unwrap()
        .interaction()
        .selections
        .first()
        .copied()
        .expect("display selection set");
    assert_eq!((selection.anchor, selection.head), (0, 3));
    assert_eq!(app.reading.pending_anchor.get(&tab), Some(&0));
    assert!(
        app.workspace.writing_target().is_none(),
        "find navigation never authorizes writing"
    );
    // Legacy FindNext message keeps the same contract.
    let _ = app.update(Message::FindNext { tab, forward: true });
    let selection = app
        .workspace
        .tab(&tab)
        .unwrap()
        .interaction()
        .selections
        .first()
        .copied()
        .unwrap();
    assert_eq!((selection.anchor, selection.head), (8, 11));
    let after = app.note_editor_view(tab).unwrap();
    assert_eq!(after.revision, revision, "no editor edit was emitted");
    assert_eq!(after.text, text);
}

#[test]
fn replace_all_prepares_through_the_core_and_updates_both_views() {
    let mut app = fixture(&[("a.md", "aaa bbb aaa\n")]);
    let locator = NoteLocator::from_components(["a.md"]).unwrap();
    let (first, second) = duplicate(&mut app, &locator);
    let before = app.note_editor_view(second).unwrap().revision;
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ReplaceOpen(first)));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ReplaceQuery {
        tab: first,
        query: "aaa".into(),
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ReplaceText {
        tab: first,
        text: "zz".into(),
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ReplaceAll(first)));
    wait(&mut app, |a| a.pending_operations() == 0);
    assert_eq!(
        app.note_state_text(first).unwrap(),
        "zz bbb zz\n",
        "buffer text replaced through the core"
    );
    let replace = app.note_search.replaces.get(&first).unwrap();
    assert_eq!(replace.match_count, Some(2));
    assert!(replace.plan_id.is_some());
    assert!(replace.pending.is_none());
    assert!(replace.last_error.is_none());
    // The duplicate view shares the buffer: it sees the new revision.
    let second_view = app.note_editor_view(second).unwrap();
    assert_eq!(second_view.text.as_ref(), "zz bbb zz\n");
    assert_ne!(second_view.revision, before, "revision advanced");
    wait(&mut app, |a| {
        crate_replace_visible(a, first) && crate_replace_visible(a, second)
    });
}

fn crate_replace_visible(app: &App, tab: TabId) -> bool {
    app.note_editor_view(tab).is_some_and(|view| {
        app.note_buffer_state(tab) == Some(flavite_api::BufferStatus::Dirty)
            || view.text.contains("zz")
    })
}

#[test]
fn close_while_preparation_runs_drops_the_stale_completion_without_effects() {
    let mut app = fixture(&[("a.md", "aaa bbb aaa\n")]);
    let locator = NoteLocator::from_components(["a.md"]).unwrap();
    let tab = open(&mut app, &locator, None);
    let original = app.note_state_text(tab).unwrap();
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ReplaceOpen(tab)));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ReplaceQuery {
        tab,
        query: "aaa".into(),
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ReplaceText {
        tab,
        text: "zz".into(),
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ReplaceAll(tab)));
    assert!(
        app.note_search.replaces[&tab].pending.is_some(),
        "preparation is in flight"
    );
    // Close the (clean) tab while the worker still runs.
    let _ = app.update(Message::RequestCloseTab(tab));
    assert!(app.workspace.tab(&tab).is_none(), "tab closed");
    wait(&mut app, |a| a.pending_operations() == 0);
    assert!(
        app.note_search.preparations.is_empty(),
        "the pending mapping was dropped"
    );
    // Reopen: the plan never applied to the buffer.
    let _ = app.update(Message::OpenNote {
        locator,
        group: None,
    });
    wait(&mut app, |a| !a.notes.any_pending());
    let reopened = app.workspace.focused_tab();
    assert_eq!(app.note_state_text(reopened).unwrap(), original);
}

#[test]
fn find_works_in_read_mode() {
    let mut app = fixture(&[("a.md", "# Heading\n\nParagraph one.\n")]);
    let tab = open(
        &mut app,
        &NoteLocator::from_components(["a.md"]).unwrap(),
        None,
    );
    let _ = app.update(Message::ToggleNoteMode(tab));
    wait(&mut app, |app| reading::document(app, tab).is_some());
    find(&mut app, tab, "paragraph");
    assert_eq!(spans(&app, tab).len(), 1, "case-insensitive default (D-05)");
    let selection = app
        .workspace
        .tab(&tab)
        .unwrap()
        .interaction()
        .selections
        .first()
        .copied()
        .unwrap();
    assert_eq!((selection.anchor, selection.head), (11, 20));
}

#[test]
fn empty_query_is_idle_without_counts_or_navigation() {
    let mut app = fixture(&[("a.md", "some text\n")]);
    let tab = open(
        &mut app,
        &NoteLocator::from_components(["a.md"]).unwrap(),
        None,
    );
    find(&mut app, tab, "");
    let page = note_search::matches(&app, tab).expect("find bar open");
    assert!(
        page.spans.is_empty(),
        "empty query is Idle, not match-everywhere"
    );
    let before = app
        .workspace
        .tab(&tab)
        .unwrap()
        .interaction()
        .selections
        .first()
        .copied();
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindNext {
        tab,
        forward: true,
    }));
    let after = app
        .workspace
        .tab(&tab)
        .unwrap()
        .interaction()
        .selections
        .first()
        .copied();
    assert_eq!(before, after, "no navigation happened");
    assert_eq!(app.note_search.finds[&tab].index, 0);
}
