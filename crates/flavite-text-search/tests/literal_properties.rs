//! Literal matcher and template properties (RFC-0006 S01, D-05).
//!
//! The property tests compare the case-sensitive literal matcher against an
//! independent straightforward byte scan implemented here, not against
//! another copy of the matcher. Pinned examples fix the documented simple
//! case-folding, whole-word, limit and template behaviors, and the span
//! tests prove match coordinates are the existing `flavite-syntax`
//! `Span<TextByte>`/`TextSnapshot` types (no second offset model).

use std::collections::BTreeSet;

use proptest::prelude::*;
use unicode_segmentation::UnicodeSegmentation;

use flavite_syntax::text::{SourceByte, TextByte, TextSnapshot};
use flavite_text_search::matcher::{
    MAX_QUERY_BYTES, MatchError, MatchMode, MatchOptions, MatchResult, find_matches,
    find_matches_checked,
};
use flavite_text_search::template::{LiteralTemplate, expand_literal, expanded_output_len};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn cs(query: &str) -> MatchOptions {
    MatchOptions {
        query: query.to_owned(),
        case_sensitive: true,
        ..MatchOptions::default()
    }
}

fn ci(query: &str) -> MatchOptions {
    MatchOptions {
        query: query.to_owned(),
        ..MatchOptions::default() // D-05 default: literal, case-insensitive, word off
    }
}

fn cs_ww(query: &str) -> MatchOptions {
    MatchOptions {
        whole_word: true,
        ..cs(query)
    }
}

fn ci_ww(query: &str) -> MatchOptions {
    MatchOptions {
        whole_word: true,
        ..ci(query)
    }
}

fn spans(result: &MatchResult) -> Vec<(usize, usize)> {
    result
        .matches
        .iter()
        .map(|m| (m.range.start.0, m.range.end.0))
        .collect()
}

fn find_cs(text: &str, query: &str) -> Vec<(usize, usize)> {
    spans(&find_matches(text, &cs(query), u64::MAX))
}

fn find_ci(text: &str, query: &str) -> Vec<(usize, usize)> {
    spans(&find_matches(text, &ci(query), u64::MAX))
}

fn find_cs_ww(text: &str, query: &str) -> Vec<(usize, usize)> {
    spans(&find_matches(text, &cs_ww(query), u64::MAX))
}

fn find_ci_ww(text: &str, query: &str) -> Vec<(usize, usize)> {
    spans(&find_matches(text, &ci_ww(query), u64::MAX))
}

// ---------------------------------------------------------------------------
// Independent reference scans (test-local, byte-driven)
// ---------------------------------------------------------------------------

/// Straightforward non-overlapping leftmost literal scan.
fn reference_scan(text: &str, query: &str) -> Vec<(usize, usize)> {
    assert!(!query.is_empty());
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + query.len() <= text.len() {
        if text[i..].starts_with(query) {
            out.push((i, i + query.len()));
            i += query.len(); // consume the match
        } else {
            i += 1;
        }
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
    }
    out
}

/// Whole-word variant of the reference scan: a candidate qualifies only when
/// both endpoints are UAX #29 word-boundary offsets; a rejected candidate
/// advances by one scalar, so occurrences inside it are still examined.
fn reference_scan_whole_word(text: &str, query: &str) -> Vec<(usize, usize)> {
    assert!(!query.is_empty());
    let mut offsets: Vec<usize> = text.split_word_bound_indices().map(|(i, _)| i).collect();
    offsets.push(text.len());
    let bounds: BTreeSet<usize> = offsets.into_iter().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + query.len() <= text.len() {
        if text[i..].starts_with(query) {
            let end = i + query.len();
            if bounds.contains(&i) && bounds.contains(&end) {
                out.push((i, end));
                i = end; // consume the accepted match
            } else {
                i += 1; // rejected: advance one scalar
            }
        } else {
            i += 1;
        }
        while i < text.len() && !text.is_char_boundary(i) {
            i += 1;
        }
    }
    out
}

