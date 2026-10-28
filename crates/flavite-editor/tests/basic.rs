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
fn edits_become_revision_guarded_public_invocations() {
    let action = EditorAction::replace(&view(), 5, 5, "!".into());
    let invocation = action.into_invocation(99);
    assert_eq!(invocation.command_id, "apply_edits");
    assert_eq!(invocation.request_id, 99);
    assert_eq!(invocation.preconditions.buffer_id, Some(42));
    assert_eq!(invocation.preconditions.expected_revision, Some(7));
    let op = &invocation.args["ops"][0];
    assert_eq!(op["range"], serde_json::json!([5, 5]));
    assert_eq!(op["replacement"], "!");
}

#[test]
fn history_actions_remain_typed_session_commands() {
    let undo = EditorAction::history(&view(), false).into_invocation(1);
    let redo = EditorAction::history(&view(), true).into_invocation(2);
    assert_eq!(undo.command_id, "undo");
    assert_eq!(redo.command_id, "redo");
    assert_eq!(undo.args["buffer_id"], 42);
    assert_eq!(redo.args["revision"], 7);
}
