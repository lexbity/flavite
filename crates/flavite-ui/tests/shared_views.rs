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
use flavite_workspace::{CommittedSelection, LayoutAction, TabId};
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
    acknowledge_caret_with_anchor(app, tab, byte, 0, sequence);
}

fn acknowledge_caret_with_anchor(
    app: &mut App,
    tab: TabId,
    byte: usize,
    viewport_anchor: usize,
    sequence: u64,
) {
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
            selections: vec![[0, byte, byte]],
            viewport_anchor,
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
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator,
        group,
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

    // Independent carets and independent scroll anchors in one shared buffer.
    acknowledge_caret_with_anchor(&mut app, first, 1, 0, 1);
    acknowledge_caret_with_anchor(&mut app, second, 7, 5, 1);
    assert_eq!(app.workspace.document_anchor(), Some(second));
    assert_eq!(app.workspace.writing_target().unwrap().tab, second);
    let first_view = app.note_editor_view(first).unwrap();
    let second_view = app.note_editor_view(second).unwrap();
    assert!(std::sync::Arc::ptr_eq(&first_view.text, &second_view.text));
    assert_eq!(
        app.workspace.tab(&first).unwrap().interaction().selections,
        vec![CommittedSelection::caret(1)]
    );
    assert_eq!(
        app.workspace.tab(&second).unwrap().interaction().selections,
        vec![CommittedSelection::caret(7)]
    );
    assert_eq!(
        app.workspace
            .tab(&first)
            .unwrap()
            .interaction()
            .viewport_anchor,
        0
    );
    assert_eq!(
        app.workspace
            .tab(&second)
            .unwrap()
            .interaction()
            .viewport_anchor,
        5,
        "each view keeps its own scroll anchor"
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
    // Redo from the other view restores the same one-edit boundary.
    let after_undo = app.note_editor_view(second).unwrap();
    let redo = EditorAction::history(&after_undo, true);
    let incarnation = app.notes.incarnation(second).unwrap();
    let _ = app.update(Message::EditorAction {
        tab: second,
        incarnation,
        action: redo,
    });
    assert_eq!(
        app.note_editor_view(first).unwrap().text.as_ref(),
        "alpha\nbeta\n!",
        "redo restores the shared edit"
    );
    // Undo again before the final save so the disk bytes match the original.
    let after_redo = app.note_editor_view(second).unwrap();
    let undo_again = EditorAction::history(&after_redo, false);
    let incarnation = app.notes.incarnation(second).unwrap();
    let _ = app.update(Message::EditorAction {
        tab: second,
        incarnation,
        action: undo_again,
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
        vec![CommittedSelection::caret(1)]
    );
    assert_eq!(
        app.workspace.tab(&second).unwrap().interaction().selections,
        vec![CommittedSelection::caret(7)]
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
    // RFC-0004 W-D: ticketed acquisition — the page lands under pumping.
    let catalog_deadline = Instant::now() + Duration::from_secs(10);
    while app.catalog().is_empty() {
        assert!(
            Instant::now() < catalog_deadline,
            "canonical catalog never arrived: {:?}",
            app.catalog_status()
        );
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
    let group = app.focused_group().unwrap();
    let locator = flavite_workspace::NoteLocator::from_components(["source.md"]).unwrap();
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator,
        group,
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
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator: flavite_workspace::NoteLocator::from_components(["note.md"]).unwrap(),
        group,
    });
    wait_for_open(&mut app);
    let tab = app.workspace.focused_tab();
    acknowledge_caret(&mut app, tab, 2, 1);
    let original = app.workspace.writing_target().unwrap();
    let mut blur = app.editor_interaction_snapshot(tab).unwrap();
    blur.sequence += 1;
    blur.focused = false;
    let blur_sequence = blur.sequence;
    let incarnation = app.notes.incarnation(tab).unwrap();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(blur.clone()),
    });
    let _ = app.update(Message::Pump);
    let retained = app.workspace.writing_target().unwrap();
    assert_eq!(retained.tab, original.tab);
    assert_eq!(retained.revision, original.revision);
    assert_eq!(retained.caret_text_byte, 2);
    assert_eq!(retained.interaction_sequence, blur_sequence);
    // A changed selection while unfocused does not silently reauthorize it.
    let mut blur = app.editor_interaction_snapshot(tab).unwrap();
    blur.sequence = blur_sequence + 1;
    blur.focused = false;
    blur.selection = (3, 3);
    blur.selections = vec![[0, 3, 3]];
    blur.caret = 3;
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(blur),
    });
    assert!(app.workspace.writing_target().is_none());
}

#[path = "support/editor_latency.rs"]
mod editor_latency;

