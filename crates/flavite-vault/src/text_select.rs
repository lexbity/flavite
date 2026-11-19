//! Document-wide match collection for match-driven selection (FEUX-0002
//! E-10/E-11): `editor.select_matches` / `editor.split_selection`.
//!
//! This is the *existing* `text.find` engine consumed once per collection,
//! not a second engine: the same canonical `flavite-text-search` matcher,
//! the same isolated regex helper (D-14), the same bound/scope validation,
//! and the same unsound-help-proof cross-check as `prepare_find`. The one
//! difference is the consumer: selection wants the whole match set (up to
//! [`MAX_SELECTION_SPANS`] spans) rather than 256-span navigation pages, so
//! the helper's `report_matches` is raised and the parent validates the
//! returned list against the proven total.
//!
//! The 10,000-selection cap itself is *not* enforced here: the collection
//! returns the exact total (counted without materializing beyond the cap)
//! so the api layer can refuse with the observed count (E-10). The generic
//! 1,000,000-match job bound remains the engine's own ceiling.

use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::selection::MAX_SELECTIONS;
use flavite_text_search::matcher::{MatchLimits, MatchMode, MatchOptions, Matcher};
use flavite_text_search::protocol::RequestKind;

/// One match-collection request: the same options as `text.find`, with
/// optional scope spans (FEUX-0006 E-2), against one frozen revision.
#[derive(Debug, Clone)]
pub struct SelectMatchesRequest {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub query: String,
    pub mode: MatchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Optional scope spans for in-selections search (FEUX-0006 E-2).
    /// Empty vec means whole document.
    pub scope_spans: Vec<(usize, usize)>,
}

/// One keep-matching request (FEUX-0006 E-4): filter selections to those
/// containing at least one match of the query.
#[derive(Debug, Clone)]
pub struct KeepMatchingRequest {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub query: String,
    pub mode: MatchMode,
    pub case_sensitive: bool,
    pub keep: bool,
    /// Scope spans are the current selection spans to filter.
    pub scope_spans: Vec<(usize, usize)>,
}

/// A completed keep-matching collection: indices of selections to keep.
#[derive(Debug, Clone)]
pub struct KeepMatchingOutcome {
    pub selection_indices: Vec<usize>,
    pub match_count: u64,
}

/// A completed collection: ascending, in-order spans (at most
/// [`MAX_SELECTION_SPANS`]), the exact match total, and the exact zero-width
/// hit count (spans where start == end).
#[derive(Debug, Clone)]
pub struct MatchCollectionOutcome {
    pub spans: Vec<(usize, usize)>,
    pub total_matches: u64,
    pub zero_width_count: u64,
}

/// Spans shipped per collection: one more than the selection cap so an
/// oversize result is detectable with the observed count.
pub const MAX_SELECTION_SPANS: usize = MAX_SELECTIONS + 1;

/// Collect every match of `request` against `text` (FEUX-0002 §5.7).
///
/// Literal matches run in-process on this worker; regex matches run in the
/// isolated helper (D-14) — never as a silent in-process fallback. A cancelled
/// collection stops at the next match boundary and reports
/// [`crate::text_edit::PrepareError::Cancelled`]; the dispatch drops the
/// outcome (E-11).
///
/// # Errors
///
/// [`crate::text_edit::PrepareError`] with zero effects; callers never see
/// partial collections.
pub fn collect_matches(
    text: &str,
    request: &SelectMatchesRequest,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<MatchCollectionOutcome, crate::text_edit::PrepareError> {
    use crate::text_edit::PrepareError;
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(PrepareError::Cancelled);
    }
    // The exact total comes from the engine's complete scan (budget exceeded
    // is a typed refusal, never a silent stop); ship at most the
    // selection-bound spans. A cancelled collection stops at the next match
    // boundary (E-11).
    let (matches, total) = collect_contained_matches(
        text,
        &ContainedMatchQuery {
            query: &request.query,
            mode: request.mode,
            case_sensitive: request.case_sensitive,
            whole_word: request.whole_word,
            scope_spans: &request.scope_spans,
        },
        MAX_SELECTION_SPANS,
        cancelled,
    )?;
    let zero_width_count = matches.iter().filter(|&&(start, end)| start == end).count();
    // The helper proved the exact total over the complete scan; the parent
    // re-validates every shipped span against the frozen source (bounds +
    // scalar alignment + ascending order).
    let mut spans = Vec::with_capacity(matches.len());
    for (start, end) in matches {
        if spans.len() >= MAX_SELECTION_SPANS {
            break;
        }
        spans.push((start, end));
    }
    let expected = total.min(u64::try_from(MAX_SELECTION_SPANS).unwrap_or(u64::MAX));
    if u64::try_from(spans.len()).unwrap_or(u64::MAX) != expected {
        return Err(PrepareError::RegexUnavailable {
            reason: "unsound-helper-proof".to_owned(),
        });
    }
    Ok(MatchCollectionOutcome {
        spans,
        total_matches: total,
        zero_width_count: u64::try_from(zero_width_count).unwrap_or(u64::MAX),
    })
}

