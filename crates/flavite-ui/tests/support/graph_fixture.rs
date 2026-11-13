#![allow(dead_code)]
//! Real UI/Session/index fixture shared by context and graph verification.
use flavite_api::{Flavite, VaultConfig};
use flavite_buffer::manager::BufferManager;
use flavite_editor::{EditorAction, EditorInteractionSnapshot};
use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_ui::app::{App, Message};
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use flavite_workspace::{NoteLocator, TabId, ViewKind};
use std::time::{Duration, Instant};

pub fn fixture() -> (App, MemFs, tempfile::TempDir) {
    fixture_with(&[
        ("source.md", "See [[target]] here.\n"),
        ("other.md", "See [[target]] too.\n"),
        ("target.md", "Target note.\n"),
    ])
}

/// The same real UI/Session/index fixture over caller-supplied notes.
pub fn fixture_with(notes: &[(&str, &str)]) -> (App, MemFs, tempfile::TempDir) {
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/context-routing");
    fs.create_dir_all(&root).unwrap();
    for (name, body) in notes {
        let path = root.join(name);
        if let Some(parent) = path.parent()
            && parent != root
        {
            fs.create_dir_all(parent).unwrap();
        }
        fs.write_new(&path, body.as_bytes()).unwrap();
    }
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("context-index.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let identity = flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root, vec![5, 1, 7]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .build()
        .unwrap()
        .start()
        .unwrap();
    let channels = ChannelSet::new();
    let session = Session::new(
        identity.clone(),
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig::default(),
    );
    let mut flavite = Flavite::open(identity, session, VaultConfig::default()).unwrap();
    flavite.set_graph_db_path_for_test(db);
    (App::from_flavite(flavite), fs, cache)
}

/// Pump until `predicate` holds on the app, with a hard deadline.
pub fn wait_for(app: &mut App, what: &str, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    let _debugged = false;
    while !predicate(app) {
        if Instant::now() >= deadline {
            let tool = app.workspace.tabs().find_map(|(tab, session)| {
                matches!(
                    session.kind(),
                    ViewKind::Backlinks(_) | ViewKind::Preview(_)
                )
                .then_some(*tab)
            });
            eprintln!(
                "DEBUG what={what} tool={tool:?} state={:?} binding={:?} in_flight={} any_pending={}",
                tool.and_then(|tab| app.contexts.state(&tab)),
                tool.and_then(|tab| app.workspace.binding_of(&tab)),
                app.flavite
                    .as_ref()
                    .map(|f| f.query_service().in_flight())
                    .unwrap_or(999),
                app.contexts.any_pending(),
            );
            panic!("timed out waiting for {what}");
        }
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
}

pub fn open_note(app: &mut App, name: &str) -> TabId {
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator: NoteLocator::from_components([name]).unwrap(),
        group,
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.notes.any_pending() && Instant::now() < deadline {
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
    let tab = app.workspace.focused_tab();
    // A real acknowledged caret interaction establishes the document
    // anchor (D-04/D-07: opening alone is not a writing caret).
    let incarnation = app.notes.incarnation(tab).unwrap();
    let view = app.note_editor_view(tab).unwrap();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(EditorInteractionSnapshot {
            sequence: 1,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret: 0,
            selection: (0, 0),
            selections: vec![[0, 0, 0]],
            primary: 0,
            viewport_anchor: 0,
            focused: true,
            composing: false,
        }),
    });
    tab
}
