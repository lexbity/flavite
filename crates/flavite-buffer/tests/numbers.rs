//! Number operation tests for FEUX-0004 X02 (AC-3).
//!
//! Tests the number scanner and operations as specified in E-9.

use flavite_buffer::commands::{EditOperation, PlanError, plan};
use flavite_buffer::selection::{RawSelection, SelectionSet};

fn set_of(text: &str, range: std::ops::Range<usize>) -> SelectionSet {
    SelectionSet::normalize(
        text,
        vec![RawSelection {
            id: None,
            anchor: range.start,
            head: range.end,
        }],
        None,
    )
    .unwrap()
    .0
}

fn set_caret(text: &str, pos: usize) -> SelectionSet {
    SelectionSet::normalize(
        text,
        vec![RawSelection {
            id: None,
            anchor: pos,
            head: pos,
        }],
        None,
    )
    .unwrap()
    .0
}

fn multi_set(text: &str, ranges: &[[usize; 3]]) -> SelectionSet {
    let candidates: Vec<_> = ranges
        .iter()
        .map(|[id, anchor, head]| RawSelection {
            id: Some((*id).try_into().unwrap()),
            anchor: *anchor,
            head: *head,
        })
        .collect();
    SelectionSet::normalize(text, candidates, None).unwrap().0
}

// E-9 spec tests: §AC-3 table verbatim

#[test]
fn number_inc_increments_single_number() {
    let text = "The answer is 42";
    let set = set_of(text, 14..16);
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    assert_eq!(prepared.after_text, "The answer is 43");
}

#[test]
fn number_dec_decrements_single_number() {
    let text = "Count: 10";
    let set = set_of(text, 7..9);
    let prepared = plan(text, &set, &EditOperation::NumberDec).unwrap();
    assert_eq!(prepared.after_text, "Count: 9");
}

#[test]
fn number_dec_preserves_leading_zeros() {
    let text = "Count: 007";
    let set = set_of(text, 7..10);
    let prepared = plan(text, &set, &EditOperation::NumberDec).unwrap();
    assert_eq!(prepared.after_text, "Count: 006");
}

#[test]
fn number_sequence_numbers_members() {
    let text = "a 0 b 0 c 0";
    let set = multi_set(
        text,
        &[
            [1, 2, 3],   // first "0"
            [2, 6, 7],   // second "0"
            [3, 10, 11], // third "0"
        ],
    );
    let prepared = plan(text, &set, &EditOperation::NumberSequence).unwrap();
    assert_eq!(prepared.after_text, "a 1 b 2 c 3");
}

// Additional E-9 boundary tests

#[test]
fn number_inc_width_grows_past_capacity() {
    let text = "099";
    let set = set_of(text, 0..3);
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    assert_eq!(prepared.after_text, "100");
}

#[test]
fn number_dec_minus_zero() {
    let text = "-0";
    let set = set_of(text, 0..2);
    let prepared = plan(text, &set, &EditOperation::NumberDec).unwrap();
    assert_eq!(prepared.after_text, "-1");
}

#[test]
fn number_inc_overflow_skips() {
    // NumberInc/NumberDec silently skip members with overflow
    let text = "9223372036854775807";
    let set = set_of(text, 0..text.len());
    let result = plan(text, &set, &EditOperation::NumberInc);
    // Should succeed with no changes (silently skipped)
    assert!(result.is_ok());
    let prepared = result.unwrap();
    assert_eq!(prepared.after_text, text);
    assert_eq!(prepared.ops.len(), 0);
}

#[test]
fn number_dec_underflow_skips() {
    // NumberInc/NumberDec silently skip members with overflow
    let text = "-9223372036854775808";
    let set = set_of(text, 0..text.len());
    let result = plan(text, &set, &EditOperation::NumberDec);
    // Should succeed with no changes (silently skipped)
    assert!(result.is_ok());
    let prepared = result.unwrap();
    assert_eq!(prepared.after_text, text);
    assert_eq!(prepared.ops.len(), 0);
}

#[test]
fn number_sequence_refuses_when_member_lacks_number() {
    let text = "a 0 b X c 0";
    let set = multi_set(
        text,
        &[
            [1, 2, 3],   // first "0"
            [2, 6, 7],   // "X" - no number
            [3, 10, 11], // third "0"
        ],
    );
    let result = plan(text, &set, &EditOperation::NumberSequence);
    assert!(matches!(result, Err(PlanError::SequenceNoNumber)));
}

#[test]
fn number_inc_mid_word_digits() {
    let text = "abc123def";
    let set = set_of(text, 0..text.len());
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    assert_eq!(prepared.after_text, "abc124def");
}

#[test]
fn number_caret_scans_forward() {
    let text = "abc123def";
    let set = set_caret(text, 3);
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    assert_eq!(prepared.after_text, "abc124def");
}

#[test]
fn number_caret_scans_backward() {
    let text = "123abc";
    let set = set_caret(text, 3);
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    assert_eq!(prepared.after_text, "124abc");
}

#[test]
fn number_sign_must_be_adjacent() {
    let text = "a -5";
    let set = set_of(text, 0..4);
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    assert_eq!(prepared.after_text, "a -4");
}

#[test]
fn number_sign_not_adjacent_finds_digit() {
    let text = "a- 5";
    let set = set_of(text, 0..text.len());
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    // Should find and increment "5"
    assert_eq!(prepared.after_text, "a- 6");
}

#[test]
fn number_sequence_document_order() {
    // Test that sequence uses document order, not selection order
    let text = "c 0 b 0 a 0";
    // text indices: 0=c, 1= , 2=0, 3= , 4=b, 5= , 6=0, 7= , 8=a, 9= , 10=0
    let set = multi_set(
        text,
        &[
            [1, 10, 11], // last "0" at position 10
            [2, 6, 7],   // middle "0" at position 6
            [3, 2, 3],   // first "0" at position 2
        ],
    );
    let prepared = plan(text, &set, &EditOperation::NumberSequence).unwrap();
    // Document order: position 2 (first 0), then 6 (middle 0), then 10 (last 0)
    // So position 2: 0->1, position 6: 0->2, position 10: 0->3
    assert_eq!(prepared.after_text, "c 1 b 2 a 3");
}

// CRLF adjacency test
#[test]
fn number_scans_across_crlf() {
    let text = "123\r\n456";
    let set = set_of(text, 0..text.len());
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    // Should find "123" and increment it
    assert_eq!(prepared.after_text, "124\r\n456");
}

// +7 test (positive sign)
#[test]
fn number_scans_positive_sign() {
    let text = "+7";
    let set = set_of(text, 0..2);
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    assert_eq!(prepared.after_text, "+8");
}

// 1e3 non-match test (1e3 is not a valid integer, but "1" is)
#[test]
fn number_finds_number_in_invalid_format() {
    let text = "1e3";
    let set = set_of(text, 0..3);
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    // "1" is a valid number, so it will be incremented to "2"
    assert_eq!(prepared.after_text, "2e3");
}

// Mid-word digits test
#[test]
fn number_finds_leftmost_in_range() {
    let text = "abc123def456";
    let set = set_of(text, 0..12);
    let prepared = plan(text, &set, &EditOperation::NumberInc).unwrap();
    // Leftmost number in range is 123
    assert_eq!(prepared.after_text, "abc124def456");
}
