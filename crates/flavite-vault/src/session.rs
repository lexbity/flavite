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

use crate::actor::VaultActorHandle;
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
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use thiserror::Error;
use uuid::Uuid;

const MAX_PENDING_SAVE_OPERATIONS: usize = 4096;

#[derive(Debug)]
struct SaveFlight {
    request: SaveRequest,
    response: crossbeam::channel::Receiver<crate::actor::VaultActorResp>,
    queued: Option<SaveRequest>,
}

/// Terminal result for an admitted asynchronous Session operation.
#[derive(Debug, Clone)]
pub struct OperationCompletion {
    /// Request ID returned by the initial Pending result.
    pub request_id: u64,
    /// The operation's terminal result.
    pub result: SessionResult,
}

/// Immutable revisioned buffer data exposed by Session to API hosts.
#[derive(Debug, Clone)]
pub struct BufferSnapshot {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub text: String,
    pub base_hash: Option<[u8; 32]>,
    pub state: String,
    pub path: PathBuf,
}

/// Failure while acquiring an immutable buffer snapshot.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BufferSnapshotError {
    #[error("buffer {0} is not open")]
    NotFound(u64),
    #[error("stale revision: expected {expected}, current {actual}")]
    Stale { expected: u64, actual: u64 },
}

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
    /// Apply a user-reviewed three-way merge to the exact buffer and disk
    /// versions represented by its preview.
    ApplyMerge {
        path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        disk_hash: [u8; 32],
        merged_bytes: Vec<u8>,
    },
    /// Save a note.
    SaveNote { path: PathBuf },
    /// Trigger a reconcile.
    Reconcile {
        scope: ReconcileScope,
        mode: ReconcileMode,
    },
    /// Undo an edit.
    Undo {
        buffer_id: BufferId,
        revision: Revision,
    },
    /// Redo an edit.
    Redo {
        buffer_id: BufferId,
        revision: Revision,
    },
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
    /// Operation was not admitted because the lossless completion ledger is full.
    Busy,
    /// Revision precondition did not match the current authoritative buffer.
    StaleRevision {
        buffer_id: BufferId,
        expected: Revision,
        actual: Revision,
    },
    /// An admitted save was replaced by a later request for the same buffer.
    Superseded { request_id: u64, by_request: u64 },
    /// Command failed.
    Error { message: String },
    /// Buffer is in conflict state.
    Conflict { path: PathBuf },
    /// Three-way merge preview. Conflicts are structured data and no markers
    /// are inserted into note content.
    MergeReady {
        path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        disk_hash: [u8; 32],
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
        /// Buffer instance whose immutable snapshot was saved.
        buffer_id: BufferId,
        /// Revision represented by the saved bytes.
        saved_revision: Revision,
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
    /// Serializes every command transition and worker reply application.
    driving_turn: Mutex<()>,
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
    /// One active save and at most one coalesced successor per buffer.
    save_flights: Mutex<HashMap<u64, SaveFlight>>,
    /// Correlated reconcile workers.
    reconcile_flights:
        Mutex<HashMap<u64, crossbeam::channel::Receiver<crate::actor::VaultActorResp>>>,
    /// Lossless terminal results. Admission is bounded by the pending-save cap.
    completions: Mutex<VecDeque<OperationCompletion>>,
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
            driving_turn: Mutex::new(()),
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
            save_flights: Mutex::new(HashMap::new()),
            reconcile_flights: Mutex::new(HashMap::new()),
            completions: Mutex::new(VecDeque::new()),
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
        let _turn = self.driving_turn.lock().unwrap();
        self.execute_inner(command)
    }

    fn execute_inner(&self, command: SessionCommand) -> SessionResult {
        match command {
            SessionCommand::OpenNote { path } => self.open_note(path),
            SessionCommand::CloseNote { path } => self.close_note(path),
            SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops,
            } => self.apply_edits(buffer_id, revision, ops),
            SessionCommand::ApplyMerge {
                path,
                buffer_id,
                revision,
                disk_hash,
                merged_bytes,
            } => self.apply_merge(path, buffer_id, revision, disk_hash, merged_bytes),
            SessionCommand::SaveNote { path } => self.save_note(path),
            SessionCommand::Reconcile { scope, mode } => self.reconcile(scope, mode),
            SessionCommand::Undo {
                buffer_id,
                revision,
            } => self.undo(buffer_id, revision),
            SessionCommand::Redo {
                buffer_id,
                revision,
            } => self.redo(buffer_id, revision),
            SessionCommand::ResolveConflict { path, resolution } => {
                self.resolve_conflict(path, resolution)
            }
            SessionCommand::RestoreRecovery { path } => self.restore_recovery(path),
            SessionCommand::DiscardRecovery { path } => self.discard_recovery(path),
        }
    }

    /// Return the current indexed note-title and alias snapshot for UI
    /// autocomplete. This is a read-only request to the VaultActor.
    pub fn completion_candidates(
        &self,
    ) -> Result<Vec<flavite_index::types::CompletionCandidate>, String> {
        self.actor.completion_candidates()
    }

    /// Process ready worker replies and return every terminal result that has
    /// not yet been presented to the host. This is lossless: completions stay
    /// queued until the host drains them.
    pub fn drain_operation_completions(&self) -> Vec<OperationCompletion> {
        let _turn = self.driving_turn.lock().unwrap();
        self.pump_save_completions();
        self.pump_reconcile_completions();
        self.completions.lock().unwrap().drain(..).collect()
    }

    /// Drain lossy vault pulses for a host adapter.
    pub fn drain_pulses(&self) -> Vec<crate::channels::Pulse> {
        let mut events = Vec::new();
        while let Ok(event) = self.pulse.try_recv() {
            events.push(event);
        }
        events
    }

    /// Drain lossless, coalesced notices for a host adapter.
    pub fn drain_notices(&self) -> Vec<crate::channels::Notice> {
        let mut events = Vec::new();
        while let Ok(event) = self.notice.try_recv() {
            events.push(event);
        }
        events
    }

    /// Acknowledge notice presentation. This never changes operation state.
    pub fn acknowledge_notice(&self, notice: &crate::channels::Notice) {
        self.notice.ack(notice);
    }

    /// CLI/headless convenience for commands that promise a disk mutation.
    /// GUI hosts use `drain_operation_completions` from their normal pump.
    pub fn wait_for_operation(&self, request_id: u64) -> SessionResult {
        loop {
            let _turn = self.driving_turn.lock().unwrap();
            self.pump_save_completions();
            self.pump_reconcile_completions();
            let completed = {
                let mut completions = self.completions.lock().unwrap();
                completions
                    .iter()
                    .position(|completion| completion.request_id == request_id)
                    .map(|position| completions.remove(position).unwrap().result)
            };
            if let Some(result) = completed {
                return result;
            }
            let active = self.save_flights.lock().unwrap().values().any(|flight| {
                flight.request.request_id == request_id
                    || flight
                        .queued
                        .as_ref()
                        .is_some_and(|request| request.request_id == request_id)
            }) || self
                .reconcile_flights
                .lock()
                .unwrap()
                .contains_key(&request_id);
            if !active {
                return SessionResult::Error {
                    message: format!("unknown or already-drained operation {request_id}"),
                };
            }
            drop(_turn);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn pump_save_completions(&self) {
        let ready = {
            let mut flights = self.save_flights.lock().unwrap();
            let ready_ids = flights
                .iter()
                .filter_map(|(buffer, flight)| match flight.response.try_recv() {
                    Ok(response) => Some((*buffer, response)),
                    Err(crossbeam::channel::TryRecvError::Empty) => None,
                    Err(crossbeam::channel::TryRecvError::Disconnected) => Some((
                        *buffer,
                        crate::actor::VaultActorResp::Error(
                            "save worker disconnected before returning a terminal result".into(),
                        ),
                    )),
                })
                .collect::<Vec<_>>();
            ready_ids
                .into_iter()
                .filter_map(|(buffer_id, response)| {
                    flights
                        .remove(&buffer_id)
                        .map(|flight| (buffer_id, flight, response))
                })
                .collect::<Vec<_>>()
        };

        for (buffer_id, flight, response) in ready {
            let operation_result = self.finish_save(&flight.request, &response);
            self.completions
                .lock()
                .unwrap()
                .push_back(OperationCompletion {
                    request_id: flight.request.request_id,
                    result: operation_result,
                });

            let Some(mut queued) = flight.queued else {
                continue;
            };
            match response {
                crate::actor::VaultActorResp::Saved(Ok(receipt)) => {
                    // The first save established a new disk base. Bind the
                    // queued immutable snapshot to those exact bytes before
                    // admitting its worker request.
                    queued.precondition = DiskPrecondition::Existing(receipt.hash);
                    queued.merge_base = flight.request.bytes.clone();
                    let buffer_is_live = self
                        .buffer_manager
                        .lock()
                        .unwrap()
                        .get_buffer_by_id(BufferId::from(buffer_id))
                        .is_some();
                    if !buffer_is_live {
                        self.completions
                            .lock()
                            .unwrap()
                            .push_back(OperationCompletion {
                                request_id: queued.request_id,
                                result: SessionResult::Error {
                                    message: "queued save's buffer was closed before admission"
                                        .into(),
                                },
                            });
                        continue;
                    }
                    match self.actor.submit_save(queued.clone()) {
                        Ok(response) => {
                            self.save_flights.lock().unwrap().insert(
                                buffer_id,
                                SaveFlight {
                                    request: queued,
                                    response,
                                    queued: None,
                                },
                            );
                        }
                        Err(error) => {
                            self.completions
                                .lock()
                                .unwrap()
                                .push_back(OperationCompletion {
                                    request_id: queued.request_id,
                                    result: SessionResult::Error {
                                        message: error.to_string(),
                                    },
                                })
                        }
                    }
                }
                _ => self
                    .completions
                    .lock()
                    .unwrap()
                    .push_back(OperationCompletion {
                        request_id: queued.request_id,
                        result: SessionResult::Error {
                            message: format!(
                                "queued save was not admitted because save {} did not complete",
                                flight.request.request_id
                            ),
                        },
                    }),
            }
        }
    }

    fn pump_reconcile_completions(&self) {
        let ready = {
            let mut flights = self.reconcile_flights.lock().unwrap();
            let ids = flights
                .iter()
                .filter_map(|(id, receiver)| match receiver.try_recv() {
                    Ok(response) => Some((*id, response)),
                    Err(crossbeam::channel::TryRecvError::Empty) => None,
                    Err(crossbeam::channel::TryRecvError::Disconnected) => Some((
                        *id,
                        crate::actor::VaultActorResp::Error(
                            "reconcile worker disconnected before returning a terminal result"
                                .into(),
                        ),
                    )),
                })
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|(id, response)| flights.remove(&id).map(|_| (id, response)))
                .collect::<Vec<_>>()
        };
        for (request_id, response) in ready {
            let result = match response {
                crate::actor::VaultActorResp::ReconcileComplete { summary, .. } => {
                    SessionResult::Success(CommandResult::ReconcileComplete { summary })
                }
                crate::actor::VaultActorResp::Error(message) => SessionResult::Error { message },
                _ => SessionResult::Error {
                    message: "reconcile worker returned an invalid response".into(),
                },
            };
            self.completions
                .lock()
                .unwrap()
                .push_back(OperationCompletion { request_id, result });
        }
    }

    fn finish_save(
        &self,
        request: &SaveRequest,
        response: &crate::actor::VaultActorResp,
    ) -> SessionResult {
        let path = request.path.clone();
        let buffer_id = BufferId::from(request.buffer_id);
        match response {
            crate::actor::VaultActorResp::Saved(Ok(receipt)) => {
                let revision = Revision::new(request.revision);
                let mut manager = self.buffer_manager.lock().unwrap();
                if let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) {
                    buffer.handle_save_completed_revision(revision, &request.bytes);
                }
                drop(manager);
                if let Some(writer) = &self.recovery_writer {
                    let mut writer = writer.lock().unwrap();
                    writer.retire_covered(buffer_id, revision);
                }
                self.absent_on_open.lock().unwrap().remove(&path);
                SessionResult::Success(CommandResult::NoteSaved {
                    buffer_id,
                    saved_revision: revision,
                    path,
                    hash: receipt.hash.to_vec(),
                    epoch: Epoch::new(receipt.epoch),
                    generation: Generation::new(receipt.generation),
                })
            }
            crate::actor::VaultActorResp::Saved(Err(failure)) => {
                if failure.stage == SaveFailureStage::Compare {
                    if let Some(buffer) = self
                        .buffer_manager
                        .lock()
                        .unwrap()
                        .get_buffer_by_id_mut(buffer_id)
                        && let Some(hash) = failure.current_hash
                    {
                        buffer.mark_conflicted_disk_hash(hash);
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
                    let mut manager = self.buffer_manager.lock().unwrap();
                    if let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) {
                        let saved_hash = *blake3::hash(&request.bytes).as_bytes();
                        if failure.current_hash == Some(saved_hash) {
                            buffer.record_replaced_disk_snapshot(&request.bytes);
                        } else if let Some(hash) = failure.current_hash {
                            buffer.mark_conflicted_disk_hash(hash);
                        } else {
                            // No observed hash means replacement outcome
                            // remains unknown. The old precondition is kept,
                            // so another save cannot pass compare without a
                            // fresh worker observation.
                        }
                    }
                    drop(manager);
                    if failure.current_hash.is_some() {
                        self.absent_on_open.lock().unwrap().remove(&path);
                    }
                }
                if failure.stage == SaveFailureStage::Compare {
                    SessionResult::Conflict { path }
                } else {
                    SessionResult::Error {
                        message: failure.to_string(),
                    }
                }
            }
            crate::actor::VaultActorResp::Error(message) => SessionResult::Error {
                message: message.clone(),
            },
            _ => SessionResult::Error {
                message: "save worker returned an invalid response".into(),
            },
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
            if self
                .save_flights
                .lock()
                .unwrap()
                .contains_key(&_buffer_id.as_u64())
            {
                return SessionResult::ActionRequired {
                    code: "save-pending",
                    data: Some(path.display().to_string()),
                };
            }
            let dirty = self
                .buffer_manager
                .lock()
                .unwrap()
                .get_buffer_by_id(_buffer_id)
                .is_some_and(|buffer| buffer.is_dirty());
            if dirty {
                return SessionResult::ActionRequired {
                    code: "dirty-buffer-close",
                    data: Some(path.display().to_string()),
                };
            }
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
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }

            // Validate the command once and apply all positions as one
            // authoritative Buffer transition/undo unit.
            let Some((new_revision, _)) = buffer.apply_local_edits(&ops) else {
                return SessionResult::Error {
                    message: "edit ranges overlap, are out of bounds, or split UTF-8".into(),
                };
            };

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
        let buffer_id = {
            let open_buffers = self.open_buffers.lock().unwrap();
            open_buffers.get(&path).copied()
        };
        let Some(buffer_id) = buffer_id else {
            return SessionResult::Error {
                message: "Buffer not found".to_string(),
            };
        };
        let (bytes, revision, base_hash, base_bytes, state) = {
            let manager = self.buffer_manager.lock().unwrap();
            let Some(buffer) = manager.get_buffer_by_id(buffer_id) else {
                return SessionResult::Error {
                    message: "Buffer not found".into(),
                };
            };
            (
                buffer.model().to_source_bytes(),
                buffer.revision(),
                buffer.base_hash(),
                buffer.base_bytes().to_vec(),
                buffer.state(),
            )
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
                message: "buffer has no disk base; explicit create resolution is required".into(),
            };
        };
        let request_id = self.next_request_id();
        let note_key = native_note_key(&path, &self.identity.root);
        let request = SaveRequest {
            request_id,
            path,
            note_key,
            bytes,
            merge_base: base_bytes,
            precondition,
            buffer_id: buffer_id.as_u64(),
            revision: revision.as_u64(),
        };

        let mut flights = self.save_flights.lock().unwrap();
        let outstanding = self.completions.lock().unwrap().len()
            + flights
                .values()
                .map(|flight| 1 + usize::from(flight.queued.is_some()))
                .sum::<usize>();
        if outstanding + self.reconcile_flights.lock().unwrap().len() >= MAX_PENDING_SAVE_OPERATIONS
        {
            return SessionResult::Busy;
        }
        if let Some(flight) = flights.get_mut(&buffer_id.as_u64()) {
            if let Some(superseded) = flight.queued.replace(request) {
                self.completions
                    .lock()
                    .unwrap()
                    .push_back(OperationCompletion {
                        request_id: superseded.request_id,
                        result: SessionResult::Superseded {
                            request_id: superseded.request_id,
                            by_request: request_id,
                        },
                    });
            }
            return SessionResult::Pending { request_id };
        }
        match self.actor.submit_save(request.clone()) {
            Ok(response) => {
                flights.insert(
                    buffer_id.as_u64(),
                    SaveFlight {
                        request,
                        response,
                        queued: None,
                    },
                );
                SessionResult::Pending { request_id }
            }
            Err(error) => SessionResult::Error {
                message: error.to_string(),
            },
        }
    }

    /// Trigger a reconcile.
    fn reconcile(&self, scope: ReconcileScope, mode: ReconcileMode) -> SessionResult {
        let request_id = self.next_request_id();
        let flights = self.save_flights.lock().unwrap();
        let outstanding = self.completions.lock().unwrap().len()
            + flights
                .values()
                .map(|flight| 1 + usize::from(flight.queued.is_some()))
                .sum::<usize>()
            + self.reconcile_flights.lock().unwrap().len();
        drop(flights);
        if outstanding >= MAX_PENDING_SAVE_OPERATIONS {
            return SessionResult::Busy;
        }
        match self.actor.submit_reconcile(scope, mode) {
            Ok(response) => {
                self.reconcile_flights
                    .lock()
                    .unwrap()
                    .insert(request_id, response);
                SessionResult::Pending { request_id }
            }
            Err(crate::actor::VaultActorError::Busy) => SessionResult::Busy,
            Err(error) => SessionResult::Error {
                message: error.to_string(),
            },
        }
    }

    /// Undo an edit.
    fn undo(&self, buffer_id: BufferId, revision: Revision) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock().unwrap();

        // Get the buffer and call undo on its model
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            if buffer.revision() != revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }
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
    fn redo(&self, buffer_id: BufferId, revision: Revision) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock().unwrap();

        // Get the buffer and call redo on its model
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            if buffer.revision() != revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }
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
        let (base, mine, revision) = {
            let manager = self.buffer_manager.lock().unwrap();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => (
                    buffer.base_bytes().to_vec(),
                    buffer.model().to_source_bytes(),
                    buffer.revision(),
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
            buffer_id,
            revision,
            disk_hash: *blake3::hash(theirs.as_bytes()).as_bytes(),
            result: crate::merge::three_way_merge(base, mine, theirs),
        }
    }

    fn apply_merge(
        &self,
        path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        expected_disk_hash: [u8; 32],
        merged_bytes: Vec<u8>,
    ) -> SessionResult {
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        if blake3::hash(&theirs).as_bytes() != &expected_disk_hash {
            if let Some(buffer) = self
                .buffer_manager
                .lock()
                .unwrap()
                .get_buffer_by_id_mut(buffer_id)
            {
                buffer.mark_conflicted_disk_hash(*blake3::hash(&theirs).as_bytes());
            }
            return SessionResult::Conflict { path };
        }
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            theirs.clone(),
            crate::history::HistoryReason::Merge,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot merge disk input: {error}"),
            };
        }
        let mine = {
            let manager = self.buffer_manager.lock().unwrap();
            let Some(buffer) = manager.get_buffer_by_id(buffer_id) else {
                return SessionResult::Error {
                    message: "merge preview buffer was closed".into(),
                };
            };
            if buffer.revision() != revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }
            buffer.model().to_source_bytes()
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            mine,
            crate::history::HistoryReason::ConflictResolution,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot merge buffer: {error}"),
            };
        }
        let mut manager = self.buffer_manager.lock().unwrap();
        let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
            return SessionResult::Error {
                message: "merge preview buffer was closed".into(),
            };
        };
        if buffer.revision() != revision {
            return SessionResult::StaleRevision {
                buffer_id,
                expected: revision,
                actual: buffer.revision(),
            };
        }
        match buffer.apply_resolved_merge(&merged_bytes, &theirs) {
            Some(new_revision) => SessionResult::Success(CommandResult::EditsApplied {
                buffer_id,
                new_revision,
            }),
            None => SessionResult::Error {
                message: "merge output is invalid UTF-8 or exceeds the note size limit".into(),
            },
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

    /// Get an immutable snapshot of an open buffer.
    pub fn buffer_snapshot(
        &self,
        buffer_id: BufferId,
        revision: Option<Revision>,
    ) -> Result<BufferSnapshot, BufferSnapshotError> {
        let _turn = self.driving_turn.lock().unwrap();
        let manager = self.buffer_manager.lock().unwrap();
        let buffer = manager
            .get_buffer_by_id(buffer_id)
            .ok_or(BufferSnapshotError::NotFound(buffer_id.as_u64()))?;
        if let Some(expected) = revision
            && buffer.revision() != expected
        {
            return Err(BufferSnapshotError::Stale {
                expected: expected.as_u64(),
                actual: buffer.revision().as_u64(),
            });
        }
        let path = manager
            .path_for_id(buffer_id)
            .cloned()
            .ok_or(BufferSnapshotError::NotFound(buffer_id.as_u64()))?;
        Ok(BufferSnapshot {
            buffer_id,
            revision: buffer.revision(),
            text: buffer.text().to_owned(),
            base_hash: buffer.base_hash().map(|hash| hash.as_bytes()),
            state: buffer.state().to_string(),
            path,
        })
    }

    /// Internal access for Session-owned integrations and in-crate tests.
    #[must_use]
    pub(crate) fn buffer_manager(&self) -> Arc<Mutex<BufferManager>> {
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

    /// Force a durable checkpoint for every dirty buffer before a host exits.
    /// Returns failures rather than claiming that the timer interval alone
    /// protects edits made shortly before shutdown.
    pub fn flush_recovery(&self) -> Result<Vec<PathBuf>, String> {
        let snapshots = {
            let manager = self.buffer_manager.lock().unwrap();
            manager
                .dirty_buffers()
                .into_iter()
                .map(|(path, buffer)| {
                    (
                        native_note_key(path, &self.identity.root),
                        buffer.base_bytes().to_vec(),
                        flavite_buffer::recovery::RecoveryFile::from_buffer(buffer, 0, false),
                    )
                })
                .collect::<Vec<_>>()
        };
        if snapshots.is_empty() {
            return Ok(Vec::new());
        }
        let Some(writer) = &self.recovery_writer else {
            return Err("local recovery storage is unavailable".into());
        };
        let mut written = Vec::with_capacity(snapshots.len());
        for (key, base, snapshot) in snapshots {
            self.actor
                .pin_merge_base(key.clone(), base)
                .map_err(|error| format!("could not persist recovery merge base: {error}"))?;
            writer
                .lock()
                .unwrap()
                .write_snapshot_now(&key, snapshot)
                .ok_or_else(|| {
                    "could not durably write a dirty-buffer recovery snapshot".to_owned()
                })
                .map(|path| written.push(path))?;
        }
        Ok(written)
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
        if let Err(error) = self.flush_recovery() {
            tracing::error!(error = %error, "failed to flush dirty buffers during session shutdown");
        }
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

    fn save_to_terminal(session: &Session, path: PathBuf) -> SessionResult {
        match session.execute(SessionCommand::SaveNote { path }) {
            SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
            result => result,
        }
    }

    #[test]
    fn test_session_builder() {
        let identity = VaultIdentity::new(
            uuid::Uuid::now_v7(),
            PathBuf::from("/test/vault"),
            vec![1, 2, 3, 4],
        );
        let fs = MemFs::new();
        fs.create_dir_all(&identity.root).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("builder.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let channels = ChannelSet::new();
        let session = SessionBuilder::new()
            .with_identity(identity)
            .with_actor(actor)
            .with_buffer_manager(BufferManager::new())
            .with_pulse(channels.pulse)
            .with_notice(channels.notice)
            .with_config(VaultConfig::default())
            .build()
            .unwrap();
        assert!(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .all_buffers()
                .is_empty()
        );
    }

    #[test]
    fn test_request_counter() {
        let (session, _, _) = test_session();
        assert_eq!(session.next_request_id(), 0);
        assert_eq!(session.next_request_id(), 1);
    }

    #[test]
    fn reconcile_and_save_completions_keep_request_identity_and_revision() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let (buffer_id, revision) =
            match session.execute(SessionCommand::OpenNote { path: path.clone() }) {
                SessionResult::Success(CommandResult::NoteOpened {
                    buffer_id,
                    revision,
                }) => (buffer_id, revision),
                other => panic!("expected opened note, got {other:?}"),
            };
        let first_revision = match session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(5), TextByte(5)),
                " one".into(),
            )],
        }) {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("expected first edit, got {other:?}"),
        };

        let first_save = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("expected first save to be admitted, got {other:?}"),
        };
        let second_revision = match session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision: first_revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(9), TextByte(9)),
                " two".into(),
            )],
        }) {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("expected second edit, got {other:?}"),
        };
        let superseded = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("expected queued save to be admitted, got {other:?}"),
        };
        let final_revision = match session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision: second_revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(13), TextByte(13)),
                " three".into(),
            )],
        }) {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("expected third edit, got {other:?}"),
        };
        let final_save = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("expected coalesced save to be admitted, got {other:?}"),
        };

        assert!(matches!(
            session.wait_for_operation(first_save),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert!(matches!(
            session.wait_for_operation(superseded),
            SessionResult::Superseded {
                request_id,
                by_request
            } if request_id == superseded && by_request == final_save
        ));
        assert!(matches!(
            session.wait_for_operation(final_save),
            SessionResult::Success(CommandResult::NoteSaved { saved_revision, .. })
                if saved_revision == final_revision
        ));
        assert_eq!(
            fs.read_if_exists(&path).unwrap().unwrap(),
            b"hello one two three"
        );

        let reconcile = match session.execute(SessionCommand::Reconcile {
            scope: ReconcileScope::Full,
            mode: ReconcileMode::VerifyContent,
        }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("expected reconcile to be admitted, got {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(reconcile),
            SessionResult::Success(CommandResult::ReconcileComplete { .. })
        ));
        assert!(session.drain_operation_completions().is_empty());
        assert!(matches!(
            session.wait_for_operation(reconcile),
            SessionResult::Error { .. }
        ));
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
            SessionResult::StaleRevision { .. }
        ));
        let edited_revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::Undo {
                buffer_id,
                revision,
            }),
            SessionResult::StaleRevision { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Undo {
                buffer_id,
                revision: edited_revision,
            }),
            SessionResult::Success(CommandResult::UndoPerformed { .. })
        ));
        let undone_revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision: edited_revision,
            }),
            SessionResult::StaleRevision { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision: undone_revision,
            }),
            SessionResult::Success(CommandResult::RedoPerformed { .. })
        ));
        let request_id = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("save must be admitted asynchronously: {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(request_id),
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
    fn session_rejects_invalid_lifecycle_operations_and_reports_unavailable_recovery() {
        let (session, _fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let missing = PathBuf::from("/test/vault/missing.md");
        let never_opened = PathBuf::from("/test/vault/never-opened.md");

        assert!(matches!(
            session.execute(SessionCommand::OpenNote {
                path: missing.clone()
            }),
            SessionResult::Success(CommandResult::NoteOpened { .. })
        ));
        assert!(matches!(
            session.execute(SessionCommand::CloseNote {
                path: never_opened.clone()
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::SaveNote { path: never_opened }),
            SessionResult::Error { .. }
        ));

        let (buffer_id, revision) =
            match session.execute(SessionCommand::OpenNote { path: path.clone() }) {
                SessionResult::Success(CommandResult::NoteOpened {
                    buffer_id,
                    revision,
                }) => (buffer_id, revision),
                other => panic!("unexpected open result: {other:?}"),
            };
        assert!(matches!(
            session.execute(SessionCommand::OpenNote { path: path.clone() }),
            SessionResult::Success(CommandResult::NoteOpened { buffer_id: existing, .. }) if existing == buffer_id
        ));

        let bad_edit = session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(99), TextByte(99)),
                "x".into(),
            )],
        });
        assert!(matches!(bad_edit, SessionResult::Error { .. }));
        assert!(matches!(
            session.execute(SessionCommand::Undo {
                buffer_id,
                revision
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision
            }),
            SessionResult::Error { .. }
        ));

        let changed = session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(Span::new(TextByte(5), TextByte(5)), "!".into())],
        });
        let dirty_revision = match changed {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("unexpected edit result: {other:?}"),
        };
        assert!(matches!(
            session.execute(SessionCommand::CloseNote { path: path.clone() }),
            SessionResult::ActionRequired {
                code: "dirty-buffer-close",
                ..
            }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![],
            }),
            SessionResult::StaleRevision { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::SaveAs {
                    new_path: PathBuf::from("copy.md")
                }
            }),
            SessionResult::ActionRequired {
                code: "save-as-confirm",
                ..
            }
        ));
        assert!(matches!(
            session.execute(SessionCommand::RestoreRecovery { path: path.clone() }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::DiscardRecovery { path }),
            SessionResult::Error { .. }
        ));
        assert!(session.recovery_tick().is_empty());
        assert!(session.flush_recovery().is_err());
        assert!(matches!(
            session.wait_for_operation(u64::MAX),
            SessionResult::Error { .. }
        ));
        assert!(dirty_revision > revision);
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
        let saved = save_to_terminal(&session, path.clone());
        assert!(matches!(saved, SessionResult::Conflict { .. }), "{saved:?}");
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
            session.execute(SessionCommand::Undo {
                buffer_id,
                revision
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision: Revision::new(revision.as_u64() + 1),
                ops: vec![],
            }),
            SessionResult::StaleRevision { .. }
        ));
        let request_id = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("save must be admitted asynchronously: {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(request_id),
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

    #[test]
    fn save_command_is_pending_until_disk_and_index_complete() {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("/vault/notes")).unwrap();
        fs.write_new(Path::new("/vault/notes/a.md"), b"# Base")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db_path).unwrap());
        let index = IndexActor::new(&db_path, 1).unwrap();
        let identity =
            VaultIdentity::new(Uuid::now_v7(), PathBuf::from("/vault"), vec![1, 2, 3, 4]);
        let actor = crate::actor::VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(index)
            .build()
            .unwrap()
            .start()
            .unwrap();
        let session = Session::new(
            identity,
            actor.clone(),
            BufferManager::default(),
            PulseChannel::new(8),
            NoticeChannel::new(Duration::from_millis(10)),
            VaultConfig::default(),
        );
        let path = PathBuf::from("/vault/notes/a.md");
        let (buffer_id, revision) =
            match session.execute(SessionCommand::OpenNote { path: path.clone() }) {
                SessionResult::Success(CommandResult::NoteOpened {
                    buffer_id,
                    revision,
                }) => (buffer_id, revision),
                other => panic!("unexpected open result: {other:?}"),
            };
        let applied = session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(6), TextByte(6)),
                " edited".into(),
            )],
        });
        let new_revision = match applied {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("unexpected edit result: {other:?}"),
        };
        assert!(new_revision > revision);
        assert!(matches!(
            session.execute(SessionCommand::CloseNote { path: path.clone() }),
            SessionResult::ActionRequired {
                code: "dirty-buffer-close",
                ..
            }
        ));
        let request_id = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("save must be admitted asynchronously: {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(request_id),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(
            actor.read_file(path.clone()).unwrap().unwrap(),
            b"# Base edited"
        );
        let current_revision = session
            .buffer_manager
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision: current_revision,
                ops: vec![EditOp::new(
                    Span::new(TextByte(13), TextByte(13)),
                    " mine".into(),
                )],
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        fs.remove(&path).unwrap();
        fs.write_new(&path, b"# External").unwrap();
        let (merge_revision, merge_disk_hash) = match session.prepare_merge(path.clone()) {
            SessionResult::MergeReady {
                buffer_id: preview_buffer,
                revision,
                disk_hash,
                result,
                ..
            } => {
                assert_eq!(preview_buffer, buffer_id);
                assert!(result.merged.is_none());
                (revision, disk_hash)
            }
            other => panic!("unexpected merge preview: {other:?}"),
        };
        assert!(matches!(
            session.execute(SessionCommand::ApplyMerge {
                path: path.clone(),
                buffer_id,
                revision: merge_revision,
                disk_hash: merge_disk_hash,
                merged_bytes: b"# Merged".to_vec(),
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        let merged_base = session
            .buffer_manager
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .base_bytes()
            .to_vec();
        assert_eq!(merged_base, b"# External");
        let merge_save_id = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("merged buffer save must be pending: {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(merge_save_id),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(actor.read_file(path.clone()).unwrap().unwrap(), b"# Merged");
        assert!(matches!(
            session.execute(SessionCommand::CloseNote { path }),
            SessionResult::Success(CommandResult::NoteClosed)
        ));
        assert!(session.drain_operation_completions().is_empty());
    }
}
