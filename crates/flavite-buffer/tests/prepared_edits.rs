//! Prepared-batch replacement semantics (RFC-0006 S01, D-06/TD-8).
//!
//! One prepared batch is one revision-guarded install and one isolated Undo
//! unit: invalid batches change nothing, isolated batches never merge with
//! neighboring typing, and the EOL splice agrees with whole-reference
//! reconstruction across CRLF/LF/BOM fixtures.

use flavite_buffer::model::{BatchError, EditorModel};
use flavite_buffer::revision::Revision;
use flavite_buffer::state::Buffer;
use flavite_buffer::undo::{EditOp, UndoPolicy};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};

fn model_with(text: &str) -> EditorModel {
    let snapshot = TextSnapshot::new(text.as_bytes()).expect("valid UTF-8 fixture");
    EditorModel::from_snapshot(snapshot)
}

fn op(start: usize, end: usize, replacement: &str) -> EditOp {
    EditOp::new(Span::new(TextByte(start), TextByte(end)), replacement.to_owned())
}

#[test]
fn prepared_batch_applies_all_matches_and_installs_one_boundary() {
    let mut model = model_with("aa bb aa cc aa");
    // "aa bb aa cc aa": matches at 0..2, 6..8, 12..14.
    let batch = model
        .prepare_batch(&[op(0, 2, "X"), op(6, 8, "X"), op(12, 14, "X")])
        .expect("valid batch");
    let before = model.revision();
    let revision = model.install_prepared(batch);
    assert!(revision > before);
    assert_eq!(model.text(), "X bb X cc X");

    // One isolated undo restores the whole batch — and the boundary means
    // neighboring typing neither merges into it nor splits it.
    model
        .try_apply_batch(&[op(0, 1, "typing")], UndoPolicy::Typing)
        .expect("typing applies");
    let typing_revision = model.revision();
    model.undo();
    assert_eq!(model.text(), "X bb X cc X", "typing undo does not touch the batch");
    model.undo();
    assert_eq!(model.text(), "aa bb aa cc aa", "one undo removes the whole batch");
    let _ = typing_revision; // every undo also advances the revision (§4.5.2)
    model.redo();
    assert_eq!(model.text(), "X bb X cc X", "redo replays the whole batch");
}

#[test]
fn typing_policy_coalesces_isolated_does_not() {
    let mut model = model_with("hello");
    // Isolated batch.
    model
        .try_apply_batch(&[op(5, 5, "!")], UndoPolicy::Isolated)
        .expect("batch applies");
    let isolated_revision = model.revision();
    // Adjacent typing within the coalesce window must NOT merge with the
    // isolated boundary.
    model
        .try_apply_batch(&[op(6, 6, "?")], UndoPolicy::Typing)
        .expect("typing applies");
    // The typing edit forms its own unit: undoing lands exactly on the
    // isolated batch result, not on a merged superset. (Every undo still
    // advances the revision, §4.5.2 — only the unit boundaries matter.)
    let _ = isolated_revision;
    model.undo();
    assert_eq!(model.text(), "hello!");
}

#[test]
fn invalid_batch_after_valid_earlier_ranges_changes_nothing() {
    let mut model = model_with("abcdef");
    let text_before = model.text().to_owned();
    let revision_before = model.revision();
    // The LAST range is invalid: nothing may change — Rope, EOL, revision,
    // and the Undo tree included.
    let error = model
        .try_apply_batch(
            &[op(0, 1, "X"), op(2, 3, "Y"), op(5, 9, "Z")],
            UndoPolicy::Isolated,
        )
        .expect_err("out-of-bounds batch refused");
    assert!(matches!(error, BatchError::OutOfBounds { .. }));
    assert_eq!(model.text(), text_before);
    assert_eq!(model.revision(), revision_before);
    // Overlap refusal.
    let error = model
        .try_apply_batch(&[op(0, 3, "X"), op(2, 4, "Y")], UndoPolicy::Isolated)
        .expect_err("overlapping batch refused");
    assert!(matches!(error, BatchError::Overlapping { .. }));
    assert_eq!(model.text(), text_before);
    assert_eq!(model.revision(), revision_before);
    // Char-boundary refusal against multibyte content.
    let mut multibyte = model_with("aé😀b");
    let emoji_start = "aé".len();
    let error = multibyte
        .try_apply_batch(&[op(emoji_start + 1, emoji_start + 3, "X")], UndoPolicy::Isolated)
        .expect_err("mid-scalar range refused");
    assert!(matches!(error, BatchError::CharBoundary { .. }));
    // Undo history is unchanged: undo after the refusals returns to the
    // original text in one step (no partially applied node exists).
    model.undo();
    assert_eq!(model.text(), text_before);
}

