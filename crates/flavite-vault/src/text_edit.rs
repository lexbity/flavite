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
    /// Replacement text: `$` is verbatim in literal mode; the dollar
    /// capture grammar applies in regex mode (D-05).
    pub replacement: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Literal (in-process, always available) or regex (isolated helper,
    /// D-14 — never a silent in-process fallback).
    pub mode: MatchMode,
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
    /// The frozen scope or a helper-reported span is out of bounds or
    /// not char-boundary aligned.
    InvalidScope,
    /// Regex work requires the isolated helper (D-14) and it is missing,
    /// incompatible, late or over-queued. Literal matching stays
    /// available; there is no in-process regex fallback.
    RegexUnavailable {
        reason: String,
    },
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
    match request.mode {
        // D-14: regex runs in the isolated helper process; literal stays
        // in-process and never depends on helper availability.
        MatchMode::Regex => prepare_regex(text, request),
        MatchMode::Literal => prepare_literal(text, request),
    }
}

/// Regex preparation through the isolated helper. The helper returns
/// spans + expansions; the parent re-derives the output length from those
/// spans and cross-checks the helper's terminal proof, so a buggy or
/// hostile helper cannot hand back an unsound candidate.
fn prepare_regex(
    text: &str,
    request: &ReplaceRequest,
) -> Result<(Vec<EditOp>, u64, u64), PrepareError> {
    // Local template validation first: typed and exact, before any
    // helper work (the helper re-validates its own copy, §6.2).
    let template = ReplacementTemplate::new(&request.replacement, TemplateGrammar::Dollar)
        .map_err(|error| match error {
            flavite_text_search::template::TemplateError::TooLong { .. } => {
                PrepareError::OutputLimitExceeded
            }
            other => PrepareError::Template(other),
        })?;
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    let run = crate::text_worker::run_match(
        text,
        &request.query,
        MatchMode::Regex,
        request.case_sensitive,
        request.whole_word,
        MAX_MATCHES_PER_JOB,
        &request.replacement,
        &cancelled,
    )
    .map_err(|error| match error {
        crate::text_worker::TextWorkerError::Failure(
            flavite_text_search::protocol::FailureCode::Pattern,
        ) => PrepareError::InvalidQuery("pattern refused by the matcher"),
        crate::text_worker::TextWorkerError::Failure(
            flavite_text_search::protocol::FailureCode::Limit,
        ) => PrepareError::MatchLimitExceeded,
        crate::text_worker::TextWorkerError::Deadline => PrepareError::MatchLimitExceeded,
        other => PrepareError::RegexUnavailable {
            reason: match other {
                crate::text_worker::TextWorkerError::Unavailable { reason } => reason.to_owned(),
                crate::text_worker::TextWorkerError::Busy => "busy".to_owned(),
                crate::text_worker::TextWorkerError::Cancelled => "cancelled".to_owned(),
                crate::text_worker::TextWorkerError::Deadline => "deadline".to_owned(),
                crate::text_worker::TextWorkerError::HelperDied => "helper-died".to_owned(),
                crate::text_worker::TextWorkerError::Failure(code) => code.code().to_owned(),
                crate::text_worker::TextWorkerError::Frame(_) => "frame".to_owned(),
            },
        },
    })?;
    // The validated template is the authority for the grammar semantics;
    // its digest (plus options/scope) forms the plan identity.
    let mut ops = Vec::with_capacity(run.matches.len());
    let mut output_len = text.len() as u64;
    let mut match_count = 0u64;
    for hit in &run.matches {
        let start = usize::try_from(hit.start).map_err(|_| PrepareError::InvalidScope)?;
        let end = usize::try_from(hit.end).map_err(|_| PrepareError::InvalidScope)?;
        if end < start || end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end)
        {
            return Err(PrepareError::InvalidScope);
        }
        let expansion = String::from_utf8(hit.expansion.clone())
            .map_err(|_| PrepareError::InvalidQuery("expansion was not UTF-8"))?;
        output_len = output_len
            .checked_sub((end - start) as u64)
            .and_then(|rest| rest.checked_add(expansion.len() as u64))
            .ok_or(PrepareError::OutputLimitExceeded)?;
        ops.push(EditOp::new(
            Span::new(TextByte(start), TextByte(end)),
            expansion,
        ));
        match_count += 1;
    }
    // Cross-check the helper's terminal proof against the parent's own
    // arithmetic over the returned spans (§6.2 soundness).
    if output_len > MAX_OUTPUT_BYTES as u64
        || run.output_len != output_len
        || run.total_matches != match_count
    {
        return Err(PrepareError::RegexUnavailable {
            reason: "unsound-helper-proof".to_owned(),
        });
    }
    let digest = template.digest() ^ scope_digest(request);
    Ok((ops, match_count, digest))
}

/// Scope/options contribution to the plan digest (D-05: scope and options
/// are part of the plan identity) — composed with the canonical
/// matcher/template digests, never a second semantics system.
fn scope_digest(request: &ReplaceRequest) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in format!(
        "{}{}:{}",
        request.case_sensitive, request.whole_word,
        request.scope.map_or_else(String::new, |(start, end)| format!("{start}..{end}"))
    )
    .as_bytes()
    {
        hash = hash.rotate_left(8) ^ u64::from(*byte);
    }
    hash
}

/// Literal preparation in-process (unchanged from S01).
fn prepare_literal(
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
            mode: MatchMode::Literal,
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
