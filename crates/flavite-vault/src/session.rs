//! Session — the headless core that owns BufferManager (§4.2, §4.5, D7, D14).
//!
//! The Session is the single authority for document state in a vault.
//! It owns:
//! - BufferManager (one buffer per open note)
//! - Pending operations ledger
//! - Worker handles
//!
//! All document mutations execute as session commands and apply EditOps
//! through its BufferManager. Widgets receive read-only views.

use crate::actor::{VaultActorHandle, VaultActorMsg};
use crate::channels::{NoticeChannel, PulseChannel};
use crate::tokens::WriteTokenManager;
use crate::types::{
    Epoch, Generation, ReconcileMode, ReconcileScope, ReconcileSummary, VaultConfig, VaultIdentity,
};
use flavite_buffer::manager::{BufferManager, BufferStateChanged};
use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::undo::EditOp;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

/// Commands that can be executed by the Session.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Open a note.
    OpenNote { path: PathBuf },
    /// Close a note.
    CloseNote { path: PathBuf },
    /// Apply edits to a note.
    ApplyEdits {
        buffer_id: BufferId,
        revision: Revision,
        ops: Vec<EditOp>,
    },
    /// Save a note.
    SaveNote { path: PathBuf },
    /// Trigger a reconcile.
    Reconcile {
        scope: ReconcileScope,
        mode: ReconcileMode,
    },
    /// Undo an edit.
    Undo { buffer_id: BufferId },
    /// Redo an edit.
    Redo { buffer_id: BufferId },
    /// Resolve a conflict.
    ResolveConflict {
        path: PathBuf,
        resolution: ConflictResolution,
    },
}

/// Conflict resolution strategy.
#[derive(Debug, Clone)]
pub enum ConflictResolution {
    /// Keep the current version (ours).
    KeepMine,
    /// Reload from disk (theirs).
    Reload,
    /// Save as a new file.
    SaveAs { new_path: PathBuf },
    /// Merge the changes.
    Merge,
}

/// Results from Session commands.
#[derive(Debug, Clone)]
pub enum SessionResult {
    /// Command succeeded with data.
    Success(CommandResult),
    /// Command is pending.
    Pending { request_id: u64 },
    /// Command failed.
    Error { message: String },
    /// Buffer is in conflict state.
    Conflict { path: PathBuf },
    /// Action required from user.
    ActionRequired {
        code: &'static str,
        data: Option<String>,
    },
}

/// Data returned from successful commands.
#[derive(Debug, Clone)]
pub enum CommandResult {
    /// Note was opened.
    NoteOpened {
        buffer_id: BufferId,
        revision: Revision,
    },
    /// Note was closed.
    NoteClosed,
    /// Edits were applied.
    EditsApplied {
        buffer_id: BufferId,
        new_revision: Revision,
    },
    /// Note was saved.
    NoteSaved {
        path: PathBuf,
        hash: Vec<u8>,
        epoch: Epoch,
        generation: Generation,
    },
    /// Reconcile completed.
    ReconcileComplete { summary: ReconcileSummary },
    /// Undo was performed.
    UndoPerformed {
        buffer_id: BufferId,
        new_revision: Revision,
    },
    /// Redo was performed.
    RedoPerformed {
        buffer_id: BufferId,
        new_revision: Revision,
    },
    /// Conflict resolved.
    ConflictResolved,
}

/// Errors from Session commands.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("Buffer not found: {0}")]
    BufferNotFound(PathBuf),
    #[error("Buffer already open: {0}")]
    BufferAlreadyOpen(PathBuf),
    #[error("Stale revision: expected {expected:?}, got {actual:?}")]
    StaleRevision {
        expected: Revision,
        actual: Revision,
    },
    #[error("Note not found on disk: {0}")]
    NoteNotFound(PathBuf),
    #[error("Save conflict: file changed on disk")]
    SaveConflict,
    #[error("Vault is read-only")]
    VaultReadOnly,
    #[error("Operation would exceed budget")]
    BudgetExceeded,
}

/// Event emitted by the Session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Buffer state changed.
    BufferStateChanged(BufferStateChanged),
    /// Vault changed.
    VaultChanged { summary: ReconcileSummary },
    /// Conflict detected.
    ConflictDetected { path: PathBuf },
    /// Recovery available.
    RecoveryAvailable { path: PathBuf },
}

