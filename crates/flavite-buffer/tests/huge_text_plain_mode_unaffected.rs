//! S7-T4 (plain-mode companion): documents ≥ 64 MiB skip markdown parsing in
//! the editor (FR-9), but editing remains fully functional — the gate must
//! never touch buffer behavior.
//!
//! Note on undo at huge sizes: with today's eager full-snapshot history
//! nodes, two ~64 MiB snapshots (root + one edit node) exceed the default
//! 128 MiB per-buffer history budget, so the undo tree's budget eviction
//! takes over exactly as it does today — that is pre-existing FEUX-0002
//! X01 budget behavior, unrelated to the plain-mode gate. Undo/redo are
//! asserted here at budget-feasible large sizes.

use flavite_buffer::model::EditorModel;
use flavite_buffer::undo::EditOp;
use flavite_syntax::text::{Span, TextByte};

const HUGE: usize = 64 * 1024 * 1024;

fn build_huge() -> String {
    let mut big = String::with_capacity(HUGE + 4096);
    while big.len() < HUGE {
        big.push_str("line of ordinary prose with a [[link]] in it\n");
    }
    assert!(big.len() >= HUGE);
    big
}

#[test]
fn editing_works_on_huge_documents_above_the_plain_mode_threshold() {
    let big = build_huge();
    let mut model = EditorModel::from_text(&big);
    assert_eq!(model.len(), big.len());

    // Typing (insert) at EOF works on the ≥ 64 MiB document.
    let tail = TextByte(big.len());
    let rev = model.insert(tail, " typed tail");
    assert_eq!(rev, model.revision());
    assert_eq!(model.len(), big.len() + " typed tail".len());
    assert!(model.text().ends_with(" typed tail"));

    // A mid-document batch also applies cleanly.
    let start = TextByte(big.len() / 3);
    let end = TextByte(start.0 + 5);
    let rev2 = model.apply_edit_ops(&[EditOp::new(Span::new(start, end), "replaced".into())]);
    assert_eq!(
        model.len(),
        big.len() + " typed tail".len() - 5 + "replaced".len()
    );
    assert_eq!(rev2, model.revision());
    assert!(model.text().starts_with(&big[..big.len() / 3]));
}

#[test]
fn undo_and_redo_work_on_large_documents_within_history_budget() {
    // 40 MiB: root + edit-node snapshots (≈80 MiB) stay under the default
    // 128 MiB per-buffer history budget, so undo/redo are fully functional.
    let target = 40 * 1024 * 1024;
    let mut big = String::with_capacity(target + 1024);
    while big.len() < target {
        big.push_str("line of ordinary prose with a [[link]] in it\n");
    }
    let mut model = EditorModel::from_text(&big);

    let tail = TextByte(big.len());
    let rev = model.insert(tail, " typed tail");
    let mut with_tail = big.clone();
    with_tail.push_str(" typed tail");
    assert_eq!(model.text(), with_tail);

    assert_eq!(model.undo(), Some(rev.next()));
    assert_eq!(model.text(), big);
    assert_eq!(model.len(), big.len());

    assert!(model.redo().is_some());
    assert_eq!(model.len(), big.len() + " typed tail".len());

    // A second edit cycle around the midpoint.
    let mid = TextByte(big.len() / 2);
    let rev2 = model.insert(mid, "X");
    assert_eq!(model.len(), big.len() + " typed tail".len() + 1);
    assert_eq!(model.undo(), Some(rev2.next()));
    assert_eq!(model.text(), with_tail);
}
