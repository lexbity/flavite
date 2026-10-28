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
use crate::save::{DiskPrecondition, DurabilityOutcome, SaveFailureStage, SaveRequest};
use crate::tokens::WriteTokenManager;
use crate::types::{
    Epoch, Generation, ReconcileMode, ReconcileScope, ReconcileSummary, VaultConfig, VaultIdentity,
};
use flavite_buffer::manager::{BufferManager, BufferStateChanged};
use flavite_buffer::recovery::{RecoveryConfig, RecoveryWriter};
use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::undo::EditOp;
use std::collections::HashMap;
use std::collections::HashSet;
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
    /// Restore the recovery snapshot for a note after explicit user choice.
    RestoreRecovery { path: PathBuf },
    /// Discard the recovery snapshot for a note after explicit user choice.
    DiscardRecovery { path: PathBuf },
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
    /// Three-way merge preview. Conflicts are structured data and no markers
    /// are inserted into note content.
    MergeReady {
        path: PathBuf,
        result: crate::merge::MergeResult,
    },
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
    /// Recovery snapshot restored into the authoritative buffer.
    RecoveryRestored {
        buffer_id: BufferId,
        revision: Revision,
    },
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
    /// Paths that were absent when opened; creation uses an absent-file guard.
    absent_on_open: Arc<Mutex<HashSet<PathBuf>>>,
    /// Dirty-buffer recovery checkpoints. The host schedules `recovery_tick`
    /// using the configured buffer recovery interval.
    recovery_writer: Option<Arc<Mutex<RecoveryWriter>>>,
    /// Background recovery checkpoint driver owned by this Session.
    recovery_worker: Option<std::thread::JoinHandle<()>>,
    recovery_stop: Arc<std::sync::atomic::AtomicBool>,
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
        let manager = Arc::new(Mutex::new(buffer_manager));
        let recovery_writer = crate::identity::get_data_dir().map(|data| {
            let dir = data
                .join("flavite")
                .join("vaults")
                .join(identity.uuid.to_string())
                .join("recovery");
            Arc::new(Mutex::new(RecoveryWriter::new(RecoveryConfig::new(
                identity.uuid.to_string(),
                dir,
                manager.lock().unwrap().recovery_interval(),
            ))))
        });
        let recovery_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recovery_worker = recovery_writer.as_ref().map(|writer| {
            let manager = manager.clone();
            let writer = writer.clone();
            let stop = recovery_stop.clone();
            let root = identity.root.clone();
            let actor = actor.clone();
            std::thread::Builder::new()
                .name("vault-recovery".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let buffer_manager = manager.lock().unwrap();
                        let dirty = buffer_manager
                            .dirty_buffers()
                            .into_iter()
                            .map(|(path, buffer)| {
                                (
                                    native_note_key(path, &root),
                                    buffer.base_bytes().to_vec(),
                                    flavite_buffer::recovery::RecoveryFile::from_buffer(
                                        buffer, 0, false,
                                    ),
                                )
                            })
                            .collect::<Vec<_>>();
                        drop(buffer_manager);
                        for (key, base, snapshot) in dirty {
                            let due = writer.lock().unwrap().should_write_snapshot(&snapshot);
                            if due && actor.pin_merge_base(key.clone(), base).is_ok() {
                                writer.lock().unwrap().write_snapshot(&key, snapshot);
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })
                .expect("recovery worker thread creation failed")
        });
        Self {
            identity,
            actor,
            buffer_manager: manager,
            open_buffers: Arc::new(Mutex::new(HashMap::new())),
            absent_on_open: Arc::new(Mutex::new(HashSet::new())),
            recovery_writer,
            recovery_worker,
            recovery_stop,
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
            SessionCommand::RestoreRecovery { path } => self.restore_recovery(path),
            SessionCommand::DiscardRecovery { path } => self.discard_recovery(path),
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

        // Capture exact disk bytes before establishing the buffer's base hash.
        let content = match self.actor.read_file(path.clone()) {
            Ok(bytes) => bytes,
            Err(error) => {
                return SessionResult::Error {
                    message: error.to_string(),
                };
            }
        };
        let absent = content.is_none();
        let content = content.unwrap_or_default();

        // Open the buffer
        let mut buffer_manager = self.buffer_manager.lock().unwrap();
        let (buffer, _transition) = buffer_manager.open_note(path.clone(), &content);
        let buffer_id = buffer.buffer_id();

        if let Some(writer) = &self.recovery_writer {
            let key = native_note_key(&path, &self.identity.root);
            if let Some(recovery) = writer.lock().unwrap().read_for_note_key(&key) {
                let recovered = recovery.to_source_bytes();
                if blake3::hash(&recovered).as_bytes() != blake3::hash(&content).as_bytes() {
                    self.notice.send(crate::channels::Notice::RecoveryOffer {
                        note: flavite_buffer::manager::NoteRef::new(path.clone()),
                    });
                }
            }
        }

        // Track the open buffer
        {
            let mut open_buffers = self.open_buffers.lock().unwrap();
            open_buffers.insert(path.clone(), buffer_id);
        }
        if absent {
            self.absent_on_open.lock().unwrap().insert(path.clone());
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
            self.absent_on_open.lock().unwrap().remove(&path);

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
            let (bytes, revision, base_hash, base_bytes, state) = {
                let manager = self.buffer_manager.lock().unwrap();
                if let Some(buffer) = manager.get_buffer_by_id(buffer_id) {
                    (
                        buffer.model().to_source_bytes(),
                        buffer.revision(),
                        buffer.base_hash(),
                        buffer.base_bytes().to_vec(),
                        buffer.state(),
                    )
                } else {
                    return SessionResult::Error {
                        message: "Buffer not found".into(),
                    };
                }
            };
            if !state.allows_manual_save() {
                return SessionResult::Conflict { path };
            }
            let precondition = if self.absent_on_open.lock().unwrap().contains(&path) {
                DiskPrecondition::Absent
            } else if let Some(hash) = base_hash {
                DiskPrecondition::Existing(hash.as_bytes())
            } else {
                return SessionResult::Error {
                    message: "buffer has no disk base; explicit create resolution is required"
                        .into(),
                };
            };
            let request_id = self.next_request_id();
            let request = SaveRequest {
                request_id,
                path: path.clone(),
                note_key: native_note_key(&path, &self.identity.root),
                bytes: bytes.clone(),
                precondition,
                buffer_id: buffer_id.as_u64(),
                revision: revision.as_u64(),
            };
            let note_key = native_note_key(&path, &self.identity.root);
            if let Err(error) = self
                .actor
                .pin_merge_base(note_key.clone(), base_bytes.clone())
            {
                return SessionResult::Error {
                    message: format!("could not persist merge base before save: {error}"),
                };
            }
            match self.actor.save(request) {
                Ok(Ok(receipt)) => {
                    let mut manager = self.buffer_manager.lock().unwrap();
                    let caught_up = if let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) {
                        let caught_up = buffer.revision() == revision;
                        buffer.handle_save_completed_revision(revision, &bytes);
                        caught_up
                    } else {
                        false
                    };
                    drop(manager);
                    if let Some(writer) = &self.recovery_writer {
                        let writer = writer.lock().unwrap();
                        writer.retire_covered(buffer_id, revision);
                        if caught_up {
                            // A recovery produced by a prior process has a
                            // different BufferId; the note-keyed record is
                            // still covered by this exact saved revision.
                            writer.delete_for_note_key(&note_key);
                        }
                    }
                    if self
                        .actor
                        .pin_merge_base(note_key.clone(), bytes.clone())
                        .is_ok()
                    {
                        let previous_base_hash = blake3::hash(&base_bytes).to_hex().to_string();
                        let saved_base_hash = blake3::hash(&bytes).to_hex().to_string();
                        if previous_base_hash != saved_base_hash {
                            let _ = self.actor.unpin_merge_base(note_key, previous_base_hash);
                        }
                    }
                    self.absent_on_open.lock().unwrap().remove(&path);
                    return SessionResult::Success(CommandResult::NoteSaved {
                        path,
                        hash: receipt.hash.to_vec(),
                        epoch: Epoch::new(receipt.epoch),
                        generation: Generation::new(receipt.generation),
                    });
                }
                Ok(Err(failure)) => {
                    if failure.stage == SaveFailureStage::Compare
                        && let Ok(Some(theirs)) = self.actor.read_file(path.clone())
                    {
                        if let Some(buffer) = self
                            .buffer_manager
                            .lock()
                            .unwrap()
                            .get_buffer_by_id_mut(buffer_id)
                        {
                            buffer.check_external_change(&theirs);
                        }
                        self.notice.send(crate::channels::Notice::Conflict {
                            note: flavite_buffer::manager::NoteRef::new(path.clone()),
                        });
                    }
                    if matches!(
                        failure.outcome,
                        DurabilityOutcome::ReplacedIndexPending
                            | DurabilityOutcome::ReplacedDurabilityUncertain
                            | DurabilityOutcome::OutcomeUnknown
                    ) {
                        // A post-replace failure has no successful save
                        // receipt. Re-read the destination before allowing a
                        // retry so its disk precondition describes the actual
                        // observed bytes, not the pre-save base. If the read
                        // itself is unavailable, retain the old precondition;
                        // the save path will refuse a stale retry by comparing
                        // against disk again.
                        if let Ok(Some(observed)) = self.actor.read_file(path.clone()) {
                            let mut manager = self.buffer_manager.lock().unwrap();
                            if let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) {
                                if observed == bytes {
                                    buffer.record_replaced_disk_snapshot(&observed);
                                } else {
                                    // The observed destination no longer
                                    // contains our staged bytes. Preserve the
                                    // conflict protocol instead of treating an
                                    // intervening external write as a new base.
                                    buffer.check_external_change(&observed);
                                }
                            }
                            drop(manager);
                            self.absent_on_open.lock().unwrap().remove(&path);
                        }
                    }
                    return SessionResult::Error {
                        message: failure.to_string(),
                    };
                }
                Err(error) => {
                    return SessionResult::Error {
                        message: error.to_string(),
                    };
                }
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
            ConflictResolution::KeepMine => self.resolve_keep_mine(path),
            ConflictResolution::Reload => self.resolve_reload(path),
            ConflictResolution::SaveAs { new_path } => {
                // Save as new file
                SessionResult::ActionRequired {
                    code: "save-as-confirm",
                    data: Some(new_path.display().to_string()),
                }
            }
            ConflictResolution::Merge => self.prepare_merge(path),
        }
    }

    fn resolve_keep_mine(&self, path: PathBuf) -> SessionResult {
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "Buffer not found".into(),
            };
        };
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            theirs.clone(),
            crate::history::HistoryReason::ConflictResolution,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot conflict bytes: {error}"),
            };
        }
        let mut manager = self.buffer_manager.lock().unwrap();
        match manager
            .get_buffer_by_id_mut(buffer_id)
            .and_then(|buffer| buffer.resolve_keep_local(&theirs).map(|_| ()))
        {
            Some(()) => SessionResult::Success(CommandResult::ConflictResolved),
            None => SessionResult::Error {
                message: "external bytes are not valid UTF-8".into(),
            },
        }
    }

    fn resolve_reload(&self, path: PathBuf) -> SessionResult {
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "Buffer not found".into(),
            };
        };
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        let mine = {
            let manager = self.buffer_manager.lock().unwrap();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => buffer.model().to_source_bytes(),
                None => {
                    return SessionResult::Error {
                        message: "Buffer not found".into(),
                    };
                }
            }
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            mine,
            crate::history::HistoryReason::ConflictResolution,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot discarded bytes: {error}"),
            };
        }
        let mut manager = self.buffer_manager.lock().unwrap();
        match manager
            .get_buffer_by_id_mut(buffer_id)
            .and_then(|buffer| buffer.reload_from_disk(&theirs))
        {
            Some(_) => SessionResult::Success(CommandResult::ConflictResolved),
            None => SessionResult::Error {
                message: "external bytes are not valid UTF-8".into(),
            },
        }
    }

    fn prepare_merge(&self, path: PathBuf) -> SessionResult {
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "Buffer not found".into(),
            };
        };
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        let (base, mine) = {
            let manager = self.buffer_manager.lock().unwrap();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => (
                    buffer.base_bytes().to_vec(),
                    buffer.model().to_source_bytes(),
                ),
                None => {
                    return SessionResult::Error {
                        message: "Buffer not found".into(),
                    };
                }
            }
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            theirs.clone(),
            crate::history::HistoryReason::Merge,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot merge input: {error}"),
            };
        }
        let (Ok(base), Ok(mine), Ok(theirs)) = (
            std::str::from_utf8(&base),
            std::str::from_utf8(&mine),
            std::str::from_utf8(&theirs),
        ) else {
            return SessionResult::ActionRequired {
                code: "merge-unsupported-encoding",
                data: Some(path.display().to_string()),
            };
        };
        SessionResult::MergeReady {
            path,
            result: crate::merge::three_way_merge(base, mine, theirs),
        }
    }

    fn restore_recovery(&self, path: PathBuf) -> SessionResult {
        let Some(writer) = &self.recovery_writer else {
            return SessionResult::Error {
                message: "recovery storage is unavailable".into(),
            };
        };
        let key = native_note_key(&path, &self.identity.root);
        let Some(recovery) = writer.lock().unwrap().read_for_note_key(&key) else {
            return SessionResult::Error {
                message: "no valid recovery snapshot exists for this note".into(),
            };
        };
        let disk = match self.actor.read_file(path.clone()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => Vec::new(),
            Err(error) => {
                return SessionResult::Error {
                    message: error.to_string(),
                };
            }
        };
        if !self.is_open(&path) {
            match self.open_note(path.clone()) {
                SessionResult::Success(_) => {}
                other => return other,
            }
        }
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "buffer disappeared while restoring recovery".into(),
            };
        };
        let recovered = recovery.to_source_bytes();
        let transition = self
            .buffer_manager
            .lock()
            .unwrap()
            .get_buffer_by_id_mut(buffer_id)
            .and_then(|buffer| buffer.restore_recovery(&recovered, &disk, recovery.base_hash));
        if transition.is_none() {
            return SessionResult::Error {
                message: "recovery snapshot could not be applied".into(),
            };
        }
        if recovery.base_hash.is_some_and(|base| {
            base != flavite_buffer::revision::BaseHash::from_bytes(*blake3::hash(&disk).as_bytes())
        }) {
            self.notice.send(crate::channels::Notice::Conflict {
                note: flavite_buffer::manager::NoteRef::new(path.clone()),
            });
        }
        let revision = self
            .buffer_manager
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .map(|buffer| buffer.revision())
            .unwrap_or(Revision::INITIAL);
        SessionResult::Success(CommandResult::RecoveryRestored {
            buffer_id,
            revision,
        })
    }

    fn discard_recovery(&self, path: PathBuf) -> SessionResult {
        let Some(writer) = &self.recovery_writer else {
            return SessionResult::Error {
                message: "recovery storage is unavailable".into(),
            };
        };
        let key = native_note_key(&path, &self.identity.root);
        if writer.lock().unwrap().delete_for_note_key(&key) {
            SessionResult::Success(CommandResult::ConflictResolved)
        } else {
            SessionResult::Error {
                message: "recovery snapshot could not be removed".into(),
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

    /// Write recovery files for dirty buffers whose configured interval is
    /// due. GUI and CLI hosts call this from their timer pump; errors remain
    /// best-effort as required by D6, while the dirty buffer remains present.
    pub fn recovery_tick(&self) -> Vec<PathBuf> {
        let Some(writer) = &self.recovery_writer else {
            return Vec::new();
        };
        let manager = self.buffer_manager.lock().unwrap();
        let dirty = manager
            .dirty_buffers()
            .into_iter()
            .map(|(path, buffer)| {
                (
                    native_note_key(path, &self.identity.root),
                    buffer.base_bytes().to_vec(),
                    flavite_buffer::recovery::RecoveryFile::from_buffer(buffer, 0, false),
                )
            })
            .collect::<Vec<_>>();
        drop(manager);
        let mut written = Vec::new();
        for (key, base, snapshot) in dirty {
            let due = writer.lock().unwrap().should_write_snapshot(&snapshot);
            if due
                && self.actor.pin_merge_base(key.clone(), base).is_ok()
                && let Some(path) = writer.lock().unwrap().write_snapshot(&key, snapshot)
            {
                written.push(path);
            }
        }
        written
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

impl Drop for Session {
    fn drop(&mut self) {
        self.recovery_stop.store(true, Ordering::Release);
        if let Some(worker) = self.recovery_worker.take() {
            let _ = worker.join();
        }
    }
}

fn native_note_key(path: &Path, root: &Path) -> Vec<u8> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        relative.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        relative
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        relative.to_string_lossy().as_bytes().to_vec()
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
    use crate::actor::VaultActorBuilder;
    use crate::channels::ChannelSet;
    use flavite_buffer::state::BufferState;
    use flavite_fs::{Fs, MemFs};
    use flavite_index::IndexActor;
    use flavite_syntax::text::{Span, TextByte};
    use std::path::PathBuf;

    fn test_session() -> (Session, MemFs, tempfile::TempDir) {
        let fs = MemFs::new();
        let root = PathBuf::from("/test/vault");
        fs.create_dir_all(&root).unwrap();
        fs.write_new(&root.join("note.md"), b"hello").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root, vec![1, 2, 3]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let channels = ChannelSet::new();
        let session = Session::new(
            identity,
            actor,
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        (session, fs, temp)
    }

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

    #[test]
    fn session_open_edit_undo_redo_and_close_tracks_buffer_state() {
        let (session, _fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let opened = session.execute(SessionCommand::OpenNote { path: path.clone() });
        let (buffer_id, revision) = match opened {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected opened note, got {other:?}"),
        };
        assert!(session.is_open(&path));
        assert_eq!(session.get_buffer_id(&path), Some(buffer_id));
        assert!(matches!(
            session.execute(SessionCommand::OpenNote { path: path.clone() }),
            SessionResult::Success(CommandResult::NoteOpened { .. })
        ));

        let edit = EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".into());
        let edited = session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![edit],
        });
        assert!(matches!(
            edited,
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![],
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Undo { buffer_id }),
            SessionResult::Success(CommandResult::UndoPerformed { .. })
        ));
        assert!(matches!(
            session.execute(SessionCommand::Redo { buffer_id }),
            SessionResult::Success(CommandResult::RedoPerformed { .. })
        ));
        assert!(matches!(
            session.execute(SessionCommand::SaveNote { path: path.clone() }),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(_fs.read_if_exists(&path).unwrap().unwrap(), b"hello world");
        assert!(matches!(
            session.execute(SessionCommand::CloseNote { path: path.clone() }),
            SessionResult::Success(CommandResult::NoteClosed)
        ));
        assert!(!session.is_open(&path));
        assert!(matches!(
            session.execute(SessionCommand::CloseNote { path }),
            SessionResult::Error { .. }
        ));
    }

    #[test]
    fn session_detects_external_save_conflict_and_supports_merge_reload_and_save_as() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let buffer_id = match session.execute(SessionCommand::OpenNote { path: path.clone() }) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected opened note, got {other:?}"),
        };
        let revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![EditOp::new(
                    Span::new(TextByte(5), TextByte(5)),
                    " mine".into()
                )],
            }),
            SessionResult::Success(_)
        ));

        fs.remove(&path).unwrap();
        fs.write_new(&path, b"hello theirs").unwrap();
        let saved = session.execute(SessionCommand::SaveNote { path: path.clone() });
        assert!(matches!(saved, SessionResult::Error { .. }), "{saved:?}");
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::Merge,
            }),
            SessionResult::MergeReady { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::SaveAs {
                    new_path: PathBuf::from("/test/vault/copy.md"),
                },
            }),
            SessionResult::ActionRequired {
                code: "save-as-confirm",
                ..
            }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::Reload,
            }),
            SessionResult::Success(CommandResult::ConflictResolved)
        ));
        assert_eq!(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .get_buffer_by_id(buffer_id)
                .unwrap()
                .model()
                .to_source_bytes(),
            b"hello theirs"
        );
    }

    #[test]
    fn session_restores_then_discards_note_keyed_recovery() {
        let (mut session, _fs, temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let buffer_id = match session.execute(SessionCommand::OpenNote { path: path.clone() }) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected opened note, got {other:?}"),
        };
        session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id_mut(buffer_id)
            .unwrap()
            .apply_local_edit(EditOp::new(
                Span::new(TextByte(5), TextByte(5)),
                " recovered".into(),
            ));

        let recovery_dir = temp.path().join("recovery");
        let mut writer = RecoveryWriter::new(RecoveryConfig::new("test".into(), recovery_dir, 0));
        writer
            .write_buffer(
                b"note.md",
                session
                    .buffer_manager()
                    .lock()
                    .unwrap()
                    .get_buffer_by_id(buffer_id)
                    .unwrap(),
            )
            .unwrap();
        session.recovery_writer = Some(Arc::new(Mutex::new(writer)));

        assert!(matches!(
            session.execute(SessionCommand::RestoreRecovery { path: path.clone() }),
            SessionResult::Success(CommandResult::RecoveryRestored { .. })
        ));
        assert_eq!(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .get_buffer_by_id(buffer_id)
                .unwrap()
                .model()
                .to_source_bytes(),
            b"hello recovered"
        );
        assert!(matches!(
            session.execute(SessionCommand::DiscardRecovery { path: path.clone() }),
            SessionResult::Success(CommandResult::ConflictResolved)
        ));
        assert!(matches!(
            session.execute(SessionCommand::DiscardRecovery { path }),
            SessionResult::Error { .. }
        ));
    }

    #[test]
    fn session_handles_absent_notes_stale_edits_and_empty_undo_redo() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/new.md");
        let buffer_id = match session.execute(SessionCommand::OpenNote { path: path.clone() }) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected opened absent note, got {other:?}"),
        };
        let revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::Undo { buffer_id }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Redo { buffer_id }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision: Revision::new(revision.as_u64() + 1),
                ops: vec![],
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::SaveNote { path: path.clone() }),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(fs.read_if_exists(&path).unwrap().unwrap(), b"");
        assert!(matches!(
            session.execute(SessionCommand::SaveNote {
                path: PathBuf::from("/test/vault/closed.md")
            }),
            SessionResult::Error { .. }
        ));
    }

    #[test]
    fn conflict_resolution_handles_missing_buffers_deleted_files_and_keep_mine() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::KeepMine,
            }),
            SessionResult::Error { .. }
        ));
        let buffer_id = match session.execute(SessionCommand::OpenNote { path: path.clone() }) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected opened note, got {other:?}"),
        };
        fs.remove(&path).unwrap();
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::Reload,
            }),
            SessionResult::ActionRequired {
                code: "file-deleted",
                ..
            }
        ));
        fs.write_new(&path, b"external").unwrap();
        let revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(0), TextByte(0)),
                "mine ".into(),
            )],
        });
        session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id_mut(buffer_id)
            .unwrap()
            .check_external_change(b"theirs");
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::KeepMine,
            }),
            SessionResult::Success(CommandResult::ConflictResolved)
        ));
        assert_eq!(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .get_buffer_by_id(buffer_id)
                .unwrap()
                .state(),
            BufferState::Dirty
        );
    }
}