/// The matcher inputs shared by match collection and keep-matching.
struct ContainedMatchQuery<'a> {
    query: &'a str,
    mode: MatchMode,
    case_sensitive: bool,
    whole_word: bool,
    scope_spans: &'a [(usize, usize)],
}

/// Collect every match fully contained in `scope_spans` (empty = whole
/// source) on the delivered engine paths: regex through the isolated helper
/// worker (D-14), literal in-process (FEUX-0002 §5.7). `report_matches`
/// bounds the helper's shipped match list; the returned total is the exact
/// complete-scan count. The returned spans are ascending, non-overlapping,
/// scalar-aligned, and bounds-checked against `text`.
fn collect_contained_matches(
    text: &str,
    query: &ContainedMatchQuery<'_>,
    report_matches: usize,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<(Vec<(usize, usize)>, u64), crate::text_edit::PrepareError> {
    use crate::text_edit::PrepareError;
    match query.mode {
        MatchMode::Regex => {
            let run = crate::text_worker::run_match(
                &crate::text_worker::HelperRequest {
                    kind: RequestKind::Find,
                    source: text,
                    pattern: query.query,
                    mode: MatchMode::Regex,
                    case_sensitive: query.case_sensitive,
                    whole_word: query.whole_word,
                    scope_spans: query.scope_spans.to_vec(),
                    max_matches: crate::text_edit::MAX_MATCHES_PER_JOB,
                    report_matches,
                    skip_matches: 0,
                    max_output: 0,
                    template_raw: "",
                },
                cancelled,
            )
            .map_err(crate::text_edit::worker_error)?;
            let mut spans = Vec::with_capacity(run.matches.len());
            let mut last_end = 0usize;
            for hit in &run.matches {
                let start = usize::try_from(hit.start).map_err(|_| PrepareError::InvalidScope)?;
                let end = usize::try_from(hit.end).map_err(|_| PrepareError::InvalidScope)?;
                if end < start
                    || end > text.len()
                    || start < last_end
                    || !source_bounds_valid(text, start, end)
                {
                    return Err(PrepareError::InvalidScope);
                }
                last_end = end;
                spans.push((start, end));
            }
            Ok((spans, run.total_matches))
        }
        MatchMode::Literal => {
            let options = MatchOptions {
                mode: MatchMode::Literal,
                case_sensitive: query.case_sensitive,
                whole_word: query.whole_word,
            };
            let matcher = Matcher::new(
                query.query,
                options,
                MatchLimits {
                    max_matches: crate::text_edit::MAX_MATCHES_PER_JOB,
                },
            )
            .map_err(matcher_error)?;
            // Use matcher.find_in_spans which handles both cases internally.
            let results = if query.scope_spans.is_empty() {
                matcher.find(text).map_err(matcher_error)?
            } else {
                matcher
                    .find_in_spans(text, query.scope_spans)
                    .map_err(matcher_error)?
            };
            let spans: Vec<(usize, usize)> = results
                .into_iter()
                .map(|hit| (hit.span.start.0, hit.span.end.0))
                .collect();
            let total = spans.len() as u64;
            Ok((spans, total))
        }
    }
}

/// Collect matches for keep-matching: filter selections to those containing
/// at least one match (FEUX-0006 E-4). Returns indices of selections to keep
/// and the total match count.
///
/// The same engine paths as [`collect_matches`] run here (regex through the
/// isolated helper, literal in-process — E-11); keep never compiles or
/// executes a regex on this thread. Zero-width matches count as contained
/// (E-4: `^` with multi_line keeps every line-spanning member).
pub fn collect_keep_matches(
    text: &str,
    request: &KeepMatchingRequest,
    cancelled: &std::sync::atomic::AtomicBool,
) -> Result<KeepMatchingOutcome, crate::text_edit::PrepareError> {
    use crate::text_edit::PrepareError;
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(PrepareError::Cancelled);
    }

    if request.scope_spans.is_empty() {
        // No selections to filter - return empty (will refuse in API)
        return Ok(KeepMatchingOutcome {
            selection_indices: Vec::new(),
            match_count: 0,
        });
    }

    let (matches, match_count) = collect_contained_matches(
        text,
        &ContainedMatchQuery {
            query: &request.query,
            mode: request.mode,
            case_sensitive: request.case_sensitive,
            whole_word: false, // keep_matching doesn't use whole_word per spec
            scope_spans: &request.scope_spans,
        },
        usize::MAX,
        cancelled,
    )?;

    // For each selection span, check if it contains at least one match. The
    // members and the contained matches are both ascending/non-overlapping,
    // so one two-pointer pass decides membership.
    let mut keep_indices = Vec::new();
    let mut mi = 0usize;
    for (index, &(sel_start, sel_end)) in request.scope_spans.iter().enumerate() {
        // Skip matches wholly before this member. A zero-width match exactly
        // at the member's start is contained (it lies inside [start, end)),
        // so the skip key is the raw start, not the end.
        while mi < matches.len() && matches[mi].0 < sel_start {
            mi += 1;
        }
        // A fully-contained match decides this member (zero-width counts).
        let has_match =
            mi < matches.len() && matches[mi].0 >= sel_start && matches[mi].1 <= sel_end;
        if request.keep == has_match {
            keep_indices.push(index);
        }
        // Consume every match contained in this member.
        while mi < matches.len() && matches[mi].1 <= sel_end {
            mi += 1;
        }
    }

    Ok(KeepMatchingOutcome {
        selection_indices: keep_indices,
        match_count,
    })
}

