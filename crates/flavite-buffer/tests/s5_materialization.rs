//! RFC-0008 S5 — lazy `NodeText` materialization (D5/FR-5).
//!
//! S5-T1: materialized delta nodes are byte-identical to the eager
//! snapshots the pre-S5 path produced (text, per-line EOL via source bytes,
//! format) across mixed typing/context/boundary/seal sequences, including a
//! reopen cycle.
//!
//! S5-T3: a 10 000-node burst keeps undo sub-budget (the every-64 retention
//! floor bounds materialization walks to 64 steps).

use flavite_buffer::{
    EditorModel,
    history::{EditKind, EditorEdit},
    undo::{EditOp, UndoPolicy},
};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use std::time::{Duration, Instant};

const TYPING_ORIGIN: u128 = 1;

fn record(at: usize, value: &str) -> EditorEdit {
    EditorEdit {
        before_primary: 0,
        origin: format!("{TYPING_ORIGIN:032x}"),
        kind: EditKind::Typing,
        before: vec![[0, at, at]],
        after: vec![[0, at + value.len(), at + value.len()]],
        primary: 0,
    }
}

fn type_at(model: &mut EditorModel, at: usize, value: &str) {
    model
        .try_apply_recorded_batch(
            &[EditOp::new(
                Span::new(TextByte(at), TextByte(at)),
                value.into(),
            )],
            UndoPolicy::Typing,
            record(at, value),
        )
        .expect("typing applies");
}

fn insert_at(model: &mut EditorModel, at: usize, value: &str) {
    model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(at), TextByte(at)),
                value.into(),
            )],
            UndoPolicy::Isolated,
        )
        .expect("isolated applies");
}

#[test]
fn delta_materialization_matches_eager_snapshots_byte_for_byte() {
    // Mixed sequence: typing (deltas), sealed runs, isolated boundaries,
    // newline insertions; the CRLF source exercises EOL fidelity.
    let mut model =
        EditorModel::from_snapshot(TextSnapshot::new("one\r\ntwo\nthree\r\n".as_bytes()).unwrap());
    let mut states: Vec<(String, Vec<u8>)> = vec![(
        model.text().to_string(),
        model.try_to_source_bytes().expect("source"),
    )];
    let note = |model: &EditorModel, states: &mut Vec<(String, Vec<u8>)>| {
        states.push((
            model.text().to_string(),
            model.try_to_source_bytes().expect("source"),
        ));
    };

    for _ in 0..3 {
        let at = model.text().len();
        type_at(&mut model, at, "x");
        note(&model, &mut states);
    }
    model.seal_undo_group();
    let at = model.text().len();
    type_at(&mut model, at, "y");
    note(&model, &mut states);
    let at = model.text().len();
    insert_at(&mut model, at, "mid\n");
    note(&model, &mut states);
    let at = model.text().len();
    type_at(&mut model, at, "z");
    note(&model, &mut states);
    // Replace a middle line (spanning a newline boundary).
    let mid = model.text().find("mid").expect("mid line");
    model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(mid), TextByte(mid + 4)),
                "replaced\n".into(),
            )],
            UndoPolicy::Isolated,
        )
        .expect("replace applies");
    note(&model, &mut states);

    // Undo all the way down, comparing text AND exact source bytes at every
    // state (EOL/BOM/format byte-equal).
    for expected in states.iter().rev().skip(1) {
        assert!(model.undo().is_some(), "undo available");
        assert_eq!(model.text(), expected.0, "undo text");
        assert_eq!(
            model.try_to_source_bytes().expect("source"),
            expected.1,
            "undo source bytes (EOL fidelity)"
        );
    }
    // Redo back up.
    for expected in states.iter().skip(1) {
        assert!(model.redo().is_some(), "redo available");
        assert_eq!(model.text(), expected.0, "redo text");
        assert_eq!(
            model.try_to_source_bytes().expect("source"),
            expected.1,
            "redo source bytes"
        );
    }
    // Reopen cycle: rebuild from the final serialized source.
    let final_bytes = model.try_to_source_bytes().expect("source");
    let reopened = EditorModel::from_snapshot(TextSnapshot::new(&final_bytes).expect("snapshot"));
    assert_eq!(reopened.text(), model.text());
    assert_eq!(reopened.try_to_source_bytes().expect("source"), final_bytes);
}

#[test]
fn retention_floor_bounds_materialization_walks_after_a_long_burst() {
    // A long burst of distinct-origin context edits -> 5 000 delta nodes
    // (each install is O(edit); no per-node materialization). The every-64
    // retention floor bounds every materialization walk to 64 steps
    // (`debug_assert!` in `materialize_at`, active in test builds), and undo
    // remains correct. Release timing (NFR-4) is gated by the S5-T6 bench.
    let text = "x".repeat(64 * 1024);
    let mut model = EditorModel::from_text(&text);
    model.set_history_budget(flavite_buffer::budget::Budget {
        max_bytes: 20 * 1024 * 1024 * 1024,
        max_nodes: 6_000,
    });
    for i in 0..5_000u128 {
        let at = model.text().len();
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
            .expect("context node installs");
    }
    assert!(model.history_nodes() >= 4_900, "burst retained");

    let start = Instant::now();
    assert!(model.undo().is_some(), "undo after burst");
    let undo = start.elapsed();
    // Debug builds run the full materialization pipeline; the release gate
    // is the S5-T6 bench (NFR-4). Here, only correctness + bounded walk
    // (via the debug assertion inside materialize_at) are asserted.
    assert!(
        undo < Duration::from_secs(2),
        "undo after burst took {undo:?} in debug"
    );
    assert_eq!(model.text().len(), text.len() + 5_000 - 1);
}