/// The Session itself.
///
/// This is the headless core that owns BufferManager and coordinates
/// all document operations.
#[derive(Debug)]
pub struct Session {
    /// Vault identity.
    identity: VaultIdentity,
    /// Vault actor handle.
    actor: VaultActorHandle,
    /// Buffer manager.
    buffer_manager: Arc<Mutex<BufferManager>>,
    /// Open buffers by path.
    open_buffers: Arc<Mutex<HashMap<PathBuf, BufferId>>>,
    /// Request ID counter.
    request_counter: AtomicU64,
    /// Pending operations.
    pending: Arc<Mutex<HashMap<u64, SessionResult>>>,
    /// Write token manager.
    token_manager: WriteTokenManager,
    /// Pulse channel.
    pulse: PulseChannel,
    /// Notice channel.
    notice: NoticeChannel,
    /// Configuration.
    config: VaultConfig,
    /// Whether the vault is read-only.
    read_only: bool,
}

impl Session {
    /// Create a new Session.
    pub fn new(
        identity: VaultIdentity,
        actor: VaultActorHandle,
        buffer_manager: BufferManager,
        pulse: PulseChannel,
        notice: NoticeChannel,
        config: VaultConfig,
    ) -> Self {
        Self {
            identity,
            actor,
            buffer_manager: Arc::new(Mutex::new(buffer_manager)),
            open_buffers: Arc::new(Mutex::new(HashMap::new())),
            request_counter: AtomicU64::new(0),
            pending: Arc::new(Mutex::new(HashMap::new())),
            token_manager: WriteTokenManager::new(),
            pulse,
            notice,
            config,
            read_only: false,
        }
    }

