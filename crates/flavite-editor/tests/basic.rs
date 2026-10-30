use flavite_api::session::SessionCommand;
use flavite_editor::{EditorAction, EditorSurface, EditorView, LinkNavigation};
use flavite_syntax::text::{Span, TextByte};
use std::path::PathBuf;

struct SurfaceDouble {
    view: EditorView,
}

impl EditorSurface for SurfaceDouble {
    fn view(&self) -> &EditorView {
        &self.view
    }
}

fn view() -> EditorView {
    EditorView {
        buffer_id: 42,
        revision: 7,
        text: "hello".into(),
        path: PathBuf::from("note.md"),
        state: flavite_api::BufferStatus::Dirty,
    }
}

#[test]
fn surface_trait_is_implementable_without_iced() {
    let surface = SurfaceDouble { view: view() };
    let action = surface.replace_selection(1, 4, "i".into());
    let (buffer_id, revision, edits) = action.as_edit().expect("edit action");
    assert_eq!(buffer_id, 42);
    assert_eq!(revision, 7);
    assert_eq!(edits[0].range, Span::new(TextByte(1), TextByte(4)));
    assert_eq!(edits[0].replacement, "i");
}

#[test]
fn edits_become_revision_guarded_session_commands() {
    let action = EditorAction::replace(&view(), 5, 5, "!".into());
    match action.into_session_command() {
        SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops,
        } => {
            assert_eq!(buffer_id.as_u64(), 42);
            assert_eq!(revision.as_u64(), 7);
            assert_eq!(ops[0].range, Span::new(TextByte(5), TextByte(5)));
            assert_eq!(ops[0].replacement, "!");
        }
        other => panic!("expected ApplyEdits, got {other:?}"),
    }
}

#[test]
fn history_actions_remain_typed_session_commands() {
    let undo = EditorAction::history(&view(), false).into_session_command();
    let redo = EditorAction::history(&view(), true).into_session_command();
    assert!(
        matches!(undo, SessionCommand::Undo { buffer_id, revision } if buffer_id.as_u64() == 42 && revision.as_u64() == 7)
    );
    assert!(
        matches!(redo, SessionCommand::Redo { buffer_id, revision } if buffer_id.as_u64() == 42 && revision.as_u64() == 7)
    );
}

#[test]
fn link_activation_becomes_revision_guarded_note_open_invocation() {
    let action = EditorAction::open_link(
        &view(),
        LinkNavigation {
            target: "Ideas".into(),
            alias: Some("Ideas".into()),
            anchor: Some("Plan".into()),
            by_id: false,
            span: Span::new(TextByte(0), TextByte(12)),
            create_if_unresolved: true,
        },
    );
    let invocation = action
        .into_link_invocation(73, PathBuf::from("ideas.md"))
        .expect("link action should become a host command");
    assert_eq!(invocation.command_id, "note.open");
    assert_eq!(invocation.request_id, 73);
    assert_eq!(invocation.preconditions.buffer_id, Some(42));
    assert_eq!(invocation.preconditions.expected_revision, Some(7));
    assert_eq!(invocation.args["target"], "Ideas");
    assert_eq!(invocation.args["path"], "ideas.md");
}
