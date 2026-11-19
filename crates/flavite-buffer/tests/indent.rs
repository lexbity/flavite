//! FEUX-0005 indent/whitespace acceptance tests (AC-1…AC-5) plus the
//! property harness (NFR-2/3): detection matrix, indent/dedent matrix,
//! conversion matrix, auto-indent matrix, purity double-run, and the
//! proptest suite (UTF-8 validity, ascending non-overlap, after-set
//! validates, undo round-trip byte-identical).

use flavite_buffer::commands::{EditOperation, PreparedEdit, plan};
use flavite_buffer::indent::{Style, Unit, detect_unit};
use flavite_buffer::selection::{RawSelection, SelectionSet};
use proptest::prelude::*;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn set_of(text: &str, at: usize) -> SelectionSet {
    SelectionSet::normalize(
        text,
        vec![RawSelection {
            id: None,
            anchor: at,
            head: at,
        }],
        None,
    )
    .unwrap()
    .0
}

fn set_full(text: &str) -> SelectionSet {
    SelectionSet::normalize(
        text,
        vec![RawSelection {
            id: None,
            anchor: 0,
            head: text.len(),
        }],
        None,
    )
    .unwrap()
    .0
}

fn assert_unit(text: &str, style: Style, width: u8) {
    let unit = detect_unit(text);
    assert_eq!(unit.style, style, "style for {text:?}");
    assert_eq!(unit.width, width, "width for {text:?}");
}

// ---------------------------------------------------------------------------
// AC-1: detection matrix
// ---------------------------------------------------------------------------

#[test]
fn detection_pure_tab() {
    assert_unit("\tfn main() {\n\t\tprintln!();\n\t}\n", Style::Tab, 4);
}

#[test]
fn detection_4_space() {
    assert_unit(
        "    fn main() {\n        println!();\n    }\n",
        Style::Space,
        4,
    );
}

#[test]
fn detection_2_space() {
    assert_unit("  fn main() {\n    println!();\n  }\n", Style::Space, 2);
}

#[test]
fn detection_mixed_tab_space_majority() {
    assert_unit("\tfn a() {}\n\tfn b() {}\n  fn c() {}\n", Style::Tab, 2);
    assert_unit("  fn a() {}\n  fn b() {}\n\tfn c() {}\n", Style::Space, 2);
}

#[test]
fn detection_whitespace_only_noise_excluded() {
    // Whitespace-only lines vote neither style nor width.
    assert_unit("    \n\t\n  \nfn a()\n", Style::Space, 4);
}

#[test]
fn detection_alignment_padding_excluded_by_cap() {
    // 12-space alignment lines are excluded from width candidates (style only).
    assert_unit(
        "    fn main() {\n    let x:    i32 = 1;\n}\n",
        Style::Space,
        4,
    );
    assert_unit("         x\n          y\n", Style::Space, 4);
}

#[test]
fn detection_flush_left_prose_fallback() {
    assert_unit("one\ntwo\nthree\n", Style::Space, 4);
    assert_unit("", Style::Space, 4);
}

#[test]
fn detection_space_first_mixed_run_no_width_candidate() {
    // E-2: spaces-then-tab runs vote space but contribute no width candidate.
    assert_unit("  \tfn a() {}\n  \tfn b() {}\n", Style::Space, 4);
}

#[test]
fn detection_mode_smallest_on_tie() {
    assert_unit("  a\n  b\n    c\n    d\n", Style::Space, 2);
}

// ---------------------------------------------------------------------------
// AC-2: indent/dedent matrix
// ---------------------------------------------------------------------------

#[test]
fn indent_blank_lines_untouched() {
    let text = "    a\n\n    b\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::IndentLines { amount: 1 }).unwrap();
    assert_eq!(prepared.after_text, "        a\n\n        b\n");
}

#[test]
fn indent_whitespace_only_lines_untouched() {
    let text = "    a\n    \n    b\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::IndentLines { amount: 1 }).unwrap();
    assert_eq!(prepared.after_text, "        a\n    \n        b\n");
}

#[test]
fn indent_count_units_in_one_plan() {
    let text = "a\nb\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::IndentLines { amount: 3 }).unwrap();
    assert_eq!(prepared.after_text, "            a\n            b\n");
}

#[test]
fn dedent_tab_stop_floor() {
    // Width is detected as 4 (two 4-space lines dominate). Then:
    // 5@4 → 4, 2@4 → 0, 7@4 → 4, 4@4 → 0.
    let text = "     a\n  b\n       c\n    d\n    e\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::DedentLines { amount: 1 }).unwrap();
    assert_eq!(prepared.after_text, "    a\nb\n    c\nd\ne\n");
}