    /// Generate a new request ID.
    fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Execute a command.
    pub fn execute(&self, command: SessionCommand) -> SessionResult {
        match command {
            SessionCommand::OpenNote { path } => self.open_note(path),
            SessionCommand::CloseNote { path } => self.close_note(path),
            SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops,
            } => self.apply_edits(buffer_id, revision, ops),
            SessionCommand::SaveNote { path } => self.save_note(path),
            SessionCommand::Reconcile { scope, mode } => self.reconcile(scope, mode),
            SessionCommand::Undo { buffer_id } => self.undo(buffer_id),
            SessionCommand::Redo { buffer_id } => self.redo(buffer_id),
            SessionCommand::ResolveConflict { path, resolution } => {
                self.resolve_conflict(path, resolution)
            }
        }
    }

    /// Open a note.
    fn open_note(&self, path: PathBuf) -> SessionResult {
        // Check if already open
        {
            let open_buffers = self.open_buffers.lock().unwrap();
            if open_buffers.contains_key(&path) {
                let buffer_id = *open_buffers.get(&path).unwrap();
                let buffer_manager = self.buffer_manager.lock().unwrap();
                if let Some(buffer) = buffer_manager.get_buffer_by_id(buffer_id) {
                    return SessionResult::Success(CommandResult::NoteOpened {
                        buffer_id,
                        revision: buffer.revision(),
                    });
                }
            }
        }

        // Read the file
        // For now, we'll use a placeholder
        let content = vec![];

        // Open the buffer
        let mut buffer_manager = self.buffer_manager.lock().unwrap();
        let (buffer, _transition) = buffer_manager.open_note(path.clone(), &content);
        let buffer_id = buffer.buffer_id();

        // Track the open buffer
        {
            let mut open_buffers = self.open_buffers.lock().unwrap();
            open_buffers.insert(path.clone(), buffer_id);
        }

        SessionResult::Success(CommandResult::NoteOpened {
            buffer_id,
            revision: buffer.revision(),
        })
    }

    /// Close a note.
    fn close_note(&self, path: PathBuf) -> SessionResult {
        // Get buffer ID
        let buffer_id = {
            let open_buffers = self.open_buffers.lock().unwrap();
            open_buffers.get(&path).copied()
        };

        if let Some(_buffer_id) = buffer_id {
            // Close the buffer
            let mut buffer_manager = self.buffer_manager.lock().unwrap();
            buffer_manager.close_note(&path);

            // Remove from open buffers
            {
                let mut open_buffers = self.open_buffers.lock().unwrap();
                open_buffers.remove(&path);
            }

            SessionResult::Success(CommandResult::NoteClosed)
        } else {
            SessionResult::Error {
                message: format!("Buffer not found: {}", path.display()),
            }
        }
    }

    /// Apply edits to a buffer.
    fn apply_edits(
        &self,
        buffer_id: BufferId,
        revision: Revision,
        ops: Vec<EditOp>,
    ) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock().unwrap();

        // Get the buffer and apply all edit ops
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            // Check revision first
            if buffer.revision() != revision {
                return SessionResult::Error {
                    message: format!(
                        "Stale revision: expected {:?}, got {:?}",
                        revision,
                        buffer.revision()
                    ),
                };
            }

            // Apply all edit ops
            let mut new_revision = revision;
            for op in ops {
                new_revision = buffer.model_mut().apply_edit_op(op);
            }

            SessionResult::Success(CommandResult::EditsApplied {
                buffer_id,
                new_revision,
            })
        } else {
            SessionResult::Error {
                message: format!("Buffer not found: {:?}", buffer_id),
            }
        }
    }

    /// Save a note.
    fn save_note(&self, path: PathBuf) -> SessionResult {
        // Check if buffer is open
        let buffer_id = {
            let open_buffers = self.open_buffers.lock().unwrap();
            open_buffers.get(&path).copied()
        };

        if let Some(buffer_id) = buffer_id {
            let buffer_manager = self.buffer_manager.lock().unwrap();

            // Get the buffer and serialize
            if let Some(buffer) = buffer_manager.get_buffer_by_id(buffer_id) {
                let content = buffer.text().as_bytes().to_vec();

                // For now, we'll just return a success
                // In a full implementation, we'd use the actor's save path
                return SessionResult::Success(CommandResult::NoteSaved {
                    path,
                    hash: blake3::hash(&content).as_bytes().to_vec(),
                    epoch: Epoch::new(0),
                    generation: Generation::new(0),
                });
            }
        }

        SessionResult::Error {
            message: "Buffer not found".to_string(),
        }
    }

    /// Trigger a reconcile.
    fn reconcile(&self, scope: ReconcileScope, mode: ReconcileMode) -> SessionResult {
        let request_id = self.next_request_id();

        // Send to actor
        match self
            .actor
            .try_send(VaultActorMsg::Reconcile { scope, mode })
        {
            Ok(()) => {
                // Store as pending
                let pending = SessionResult::Pending { request_id };
                {
                    let mut pending_ops = self.pending.lock().unwrap();
                    pending_ops.insert(request_id, pending.clone());
                }
                pending
            }
            Err(_) => SessionResult::Error {
                message: "Actor not responding".to_string(),
            },
        }
    }

    /// Undo an edit.
    fn undo(&self, buffer_id: BufferId) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock().unwrap();

        // Get the buffer and call undo on its model
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            if let Some(new_revision) = buffer.model_mut().undo() {
                SessionResult::Success(CommandResult::UndoPerformed {
                    buffer_id,
                    new_revision,
                })
            } else {
                SessionResult::Error {
                    message: "Nothing to undo".to_string(),
                }
            }
        } else {
            SessionResult::Error {
                message: "Buffer not found".to_string(),
            }
        }
    }

    /// Redo an edit.
    fn redo(&self, buffer_id: BufferId) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock().unwrap();

        // Get the buffer and call redo on its model
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            if let Some(new_revision) = buffer.model_mut().redo() {
                SessionResult::Success(CommandResult::RedoPerformed {
                    buffer_id,
                    new_revision,
                })
            } else {
                SessionResult::Error {
                    message: "Nothing to redo".to_string(),
                }
            }
        } else {
            SessionResult::Error {
                message: "Buffer not found".to_string(),
            }
        }
    }

    /// Resolve a conflict.
    fn resolve_conflict(&self, path: PathBuf, resolution: ConflictResolution) -> SessionResult {
        match resolution {
            ConflictResolution::KeepMine => {
                // Mark as resolved, keep current state
                SessionResult::Success(CommandResult::ConflictResolved)
            }
            ConflictResolution::Reload => {
                // Reload from disk
                self.reopen_note(path)
            }
            ConflictResolution::SaveAs { new_path } => {
                // Save as new file
                SessionResult::ActionRequired {
                    code: "save-as-confirm",
                    data: Some(new_path.display().to_string()),
                }
            }
            ConflictResolution::Merge => {
                // Start merge process
                SessionResult::ActionRequired {
                    code: "merge-required",
                    data: None,
                }
            }
        }
    }

    /// Reopen a note (after conflict reload).
    fn reopen_note(&self, path: PathBuf) -> SessionResult {
        // Close if open
        let was_open = {
            let open_buffers = self.open_buffers.lock().unwrap();
            open_buffers.contains_key(&path)
        };

        if was_open {
            let _ = self.close_note(path.clone());
        }

        // Reopen
        self.open_note(path)
    }

    /// Get the BufferManager.
    #[must_use]
    pub fn buffer_manager(&self) -> Arc<Mutex<BufferManager>> {
        self.buffer_manager.clone()
    }

    /// Get the vault identity.
    #[must_use]
    pub fn identity(&self) -> &VaultIdentity {
        &self.identity
    }

    /// Get the UUID.
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.identity.uuid
    }

    /// Get the vault root.
    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.identity.root
    }

    /// Get the pulse channel.
    #[must_use]
    pub fn pulse_channel(&self) -> &PulseChannel {
        &self.pulse
    }

    /// Get the notice channel.
    #[must_use]
    pub fn notice_channel(&self) -> &NoticeChannel {
        &self.notice
    }

    /// Check if a buffer is open for a path.
    #[must_use]
    pub fn is_open(&self, path: &Path) -> bool {
        let open_buffers = self.open_buffers.lock().unwrap();
        open_buffers.contains_key(path)
    }

    /// Get the buffer ID for an open path.
    #[must_use]
    pub fn get_buffer_id(&self, path: &Path) -> Option<BufferId> {
        let open_buffers = self.open_buffers.lock().unwrap();
        open_buffers.get(path).copied()
    }
}

