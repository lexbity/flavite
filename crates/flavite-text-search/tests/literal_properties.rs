//! Literal matching properties: independent-scan oracle comparison for the
//! case-sensitive path, pinned simple-folding and whole-word examples,
//! scope containment, limits and paging (RFC-0006 S01/D-05).

use flavite_syntax::{Span, TextByte};
use flavite_text_search::{
    MatchError, MatchLimits, MatchMode, MatchOptions, MatchPage, Matcher, PATTERN_MAX_BYTES,
};
use proptest::prelude::*;

fn build(pattern: &str, options: MatchOptions) -> Matcher {
    Matcher::new(pattern, options, MatchLimits::default()).expect("compiles")
}

/// Independent straightforward scan: non-overlapping ascending occurrences.
fn naive_find(text: &str, pattern: &str) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut start = 0usize;
    while let Some(idx) = text[start..].find(pattern) {
        let absolute = start + idx;
        found.push((absolute, absolute + pattern.len()));
        start = absolute + pattern.len();
    }
    found
}

fn spans(matches: &[flavite_text_search::Match]) -> Vec<(usize, usize)> {
    matches
        .iter()
        .map(|m| (m.span.start.0, m.span.end.0))
        .collect()
}

proptest::proptest! {
    #[test]
    fn case_sensitive_literal_matches_the_independent_scan(
        text in proptest::collection::vec(proptest::char::any(), 0..=80)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
        pattern in proptest::collection::vec(proptest::char::any(), 1..=6)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
    ) {
        let options = MatchOptions { case_sensitive: true, ..MatchOptions::default() };
        let matcher = build(&pattern, options);
        let found = matcher.find(&text).unwrap();
        prop_assert_eq!(spans(&found), naive_find(&text, &pattern));
        // Every match re-extracts the pattern verbatim.
        for m in &found {
            prop_assert_eq!(m.group_text(&text, 0).unwrap(), pattern.as_str());
        }
    }
}

#[test]
fn default_options_are_case_insensitive_literal_without_word_filter() {
    let matcher = build("Case", MatchOptions::default());
    assert_eq!(matcher.options().mode, MatchMode::Literal);
    assert_eq!(spans(&matcher.find("case CASE Case").unwrap()).len(), 3);
}

#[test]
fn simple_folding_examples_are_pinned() {
    // Rust regex Unicode simple folding: ß never equals ss, but capital
    // sharp s folds to ß.
    let matcher = build("ß", MatchOptions::default());
    // ẞ (U+1E9E) is three UTF-8 bytes.
    assert_eq!(
        spans(&matcher.find("ß ẞ ss SS").unwrap()),
        vec![(0, 2), (3, 6)]
    );
    // Composed and decomposed accents remain distinct; no normalization.
    let matcher = build("é", MatchOptions::default());
    let found = matcher.find("é e\u{0301} E\u{0301} É").unwrap();
    assert_eq!(spans(&found), vec![(0, 2), (11, 13)]);
    // Long s and Kelvin sign fold to s and k.
    let matcher = build("s", MatchOptions::default());
    assert_eq!(spans(&matcher.find("s S ſ").unwrap()).len(), 3);
    let matcher = build("k", MatchOptions::default());
    assert_eq!(spans(&matcher.find("k K \u{212A}").unwrap()).len(), 3);
}

#[test]
fn whole_word_requires_uax29_boundaries_on_both_endpoints() {
    let options = MatchOptions {
        whole_word: true,
        ..MatchOptions::default()
    };
    let matcher = build("can", options);
    // Inside "can't" the end at 3 is not a word-segment boundary.
    let found = matcher.find("can't can canny").unwrap();
    assert_eq!(spans(&found), vec![(6, 9)]);

    let matcher = build("case", options);
    let found = matcher.find("casey. case, Case").unwrap();
    assert_eq!(spans(&found), vec![(7, 11), (13, 17)]);

    // Punctuation-only matches qualify when both endpoints are
    // boundaries; consecutive punctuation chars break between each other
    // (WB999 fallback), so both marks in "!!" qualify.
    let matcher = build("!", options);
    let found = matcher.find("a!b ! !!").unwrap();
    assert_eq!(spans(&found), vec![(1, 2), (4, 5), (6, 7), (7, 8)]);
}

#[test]
fn scope_is_frozen_to_exact_ranges_and_validated() {
    let matcher = build("word", MatchOptions::default());
    let found = matcher
        .find_in("word word word", Some(Span::new(TextByte(5), TextByte(14))))
        .unwrap();
    assert_eq!(spans(&found), vec![(5, 9), (10, 14)]);
    // A match straddling the scope start is excluded, never clipped.
    let found = matcher
        .find_in("word word", Some(Span::new(TextByte(2), TextByte(9))))
        .unwrap();
    assert_eq!(spans(&found), vec![(5, 9)]);
}

#[test]
fn empty_query_and_oversize_patterns_are_typed_errors() {
    let err = Matcher::new("", MatchOptions::default(), MatchLimits::default())
        .expect_err("empty literal query");
    assert!(matches!(err, MatchError::EmptyQuery));
    let long = "x".repeat(PATTERN_MAX_BYTES + 1);
    let err = Matcher::new(&long, MatchOptions::default(), MatchLimits::default())
        .expect_err("oversize pattern");
    assert!(matches!(err, MatchError::PatternTooLong { .. }));
}

#[test]
fn oversized_result_sets_are_refused_not_truncated() {
    let bounded =
        Matcher::new("a", MatchOptions::default(), MatchLimits { max_matches: 3 }).unwrap();
    let found = bounded.find("aaaaa").unwrap_err();
    assert!(matches!(
        found,
        MatchError::LimitExceeded {
            observed: 4,
            limit: 3,
        }
    ));
}

#[test]
fn replacement_length_is_computed_before_any_edit() {
    use flavite_text_search::{ReplacementTemplate, TemplateError};
    let matcher = build("a", MatchOptions::default());
    let found = matcher.find("aaa").unwrap();
    let template = ReplacementTemplate::literal("XX").unwrap();
    // 3 bytes − 3 matched + 3×2 expansion = 6
    assert_eq!(
        template.total_output_len(3, &matcher, &found, 6).unwrap(),
        6
    );
    assert!(matches!(
        template.total_output_len(3, &matcher, &found, 5),
        Err(TemplateError::OutputTooLarge { .. })
    ));
}

#[test]
fn pages_replay_idempotently() {
    let matcher = build("ab", MatchOptions::default());
    let found = matcher.find("ab ab ab ab").unwrap();
    assert_eq!(found.len(), 4);
    // Page 50 default: one page covers everything; replay is idempotent.
    let page = MatchPage::new(0, MatchPage::DEFAULT_LEN);
    assert_eq!(page.slice(&found).len(), 4);
    assert_eq!(page.slice(&found), page.slice(&found));
    assert!(!page.has_more(found.len()));
}
