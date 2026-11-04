//! Recovery command surface (RFC-0003 W12, D-10/D-12).
//!
//! Recovery snapshots are core-owned bytes; hosts reach them only through
//! these typed commands against the session policy. `recovery.restore`
//! installs a retained snapshot into the note's buffer (opening the note
//! if needed, per the core restore policy); `recovery.discard` deletes a
//! retained copy; `recovery.flush` durably checkpoints a dirty buffer now
//! and returns the revision-bound receipt.

use crate::command::Invocation;
use crate::flavite::decode;
use flavite_vault::session::{Session, SessionCommand, SessionResult};

/// `recovery.restore` / `recovery.discard` arguments.
#[derive(Debug, serde::Deserialize)]
pub struct RecoveryNoteArgs {
    /// Exact native note path.
    pub path: std::path::PathBuf,
}

/// `recovery.flush` arguments.
#[derive(Debug, serde::Deserialize)]
pub struct FlushRecoveryArgs {
    /// Exact native note path.
    pub path: std::path::PathBuf,
}

/// # Errors
/// Invalid arguments or session transport errors.
pub fn handle_restore(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: RecoveryNoteArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::RestoreRecovery { path: args.path }))
}

/// # Errors
/// Invalid arguments or session transport errors.
pub fn handle_discard(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: RecoveryNoteArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::DiscardRecovery { path: args.path }))
}

/// # Errors
/// Invalid arguments or session transport errors.
pub fn handle_flush(session: &Session, invocation: &Invocation) -> Result<SessionResult, String> {
    let args: FlushRecoveryArgs = decode(invocation)?;
    Ok(session.execute(SessionCommand::FlushRecovery { path: args.path }))
}