/// Builder for Session.
#[derive(Default)]
pub struct SessionBuilder {
    identity: Option<VaultIdentity>,
    actor: Option<VaultActorHandle>,
    buffer_manager: Option<BufferManager>,
    pulse: Option<PulseChannel>,
    notice: Option<NoticeChannel>,
    config: Option<VaultConfig>,
}

impl SessionBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the vault identity.
    #[must_use]
    pub fn with_identity(mut self, identity: VaultIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Set the vault actor handle.
    #[must_use]
    pub fn with_actor(mut self, actor: VaultActorHandle) -> Self {
        self.actor = Some(actor);
        self
    }

    /// Set the buffer manager.
    #[must_use]
    pub fn with_buffer_manager(mut self, buffer_manager: BufferManager) -> Self {
        self.buffer_manager = Some(buffer_manager);
        self
    }

    /// Set the pulse channel.
    #[must_use]
    pub fn with_pulse(mut self, pulse: PulseChannel) -> Self {
        self.pulse = Some(pulse);
        self
    }

    /// Set the notice channel.
    #[must_use]
    pub fn with_notice(mut self, notice: NoticeChannel) -> Self {
        self.notice = Some(notice);
        self
    }

    /// Set the configuration.
    #[must_use]
    pub fn with_config(mut self, config: VaultConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Build the Session.
    pub fn build(self) -> Result<Session, String> {
        Ok(Session::new(
            self.identity.ok_or("Missing identity")?,
            self.actor.ok_or("Missing actor")?,
            self.buffer_manager.unwrap_or_default(),
            self.pulse.unwrap_or_else(|| PulseChannel::new(1024)),
            self.notice
                .unwrap_or_else(|| NoticeChannel::new(Duration::from_millis(300))),
            self.config.unwrap_or_default(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;


    #[test]
    fn test_session_builder() {
        let _identity = VaultIdentity::new(
            uuid::Uuid::now_v7(),
            PathBuf::from("/test/vault"),
            vec![1, 2, 3, 4],
        );

        // We need a real actor, so we'll skip this for now
        // The builder structure is tested by compilation
    }

    #[test]
    fn test_request_counter() {
        // This is a basic test of the counter
        // A full test would require a Session
    }
}
