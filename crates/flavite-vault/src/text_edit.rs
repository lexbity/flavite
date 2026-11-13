//! Prepared note replacement and find (RFC-0006 S01/S03; D-05/D-06).
//!
//! The matching/validation/output-bound work runs on bounded prepare
//! workers off the host turn; the Session owns the core-issued plan
//! registry and installs candidates only when the buffer, incarnation and
//! revision still match. One prepared replacement is one revision-guarded
//! batch and one isolated Undo unit; invalid preparation has zero note
//! effects. Matching semantics are the canonical `flavite-text-search`
//! matcher (Unicode simple folding, UAX #29 whole-word, frozen scope):
//! literal matches run in-process on the worker, regex matches run in the
//! isolated helper (D-14) — never as a silent in-process fallback.

use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::undo::EditOp;
use flavite_syntax::text::{Span, TextByte};
use flavite_text_search::matcher::{MatchLimits, MatchMode, MatchOptions, Matcher};
use flavite_text_search::protocol::RequestKind;
use flavite_text_search::template::{ReplacementTemplate, TemplateGrammar};

/// NFR-8: at most one million matches per job.
pub const MAX_MATCHES_PER_JOB: usize = flavite_text_search::matcher::DEFAULT_MAX_MATCHES;
/// NFR-8/NFR-19: per-note normalized replacement output bound.
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
/// S03: note Find emits bounded continuation pages, never an arbitrary
/// first-N prefix presented as the complete match list.
pub const MAX_FIND_SPANS: usize = 256;
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

/// One note find request (S03): the same options as replacement, over the
/// same canonical matcher, returning revision-bound span pages.
#[derive(Debug, Clone)]
pub struct FindRequest {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub query: String,
    pub mode: MatchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Zero-based match ordinal for continuation. The source revision and
    /// all options are supplied again and revalidated for every page.
    pub offset: u64,
    /// Frozen selection scope at the current revision; `None` means the
    /// whole note.
    pub scope: Option<(usize, usize)>,
}

/// A completed find: ascending, fully-in-scope spans, the exact match
/// total, and the honest page-cap flag (§6.2: "counts may be pending but
/// MUST NOT silently stop at the cap").
#[derive(Debug, Clone)]
pub struct FindOutcome {
    pub spans: Vec<(usize, usize)>,
    pub offset: u64,
    pub total_matches: u64,
    /// Next stable ordinal, if more matches remain.
    pub next_offset: Option<u64>,
    /// Kept for existing UI adapters; equals `next_offset.is_some()`.
    pub truncated: bool,
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
    /// A continuation ordinal is outside the exact match set.
    MatchOffsetOutOfRange,
    /// The serialized output would exceed the per-note bound (NFR-8/19).
    OutputLimitExceeded,
    /// The frozen scope or a helper-reported span is out of bounds or
    /// not char-boundary aligned.
    InvalidScope,
    /// Regex work requires the isolated helper (D-14) and it is missing,
    /// incompatible, late or over-queued. Literal matching stays
    /// available; there is no in-process regex fallback.
    RegexUnavailable { reason: String },
    /// A helper operation exceeded the 5 s wall deadline and was killed
    /// (§6.2/D-14): a typed refusal naming the deadline, never mislabelled
    /// as a match-count limit.
    RegexTimeout,
    /// The helper refused the input it was given for a reason that maps
    /// directly to a protocol code; retained verbatim for typed routing.
    Worker(crate::text_worker::TextWorkerError),
    /// The work was cancelled before it completed (FEUX-0002 E-11): the
    /// result is dropped at dispatch and is never observable by a caller.
    Cancelled,
}

