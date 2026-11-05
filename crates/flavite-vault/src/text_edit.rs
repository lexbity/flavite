//! Prepared literal note replacement (RFC-0006 S01; D-05/D-06).
//!
//! The matching/validation/output-bound work runs on bounded prepare
//! workers off the host turn; the Session owns the core-issued plan
//! registry and installs candidates only when the buffer, incarnation and
//! revision still match. One prepared replacement is one revision-guarded
//! batch and one isolated Undo unit; invalid preparation has zero note
//! effects. Matching semantics are the canonical `flavite-text-search`
//! matcher (Unicode simple folding, UAX #29 whole-word, frozen scope);
//! regex capability is absent until S03 — this module is literal only and
//! says so.

use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::undo::EditOp;
use flavite_syntax::text::{Span, TextByte};
use flavite_text_search::matcher::{MatchLimits, MatchMode, MatchOptions, Matcher};
use flavite_text_search::template::{ReplacementTemplate, TemplateGrammar};

/// NFR-8: at most one million matches per job.
pub const MAX_MATCHES_PER_JOB: usize = flavite_text_search::matcher::DEFAULT_MAX_MATCHES;
/// NFR-8/NFR-19: per-note normalized replacement output bound.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
/// D-05: an empty query is Idle, not "match everywhere".
pub const IDLE_QUERY: &str = "query is empty: find is idle";

