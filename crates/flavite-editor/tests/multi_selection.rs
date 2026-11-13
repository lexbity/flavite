//! Multi-selection editing against an independent right-to-left string
//! oracle (FEUX-0002 X04, AC-6): every planner batch must equal applying
//! each member's effective edit separately from the last member backward,
//! and the after-set must be canonical against the after-text.

use flavite_buffer::selection::{RawSelection, SelectionSet};
use flavite_editor::EditorView;
use flavite_editor::input::{CapturedInput, PreparedInput, prepare};
use flavite_editor::keymap::KeyAction;

fn view_of(text: &str) -> EditorView {
    EditorView {
        buffer_id: 7,
        revision: 3,
        text: text.to_owned().into(),
        path: "note.md".into(),
        state: flavite_buffer::state::BufferState::Clean,
    }
}

fn set_of(text: &str, members: &[(usize, usize)]) -> SelectionSet {
    let raw: Vec<RawSelection> = members
        .iter()
        .map(|&(anchor, head)| RawSelection {
            id: None,
            anchor,
            head,
        })
        .collect();
    SelectionSet::normalize(text, raw, None).unwrap().0
}

fn captured(action: KeyAction, text: &str) -> CapturedInput {
    CapturedInput {
        buffer: 7,
        observed_revision: 3,
        sequence: 9,
        action,
        text: text.to_owned(),
        extend: false,
        document_boundary: false,
        vertical: None,
        preferred_x: Vec::new(),
        clipboard_acquired: false,
    }
}

/// The independent oracle: apply ascending, non-overlapping
/// `(start, end, replacement)` edits by splicing segments of the original
/// text (no shared machinery; linear in document + edits).
fn oracle_apply(text: &str, edits: &[(usize, usize, String)]) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut pos = 0;
    for (start, end, replacement) in edits {
        out.push_str(&text[pos..*start]);
        out.push_str(replacement);
        pos = *end;
    }
    out.push_str(&text[pos..]);
    out
}

/// Buffer edit ops in the oracle shape.
fn oracle_edits(edits: &[flavite_buffer::undo::EditOp]) -> Vec<(usize, usize, String)> {
    edits
        .iter()
        .map(|op| (op.range.start.0, op.range.end.0, op.replacement.clone()))
        .collect()
}

/// The planner's edit batch as `(start, end, replacement)` triples.
fn planned_edits(prepared: &PreparedInput) -> Vec<(usize, usize, String)> {
    let PreparedInput::Edit { action, .. } = prepared else {
        panic!("expected an edit, got {prepared:?}");
    };
    let (_, _, edits) = action.as_edit().expect("edit action");
    oracle_edits(edits)
}

/// A multibyte-rich document with a known line structure.
fn unicode_doc() -> String {
    "alpha café\n雪 beta\ngamma — δ\nélève 雪\nlast\n".to_owned()
}

/// A ~1 MiB ASCII document with many lines (for large N).
fn big_doc() -> String {
    let mut s = String::with_capacity(1024 * 1024);
    let mut i = 0;
    while s.len() < 1024 * 1024 {
        s.push_str(&format!("line {i} some words here\n"));
        i += 1;
    }
    s
}

/// Caret members spread across `doc` (grapheme-safe: ASCII line starts).
fn spread_carets(count: usize, doc: &str) -> Vec<(usize, usize)> {
    let lines: Vec<usize> = doc.match_indices('\n').map(|(i, _)| i + 1).collect();
    let mut members = Vec::with_capacity(count);
    for k in 0..count {
        let idx = (k * (lines.len() - 1)) / count.max(1);
        let at = lines.get(idx).copied().unwrap_or(0).min(doc.len());
        members.push((at, at));
    }
    members
}

