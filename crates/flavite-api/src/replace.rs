//! Vault source Find and sealed replacement preview commands (RFC-0006
//! S07): `source_find.start/page/close` and `replace.preview/select/status`.
//!
//! All S07 commands are read-only (ReadNotes): source find is an
//! observation vector over a frozen scope; preview computes and seals but
//! never applies. Apply/Cancel/Resume/Revert/forget are S08/S11 commands
//! and are intentionally not registered here. Read-only vaults refuse
//! `replace.preview` at the facade with a typed non-applicable report
//! (D-09), while Find remains available under the read budget.

use crate::command::Invocation;
use crate::flavite::decode;
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use serde::Deserialize;

/// Wrapper for `source_find.start`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFindStartArgs {
    /// Literal or regex query. Empty literal is Idle (typed refusal).
    pub query: String,
    /// Match mode: `literal` (default) or `regex`.
    pub mode: Option<String>,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub whole_word: bool,
}

/// Wrapper for `source_find.page`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFindPageArgs {
    /// Hex finder id from the start result.
    pub finder_id: String,
    /// Note ordinal to begin at (default 0).
    #[serde(default)]
    pub note_index: u64,
    /// Match ordinal within the first note (default 0).
    #[serde(default)]
    pub match_offset: u64,
    /// Maximum matches on this page (clamped 1..=200).
    #[serde(default = "default_page_len")]
    pub page_len: usize,
}

/// Wrapper for `source_find.close`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceFindCloseArgs {
    pub finder_id: String,
}

/// Wrapper for `replace.preview`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplacePreviewArgs {
    /// Literal or regex query. Empty literal is Idle.
    pub query: String,
    /// Replacement text (dollar grammar in regex mode, verbatim literal).
    pub replacement: String,
    /// Match mode: `literal` (default) or `regex`.
    pub mode: Option<String>,
    #[serde(default)]
    pub case_sensitive: bool,
    #[serde(default)]
    pub whole_word: bool,
    /// Draft inclusion is NOT available in S07 (S09); requesting it is a
    /// typed refusal.
    #[serde(default)]
    pub include_drafts: bool,
}

/// Wrapper for `replace.select`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceSelectArgs {
    /// Hex preview id from `replace.preview`.
    pub preview_id: String,
    /// Selection: `"all"`, `"none"`, or `{"keys": ["<hex-native>", ...]}`.
    pub select: serde_json::Value,
}

/// Wrapper for `replace.status`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceStatusArgs {
    pub preview_id: String,
    /// Row offset into the candidate list (default 0).
    #[serde(default)]
    pub offset: usize,
    /// Maximum rows (clamped 1..=200, default 50).
    #[serde(default = "default_page_len")]
    pub limit: usize,
}

/// Wrapper for `replace.apply` (RFC-0006 S08/D-10): frozen selection
/// revision + caller-generated idempotency key.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceApplyArgs {
    pub preview_id: String,
    pub selection_revision: u64,
    /// Caller-generated 128-bit hex idempotency key.
    pub idempotency_key: String,
}

/// Wrapper for `replace.cancel`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceCancelArgs {
    pub job_id: String,
}

/// Wrapper for `replace.resume`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceResumeArgs {
    pub job_id: String,
    pub idempotency_key: String,
}

/// Wrapper for `replace.job_status`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReplaceJobStatusArgs {
    pub job_id: String,
}

fn default_page_len() -> usize {
    50
}

/// Parse a hex 128-bit core-issued id (32 lowercase hex chars).
fn decode_id(value: &str) -> Result<u128, String> {
    if value.len() != 32 {
        return Err("id must be 32 hex characters".to_owned());
    }
    let bytes = hex::decode(value).map_err(|_| "id must be 32 hex characters")?;
    let bytes: [u8; 16] = bytes
        .try_into()
        .map_err(|_| "id must be 32 hex characters")?;
    Ok(u128::from_be_bytes(bytes))
}

/// `source_find.start` (ReadNotes): frozen scope, exact per-note matches,
/// first page. Pending; completes through normal routing.
pub(crate) fn handle_source_find_start(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: SourceFindStartArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::SourceFindStart {
        request_id: invocation.request_id,
        request: flavite_vault::replace::preview::SourceFindRequest {
            query: args.query,
            mode: decode_mode(args.mode.as_deref())?,
            case_sensitive: args.case_sensitive,
            whole_word: args.whole_word,
        },
    }))
}

/// `source_find.page` (ReadNotes): continuation by (note ordinal, match
/// ordinal); replay is idempotent.
pub(crate) fn handle_source_find_page(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: SourceFindPageArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::SourceFindPage {
        finder_id: decode_id(&args.finder_id)?,
        note_index: args.note_index,
        match_offset: args.match_offset,
        page_len: args.page_len,
    }))
}

/// `source_find.close` (ReadNotes): drops the finder spool.
pub(crate) fn handle_source_find_close(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: SourceFindCloseArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::SourceFindClose {
        finder_id: decode_id(&args.finder_id)?,
    }))
}

