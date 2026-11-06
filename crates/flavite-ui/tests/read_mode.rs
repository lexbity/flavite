//! Mounted workspace/Session behavior and OS-action routing; native evidence is separate.
#[path = "../../flavite-api/tests/support/links.rs"]
mod support;
use flavite_ui::{
    app::{App, Message},
    host::reading,
};
use flavite_workspace::{ContextViewKind, LayoutAction, NoteLocator, NoteMode, ViewKind};
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
fn fixture() -> (
    App,
    std::sync::Arc<flavite_fs::MemFs>,
    std::path::PathBuf,
    tempfile::TempDir,
) {
    let f = support::fixture(&[
        (
            "a.md",
            "---\ntitle: 'A' # keep\ncount: 2\n---\n# Heading\n\nParagraph 雪.\n\n- [ ] task\n\n| A | B |\n|---|---|\n| x | y |\n\n![[b]]\n![[b]]\n",
        ),
        ("b.md", "# B\nBody\n"),
    ]);
    (App::from_flavite(f.flavite), f.fs, f.root, f.cache)
}
fn open(app: &mut App) -> flavite_workspace::TabId {
    let _ = app.update(Message::OpenIntent(
        flavite_ui::host::open_intents::OpenIntent::reuse(
            NoteLocator::from_components(["a.md"]).unwrap(),
            None,
        ),
    ));
    wait(app, |a| !a.notes.any_pending());
    app.workspace.focused_tab()
}
#[test]
fn same_tab_read_mode_current_dirty_text_guarded_task_undo_and_independent_duplicate_mode() {
    let (mut app, fs, root, _cache) = fixture();
    let tab = open(&mut app);
    let original = app.note_editor_view(tab).unwrap();
    let _ = app.workspace.set_content_anchor(
        tab,
        original.text.find("Paragraph").unwrap(),
        &original.text,
    );
    let _ = app.update(Message::ToggleNoteMode(tab));
    wait(&mut app, |app| reading::document(app, tab).is_some());
    let doc = reading::document(&app, tab).unwrap().clone();
    assert_eq!(doc.buffer, Some((original.buffer_id, original.revision)));
    assert!(
        matches!(app.workspace.tab(&tab).unwrap().kind(),ViewKind::Note(n) if n.mode()==NoteMode::Read)
    );
    assert!(app.valid_writing_target().is_none());
    assert_eq!(
        app.workspace
            .tab(&tab)
            .unwrap()
            .interaction()
            .viewport_anchor,
        original.text.find("Paragraph").unwrap()
    );
    // Build the actual widget tree, not only the DTO.
    let _ = flavite_ui::workspace_shell::view(&app);
    let start = doc.text.find("[ ] task").unwrap();
    let _=app.update(Message::StructureEdit{tab,document:doc.clone(),command:"task.toggle",args:serde_json::json!({"buffer_id":original.buffer_id,"revision":original.revision,"start":start,"checked":false})});
    wait(&mut app, |app| app.pending_operations() == 0);
    assert!(app.note_state_text(tab).unwrap().contains("[x] task"));
    assert_eq!(
        fs.read_if_exists(&root.join("a.md")).unwrap().unwrap(),
        original.text.as_bytes()
    );
    wait(&mut app, |app| {
        reading::document(app, tab).is_some_and(|d| d.unsaved)
    });
    let now = app.note_editor_view(tab).unwrap();
    let api = app.flavite.as_ref().unwrap();
    assert!(
        api.execute(flavite_api::Invocation::new(
            api.command_registry().next_request_id(),
            "undo",
            serde_json::json!({"buffer_id":now.buffer_id,"revision":now.revision}),
            flavite_api::Origin::Gui
        ))
        .is_success()
    );
    let before = app.note_state_text(tab).unwrap();
    assert!(before.contains("[ ] task"));
    // A callback from the previous rendered revision cannot mutate the new one.
    let _=app.update(Message::StructureEdit{tab,document:doc.clone(),command:"task.toggle",args:serde_json::json!({"buffer_id":original.buffer_id,"revision":original.revision,"start":start,"checked":false})});
    assert_eq!(app.note_state_text(tab).unwrap(), before);
    let _ = app.update(Message::ToggleNoteMode(tab));
    assert!(
        matches!(app.workspace.tab(&tab).unwrap().kind(),ViewKind::Note(n) if n.mode()==NoteMode::Edit)
    );
    assert_eq!(
        app.note_editor_view(tab).unwrap().buffer_id,
        original.buffer_id
    );
    assert!(app.valid_writing_target().is_none());
}
use flavite_fs::Fs;
#[test]
fn outline_properties_bindings_and_native_action_routes_share_current_source() {
    let (mut app, _fs, _root, _cache) = fixture();
    let note = open(&mut app);
    let _ = app.update(Message::OpenContextView {
        view: ContextViewKind::Outline,
        group: None,
    });
    let outline = app.workspace.focused_tab();
    wait(&mut app, |app| reading::document(app, outline).is_some());
    let doc = reading::document(&app, outline).unwrap().clone();
    assert_eq!(doc.document.outline[0].title, "Heading");
    let _ = app.update(Message::OutlineNavigate {
        tab: outline,
        document: doc.clone(),
        byte: doc.document.outline[0].byte,
    });
    assert_eq!(app.workspace.focused_tab(), note);
    assert_eq!(
        app.workspace
            .tab(&note)
            .unwrap()
            .interaction()
            .viewport_anchor,
        doc.document.outline[0].byte
    );
    let _ = app.update(Message::OpenContextView {
        view: ContextViewKind::Properties,
        group: None,
    });
    let props = app.workspace.focused_tab();
    wait(&mut app, |app| reading::document(app, props).is_some());
    app.refresh_semantics();
    let projection = flavite_ui::accessibility::reading::project(&app);
    let mut seen = std::collections::BTreeSet::new();
    assert!(
        projection.nodes.iter().all(|(id, _)| seen.insert(*id)),
        "semantic ids must be unique"
    );
    let (input,target)=projection.actions.iter().find(|(_,target)|matches!(target,flavite_ui::accessibility::NodeTarget::Reading{action:flavite_ui::accessibility::reading::ReadingAction::PropertyInput(key),..} if key=="title")).unwrap();
    let _ = target;
    let request = accesskit::ActionRequest {
        action: accesskit::Action::SetValue,
        target_tree: accesskit::TreeId::ROOT,
        target_node: *input,
        data: Some(accesskit::ActionData::Value("New title".into())),
    };
    let message = flavite_ui::accessibility::route_action(&app, request).unwrap();
    let _ = app.update(message);
    let doc = reading::document(&app, props).unwrap().clone();
    let _ = app.update(Message::PropertyApply {
        tab: props,
        document: doc,
        key: "title".into(),
    });
    wait(&mut app, |app| app.pending_operations() == 0);
    assert!(
        app.note_state_text(note)
            .unwrap()
            .contains("title: 'New title' # keep")
    );
    assert_eq!(app.workspace.focused_tab(), props);
    assert!(app.valid_writing_target().is_none());
    // Close before dispatch: removed semantic controls cannot execute.
    let removed = *input;
    let _ = app.update(Message::Layout(LayoutAction::CloseTab {
        expected_revision: app.workspace.revision(),
        tab: props,
    }));
    assert!(
        flavite_ui::accessibility::route_action(
            &app,
            accesskit::ActionRequest {
                action: accesskit::Action::SetValue,
                target_tree: accesskit::TreeId::ROOT,
                target_node: removed,
                data: Some(accesskit::ActionData::Value("wrong".into()))
            }
        )
        .is_none()
    );
}
#[test]
fn read_semantic_ids_are_unique_for_repeated_embeds_and_editor_actions_are_absent() {
    let (mut app, _fs, _root, _cache) = fixture();
    let tab = open(&mut app);
    let _ = app.update(Message::ToggleNoteMode(tab));
    wait(&mut app, |app| reading::document(app, tab).is_some());
    app.refresh_semantics();
    let p = flavite_ui::accessibility::reading::project(&app);
    let mut seen = std::collections::BTreeSet::new();
    assert!(
        p.nodes.iter().all(|(id, _)| seen.insert(*id)),
        "duplicate reading ids"
    );
    assert!(!app.a11y_nodes.values().any(
        |target| matches!(target,flavite_ui::accessibility::NodeTarget::Editor{tab:t,..} if *t==tab)
    ));
    assert!(p.actions.values().any(|target| matches!(
        target,
        flavite_ui::accessibility::NodeTarget::Reading {
            action: flavite_ui::accessibility::reading::ReadingAction::Task { .. },
            ..
        }
    )));
}

