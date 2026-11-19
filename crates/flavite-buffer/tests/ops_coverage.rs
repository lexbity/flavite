//! Coverage completion for the FEUX-0004/0005/0006 command families.
//!
//! Exercises production branches that the original unit tests did not reach:
//! the Lines/Surround/Indent/Whitespace planner ops, pair scanning, number
//! helpers, indentation detection/conversion, and count motions. These run
//! as integration tests so they count in the full-workspace instrumented
//! run (`just ci-extras-coverage`).

use flavite_buffer::commands::{EditOperation, PlanError, plan};
use flavite_buffer::indent::{
    Unit, conversion_would_change, dedent_amount, dedent_step, detect_unit, is_blank_or_ws,
    is_whitespace_only, leading_whitespace, spaces_to_tabs_line, tabs_to_spaces_line,
};
use flavite_buffer::motion::{Motion, apply_with_count};
use flavite_buffer::numbers::{
    extract_first_number, increment_number, number_range, number_str, scan_number,
};
use flavite_buffer::pairs::{
    Pair, SurroundValidation, find_enclosing_pair, pair_for_char, validate_enclosing_pairs,
};
use flavite_buffer::selection::{RawSelection, Selection, SelectionSet};

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

fn first_member(set: &SelectionSet) -> Selection {
    *set.iter().next().unwrap()
}

// ---------------------------------------------------------------------------
// plan(): Lines family
// ---------------------------------------------------------------------------

#[test]
fn plan_lines_sort_asc_desc_numeric() {
    let text = "banana\napple\ncherry\n";
    let set = set_of(text, 0..text.len());
    let asc = plan(text, &set, &EditOperation::LinesSortAsc).unwrap();
    assert_eq!(asc.after_text, "apple\nbanana\ncherry\n");
    let desc = plan(text, &set, &EditOperation::LinesSortDesc).unwrap();
    assert_eq!(desc.after_text, "cherry\nbanana\napple\n");
    let nums = "10\n2\n30\n";
    let set = set_of(nums, 0..nums.len());
    let numeric = plan(nums, &set, &EditOperation::LinesSortNumeric).unwrap();
    assert_eq!(numeric.after_text, "2\n10\n30\n");
}

#[test]
fn plan_lines_sort_numeric_scans_whole_line() {
    // E-8: the numeric key is the first integer found anywhere in the line
    // (sign-aware), not just a line-leading integer.
    let nums = "item 10\nitem 2\nitem 30\n";
    let set = set_of(nums, 0..nums.len());
    let numeric = plan(nums, &set, &EditOperation::LinesSortNumeric).unwrap();
    assert_eq!(numeric.after_text, "item 2\nitem 10\nitem 30\n");
    let signed = "x -5\ny +3\nz 1\n";
    let set = set_of(signed, 0..signed.len());
    let numeric = plan(signed, &set, &EditOperation::LinesSortNumeric).unwrap();
    assert_eq!(numeric.after_text, "x -5\nz 1\ny +3\n");
}

#[test]
fn plan_lines_join_duplicate_dedupe() {
    let text = "a\nb\nc\n";
    let set = set_of(text, 0..text.len());
    let joined = plan(text, &set, &EditOperation::LinesJoin).unwrap();
    assert_eq!(joined.after_text, "a b c\n");
    let dup = plan(text, &set, &EditOperation::LinesDuplicate).unwrap();
    assert_eq!(dup.after_text, "a\nb\nc\na\nb\nc\n");
    let duped = "x\nx\ny\nx\n";
    let set = set_of(duped, 0..duped.len());
    let dedupe = plan(duped, &set, &EditOperation::LinesDedupe).unwrap();
    assert_eq!(dedupe.after_text, "x\ny\n");
}

#[test]
fn plan_lines_shuffle_is_a_permutation() {
    let text = "a\nb\nc\nd\ne\n";
    let set = set_of(text, 0..text.len());
    let shuffled = plan(text, &set, &EditOperation::LinesShuffle).unwrap();
    let mut after: Vec<_> = shuffled.after_text.lines().collect();
    after.sort_unstable();
    let mut before: Vec<_> = text.lines().collect();
    before.sort_unstable();
    assert_eq!(after, before);
    assert_ne!(shuffled.after_text, text, "shuffle must not be identity");
}

#[test]
fn plan_lines_move_up_down() {
    let text = "a\nb\nc\n";
    let set = set_of(text, 2..3); // "b"
    let up = plan(text, &set, &EditOperation::LinesMoveUp).unwrap();
    assert_eq!(up.after_text, "b\na\nc\n");
    let set = set_of(text, 2..3);
    let down = plan(text, &set, &EditOperation::LinesMoveDown).unwrap();
    assert_eq!(down.after_text, "a\nc\nb\n");
}

