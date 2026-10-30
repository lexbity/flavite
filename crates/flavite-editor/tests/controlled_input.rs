//! Controlled-input integration tests (RFC-0003 §19, W03).
//!
//! These drive the real widget/runtime seam headlessly: delayed and stale
//! acknowledgements, keyed reparenting, pointer selection, Unicode/CRLF
//! positions, preedit cancellation, and buffer replacement under the same
//! tab role. The host owns acknowledged state; a stale runtime event can
//! never overwrite a newer host acknowledgement or move document text.

use flavite_api::{BufferStatus, BufferView};
use flavite_editor::{
    EditorAction, EditorInteractionSnapshot as Snapshot, EditorView, EditorWidget, focus_editor,
    inspect_focused_editor, set_focused_editor_selection,
};
use iced::advanced::clipboard::Null as Clipboard;
use iced::advanced::input_method::Event as ImeEvent;
use iced::advanced::widget::Operation as _;
use iced::advanced::{Shell, Widget};
use iced::keyboard::{Key, Modifiers, key::Named};
use std::collections::HashMap;

fn view(text: &str, buffer_id: u64) -> BufferView {
    BufferView {
        buffer_id,
        revision: 11,
        text: text.to_owned(),
        base_hash: Vec::new(),
        state: BufferStatus::Clean,
        path: std::path::PathBuf::from("note.md"),
    }
}

fn snapshot(buffer_id: u64, revision: u64, caret: usize, sequence: u64, focused: bool) -> Snapshot {
    Snapshot {
        sequence,
        buffer_id,
        revision,
        caret,
        selection: (caret, caret),
        viewport_anchor: 0,
        focused,
        composing: false,
    }
}

fn typed(
    widget: &EditorWidget<EditorAction>,
    state: &mut flavite_editor::widget::EditorState,
    input: &str,
) -> Vec<EditorAction> {
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    for character in input.chars() {
        widget.execute_key(
            state,
            &Key::Character(character.to_string().into()),
            Modifiers::empty(),
            Some(&character.to_string()),
            &mut Clipboard,
            &mut shell,
        );
    }
    drop(shell);
    messages
}

#[test]
fn delayed_acknowledgement_installs_and_lower_sequences_are_rejected() {
    let buffer = view("hello", 12);
    let editor_view = EditorView::from(&buffer);
    let _widget = EditorWidget::new(&buffer, |action| action);
    let mut state = EditorWidget::<EditorAction>::state_for_test();
    state.reconcile(&editor_view, &HashMap::new());

    // A delayed host acknowledgement (sequence 7) installs caret and focus.
    state.restore_interaction("hello", snapshot(12, 11, 3, 7, true));
    assert_eq!(state.caret(), 3);
    assert!(state.is_focused());

    // A stale runtime event (lower sequence) cannot overwrite it.
    state.restore_interaction("hello", snapshot(12, 11, 0, 6, false));
    assert_eq!(state.caret(), 3, "stale sequence must be ignored");
    assert!(state.is_focused(), "stale unfocused snapshot must not blur");

    // A newer acknowledgement takes effect, including blur.
    state.restore_interaction("hello", snapshot(12, 11, 5, 9, false));
    assert_eq!(state.caret(), 5);
    assert!(!state.is_focused());

    // A mismatched buffer or revision is rejected outright.
    state.restore_interaction("other", snapshot(13, 11, 1, 10, true));
    state.restore_interaction("hello", snapshot(12, 12, 1, 10, true));
    assert_eq!(
        state.caret(),
        5,
        "wrong buffer/revision cannot move the caret"
    );
    assert!(!state.is_focused());
}

