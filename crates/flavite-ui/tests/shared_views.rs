//! Shared note buffers with independent logical editor state (RFC-0003 W03).

use flavite_api::{Flavite, VaultConfig};
use flavite_buffer::manager::BufferManager;
use flavite_editor::{EditorAction, EditorInteractionSnapshot};
use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_ui::app::{App, Message};
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use flavite_workspace::{LayoutAction, Selection, TabId};
use std::time::{Duration, Instant};

fn fixture() -> (App, MemFs, tempfile::TempDir) {
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/shared-views");
    fs.create_dir_all(&root).unwrap();
    fs.write_new(&root.join("note.md"), b"alpha\nbeta\n")
        .unwrap();
    fs.write_new(&root.join("source.md"), b"See [[target]].\n")
        .unwrap();
    fs.write_new(&root.join("target.md"), b"Target note.\n")
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("shared-index.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let identity = flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root, vec![7, 3, 1]);
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
    let flavite = Flavite::open(identity, session, VaultConfig::default()).unwrap();
    (App::from_flavite(flavite), fs, cache)
}

fn wait_for_open(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.notes.any_pending() && Instant::now() < deadline {
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!app.notes.any_pending(), "note opens complete");
}

fn acknowledge_caret(app: &mut App, tab: TabId, byte: usize, sequence: u64) {
    let incarnation = app.notes.incarnation(tab).unwrap();
    let view = app.note_editor_view(tab).unwrap();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(EditorInteractionSnapshot {
            sequence,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret: byte,
            selection: (byte, byte),
            viewport_anchor: 0,
            focused: true,
            composing: false,
        }),
    });
}

#[test]
fn split_views_share_revision_text_and_history_but_keep_independent_carets() {
    let (mut app, fs, _cache) = fixture();
    let group = app.focused_group().unwrap();
    let locator = flavite_workspace::NoteLocator::from_components(["note.md"]).unwrap();
    let _ = app.update(Message::OpenNote {
        locator,
        group: Some(group),
    });
    wait_for_open(&mut app);
    let first = app.workspace.focused_tab();
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    wait_for_open(&mut app);
    let second = app.workspace.focused_tab();
    assert_ne!(first, second);
    assert_eq!(
        app.notes.state(&first).unwrap().buffer,
        app.notes.state(&second).unwrap().buffer
    );

    acknowledge_caret(&mut app, first, 1, 1);
    acknowledge_caret(&mut app, second, 7, 1);
    assert_eq!(app.workspace.document_anchor(), Some(second));
    assert_eq!(app.workspace.writing_target().unwrap().tab, second);
    let first_view = app.note_editor_view(first).unwrap();
    let second_view = app.note_editor_view(second).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first_view.text, &second_view.text));
    assert_eq!(
        app.workspace.tab(&first).unwrap().interaction().selections,
        vec![Selection::caret(1)]
    );
    assert_eq!(
        app.workspace.tab(&second).unwrap().interaction().selections,
        vec![Selection::caret(7)]
    );

    let edit = EditorAction::replace(
        &first_view,
        first_view.text.len(),
        first_view.text.len(),
        "!".into(),
    );
    let incarnation = app.notes.incarnation(first).unwrap();
    let _ = app.update(Message::EditorAction {
        tab: first,
        incarnation,
        action: edit,
    });
    assert_eq!(
        app.note_editor_view(first).unwrap().text.as_ref(),
        "alpha\nbeta\n!"
    );
    let after_edit_first = app.note_editor_view(first).unwrap();
    let after_edit_second = app.note_editor_view(second).unwrap();
    assert!(std::sync::Arc::ptr_eq(
        &after_edit_first.text,
        &after_edit_second.text
    ));
    assert!(
        app.workspace.writing_target().is_none(),
        "the old target revision is invalidated"
    );

    let undo = EditorAction::history(&after_edit_second, false);
    let incarnation = app.notes.incarnation(second).unwrap();
    let _ = app.update(Message::EditorAction {
        tab: second,
        incarnation,
        action: undo,
    });
    assert_eq!(
        app.note_editor_view(first).unwrap().text.as_ref(),
        "alpha\nbeta\n"
    );
    let _ = app.update(Message::RunHostCommand("note.save"));
    let save_deadline = Instant::now() + Duration::from_secs(5);
    while app.pending_operations() > 0 && Instant::now() < save_deadline {
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(app.pending_operations(), 0, "save completion is correlated");
    let path = std::path::Path::new("/test/shared-views/note.md");
    let bytes = fs.read_if_exists(path).unwrap().unwrap();
    assert_eq!(bytes, b"alpha\nbeta\n");
    assert_eq!(
        app.workspace.tab(&first).unwrap().interaction().selections,
        vec![Selection::caret(1)]
    );
    assert_eq!(
        app.workspace.tab(&second).unwrap().interaction().selections,
        vec![Selection::caret(7)]
    );

    // Closing one view does not release the shared Session buffer.
    let _ = app.update(Message::Layout(LayoutAction::CloseTab {
        expected_revision: app.workspace.revision(),
        tab: second,
    }));
    assert!(app.notes.state(&first).unwrap().buffer.is_some());
}