#[test]
fn plan_lines_move_up_clamps_first_line_with_refusal() {
    let text = "a\nb\nc\n";
    let set = set_of(text, 0..1); // first line
    let result = plan(text, &set, &EditOperation::LinesMoveUp);
    assert!(matches!(result, Err(PlanError::MoveClamp)));
}

#[test]
fn plan_lines_move_down_clamps_last_line_with_refusal() {
    let text = "a\nb\nc\n";
    let set = set_of(text, 4..5); // last line
    let result = plan(text, &set, &EditOperation::LinesMoveDown);
    assert!(matches!(result, Err(PlanError::MoveClamp)));
}

#[test]
fn plan_lines_move_up_refuses_overlapping_targets() {
    // Two adjacent selected blocks (lines 3 and 4): moving both up sends the
    // upper block's target into nothing, but the lower block's target IS the
    // upper block (E-8 §5.7 overlap precheck refuses whole).
    let text = "w\nx\na\nb\ny\nz\n";
    let set = multi_set(
        text,
        &[[1, 4, 6], [2, 6, 8]], // "a\n" and "b\n"
    );
    let result = plan(text, &set, &EditOperation::LinesMoveUp);
    assert!(matches!(result, Err(PlanError::MoveOverlap)));
}

#[test]
fn plan_lines_move_down_refuses_overlapping_targets() {
    let text = "w\nx\na\nb\ny\nz\n";
    let set = multi_set(
        text,
        &[[1, 4, 6], [2, 6, 8]], // "a\n" and "b\n"
    );
    let result = plan(text, &set, &EditOperation::LinesMoveDown);
    assert!(matches!(result, Err(PlanError::MoveOverlap)));
}

#[test]
fn plan_lines_move_multiple_disjoint_blocks() {
    // Two disjoint blocks (lines 1 and 3) move up independently; the swap
    // pairs do not intersect, so the batch composes.
    let text = "a\nb\nc\nd\ne\n";
    let set = multi_set(
        text,
        &[[1, 2, 4], [2, 6, 8]], // "b\n" and "d\n"
    );
    let moved = plan(text, &set, &EditOperation::LinesMoveUp).unwrap();
    assert_eq!(moved.after_text, "b\na\nd\nc\ne\n");
}

#[test]
fn plan_lines_align_on_delimiter() {
    let text = "a = 1\nlong = 22\n";
    let set = set_of(text, 0..text.len());
    let aligned = plan(
        text,
        &set,
        &EditOperation::LinesAlign {
            delimiter: "=".into(),
        },
    )
    .unwrap();
    assert_eq!(aligned.after_text, "a    = 1\nlong = 22\n");
}

// ---------------------------------------------------------------------------
// plan(): Surround family
// ---------------------------------------------------------------------------

#[test]
fn plan_surround_add_at_range_and_caret() {
    let text = "hello world";
    let set = set_of(text, 0..5);
    let add = plan(
        text,
        &set,
        &EditOperation::SurroundAdd {
            open: "(".into(),
            close: ")".into(),
        },
    )
    .unwrap();
    assert_eq!(add.after_text, "(hello) world");
    let set = set_caret(text, 5);
    let add = plan(
        text,
        &set,
        &EditOperation::SurroundAdd {
            open: "[".into(),
            close: "]".into(),
        },
    )
    .unwrap();
    assert_eq!(add.after_text, "hello[] world");
}

#[test]
fn plan_surround_change_and_delete() {
    let text = "(hello) world";
    let set = set_of(text, 1..6);
    let change = plan(
        text,
        &set,
        &EditOperation::SurroundChange {
            old_open: "(".into(),
            old_close: ")".into(),
            new_open: "{".into(),
            new_close: "}".into(),
        },
    )
    .unwrap();
    assert_eq!(change.after_text, "{hello} world");
    let del = plan(
        text,
        &set,
        &EditOperation::SurroundDelete {
            open: "(".into(),
            close: ")".into(),
        },
    )
    .unwrap();
    assert_eq!(del.after_text, "hello world");
    // Deleting when not enclosed is a zero-effect no-op.
    let bare = "hello world";
    let set = set_of(bare, 0..5);
    let refused = plan(
        bare,
        &set,
        &EditOperation::SurroundDelete {
            open: "(".into(),
            close: ")".into(),
        },
    )
    .unwrap();
    assert_eq!(refused.after_text, bare);
}

