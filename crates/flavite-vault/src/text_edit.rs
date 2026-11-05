//! Prepared literal note replacement (RFC-0006 S01; D-05/D-06).
//!
//! The matching/validation/output-bound work runs on bounded prepare
//! workers off the host turn; the Session owns the core-issued plan
//! registry and installs candidates only when the buffer, incarnation and
//! revision still match. One prepared replacement is one revision-guarded
//! batch and one isolated Undo unit; invalid preparation has zero note
//! effects. Regex capability is absent until S03 — this module is literal
//! only and says so.

use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::undo::EditOp;
use flavite_syntax::text::{Span, TextByte};
use flavite_text_search::matcher::{MatchMode, MatchOptions, find_matches_checked};

/// NFR-8: at most one million matches per job.
pub const MAX_MATCHES_PER_JOB: u64 = 1_000_000;
/// NFR-8/NFR-19: per-note normalized replacement output bound.
pub const MAX_OUTPUT_BYTES: u64 = 32 * 1024 * 1024;
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareError {
    /// The query is empty (Idle) or exceeds the 8 KiB bound (NFR-8).
    InvalidQuery(&'static str),
    /// The frozen scope is out of bounds or not char-boundary aligned.
    InvalidScope,
    /// The matcher refused the options.
    Matcher(flavite_text_search::matcher::MatchError),
    /// More matches than the job bound; never silently truncated.
    MatchLimitExceeded,
    /// The serialized output would exceed the per-note bound (NFR-8/19).
    OutputLimitExceeded,
}

/// The pure prepare step, executed on a prepare worker against the exact
/// immutable snapshot text. Returns ascending, non-overlapping edit
/// operations over the ORIGINAL text plus the match count; the combined
/// output bound is checked before any edit exists.
///
/// # Errors
///
/// [`PrepareError`] with zero effects; callers never see partial matches.
pub fn prepare_ops(
    text: &str,
    request: &ReplaceRequest,
) -> Result<(Vec<EditOp>, u64), PrepareError> {
    if request.query.is_empty() {
        return Err(PrepareError::InvalidQuery(IDLE_QUERY));
    }
    if request.query.len() > flavite_text_search::matcher::MAX_QUERY_BYTES {
        return Err(PrepareError::InvalidQuery(
            "query exceeds the 8 KiB bound",
        ));
    }
    let (scope_start, scope_text) = match request.scope {
        Some((start, end)) => {
            if start > end || end > text.len() {
                return Err(PrepareError::InvalidScope);
            }
            let Some(scope_text) = text.get(start..end) else {
                return Err(PrepareError::InvalidScope);
            };
            (start, scope_text)
        }
        None => (0usize, text),
    };
    let options = MatchOptions {
        mode: MatchMode::Literal,
        query: request.query.clone(),
        case_sensitive: request.case_sensitive,
        whole_word: request.whole_word,
    };
    let result = find_matches_checked(scope_text, &options, MAX_MATCHES_PER_JOB)
        .map_err(PrepareError::Matcher)?;
    if result.truncated {
        // Counting may be pending in the UI, but a prepared batch must be
        // exact: exceeding the bound refuses instead of silently stopping.
        return Err(PrepareError::MatchLimitExceeded);
    }
    let mut ops = Vec::with_capacity(result.matches.len());
    let mut match_bytes = 0u64;
    for hit in &result.matches {
        let start = scope_start + hit.range.start.0;
        let end = scope_start + hit.range.start.0 + hit.length;
        if end > text.len() || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            return Err(PrepareError::InvalidScope);
        }
        match_bytes = match_bytes
            .checked_add(hit.length as u64)
            .ok_or(PrepareError::OutputLimitExceeded)?;
        ops.push(EditOp::new(
            Span::new(TextByte(start), TextByte(end)),
            request.replacement.clone(),
        ));
    }
    let output_len = flavite_text_search::template::expanded_output_len(
        text.len() as u64,
        &request.replacement,
        ops.len() as u64,
        match_bytes,
    )
    .ok_or(PrepareError::OutputLimitExceeded)?;
    if output_len > MAX_OUTPUT_BYTES {
        return Err(PrepareError::OutputLimitExceeded);
    }
    let count = ops.len() as u64;
    Ok((ops, count))
}

/// FNV-1a 64 over the canonical options encoding: the digest is part of the
/// plan identity (D-05: scope and options are part of the plan digest).
#[must_use]
pub fn options_digest(request: &ReplaceRequest) -> u64 {
    let mut canonical = String::new();
    canonical.push_str("literal\u{1}");
    canonical.push_str(&request.query);
    canonical.push('\u{1}');
    canonical.push_str(&request.replacement);
    canonical.push('\u{1}');
    canonical.push_str(if request.case_sensitive { "cs" } else { "ci" });
    canonical.push_str(if request.whole_word { "ww" } else { "" });
    if let Some((start, end)) = request.scope {
        canonical.push_str(&format!("\u{1}{start}..{end}"));
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in canonical.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
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
        let (ops, count) = prepare_ops(text, &request("aa", "X")).expect("prepare");
        assert_eq!(count, 2);
        assert_eq!(ops[0].range.start.0, 0);
        assert_eq!(ops[1].range.start.0, 6);
        assert!(ops[0].range.end.0 <= ops[1].range.start.0);
    }

    #[test]
    fn empty_query_is_idle_and_limits_are_typed() {
        assert_eq!(
            prepare_ops("text", &request("", "X")),
            Err(PrepareError::InvalidQuery(IDLE_QUERY))
        );
        let long = "a".repeat(flavite_text_search::matcher::MAX_QUERY_BYTES + 1);
        assert!(matches!(
            prepare_ops("text", &request(&long, "X")),
            Err(PrepareError::InvalidQuery(_))
        ));
    }

    #[test]
    fn scope_is_frozen_and_output_bound_checked() {
        let (ops, count) = prepare_ops("aa|aa", &request("aa", "X")).expect("full");
        assert_eq!(count, 2);
        let (scoped, count) = prepare_ops(
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
        let text = "match".to_owned();
        let mut req = request("match", "X");
        req.replacement = huge;
        assert!(matches!(
            prepare_ops(&text, &req),
            Err(PrepareError::OutputLimitExceeded)
        ));
    }
}