#[test]
fn typed_edits_are_revision_guarded_and_stale_actions_fail_conversion() {
    let buffer = view("hello", 12);
    let editor_view = EditorView::from(&buffer);
    let widget = EditorWidget::new(&buffer, |action| action);
    let mut state = EditorWidget::<EditorAction>::state_for_test();
    state.reconcile(&editor_view, &HashMap::new());
    state.restore_interaction("hello", snapshot(12, 11, 2, 1, true));

    let messages = typed(&widget, &mut state, "é漢");
    assert_eq!(messages.len(), 2, "one guarded edit per input event");
    let (_, revision, edits) = messages[0].as_edit().expect("edit action");
    assert_eq!(revision, 11);
    assert_eq!(edits[0].range.start.0, 2);
    assert_eq!(edits[0].replacement, "é");
    // The optimistic caret advances by the inserted UTF-8 length.
    let (_, _, second) = messages[1].as_edit().expect("edit action");
    assert_eq!(second[0].range.start.0, 2 + "é".len());
    assert_eq!(second[0].replacement, "漢");

    // The typed action converts to a registry invocation carrying the
    // expected revision; a stale revision cannot become an invocation.
    let invocation = messages[0].clone().into_invocation(1).expect("invocation");
    assert_eq!(invocation.preconditions.expected_revision, Some(11));
    assert_eq!(messages[0].as_edit().unwrap().0, 12);
}

#[test]
fn focused_editor_operations_respect_buffer_and_revision() {
    let editor_view = EditorView::from(&view("text", 12));
    let mut state = EditorWidget::<EditorAction>::state_for_test();
    state.reconcile(&editor_view, &HashMap::new());
    state.restore_interaction("text", snapshot(12, 11, 2, 4, false));

    // Stale revision focus is refused; current revision focus works.
    let mut stale = focus_editor(12, 12);
    stale.custom(
        None,
        iced::Rectangle::with_size(iced::Size::ZERO),
        &mut state,
    );
    assert!(!state.is_focused());
    assert!(matches!(
        stale.finish(),
        iced::advanced::widget::operation::Outcome::Some(None)
    ));

    let mut current = focus_editor(12, 11);
    current.custom(
        None,
        iced::Rectangle::with_size(iced::Size::ZERO),
        &mut state,
    );
    assert!(state.is_focused());
    assert!(matches!(
        current.finish(),
        iced::advanced::widget::operation::Outcome::Some(Some(_))
    ));

    // Selection operations are revision-bound too.
    let mut selection = set_focused_editor_selection(12, 11, 1, 4);
    selection.custom(
        None,
        iced::Rectangle::with_size(iced::Size::ZERO),
        &mut state,
    );
    assert_eq!(state.selection(), (1, 4));

    // Inspection returns the live controlled snapshot.
    let mut inspect = inspect_focused_editor();
    inspect.custom(
        None,
        iced::Rectangle::with_size(iced::Size::ZERO),
        &mut state,
    );
    match inspect.finish() {
        iced::advanced::widget::operation::Outcome::Some(Some(found)) => {
            assert_eq!(found.buffer_id, 12);
            assert_eq!(found.revision, 11);
            assert_eq!(found.caret, 4);
            assert_eq!(found.selection, (1, 4));
            assert!(found.focused);
        }
        _ => panic!("focused editor snapshot was not returned"),
    }
}

#[test]
fn pointer_selection_places_caret_at_grapheme_boundaries() {
    // Multi-byte text: the pointer can never split a character.
    let text = "abé漢";
    let shaped = flavite_editor::widget::shape_for_test(text, 300.0);
    let run = shaped.layout_runs().next().expect("one shaped line");
    let mut state = EditorWidget::<EditorAction>::state_for_test();
    for glyph in run.glyphs {
        state.place_caret_from_point(text, glyph.x + 0.1, 1.0, 300.0, false);
        let caret = state.caret();
        assert!(
            text.is_char_boundary(caret),
            "caret {caret} split a character in {text}"
        );
    }
}

