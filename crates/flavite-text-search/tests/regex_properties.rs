//! Regex-mode properties: Rust regex semantics, monotonic scalar-progress
//! empty matches, capture/template behavior, inline flag precedence and
//! bounded compilation (RFC-0006 D-05/S03).

use flavite_text_search::TemplateGrammar;
use flavite_text_search::{
    MatchLimits, MatchMode, MatchOptions, Matcher, REGEX_MAX_GROUPS, ReplacementTemplate,
    TemplateError,
};
use proptest::prelude::*;

fn regex(pattern: &str) -> Matcher {
    regex_with(pattern, MatchOptions::default())
}

fn regex_with(pattern: &str, options: MatchOptions) -> Matcher {
    Matcher::new(
        pattern,
        MatchOptions {
            mode: MatchMode::Regex,
            ..options
        },
        MatchLimits::default(),
    )
    .expect("compiles")
}

fn spans(matches: &[flavite_text_search::Match]) -> Vec<(usize, usize)> {
    matches
        .iter()
        .map(|m| (m.span.start.0, m.span.end.0))
        .collect()
}

#[test]
fn empty_matches_advance_one_scalar_and_report_eof_once() {
    // `a*` on "b": an empty match at 0, then the real match, then the
    // end-of-document empty match — one per location, ascending.
    let matcher = regex("a*");
    assert_eq!(spans(&matcher.find("b").unwrap()), vec![(0, 0), (1, 1)]);
    assert_eq!(spans(&matcher.find("ba").unwrap()), vec![(0, 0), (1, 2)]);
    // Greedy `a*` consumes the whole run in one match.
    assert_eq!(matcher.find("a".repeat(1_000).as_str()).unwrap().len(), 1);
    // The empty regex is legal and makes scalar-progress explicit: one
    // empty match per location, ascending, terminating at EOF.
    let empty = regex("");
    let found = empty.find("abc").unwrap();
    assert_eq!(spans(&found), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
}

#[test]
fn capture_groups_map_to_syntax_spans() {
    let matcher = regex(r"(a)(b)?c");
    let found = matcher.find("ac abc").unwrap();
    assert_eq!(spans(&found), vec![(0, 2), (3, 6)]);
    // Group 2 participates only in the second match.
    assert_eq!(found[0].group_text("ac abc", 2), None);
    assert_eq!(found[1].group_text("ac abc", 2), Some("b"));
    assert_eq!(found[1].group_text("ac abc", 0), Some("abc"));
}

#[test]
fn named_groups_resolve_by_name() {
    let matcher = regex(r"(?P<user>\w+)@(?P<host>\w+)");
    let found = matcher.find("bob@example").unwrap();
    assert_eq!(found[0].group_text("bob@example", 0), Some("bob@example"));
    assert_eq!(matcher.capture_name(1), Some("user"));
    assert_eq!(matcher.capture_name(2), Some("host"));

    let template = ReplacementTemplate::new("${user} at ${host}", TemplateGrammar::Dollar).unwrap();
    let expansion = template.expand(&matcher, "bob@example", &found[0]).unwrap();
    assert_eq!(expansion, "bob at example");
    // Expansion is evaluated per original match; inserted text is never
    // rescanned.
    let both = matcher.find("bob@example al@example").unwrap();
    let pairs = template
        .expand_pairs(&matcher, "bob@example al@example", &both)
        .unwrap();
    assert_eq!(pairs[0], (both[0].span, "bob at example".to_owned()));
    assert_eq!(pairs[1], (both[1].span, "al at example".to_owned()));
}

#[test]
fn inline_flags_override_the_case_option_both_ways() {
    // Builder case-insensitive off, inline flag on.
    let matcher = regex("(?i)abc");
    assert_eq!(spans(&matcher.find("ABC abc").unwrap()).len(), 2);
    // Builder case-insensitive on, inline flag off.
    let options = MatchOptions {
        case_sensitive: true,
        ..MatchOptions::default()
    };
    let matcher = regex_with("(?-i)abc", options);
    assert_eq!(spans(&matcher.find("ABC abc").unwrap()), vec![(4, 7)]);
}

#[test]
fn multiline_anchors_on_and_dot_newline_off() {
    let matcher = regex("^b");
    assert_eq!(spans(&matcher.find("a\nb").unwrap()), vec![(2, 3)]);
    let matcher = regex("a.b");
    assert_eq!(
        matcher.find("a\nb").unwrap(),
        vec![],
        "dot never matches \\n"
    );
}

#[test]
fn literal_and_regex_modes_never_confuse_metacharacters() {
    let literal_dot = Matcher::new("a.c", MatchOptions::default(), MatchLimits::default()).unwrap();
    assert_eq!(spans(&literal_dot.find("a.c abc").unwrap()), vec![(0, 3)]);
    let regex_dot = regex("a.c");
    assert_eq!(
        spans(&regex_dot.find("a.c abc").unwrap()),
        vec![(0, 3), (4, 7)]
    );
}

#[test]
fn compilation_is_bounded() {
    // Pattern byte bound.
    let long = "a".repeat(8 * 1024 + 1);
    assert!(
        Matcher::new(
            &long,
            MatchOptions {
                mode: MatchMode::Regex,
                ..MatchOptions::default()
            },
            MatchLimits::default(),
        )
        .is_err()
    );
    // Invalid pattern is a typed regex error.
    let invalid = Matcher::new(
        "(unclosed",
        MatchOptions {
            mode: MatchMode::Regex,
            ..MatchOptions::default()
        },
        MatchLimits::default(),
    );
    assert!(invalid.is_err());
}

#[test]
fn group_count_is_bounded() {
    let too_many = "(a)".repeat(REGEX_MAX_GROUPS + 1);
    let err = Matcher::new(
        &too_many,
        MatchOptions {
            mode: MatchMode::Regex,
            ..MatchOptions::default()
        },
        MatchLimits::default(),
    )
    .expect_err("too many groups");
    assert!(matches!(
        err,
        flavite_text_search::MatchError::RegexTooManyGroups { .. }
    ));
}

#[test]
fn whole_word_postfilter_applies_to_regex_matches() {
    let options = MatchOptions {
        whole_word: true,
        ..MatchOptions::default()
    };
    let matcher = regex_with("cat", options);
    let found = matcher.find("concatenate cat catalog").unwrap();
    assert_eq!(spans(&found), vec![(12, 15)]);
}

#[test]
fn unknown_group_and_expansion_length_are_validation_errors() {
    let matcher = regex("(a)");
    let found = matcher.find("a").unwrap();
    let template = ReplacementTemplate::new("$2", TemplateGrammar::Dollar).unwrap();
    assert!(matches!(
        template.expand(&matcher, "a", &found[0]),
        Err(TemplateError::UnknownGroup { .. })
    ));
    // The unknown group is caught before any edit is computed.
    assert!(matches!(
        template.expansion_len(&matcher, &found[0]),
        Err(TemplateError::UnknownGroup { .. })
    ));
}

proptest::proptest! {
    #[test]
    fn regex_matches_never_panic_and_stay_ascending(
        text in proptest::collection::vec(proptest::char::any(), 0..=80)
            .prop_map(|chars| chars.into_iter().collect::<String>()),
    ) {
        let matcher = regex("a*");
        let found = matcher.find(&text).unwrap();
        for pair in found.windows(2) {
            prop_assert!(pair[0].span.start.0 <= pair[1].span.start.0);
        }
        // Matches partition without overlapping.
        let mut last_end = 0usize;
        for m in &found {
            prop_assert!(m.span.start.0 >= last_end || m.span.end.0 == m.span.start.0);
            if m.span.end.0 > m.span.start.0 {
                last_end = m.span.end.0;
            }
        }
    }
}