proptest! {
    /// The nonempty case-sensitive literal matcher agrees exactly with the
    /// independent byte scan (spans, order, non-overlap, no truncation).
    #[test]
    fn case_sensitive_matches_reference_scan(text in text_strategy(), query in query_strategy()) {
        let options = cs(&query);
        let result = find_matches(&text, &options, u64::MAX);
        prop_assert_eq!(spans(&result), reference_scan(&text, &query));
        prop_assert!(!result.truncated);
        for pair in result.matches.windows(2) {
            prop_assert!(pair[0].range.end <= pair[1].range.start, "overlapping matches");
        }
    }

    /// Case-sensitive whole-word mode agrees with the independent scan that
    /// applies the same UAX #29 endpoint postfilter.
    #[test]
    fn whole_word_matches_reference_scan(text in text_strategy(), query in query_strategy()) {
        let options = cs_ww(&query);
        let result = find_matches(&text, &options, u64::MAX);
        prop_assert_eq!(spans(&result), reference_scan_whole_word(&text, &query));
        for pair in result.matches.windows(2) {
            prop_assert!(pair[0].range.end <= pair[1].range.start, "overlapping matches");
        }
    }

    /// The limit cap is respected and `truncated` is exact in both directions.
    #[test]
    fn limit_is_respected_and_truncation_is_exact(
        text in text_strategy(),
        query in query_strategy(),
        limit in 0u64..12,
    ) {
        let options = cs(&query);
        let full = find_matches(&text, &options, u64::MAX);
        let capped = find_matches(&text, &options, limit);
        let expected_len = usize::try_from(limit).unwrap_or(usize::MAX).min(full.matches.len());
        prop_assert_eq!(capped.matches.len(), expected_len);
        prop_assert_eq!(capped.truncated, full.matches.len() > capped.matches.len());
    }
}

fn char_pool() -> Vec<char> {
    vec![
        'a', 'b', 'c', 'A', 'B', 'C', ' ', '-', '.', '\n', '\r', 'é', 'É', 'e', 'ß', 'i', 'I', 'İ',
        'σ', 'ς', 'Σ', 'ο', 'Ο', '😀', '日',
    ]
}

fn text_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(proptest::sample::select(char_pool()), 0..48)
        .prop_map(|chars| chars.into_iter().collect())
}

fn query_strategy() -> impl Strategy<Value = String> {
    proptest::collection::vec(proptest::sample::select(char_pool()), 1..5)
        .prop_map(|chars| chars.into_iter().collect())
}

// ---------------------------------------------------------------------------
// Pinned: simple case folding (D-05), not full folding, no normalization
// ---------------------------------------------------------------------------

#[test]
fn sharp_s_never_folds_to_ss() {
    assert_eq!(find_ci("ss", "ß"), Vec::<(usize, usize)>::new());
    assert_eq!(find_ci("ß", "ss"), Vec::<(usize, usize)>::new());
    assert_eq!(find_ci("STRASSE", "Straße"), Vec::<(usize, usize)>::new());
    assert_eq!(find_ci("Straße", "STRASSE"), Vec::<(usize, usize)>::new());
    // Identical characters still match exactly (ß occupies bytes 4..6).
    assert_eq!(find_ci("Straße", "ß"), vec![(4, 6)]);
}

#[test]
fn accents_fold_by_case_but_stay_decomposition_distinct() {
    // Composed vs decomposed: distinct under D-05 (no canonical normalization).
    assert_eq!(find_ci("café", "e\u{301}"), Vec::<(usize, usize)>::new());
    assert_eq!(find_ci("cafe\u{301}", "é"), Vec::<(usize, usize)>::new());
    // Same composition folds by case.
    assert_eq!(find_ci("café", "É"), vec![(3, 5)]);
    assert_eq!(find_ci("É", "é"), vec![(0, 2)]);
}

#[test]
fn ascii_folds_case_insensitively() {
    assert_eq!(find_ci("abc", "ABC"), vec![(0, 3)]);
    assert_eq!(find_ci("aBc AbC", "abc"), vec![(0, 3), (4, 7)]);
    // Case-sensitive mode does not fold.
    assert_eq!(find_cs("abc", "ABC"), Vec::<(usize, usize)>::new());
}

#[test]
fn greek_final_sigma_folds_per_documented_behavior() {
    // The matcher folds the sigma family together: both `σ` and `ς`
    // uppercase to `Σ` under std, matching Unicode simple folding.
    assert_eq!(find_ci("σ", "ς"), vec![(0, 2)]);
    assert_eq!(find_ci("ς", "σ"), vec![(0, 2)]);
    assert_eq!(find_ci("Σ", "ς"), vec![(0, 2)]);
    assert_eq!(find_ci("ΟΣ", "ος"), vec![(0, 4)]);
    // Different Greek letters never fold together (omicron is not sigma).
    assert_eq!(find_ci("Ο", "σ"), Vec::<(usize, usize)>::new());
}

#[test]
fn dotted_capital_i_folds_only_to_itself() {
    // `İ` (U+0130) does not fold to `i`/`I` — non-Turkic simple folding.
    assert_eq!(find_ci("i", "İ"), Vec::<(usize, usize)>::new());
    assert_eq!(find_ci("I", "İ"), Vec::<(usize, usize)>::new());
    assert_eq!(find_ci("İstanbul", "İ"), vec![(0, 2)]);
}