#[test]
fn source_preview_exposes_source_text_without_hidden_rendered_task_actions() {
    let (mut app, _fs, _root, _cache) = fixture();
    open(&mut app);
    let _ = app.update(Message::OpenContextView {
        view: ContextViewKind::Preview,
        group: None,
    });
    let tab = app.workspace.focused_tab();
    wait(&mut app, |app| reading::document(app, tab).is_some());
    let _ = app.update(Message::ToggleSourcePreview(tab));
    let projection = flavite_ui::accessibility::reading::project(&app);
    assert!(
        projection
            .nodes
            .iter()
            .any(|(_, node)| node.value().is_some_and(|text| text.contains("- [ ] task")))
    );
    assert!(!projection.actions.values().any(|target| matches!(target,
        flavite_ui::accessibility::NodeTarget::Reading { tab: t, action: flavite_ui::accessibility::reading::ReadingAction::Task { .. }, .. } if *t == tab)));
}

#[test]
fn properties_edit_source_activates_the_bound_note_in_edit_mode() {
    let (mut app, _fs, _root, _cache) = fixture();
    let note = open(&mut app);
    let _ = app.update(Message::ToggleNoteMode(note));
    let _ = app.update(Message::OpenContextView {
        view: ContextViewKind::Properties,
        group: None,
    });
    let props = app.workspace.focused_tab();
    wait(&mut app, |app| reading::document(app, props).is_some());
    app.refresh_semantics();
    let projection = flavite_ui::accessibility::reading::project(&app);
    let (&id, _) = projection
        .actions
        .iter()
        .find(|(_, target)| {
            matches!(
                target,
                flavite_ui::accessibility::NodeTarget::Reading {
                    action: flavite_ui::accessibility::reading::ReadingAction::OpenSource,
                    ..
                }
            )
        })
        .unwrap();
    let action = flavite_ui::accessibility::route_action(
        &app,
        accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target_tree: accesskit::TreeId::ROOT,
            target_node: id,
            data: None,
        },
    )
    .unwrap();
    let _ = app.update(action);
    assert_eq!(app.workspace.focused_tab(), note);
    assert!(
        matches!(app.workspace.tab(&note).unwrap().kind(),ViewKind::Note(n) if n.mode()==NoteMode::Edit)
    );
    assert!(app.valid_writing_target().is_none());
}