/// `replace.preview` (ReadNotes): full scan, exact outputs, sealed
/// manifest. Never applies. Read-only vaults refuse at the facade.
pub(crate) fn handle_replace_preview(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ReplacePreviewArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::ReplacePreviewPrepare {
        request_id: invocation.request_id,
        request: flavite_vault::replace::preview::PreviewRequest {
            query: args.query,
            replacement: args.replacement,
            mode: decode_mode(args.mode.as_deref())?,
            case_sensitive: args.case_sensitive,
            whole_word: args.whole_word,
            include_drafts: args.include_drafts,
        },
    }))
}

/// `replace.select` (ReadNotes): records a selection revision/digest on a
/// sealed preview. Local identity, never a mutation grant.
pub(crate) fn handle_replace_select(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ReplaceSelectArgs = decode(invocation)?;
    let preview_id = decode_id(&args.preview_id)?;
    let selected = match &args.select {
        serde_json::Value::String(kind) if kind == "all" => {
            flavite_vault::session::ReplaceSelection::All
        }
        serde_json::Value::String(kind) if kind == "none" => {
            flavite_vault::session::ReplaceSelection::None
        }
        serde_json::Value::Object(map) => {
            let keys = map
                .get("keys")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    "select must be \"all\", \"none\" or {\"keys\": [...]}".to_owned()
                })?;
            let mut parsed = Vec::with_capacity(keys.len());
            for key in keys {
                let text = key
                    .as_str()
                    .ok_or_else(|| "selection keys must be hex strings".to_owned())?;
                if text.len() != hex::encode([0u8]).len() * 2 && text.len() % 2 != 0 {
                    return Err("selection key is not even-length hex".to_owned());
                }
                hex::decode(text).map_err(|_| "selection key is not hex".to_owned())?;
                parsed.push(text.to_owned());
            }
            flavite_vault::session::ReplaceSelection::Keys(parsed)
        }
        other => {
            return Err(format!(
                "select must be \"all\", \"none\" or {{\"keys\": [...]}}, got {other}"
            ));
        }
    };
    Ok(session.execute(SessionCommand::ReplaceSelect {
        preview_id,
        selected,
    }))
}

/// `replace.status` (ReadNotes): current sealed-preview status and totals.
pub(crate) fn handle_replace_status(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ReplaceStatusArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::ReplaceStatus {
        preview_id: decode_id(&args.preview_id)?,
    }))
}

/// `replace.apply` (WriteNotes): runs the durable job; idempotent.
pub(crate) fn handle_replace_apply(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ReplaceApplyArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::ReplaceApply {
        request_id: invocation.request_id,
        preview_id: decode_id(&args.preview_id)?,
        selection_revision: args.selection_revision,
        idempotency_key: decode_id(&args.idempotency_key)?,
    }))
}

/// `replace.cancel` (WriteNotes): stops admission of the next item.
pub(crate) fn handle_replace_cancel(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ReplaceCancelArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::ReplaceCancel {
        job_id: decode_id(&args.job_id)?,
    }))
}

/// `replace.resume` (WriteNotes): explicit resume of a paused/partial job.
pub(crate) fn handle_replace_resume(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ReplaceResumeArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::ReplaceResume {
        request_id: invocation.request_id,
        job_id: decode_id(&args.job_id)?,
        idempotency_key: decode_id(&args.idempotency_key)?,
    }))
}

/// `replace.job_status` (ReadNotes): current job phase and totals.
pub(crate) fn handle_replace_job_status(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ReplaceJobStatusArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::ReplaceJobStatus {
        job_id: decode_id(&args.job_id)?,
    }))
}

/// Stable snake_case job phase string.
#[must_use]
pub fn job_phase_str(phase: flavite_vault::replace::JobPhase) -> &'static str {
    phase.label()
}

/// One job status projection (phase + totals; never note text).
#[must_use]
pub fn job_status_json(
    phase: flavite_vault::replace::JobPhase,
    totals: &flavite_vault::replace::JobTotals,
) -> serde_json::Value {
    serde_json::json!({
        "phase": job_phase_str(phase),
        "totals": {
            "selected": totals.selected,
            "saved": totals.saved,
            "index_pending": totals.index_pending,
            "conflicts": totals.conflicts,
            "stale": totals.stale,
            "busy": totals.busy,
            "unsupported": totals.unsupported,
            "needs_recovery": totals.needs_recovery,
            "satisfied_unattributed": totals.satisfied_unattributed,
            "not_started": totals.not_started,
        },
    })
}

/// Parse the wire match mode (§6.1 explicit snake_case discriminants).
fn decode_mode(value: Option<&str>) -> Result<flavite_text_search::matcher::MatchMode, String> {
    match value.unwrap_or("literal") {
        "literal" => Ok(flavite_text_search::matcher::MatchMode::Literal),
        "regex" => Ok(flavite_text_search::matcher::MatchMode::Regex),
        other => Err(format!("mode must be `literal` or `regex`, got {other}")),
    }
}