#[test]
fn compatibility_scalars_fold_to_their_ascii_counterparts() {
    // U+212A KELVIN SIGN and U+212B ANGSTROM SIGN fold to k/å.
    assert_eq!(find_ci("k", "\u{212A}"), vec![(0, 1)]);
    assert_eq!(find_ci("å", "\u{212B}"), vec![(0, 2)]);
}

// ---------------------------------------------------------------------------
// Pinned: whole-word filtering (UAX #29 endpoints)
// ---------------------------------------------------------------------------

#[test]
fn whole_word_requires_word_segment_endpoints() {
    assert_eq!(find_cs_ww("a word here", "word"), vec![(2, 6)]);
    assert_eq!(find_cs_ww("word", "word"), vec![(0, 4)]);
    assert_eq!(find_cs_ww("word.", "word"), vec![(0, 4)]);
    assert_eq!(find_cs_ww("sword", "word"), Vec::<(usize, usize)>::new());
    assert_eq!(find_cs_ww("words", "word"), Vec::<(usize, usize)>::new());
    // Punctuation-only queries qualify when both endpoints are boundaries.
    // UAX #29 splits "-a-b-" into ["-", "a", "-", "b", "-"], so every hyphen
    // starts and ends a segment.
    assert_eq!(find_cs_ww("a-b", "-"), vec![(1, 2)]);
    assert_eq!(find_cs_ww("-a-b-", "-"), vec![(0, 1), (2, 3), (4, 5)]);
    // Multi-byte query stays whole-word correct.
    assert_eq!(find_cs_ww("café", "café"), vec![(0, 5)]);
    assert_eq!(find_cs_ww("rcaféx", "café"), Vec::<(usize, usize)>::new());
}

#[test]
fn whole_word_applies_after_case_folding() {
    assert_eq!(find_ci_ww("A WORD here", "word"), vec![(2, 6)]);
    assert_eq!(find_ci_ww("SWORD", "WORD"), Vec::<(usize, usize)>::new());
}

#[test]
fn rejected_whole_word_candidate_advances_one_scalar() {
    // "xa a a": the occurrence at 1 is rejected (byte 1 is inside the word
    // "xa"); the scan advances one scalar and still finds the bounded
    // occurrence at 3. A postfilter that resumed after the rejected match
    // would wrongly return no matches.
    assert_eq!(find_cs_ww("xa a a", "a a"), vec![(3, 6)]);
}

// ---------------------------------------------------------------------------
// Pinned: monotonic, non-overlapping, limit and validation behavior
// ---------------------------------------------------------------------------

#[test]
fn overlapping_candidates_collapse_to_leftmost_nonoverlapping() {
    assert_eq!(find_cs("aaa", "aa"), vec![(0, 2)]);
    assert_eq!(find_cs("aaaa", "aa"), vec![(0, 2), (2, 4)]);
    assert_eq!(find_cs("aabaa", "aa"), vec![(0, 2), (3, 5)]);
    // Multi-byte adjacency (four 4-byte emoji = 16 bytes).
    assert_eq!(find_cs("😀😀😀😀", "😀😀"), vec![(0, 8), (8, 16)]);
    // Case-insensitive scanning is also non-overlapping.
    assert_eq!(find_ci("aAa", "aa"), vec![(0, 2)]);
}

#[test]
fn limit_sets_truncated_only_when_more_matches_exist() {
    let text = "ab ab ab";
    assert_eq!(find_cs(text, "ab").len(), 3);

    let two = find_matches(text, &cs("ab"), 2);
    assert_eq!(spans(&two), vec![(0, 2), (3, 5)]);
    assert!(two.truncated);

    let three = find_matches(text, &cs("ab"), 3);
    assert_eq!(three.matches.len(), 3);
    assert!(!three.truncated);

    let zero = find_matches(text, &cs("ab"), 0);
    assert!(zero.matches.is_empty());
    assert!(zero.truncated);

    let none = find_matches(text, &cs("zz"), 0);
    assert!(none.matches.is_empty());
    assert!(!none.truncated);
}

#[test]
fn empty_query_is_idle_and_rejected() {
    let idle = MatchOptions::default();
    let result = find_matches("abc", &idle, u64::MAX);
    assert!(result.matches.is_empty());
    assert!(!result.truncated);
    assert_eq!(
        find_matches_checked("abc", &idle, u64::MAX),
        Err(MatchError::EmptyQuery)
    );
}

