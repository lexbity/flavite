//! Undo restores the recorded primary after a collision merges members.
#[path = "support/mod.rs"]
mod support;
use flavite_edit_context::ContextCommand;
use flavite_editor::{EditorAction, EditorInteractionSnapshot};
use flavite_ui::{
    app::Message,
    host::context_mode::{ContextMessage, EditorTarget},
};
use flavite_workspace::{LayoutAction, NoteLocator};
use std::time::{Duration, Instant};

#[test]
fn undo_restores_primary_after_an_edit_merges_members() {
    let mut fixture = support::ui_fixture(
        99110,
        "/test/feux-followup",
        &[support::SeedNote {
            components: &["note.md"],
            body: "ab",
        }],
    );
    let app = &mut fixture.app;
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|expected_revision| LayoutAction::OpenNote {
        expected_revision,
        group,
        locator: NoteLocator::from_components(["note.md"]).unwrap(),
    });
    let tab = app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline);
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    let view = app.note_editor_view(tab).unwrap();
    let incarnation = app.notes.incarnation(tab).unwrap();
    let snapshot = EditorInteractionSnapshot {
        sequence: app.workspace.tab(&tab).unwrap().interaction().sequence + 1,
        buffer_id: view.buffer_id,
        revision: view.revision,
        caret: 1,
        selection: (1, 1),
        selections: vec![[0, 0, 0], [1, 1, 1]],
        primary: 1,
        viewport_anchor: 0,
        focused: true,
        composing: false,
    };
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(snapshot),
    });
    assert_eq!(EditorTarget::capture(app, tab).unwrap().primary, 1);
    for command in [
        ContextCommand::Toggle,
        ContextCommand::Delete,
        ContextCommand::Undo,
    ] {
        let target = EditorTarget::capture(app, tab).unwrap();
        let _ = app.update(Message::ContextMode(ContextMessage::Run(target, command)));
    }
    assert_eq!(&*app.note_editor_view(tab).unwrap().text, "ab");
    let restored = EditorTarget::capture(app, tab).unwrap();
    assert_eq!(restored.selections, vec![(0, 0), (1, 1)]);
    assert_eq!(
        restored.primary, 1,
        "undo must restore the before-primary, not the after-primary"
    );
}

#[test]
fn session_rejects_old_view_versions_for_edits_and_history_without_host_guards() {
    use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};
    let mut f = support::ui_fixture(
        99111,
        "/test/view-version",
        &[support::SeedNote {
            components: &["note.md"],
            body: "abc",
        }],
    );
    let app = &mut f.app;
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|expected_revision| LayoutAction::OpenNote {
        expected_revision,
        group,
        locator: NoteLocator::from_components(["note.md"]).unwrap(),
    });
    let tab = app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline);
        let _ = app.update(Message::Pump);
        std::thread::yield_now();
    }
    let view = app.note_editor_view(tab).unwrap();
    let incarnation = app.notes.incarnation(tab).unwrap();
    let publish = |app: &mut flavite_ui::app::App, at| {
        let _ = app.update(Message::EditorAction {
            tab,
            incarnation,
            action: EditorAction::Interaction(EditorInteractionSnapshot {
                sequence: app.workspace.tab(&tab).unwrap().interaction().sequence + 1,
                buffer_id: view.buffer_id,
                revision: view.revision,
                caret: at,
                selection: (at, at),
                selections: vec![[0, at, at]],
                primary: 0,
                viewport_anchor: 0,
                focused: true,
                composing: false,
            }),
        });
    };
    publish(app, 0);
    let target = EditorTarget::capture(app, tab).unwrap();
    let api = app.flavite.as_ref().unwrap();
    let page = api.execute(editor_operations_fixture::admit(
        &api,
        Invocation::new(
            api.command_registry().next_request_id(),
            "buffer.history_page",
            serde_json::json!({"buffer_id":view.buffer_id,"revision":view.revision}),
            Origin::Gui,
        ),
    ));
    let ExecutionResult::Local(LocalResult::Success(page)) = page else {
        panic!("history page")
    };
    let history_target = page.data["page"]["entries"][0]["target"].clone();
    let issue = || {
        api.issue_editor_operation(
            view.buffer_id,
            view.revision,
            Some(flavite_api::editor_commands::EditorViewArg {
                view: format!("{:032x}", tab.raw()),
                incarnation,
                selection_version: target.selection_version,
            }),
        )
        .unwrap()
    };
    let operations = [issue(), issue(), issue(), issue()];
    publish(app, 1); // same text revision, new committed selection version
    assert_eq!(app.note_editor_view(tab).unwrap().revision, view.revision);
    for (command, operation) in [
        "apply_edits",
        "buffer.undo",
        "buffer.redo",
        "buffer.restore_state",
    ]
    .into_iter()
    .zip(operations)
    {
        let api = app.flavite.as_ref().unwrap();
        let mut args = serde_json::json!({"buffer_id":view.buffer_id,"revision":view.revision,"operation":operation});
        if command == "apply_edits" {
            args["ops"] = serde_json::json!([{"range":[0,0],"replacement":"WRONG"}]);
        }
        if command == "buffer.restore_state" {
            args["target"] = history_target.clone();
        }
        let result = api.execute(editor_operations_fixture::admit(
            &api,
            Invocation::new(
                api.command_registry().next_request_id(),
                command,
                args,
                Origin::Gui,
            ),
        ));
        assert!(
            matches!(result,ExecutionResult::Local(LocalResult::Error(ref error)) if error.message=="StaleSelectionVersion"),
            "{command}: {result:?}"
        );
        let after = app.note_editor_view(tab).unwrap();
        assert_eq!(after.text, view.text);
        assert_eq!(after.revision, view.revision);
    }
}

#[path = "../../flavite-api/tests/support/editor_operations.rs"]
mod editor_operations_fixture;
