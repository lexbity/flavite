//! Embedded reading edits retain the source identity and refresh parent readers.
#[path = "../../flavite-api/tests/support/links.rs"]
mod support;
use flavite_api::{EmbedContent, ReadSnapshot};
use flavite_ui::{
    app::{App, Message},
    host::reading,
};
use flavite_workspace::{NoteLocator, TabId};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
fn wait(app: &mut App, predicate: impl Fn(&App) -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = app.update(Message::Pump);
        if predicate(app) {
            return;
        }
        assert!(Instant::now() < until, "{}", app.status_text());
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn open(app: &mut App, path: &str) -> TabId {
    let _ = app.update(Message::OpenNote {
        locator: NoteLocator::from_components([path]).unwrap(),
        group: None,
    });
    wait(app, |app| !app.notes.any_pending());
    app.workspace.focused_tab()
}
fn child(doc: &ReadSnapshot) -> Arc<ReadSnapshot> {
    doc.embeds
        .values()
        .find_map(|e| {
            if let EmbedContent::Note(child) = e {
                Some(child.clone())
            } else {
                None
            }
        })
        .unwrap()
}
#[test]
fn embedded_task_edits_only_its_own_open_buffer_and_parent_reader_refreshes() {
    let f = support::fixture(&[
        ("a.md", "# A\n![[b]]\n"),
        ("b.md", "# B\n- [ ] embedded task\n![[a]]\n"),
    ]);
    let _cache = f.cache;
    let mut app = App::from_flavite(f.flavite);
    let b = open(&mut app, "b.md");
    let a = open(&mut app, "a.md");
    let original = app.note_state_text(a).unwrap();
    let _ = app.update(Message::ToggleNoteMode(a));
    wait(&mut app, |app| reading::document(app, a).is_some());
    let root = reading::document(&app, a).unwrap().clone();
    let embedded = child(&root);
    assert!(
        embedded
            .embeds
            .values()
            .any(|e| matches!(e,EmbedContent::Unavailable(message) if message.contains("cycle")))
    );
    let (buffer, revision) = embedded.buffer.unwrap();
    let start = embedded.text.find("[ ]").unwrap();
    let _=app.update(Message::StructureEdit{tab:a,document:embedded.clone(),command:"task.toggle",args:serde_json::json!({"buffer_id":buffer,"revision":revision,"start":start,"checked":false})});
    wait(&mut app, |app| {
        app.pending_operations() == 0
            && app
                .note_state_text(b)
                .is_some_and(|text| text.contains("[x]"))
    });
    wait(&mut app, |app| {
        reading::document(app, a).is_some_and(|doc| child(doc).text.contains("[x]"))
    });
    assert_eq!(app.note_state_text(a).unwrap(), original);
    assert!(child(reading::document(&app, a).unwrap()).unsaved);
    let before = app.note_state_text(b).unwrap();
    // The old embedded callback cannot be replayed into the refreshed resource.
    let _=app.update(Message::StructureEdit{tab:a,document:embedded,command:"task.toggle",args:serde_json::json!({"buffer_id":buffer,"revision":revision,"start":start,"checked":false})});
    assert_eq!(app.note_state_text(b).unwrap(), before);
    let _ = flavite_ui::workspace_shell::view(&app);
}