#[test]
fn rapid_captured_input_is_interpreted_in_order_without_replaying_after_another_origin_edit() {
    use flavite_editor::{
        EditorAction, EditorInteractionSnapshot, input::CapturedInput, keymap::KeyAction,
    };
    let (mut app, _fs, _root, _cache) = fixture();
    let tab = open(&mut app);
    let view = app.note_editor_view(tab).unwrap();
    let incarnation = app.notes.incarnation(tab).unwrap();
    let at = view.text.len();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(EditorInteractionSnapshot {
            sequence: 1,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret: at,
            selection: (at, at),
            viewport_anchor: 0,
            focused: true,
            composing: false,
        }),
    });
    let mut sequence = 1;
    // Every capture deliberately precedes a redraw: all name the same old
    // revision, including Unicode deletion and document-boundary navigation.
    for (action, text) in [
        (KeyAction::Insert, "雪"),
        (KeyAction::Insert, "a"),
        (KeyAction::Insert, "\u{301}"),
        (KeyAction::Insert, "👩‍💻"),
        (KeyAction::Backspace, ""),
        (KeyAction::Backspace, ""),
    ] {
        sequence += 1;
        let _ = app.update(Message::EditorInput {
            tab,
            incarnation,
            input: CapturedInput {
                buffer: view.buffer_id,
                observed_revision: view.revision,
                sequence,
                action,
                text: text.into(),
                extend: false,
                document_boundary: false,
            },
        });
    }
    assert_eq!(
        app.note_state_text(tab).unwrap(),
        format!("{}雪", view.text)
    );
    assert!(app.valid_writing_target().is_some());
    let api = app.flavite.as_ref().unwrap();
    let current = api
        .get_shared_buffer_view(view.buffer_id.into(), None)
        .unwrap();
    assert!(api.execute(flavite_api::Invocation::new(api.command_registry().next_request_id(),"apply_edits",serde_json::json!({"buffer_id":view.buffer_id,"revision":current.revision,"ops":[{"range":[0,0],"replacement":"other origin\n"}]}),flavite_api::Origin::Gui)).is_success());
    let before = app.note_state_text(tab).unwrap();
    sequence += 1;
    let _ = app.update(Message::EditorInput {
        tab,
        incarnation,
        input: CapturedInput {
            buffer: view.buffer_id,
            observed_revision: view.revision,
            sequence,
            action: KeyAction::Insert,
            text: "must not appear".into(),
            extend: false,
            document_boundary: false,
        },
    });
    assert_eq!(app.note_state_text(tab).unwrap(), before);
    let _ = app.update(Message::ToggleNoteMode(tab));
    let current = app.note_editor_view(tab).unwrap();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::replace(&current, 0, 0, "stale hidden editor".into()),
    });
    assert_eq!(app.note_state_text(tab).unwrap(), before);
}