impl PrepareError {
    /// Stable user-facing message: typed and redacted — never pattern,
    /// replacement or source text (§6.7, NFR-17). Regex parse details stay
    /// in the typed enum for tests; only these summaries cross the facade.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Self::InvalidQuery(message) => (*message).to_owned(),
            Self::Matcher(flavite_text_search::matcher::MatchError::EmptyQuery) => {
                IDLE_QUERY.to_owned()
            }
            Self::Matcher(flavite_text_search::matcher::MatchError::PatternTooLong { .. }) => {
                "pattern exceeds the 8 KiB bound".to_owned()
            }
            Self::Matcher(flavite_text_search::matcher::MatchError::Regex(_)) => {
                "invalid regular expression".to_owned()
            }
            Self::Matcher(flavite_text_search::matcher::MatchError::RegexTooManyGroups {
                limit,
                ..
            }) => format!("pattern exceeds the {limit}-group capture bound"),
            Self::Matcher(flavite_text_search::matcher::MatchError::InvalidScope { offset }) => {
                format!("scope offset {offset} is not a character boundary")
            }
            Self::Matcher(flavite_text_search::matcher::MatchError::LimitExceeded { .. }) => {
                "too many matches for one operation".to_owned()
            }
            Self::Matcher(flavite_text_search::matcher::MatchError::OrdinalOutOfRange {
                ..
            }) => "requested match ordinal is out of range".to_owned(),
            Self::Template(flavite_text_search::template::TemplateError::TooLong { .. }) => {
                "replacement exceeds the 64 KiB bound".to_owned()
            }
            Self::Template(flavite_text_search::template::TemplateError::DanglingDollar {
                offset,
            }) => format!("replacement template has a dangling '$' at offset {offset}"),
            Self::Template(flavite_text_search::template::TemplateError::MalformedBraces {
                offset,
            }) => format!("replacement template has malformed braces at offset {offset}"),
            Self::Template(flavite_text_search::template::TemplateError::UnknownGroup {
                ..
            }) => "replacement references an unknown capture group".to_owned(),
            Self::Template(_) => "invalid replacement template".to_owned(),
            Self::MatchLimitExceeded => "too many matches for one operation".to_owned(),
            Self::MatchOffsetOutOfRange => {
                "find continuation is out of range; search again".to_owned()
            }
            Self::OutputLimitExceeded => "replacement output exceeds its bound".to_owned(),
            Self::InvalidScope => "the frozen selection scope no longer fits the note".to_owned(),
            Self::Cancelled => "the match collection was cancelled".to_owned(),
            Self::RegexUnavailable { reason } => {
                format!("regex is unavailable ({reason}); literal matching still works")
            }
            Self::RegexTimeout => "the pattern exceeded the 5-second helper deadline".to_owned(),
            Self::Worker(crate::text_worker::TextWorkerError::Busy) => {
                "regex helper slots are busy — retry".to_owned()
            }
            Self::Worker(_) => "the regex helper failed; retry or simplify the pattern".to_owned(),
        }
    }
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
    prepare_ops_with_class(
        text,
        request,
        crate::text_worker::HelperPermitClass::Interactive,
    )
}

fn prepare_ops_with_class(
    text: &str,
    request: &ReplaceRequest,
    class: crate::text_worker::HelperPermitClass,
) -> Result<(Vec<EditOp>, u64, u64), PrepareError> {
    match request.mode {
        // D-14: regex runs in the isolated helper process; literal stays
        // in-process and never depends on helper availability.
        MatchMode::Regex => prepare_regex(text, request, class),
        MatchMode::Literal => prepare_literal(text, request),
    }
}

/// The pure find step (S03): the same matcher semantics as replacement,
/// reporting revision-bound spans and the exact total. Literal matches run
/// in-process; regex matches run through the helper with the display page
/// cap and an empty expansion budget (find never mutates).
///
/// # Errors
///
/// [`PrepareError`] with zero effects.
pub fn prepare_find(text: &str, request: &FindRequest) -> Result<FindOutcome, PrepareError> {
    prepare_find_with_class(
        text,
        request,
        crate::text_worker::HelperPermitClass::Interactive,
    )
}