#[test]
fn queued_navigation_and_word_edits_follow_acknowledged_carets_without_redraw() {
    use flavite_editor::{
        TypeMetrics, input::CapturedInput, keymap::KeyAction, navigation::VerticalMotion,
    };
    let (mut app, fs, _cache) = fixture();
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|revision| LayoutAction::OpenNote {
        expected_revision: revision,
        locator: flavite_workspace::NoteLocator::from_components(["note.md"]).unwrap(),
        group,
    });
    wait_for_open(&mut app);
    let tab = app.workspace.focused_tab();
    acknowledge_caret(&mut app, tab, 0, 100);
    let source = app.note_editor_view(tab).unwrap();
    let incarnation = app.notes.incarnation(tab).unwrap();
    // All these captures saw the same widget snapshot. Each must be
    // interpreted against the previous acknowledgement, never that old caret.
    for (sequence, action, rows, expected) in [
        (101, KeyAction::Down, 1, 6),
        (102, KeyAction::Down, 1, 11),
        (103, KeyAction::Up, -1, 6),
    ] {
        let _ = app.update(Message::EditorInput {
            tab,
            incarnation,
            input: CapturedInput {
                buffer: source.buffer_id,
                observed_revision: source.revision,
                sequence,
                action,
                text: String::new(),
                extend: false,
                document_boundary: false,
                vertical: Some(VerticalMotion {
                    rows,
                    width: 500.0,
                    metrics: TypeMetrics::default(),
                }),
            },
        });
        assert_eq!(
            app.workspace.tab(&tab).unwrap().interaction().selections,
            vec![CommittedSelection::caret(expected)]
        );
        assert_eq!(
            app.note_editor_view(tab).unwrap().revision,
            source.revision,
            "navigation cannot mutate text"
        );
    }
    let _ = app.update(Message::EditorInput {
        tab,
        incarnation,
        input: CapturedInput {
            buffer: source.buffer_id,
            observed_revision: source.revision,
            sequence: 104,
            action: KeyAction::DeleteWord,
            text: String::new(),
            extend: false,
            document_boundary: false,
            vertical: None,
            preferred_x: Vec::new(),
        },
    });
    assert_eq!(
        app.note_editor_view(tab).unwrap().text.as_ref(),
        "alpha\n\n"
    );
    let _ = app.update(Message::EditorInput {
        tab,
        incarnation,
        input: CapturedInput {
            buffer: source.buffer_id,
            observed_revision: source.revision,
            sequence: 105,
            action: KeyAction::Insert,
            text: "雪".into(),
            extend: false,
            document_boundary: false,
            vertical: None,
            preferred_x: Vec::new(),
        },
    });
    assert_eq!(
        app.note_editor_view(tab).unwrap().text.as_ref(),
        "alpha\n雪\n"
    );
    assert_eq!(
        fs.read_if_exists(&app.root().join("note.md"))
            .unwrap()
            .unwrap(),
        source.text.as_bytes()
    );
    let current = app.note_editor_view(tab).unwrap();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::history(&current, false),
    });
    assert!(
        !app.note_editor_view(tab).unwrap().text.contains('雪'),
        "undo remains Session-owned"
    );
}

#[test]
fn home_then_vertical_keys_ignore_the_widgets_old_column_before_redraw() {
    use flavite_editor::{
        TypeMetrics, input::CapturedInput, keymap::KeyAction, navigation::VerticalMotion,
    };
    let (mut app, _fs, _cache) = fixture();
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|expected_revision| LayoutAction::OpenNote {
        expected_revision,
        group,
        locator: flavite_workspace::NoteLocator::from_components(["note.md"]).unwrap(),
    });
    wait_for_open(&mut app);
    let tab = app.workspace.focused_tab();
    let view = app.note_editor_view(tab).unwrap();
    let incarnation = app.notes.incarnation(tab).unwrap();
    acknowledge_caret(&mut app, tab, view.text.len(), 100);
    let _ = app.update(Message::EditorInput {
        tab,
        incarnation,
        input: CapturedInput {
            buffer: view.buffer_id,
            observed_revision: view.revision,
            sequence: 101,
            action: KeyAction::Home,
            text: String::new(),
            extend: false,
            document_boundary: true,
            vertical: None,
            preferred_x: Vec::new(),
        },
    });
    for (sequence, action, rows, expected) in [
        (102, KeyAction::Down, 1, 6),
        (103, KeyAction::Down, 1, 11),
        (104, KeyAction::Up, -1, 6),
    ] {
        let _ = app.update(Message::EditorInput {
            tab,
            incarnation,
            input: CapturedInput {
                buffer: view.buffer_id,
                observed_revision: view.revision,
                sequence,
                action,
                text: String::new(),
                extend: false,
                document_boundary: false,
                // Deliberately stale widget geometry: Home's new caret was not
                // yet drawn when these events were captured.
                vertical: Some(VerticalMotion {
                    rows,
                    width: 500.0,
                    metrics: TypeMetrics::default(),
                }),
            },
        });
        assert_eq!(
            app.workspace.tab(&tab).unwrap().interaction().selections,
            vec![CommittedSelection::caret(expected)]
        );
        assert_eq!(app.note_editor_view(tab).unwrap().text, view.text);
    }
}
