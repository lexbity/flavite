//! Link activation opens the target note in both modes (manual-UX:
//! point-and-click navigation). Read mode must navigate without requiring
//! the snapshot's catalog version to still be current; the editor path
//! must resolve, decode, and open through canonical routing.
#[path = "../../flavite-api/tests/support/links.rs"]
mod support;
use flavite_ui::{
    app::{App, Message},
    host::reading,
};
use flavite_workspace::{NoteLocator, TabId};
use std::time::{Duration, Instant};

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

fn fixture() -> (App, tempfile::TempDir) {
    let f = support::fixture(&[("a.md", "See [[b]] here.\n"), ("b.md", "# B\nBody\n")]);
    (App::from_flavite(f.flavite), f.cache)
}

fn open(app: &mut App, name: &str) -> TabId {
    let _ = app.update(Message::OpenNote {
        locator: NoteLocator::from_components([name]).unwrap(),
        group: None,
    });
    wait(app, |a| !a.notes.any_pending());
    app.workspace.focused_tab()
}

fn tab_of(app: &App, name: &str) -> Option<TabId> {
    app.workspace
        .tabs()
        .find_map(|(tab, session)| {
            matches!(session.kind(), flavite_workspace::ViewKind::Note(note)
        if note.locator() == &NoteLocator::from_components([name]).unwrap())
            .then_some(tab)
        })
        .copied()
}

fn b_ready(app: &App) -> bool {
    tab_of(app, "b.md").is_some_and(|tab| app.note_editor_view(tab).is_some())
}

#[test]
fn read_mode_link_activation_opens_target_note() {
    let (mut app, _cache) = fixture();
    let tab = open(&mut app, "a.md");
    let _ = app.update(Message::ToggleNoteMode(tab));
    wait(&mut app, |app| reading::document(app, tab).is_some());
    let doc = reading::document(&app, tab).unwrap().clone();
    let (start, path) = doc
        .links
        .iter()
        .find_map(|(start, link)| match link {
            flavite_api::ReadLink::Note { path, .. } => Some((*start, path.clone())),
            _ => None,
        })
        .expect("the rendered snapshot carries the [[b]] link");
    assert_eq!(path.to_string_lossy(), "b.md");
    // The strict edit gate accepts a fresh snapshot too; navigation uses
    // the relaxed gate (ptr identity), pinned here so a future tightening
    // of `current()` cannot silently re-enter navigation.
    assert!(reading::bound_navigation(&app, tab, &doc));
    let opened = app.update(Message::ReadActivate {
        tab,
        document: doc,
        start,
    });
    let _ = opened;
    wait(&mut app, b_ready);
    assert!(
        b_ready(&app),
        "Read-mode link activation must open b.md: {}",
        app.status_text()
    );
    assert_eq!(app.status_text(), app.status_text());
}

#[test]
fn editor_navigate_action_opens_target_note() {
    let (mut app, _cache) = fixture();
    let tab = open(&mut app, "a.md");
    let view = app.note_editor_view(tab).expect("open note has a view");
    let incarnation = app.notes.incarnation(tab).expect("attached view");
    // Byte range of the "[[b]]" occurrence in a.md; only used to correlate
    // the gesture, not to authorize the open.
    let mut navigation = flavite_editor::LinkNavigation::note("b", 4, 9);
    navigation.create_if_unresolved = true;
    let action = flavite_editor::EditorAction::open_link(&view, navigation);
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action,
    });
    wait(&mut app, b_ready);
    assert!(
        b_ready(&app),
        "editor Navigate must resolve and open b.md: {}",
        app.status_text()
    );
}
