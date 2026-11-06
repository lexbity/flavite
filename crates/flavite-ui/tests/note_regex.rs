//! S03 controls expose regex/case/whole-word options through the shared
//! note-search host; no matcher or regex parser lives in the UI.
#[path = "../../flavite-api/tests/support/links.rs"]
mod support;

use flavite_ui::{
    app::{App, Message},
    host::note_search::{self, NoteSearchMessage},
};
use flavite_workspace::{NoteLocator, TabId};

fn open(app: &mut App) -> TabId {
    let _ = app.update(Message::OpenIntent(
        flavite_ui::host::open_intents::OpenIntent::reuse(
            NoteLocator::from_components(["a.md"]).unwrap(),
            None,
        ),
    ));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let _ = app.update(Message::Pump);
        if !app.notes.any_pending() {
            break;
        }
        assert!(std::time::Instant::now() < until, "note open timed out");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    app.workspace.focused_tab()
}

#[test]
fn regex_and_match_options_are_per_view_and_revision_bound() {
    let mut app = App::from_flavite(support::fixture(&[("a.md", "Cat cat scatter\n")]).flavite);
    let tab = open(&mut app);
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindOpen(tab)));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindQuery {
        tab,
        query: "cat".into(),
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::SetMode {
        tab,
        mode: "regex".into(),
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ToggleCaseSensitive(
        tab,
    )));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::ToggleWholeWord(tab)));
    let state = &app.note_search.finds[&tab];
    assert_eq!(state.mode, "regex");
    assert!(state.case_sensitive);
    assert!(state.whole_word);
    // Literal empty query is Idle; regex empty remains a valid regex.
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::SetMode {
        tab,
        mode: "literal".into(),
    }));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindQuery {
        tab,
        query: String::new(),
    }));
    assert!(note_search::matches(&app, tab).is_some_and(|page| page.total_matches == 0));
}

#[test]
fn selection_scope_uses_the_pre_find_selection_and_does_not_widen() {
    let mut app = App::from_flavite(support::fixture(&[("a.md", "cat scatter cat\n")]).flavite);
    let tab = open(&mut app);
    let view = app.note_editor_view(tab).unwrap();
    // Select the final occurrence before Find takes focus. The find
    // selection shown afterward must not become a new scope authority.
    app.workspace
        .set_display_selection(tab, 12, 15, view.revision, &view.text)
        .unwrap();
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindOpen(tab)));
    let _ = app.update(Message::NoteSearch(
        NoteSearchMessage::ToggleSelectionScope(tab),
    ));
    let _ = app.update(Message::NoteSearch(NoteSearchMessage::FindQuery {
        tab,
        query: "cat".into(),
    }));
    let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let _ = app.update(Message::Pump);
        if note_search::matches(&app, tab).is_some() {
            break;
        }
        assert!(
            std::time::Instant::now() < until,
            "selection-scoped find timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let found = note_search::matches(&app, tab).unwrap();
    assert_eq!(found.spans, vec![(12, 15)]);
    assert_eq!(found.total_matches, 1);
}