#[test]
fn oversized_query_is_rejected_per_nfr8() {
    let at_limit = "a".repeat(MAX_QUERY_BYTES);
    let options = cs(&at_limit);
    assert_eq!(options.validate(), Ok(()));
    // A text just long enough for one non-overlapping 8 KiB match.
    let long_text = "a".repeat(MAX_QUERY_BYTES + 5);
    assert_eq!(
        find_matches_checked(&long_text, &options, u64::MAX).map(|r| spans(&r)),
        Ok(vec![(0, MAX_QUERY_BYTES)])
    );

    let over = "a".repeat(MAX_QUERY_BYTES + 1);
    let options = cs(&over);
    assert_eq!(
        options.validate(),
        Err(MatchError::QueryTooLarge {
            len: MAX_QUERY_BYTES + 1,
            max: MAX_QUERY_BYTES,
        })
    );
    // The infallible wrapper degrades invalid options to the Idle result.
    let result = find_matches("aaa", &options, u64::MAX);
    assert!(result.matches.is_empty());
    assert!(!result.truncated);
}

#[test]
fn literal_mode_replacement_is_literal() {
    assert_eq!(MatchMode::default(), MatchMode::Literal);
    assert!(MatchMode::Literal.replacement_is_literal());
}

// ---------------------------------------------------------------------------
// Pinned: literal template expansion and checked output length
// ---------------------------------------------------------------------------

#[test]
fn literal_expansion_is_verbatim() {
    let capture = flavite_syntax::Span::new(TextByte(4), TextByte(7));
    let capture = flavite_text_search::matcher::MatchSpan {
        range: capture,
        length: 3,
    };
    // `$0`, `$1`, `${name}` and `$$` are literal characters in literal mode.
    assert_eq!(
        expand_literal("[$0 $$ ${name} $1]", &capture, "source"),
        "[$0 $$ ${name} $1]"
    );
    assert_eq!(expand_literal("", &capture, "source"), "");

    let template = LiteralTemplate::from(String::from("a$$b"));
    assert_eq!(template.as_str(), "a$$b");
    assert_eq!(template.clone().into_inner(), "a$$b");
    assert_eq!(template.to_string(), "a$$b");
    assert_eq!(LiteralTemplate::default().as_str(), "");
}

#[test]
fn expanded_output_len_computes_checked_totals() {
    // 10 source bytes, three 2-byte matches replaced by a 2-byte template.
    assert_eq!(expanded_output_len(10, "xy", 3, 6), Some(10));
    // No matches: output equals the source.
    assert_eq!(expanded_output_len(10, "xyz", 0, 0), Some(10));
    // Whole-string replacement.
    assert_eq!(expanded_output_len(5, "abc", 2, 5), Some(6));
    // Deletion template shrinks the output.
    assert_eq!(expanded_output_len(10, "", 3, 6), Some(4));
    // Matches cannot cover more than the source.
    assert_eq!(expanded_output_len(5, "a", 1, 6), None);
    // Checked overflow: replacement total and grand total.
    assert_eq!(expanded_output_len(u64::MAX, "a", 2, 0), None);
    assert_eq!(expanded_output_len(0, "abc", u64::MAX, 0), None);
}

// ---------------------------------------------------------------------------
// Pinned: match spans are the existing syntax coordinate types
// ---------------------------------------------------------------------------

#[test]
fn spans_convert_through_from_normalized_snapshot() {
    let text = "alpha beta gamma";
    let snap = TextSnapshot::from_normalized(text);
    let result = find_matches(text, &cs("beta"), u64::MAX);
    assert_eq!(result.matches.len(), 1);
    let span = result.matches[0].range;
    assert_eq!(span.start, TextByte(6));
    assert_eq!(span.end, TextByte(10));
    assert_eq!(result.matches[0].length, 4);
    let source = snap.span_to_source(span).expect("span must map");
    assert_eq!(source.start, SourceByte(6));
    assert_eq!(source.end, SourceByte(10));
    assert_eq!(snap.text_to_source(TextByte(6)), Some(SourceByte(6)));
}

#[test]
fn spans_map_through_a_real_crlf_snapshot() {
    // Decoded + normalized snapshot: CRLF folded to LF, source byte offsets
    // shifted by the removed CR bytes. Match spans are TextByte coordinates
    // and map back through the snapshot that lent the text.
    let snap = TextSnapshot::new(b"one\r\ntwo two\r\n").expect("valid UTF-8 note");
    assert_eq!(snap.text(), "one\ntwo two\n");
    let result = find_matches(snap.text(), &cs("two"), u64::MAX);
    assert_eq!(spans(&result), vec![(4, 7), (8, 11)]);
    let first = snap.span_to_source(result.matches[0].range).expect("maps");
    assert_eq!((first.start.0, first.end.0), (5, 8));
    let second = snap.span_to_source(result.matches[1].range).expect("maps");
    assert_eq!((second.start.0, second.end.0), (9, 12));
}