#[test]
fn dedent_tab_and_mixed_runs_drop_first_tab() {
    // "\t" → ""; "  \t" → "  " (preceding spaces stay).
    let text = "\ta\n  \tb\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::DedentLines { amount: 1 }).unwrap();
    assert_eq!(prepared.after_text, "a\n  b\n");
}

#[test]
fn dedent_blank_lines_untouched() {
    let text = "    a\n\n    b\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::DedentLines { amount: 1 }).unwrap();
    assert_eq!(prepared.after_text, "a\n\nb\n");
}

#[test]
fn dedent_whitespace_only_lines_untouched() {
    let text = "    a\n    \n    b\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::DedentLines { amount: 1 }).unwrap();
    assert_eq!(prepared.after_text, "a\n    \nb\n");
}

#[test]
fn dedent_preserves_unicode_whitespace_content() {
    // The content after the ASCII leading run must survive byte-exactly
    // (never trim_start(): NBSP is content here, not indentation).
    let text = "  a\u{00A0}b\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::DedentLines { amount: 1 }).unwrap();
    assert_eq!(prepared.after_text, "a\u{00A0}b\n");
}

#[test]
fn dedent_all_at_column_zero_is_silent_noop() {
    let text = "a\nb\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::DedentLines { amount: 1 }).unwrap();
    assert!(
        prepared.edits.is_empty(),
        "zero-effect dedent plans no edits"
    );
    assert_eq!(prepared.after_text, text);
    assert_eq!(prepared.unit, Some(Unit::fallback()));
}

#[test]
fn dedent_count_stops_at_column_zero_per_line() {
    let text = "    a\n  b\nc\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::DedentLines { amount: 3 }).unwrap();
    assert_eq!(prepared.after_text, "a\nb\nc\n");
}

#[test]
fn indent_dedent_crlf_keeps_eol_bytes() {
    let text = "    a\r\n    b\r\n";
    let set = set_full(text);
    let indented = plan(text, &set, &EditOperation::IndentLines { amount: 1 }).unwrap();
    assert_eq!(indented.after_text, "        a\r\n        b\r\n");
    let dedented = plan(text, &set, &EditOperation::DedentLines { amount: 1 }).unwrap();
    assert_eq!(dedented.after_text, "a\r\nb\r\n");
}

#[test]
fn indent_eof_final_line_without_newline() {
    let text = "a\n    b";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::IndentLines { amount: 1 }).unwrap();
    assert_eq!(prepared.after_text, "    a\n        b");
}

#[test]
fn indent_dedent_report_the_used_unit() {
    let text = "    a\n    b\n";
    let set = set_full(text);
    let indented = plan(text, &set, &EditOperation::IndentLines { amount: 1 }).unwrap();
    assert_eq!(
        indented.unit,
        Some(Unit {
            style: Style::Space,
            width: 4,
        })
    );
    let tabbed = plan(
        "\ta\n\tb\n",
        &set_full("\ta\n\tb\n"),
        &EditOperation::IndentLines { amount: 1 },
    )
    .unwrap();
    assert_eq!(
        tabbed.unit,
        Some(Unit {
            style: Style::Tab,
            width: 4,
        })
    );
}

// ---------------------------------------------------------------------------
// AC-4: conversion matrix
// ---------------------------------------------------------------------------

#[test]
fn tabs_to_spaces_leading_only_content_tabs_untouched() {
    let text = "\tfn main() {\tprintln!();\n\t}\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: false }).unwrap();
    assert_eq!(prepared.after_text, "    fn main() {\tprintln!();\n    }\n");
}

#[test]
fn spaces_to_tabs_greedy_with_remainder() {
    // Width is detected as 4 (two 4-space lines dominate); 6@4 → "\t  "
    // (one tab + 2-space remainder).
    let text = "    a\n    b\n      c\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: true }).unwrap();
    assert_eq!(prepared.after_text, "\ta\n\tb\n\t  c\n");
}

#[test]
fn spaces_to_tabs_exact_multiples_and_short_remainder() {
    // Width 4 (two 4-space lines dominate): 8 → two tabs; 3 stays spaces.
    let text = "    z\n    w\n        a\n   b\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: true }).unwrap();
    assert_eq!(prepared.after_text, "\tz\n\tw\n\t\ta\n   b\n");
}

#[test]
fn conversion_whitespace_only_lines_are_included() {
    let text = "    a\n    \n\tb\n";
    let set = set_full(text);
    let to_tabs = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: true }).unwrap();
    assert_eq!(to_tabs.after_text, "\ta\n\t\n\tb\n");
    let to_spaces = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: false }).unwrap();
    assert_eq!(to_spaces.after_text, "    a\n    \n    b\n");
}

#[test]
fn conversion_crlf_eol_bytes_identical() {
    let text = "\ta\r\n\tb\r\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: false }).unwrap();
    assert_eq!(prepared.after_text, "    a\r\n    b\r\n");
}