/// One prepared-replacement request, validated at admission.
#[derive(Debug, Clone)]
pub struct ReplaceRequest {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub query: String,
    /// Literal replacement text: `$` is verbatim (D-05); regex templates
    /// arrive with S03's typed grammar.
    pub replacement: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Frozen selection scope at the current revision (`TextByte` range,
    /// half-open); `None` means the whole note.
    pub scope: Option<(usize, usize)>,
}

/// Typed preparation failure (never a partial or lossy outcome).
#[derive(Debug, Clone)]
pub enum PrepareError {
    /// The query is empty (Idle) or exceeds the pattern bound (NFR-8).
    InvalidQuery(&'static str),
    /// The matcher refused the options/pattern.
    Matcher(flavite_text_search::matcher::MatchError),
    /// The replacement template was refused before any edit exists.
    Template(flavite_text_search::template::TemplateError),
    /// More matches than the job bound; never silently truncated.
    MatchLimitExceeded,
    /// The serialized output would exceed the per-note bound (NFR-8/19).
    OutputLimitExceeded,
}

/// The pure prepare step, executed on a prepare worker against the exact
/// immutable snapshot text. Returns ascending, non-overlapping edit
/// operations over the ORIGINAL text plus the match count and the plan
/// digest; the combined output bound is checked before any edit exists.
///
/// # Errors
///
/// [`PrepareError`] with zero effects; callers never see partial matches.
pub fn prepare_ops(
    text: &str,
    request: &ReplaceRequest,
) -> Result<(Vec<EditOp>, u64, u64), PrepareError> {
    let options = MatchOptions {
        mode: MatchMode::Literal,
        case_sensitive: request.case_sensitive,
        whole_word: request.whole_word,
    };
    let matcher = Matcher::new(
        &request.query,
        options,
        MatchLimits {
            max_matches: MAX_MATCHES_PER_JOB,
        },
    )
    .map_err(|error| match error {
        flavite_text_search::matcher::MatchError::EmptyQuery => {
            PrepareError::InvalidQuery(IDLE_QUERY)
        }
        flavite_text_search::matcher::MatchError::PatternTooLong { .. } => {
            PrepareError::InvalidQuery("query exceeds the 8 KiB pattern bound")
        }
        other => PrepareError::Matcher(other),
    })?;
    let template = ReplacementTemplate::new(&request.replacement, TemplateGrammar::Literal)
        .map_err(|error| match error {
            // NFR-8: the replacement itself exceeds the 64 KiB bound —
            // refused before any edit exists.
            flavite_text_search::template::TemplateError::TooLong { .. } => {
                PrepareError::OutputLimitExceeded
            }
            other => PrepareError::Template(other),
        })?;
    let scope = request
        .scope
        .map(|(start, end)| Span::new(TextByte(start), TextByte(end)));
    let hits = matcher.find_in(text, scope).map_err(|error| match error {
        flavite_text_search::matcher::MatchError::LimitExceeded { .. } => {
            PrepareError::MatchLimitExceeded
        }
        other => PrepareError::Matcher(other),
    })?;
    // Counting may be pending in the UI, but a prepared batch must be
    // exact: exceeding the bound refuses instead of silently stopping.
    if hits.len() >= MAX_MATCHES_PER_JOB {
        return Err(PrepareError::MatchLimitExceeded);
    }
    // Exact output bound before any edit exists (D-06/§6.2).
    template
        .total_output_len(text.len(), &matcher, &hits, MAX_OUTPUT_BYTES)
        .map_err(|error| match error {
            flavite_text_search::template::TemplateError::LengthOverflow => {
                PrepareError::OutputLimitExceeded
            }
            flavite_text_search::template::TemplateError::OutputTooLarge { .. } => {
                PrepareError::OutputLimitExceeded
            }
            other => PrepareError::Template(other),
        })?;
    // Literal replacement treats `$` verbatim: every match expands to the
    // raw template text (D-05). Regex capture expansion is S03's grammar.
    let replacement = template.raw().to_owned();
    let ops = hits
        .iter()
        .map(|hit| EditOp::new(hit.span, replacement.clone()))
        .collect();
    let count = hits.len() as u64;
    Ok((ops, count, plan_digest(request, &options, &template)))
}

/// Plan digest (D-05: scope and options are part of the plan identity).
/// Composed from the canonical matcher/template digests plus the frozen
/// scope — never a second matching-semantics system.
fn plan_digest(
    request: &ReplaceRequest,
    options: &MatchOptions,
    template: &ReplacementTemplate,
) -> u64 {
    let mut hash = options.digest() ^ template.digest();
    if let Some((start, end)) = request.scope {
        for byte in format!("{start}..{end}").as_bytes() {
            hash = hash.rotate_left(8) ^ u64::from(*byte);
        }
    }
    hash ^ u64::from(request.scope.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(query: &str, replacement: &str) -> ReplaceRequest {
        ReplaceRequest {
            buffer_id: BufferId::from(1),
            revision: Revision::new(1),
            query: query.to_owned(),
            replacement: replacement.to_owned(),
            case_sensitive: false,
            whole_word: false,
            scope: None,
        }
    }

    #[test]
    fn prepare_finds_ascending_non_overlapping_matches() {
        let text = "aa bb aa";
        let (ops, count, _digest) = prepare_ops(text, &request("aa", "X")).expect("prepare");
        assert_eq!(count, 2);
        assert_eq!(ops[0].range.start.0, 0);
        assert_eq!(ops[1].range.start.0, 6);
        assert!(ops[0].range.end.0 <= ops[1].range.start.0);
    }

    #[test]
    fn empty_query_is_idle_and_limits_are_typed() {
        match prepare_ops("text", &request("", "X")) {
            Err(PrepareError::InvalidQuery(message)) => assert_eq!(message, IDLE_QUERY),
            other => panic!("expected idle query, got {other:?}"),
        }
        let long = "a".repeat(flavite_text_search::matcher::PATTERN_MAX_BYTES + 1);
        assert!(matches!(
            prepare_ops("text", &request(&long, "X")),
            Err(PrepareError::InvalidQuery(_))
        ));
    }

    #[test]
    fn case_insensitive_uses_simple_folding() {
        // D-05: Unicode SIMPLE folding — ß never equals ss; accents fold
        // through the canonical matcher engine.
        let (ops, count, _) =
            prepare_ops("Straße STRASSE École", &request("straße", "X")).expect("prepare");
        assert_eq!(count, 1);
        assert_eq!(ops[0].range.start.0, 0);
        let (_, count, _) = prepare_ops("école ÉCOLE", &request("école", "X")).expect("prepare");
        assert_eq!(count, 2);
    }

    #[test]
    fn scope_is_frozen_and_output_bound_checked() {
        let (_ops, count, _) = prepare_ops("aa|aa", &request("aa", "X")).expect("full");
        assert_eq!(count, 2);
        let (scoped, count, _) = prepare_ops(
            "aa|aa",
            &ReplaceRequest {
                scope: Some((3, 5)),
                ..request("aa", "X")
            },
        )
        .expect("scoped");
        assert_eq!(count, 1);
        assert_eq!(scoped[0].range.start.0, 3);

        let huge = "a".repeat(64 * 1024 * 1024);
        let mut req = request("match", "X");
        req.replacement = huge;
        assert!(matches!(
            prepare_ops("match", &req),
            Err(PrepareError::OutputLimitExceeded)
        ));
    }
}