/// The wire encoding for core-issued ids (32 lowercase hex chars).
#[must_use]
pub fn encode_id(id: u128) -> String {
    hex::encode(id.to_be_bytes())
}

/// Stable snake_case exclusion reason strings (§6.1 explicit discriminants).
#[must_use]
pub fn exclusion_reason_str(reason: flavite_vault::replace::ExclusionReason) -> &'static str {
    match reason {
        flavite_vault::replace::ExclusionReason::DirtyBuffer => "dirty_buffer",
        flavite_vault::replace::ExclusionReason::ConflictedBuffer => "conflicted_buffer",
        flavite_vault::replace::ExclusionReason::OrphanedBuffer => "orphaned_buffer",
        flavite_vault::replace::ExclusionReason::Missing => "missing",
        flavite_vault::replace::ExclusionReason::Unreadable => "unreadable",
        flavite_vault::replace::ExclusionReason::Oversize => "oversize",
        flavite_vault::replace::ExclusionReason::Undecodable => "undecodable",
        flavite_vault::replace::ExclusionReason::OutputTooLarge => "output_too_large",
    }
}

/// One source-find page as JSON (S07).
#[must_use]
pub fn find_page_json(page: &flavite_vault::session::SourceFindPageData) -> serde_json::Value {
    serde_json::json!({
        "note_index": page.note_index,
        "note_total": page.note_total,
        "match_offset": page.match_offset,
        "rows": page.rows.iter().map(|row| serde_json::json!({
            "ordinal": row.ordinal,
            "path": row.path,
            "title": row.title,
            "note_matches": row.note_matches,
            "matches": row.matches.iter().map(|(start, end)| [*start, *end]).collect::<Vec<[u64; 2]>>(),
        })).collect::<Vec<_>>(),
        "next_note_index": page.next_note_index,
        "next_match_offset": page.next_match_offset,
    })
}

/// The sealed plan header as JSON (identity + totals; never note bodies).
#[must_use]
pub fn preview_plan_json(manifest: &flavite_vault::replace::PreviewManifest) -> serde_json::Value {
    serde_json::json!({
        "job_id": manifest.job_id,
        "vault_uuid": manifest.vault_uuid,
        "plan_digest": manifest.plan_digest,
        "record_digest": manifest.record_digest(),
        "scope_version": {
            "epoch": manifest.scope_version.epoch,
            "generation": manifest.scope_version.generation,
        },
        "source_policy": manifest.source_policy,
        "state": manifest.state,
        "totals": {
            "scoped": manifest.totals.scoped,
            "notes": manifest.totals.notes,
            "excluded": manifest.totals.excluded,
            "matched": manifest.totals.matched,
            "matches": manifest.totals.matches,
            "drafts": manifest.totals.drafts,
            "payload_bytes": manifest.totals.payload_bytes,
        },
        "payload_bytes": manifest.payload_bytes,
        "created_at_unix_ms": manifest.created_at_unix_ms,
        "expires_at_unix_ms": manifest.expires_at_unix_ms,
        "selection_revision": manifest.selection_revision,
        "selection_digest": manifest.selection_digest,
        "exclusions": manifest.exclusions.iter().map(|exclusion| serde_json::json!({
            "path": exclusion.path,
            "reason": exclusion_reason_str(exclusion.reason),
        })).collect::<Vec<_>>(),
    })
}

/// One candidate row as JSON (identity + hashes + sealed payload refs).
#[must_use]
pub fn preview_candidate_json(
    note: &flavite_vault::replace::NoteCandidateRecord,
) -> serde_json::Value {
    serde_json::json!({
        "item_id": note.item_id,
        "path": note.path,
        "path_native": note.path_native,
        "source_kind": "saved",
        "disk_hash": note.disk_hash,
        "content_hash": note.content_hash,
        "format": {
            "encoding": note.format.encoding,
            "bom": note.format.bom,
            "codec_version": note.codec_version,
            "hint_generation": note.hint_generation,
        },
        "before": { "hash": note.before.hash, "len": note.before.len },
        "after": { "hash": note.after.hash, "len": note.after.len },
        "match_count": note.match_count,
        "output_bytes": note.output_bytes,
        "no_change": note.no_change,
        "open_lineage": note.open_lineage.as_ref().map(|lineage| serde_json::json!({
            "buffer_id": lineage.buffer_id,
            "revision": lineage.revision,
        })),
    })
}

/// A page of candidate rows from a sealed manifest (S07).
#[must_use]
pub fn preview_page_json(
    manifest: &flavite_vault::replace::PreviewManifest,
    offset: usize,
    limit: usize,
) -> serde_json::Value {
    let offset = offset.min(manifest.notes.len());
    let end = offset.saturating_add(limit).min(manifest.notes.len());
    serde_json::json!({
        "offset": offset,
        "total": manifest.notes.len(),
        "rows": manifest.notes[offset..end]
            .iter()
            .map(preview_candidate_json)
            .collect::<Vec<_>>(),
    })
}