// ---------------------------------------------------------------------------
// plan(): Indent / whitespace family
// ---------------------------------------------------------------------------

#[test]
fn plan_indent_dedent_lines() {
    let text = "a\nb\nc\n";
    let set = set_of(text, 0..text.len());
    let indent = plan(text, &set, &EditOperation::IndentLines { amount: 1 }).unwrap();
    assert_eq!(indent.after_text, "    a\n    b\n    c\n");
    let unindent = plan(
        &indent.after_text,
        &set_of(&indent.after_text, 0..indent.after_text.len()),
        &EditOperation::DedentLines { amount: 1 },
    )
    .unwrap();
    assert_eq!(unindent.after_text, text);
    // Blank lines are left untouched.
    let blanky = "a\n\nc\n";
    let set = set_of(blanky, 0..blanky.len());
    let indent = plan(blanky, &set, &EditOperation::IndentLines { amount: 1 }).unwrap();
    assert_eq!(indent.after_text, "    a\n\n    c\n");
    // All-at-column-0 dedent is a zero-effect no-op.
    let none = plan(
        text,
        &set_of(text, 0..text.len()),
        &EditOperation::DedentLines { amount: 1 },
    )
    .unwrap();
    assert_eq!(none.after_text, text);
}

#[test]
fn plan_re_whitespace_tabs_to_spaces_and_back() {
    let text = "\talpha\n\t\tbeta\nplain\n";
    let set = set_of(text, 0..text.len());
    let to_spaces = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: false }).unwrap();
    assert_eq!(to_spaces.after_text, "    alpha\n        beta\nplain\n");
    let back = plan(
        &to_spaces.after_text,
        &set_of(&to_spaces.after_text, 0..to_spaces.after_text.len()),
        &EditOperation::ReWhitespace { to_tabs: true },
    )
    .unwrap();
    assert_eq!(back.after_text, text);
}

#[test]
fn plan_open_line_above_and_below() {
    let text = "a\nb\n";
    let set = set_caret(text, 2);
    let above = plan(text, &set, &EditOperation::OpenLine { above: true }).unwrap();
    assert_eq!(above.after_text, "a\n\nb\n");
    let set = set_caret(text, 2);
    let below = plan(text, &set, &EditOperation::OpenLine { above: false }).unwrap();
    assert_eq!(below.after_text, "a\nb\n\n");
}

// ---------------------------------------------------------------------------
// plan(): number family
// ---------------------------------------------------------------------------

#[test]
fn plan_number_sequence_assigns_document_order_indexes() {
    let text = "x=7\ny=9\n";
    let set = multi_set(text, &[[0, 2, 3], [1, 6, 7]]);
    let seq = plan(text, &set, &EditOperation::NumberSequence).unwrap();
    assert_eq!(seq.after_text, "x=1\ny=2\n");
}

#[test]
fn plan_number_inc_dec_with_leading_zeros_and_signs() {
    let text = "a=007\nb=+3\nc=-4\n";
    let set = multi_set(text, &[[0, 2, 5], [1, 8, 10], [2, 13, 15]]);
    let inc = plan(text, &set, &EditOperation::NumberInc).unwrap();
    assert_eq!(inc.after_text, "a=008\nb=+4\nc=-3\n");
    let dec = plan(text, &set, &EditOperation::NumberDec).unwrap();
    assert_eq!(dec.after_text, "a=006\nb=+2\nc=-5\n");
}

// ---------------------------------------------------------------------------
// plan(): case family edge branches
// ---------------------------------------------------------------------------

#[test]
fn plan_case_title_and_swap() {
    let text = "hello hELLo\n";
    let set = set_of(text, 0..text.len());
    let title = plan(text, &set, &EditOperation::CaseTitle).unwrap();
    assert_eq!(title.after_text, "Hello Hello\n");
    let swap = plan(text, &set, &EditOperation::CaseSwap).unwrap();
    assert_eq!(swap.after_text, "HELLO HellO\n");
}

#[test]
fn plan_case_title_uses_uax29_word_boundaries() {
    // E-10: apostrophes stay inside their word (UAX #29 MidLetter); the
    // hyphen is a word boundary, so hyphenated parts title independently;
    // uncased chars pass through.
    let text = "\"café's\" well-known 123 雪\n";
    let set = set_of(text, 0..text.len());
    let title = plan(text, &set, &EditOperation::CaseTitle).unwrap();
    assert_eq!(title.after_text, "\"Café's\" Well-Known 123 雪\n");
    // A lowercase run whose first cased char is a MID-word letter still
    // uppercases only the word's first cased char.
    let text = "one TWO three\n";
    let set = set_of(text, 0..text.len());
    let title = plan(text, &set, &EditOperation::CaseTitle).unwrap();
    assert_eq!(title.after_text, "One Two Three\n");
}