fn prepare_find_with_class(
    text: &str,
    request: &FindRequest,
    class: crate::text_worker::HelperPermitClass,
) -> Result<FindOutcome, PrepareError> {
    if request.offset > u64::try_from(MAX_MATCHES_PER_JOB).unwrap_or(u64::MAX) {
        return Err(PrepareError::MatchOffsetOutOfRange);
    }
    let scope = scope_of(request.scope)?;
    match request.mode {
        MatchMode::Regex => {
            let run = match class {
                crate::text_worker::HelperPermitClass::Interactive => {
                    crate::text_worker::run_match(
                        &crate::text_worker::HelperRequest {
                            kind: RequestKind::Find,
                            source: text,
                            pattern: &request.query,
                            mode: MatchMode::Regex,
                            case_sensitive: request.case_sensitive,
                            whole_word: request.whole_word,
                            scope: request.scope,
                            max_matches: MAX_MATCHES_PER_JOB,
                            report_matches: MAX_FIND_SPANS,
                            skip_matches: request.offset,
                            max_output: 0,
                            template_raw: "",
                        },
                        &std::sync::atomic::AtomicBool::new(false),
                    )
                }
                crate::text_worker::HelperPermitClass::Bulk => crate::text_worker::run_match_bulk(
                    &crate::text_worker::HelperRequest {
                        kind: RequestKind::Find,
                        source: text,
                        pattern: &request.query,
                        mode: MatchMode::Regex,
                        case_sensitive: request.case_sensitive,
                        whole_word: request.whole_word,
                        scope: request.scope,
                        max_matches: MAX_MATCHES_PER_JOB,
                        report_matches: MAX_FIND_SPANS,
                        skip_matches: request.offset,
                        max_output: 0,
                        template_raw: "",
                    },
                    &std::sync::atomic::AtomicBool::new(false),
                ),
            }
            .map_err(worker_error)?;
            // The helper proved the exact total over the complete scan.
            let spans = run
                .matches
                .iter()
                .map(|hit| {
                    let start =
                        usize::try_from(hit.start).map_err(|_| PrepareError::InvalidScope)?;
                    let end = usize::try_from(hit.end).map_err(|_| PrepareError::InvalidScope)?;
                    if end < start || end > text.len() || !source_bounds_valid(text, start, end) {
                        return Err(PrepareError::InvalidScope);
                    }
                    Ok((start, end))
                })
                .collect::<Result<Vec<_>, PrepareError>>()?;
            let remaining = run.total_matches.saturating_sub(request.offset);
            if request.offset > run.total_matches {
                return Err(PrepareError::MatchOffsetOutOfRange);
            }
            let expected = remaining.min(u64::try_from(MAX_FIND_SPANS).unwrap_or(u64::MAX));
            if u64::try_from(spans.len()).unwrap_or(u64::MAX) != expected {
                return Err(PrepareError::RegexUnavailable {
                    reason: "unsound-helper-proof".to_owned(),
                });
            }
            Ok(FindOutcome {
                offset: request.offset,
                next_offset: (request.offset.saturating_add(expected) < run.total_matches)
                    .then_some(request.offset.saturating_add(expected)),
                truncated: request.offset.saturating_add(expected) < run.total_matches,
                spans,
                total_matches: run.total_matches,
            })
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
                    max_matches: MAX_MATCHES_PER_JOB,
                },
            )
            .map_err(matcher_error)?;
            // Count every match for the exact total (budget exceeded is a
            // typed refusal, never a silent stop); ship at most the page
            // cap and mark truncation honestly.
            let mut spans = Vec::new();
            let mut total = 0u64;
            for item in matcher.iter_matches(text, scope).map_err(matcher_error)? {
                let hit = item.map_err(matcher_error)?;
                let ordinal = total;
                total = total.saturating_add(1);
                if ordinal >= request.offset && spans.len() < MAX_FIND_SPANS {
                    spans.push((hit.span.start.0, hit.span.end.0));
                }
            }
            if request.offset > total {
                return Err(PrepareError::MatchOffsetOutOfRange);
            }
            let next_offset = request
                .offset
                .saturating_add(u64::try_from(spans.len()).unwrap_or(u64::MAX));
            Ok(FindOutcome {
                offset: request.offset,
                next_offset: (next_offset < total).then_some(next_offset),
                truncated: next_offset < total,
                spans,
                total_matches: total,
            })
        }
    }
}

