//! Typed replacement and find commands (RFC-0006 S01/S03; §6.1 v2
//! protocol): `text.find`, `text.prepare_replace` and `text.apply_replace`.
//!
//! The public API accepts a match plan/token, never client-supplied
//! "trusted" offsets from a previous preview. Prepare is a bounded
//! read-only computation (ReadNotes) returning a core-issued plan bound to
//! the exact buffer revision; apply is WriteNotes on the actual source and
//! installs the plan as one revision-guarded, isolated Undo batch (D-06).
//! `text.find` (S03) is the read-only span page for the current buffer:
//! the same case/word/regex options as replacement, literal in-process and
//! regex in the isolated helper (D-14), revision-bound results.

use crate::command::Invocation;
use crate::flavite::decode;
use flavite_buffer::revision::{BufferId, Revision};
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use serde::Deserialize;

/// Arguments for `text.prepare_replace`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareReplaceArgs {
    pub buffer_id: u64,
    /// The exact revision the caller observed; the plan binds to it.
    pub revision: u64,
    /// Literal or regex query. An empty literal query is Idle (D-05), not
    /// "match everywhere". Regex runs in the isolated helper (D-14).
    pub query: String,
    /// Match mode: `literal` (default) or `regex`.
    pub mode: Option<String>,
    /// Replacement text: `$` is verbatim in literal mode; the dollar
    /// capture grammar applies in regex mode (D-05).
    pub replacement: String,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub whole_word: bool,
    /// Optional frozen scope as half-open `TextByte` offsets at `revision`.
    pub scope: Option<(usize, usize)>,
}

/// Arguments for `text.find` (S03).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindArgs {
    pub buffer_id: u64,
    /// The exact revision the page is bound to; results never mix
    /// revisions (D-05/D-06).
    pub revision: u64,
    /// Literal or regex query. An empty query is Idle (D-05) and refused
    /// as a typed no-effect error.
    pub query: String,
    /// Match mode: `literal` (default) or `regex`.
    pub mode: Option<String>,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub whole_word: bool,
    /// Zero-based continuation ordinal returned by the previous page.
    /// Omitted for the first page.
    #[serde(default)]
    pub offset: u64,
    /// Optional frozen scope as half-open `TextByte` offsets at `revision`.
    pub scope: Option<(usize, usize)>,
}

/// Arguments for `text.apply_replace`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplyReplaceArgs {
    pub buffer_id: u64,
    /// Exact revision precondition for the install.
    pub revision: u64,
    /// Core-issued plan id returned by `text.prepare_replace`.
    pub plan_id: String,
}

/// `text.prepare_replace` (ReadNotes): no mutation; the plan is computed on
/// a bounded worker and expires 30 seconds after preparation.
pub(crate) fn handle_prepare_replace(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: PrepareReplaceArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::PrepareReplace {
        request_id: invocation.request_id,
        request: flavite_vault::text_edit::ReplaceRequest {
            buffer_id: BufferId::from(args.buffer_id),
            revision: Revision::new(args.revision),
            query: args.query,
            replacement: args.replacement,
            case_sensitive: args.case_sensitive,
            whole_word: args.whole_word,
            mode: decode_mode(args.mode.as_deref())?,
            scope: args.scope,
        },
    }))
}

/// `text.find` (ReadNotes): no mutation; the page is computed on a bounded
/// worker against the exact revision and completed through the normal
/// pending-result routing.
pub(crate) fn handle_find(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: FindArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::FindMatches {
        request_id: invocation.request_id,
        request: flavite_vault::text_edit::FindRequest {
            buffer_id: BufferId::from(args.buffer_id),
            revision: Revision::new(args.revision),
            query: args.query,
            mode: decode_mode(args.mode.as_deref())?,
            case_sensitive: args.case_sensitive,
            whole_word: args.whole_word,
            offset: args.offset,
            scope: args.scope,
        },
    }))
}

/// `text.apply_replace` (WriteNotes on source): installs the prepared
/// candidate once, under the exact revision precondition, as one isolated
/// Undo unit. Duplicate requests replay the same receipt (§6.9).
pub(crate) fn handle_apply_replace(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ApplyReplaceArgs = decode(invocation)?;
    let plan_id = decode_plan_id(&args.plan_id)?;
    Ok(session.execute(SessionCommand::ApplyReplace {
        buffer_id: BufferId::from(args.buffer_id),
        revision: Revision::new(args.revision),
        plan_id,
    }))
}

/// Parse the wire match mode (§6.1 explicit snake_case discriminants).
fn decode_mode(value: Option<&str>) -> Result<flavite_text_search::matcher::MatchMode, String> {
    match value.unwrap_or("literal") {
        "literal" => Ok(flavite_text_search::matcher::MatchMode::Literal),
        "regex" => Ok(flavite_text_search::matcher::MatchMode::Regex),
        other => Err(format!("mode must be `literal` or `regex`, got {other}")),
    }
}

/// Parse the hex-encoded 128-bit core-issued plan id (§6.1).
fn decode_plan_id(value: &str) -> Result<u128, String> {
    let bytes = hex::decode(value).map_err(|_| "plan_id must be 32 hex characters")?;
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| "plan_id must be 32 hex characters")?;
    Ok(u128::from_be_bytes(bytes))
}

/// The hex encoding used by the facade when returning plan ids.
#[must_use]
pub fn encode_plan_id(plan_id: u128) -> String {
    hex::encode(plan_id.to_be_bytes())
}
