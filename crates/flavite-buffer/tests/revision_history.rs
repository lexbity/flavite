//! Revision authority is independent of undo-node coalescing and branching.
use flavite_buffer::{EditOp, EditorModel};
use flavite_syntax::text::{Span, TextByte};

#[test]
fn unicode_separators_and_standalone_cr_are_content_through_edit_undo_and_save() {
    let source = "|\u{0085}\u{2028}\u{2029}\rcontent\r\nnext\n";
    let snapshot = flavite_syntax::TextSnapshot::new(source.as_bytes()).unwrap();
    let mut model = EditorModel::from_snapshot(snapshot);
    assert_eq!(model.line_count(), 3);
    assert_eq!(model.to_source_bytes(), source.as_bytes());
    let at = model.text().find("next").unwrap();
    model.apply_edit_ops(&[EditOp::new(
        Span::new(TextByte(at), TextByte(at)),
        "[[target]]".into(),
    )]);
    assert_eq!(
        model.to_source_bytes(),
        source.replace("next", "[[target]]next").as_bytes()
    );
    model.undo().unwrap();
    assert_eq!(model.to_source_bytes(), source.as_bytes());
    model.redo().unwrap();
    assert_eq!(
        model.to_source_bytes(),
        source.replace("next", "[[target]]next").as_bytes()
    );
}
#[test]
fn every_coalesced_edit_and_branch_has_a_fresh_revision_and_correct_undo_parent() {
    let mut model = EditorModel::from_text("");
    let r1 = model.insert(TextByte(0), "a");
    let r2 = model.apply_edit_ops(&[EditOp::new(
        Span::new(TextByte(0), TextByte(1)),
        "ab".into(),
    )]);
    assert!(r2 > r1);
    assert_eq!(model.text(), "ab");
    let r3 = model.undo().unwrap();
    assert!(r3 > r2);
    assert_eq!(model.text(), "");
    let r4 = model.insert(TextByte(0), "c");
    assert!(r4 > r3);
    assert_eq!(model.text(), "c");
    assert!(model.undo().unwrap() > r4);
    assert_eq!(
        model.text(),
        "",
        "branch parent is the undone position, not abandoned text"
    );
}
#[test]
fn a_semantic_boundary_never_coalesces_with_typing_on_either_side() {
    let mut model = EditorModel::from_text("");
    model.insert(TextByte(0), "a");
    let mut edit = EditOp::new(Span::new(TextByte(1), TextByte(1)), "[[b]]".into());
    edit.boundary = true;
    model.apply_edit_ops(&[edit]);
    model.insert(TextByte(6), "c");
    model.undo();
    assert_eq!(model.text(), "a[[b]]");
    model.undo();
    assert_eq!(model.text(), "a");
    model.undo();
    assert_eq!(model.text(), "");
}

#[test]
fn coalesced_undo_transform_matches_real_text_and_redo_keeps_mixed_line_endings() {
    let mut model = EditorModel::from_snapshot(
        flavite_syntax::text::TextSnapshot::new(b"A\r\nB\nC\r\n").unwrap(),
    );
    model.apply_edit_ops(&[EditOp::new(
        Span::new(TextByte(0), TextByte(1)),
        "AA".into(),
    )]);
    let saved = model.to_source_bytes();
    model.seal_undo_group();
    model.apply_edit_ops(&[EditOp::new(
        Span::new(TextByte(0), TextByte(2)),
        "AAA".into(),
    )]);
    model.undo();
    assert_eq!(model.to_source_bytes(), saved);
    model.redo();
    assert_eq!(model.to_source_bytes(), b"AAA\r\nB\nC\r\n");
    let mut model = EditorModel::from_text("");
    model.insert(TextByte(0), "α");
    model.apply_edit_ops(&[EditOp::new(
        Span::new(TextByte(0), TextByte(2)),
        "αβ".into(),
    )]);
    model.undo();
    let forward = &model.last_transition_ops()[0];
    assert_eq!(forward.apply(""), "αβ");
    let inverse = forward.invert("");
    assert_eq!(inverse.range.end.0, 4);
    assert_eq!(inverse.replacement, "");
}
