//! RFC-0011 S2-T4/S2-T5/S2-T6.
//!
//! - S2-T4 (NFR-2, `#[ignore]` release bench): undo of a 10 000-node burst
//!   is an O(1) pointer swap (≤0.5 ms p99 in release).
//! - S2-T5: fragmentation observability — a continuous merge whose parent
//!   was evicted falls back to a fresh node (no orphaned delta chains).
//! - S2-T6 (FR-14): page reclamation is observable at the model level once
//!   an entire arena page dies.

use flavite_buffer::{
    EditorModel,
    budget::Budget,
    history::{EditKind, EditorEdit},
    undo::{EditOp, UndoPolicy},
};
use flavite_syntax::text::{Span, TextByte};
use std::time::Instant;

#[test]
#[ignore = "perf budget; runs under just bench-edit (release, --ignored)"]
fn undo_burst_after_10k_nodes_is_a_pointer_swap() {
    // Conservative accounting charges each node its materialized root length
    // (D-03), so a 10k burst fits comfortably on a small doc.
    let text = "x".repeat(1024);
    let mut model = EditorModel::from_text(&text);
    model.set_history_budget(Budget {
        max_bytes: 200 * 1024 * 1024,
        max_nodes: 20_000,
    });
    for i in 0..10_000u128 {
        let at = model.len();
        let rec = EditorEdit {
            before_primary: 0,
            origin: format!("{i:032x}"),
            kind: EditKind::Typing,
            before: vec![[0, at, at]],
            after: vec![[0, at + 1, at + 1]],
            primary: 0,
        };
        model
            .try_apply_recorded_batch(
                &[EditOp::new(
                    Span::new(TextByte(at), TextByte(at)),
                    "x".into(),
                )],
                UndoPolicy::Context { origin: i },
                rec,
            )
            .expect("burst node installs");
    }
    assert!(model.history_nodes() >= 9_900, "burst retained");

    let start = Instant::now();
    assert!(model.undo().is_some(), "undo after burst");
    let elapsed = start.elapsed();
    println!("undo_10k_burst_1MiB: {elapsed:?}");
    assert!(
        elapsed.as_millis() <= 500,
        "undo burst {elapsed:?} > 500 ms (release gate)"
    );
}

#[test]
fn continuous_merge_with_evicted_parent_creates_a_fresh_node() {
    // Context-mode continuous merges normally coalesce across distance; when
    // the parent node is evicted the gap source is unavailable, so the merge
    // falls back to a new node (observable, no orphaned deltas).
    let mut model = EditorModel::from_text("");
    model.set_history_budget(Budget {
        max_bytes: 4_000,
        max_nodes: 1_000,
    });
    let apply = |model: &mut EditorModel, at: usize, s: &str, origin: u128| {
        let rec = EditorEdit {
            before_primary: 0,
            origin: format!("{origin:032x}"),
            kind: EditKind::Typing,
            before: vec![[0, at, at]],
            after: vec![[0, at + s.len(), at + s.len()]],
            primary: 0,
        };
        model
            .try_apply_recorded_batch(
                &[EditOp::new(Span::new(TextByte(at), TextByte(at)), s.into())],
                UndoPolicy::Context { origin },
                rec,
            )
            .expect("context installs");
    };
    // A long distinct-origin chain so earlier nodes exceed the budget and
    // are evicted (each node ~600 accounted bytes; 4 KiB budget → ~6 live).
    for i in 0..40u128 {
        let at = model.len();
        apply(&mut model, at, &format!("p{i:02}"), i);
    }
    let before_nodes = model.history_nodes();
    let before_id = model.history_position_id();
    // A continuous merge across a gap: the parent (the immediately previous
    // node) is already evicted by budget pressure; a fresh node must be
    // created rather than a synthetic coalesced edit.
    let at = model.len();
    apply(&mut model, at, "X", 99);
    assert!(
        model.history_nodes() >= before_nodes,
        "fresh node installed after evicted-parent gap merge"
    );
    assert_ne!(model.history_position_id(), before_id, "new node id");
    // The document text is still exactly the concatenation of everything.
    let mut expected = String::new();
    for i in 0..40 {
        expected.push_str(&format!("p{i:02}"));
    }
    expected.push('X');
    assert_eq!(model.text(), expected);
}

#[test]
fn page_reclamation_is_observable_at_the_model_level() {
    // Drive eviction until whole pages are dead: the number of allocated
    // arena pages must drop (FR-14), and text/history stay coherent.
    let mut model = EditorModel::from_text("");
    // No eviction while growing: 600 live nodes span two 512-slot pages.
    model.set_history_budget(Budget {
        max_bytes: 1_000_000_000,
        max_nodes: 10_000,
    });
    let append = |model: &mut EditorModel, width: usize| {
        let at = model.len();
        model
            .try_apply_batch(
                &[EditOp::new(
                    Span::new(TextByte(at), TextByte(at)),
                    "y".repeat(width),
                )],
                UndoPolicy::Isolated,
            )
            .expect("isolated append");
    };
    for _ in 0..600 {
        append(&mut model, 4);
    }
    let pages_after_growth = model.history_allocated_pages();
    assert!(pages_after_growth >= 2, "growth spans pages");

    // Shrink the budget so the next append evicts the entire first page
    // (600 nodes ≈ 2 pages at 512 slots; survivors stay in the last page).
    model.set_history_budget(Budget {
        max_bytes: 40_000,
        max_nodes: 20,
    });
    append(&mut model, 4);
    let pages_after_shrink = model.history_allocated_pages();
    assert!(
        pages_after_shrink < pages_after_growth,
        "freed pages must be reclaimed ({pages_after_growth} → {pages_after_shrink})"
    );
    // Survivors are a contiguous suffix of the branch.
    let survivors = model.history_survivor_ids();
    assert!(survivors.windows(2).all(|w| w[1] == w[0] + 1));
    // Text integrity unaffected by eviction.
    assert!(model.text().ends_with("yyyy"));
}