// ---------------------------------------------------------------------------
// plan(): cut / delete / insert branch coverage
// ---------------------------------------------------------------------------

#[test]
fn plan_cut_whole_lines_for_carets_and_dedup_insert_points() {
    let text = "one\ntwo\nthree\n";
    let cut = plan(text, &set_caret(text, 2), &EditOperation::Cut).unwrap();
    assert_eq!(cut.after_text, "two\nthree\n");
    assert_eq!(cut.fragments, vec!["one\n".to_string()]);
    let ins = plan(
        text,
        &multi_set(text, &[[0, 0, 0], [1, 4, 4], [2, 8, 8]]),
        &EditOperation::InsertAtStarts("*".into()),
    )
    .unwrap();
    assert_eq!(ins.after_text, "*one\n*two\n*three\n");
}

#[test]
fn plan_delete_edges_and_backspace_join() {
    let text = "abc def";
    let set = set_caret(text, 0);
    let del = plan(text, &set, &EditOperation::Delete).unwrap();
    assert_eq!(del.after_text, "bc def");
    // Backspace at EOL joins the next line.
    let two = "ab\ncd";
    let set = set_caret(two, 2);
    let bs = plan(two, &set, &EditOperation::Backspace).unwrap();
    assert_eq!(bs.after_text, "a\ncd");
}

// ---------------------------------------------------------------------------
// pairs
// ---------------------------------------------------------------------------

#[test]
fn pair_from_open_and_close_cover_all_pair_chars() {
    for c in ['(', '[', '{', '<', '"', '\'', '`'] {
        let p = Pair::from_open(c).expect("open pair");
        assert_eq!(p.open, c);
        assert_eq!(
            Pair::from_open(c).unwrap().close,
            Pair::from_open(c).unwrap().close
        );
    }
    for c in [')', ']', '}', '>', '"', '\'', '`'] {
        let p = Pair::from_close(c).expect("close pair");
        assert_eq!(p.close, c);
    }
    assert!(Pair::from_open('x').is_none());
    assert!(Pair::from_close('x').is_none());
    assert_eq!(pair_for_char('(').open, '(');
    assert_eq!(pair_for_char(')').close, ')');
    assert!(pair_for_char('(').is_nesting);
    assert!(!pair_for_char('"').is_nesting);
}

#[test]
fn find_enclosing_nesting_pair_picks_innermost() {
    let text = "outer (middle [inner] middle) outer";
    let set = set_caret(text, 18); // inside "inner"
    let member = first_member(&set);
    let pair = Pair::from_open('[').unwrap();
    let m = find_enclosing_pair(text, &member, &pair).expect("enclosing [ ]");
    assert_eq!(&text[m.open_start..m.open_end], "[");
    assert_eq!(&text[m.close_start..m.close_end], "]");
    // The innermost enclosing pair is the bracket, not the paren.
    assert_eq!(m.open_start, 14);
    // No enclosing pair for a selection outside any pair.
    let set = set_caret(text, 0);
    let member = first_member(&set);
    assert!(find_enclosing_pair(text, &member, &pair).is_none());
}

#[test]
fn find_enclosing_self_pair_respects_escapes() {
    let text = r#"say "hi \" there" now"#;
    let set = set_caret(text, 7); // inside the quoted span, before the escape
    let member = first_member(&set);
    let pair = Pair::from_open('"').unwrap();
    let m = find_enclosing_pair(text, &member, &pair).expect("enclosing quotes");
    assert_eq!(&text[m.open_start..m.open_end], "\"");
    assert_eq!(&text[m.close_start..m.close_end], "\"");
}

#[test]
fn validate_enclosing_pairs_reports_each_member() {
    let text = "x (a) y";
    let set = multi_set(text, &[[0, 3, 4]]);
    let pair = Pair::from_open('(').unwrap();
    let ok = validate_enclosing_pairs(text, &set, &pair);
    assert!(matches!(ok, SurroundValidation::Valid));
    let text = "x (a y";
    let set = multi_set(text, &[[0, 3, 4]]);
    let bad = validate_enclosing_pairs(text, &set, &pair);
    assert!(matches!(bad, SurroundValidation::MissingPairs { .. }));
}

// ---------------------------------------------------------------------------
// numbers
// ---------------------------------------------------------------------------

