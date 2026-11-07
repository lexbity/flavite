//! Document close/merge command surface (RFC-0003 W12, D-10).
//!
//! One core close policy: a pending save flight refuses every close; a
//! dirty buffer refuses a plain close (the host must save, retain a
//! durable recovery snapshot, or discard explicitly). There is no
//! GUI-side bypass of these checks.

use crate::command::Invocation;
use crate::flavite::decode;
use flavite_vault::session::{Session, SessionCommand, SessionResult};

/// `note.close` arguments.
#[derive(Debug, serde::Deserialize)]
pub struct CloseArgs {
    /// Exact native note path.
    pub path: std::path::PathBuf,
    /// `clean` (default) refuses a dirty close; `retain_recovery` requires
    /// and retains a durable recovery snapshot covering the unsaved edits.
    #[serde(default)]
    pub policy: ClosePolicy,
}

/// The close policy for one `note.close`.
#[derive(Debug, Clone, Copy, Default, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosePolicy {
    /// Refuse a dirty close (`dirty-buffer-close` action required).
    #[default]
    Clean,
    /// Durably retain the unsaved edits as a recovery snapshot, then drop
    /// the buffer.
    RetainRecovery,
}

/// `conflict.apply_merge` arguments (the host confirms the merged text
/// against the `MergeReady` receipt before core applies it).
#[derive(Debug, serde::Deserialize)]
pub struct ApplyMergeArgs {
    /// Exact native note path.
    pub path: std::path::PathBuf,
    /// The buffer id from the `MergeReady` receipt.
    pub buffer_id: u64,
    /// The expected buffer revision from the receipt.
    pub revision: u64,
    /// The disk hash the merge was prepared against.
    pub disk_hash: String,
    /// The confirmed merged document text.
    pub merged: String,
}

/// # Errors
/// Invalid arguments or session transport errors.
pub fn handle_close(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: CloseArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::CloseNote {
        path: args.path,
        retain_recovery: matches!(args.policy, ClosePolicy::RetainRecovery),
    }))
}

/// # Errors
/// Invalid arguments or session transport errors.
pub fn handle_discard_edits(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: crate::recovery::RecoveryNoteArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::DiscardEdits { path: args.path }))
}

/// # Errors
/// Invalid arguments or session transport errors.
pub fn handle_apply_merge(
    session: &Session,
    invocation: &Invocation,
) -> Result<SessionResult, String> {
    let args: ApplyMergeArgs = decode(invocation)?;
    let disk_hash = crate::note_encoding::decode_hash32(&args.disk_hash, "disk_hash")?;
    Ok(session.execute(SessionCommand::ApplyMerge {
        path: args.path,
        buffer_id: flavite_buffer::revision::BufferId::from(args.buffer_id),
        revision: flavite_buffer::revision::Revision::new(args.revision),
        disk_hash,
        merged_bytes: args.merged.into_bytes(),
    }))
}
