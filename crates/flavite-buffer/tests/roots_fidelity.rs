//! RFC-0011 S2-T2 (AC-4): snapshot fidelity across edit/undo/redo/reopen
//! cycles with CRLF/BOM/trailing-newline fixtures, plus a randomized
//! LF-only source-byte proptest.
//!
//! A root must materialize byte-identically to the eager snapshot path:
//! text content, per-line EOL state, BOM/descriptor, trailing-newline state.

use flavite_buffer::{
    EditorModel,
    undo::{EditOp, UndoPolicy},
};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};

#[test]
fn crlf_bom_and_trailing_newline_survive_edit_undo_redo_reopen() {
    let source = "\u{FEFF}one\r\ntwo\r\nthree\nfour\r\nfive";
    let mut model = EditorModel::from_snapshot(TextSnapshot::new(source.as_bytes()).unwrap());

    let snapshot_of = |model: &EditorModel| {
        (
            model.text().to_owned(),
            model.try_to_source_bytes().expect("source"),
        )
    };
    let mut states = vec![snapshot_of(&model)];

    // Typing in the middle of a CRLF line (no newline touch).
    model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(6), TextByte(6)),
                "XY".into(),
            )],
            UndoPolicy::Isolated,
        )
        .expect("typing applies");
    states.push(snapshot_of(&model));

    // Insert a newline at EOF: must inherit the dominant CRLF ending.
    let at = model.len();
    model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(at), TextByte(at)),
                "\n".into(),
            )],
            UndoPolicy::Isolated,
        )
        .expect("newline insert applies");
    states.push(snapshot_of(&model));

    // Replace a whole line including its terminator.
    let line3 = model.text().find("three").expect("three line");
    model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(line3), TextByte(line3 + 6)),
                "THREE\n".into(),
            )],
            UndoPolicy::Isolated,
        )
        .expect("line replace applies");
    states.push(snapshot_of(&model));

    // Undo all the way down, byte-exact at every state.
    for expected in states.iter().rev().skip(1) {
        assert!(model.undo().is_some(), "undo available");
        assert_eq!(model.text(), expected.0, "undo text");
        assert_eq!(
            model.try_to_source_bytes().expect("source"),
            expected.1,
            "undo source bytes (EOL/BOM fidelity)"
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

    // Reopen from the final serialized source: byte-exact round-trip.
    let final_bytes = model.try_to_source_bytes().expect("source");
    let reopened = EditorModel::from_snapshot(TextSnapshot::new(&final_bytes).expect("snapshot"));
    assert_eq!(reopened.text(), model.text());
    assert_eq!(reopened.try_to_source_bytes().expect("source"), final_bytes);
    assert!(
        reopened.source_format().bom(),
        "BOM descriptor survives reopen"
    );
}

proptest::proptest! {
    #![proptest_config(proptest::test_runner::Config {
        cases: 512, ..proptest::test_runner::Config::default()
    })]

    /// LF-only randomized cycle (ASCII-safe alphabet, no CR / multibyte):
    /// source bytes equal the text at every step (the root is the only
    /// representation, so eager-snapshot equivalence is byte-exact by
    /// construction).
    #[test]
    fn lf_source_bytes_equal_text_across_random_cycles(
        base in "[a-zA-Z0-9 \n.,!?]{0,256}",
        ops in proptest::collection::vec(
            (0usize..512, 0usize..512, "[a-zA-Z0-9 \n.,!?]{0,16}", proptest::bool::ANY),
            1..50,
        ),
    ) {
        let mut model = EditorModel::from_text(&base);
        let mut text = base;
        for (x, y, repl, undo_scale) in ops.iter() {
            let mut a = x % (text.len() + 1);
            while !text.is_char_boundary(a) {
                a -= 1;
            }
            let mut b = (a + y % (text.len() + 1 - a)).min(text.len());
            while b < text.len() && !text.is_char_boundary(b) {
                b += 1;
            }
            model
                .try_apply_batch(
                    &[EditOp::new(
                        Span::new(TextByte(a), TextByte(b)),
                        repl.clone(),
                    )],
                    if *undo_scale {
                        UndoPolicy::Isolated
                    } else {
                        UndoPolicy::Typing
                    },
                )
                .expect("batch applies");
            text.replace_range(a..b, repl);
            proptest::prop_assert_eq!(model.text(), &text);
            proptest::prop_assert_eq!(
                model.try_to_source_bytes().expect("source"),
                text.as_bytes().to_vec(),
                "step: source bytes must equal normalized text (LF-only fixture)"
            );
            let snap_text = model.snapshot().text().to_owned();
            proptest::prop_assert_eq!(snap_text, text.as_str());
        }
    }
}