#[test]
fn conversion_nothing_to_convert_is_silent_noop() {
    // No leading whitespace anywhere: neither direction has anything to
    // convert → zero-effect no-op (no edits).
    let text = "a\nb\n";
    let set = set_full(text);
    let prepared = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: true }).unwrap();
    assert!(prepared.edits.is_empty());
    assert_eq!(prepared.after_text, text);
    let prepared = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: false }).unwrap();
    assert!(prepared.edits.is_empty());
    assert_eq!(prepared.after_text, text);
}

#[test]
fn conversion_roundtrip_preserves_visible_columns() {
    // Byte-lossless in visible columns: tabs*width + spaces before content
    // must survive spaces_to_tabs → tabs_to_spaces.
    let text = "    a\n      b\n\tc\n  \td\n";
    let set = set_full(text);
    let to_tabs = plan(text, &set, &EditOperation::ReWhitespace { to_tabs: true }).unwrap();
    let back = plan(
        &to_tabs.after_text,
        &set_full(&to_tabs.after_text),
        &EditOperation::ReWhitespace { to_tabs: false },
    )
    .unwrap();
    let col = |line: &str| -> usize {
        let mut n = 0;
        for ch in line.chars() {
            match ch {
                '\t' => n += 4,
                ' ' => n += 1,
                _ => break,
            }
        }
        n
    };
    for (before, after) in text.lines().zip(back.after_text.lines()) {
        assert_eq!(col(before), col(after), "column loss for {before:?}");
    }
}

// ---------------------------------------------------------------------------
// AC-5: auto-indent matrix
// ---------------------------------------------------------------------------

#[test]
fn auto_indent_below_on_indented_line() {
    let text = "    fn main() {}\n";
    let prepared = plan(
        text,
        &set_of(text, 0),
        &EditOperation::OpenLine { above: false },
    )
    .unwrap();
    assert_eq!(prepared.after_text, "    fn main() {}\n    \n");
    assert_eq!(
        prepared.after_set.primary().head(),
        21,
        "caret after copied ws"
    );
}

#[test]
fn auto_indent_above_on_indented_line() {
    let text = "    fn main() {}\n";
    let prepared = plan(
        text,
        &set_of(text, 0),
        &EditOperation::OpenLine { above: true },
    )
    .unwrap();
    assert_eq!(prepared.after_text, "    \n    fn main() {}\n");
    assert_eq!(
        prepared.after_set.primary().head(),
        4,
        "caret after copied ws"
    );
}

#[test]
fn auto_indent_mid_line_split_yields_ws_plus_tail() {
    let text = "    foo bar";
    let prepared = plan(
        text,
        &set_of(text, 8),
        &EditOperation::OpenLine { above: false },
    )
    .unwrap();
    assert_eq!(prepared.after_text, "    foo \n    bar");
    assert_eq!(prepared.after_set.primary().head(), 13);
}

#[test]
fn auto_indent_whitespace_only_line_copies_verbatim() {
    let text = "    \n";
    let prepared = plan(
        text,
        &set_of(text, 0),
        &EditOperation::OpenLine { above: false },
    )
    .unwrap();
    assert_eq!(prepared.after_text, "    \n    \n");
    assert_eq!(prepared.after_set.primary().head(), 9);
}

// ---------------------------------------------------------------------------
// NFR-2: purity double-run
// ---------------------------------------------------------------------------

#[test]
fn purity_double_run_produces_identical_plans() {
    let texts = [
        "    a\n    b\n",
        "\ta\n\tb\n",
        "  a\n    b\n",
        "a\nb\n",
        "    a\r\n    b\r\n",
    ];
    let ops = [
        EditOperation::IndentLines { amount: 2 },
        EditOperation::DedentLines { amount: 1 },
        EditOperation::ReWhitespace { to_tabs: true },
        EditOperation::ReWhitespace { to_tabs: false },
        EditOperation::OpenLine { above: false },
        EditOperation::OpenLine { above: true },
    ];
    for text in texts {
        for op in &ops {
            let set = set_full(text);
            let first = plan(text, &set, op).unwrap();
            let second = plan(text, &set, op).unwrap();
            assert_eq!(first, second, "double-run purity for {op:?} on {text:?}");
        }
    }
}

// ---------------------------------------------------------------------------
// NFR-3: property tests
// ---------------------------------------------------------------------------

fn arb_doc() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just("a".to_string()),
            Just("é".to_string()),
            Just("漢".to_string()),
            Just("\n".to_string()),
            Just("\t".to_string()),
            Just(" ".to_string()),
            Just("  ".to_string()),
            Just("    ".to_string()),
            Just("b".to_string()),
            Just("\r\n".to_string()),
        ],
        0..40,
    )
    .prop_map(|units| units.concat())
}