/// Regex preparation through the isolated helper. The helper returns
/// spans + expansions; the parent re-derives the output length from those
/// spans and cross-checks the helper's terminal proof, so a buggy or
/// hostile helper cannot hand the session an unsound candidate. The frozen
/// scope rides the request: matching sees the whole text (whole-word
/// boundaries are whole-text, D-05) but only fully-contained matches ship.
fn prepare_regex(
    text: &str,
    request: &ReplaceRequest,
    class: crate::text_worker::HelperPermitClass,
) -> Result<(Vec<EditOp>, u64, u64), PrepareError> {
    let scope = scope_of(request.scope)?;
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
    let run = match class {
        crate::text_worker::HelperPermitClass::Interactive => crate::text_worker::run_match(
            &crate::text_worker::HelperRequest {
                kind: RequestKind::Replace,
                source: text,
                pattern: &request.query,
                mode: MatchMode::Regex,
                case_sensitive: request.case_sensitive,
                whole_word: request.whole_word,
                scope: request.scope,
                max_matches: MAX_MATCHES_PER_JOB,
                report_matches: usize::MAX,
                skip_matches: 0,
                max_output: MAX_OUTPUT_BYTES,
                template_raw: &request.replacement,
            },
            &cancelled,
        ),
        crate::text_worker::HelperPermitClass::Bulk => crate::text_worker::run_match_bulk(
            &crate::text_worker::HelperRequest {
                kind: RequestKind::Replace,
                source: text,
                pattern: &request.query,
                mode: MatchMode::Regex,
                case_sensitive: request.case_sensitive,
                whole_word: request.whole_word,
                scope: request.scope,
                max_matches: MAX_MATCHES_PER_JOB,
                report_matches: usize::MAX,
                skip_matches: 0,
                max_output: MAX_OUTPUT_BYTES,
                template_raw: &request.replacement,
            },
            &cancelled,
        ),
    }
    .map_err(worker_error)?;
    let mut ops = Vec::with_capacity(run.matches.len());
    let mut output_len = text.len() as u64;
    let mut match_count = 0u64;
    for hit in &run.matches {
        let start = usize::try_from(hit.start).map_err(|_| PrepareError::InvalidScope)?;
        let end = usize::try_from(hit.end).map_err(|_| PrepareError::InvalidScope)?;
        if end < start
            || end > text.len()
            || !source_bounds_valid(text, start, end)
            // Scope containment re-check against the parent's own bounds:
            // a helper that shipped a match outside the frozen scope is a
            // protocol violation, never a silently accepted candidate.
            || scope.is_some_and(|scope| start < scope.start.0 || end > scope.end.0)
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
/// are part of the plan identity), bound to the pinned engine semantics so
/// an old plan is never reinterpreted after a Unicode/regex upgrade (§6.2).
fn scope_digest(request: &ReplaceRequest) -> u64 {
    let mut hash = fnv1a64(flavite_text_search::matcher::MATCH_SEMANTICS_VERSION.to_le_bytes());
    for byte in format!(
        "{}{}:{}:{}",
        request.mode == MatchMode::Regex,
        request.case_sensitive,
        request.whole_word,
        request
            .scope
            .map_or_else(String::new, |(start, end)| format!("{start}..{end}"))
    )
    .as_bytes()
    {
        hash = hash.rotate_left(8) ^ u64::from(*byte);
    }
    hash
}

/// One canonical FNV-1a seed over the engine semantics version.
fn fnv1a64(version: [u8; 4]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in version {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01B3);
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
    .map_err(matcher_error)?;
    let template = ReplacementTemplate::new(&request.replacement, TemplateGrammar::Literal)
        .map_err(|error| match error {
            // NFR-8: the replacement itself exceeds the 64 KiB bound —
            // refused before any edit exists.
            flavite_text_search::template::TemplateError::TooLong { .. } => {
                PrepareError::OutputLimitExceeded
            }
            other => PrepareError::Template(other),
        })?;
    let scope = scope_of(request.scope)?;
    let hits = matcher.find_in(text, scope).map_err(matcher_error)?;
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
    // raw template text (D-05).
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
    options.digest() ^ template.digest() ^ scope_digest(request)
}

/// Validate the declared scope endpoints against the source (char
/// boundaries and order), before any helper round trip.
fn scope_of(scope: Option<(usize, usize)>) -> Result<Option<Span<TextByte>>, PrepareError> {
    let Some((start, end)) = scope else {
        return Ok(None);
    };
    if end < start {
        return Err(PrepareError::InvalidScope);
    }
    Ok(Some(Span::new(TextByte(start), TextByte(end))))
}

/// Char-boundary validation for spans received from the helper (the
/// endpoints themselves must be legal UTF-8 boundaries).
fn source_bounds_valid(text: &str, start: usize, end: usize) -> bool {
    text.is_char_boundary(start) && text.is_char_boundary(end)
}

/// Map a canonical match error to the typed prepare error.
fn matcher_error(error: flavite_text_search::matcher::MatchError) -> PrepareError {
    match error {
        flavite_text_search::matcher::MatchError::EmptyQuery => {
            PrepareError::InvalidQuery(IDLE_QUERY)
        }
        flavite_text_search::matcher::MatchError::PatternTooLong { .. } => {
            PrepareError::InvalidQuery("query exceeds the 8 KiB pattern bound")
        }
        flavite_text_search::matcher::MatchError::LimitExceeded { .. } => {
            PrepareError::MatchLimitExceeded
        }
        flavite_text_search::matcher::MatchError::InvalidScope { .. } => PrepareError::InvalidScope,
        other => PrepareError::Matcher(other),
    }
}

/// Map a helper error to the typed prepare error. Deadline is its own
/// outcome (S03/§6.2); transport-shaped failures keep a stable reason code
/// so the UI offers the repair and literal matching stays available.
pub(crate) fn worker_error(error: crate::text_worker::TextWorkerError) -> PrepareError {
    use crate::text_worker::TextWorkerError as Worker;
    match error {
        Worker::Failure(flavite_text_search::protocol::FailureCode::Pattern) => {
            PrepareError::InvalidQuery("invalid regular expression")
        }
        Worker::Failure(flavite_text_search::protocol::FailureCode::Template) => {
            PrepareError::InvalidQuery("invalid replacement template")
        }
        Worker::Failure(flavite_text_search::protocol::FailureCode::Limit) => {
            PrepareError::MatchLimitExceeded
        }
        Worker::Failure(flavite_text_search::protocol::FailureCode::Source) => {
            PrepareError::InvalidScope
        }
        Worker::Deadline => PrepareError::RegexTimeout,
        Worker::Cancelled => PrepareError::RegexUnavailable {
            reason: "cancelled".to_owned(),
        },
        Worker::Busy => PrepareError::RegexUnavailable {
            reason: "busy".to_owned(),
        },
        Worker::HelperDied => PrepareError::RegexUnavailable {
            reason: "helper-died".to_owned(),
        },
        Worker::Frame(_) => PrepareError::RegexUnavailable {
            reason: "frame".to_owned(),
        },
        Worker::Unavailable { reason } => PrepareError::RegexUnavailable {
            reason: reason.to_owned(),
        },
        Worker::Failure(code) => PrepareError::RegexUnavailable {
            reason: code.code().to_owned(),
        },
    }
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

    #[test]
    fn literal_find_pages_report_exact_totals_and_honest_truncation() {
        let outcome = prepare_find(
            "aa|aa",
            &FindRequest {
                buffer_id: BufferId::from(1),
                revision: Revision::new(1),
                query: "aa".into(),
                mode: MatchMode::Literal,
                case_sensitive: false,
                whole_word: false,
                offset: 0,
                scope: None,
            },
        )
        .expect("find");
        assert_eq!(outcome.spans, vec![(0, 2), (3, 5)]);
        assert_eq!(outcome.total_matches, 2);
        assert!(!outcome.truncated);
    }

    #[test]
    fn literal_find_continuation_uses_zero_based_ordinals() {
        let source = "x ".repeat(600);
        let outcome = prepare_find(
            &source,
            &FindRequest {
                buffer_id: BufferId::from(1),
                revision: Revision::new(1),
                query: "x".into(),
                mode: MatchMode::Literal,
                case_sensitive: false,
                whole_word: false,
                offset: 256,
                scope: None,
            },
        )
        .expect("continuation page");
        assert_eq!(outcome.spans.len(), 256);
        assert_eq!(outcome.spans[0], (512, 513));
        assert_eq!(outcome.spans[255], (1022, 1023));
        assert_eq!(outcome.offset, 256);
        assert_eq!(outcome.total_matches, 600);
        assert_eq!(outcome.next_offset, Some(512));
        assert!(outcome.truncated);
    }

    #[test]
    fn literal_find_respects_frozen_scope() {
        let outcome = prepare_find(
            "aa|aa",
            &FindRequest {
                buffer_id: BufferId::from(1),
                revision: Revision::new(1),
                query: "aa".into(),
                mode: MatchMode::Literal,
                case_sensitive: false,
                whole_word: false,
                offset: 0,
                scope: Some((3, 5)),
            },
        )
        .expect("scoped find");
        assert_eq!(outcome.spans, vec![(3, 5)]);
    }

    #[test]
    fn scoped_prepare_digest_binds_options_scope_and_engine() {
        let base = prepare_ops("aa", &request("aa", "X")).expect("full").2;
        let scoped = prepare_ops(
            "aa|aa",
            &ReplaceRequest {
                scope: Some((0, 2)),
                ..request("aa", "X")
            },
        )
        .expect("scoped")
        .2;
        assert_ne!(base, scoped, "scope changes plan identity");
        let cased = prepare_ops(
            "aa",
            &ReplaceRequest {
                case_sensitive: true,
                ..request("aa", "X")
            },
        )
        .expect("cased")
        .2;
        let wword = prepare_ops(
            "aa",
            &ReplaceRequest {
                whole_word: true,
                ..request("aa", "X")
            },
        )
        .expect("whole-word")
        .2;
        assert_ne!(cased, wword, "options change plan identity");
    }

    #[test]
    fn regex_without_helper_is_refused_not_fallen_back() {
        // A regex prepare must never produce an in-process result when the
        // isolated helper is unusable: the outcome is a typed
        // RegexUnavailable reason (missing sibling, stale protocol, spawn
        // or engine refusal), and the message says literal stays available.
        // The exact "helper-not-installed" and real round-trip cases are
        // owned deterministically by tests/text_worker.rs, which controls
        // the sibling file.
        let error = prepare_ops(
            "abc",
            &ReplaceRequest {
                mode: MatchMode::Regex,
                ..request("b", "X")
            },
        )
        .expect_err("regex never falls back to an in-process engine");
        match &error {
            PrepareError::RegexUnavailable { reason } => assert!(
                matches!(
                    reason.as_str(),
                    "helper-not-installed"
                        | "helper-spawn-failed"
                        | "engine-mismatch"
                        | "protocol"
                        | "helper-died"
                        | "frame"
                        | "busy"
                        | "cancelled"
                ),
                "typed helper reason, got {reason}"
            ),
            other => panic!("expected RegexUnavailable, got {other:?}"),
        }
        assert!(
            error.message().contains("literal matching still works"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn regex_find_without_helper_is_refused() {
        let error = prepare_find(
            "abc",
            &FindRequest {
                buffer_id: BufferId::from(1),
                revision: Revision::new(1),
                query: "b".into(),
                mode: MatchMode::Regex,
                case_sensitive: false,
                whole_word: false,
                offset: 0,
                scope: None,
            },
        )
        .expect_err("no helper sibling");
        assert!(matches!(error, PrepareError::RegexUnavailable { .. }));
    }

    #[test]
    fn helper_deadline_maps_to_typed_timeout() {
        // Deadline is its own outcome, never a match-count lie (§6.2).
        let error = worker_error(crate::text_worker::TextWorkerError::Deadline);
        assert!(matches!(error, PrepareError::RegexTimeout));
        assert!(error.message().contains("deadline"));
    }

    #[test]
    fn invalid_scope_endpoints_are_typed() {
        let err = scope_of(Some((5, 2))).expect_err("reversed scope");
        assert!(matches!(err, PrepareError::InvalidScope));
    }
}
