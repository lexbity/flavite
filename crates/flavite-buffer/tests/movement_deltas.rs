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

#[test]
fn undo_of_a_collapsed_batch_maps_carets_in_unchanged_gaps_exactly() {
    // FEUX-0002 R3: a batch of disjoint edits installs as one combined
    // node edit; its movement must come from the original per-op geometry,
    // not from the combined span (a caret in an unchanged gap is not a
    // replacement-interior point).
    use flavite_buffer::{Affinity, selection::map_point_through_spans};
    let mut model = EditorModel::from_text("abcdefghij");
    model
        .try_apply_batch(
            &[
                EditOp::new(Span::new(TextByte(2), TextByte(2)), "X".into()),
                EditOp::new(Span::new(TextByte(8), TextByte(8)), "Y".into()),
            ],
            flavite_buffer::undo::UndoPolicy::Isolated,
        )
        .unwrap();
    assert_eq!(model.text(), "abXcdefghYij");
    let _ = model.undo();
    assert_eq!(model.text(), "abcdefghij");
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(movement.spans.len(), 2, "one span per original op");
    // Inverse spans in the pre-undo frame: X occupies 2..3, Y occupies 9..10.
    assert_eq!((movement.spans[0].start, movement.spans[0].end), (2, 3));
    assert_eq!((movement.spans[1].start, movement.spans[1].end), (9, 10));
    // A passive caret at byte 5 (`e`) maps to 4, never to the combined
    // replacement's edge (8).
    let mapped = map_point_through_spans(5, &movement.spans, Affinity::Right);
    assert_eq!(mapped, 4);
    // Redo restores the exact forward geometry.
    let _ = model.redo();
    let movement = model.history_movement().unwrap();
    assert_eq!(movement.spans.len(), 2);
    assert_eq!((movement.spans[0].start, movement.spans[0].end), (2, 2));
    assert_eq!((movement.spans[1].start, movement.spans[1].end), (8, 8));
    assert_eq!(movement.spans[1].replacement_len, 1);
    let mapped = map_point_through_spans(4, &movement.spans, Affinity::Right);
    assert_eq!(mapped, 5);
}

#[test]
fn restore_across_a_collapsed_batch_maps_exactly() {
    use flavite_buffer::{Affinity, selection::map_point_through_spans};
    let mut model = EditorModel::from_text("abcdefghij");
    model
        .try_apply_batch(
            &[
                EditOp::new(Span::new(TextByte(2), TextByte(4)), "XY".into()),
                EditOp::new(Span::new(TextByte(8), TextByte(8)), "Z".into()),
            ],
            flavite_buffer::undo::UndoPolicy::Isolated,
        )
        .unwrap();
    assert_eq!(model.text(), "abXYefghZij");
    let page = model.history_page(None, 100).unwrap();
    let root = page.entries.iter().find(|entry| entry.initial).unwrap();
    let _ = model.restore_state(root.target).unwrap();
    assert_eq!(model.text(), "abcdefghij");
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(movement.spans.len(), 2);
    // The first op is length-preserving, so a caret at 6 (`g`, in the
    // unchanged gap between the ops) maps to 6 — never to an edge of the
    // combined span. A caret after the second op shifts by its delta.
    let mapped = map_point_through_spans(6, &movement.spans, Affinity::Right);
    assert_eq!(mapped, 6, "unchanged-gap caret keeps its position");
    let mapped = map_point_through_spans(9, &movement.spans, Affinity::Right);
    assert_eq!(mapped, 8, "caret after the insertion shifts by the delta");
}