#[test]
fn eol_splice_matches_whole_reference_reconstruction() {
    // Incremental splice vs whole-reference oracle across EOL styles,
    // including BOM and mixed endings (S01 test contract).
    let fixtures = [
        "line1\r\nline2\nline3\r\nline4",
        "\u{FEFF}line1\r\nline2\r\nline3",
        "lf only\nsecond\nthird",
        "cr-only\rsecond",
        "no trailing newline",
        "mixed\r\n\n\r\nend",
    ];
    for source in fixtures {
        let mut model = model_with(source);
        let expected_format = model.source_format();
        // Edits address the NORMALIZED text (CRLF folded, BOM removed); the
        // untouched lines' physical endings must survive byte-exactly.
        let normalized = source.replace("\r\n", "\n").replace('\u{FEFF}', "");
        let lines: Vec<&str> = normalized.split('\n').collect();
        if lines.len() < 3 {
            continue;
        }
        let second_start = normalized.find(lines[1]).expect("line found");
        let second_end = second_start + lines[1].len();
        let batch = model
            .prepare_batch(&[op(second_start, second_end, "REPLACED")])
            .expect("batch prepares");
        // The prepared undo snapshot carries the edited normalized text.
        let mut expected_normalized = normalized.clone();
        expected_normalized.replace_range(second_start..second_end, "REPLACED");
        assert_eq!(batch.undo_snapshot.text(), expected_normalized);
        let revision = model.install_prepared(batch);
        assert!(revision > Revision::INITIAL);
        assert_eq!(model.source_format(), expected_format);
        // Independent whole-reference oracle: the exact original bytes with
        // only the second line's content replaced — every untouched line's
        // EOL byte-exact (the splice must not downgrade CRLF/BOM state).
        let mut expected_bytes = source.as_bytes().to_vec();
        let raw_line = source
            .find(lines[1])
            .map(|at| at..at + lines[1].len())
            .unwrap_or_else(|| {
                // BOM fixture: skip the BOM before searching the raw bytes.
                let at = source.find(lines[1]).expect("raw line found");
                at..at + lines[1].len()
            });
        expected_bytes.splice(raw_line, b"REPLACED".iter().copied());
        let source_bytes = model.try_to_source_bytes().expect("encodable");
        assert_eq!(
            source_bytes, expected_bytes,
            "spliced EOL serialization must be byte-exact against the whole-reference oracle"
        );
    }
}

#[test]
fn batch_through_buffer_keeps_view_state_mapping() {
    // Type A, replace all, type B, undo three times — exact intermediate
    // texts (S01 test contract for the buffer spine).
    let mut buffer = Buffer::from_text("start\n");
    buffer
        .apply_local_edits_with_policy(&[op(6, 6, "A")], UndoPolicy::Typing)
        .expect("typing A");
    assert_eq!(buffer.text(), "start\nA");
    buffer
        .apply_local_edits_with_policy(&[op(0, 5, "START"), op(7, 7, "B")], UndoPolicy::Isolated)
        .expect("isolated batch");
    assert_eq!(buffer.text(), "START\nAB");
    // Undo three times: batch, typing A, then nothing left.
    assert!(buffer.model_mut().undo().is_some());
    assert_eq!(buffer.text(), "start\nA", "batch undoes as one unit");
    assert!(buffer.model_mut().undo().is_some());
    assert_eq!(buffer.text(), "start\n", "typing undoes separately");
    assert!(buffer.model_mut().undo().is_none(), "no third unit exists");
}

#[test]
fn unencodable_after_state_still_records_exact_text() {
    // A strict-ASCII draft holding non-ASCII content: preparation succeeds,
    // the draft text/EOL/format intent survive in the undo snapshot, and
    // saving fails with zero filesystem effects (asserted at the model
    // level: the descriptor stays ASCII, the text stays intact).
    let mut model = model_with("plain\n");
    model
        .try_convert_source_format(flavite_syntax::SourceFormat::ascii())
        .expect("ascii is representable");
    let revision = model
        .try_apply_batch(&[op(6, 6, "é😀")], UndoPolicy::Isolated)
        .expect("text edits never depend on encodability");
    assert!(revision > Revision::INITIAL);
    assert_eq!(model.text(), "plain\né😀");
    assert_eq!(model.source_format(), flavite_syntax::SourceFormat::ascii());
    assert!(
        model.try_to_source_bytes().is_err(),
        "the draft is unencodable: saving is refused before any file effect"
    );
    // Undo restores the ASCII-representable predecessor exactly.
    model.undo();
    assert_eq!(model.text(), "plain\n");
}