#[test]
fn keyed_reparent_restores_state_and_fresh_keys_start_clean() {
    use iced::advanced::widget::tree::Tree;
    let buffer = view("hello", 12);
    let first = EditorWidget::new(&buffer, |action| action)
        .with_view_key(77, 3)
        .with_initial_interaction(snapshot(12, 11, 4, 8, true));
    let mut tree = Tree::empty();
    tree.state = Widget::<EditorAction, iced::Theme, iced::Renderer>::state(&first);
    // Diff against the same identity keeps the restored interaction.
    Widget::<EditorAction, iced::Theme, iced::Renderer>::diff(&first, &mut tree);
    {
        let state = tree
            .state
            .downcast_mut::<flavite_editor::widget::EditorState>();
        assert_eq!(state.caret(), 4);
        assert!(state.is_focused());
    }

    // A different view key (replaced view) resets state instead of
    // inheriting the previous editor's caret or sequences.
    let second = EditorWidget::new(&buffer, |action| action).with_view_key(88, 3);
    Widget::<EditorAction, iced::Theme, iced::Renderer>::diff(&second, &mut tree);
    let state = tree
        .state
        .downcast_mut::<flavite_editor::widget::EditorState>();
    assert_eq!(state.caret(), 0, "a replaced view starts clean");
    assert!(!state.is_focused());
}

#[test]
fn preedit_cancellation_and_buffer_replacement_under_the_same_tab_role() {
    let editor_view = EditorView::from(&view("hello", 12));
    let mut state = EditorWidget::<EditorAction>::state_for_test();
    state.reconcile(&editor_view, &HashMap::new());
    state.restore_interaction("hello", snapshot(12, 11, 3, 2, true));

    // Composition opens, accumulates preedit, then is cancelled: the
    // document is untouched and the caret stays at the composition start.
    state.input_method_event(&editor_view, &ImeEvent::Opened);
    state.input_method_event(&editor_view, &ImeEvent::Preedit("かな".into(), Some(0..6)));
    assert!(state.is_composing());
    assert_eq!(
        state.input_method_event(&editor_view, &ImeEvent::Closed),
        None
    );
    assert!(!state.is_composing());
    assert_eq!(state.caret(), 3);

    // A composition cannot migrate by vector index into replaced text:
    // buffer replacement under the same tab role clamps the caret to the
    // new text and resets the composition.
    let replaced = EditorView {
        revision: 12,
        text: "hi".into(),
        ..editor_view.clone()
    };
    state.reconcile(&replaced, &HashMap::new());
    assert!(!state.is_composing());
    assert_eq!(state.caret(), 2, "caret clamps to the replaced text");
    assert_eq!(state.selection(), (2, 2));

    // Actions are revision-guarded at the invocation boundary: an edit built
    // against the old snapshot carries the old expected revision and is
    // rejected as StaleRevision by the registry, never silently applied.
    let old = EditorView {
        revision: 11,
        ..replaced.clone()
    };
    let stale = EditorAction::replace(&old, 0, 0, "x".into());
    let invocation = stale.into_invocation(3).expect("typed invocation");
    assert_eq!(invocation.preconditions.expected_revision, Some(11));
    assert_ne!(
        invocation.preconditions.expected_revision,
        Some(replaced.revision)
    );
}

#[test]
fn crlf_positions_and_newline_edits_stay_byte_exact() {
    let text = "alpha\r\nbeta";
    let buffer = view(text, 12);
    let editor_view = EditorView::from(&buffer);
    let widget = EditorWidget::<EditorAction>::new(&buffer, |action| action);
    let mut state = EditorWidget::<EditorAction>::state_for_test();
    state.reconcile(&editor_view, &HashMap::new());
    // Caret after "alpha\r\n" is byte 7 (CR at 5, LF at 6).
    state.restore_interaction(text, snapshot(12, 11, 7, 1, true));
    assert_eq!(state.caret(), 7);

    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    widget.execute_key(
        &mut state,
        &Key::Named(Named::Enter),
        Modifiers::empty(),
        None,
        &mut Clipboard,
        &mut shell,
    );
    drop(shell);
    assert_eq!(messages.len(), 1);
    let (_, revision, edits) = messages[0].as_edit().expect("newline edit");
    assert_eq!(revision, 11);
    assert_eq!(edits[0].range.start.0, 7);
    assert_eq!(edits[0].replacement, "\n");
}