/// Assert the plan's after-text equals the independent right-to-left oracle
/// applied to the planned edits, for every member of `counts`.
fn assert_oracle_equality(doc: &str, counts: &[usize], action: KeyAction, text: &str) {
    for &count in counts {
        let set = set_of(doc, &spread_carets(count, doc));
        let view = view_of(doc);
        let prepared = prepare(&view, &set, &captured(action, text)).expect("plans");
        let edits = planned_edits(&prepared);
        let expected = oracle_apply(doc, &edits);
        // Independent recomputation from raw ops only.
        let mut raw = doc.to_owned();
        for (start, end, replacement) in edits.iter().rev() {
            raw.replace_range(*start..*end, replacement);
        }
        assert_eq!(
            expected, raw,
            "planner after-text vs raw oracle at N={count}"
        );
        // After-set must be canonical against the after-text.
        let PreparedInput::Edit { members, .. } = &prepared else {
            unreachable!()
        };
        let after_set = set_of(&expected, members);
        assert!(
            after_set.validate_against(&expected).is_ok(),
            "after-set invalid against after-text at N={count}"
        );
    }
}

#[test]
fn typing_matches_the_right_to_left_oracle_at_every_cardinality() {
    assert_oracle_equality(&unicode_doc(), &[1, 2, 47], KeyAction::Insert, "·x·");
}

#[test]
fn typing_matches_the_oracle_at_ten_thousand_members() {
    let doc = big_doc();
    assert_oracle_equality(&doc, &[10_000], KeyAction::Insert, "·x·");
}

#[test]
fn delete_matches_the_right_to_left_oracle_at_every_cardinality() {
    let doc = big_doc();
    assert_oracle_equality(&doc, &[1, 2, 47, 10_000], KeyAction::Delete, "");
}

#[test]
fn backspace_matches_the_oracle_across_members() {
    let doc = unicode_doc();
    assert_oracle_equality(&doc, &[1, 2, 47], KeyAction::Backspace, "");
}

#[test]
fn typed_text_lands_at_every_member_and_collapses_to_carets() {
    let doc = unicode_doc();
    let view = view_of(&doc);
    let members: Vec<(usize, usize)> = vec![(0, 0), (4, 6), (doc.len(), doc.len())];
    let set = set_of(&doc, &members);
    let prepared = prepare(&view, &set, &captured(KeyAction::Insert, "X")).unwrap();
    let PreparedInput::Edit {
        action,
        members,
        primary: _,
        clipboard: _,
    } = prepared
    else {
        panic!("typing prepares an edit");
    };
    let edits = oracle_edits(action.as_edit().unwrap().2);
    assert_eq!(edits.len(), 3, "one atomic batch per member");
    assert_eq!(members.len(), 3);
    assert_eq!(members[0], (1, 1));
}

#[test]
fn cut_collapses_members_and_joins_fragments_in_document_order() {
    let doc = "one two\nthree four\n";
    let view = view_of(doc);
    // "one" (0..3) and "four" (14..18: f=14..r=17, then the break at 18).
    let set = set_of(doc, &[(0, 3), (14, 18)]);
    let prepared = prepare(&view, &set, &captured(KeyAction::Cut, "")).unwrap();
    let PreparedInput::Edit {
        action,
        members,
        primary: _,
        clipboard,
    } = prepared
    else {
        panic!("cut prepares an edit");
    };
    assert_eq!(clipboard.as_deref(), Some("one\nfour"));
    assert_eq!(oracle_edits(action.as_edit().unwrap().2).len(), 2);
    assert_eq!(members.len(), 2);
    assert_eq!(members[0], (0, 0));
}

#[test]
fn copy_joins_fragments_with_lf_in_document_order() {
    let doc = "one two\nthree four\n";
    let view = view_of(doc);
    let set = set_of(doc, &[(0, 3), (14, 18)]);
    let prepared = prepare(&view, &set, &captured(KeyAction::Copy, "")).unwrap();
    let PreparedInput::Copy { text } = prepared else {
        panic!("copy");
    };
    assert_eq!(text, "one\nfour");
}