fn arb_op() -> impl Strategy<Value = EditOperation> {
    prop_oneof![
        any::<u8>().prop_map(|n| EditOperation::IndentLines {
            amount: (n % 4 + 1) as u32,
        }),
        any::<u8>().prop_map(|n| EditOperation::DedentLines {
            amount: (n % 4 + 1) as u32,
        }),
        Just(EditOperation::ReWhitespace { to_tabs: true }),
        Just(EditOperation::ReWhitespace { to_tabs: false }),
        Just(EditOperation::OpenLine { above: false }),
        Just(EditOperation::OpenLine { above: true }),
    ]
}

fn grapheme_boundaries(text: &str) -> Vec<usize> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut v: Vec<usize> = text.grapheme_indices(true).map(|(at, _)| at).collect();
    v.push(text.len());
    v
}

/// Forward-apply a plan's ops and reverse-apply the inverse ops; the result
/// must be byte-identical to the original text.
fn undo_roundtrip(text: &str, prepared: &PreparedEdit) -> bool {
    if prepared.ops.is_empty() {
        return prepared.after_text == text;
    }
    // Forward application with captured originals and per-op shifted regions.
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0usize;
    let mut regions: Vec<(usize, usize, String)> = Vec::with_capacity(prepared.ops.len());
    let mut shift: isize = 0;
    for op in &prepared.ops {
        let s = op.range.start.0;
        let e = op.range.end.0;
        out.push_str(&text[cursor..s]);
        out.push_str(&op.replacement);
        let shifted_s = (s as isize + shift) as usize;
        regions.push((shifted_s, op.replacement.len(), text[s..e].to_string()));
        shift += op.replacement.len() as isize - (e - s) as isize;
        cursor = e;
    }
    out.push_str(&text[cursor..]);
    if out != prepared.after_text {
        return false;
    }
    // Reverse application: undo later ops first.
    let mut undo_out = String::with_capacity(out.len());
    let mut ucursor = 0usize;
    for (s, rl, original) in regions.iter().rev() {
        undo_out.push_str(&out[ucursor..*s]);
        undo_out.push_str(original);
        ucursor = s + rl;
    }
    undo_out.push_str(&out[ucursor..]);
    undo_out == text
}

proptest! {
    #[test]
    fn indent_properties(
        text in arb_doc(),
        op in arb_op(),
        seed in any::<u64>(),
    ) {
        let boundaries = grapheme_boundaries(&text);
        let lcg = seed;
        // One random caret at a grapheme boundary; occasionally a full-range
        // selection for block operations.
        let caret = if boundaries.is_empty() {
            0
        } else {
            let idx = (lcg % boundaries.len() as u64) as usize;
            boundaries[idx]
        };
        let set = if lcg % 4 == 0 {
            set_full(&text)
        } else {
            set_of(&text, caret)
        };
        let prepared = match plan(&text, &set, &op) {
            Ok(prepared) => prepared,
            Err(_) => return Ok(()),
        };

        // NFR-3: UTF-8 validity.
        prop_assert!(std::str::from_utf8(prepared.after_text.as_bytes()).is_ok());

        // NFR-3: ascending, non-overlapping edits.
        let mut prev_end = 0usize;
        for edit in &prepared.edits {
            prop_assert!(edit.start <= edit.end, "start <= end");
            prop_assert!(edit.start >= prev_end, "ascending non-overlap");
            prev_end = edit.end;
        }

        // NFR-3: after-set validates against the after-text.
        prop_assert!(
            prepared.after_set.validate_against(&prepared.after_text).is_ok(),
            "after-set validates"
        );

        // NFR-3: undo round-trips byte-identically.
        prop_assert!(
            undo_roundtrip(&text, &prepared),
            "undo round-trip byte-identical"
        );
    }

    #[test]
    fn conversion_visible_columns_preserved(
        text in arb_doc(),
        width in 1u8..=8,
    ) {
        // FEUX-0005 §5.3: for a fixed width, leading-whitespace conversion is
        // byte-lossless in visible columns — spaces_to_tabs_line then
        // tabs_to_spaces_line at the same width preserves the column count.
        use flavite_buffer::indent::{spaces_to_tabs_line, tabs_to_spaces_line};
        let w = width as usize;
        let col = |line: &str| -> usize {
            let mut n = 0;
            for ch in line.chars() {
                match ch {
                    '\t' => n += w,
                    ' ' => n += 1,
                    _ => break,
                }
            }
            n
        };
        for line in text.lines() {
            let converted = spaces_to_tabs_line(line, width);
            let restored = tabs_to_spaces_line(&converted, width);
            prop_assert_eq!(col(line), col(&restored), "column loss for {:?}", line);
        }
    }
}