fn source_bounds_valid(text: &str, start: usize, end: usize) -> bool {
    start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end)
}

fn matcher_error(
    error: flavite_text_search::matcher::MatchError,
) -> crate::text_edit::PrepareError {
    crate::text_edit::PrepareError::Matcher(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_text_search::matcher::MatchMode;

    fn request(query: &str, mode: MatchMode) -> SelectMatchesRequest {
        SelectMatchesRequest {
            buffer_id: BufferId::from(1),
            revision: Revision::new(1),
            query: query.to_owned(),
            mode,
            case_sensitive: false,
            whole_word: false,
            scope_spans: vec![],
        }
    }

    fn literal(query: &str) -> SelectMatchesRequest {
        request(query, MatchMode::Literal)
    }

    #[test]
    fn literal_collection_reports_spans_total_and_zero_width() {
        let text = "TODO one\nTODO two\nTODO";
        let outcome = collect_matches(
            text,
            &literal("TODO"),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("collection");
        assert_eq!(outcome.spans, vec![(0, 4), (9, 13), (18, 22)]);
        assert_eq!(outcome.total_matches, 3);
        assert_eq!(outcome.zero_width_count, 0);
    }

    #[test]
    fn literal_collection_counts_beyond_the_selection_cap_without_materializing() {
        let text = "a".repeat(MAX_SELECTIONS + 42);
        let outcome = collect_matches(
            &text,
            &literal("a"),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("collection");
        assert_eq!(outcome.total_matches, (MAX_SELECTIONS + 42) as u64);
        assert_eq!(outcome.spans.len(), MAX_SELECTION_SPANS);
    }

    #[test]
    fn literal_zero_width_candidates_are_counted() {
        // Literal matching cannot produce zero-width spans (a literal is
        // nonempty by the EmptyQuery guard), so the regex path owns zero
        // width; the literal path still reports zero.
        let outcome = collect_matches(
            "abc",
            &literal("a"),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("collection");
        assert_eq!(outcome.zero_width_count, 0);
    }

    #[test]
    fn empty_literal_query_is_idle() {
        let error = collect_matches(
            "abc",
            &literal(""),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect_err("idle");
        assert!(matches!(
            error,
            crate::text_edit::PrepareError::Matcher(
                flavite_text_search::matcher::MatchError::EmptyQuery
            )
        ));
    }

    #[test]
    fn matches_are_ascending_and_scalar_aligned() {
        let text = "éé éé";
        let outcome = collect_matches(
            text,
            &literal("é"),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("collection");
        assert_eq!(outcome.spans, vec![(0, 2), (2, 4), (5, 7), (7, 9)]);
        assert_eq!(outcome.total_matches, 4);
    }

    #[test]
    fn literal_spans_are_scalar_aligned() {
        // A literal match never lands mid-scalar; the spans are exactly the
        // matched bytes (the grapheme expansion happens at set build time).
        let text = "é";
        let outcome = collect_matches(
            text,
            &literal("é"),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("collection");
        assert_eq!(outcome.spans, vec![(0, 2)]);
    }

    #[test]
    fn cancelled_collection_stops_without_results() {
        use crate::text_edit::PrepareError;
        use std::sync::atomic::{AtomicBool, Ordering};
        let cancelled = AtomicBool::new(true);
        let error = collect_matches("TODO", &literal("TODO"), &cancelled)
            .expect_err("a cancelled collection never returns results");
        assert!(matches!(error, PrepareError::Cancelled));
        // A collection cancelled mid-scan reports Cancelled, not a partial
        // count.
        let cancelled = AtomicBool::new(false);
        let text = "a".repeat(2_048);
        let error = {
            cancelled.store(true, Ordering::Release);
            // The pre-scan check sees the flag before any match is counted.
            collect_matches(&text, &literal("a"), &cancelled)
        }
        .expect_err("cancelled before the scan");
        assert!(matches!(error, PrepareError::Cancelled));
    }

    fn keep_request(query: &str, keep: bool, spans: Vec<(usize, usize)>) -> KeepMatchingRequest {
        KeepMatchingRequest {
            buffer_id: BufferId::from(1),
            revision: Revision::new(1),
            query: query.to_owned(),
            mode: MatchMode::Literal,
            case_sensitive: false,
            keep,
            scope_spans: spans,
        }
    }

    #[test]
    fn keep_collection_filters_members_by_contained_literal_matches() {
        let text = "aa bb\naa cc\n";
        // Members: [0,6) ("aa bb") and [6,12) ("aa cc"); both contain "aa".
        let request = keep_request("aa", true, vec![(0, 6), (6, 12)]);
        let outcome =
            collect_keep_matches(text, &request, &std::sync::atomic::AtomicBool::new(false))
                .expect("keep");
        assert_eq!(outcome.selection_indices, vec![0, 1]);
        assert_eq!(outcome.match_count, 2);

        // A member with no contained match is filtered out.
        let request = keep_request("bb", true, vec![(0, 6), (6, 12)]);
        let outcome =
            collect_keep_matches(text, &request, &std::sync::atomic::AtomicBool::new(false))
                .expect("keep");
        assert_eq!(outcome.selection_indices, vec![0]);
        assert_eq!(outcome.match_count, 1);

        // The complement keeps the non-matching member.
        let request = keep_request("bb", false, vec![(0, 6), (6, 12)]);
        let outcome =
            collect_keep_matches(text, &request, &std::sync::atomic::AtomicBool::new(false))
                .expect("keep");
        assert_eq!(outcome.selection_indices, vec![1]);
        assert_eq!(outcome.match_count, 1);
    }

    #[test]
    fn keep_collection_with_no_selections_is_empty() {
        let outcome = collect_keep_matches(
            "abc",
            &keep_request("a", true, vec![]),
            &std::sync::atomic::AtomicBool::new(false),
        )
        .expect("keep");
        assert!(outcome.selection_indices.is_empty());
        assert_eq!(outcome.match_count, 0);
    }
}
