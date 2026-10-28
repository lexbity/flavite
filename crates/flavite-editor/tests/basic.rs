use flavite_api::lattice::session::SessionCommand;
use flavite_editor::{EditorAction, EditorSurface, EditorView};
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
        state: "Dirty".into(),
    }
}

#[test]
fn surface_trait_is_implementable_without_iced() {
    let surface = SurfaceDouble { view: view() };
    let action = surface.replace_selection(1, 4, "i".into());
    assert_eq!(action.buffer_id, 42);
    assert_eq!(action.expected_revision, 7);
    assert_eq!(action.edits[0].range, Span::new(TextByte(1), TextByte(4)));
    assert_eq!(action.edits[0].replacement, "i");
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
    assert!(matches!(undo, SessionCommand::Undo { buffer_id } if buffer_id.as_u64() == 42));
    assert!(matches!(redo, SessionCommand::Redo { buffer_id } if buffer_id.as_u64() == 42));
}
