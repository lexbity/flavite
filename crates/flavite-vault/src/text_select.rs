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

/// One match-collection request: the same options as `text.find`, document
/// wide (no scope), against one frozen revision.
#[derive(Debug, Clone)]
pub struct SelectMatchesRequest {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub query: String,
    pub mode: MatchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
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
/// isolated helper (D-14) — never as a silent in-process fallback.
///
/// # Errors
///
/// [`crate::text_edit::PrepareError`] with zero effects; callers never see
/// partial collections.
pub fn collect_matches(
    text: &str,
    request: &SelectMatchesRequest,
) -> Result<MatchCollectionOutcome, crate::text_edit::PrepareError> {
    use crate::text_edit::PrepareError;
    match request.mode {
        MatchMode::Regex => {
            let cancelled = std::sync::atomic::AtomicBool::new(false);
            let run = crate::text_worker::run_match(
                &crate::text_worker::HelperRequest {
                    kind: RequestKind::Find,
                    source: text,
                    pattern: &request.query,
                    mode: MatchMode::Regex,
                    case_sensitive: request.case_sensitive,
                    whole_word: request.whole_word,
                    scope: None,
                    max_matches: crate::text_edit::MAX_MATCHES_PER_JOB,
                    report_matches: MAX_SELECTION_SPANS,
                    skip_matches: 0,
                    max_output: 0,
                    template_raw: "",
                },
                &cancelled,
            )
            .map_err(crate::text_edit::worker_error)?;
            // The helper proved the exact total over the complete scan; the
            // parent re-validates every shipped span against the frozen
            // source (bounds + scalar alignment + ascending order).
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
            let expected = run
                .total_matches
                .min(u64::try_from(MAX_SELECTION_SPANS).unwrap_or(u64::MAX));
            if u64::try_from(spans.len()).unwrap_or(u64::MAX) != expected {
                return Err(PrepareError::RegexUnavailable {
                    reason: "unsound-helper-proof".to_owned(),
                });
            }
            Ok(collection(spans, run.total_matches))
        }
        MatchMode::Literal => {
            let options = MatchOptions {
                mode: MatchMode::Literal,
                case_sensitive: request.case_sensitive,
                whole_word: request.whole_word,
            };
            let matcher = Matcher::new(
                &request.query,
                options,
                MatchLimits {
                    max_matches: crate::text_edit::MAX_MATCHES_PER_JOB,
                },
            )
            .map_err(matcher_error)?;
            // Count every match for the exact total (budget exceeded is a
            // typed refusal, never a silent stop); ship at most the
            // selection-bound spans.
            let mut spans = Vec::new();
            let mut total = 0u64;
            for item in matcher.iter_matches(text, None).map_err(matcher_error)? {
                let hit = item.map_err(matcher_error)?;
                total = total.saturating_add(1);
                if spans.len() < MAX_SELECTION_SPANS {
                    spans.push((hit.span.start.0, hit.span.end.0));
                }
            }
            Ok(collection(spans, total))
        }
    }
}

fn collection(spans: Vec<(usize, usize)>, total_matches: u64) -> MatchCollectionOutcome {
    let zero_width_count = spans.iter().filter(|&&(start, end)| start == end).count();
    MatchCollectionOutcome {
        spans,
        total_matches,
        zero_width_count: u64::try_from(zero_width_count).unwrap_or(u64::MAX),
    }
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
        }
    }

    fn literal(query: &str) -> SelectMatchesRequest {
        request(query, MatchMode::Literal)
    }

    #[test]
    fn literal_collection_reports_spans_total_and_zero_width() {
        let text = "TODO one\nTODO two\nTODO";
        let outcome = collect_matches(text, &literal("TODO")).expect("collection");
        assert_eq!(outcome.spans, vec![(0, 4), (9, 13), (18, 22)]);
        assert_eq!(outcome.total_matches, 3);
        assert_eq!(outcome.zero_width_count, 0);
    }

    #[test]
    fn literal_collection_counts_beyond_the_selection_cap_without_materializing() {
        let text = "a".repeat(MAX_SELECTIONS + 42);
        let outcome = collect_matches(&text, &literal("a")).expect("collection");
        assert_eq!(outcome.total_matches, (MAX_SELECTIONS + 42) as u64);
        assert_eq!(outcome.spans.len(), MAX_SELECTION_SPANS);
    }

    #[test]
    fn literal_zero_width_candidates_are_counted() {
        // Literal matching cannot produce zero-width spans (a literal is
        // nonempty by the EmptyQuery guard), so the regex path owns zero
        // width; the literal path still reports zero.
        let outcome = collect_matches("abc", &literal("a")).expect("collection");
        assert_eq!(outcome.zero_width_count, 0);
    }

    #[test]
    fn empty_literal_query_is_idle() {
        let error = collect_matches("abc", &literal("")).expect_err("idle");
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
        let outcome = collect_matches(text, &literal("é")).expect("collection");
        assert_eq!(outcome.spans, vec![(0, 2), (2, 4), (5, 7), (7, 9)]);
        assert_eq!(outcome.total_matches, 4);
    }

    #[test]
    fn literal_spans_are_scalar_aligned() {
        // A literal match never lands mid-scalar; the spans are exactly the
        // matched bytes (the grapheme expansion happens at set build time).
        let text = "é";
        let outcome = collect_matches(text, &literal("é")).expect("collection");
        assert_eq!(outcome.spans, vec![(0, 2)]);
    }
}