#[test]
fn editor_link_navigation_resolves_through_the_api_catalog() {
    let (mut app, _fs, _cache) = fixture();
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    let reconcile_deadline = Instant::now() + Duration::from_secs(5);
    while app.pending_operations() > 0 && Instant::now() < reconcile_deadline {
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
    let _ = app.update(Message::RefreshCatalog);
    assert!(!app.catalog().is_empty(), "canonical catalog is ready");
    let group = app.focused_group().unwrap();
    let locator = flavite_workspace::NoteLocator::from_components(["source.md"]).unwrap();
    let _ = app.update(Message::OpenNote {
        locator,
        group: Some(group),
    });
    wait_for_open(&mut app);
    let source = app.workspace.focused_tab();
    let buffer_id = app.notes.state(&source).unwrap().buffer.unwrap();
    let incarnation = app.notes.incarnation(source).unwrap();
    let _ = app.update(Message::EditorAction {
        tab: source,
        incarnation,
        action: EditorAction::Navigate {
            buffer_id,
            expected_revision: 0,
            navigation: flavite_editor::LinkNavigation::note("target", 4, 14),
        },
    });
    wait_for_open(&mut app);
    let opened = app.workspace.focused_tab();
    assert_eq!(app.workspace.tab(&opened).unwrap().title(), "target");
    assert!(app.notes.state(&opened).unwrap().buffer.is_some());
}

#[test]
fn blur_acknowledgements_preserve_only_an_unchanged_authorized_caret() {
    let (mut app, _fs, _cache) = fixture();
    let group = app.focused_group().unwrap();
    let _ = app.update(Message::OpenNote {
        locator: flavite_workspace::NoteLocator::from_components(["note.md"]).unwrap(),
        group: Some(group),
    });
    wait_for_open(&mut app);
    let tab = app.workspace.focused_tab();
    acknowledge_caret(&mut app, tab, 2, 1);
    let original = app.workspace.writing_target().unwrap();
    let mut blur = app.editor_interaction_snapshot(tab).unwrap();
    blur.sequence += 1;
    blur.focused = false;
    let incarnation = app.notes.incarnation(tab).unwrap();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(blur),
    });
    let _ = app.update(Message::Pump);
    let retained = app.workspace.writing_target().unwrap();
    assert_eq!(retained.tab, original.tab);
    assert_eq!(retained.revision, original.revision);
    assert_eq!(retained.caret_text_byte, 2);
    assert_eq!(retained.interaction_sequence, blur.sequence);
    // A changed selection while unfocused does not silently reauthorize it.
    blur.sequence += 1;
    blur.selection = (3, 3);
    blur.caret = 3;
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(blur),
    });
    assert!(app.workspace.writing_target().is_none());
}
