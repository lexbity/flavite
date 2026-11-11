//! Movement delta tests (FEUX-0002 E-13/AC-15): transition spans for
//! undo/redo/restore across one and several steps, and the truncated flag.

use flavite_buffer::model::EditorModel;
use flavite_buffer::undo::EditOp;
use flavite_syntax::text::{Span, TextByte};

fn insert(model: &mut EditorModel, at: usize, text: &str) {
    model.apply_edit_ops(&[EditOp::new(
        Span::new(TextByte(at), TextByte(at)),
        text.into(),
    )]);
}

#[test]
fn single_undo_yields_exactly_one_span() {
    let mut model = EditorModel::from_text("abc");
    insert(&mut model, 1, "X");
    assert_eq!(model.text(), "aXbc");
    let _ = model.undo();
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(movement.spans.len(), 1);
    let span = movement.spans[0];
    // The inverse of insert X at 1: delete bytes 1..2 in the pre-undo text.
    assert_eq!((span.start, span.end, span.replacement_len), (1, 2, 0));
}

#[test]
fn double_undo_yields_exactly_one_span_per_transition() {
    let mut model = EditorModel::from_text("abc");
    insert(&mut model, 0, "A");
    insert(&mut model, 4, "B");
    assert_eq!(model.text(), "AabcB");
    let _ = model.undo();
    assert_eq!(model.text(), "Aabc");
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(movement.spans.len(), 1);
    assert_eq!((movement.spans[0].start, movement.spans[0].end), (4, 5));
    let _ = model.undo();
    assert_eq!(model.text(), "abc");
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(movement.spans.len(), 1);
    assert_eq!((movement.spans[0].start, movement.spans[0].end), (0, 1));
}

#[test]
fn restore_to_root_across_two_steps_yields_two_spans() {
    let mut model = EditorModel::from_text("abc");
    insert(&mut model, 0, "A");
    insert(&mut model, 4, "B");
    assert_eq!(model.text(), "AabcB");
    let page = model.history_page(None, 100).unwrap();
    // Entries are newest-first; the root is the initial entry.
    let root = page.entries.iter().find(|entry| entry.initial).unwrap();
    let target = root.target;
    let _ = model.restore_state(target).unwrap();
    assert_eq!(model.text(), "abc");
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(movement.spans.len(), 2);
    // Up-step 1: delete B at 4..5 (coordinates of "AabcB").
    assert_eq!((movement.spans[0].start, movement.spans[0].end), (4, 5));
    // Up-step 2: delete A at 0..1 (coordinates of "Aabc").
    assert_eq!((movement.spans[1].start, movement.spans[1].end), (0, 1));
}

#[test]
fn restore_via_history_target_reports_one_step_down() {
    let mut model = EditorModel::from_text("abc");
    insert(&mut model, 0, "A");
    insert(&mut model, 4, "B");
    let page = model.history_page(None, 100).unwrap();
    let target = page
        .entries
        .iter()
        .find(|entry| {
            model
                .history_node_text(entry.target.node)
                .is_some_and(|text| text == "Aabc")
        })
        .unwrap()
        .target;
    let _ = model.restore_state(target).unwrap();
    assert_eq!(model.text(), "Aabc");
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(movement.spans.len(), 1, "one step down to the target");
    assert_eq!((movement.spans[0].start, movement.spans[0].end), (4, 5));
}