#[test]
fn scan_number_and_helpers() {
    let text = "  -42 ";
    let set = set_of(text, 0..text.len());
    let member = first_member(&set);
    let scanned = scan_number(text, &member).expect("scan");
    assert_eq!(scanned.value, -42);
    assert!(scanned.has_sign);
    let (start, end) = number_range(&scanned);
    assert_eq!(&text[start..end], "-42");
    assert_eq!(number_str(text, &scanned), "-42");
    assert_eq!(extract_first_number("-42"), Some(-42));
    assert_eq!(extract_first_number("+7"), Some(7));
    assert_eq!(extract_first_number("abc"), None);
    assert_eq!(extract_first_number(""), None);
}

#[test]
fn increment_number_preserves_width_and_sign() {
    let text = "007";
    let set = set_of(text, 0..3);
    let member = first_member(&set);
    let scanned = scan_number(text, &member).unwrap();
    assert_eq!(increment_number(text, &scanned, 3).unwrap(), "010");
    let text = "+099";
    let set = set_of(text, 0..4);
    let member = first_member(&set);
    let scanned = scan_number(text, &member).unwrap();
    assert_eq!(increment_number(text, &scanned, 1).unwrap(), "+100");
    let text = "9";
    let set = set_of(text, 0..1);
    let member = first_member(&set);
    let scanned = scan_number(text, &member).unwrap();
    assert!(increment_number(text, &scanned, 1).is_ok());
}

// ---------------------------------------------------------------------------
// indent
// ---------------------------------------------------------------------------

#[test]
fn detect_unit_tabs_spaces_mixed_and_fallback() {
    assert_eq!(detect_unit("\t\tx").text(2), "\t\t");
    assert!(detect_unit("        x").text(1).chars().all(|c| c == ' '));
    assert!(detect_unit(" \t x").text(1).chars().all(|c| c == ' '));
    assert!(!detect_unit("no indent").text(2).is_empty());
    assert!(!detect_unit("").text(1).is_empty());
    assert!(Unit::fallback().text(1).starts_with(' '));
    assert!(!Unit::fallback().display().is_empty());
}

#[test]
fn dedent_step_and_amount() {
    assert_eq!(dedent_step("    ", 4), "");
    assert_eq!(dedent_step("        ", 4), "    ");
    assert_eq!(dedent_step("   ", 4), "");
    assert_eq!(dedent_step("\t", 4), "");
    assert_eq!(dedent_amount("    ", 4), 4);
    assert_eq!(dedent_amount("   ", 4), 3);
    assert_eq!(dedent_amount("", 4), 0);
    assert!(is_whitespace_only("   "));
    assert!(is_blank_or_ws(""));
    assert!(!is_blank_or_ws(" x"));
    assert_eq!(leading_whitespace("   x"), "   ");
}

#[test]
fn whitespace_line_conversions() {
    assert_eq!(tabs_to_spaces_line("\tx", 4), "    x");
    assert_eq!(tabs_to_spaces_line("a\tb", 4), "a\tb"); // content tab untouched
    assert_eq!(spaces_to_tabs_line("    x", 4), "\tx");
    assert_eq!(spaces_to_tabs_line("  x", 4), "  x"); // partial tab stays
    assert!(conversion_would_change("    x", true, 4));
    assert!(conversion_would_change("\tx", false, 4));
    assert!(!conversion_would_change("x", true, 4));
    assert!(!conversion_would_change("\tx", true, 4));
    assert!(!conversion_would_change("x", false, 4));
}

// ---------------------------------------------------------------------------
// motion
// ---------------------------------------------------------------------------

#[test]
fn apply_with_count_text_motions_clamp_and_line_targets() {
    let text = "abcd\nefgh\n";
    let set = set_caret(text, 0);
    let moved = apply_with_count(text, &set, Motion::CharRight, false, None, 3).unwrap();
    assert_eq!(moved.primary().head(), 3);
    let over = apply_with_count(text, &set, Motion::CharRight, false, None, 100).unwrap();
    assert_eq!(over.primary().head(), text.len()); // clamped at document end
    let line = apply_with_count(text, &set, Motion::Line(2), false, None, 1).unwrap();
    assert_eq!(line.primary().head(), 5);
    let zero = apply_with_count(text, &set, Motion::CharRight, false, None, 0).unwrap();
    assert_eq!(zero.primary().head(), 0);
    let extend = apply_with_count(text, &set, Motion::CharRight, true, None, 2).unwrap();
    let m = extend.primary();
    assert_eq!((m.anchor(), m.head()), (0, 2));
}
