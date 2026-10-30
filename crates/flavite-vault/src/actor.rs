//! VaultActor — the main filesystem actor (§4.2, D3).
//!
//! The VaultActor owns the filesystem implementation, structural index,
//! identity, and content-addressed history store.
//!
//! It serializes vault commands and coordinates with the Session via bounded
//! channels.

use crate::channels::{Pulse, RenameTier};
use crate::coalesce::{EventCoalescer, VaultWatcher, WatcherEvent};
use crate::history::HistoryStore;
use crate::history::{HistoryReason, SnapshotRef};
use crate::rename::RenamePlanner;
use crate::save::{SaveFailure, SaveReceipt, SaveRequest};
use crate::types::{
    Epoch, Generation, ReconcileMode, ReconcileScope, ReconcileSummary, RenameStrategy,
    VaultIdentity, Version,
};
use crossbeam::channel::{Receiver, Sender, bounded, select, tick, unbounded};
use flavite_fs::Fs;
use flavite_index::{IndexActor, MAX_NOTE_BYTES};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use thiserror::Error;
use tracing::info;

/// Injectable watcher seam. Tests can retain the sender and feed events without
/// relying on OS notification behavior.
pub type EventSource = Receiver<WatcherEvent>;
pub fn event_source_channel() -> (Sender<WatcherEvent>, EventSource) {
    bounded(1024)
}

/// Lossless external disk observation forwarded to the owning Session.
#[derive(Debug, Clone)]
pub enum VaultActorEvent {
    ExternalChange {
        path: std::path::PathBuf,
        bytes: Vec<u8>,
    },
    Deleted {
        path: std::path::PathBuf,
    },
    /// A T0–T2 rename whose buffer identity should follow the destination.
    Renamed {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        tier: RenameTier,
    },
    /// The watcher disconnected; the Session converts this to a coalesced notice.
    WatcherLost,
    /// A T1 link-repair suggestion for lossless presentation by the Session.
    RenameSuggested {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        tier: RenameTier,
        inbound: u32,
    },
    /// A rename intentionally left inbound links pointing at the previous stem.
    LinksBroken {
        from: std::path::PathBuf,
        count: u32,
    },
}

/// Correlated request for Session-owned buffer mutation during WAIT-BUFFERS.
#[derive(Debug)]
pub struct ApplyBufferPatchRequest {
    pub txn_id: String,
    pub item_id: u64,
    pub kind: crate::journal::TransactionItemKind,
    pub buffer_id: u64,
    pub expected_revision: u64,
    pub old_stem: String,
    pub new_stem: String,
    pub target_key: String,
    pub indexed_hash: [u8; 32],
    pub recovery_key: Option<Vec<u8>>,
    pub new_base_bytes: Vec<u8>,
    pub reply: Sender<Result<u64, String>>,
}

/// Messages for the VaultActor.
#[derive(Debug, Clone)]
pub enum VaultActorMsg {
    /// Trigger a reconcile with the given scope and mode.
    Reconcile {
        scope: ReconcileScope,
        mode: ReconcileMode,
    },
    /// Reconcile with a correlated terminal reply for Session operation tracking.
    ReconcileRequest {
        scope: ReconcileScope,
        mode: ReconcileMode,
        reply: Sender<VaultActorResp>,
    },
    /// Execute a journaled rename, correlating open-buffer patches through Session.
    RenameNoteRequest {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        strategy: RenameStrategy,
        buffer_ids: HashMap<std::path::PathBuf, u64>,
        buffer_revisions: HashMap<u64, u64>,
        reply: Sender<VaultActorResp>,
    },
    /// Durably cancel a journal item after a tagged buffer undo.
    CancelJournalItem {
        txn_id: String,
        item_id: u64,
        reply: Sender<VaultActorResp>,
    },
    /// Notify of a filesystem event.
    FsEvent(WatcherEvent),
    /// The owning window gained focus; verify disk state after throttling.
    Focus,
    /// Increment the persisted autosave failure counter.
    AutosaveFailure,
    /// Get the current version.
    GetVersion,
    /// Get vault identity.
    GetIdentity,
    /// Shutdown.
    Shutdown,
    /// Read note bytes for opening/reloading a buffer.
    ReadFile {
        path: std::path::PathBuf,
        reply: Sender<VaultActorResp>,
    },
    /// Read the indexed title/alias snapshot for editor autocomplete.
    CompletionCandidates { reply: Sender<VaultActorResp> },
    /// Read a bounded native-identity note catalog for workspace shells.
    NoteCatalog {
        limit: usize,
        reply: Sender<VaultActorResp>,
    },
    /// Resolve one link through the index's canonical resolver.
    ResolveTarget {
        target_key: String,
        raw_target: String,
        is_embed: bool,
        source_path_key: Option<String>,
        reply: Sender<VaultActorResp>,
    },
    /// Execute full-text or hybrid search through the index owner.
    SearchNotes {
        query: flavite_index::query::Query,
        limit: usize,
        reply: Sender<VaultActorResp>,
    },
    /// Save immutable serialized bytes through the D6 path.
    Save {
        request: SaveRequest,
        reply: Sender<VaultActorResp>,
    },
    /// Persist bytes observed during an explicit merge/conflict resolution.
    SnapshotHistory {
        note_key: Vec<u8>,
        bytes: Vec<u8>,
        reason: HistoryReason,
        reply: Sender<VaultActorResp>,
    },
    /// Persist and pin a merge base before a dirty checkpoint or save.
    PinMergeBase {
        note_key: Vec<u8>,
        bytes: Vec<u8>,
        reply: Sender<VaultActorResp>,
    },
    /// Release a superseded merge-base pin after a durable save.
    UnpinMergeBase {
        note_key: Vec<u8>,
        hash: String,
        reply: Sender<VaultActorResp>,
    },
}

/// Responses from the VaultActor.
#[derive(Debug, Clone)]
pub enum VaultActorResp {
    /// Reconcile completed.
    ReconcileComplete {
        summary: ReconcileSummary,
        version: Version,
    },
    /// A queued reconcile was replaced by a newer request.
    ReconcileSuperseded {
        by_request: u64,
    },
    /// Durable journal rename and link patches completed.
    Renamed(Result<String, String>),
    /// Current version.
    Version(Version),
    /// Vault identity.
    Identity(VaultIdentity),
    /// Acknowledgment.
    Ack,
    /// Error.
    Error(String),
    /// File bytes, or `None` when absent.
    File(Option<Vec<u8>>),
    /// Indexed note completion candidates.
    CompletionCandidates(Result<Vec<flavite_index::types::CompletionCandidate>, String>),
    NoteCatalog(Result<flavite_index::NoteCatalogPage, String>),
    /// Canonical link resolution verdict.
    ResolveTarget(Result<flavite_index::resolver::Resolution, String>),
    /// Search results from the structural index and Tantivy.
    SearchNotes(Result<flavite_index::SearchPage, String>),
    /// Save completion after the structural index callback.
    Saved(Result<SaveReceipt, SaveFailure>),
    /// Durable history snapshot record.
    HistorySnapshot(Result<SnapshotRef, String>),
}

/// Errors from the VaultActor.
#[derive(Debug, Error)]
pub enum VaultActorError {
    #[error("Channel disconnected")]
    ChannelDisconnected,
    #[error("Actor already running")]
    AlreadyRunning,
    #[error("Actor not running")]
    NotRunning,
    #[error("Actor is at operation capacity")]
    Busy,
    #[error("Initial reconcile did not finish before the startup deadline")]
    StartupTimeout,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Handle to communicate with the VaultActor.
#[derive(Debug, Clone)]
pub struct VaultActorHandle {
    sender: Sender<VaultActorMsg>,
    receiver: Receiver<VaultActorResp>,
    events: Receiver<VaultActorEvent>,
    buffer_patches: Receiver<ApplyBufferPatchRequest>,
    drift_found_total: Arc<std::sync::atomic::AtomicU64>,
    autosave_failures_total: Arc<std::sync::atomic::AtomicU64>,
    rename_items_skipped_total: Arc<std::sync::atomic::AtomicU64>,
    startup_ready: Receiver<()>,
}

impl VaultActorHandle {
    /// Wait until the initial worker scan has been applied by the actor.
    pub fn wait_until_ready(&self, timeout: std::time::Duration) -> Result<(), VaultActorError> {
        self.startup_ready
            .recv_timeout(timeout)
            .map_err(|error| match error {
                crossbeam::channel::RecvTimeoutError::Timeout => VaultActorError::StartupTimeout,
                crossbeam::channel::RecvTimeoutError::Disconnected => {
                    VaultActorError::ChannelDisconnected
                }
            })
    }

    /// Get the lossless external-change stream for Session processing.
    #[must_use]
    pub fn events(&self) -> Receiver<VaultActorEvent> {
        self.events.clone()
    }

    /// Receive buffer-patch work that the Session must apply on its owner thread.
    #[must_use]
    pub fn buffer_patch_requests(&self) -> Receiver<ApplyBufferPatchRequest> {
        self.buffer_patches.clone()
    }

    /// Request a throttled full verification after the host window gains focus.
    pub fn focus(&self) -> Result<(), VaultActorError> {
        self.try_send(VaultActorMsg::Focus)
    }

    /// Record an autosave failure for persisted doctor diagnostics.
    pub fn record_autosave_failure(&self) -> Result<(), VaultActorError> {
        self.try_send(VaultActorMsg::AutosaveFailure)
    }

    /// Number of indexed disk drifts found by reconciliation.
    #[must_use]
    pub fn drift_found_total(&self) -> u64 {
        self.drift_found_total.load(Ordering::Relaxed)
    }

    /// Cumulative journal items skipped during rename execution.
    #[must_use]
    pub fn rename_items_skipped_total(&self) -> u64 {
        self.rename_items_skipped_total.load(Ordering::Relaxed)
    }

    /// Cumulative autosave failures persisted by the actor.
    #[must_use]
    pub fn autosave_failures_total(&self) -> u64 {
        self.autosave_failures_total.load(Ordering::Relaxed)
    }

    /// Send a message and wait for a response.
    pub fn send(&self, msg: VaultActorMsg) -> Result<VaultActorResp, VaultActorError> {
        self.sender
            .send(msg)
            .map_err(|_| VaultActorError::ChannelDisconnected)?;
        self.receiver
            .recv()
            .map_err(|_| VaultActorError::ChannelDisconnected)
    }

    /// Try to send a message without waiting.
    pub fn try_send(&self, msg: VaultActorMsg) -> Result<(), VaultActorError> {
        self.sender.try_send(msg).map_err(|error| match error {
            crossbeam::channel::TrySendError::Full(_) => VaultActorError::Busy,
            crossbeam::channel::TrySendError::Disconnected(_) => {
                VaultActorError::ChannelDisconnected
            }
        })
    }

    /// Admit a reconcile and return its lossless one-shot completion receiver.
    pub fn submit_reconcile(
        &self,
        scope: ReconcileScope,
        mode: ReconcileMode,
    ) -> Result<Receiver<VaultActorResp>, VaultActorError> {
        let (reply, receiver) = crossbeam::channel::bounded(1);
        self.try_send(VaultActorMsg::ReconcileRequest { scope, mode, reply })?;
        Ok(receiver)
    }

    /// Admit a journaled rename and return its correlated completion receiver.
    pub fn submit_rename(
        &self,
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        strategy: RenameStrategy,
        buffer_ids: HashMap<std::path::PathBuf, u64>,
        buffer_revisions: HashMap<u64, u64>,
    ) -> Result<Receiver<VaultActorResp>, VaultActorError> {
        let (reply, receiver) = crossbeam::channel::bounded(1);
        self.try_send(VaultActorMsg::RenameNoteRequest {
            from,
            to,
            strategy,
            buffer_ids,
            buffer_revisions,
            reply,
        })?;
        Ok(receiver)
    }

    /// Durably cancel a committed journal item before acknowledging its undo.
    pub fn cancel_journal_item(&self, txn_id: String, item_id: u64) -> Result<(), String> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::CancelJournalItem {
                txn_id,
                item_id,
                reply,
            })
            .map_err(|error| error.to_string())?;
        match response.recv().map_err(|error| error.to_string())? {
            VaultActorResp::Ack => Ok(()),
            VaultActorResp::Error(error) => Err(error),
            _ => Err("unexpected journal cancellation response".into()),
        }
    }

    /// Get the current version.
    pub fn get_version(&self) -> Result<Version, VaultActorError> {
        match self.send(VaultActorMsg::GetVersion)? {
            VaultActorResp::Version(v) => Ok(v),
            VaultActorResp::Error(_e) => Err(VaultActorError::NotRunning),
            _ => Err(VaultActorError::NotRunning),
        }
    }

    /// Send a correlated read request and receive only its reply.
    pub fn read_file(&self, path: std::path::PathBuf) -> Result<Option<Vec<u8>>, VaultActorError> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::ReadFile { path, reply })
            .map_err(|_| VaultActorError::ChannelDisconnected)?;
        match response
            .recv()
            .map_err(|_| VaultActorError::ChannelDisconnected)?
        {
            VaultActorResp::File(bytes) => Ok(bytes),
            VaultActorResp::Error(_) => Err(VaultActorError::NotRunning),
            _ => Err(VaultActorError::NotRunning),
        }
    }

    /// Read a bounded native-identity note catalog page.
    pub fn note_catalog(&self, limit: usize) -> Result<flavite_index::NoteCatalogPage, String> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::NoteCatalog { limit, reply })
            .map_err(|error| error.to_string())?;
        match response.recv().map_err(|error| error.to_string())? {
            VaultActorResp::NoteCatalog(result) => result,
            VaultActorResp::Error(error) => Err(error),
            _ => Err("unexpected note-catalog response".to_owned()),
        }
    }

    /// Read indexed note title and alias candidates.
    pub fn completion_candidates(
        &self,
    ) -> Result<Vec<flavite_index::types::CompletionCandidate>, String> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::CompletionCandidates { reply })
            .map_err(|error| error.to_string())?;
        match response.recv().map_err(|error| error.to_string())? {
            VaultActorResp::CompletionCandidates(result) => result,
            VaultActorResp::Error(error) => Err(error),
            _ => Err("unexpected completion-candidate response".to_owned()),
        }
    }

    /// Resolve one link using the index's canonical ranking and target rules.
    pub fn resolve_target(
        &self,
        target_key: String,
        raw_target: String,
        is_embed: bool,
        source_path_key: Option<String>,
    ) -> Result<flavite_index::resolver::Resolution, String> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::ResolveTarget {
                target_key,
                raw_target,
                is_embed,
                source_path_key,
                reply,
            })
            .map_err(|error| error.to_string())?;
        match response.recv().map_err(|error| error.to_string())? {
            VaultActorResp::ResolveTarget(result) => result,
            VaultActorResp::Error(error) => Err(error),
            _ => Err("unexpected resolver response".to_owned()),
        }
    }

    /// Execute a text or hybrid search on the actor that owns Tantivy.
    pub fn search_notes(
        &self,
        query: flavite_index::query::Query,
        limit: usize,
    ) -> Result<flavite_index::SearchPage, String> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::SearchNotes {
                query,
                limit,
                reply,
            })
            .map_err(|error| error.to_string())?;
        match response.recv().map_err(|error| error.to_string())? {
            VaultActorResp::SearchNotes(result) => result,
            VaultActorResp::Error(error) => Err(error),
            _ => Err("unexpected search response".to_owned()),
        }
    }

    /// Admit a save without waiting for disk, history, or index work.
    pub fn submit_save(
        &self,
        request: SaveRequest,
    ) -> Result<Receiver<VaultActorResp>, VaultActorError> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .try_send(VaultActorMsg::Save { request, reply })
            .map_err(|error| match error {
                crossbeam::channel::TrySendError::Full(_) => VaultActorError::Busy,
                crossbeam::channel::TrySendError::Disconnected(_) => {
                    VaultActorError::ChannelDisconnected
                }
            })?;
        Ok(response)
    }

    /// Snapshot observed bytes to local safety history before a destructive
    /// conflict action.
    pub fn snapshot_history(
        &self,
        note_key: Vec<u8>,
        bytes: Vec<u8>,
        reason: HistoryReason,
    ) -> Result<SnapshotRef, VaultActorError> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::SnapshotHistory {
                note_key,
                bytes,
                reason,
                reply,
            })
            .map_err(|_| VaultActorError::ChannelDisconnected)?;
        match response
            .recv()
            .map_err(|_| VaultActorError::ChannelDisconnected)?
        {
            VaultActorResp::HistorySnapshot(Ok(record)) => Ok(record),
            VaultActorResp::HistorySnapshot(Err(_)) | VaultActorResp::Error(_) => {
                Err(VaultActorError::NotRunning)
            }
            _ => Err(VaultActorError::NotRunning),
        }
    }

    /// Ensure the exact note merge base is durably stored and pinned.
    pub fn pin_merge_base(
        &self,
        note_key: Vec<u8>,
        bytes: Vec<u8>,
    ) -> Result<SnapshotRef, VaultActorError> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::PinMergeBase {
                note_key,
                bytes,
                reply,
            })
            .map_err(|_| VaultActorError::ChannelDisconnected)?;
        match response
            .recv()
            .map_err(|_| VaultActorError::ChannelDisconnected)?
        {
            VaultActorResp::HistorySnapshot(Ok(record)) => Ok(record),
            VaultActorResp::HistorySnapshot(Err(_)) | VaultActorResp::Error(_) => {
                Err(VaultActorError::NotRunning)
            }
            _ => Err(VaultActorError::NotRunning),
        }
    }

    /// Release the prior merge-base pin once a successor is durable.
    pub fn unpin_merge_base(&self, note_key: Vec<u8>, hash: String) -> Result<(), VaultActorError> {
        let (reply, response) = crossbeam::channel::bounded(1);
        self.sender
            .send(VaultActorMsg::UnpinMergeBase {
                note_key,
                hash,
                reply,
            })
            .map_err(|_| VaultActorError::ChannelDisconnected)?;
        match response
            .recv()
            .map_err(|_| VaultActorError::ChannelDisconnected)?
        {
            VaultActorResp::Ack => Ok(()),
            _ => Err(VaultActorError::NotRunning),
        }
    }
}

/// The VaultActor itself.
pub struct VaultActor {
    /// The filesystem.
    fs: Arc<dyn Fs + Send + Sync>,
    /// Vault identity.
    identity: VaultIdentity,
    /// The index.
    index: IndexActor,
    /// Content-addressed local safety history.
    history: Option<HistoryStore>,
    /// Current index version reported to session clients.
    epoch: Epoch,
    generation: Generation,
    /// Whether the actor is running.
    is_running: AtomicBool,
    /// Message receiver.
    msg_receiver: Receiver<VaultActorMsg>,
    /// Response sender.
    resp_sender: Sender<VaultActorResp>,
    /// Lossless path observations for the owning Session.
    event_sender: Sender<VaultActorEvent>,
    /// Session-owned buffer patches required to complete committed journal items.
    buffer_patch_sender: Sender<ApplyBufferPatchRequest>,
    drift_found_total: Arc<std::sync::atomic::AtomicU64>,
    autosave_failures_total: Arc<std::sync::atomic::AtomicU64>,
    rename_items_skipped_total: Arc<std::sync::atomic::AtomicU64>,
    event_source: Option<EventSource>,
    pulse_sender: Option<Sender<Pulse>>,
    coalescer: EventCoalescer,
    watcher_enabled: bool,
    watcher_connected: bool,
    initial_scan_complete: bool,
    verification_interval: std::time::Duration,
    recent_watcher_rename_pairs:
        std::collections::HashSet<(std::path::PathBuf, std::path::PathBuf)>,
    reconcile_jobs: Option<Sender<Option<crate::reconcile_worker::ReconcileJob>>>,
    reconcile_queue: Option<Receiver<Option<crate::reconcile_worker::ReconcileJob>>>,
    reconcile_messages: Option<Receiver<crate::reconcile_worker::WorkerMessage>>,
    reconcile_contexts: HashMap<u64, ReconcileContext>,
    next_reconcile_id: u64,
    save_seq: u64,
    save_paths: HashMap<PathBuf, u64>,
    reconcile_result_delay: std::time::Duration,
    reconcile_scan_signal: Option<Sender<()>>,
    watcher_start_failure: Option<String>,
    startup_ready_sender: Option<Sender<()>>,
}

struct ReconcileContext {
    job: crate::reconcile_worker::ReconcileJob,
    initial_paths: std::collections::HashSet<Vec<u8>>,
    initial_attachments: std::collections::HashSet<Vec<u8>>,
    old_notes: Vec<flavite_index::types::Note>,
    inbound_counts: HashMap<PathBuf, u32>,
    summary: ReconcileSummary,
    deleted_events: Vec<PathBuf>,
}

impl VaultActor {
    fn record_drift(&self) {
        let total = self.drift_found_total.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(drift_found_total = total, "reconcile drift found");
    }

    fn persist_runtime_meta(&mut self) {
        let watcher = if !self.watcher_enabled {
            "disabled"
        } else if self.watcher_connected {
            "true"
        } else {
            "false"
        };
        let values = [
            (
                flavite_index::schema::MetaKey::WatcherConnected,
                watcher.to_owned(),
            ),
            (
                flavite_index::schema::MetaKey::DriftFoundTotal,
                self.drift_found_total.load(Ordering::Relaxed).to_string(),
            ),
            (
                flavite_index::schema::MetaKey::AutosaveFailuresTotal,
                self.autosave_failures_total
                    .load(Ordering::Relaxed)
                    .to_string(),
            ),
            (
                flavite_index::schema::MetaKey::RenameItemsSkippedTotal,
                self.rename_items_skipped_total
                    .load(Ordering::Relaxed)
                    .to_string(),
            ),
        ];
        for (key, value) in values {
            if let Err(error) = self.index.set_runtime_meta(key, &value) {
                tracing::warn!(key = key.as_str(), %error, "failed to persist runtime meta");
            }
        }
    }

    /// Create a new VaultActor.
    pub fn new(fs: Box<dyn Fs + Send + Sync>, identity: VaultIdentity, index: IndexActor) -> Self {
        let fs: Arc<dyn Fs + Send + Sync> = Arc::from(fs);
        Self {
            fs,
            identity,
            index,
            history: None,
            epoch: Epoch::new(0),
            generation: Generation::new(0),
            is_running: AtomicBool::new(false),
            msg_receiver: unbounded().1, // Will be replaced
            resp_sender: unbounded().0,  // Will be replaced
            event_sender: unbounded().0,
            buffer_patch_sender: bounded(1).0,
            drift_found_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            autosave_failures_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            rename_items_skipped_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            event_source: None,
            pulse_sender: None,
            coalescer: EventCoalescer::with_defaults(),
            watcher_enabled: true,
            watcher_connected: false,
            initial_scan_complete: false,
            verification_interval: std::time::Duration::from_secs(600),
            recent_watcher_rename_pairs: std::collections::HashSet::new(),
            reconcile_jobs: None,
            reconcile_queue: None,
            reconcile_messages: None,
            reconcile_contexts: HashMap::new(),
            next_reconcile_id: 0,
            save_seq: 0,
            save_paths: HashMap::new(),
            reconcile_result_delay: std::time::Duration::ZERO,
            reconcile_scan_signal: None,
            watcher_start_failure: None,
            startup_ready_sender: None,
        }
    }

    /// Start the actor in a background thread.
    pub fn start(mut self) -> Result<VaultActorHandle, VaultActorError> {
        if self.is_running.load(Ordering::SeqCst) {
            return Err(VaultActorError::AlreadyRunning);
        }

        let (msg_sender, msg_receiver) = unbounded();
        let (resp_sender, resp_receiver) = unbounded();
        let (event_sender, event_receiver) = unbounded();
        let (buffer_patch_sender, buffer_patch_receiver) = bounded(64);
        let (startup_ready_sender, startup_ready) = bounded(1);
        let drift_found_total = Arc::clone(&self.drift_found_total);
        let autosave_failures_total = Arc::clone(&self.autosave_failures_total);
        let rename_items_skipped_total = Arc::clone(&self.rename_items_skipped_total);

        self.msg_receiver = msg_receiver;
        self.resp_sender = resp_sender;
        self.event_sender = event_sender;
        self.buffer_patch_sender = buffer_patch_sender;
        self.startup_ready_sender = Some(startup_ready_sender);

        let is_running = Arc::new(AtomicBool::new(false));

        self.is_running.store(true, Ordering::SeqCst);

        let is_running_for_spawn = is_running.clone();
        thread::Builder::new()
            .name("vault-actor".to_string())
            .spawn(move || {
                self.run(is_running_for_spawn);
            })?;

        Ok(VaultActorHandle {
            sender: msg_sender,
            receiver: resp_receiver,
            events: event_receiver,
            buffer_patches: buffer_patch_receiver,
            drift_found_total,
            autosave_failures_total,
            rename_items_skipped_total,
            startup_ready,
        })
    }

    /// The main actor loop.
    fn run(&mut self, is_running: Arc<AtomicBool>) {
        is_running.store(true, Ordering::SeqCst);
        for (key, counter) in [
            (
                flavite_index::schema::MetaKey::DriftFoundTotal,
                &self.drift_found_total,
            ),
            (
                flavite_index::schema::MetaKey::AutosaveFailuresTotal,
                &self.autosave_failures_total,
            ),
            (
                flavite_index::schema::MetaKey::RenameItemsSkippedTotal,
                &self.rename_items_skipped_total,
            ),
        ] {
            match flavite_index::schema::get_meta_u64(self.index.writer().conn(), key) {
                Ok(Some(value)) => counter.store(value, Ordering::Relaxed),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(key = key.as_str(), %error, "failed to load runtime counter")
                }
            }
        }

        let (jobs_tx, jobs_rx) = bounded::<Option<crate::reconcile_worker::ReconcileJob>>(1);
        let worker_jobs_rx = jobs_rx.clone();
        let (worker_tx, worker_rx) = bounded::<crate::reconcile_worker::WorkerMessage>(4);
        let worker_fs = Arc::clone(&self.fs);
        let worker_root = self.identity.root.clone();
        let result_delay = self.reconcile_result_delay;
        let scan_signal = self.reconcile_scan_signal.clone();
        let worker = thread::Builder::new()
            .name("vault-reconcile".into())
            .spawn(move || {
                while let Ok(Some(job)) = worker_jobs_rx.recv() {
                    let result =
                        crate::reconcile_worker::scan(&*worker_fs, &worker_root, &job, &worker_tx);
                    if let Some(signal) = &scan_signal {
                        let _ = signal.send(());
                    }
                    if !result_delay.is_zero() {
                        thread::sleep(result_delay);
                    }
                    if worker_tx
                        .send(crate::reconcile_worker::WorkerMessage::Complete {
                            job_id: job.id,
                            result,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            })
            .expect("reconcile worker thread should start");
        self.reconcile_jobs = Some(jobs_tx);
        self.reconcile_queue = Some(jobs_rx);
        self.reconcile_messages = Some(worker_rx);

        info!(
            "VaultActor started for vault: {}",
            self.identity.root.display()
        );

        // Start the watcher before scanning, then reconcile once. Events which
        // arrive during the scan remain queued and are replayed afterwards.
        let mut fs_watcher = None;
        let mut event_receiver = if let Some(reason) = self.watcher_start_failure.take() {
            self.watcher_lost(&reason);
            crossbeam::channel::never()
        } else if let Some(receiver) = self.event_source.take() {
            self.watcher_connected = true;
            receiver
        } else if self.watcher_enabled && self.identity.root.is_dir() {
            match flavite_fs::watch::FsWatcher::with_defaults() {
                Ok(mut watcher) => {
                    if let Err(error) = watcher.watch(&self.identity.root) {
                        self.watcher_lost(&error.to_string());
                        crossbeam::channel::never()
                    } else {
                        let std_receiver = watcher.start();
                        let (sender, receiver) = bounded(1024);
                        let bridge = sender.clone();
                        let bridge_result = thread::Builder::new()
                            .name("vault-watch-bridge".into())
                            .spawn(move || {
                                loop {
                                    let result = std_receiver.lock().unwrap().recv();
                                    let Ok(event) = result else {
                                        break;
                                    };
                                    let translated = if event.is_rename && event.paths.len() >= 2 {
                                        WatcherEvent::Renamed {
                                            from: event.paths[0].clone(),
                                            to: event.paths[1].clone(),
                                        }
                                    } else if let Some(path) = event.paths.first() {
                                        match event.change_kind() {
                                            flavite_fs::watch::FsChangeKind::Created => {
                                                WatcherEvent::Created(path.clone())
                                            }
                                            flavite_fs::watch::FsChangeKind::Deleted => {
                                                WatcherEvent::Deleted(path.clone())
                                            }
                                            flavite_fs::watch::FsChangeKind::Modified => {
                                                WatcherEvent::Modified(path.clone())
                                            }
                                        }
                                    } else {
                                        continue;
                                    };
                                    let _ = bridge.send(translated);
                                }
                            });
                        match bridge_result {
                            Ok(_) => {
                                fs_watcher = Some(watcher);
                                self.watcher_connected = true;
                                receiver
                            }
                            Err(error) => {
                                watcher.shutdown();
                                self.watcher_lost(&error.to_string());
                                crossbeam::channel::never()
                            }
                        }
                    }
                }
                Err(error) => {
                    self.watcher_lost(&error.to_string());
                    crossbeam::channel::never()
                }
            }
        } else {
            self.watcher_connected = false;
            crossbeam::channel::never()
        };
        self.persist_runtime_meta();
        let watcher = VaultWatcher::new(&self.coalescer);
        self.perform_initial_scan();
        let timer = tick(std::time::Duration::from_millis(50));
        let mut last_verification = std::time::Instant::now();
        let mut last_focus: Option<std::time::Instant> = None;
        // Main loop multiplexes commands, injected/OS events, and timer work.
        loop {
            select! {
                recv(self.msg_receiver) -> message => match message {
                    Ok(VaultActorMsg::Shutdown) => {
                        let _ = self.resp_sender.try_send(VaultActorResp::Ack);
                        break;
                    }
                    Ok(VaultActorMsg::Focus) => {
                        if last_focus.is_none_or(|last| last.elapsed() >= std::time::Duration::from_secs(2)) {
                            self.handle_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent, None);
                            last_focus = Some(std::time::Instant::now());
                        }
                    }
                    Ok(msg) => {
                        self.handle_message(msg);
                        if let Err(error) = self.index.commit_search_if_due(false) {
                            tracing::error!(%error, "deferred Tantivy commit failed");
                        }
                    }
                    Err(_) => { info!("VaultActor channel disconnected, shutting down"); break; }
                },
                recv(event_receiver) -> event => match event {
                    Ok(event) => watcher.notify(event),
                    Err(_) => {
                        // A disconnected watcher must not remain permanently selectable.
                        event_receiver = crossbeam::channel::never();
                        self.watcher_lost("watcher event stream disconnected");
                    }
                },
                recv(timer) -> _ => {
                    if let Some(batch) = self.coalescer.try_recv_batch() {
                        for (from, to) in batch.rename_pairs.iter().cloned() {
                            self.handle_external_rename_pair(from, to);
                        }
                        if batch.is_bulk || batch.paths.is_empty() {
                            self.handle_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent, None);
                        } else {
                            self.enqueue_reconcile(
                                ReconcileScope::Full,
                                ReconcileMode::VerifyContent,
                                None,
                                batch.paths,
                            );
                        }
                    }
                    if let Err(error) = self.index.commit_search_if_due(false) {
                        tracing::error!(%error, "deferred Tantivy commit failed");
                    }
                    if last_verification.elapsed() >= self.verification_interval {
                        self.handle_reconcile(ReconcileScope::Full, ReconcileMode::FastMetadata, None);
                        last_verification = std::time::Instant::now();
                    }
                }
                recv(self.reconcile_messages.as_ref().expect("worker initialized")) -> message => {
                    if let Ok(message) = message { self.handle_worker_message(message); }
                }
            }
        }

        drop(self.reconcile_messages.take());
        if let Some(sender) = self.reconcile_jobs.take() {
            let _ = sender.send(None);
        }
        if let Err(error) = worker.join() {
            tracing::error!(?error, "reconcile worker panicked during shutdown");
        }

        if let Some(watcher) = fs_watcher.as_mut() {
            watcher.shutdown();
        }
        if let Err(error) = self.index.commit_search_if_due(true) {
            tracing::error!(%error, "final Tantivy commit failed");
        }

        is_running.store(false, Ordering::SeqCst);
        info!("VaultActor stopped");
    }

    /// Handle a message.
    fn handle_message(&mut self, msg: VaultActorMsg) {
        match msg {
            VaultActorMsg::Reconcile { scope, mode } => {
                self.handle_reconcile(scope, mode, None);
            }
            VaultActorMsg::ReconcileRequest { scope, mode, reply } => {
                self.handle_reconcile(scope, mode, Some(reply));
            }
            VaultActorMsg::RenameNoteRequest {
                from,
                to,
                strategy,
                buffer_ids,
                buffer_revisions,
                reply,
            } => {
                let result = self.execute_journaled_rename(
                    &from,
                    &to,
                    strategy,
                    &buffer_ids,
                    &buffer_revisions,
                );
                let _ = reply.try_send(VaultActorResp::Renamed(result));
            }
            VaultActorMsg::CancelJournalItem {
                txn_id,
                item_id,
                reply,
            } => {
                let result = crate::journal::JournalExecutor::new(&*self.fs, &self.identity.root)
                    .cancel_item(&txn_id, item_id);
                let response = result
                    .map(|()| VaultActorResp::Ack)
                    .unwrap_or_else(VaultActorResp::Error);
                let _ = reply.send(response);
            }
            VaultActorMsg::FsEvent(event) => {
                self.handle_fs_event(event);
            }
            VaultActorMsg::Focus => {}
            VaultActorMsg::AutosaveFailure => {
                let total = self
                    .autosave_failures_total
                    .fetch_add(1, Ordering::Relaxed)
                    .saturating_add(1);
                tracing::warn!(autosave_failures_total = total, "autosave failure recorded");
                self.persist_runtime_meta();
            }
            VaultActorMsg::GetVersion => {
                let version = Version::new(self.epoch, self.generation);
                let _ = self.resp_sender.try_send(VaultActorResp::Version(version));
            }
            VaultActorMsg::GetIdentity => {
                let _ = self
                    .resp_sender
                    .try_send(VaultActorResp::Identity(self.identity.clone()));
            }
            VaultActorMsg::Shutdown => {
                if let Err(error) = self.index.commit_search_if_due(true) {
                    tracing::error!(%error, "shutdown Tantivy commit failed");
                }
                let _ = self.resp_sender.try_send(VaultActorResp::Ack);
                // Shutdown will happen when the channel is dropped
            }
            VaultActorMsg::ReadFile { path, reply } => {
                let response = self
                    .fs
                    .read_if_exists(&path)
                    .map(VaultActorResp::File)
                    .unwrap_or_else(|e| VaultActorResp::Error(e.to_string()));
                let _ = reply.try_send(response);
            }
            VaultActorMsg::CompletionCandidates { reply } => {
                let response =
                    VaultActorResp::CompletionCandidates(self.index.completion_candidates());
                let _ = reply.try_send(response);
            }
            VaultActorMsg::NoteCatalog { limit, reply } => {
                let response = VaultActorResp::NoteCatalog(self.index.note_catalog(limit));
                let _ = reply.try_send(response);
            }
            VaultActorMsg::ResolveTarget {
                target_key,
                raw_target,
                is_embed,
                source_path_key,
                reply,
            } => {
                let response = VaultActorResp::ResolveTarget(self.index.resolve_target(
                    &target_key,
                    &raw_target,
                    is_embed,
                    source_path_key.as_deref(),
                ));
                let _ = reply.try_send(response);
            }
            VaultActorMsg::SearchNotes {
                query,
                limit,
                reply,
            } => {
                let response = VaultActorResp::SearchNotes(self.index.search_notes(&query, limit));
                let _ = reply.try_send(response);
            }
            VaultActorMsg::Save { request, reply } => {
                self.save_seq = self.save_seq.saturating_add(1);
                self.save_paths.insert(request.path.clone(), self.save_seq);
                self.handle_save(request, reply);
            }
            VaultActorMsg::SnapshotHistory {
                note_key,
                bytes,
                reason,
                reply,
            } => {
                let result = self.history_store().and_then(|history| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    history
                        .snapshot(&note_key, &bytes, reason, false, now)
                        .map_err(|error| error.to_string())
                });
                let _ = reply.try_send(VaultActorResp::HistorySnapshot(result));
            }
            VaultActorMsg::PinMergeBase {
                note_key,
                bytes,
                reply,
            } => {
                let result = self.history_store().and_then(|history| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    history
                        .pin_merge_base(&note_key, &bytes, now)
                        .map_err(|error| error.to_string())
                });
                let _ = reply.try_send(VaultActorResp::HistorySnapshot(result));
            }
            VaultActorMsg::UnpinMergeBase {
                note_key,
                hash,
                reply,
            } => {
                let result = self.history_store().and_then(|history| {
                    history
                        .unpin_merge_base(&note_key, &hash)
                        .map_err(|error| error.to_string())
                });
                let response = result
                    .map(|()| VaultActorResp::Ack)
                    .unwrap_or_else(VaultActorResp::Error);
                let _ = reply.try_send(response);
            }
        }
    }

    fn handle_save(&mut self, request: SaveRequest, reply: Sender<VaultActorResp>) {
        let failure_context = request.clone();
        let result: Result<SaveReceipt, SaveFailure> = (|| {
            if self.history.is_none() {
                let data = crate::identity::get_data_dir()
                    .ok_or_else(|| SaveFailure::from_message("OS data directory is unavailable"))?;
                let root = data
                    .join("flavite")
                    .join("vaults")
                    .join(self.identity.uuid.to_string())
                    .join("history");
                self.history =
                    Some(HistoryStore::open(self.fs.clone(), root).map_err(SaveFailure::from)?);
            }
            let history = self.history.as_mut().expect("history initialized");
            let old_base_hash = blake3::hash(&request.merge_base).to_hex().to_string();
            history
                .pin_merge_base(
                    &request.note_key,
                    &request.merge_base,
                    crate::history::now_secs(),
                )
                .map_err(SaveFailure::from)?;
            let index = &mut self.index;
            let display_path = request
                .path
                .strip_prefix(&self.identity.root)
                .map_err(SaveFailure::from_message)?
                .to_string_lossy()
                .replace('\\', "/");
            let scan_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(i64::MAX as u128) as i64;
            let result = crate::save::save(
                &*self.fs,
                history,
                request.clone(),
                |_path, bytes, mtime_ns| {
                    index.apply_saved_note(
                        &request.note_key,
                        &display_path,
                        bytes,
                        mtime_ns,
                        scan_ns,
                    )
                },
            )?;
            tracing::info!(path = %display_path, revision = ?request.revision, "save completed");
            let new_base_hash = blake3::hash(&request.bytes).to_hex().to_string();
            // The durable replacement and structural index application are
            // the save milestone. A failure to rotate the merge-base pin after
            // that point must not turn a completed save into a retryable
            // failure (which could overwrite newer external bytes). Keep the
            // prior base pinned unless the successor pin is durable.
            if history
                .pin_merge_base(
                    &request.note_key,
                    &request.bytes,
                    crate::history::now_secs(),
                )
                .is_ok()
                && old_base_hash != new_base_hash
            {
                // A failed unpin is safe: it leaks a pin until maintenance,
                // whereas reporting save failure here could trigger a
                // destructive retry after the replacement already succeeded.
                let _ = history.unpin_merge_base(&request.note_key, &old_base_hash);
            }
            Ok(result)
        })()
        .map_err(|failure: SaveFailure| failure.for_request(&failure_context));
        let _ = reply.try_send(VaultActorResp::Saved(result));
    }

    fn history_store(&mut self) -> Result<&mut HistoryStore, String> {
        if self.history.is_none() {
            let data = crate::identity::get_data_dir()
                .ok_or_else(|| "OS data directory is unavailable".to_owned())?;
            let root = data
                .join("flavite")
                .join("vaults")
                .join(self.identity.uuid.to_string())
                .join("history");
            self.history =
                Some(HistoryStore::open(self.fs.clone(), root).map_err(|error| error.to_string())?);
        }
        Ok(self.history.as_mut().expect("history initialized"))
    }

    /// Handle a reconcile request.
    fn handle_reconcile(
        &mut self,
        scope: ReconcileScope,
        mode: ReconcileMode,
        reply: Option<Sender<VaultActorResp>>,
    ) {
        self.enqueue_reconcile(scope, mode, reply, Default::default());
    }

    fn enqueue_reconcile(
        &mut self,
        scope: ReconcileScope,
        mode: ReconcileMode,
        reply: Option<Sender<VaultActorResp>>,
        nominated: std::collections::HashSet<PathBuf>,
    ) {
        self.next_reconcile_id = self.next_reconcile_id.saturating_add(1);
        let id = self.next_reconcile_id;
        let job = crate::reconcile_worker::ReconcileJob {
            id,
            scope,
            mode,
            watermark: self.save_seq,
            nominated,
            reply,
        };
        let initial_paths = self.index.indexed_note_paths().unwrap_or_default();
        let initial_attachments = self.index.indexed_attachment_paths().unwrap_or_default();
        let old_notes = self.index.rename_note_snapshot().unwrap_or_default();
        let inbound_counts = old_notes
            .iter()
            .map(|note| {
                let count = self
                    .index
                    .writer()
                    .conn()
                    .query_row(
                        "SELECT COUNT(*) FROM links WHERE dst_note=?1",
                        [note.note_id],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap_or(0)
                    .max(0) as u32;
                (PathBuf::from(&note.path), count)
            })
            .collect();
        let context = ReconcileContext {
            job: job.clone(),
            initial_paths: initial_paths.into_iter().collect(),
            initial_attachments: initial_attachments.into_iter().collect(),
            old_notes,
            inbound_counts,
            summary: ReconcileSummary::default(),
            deleted_events: Vec::new(),
        };
        self.reconcile_contexts.insert(id, context);
        let Some(sender) = self.reconcile_jobs.as_ref() else {
            self.reconcile_contexts.remove(&id);
            if let Some(reply) = job.reply {
                let _ = reply.send(VaultActorResp::Error(
                    "reconcile worker is unavailable".into(),
                ));
            }
            return;
        };
        match sender.try_send(Some(job)) {
            Ok(()) => {}
            Err(crossbeam::channel::TrySendError::Full(Some(job))) => {
                if let Some(queue) = self.reconcile_queue.as_ref()
                    && let Ok(Some(superseded)) = queue.try_recv()
                {
                    self.reconcile_contexts.remove(&superseded.id);
                    if let Some(reply) = superseded.reply {
                        let _ = reply.send(VaultActorResp::ReconcileSuperseded { by_request: id });
                    }
                }
                if let Err(error) =
                    sender.send_timeout(Some(job), std::time::Duration::from_millis(1))
                {
                    let job = error.into_inner().expect("reconcile job retained");
                    self.reconcile_contexts.remove(&id);
                    if let Some(reply) = job.reply {
                        let _ = reply.send(VaultActorResp::Error(
                            "reconcile worker queue is busy".into(),
                        ));
                    }
                }
            }
            Err(crossbeam::channel::TrySendError::Disconnected(Some(job))) => {
                self.reconcile_contexts.remove(&id);
                if let Some(reply) = job.reply {
                    let _ = reply.send(VaultActorResp::Error(
                        "reconcile worker disconnected".into(),
                    ));
                }
            }
            Err(_) => {}
        }
    }

    fn handle_worker_message(&mut self, message: crate::reconcile_worker::WorkerMessage) {
        match message {
            crate::reconcile_worker::WorkerMessage::Batch { job_id, outcomes } => {
                if let Some(mut context) = self.reconcile_contexts.remove(&job_id) {
                    let result = crate::reconcile_worker::ScanResult {
                        outcomes,
                        ..crate::reconcile_worker::ScanResult::default()
                    };
                    let _ = self.apply_scan_result(&mut context, result);
                    self.reconcile_contexts.insert(job_id, context);
                }
            }
            crate::reconcile_worker::WorkerMessage::Progress(progress) => {
                if self.reconcile_contexts.contains_key(&progress.job_id)
                    && let Some(pulse) = &self.pulse_sender
                {
                    let _ = pulse.try_send(Pulse::ReconcileProgress {
                        done: progress.done,
                        total: progress.total,
                    });
                }
            }
            crate::reconcile_worker::WorkerMessage::Complete { job_id, result } => {
                let Some(mut context) = self.reconcile_contexts.remove(&job_id) else {
                    return;
                };
                let summary = self.apply_scan_result(&mut context, result);
                if !self.initial_scan_complete && context.job.scope == ReconcileScope::Full {
                    self.initial_scan_complete = true;
                    if let Some(ready) = self.startup_ready_sender.take() {
                        let _ = ready.send(());
                    }
                    if self.index.needs_reparse()
                        && let Err(error) = self.index.mark_parser_current()
                    {
                        tracing::warn!(%error, "could not persist current parser version after verification");
                    }
                }
                self.persist_runtime_meta();
                self.publish_reconcile(summary, context.job.reply);
                self.prune_save_watermarks();
            }
        }
    }

    fn apply_scan_result(
        &mut self,
        context: &mut ReconcileContext,
        result: crate::reconcile_worker::ScanResult,
    ) -> ReconcileSummary {
        let scan_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .min(i64::MAX as u128) as i64;
        let mut summary = ReconcileSummary::default();
        let mut present_notes = std::collections::HashSet::new();
        let mut present_attachments = std::collections::HashSet::new();
        let scanned_notes = result.present_notes.clone();
        let scanned_attachments = result.present_attachments.clone();
        let full_scan = result.full_scan;
        let complete_scan = result.complete && !result.incomplete;
        for outcome in result.outcomes {
            let path = match &outcome {
                crate::reconcile_worker::ScanOutcome::Note { path, .. }
                | crate::reconcile_worker::ScanOutcome::Attachment { path, .. }
                | crate::reconcile_worker::ScanOutcome::Missing(path) => path.clone(),
            };
            if path_observation_is_stale(
                self.save_paths.get(&path).copied().unwrap_or(0),
                context.job.watermark,
            ) {
                continue;
            }
            match outcome {
                crate::reconcile_worker::ScanOutcome::Missing(path) => {
                    let Ok(relative) = path.strip_prefix(&self.identity.root) else {
                        continue;
                    };
                    let Ok(key) = self.fs.encode_path_key(relative) else {
                        continue;
                    };
                    if self.index.remove_note_path(&key).unwrap_or(false) {
                        summary.removed = summary.removed.saturating_add(1);
                        self.record_drift();
                        context.deleted_events.push(path.clone());
                    }
                    if self.index.remove_attachment_path(&key).unwrap_or(false) {
                        summary.removed = summary.removed.saturating_add(1);
                        self.record_drift();
                    }
                }
                crate::reconcile_worker::ScanOutcome::Note {
                    path,
                    path_key,
                    display_path,
                    size,
                    mtime_ns,
                    bytes,
                    parsed,
                } => {
                    present_notes.insert(path_key.clone());
                    let prior = context.initial_paths.contains(&path_key);
                    let previous_generation = self.generation.0;
                    let changed = if size > MAX_NOTE_BYTES as u64 {
                        self.index
                            .apply_unparsed_note(&path_key, &display_path, size, mtime_ns, scan_ns)
                            .map(|(epoch, generation)| {
                                self.epoch = Epoch::new(epoch);
                                self.generation = Generation::new(generation);
                                generation > previous_generation
                            })
                            .map_err(|error| error.to_string())
                    } else if let Some(bytes) = bytes {
                        if context.job.mode == ReconcileMode::FastMetadata
                            && self
                                .index
                                .note_metadata_matches(&path_key, size, mtime_ns, scan_ns)
                                .unwrap_or(false)
                        {
                            continue;
                        }
                        let applied = if let Some(parsed) = parsed {
                            self.index.apply_parsed_note(
                                &path_key,
                                &display_path,
                                &bytes,
                                &parsed,
                                mtime_ns,
                                scan_ns,
                            )
                        } else {
                            self.index.apply_degraded_saved_note(
                                &path_key,
                                &display_path,
                                &bytes,
                                mtime_ns,
                                scan_ns,
                            )
                        };
                        match applied {
                            Ok((epoch, generation)) => {
                                let changed = generation > previous_generation;
                                if changed && (prior || self.initial_scan_complete) {
                                    let _ =
                                        self.event_sender.send(VaultActorEvent::ExternalChange {
                                            path: path.clone(),
                                            bytes,
                                        });
                                }
                                self.epoch = Epoch::new(epoch);
                                self.generation = Generation::new(generation);
                                Ok(changed)
                            }
                            Err(error) => Err(error.to_string()),
                        }
                    } else {
                        continue;
                    };
                    match changed {
                        Ok(true) if prior => {
                            summary.changed = summary.changed.saturating_add(1);
                            self.record_drift();
                        }
                        Ok(true) => {
                            summary.added = summary.added.saturating_add(1);
                        }
                        Ok(false) => {}
                        Err(error) => {
                            tracing::warn!(path=%path.display(), %error, "note reconcile failed")
                        }
                    }
                }
                crate::reconcile_worker::ScanOutcome::Attachment {
                    path,
                    path_key,
                    display_path,
                    size,
                    mtime_ns,
                    placeholder,
                    hash,
                    bytes,
                } => {
                    present_attachments.insert(path_key.clone());
                    if context.job.mode == ReconcileMode::FastMetadata
                        && self
                            .index
                            .attachment_matches(
                                &path_key,
                                size,
                                mtime_ns,
                                None,
                                !placeholder,
                                scan_ns,
                            )
                            .unwrap_or(false)
                    {
                        continue;
                    }
                    let hydrated = !placeholder
                        && (size > flavite_index::types::ATTACHMENT_HASH_MAX as u64
                            || hash.is_some());
                    if self
                        .index
                        .attachment_matches(&path_key, size, mtime_ns, hash, hydrated, scan_ns)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                    match self.index.apply_saved_attachment(
                        &path_key,
                        &display_path,
                        bytes.as_deref(),
                        size,
                        mtime_ns,
                        scan_ns,
                        hydrated,
                    ) {
                        Ok((_, generation)) => {
                            let prior = context.initial_attachments.contains(&path_key);
                            self.generation = Generation::new(generation);
                            if prior {
                                summary.changed = summary.changed.saturating_add(1);
                                self.record_drift();
                            } else {
                                summary.added = summary.added.saturating_add(1);
                            }
                        }
                        Err(error) => {
                            tracing::warn!(path=%path.display(), %error, "attachment reconcile failed")
                        }
                    }
                }
            }
        }
        if full_scan && complete_scan {
            for key in context
                .initial_paths
                .iter()
                .filter(|key| !scanned_notes.contains(*key) && !present_notes.contains(*key))
            {
                if self.saved_after_watermark(key, context.job.watermark) {
                    continue;
                }
                if let Some(path) = self.remove_note_and_path_event(key, &mut summary) {
                    context.deleted_events.push(path);
                }
            }
            for key in context.initial_attachments.iter().filter(|key| {
                !scanned_attachments.contains(*key) && !present_attachments.contains(*key)
            }) {
                if self.saved_after_watermark(key, context.job.watermark) {
                    continue;
                }
                if self.index.remove_attachment_path(key).unwrap_or(false) {
                    summary.removed = summary.removed.saturating_add(1);
                    self.record_drift();
                }
            }
            let t2 = self.infer_note_renames(
                &context.old_notes,
                &context.initial_paths,
                &scanned_notes,
                &context.inbound_counts,
            );
            for path in std::mem::take(&mut context.deleted_events) {
                if !t2.contains(&path) {
                    let _ = self.event_sender.send(VaultActorEvent::Deleted { path });
                }
            }
        }
        if result.complete && !full_scan {
            for path in std::mem::take(&mut context.deleted_events) {
                let _ = self.event_sender.send(VaultActorEvent::Deleted { path });
            }
        }
        context.summary.added = context.summary.added.saturating_add(summary.added);
        context.summary.changed = context.summary.changed.saturating_add(summary.changed);
        context.summary.removed = context.summary.removed.saturating_add(summary.removed);
        if result.complete {
            std::mem::take(&mut context.summary)
        } else {
            ReconcileSummary::default()
        }
    }

    fn remove_note_and_path_event(
        &mut self,
        key: &[u8],
        summary: &mut ReconcileSummary,
    ) -> Option<PathBuf> {
        if !self.index.remove_note_path(key).unwrap_or(false) {
            return None;
        }
        summary.removed = summary.removed.saturating_add(1);
        self.record_drift();
        self.fs
            .decode_path_key(key)
            .ok()
            .map(|relative| self.identity.root.join(relative))
    }

    fn saved_after_watermark(&self, key: &[u8], watermark: u64) -> bool {
        self.fs.decode_path_key(key).ok().is_some_and(|relative| {
            self.save_paths
                .get(&self.identity.root.join(relative))
                .is_some_and(|seq| path_observation_is_stale(*seq, watermark))
        })
    }

    fn prune_save_watermarks(&mut self) {
        if self.reconcile_contexts.is_empty() {
            self.save_paths.clear();
            return;
        }
        let oldest = self
            .reconcile_contexts
            .values()
            .map(|context| context.job.watermark)
            .min()
            .unwrap_or(self.save_seq);
        self.save_paths.retain(|_, seq| *seq > oldest);
    }

    fn watcher_lost(&mut self, reason: &str) {
        self.watcher_connected = false;
        self.record_drift();
        tracing::warn!(%reason, "vault watcher unavailable; scheduling full verification");
        let _ = self.event_sender.send(VaultActorEvent::WatcherLost);
        self.persist_runtime_meta();
        self.handle_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent, None);
    }

    fn publish_reconcile(
        &mut self,
        summary: ReconcileSummary,
        reply: Option<Sender<VaultActorResp>>,
    ) {
        let version = Version::new(self.epoch, self.generation);
        if let Some(pulse) = &self.pulse_sender {
            let _ = pulse.try_send(Pulse::VaultChanged {
                epoch: self.epoch,
                generation: self.generation,
                summary,
            });
        }

        if let Err(error) = self.index.commit_search_if_due(true) {
            tracing::error!(%error, "post-reconcile Tantivy commit failed");
        }
        let response = VaultActorResp::ReconcileComplete { summary, version };
        if let Some(reply) = reply {
            let _ = reply.send(response);
        }
    }

    /// Handle a filesystem event.
    fn handle_fs_event(&mut self, event: WatcherEvent) {
        VaultWatcher::new(&self.coalescer).notify(event);
    }

    fn handle_external_rename_pair(&mut self, from: std::path::PathBuf, to: std::path::PathBuf) {
        let from = if from.is_absolute() {
            from
        } else {
            self.identity.root.join(from)
        };
        let to = if to.is_absolute() {
            to
        } else {
            self.identity.root.join(to)
        };
        if !from.starts_with(&self.identity.root)
            || !to.starts_with(&self.identity.root)
            || self.fs.exists(&from).unwrap_or(true)
            || !self.fs.exists(&to).unwrap_or(false)
        {
            return;
        }
        let note_rename = from
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            && to
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        let attachment_rename = attachment_kind(&from).is_some() && attachment_kind(&to).is_some();
        if !note_rename && !attachment_rename {
            return;
        }
        self.recent_watcher_rename_pairs
            .insert((from.clone(), to.clone()));
        let stem = from
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default()
            .to_lowercase();
        let filename = from
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_lowercase();
        let inbound = if note_rename {
            self.index.writer().conn().query_row("SELECT COUNT(*) FROM links WHERE target_key=?1", [stem], |row| row.get::<_, i64>(0)).unwrap_or(0)
        } else {
            self.index.writer().conn().query_row("SELECT COUNT(*) FROM links WHERE kind='embed' AND (target_key=?1 OR target_key=?2)", (&stem, &filename), |row| row.get::<_, i64>(0)).unwrap_or(0)
        }.max(0) as u32;
        if note_rename {
            let _ = self.event_sender.send(VaultActorEvent::Renamed {
                from: from.clone(),
                to: to.clone(),
                tier: RenameTier::T1,
            });
        }
        let _ = self.event_sender.send(VaultActorEvent::RenameSuggested {
            from,
            to,
            tier: RenameTier::T1,
            inbound,
        });
    }

    /// Plan inbound-link replacements from the current index, verify their
    /// source hashes, and execute the note rename through the durable journal.
    fn execute_journaled_rename(
        &mut self,
        from: &std::path::Path,
        to: &std::path::Path,
        strategy: RenameStrategy,
        buffer_ids: &HashMap<std::path::PathBuf, u64>,
        buffer_revisions: &HashMap<u64, u64>,
    ) -> Result<String, String> {
        let plan = RenamePlanner::new(
            Arc::clone(&self.fs),
            self.identity.root.clone(),
            256 * 1024 * 1024,
        )
        .plan_rename(
            from.to_path_buf(),
            to.to_path_buf(),
            buffer_ids,
            buffer_revisions,
        )
        .into_plan()?;
        let plan_item = plan
            .items
            .first()
            .ok_or_else(|| "rename planner produced an empty plan".to_owned())?;
        let from = self.identity.root.join(&plan_item.from_path);
        let to = self.identity.root.join(&plan_item.to_path);
        let note_rename = from
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            && to
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"));
        let attachment_rename = attachment_kind(&from).is_some() && attachment_kind(&to).is_some();
        if !note_rename && !attachment_rename {
            return Err(
                "rename source and destination must both be notes or supported attachments".into(),
            );
        }
        if attachment_rename && strategy == RenameStrategy::Alias {
            return Err("alias strategy is only valid for Markdown notes".into());
        }
        let old_stem = from
            .file_stem()
            .and_then(|v| v.to_str())
            .ok_or("source filename is not valid Unicode")?;
        let new_stem = to
            .file_stem()
            .and_then(|v| v.to_str())
            .ok_or("destination filename is not valid Unicode")?;
        let inbound = if strategy == RenameStrategy::Rewrite && note_rename {
            self.index.inbound_links_for_stem(old_stem)?
        } else if strategy == RenameStrategy::Rewrite {
            let relative = from
                .strip_prefix(&self.identity.root)
                .map_err(|_| "source escapes vault")?;
            let path_native = self
                .fs
                .encode_path_key(relative)
                .map_err(|error| error.to_string())?;
            self.index.inbound_embeds_for_attachment(&path_native)?
        } else {
            Vec::new()
        };
        let mut grouped = std::collections::BTreeMap::new();
        for link in inbound {
            let relative = self
                .fs
                .decode_path_key(&link.source_path_native)
                .map_err(|e| e.to_string())?;
            let source = self.identity.root.join(relative);
            let span = (
                usize::try_from(link.span_start).map_err(|_| "link span overflow")?,
                usize::try_from(link.span_end).map_err(|_| "link span overflow")?,
            );
            let entry = grouped
                .entry(link.source_path_native)
                .or_insert_with(|| (source, link.source_hash, Vec::new()));
            if entry.1 != link.source_hash {
                return Err("index contains inconsistent hashes for an inbound source".into());
            }
            entry.2.push(span);
        }

        let mut patches =
            Vec::with_capacity(grouped.len() + usize::from(strategy == RenameStrategy::Alias));
        for (_path_key, (path, expected_base_hash, mut spans)) in grouped {
            let mut bytes = self
                .fs
                .read_if_exists(&path)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("inbound source disappeared: {}", path.display()))?;
            if *blake3::hash(&bytes).as_bytes() != expected_base_hash {
                return Err(format!(
                    "inbound source changed since index scan: {}",
                    path.display()
                ));
            }
            spans.sort_unstable_by_key(|span| std::cmp::Reverse(span.0));
            let replacements = {
                let source_text = std::str::from_utf8(&bytes)
                    .map_err(|_| format!("inbound source is not UTF-8: {}", path.display()))?;
                let mut replacements = Vec::with_capacity(spans.len());
                for (start, end) in spans {
                    if start > end
                        || end > bytes.len()
                        || !source_text.is_char_boundary(start)
                        || !source_text.is_char_boundary(end)
                    {
                        return Err(format!(
                            "indexed link span is stale or invalid: {}",
                            path.display()
                        ));
                    }
                    let target = source_text
                        .get(start..end)
                        .ok_or("indexed link span is invalid")?;
                    let offset = target.find(old_stem).ok_or_else(|| {
                        format!(
                            "indexed target no longer matches link in {}",
                            path.display()
                        )
                    })?;
                    let target_start = start + offset;
                    let target_end = target_start + old_stem.len();
                    replacements.push((target_start, target_end));
                }
                replacements
            };
            for (start, end) in replacements {
                bytes.splice(start..end, new_stem.bytes());
            }
            let is_renamed_source = path == from;
            let relative_source = path
                .strip_prefix(&self.identity.root)
                .map_err(|_| "inbound link source escapes vault")?
                .to_path_buf();
            let relative_path = if is_renamed_source {
                to.strip_prefix(&self.identity.root)
                    .map_err(|_| "rename destination escapes vault")?
                    .to_path_buf()
            } else {
                relative_source.clone()
            };
            patches.push(crate::journal::FilePatch {
                path: relative_path,
                base_path: is_renamed_source.then_some(relative_source.clone()),
                kind: crate::journal::TransactionItemKind::Patch,
                expected_base_hash,
                bytes,
                buffer_id: buffer_ids.get(&relative_source).copied(),
                buffer_revision: buffer_ids
                    .get(&relative_source)
                    .and_then(|id| buffer_revisions.get(id).copied()),
                old_stem: Some(old_stem.to_owned()),
                new_stem: Some(new_stem.to_owned()),
                target_key: Some(flavite_syntax::text::fold_key(old_stem)),
                recovery_key: Some(crate::session::native_note_key(
                    if is_renamed_source { &to } else { &path },
                    &self.identity.root,
                )),
            });
        }

        if strategy == RenameStrategy::Alias {
            let source_bytes = self
                .fs
                .read_if_exists(&from)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| format!("rename source disappeared: {}", from.display()))?;
            let alias_bytes =
                flavite_syntax::frontmatter::append_list_value(&source_bytes, "aliases", old_stem)?;
            self.history_store()?
                .snapshot(
                    from.to_string_lossy().as_bytes(),
                    &source_bytes,
                    HistoryReason::Rename,
                    false,
                    crate::history::now_secs(),
                )
                .map_err(|error| error.to_string())?;
            let relative_from = from
                .strip_prefix(&self.identity.root)
                .map_err(|_| "source escapes vault")?
                .to_path_buf();
            let relative_to = to
                .strip_prefix(&self.identity.root)
                .map_err(|_| "destination escapes vault")?
                .to_path_buf();
            let buffer_id = buffer_ids.get(&relative_from).copied();
            patches.push(crate::journal::FilePatch {
                path: relative_to,
                base_path: Some(relative_from),
                kind: crate::journal::TransactionItemKind::Alias,
                expected_base_hash: *blake3::hash(&source_bytes).as_bytes(),
                bytes: alias_bytes,
                buffer_id,
                buffer_revision: buffer_id.and_then(|id| buffer_revisions.get(&id).copied()),
                old_stem: Some(old_stem.to_owned()),
                new_stem: Some(new_stem.to_owned()),
                target_key: Some(flavite_syntax::text::fold_key(old_stem)),
                recovery_key: Some(crate::session::native_note_key(&to, &self.identity.root)),
            });
        }

        let relative_from = from
            .strip_prefix(&self.identity.root)
            .map_err(|_| "source escapes vault")?;
        let relative_to = to
            .strip_prefix(&self.identity.root)
            .map_err(|_| "destination escapes vault")?;
        let patch_sender = self.buffer_patch_sender.clone();
        let mut skipped_items = 0u64;
        let txn_result = crate::journal::JournalExecutor::new(&*self.fs, &self.identity.root)
            .rename_with_patches_and_buffers(
                relative_from,
                relative_to,
                &patches,
                |txn_id, item| {
                    let (reply, response) = bounded(1);
                    let request = ApplyBufferPatchRequest {
                        txn_id: txn_id.to_owned(),
                        item_id: item.id,
                        kind: item.kind,
                        buffer_id: item.buffer_id.map(|id| id.as_u64()).unwrap_or_default(),
                        expected_revision: item.buffer_revision.unwrap_or_default(),
                        old_stem: item
                            .old_stem
                            .clone()
                            .ok_or("journal patch has no old stem")?,
                        new_stem: item
                            .new_stem
                            .clone()
                            .ok_or("journal patch has no new stem")?,
                        target_key: item
                            .target_key
                            .clone()
                            .ok_or("journal patch has no target key")?,
                        indexed_hash: item
                            .indexed_hash
                            .ok_or("journal patch has no indexed hash")?,
                        recovery_key: item.recovery_key.clone(),
                        new_base_bytes: patches
                            .iter()
                            .find(|patch| patch.path == item.from_path)
                            .map(|patch| patch.bytes.clone())
                            .ok_or("journal patch payload is missing")?,
                        reply,
                    };
                    patch_sender
                        .send_timeout(request, std::time::Duration::from_secs(10))
                        .map_err(|error| format!("buffer patch transport failed: {error}"))?;
                    let result = response
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .map_err(|error| format!("buffer patch reply failed: {error}"))?;
                    match result {
                        Ok(_) => Ok(()),
                        Err(error) => {
                            if error == "conflicted"
                                || error == "link-absent"
                                || error.starts_with("StaleRevision")
                            {
                                skipped_items = skipped_items.saturating_add(1);
                            }
                            Err(error)
                        }
                    }
                },
            );
        if skipped_items > 0 {
            self.rename_items_skipped_total
                .fetch_add(skipped_items, Ordering::Relaxed);
            self.persist_runtime_meta();
        }
        let txn = txn_result?;
        // Reindex exact committed bytes before acknowledging the command.
        let mut changed = patches
            .iter()
            .map(|patch| self.identity.root.join(&patch.path))
            .collect::<std::collections::HashSet<_>>();
        changed.insert(from.clone());
        changed.insert(to.clone());
        // Re-key an open Session buffer before the scan reports the old path
        // as deleted. Both observations are lossless and ordered on the
        // Session event channel; emitting the rename first prevents its own
        // follow-up removal from orphaning the renamed buffer.
        let _ = self.event_sender.send(VaultActorEvent::Renamed {
            from: from.clone(),
            to: to.clone(),
            tier: RenameTier::T0,
        });
        if strategy == RenameStrategy::None {
            let inbound = if note_rename {
                self.index.inbound_links_for_stem(old_stem)?.len()
            } else {
                let relative = from
                    .strip_prefix(&self.identity.root)
                    .map_err(|_| "source escapes vault")?;
                let path_native = self
                    .fs
                    .encode_path_key(relative)
                    .map_err(|error| error.to_string())?;
                self.index
                    .inbound_embeds_for_attachment(&path_native)?
                    .len()
            } as u32;
            let _ = self.event_sender.send(VaultActorEvent::LinksBroken {
                from: from.clone(),
                count: inbound,
            });
        }
        self.enqueue_reconcile(
            ReconcileScope::Full,
            ReconcileMode::VerifyContent,
            None,
            changed,
        );
        Ok(txn)
    }

    fn perform_initial_scan(&mut self) {
        self.handle_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent, None);
    }

    fn infer_note_renames(
        &mut self,
        old_notes: &[flavite_index::types::Note],
        initial_paths: &std::collections::HashSet<Vec<u8>>,
        present_paths: &std::collections::HashSet<Vec<u8>>,
        inbound_link_counts: &HashMap<std::path::PathBuf, u32>,
    ) -> std::collections::HashSet<std::path::PathBuf> {
        let watcher_pairs = std::mem::take(&mut self.recent_watcher_rename_pairs);
        let vanished = old_notes
            .iter()
            .filter(|note| !present_paths.contains(&note.path_native))
            .map(|note| std::path::PathBuf::from(&note.path))
            .collect::<Vec<_>>();
        if vanished.is_empty() {
            return std::collections::HashSet::new();
        }
        let Ok(indexed_paths) = self.index.indexed_note_paths() else {
            return std::collections::HashSet::new();
        };
        let created_native = indexed_paths
            .into_iter()
            .filter(|path| !initial_paths.contains(path))
            .collect::<std::collections::HashSet<_>>();
        if created_native.is_empty() {
            return std::collections::HashSet::new();
        }
        let Ok(current_notes) = self.index.rename_note_snapshot() else {
            return std::collections::HashSet::new();
        };
        let old_by_path = old_notes
            .iter()
            .cloned()
            .map(|note| (std::path::PathBuf::from(&note.path), note))
            .collect::<HashMap<_, _>>();
        let current_by_path = current_notes
            .iter()
            .cloned()
            .map(|note| (std::path::PathBuf::from(&note.path), note))
            .collect::<HashMap<_, _>>();
        let created = current_notes
            .iter()
            .filter(|note| created_native.contains(&note.path_native))
            .map(|note| std::path::PathBuf::from(&note.path))
            .collect::<Vec<_>>();
        if vanished.is_empty() || created.is_empty() {
            return std::collections::HashSet::new();
        }
        let result = crate::rename::RenameInference::new().infer_reconcile(
            &vanished,
            &created,
            &old_by_path,
            &current_by_path,
            inbound_link_counts,
        );
        let mut t2_sources = std::collections::HashSet::new();
        for candidate in result.candidates {
            let from = self.identity.root.join(&candidate.from);
            let to = self.identity.root.join(&candidate.to);
            if watcher_pairs.contains(&(from.clone(), to.clone())) {
                continue;
            }
            if candidate.tier == RenameTier::T2 {
                t2_sources.insert(from.clone());
                let _ = self.event_sender.send(VaultActorEvent::Renamed {
                    from: from.clone(),
                    to: to.clone(),
                    tier: RenameTier::T2,
                });
            }
            let _ = self.event_sender.send(VaultActorEvent::RenameSuggested {
                from,
                to,
                tier: candidate.tier,
                inbound: candidate.inbound_links,
            });
        }
        t2_sources
    }
}

fn attachment_kind(path: &std::path::Path) -> Option<flavite_index::schema::AttachmentKind> {
    use flavite_index::schema::AttachmentKind as K;
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "avif" | "bmp" | "ico" => K::Image,
        "pdf" => K::Pdf,
        "mp3" | "wav" | "ogg" | "flac" | "m4a" => K::Audio,
        "mp4" | "webm" | "mov" | "mkv" => K::Video,
        "zip" | "tar" | "gz" | "tgz" => K::Archive,
        _ => return None,
    })
}

fn path_observation_is_stale(last_saved_seq: u64, watermark: u64) -> bool {
    last_saved_seq > watermark
}

/// Extension trait for Fs to create boxed clones.
pub trait FsExt {
    fn boxed_clone(&self) -> Box<dyn Fs + Send + Sync>;
}

impl<T: Fs + Clone + Send + Sync + 'static> FsExt for T {
    fn boxed_clone(&self) -> Box<dyn Fs + Send + Sync> {
        Box::new(self.clone())
    }
}

/// Builder for VaultActor.
#[derive(Default)]
pub struct VaultActorBuilder {
    fs: Option<Box<dyn Fs + Send + Sync>>,
    identity: Option<VaultIdentity>,
    index: Option<IndexActor>,
    event_source: Option<EventSource>,
    pulse_sender: Option<Sender<Pulse>>,
    runtime_config: Option<crate::types::VaultConfig>,
    reconcile_result_delay: std::time::Duration,
    reconcile_scan_signal: Option<Sender<()>>,
    watcher_start_failure: Option<String>,
}

impl VaultActorBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the filesystem.
    #[must_use]
    pub fn with_fs(mut self, fs: Box<dyn Fs + Send + Sync>) -> Self {
        self.fs = Some(fs);
        self
    }

    /// Set the vault identity.
    #[must_use]
    pub fn with_identity(mut self, identity: VaultIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Set the index.
    #[must_use]
    pub fn with_index(mut self, index: IndexActor) -> Self {
        self.index = Some(index);
        self
    }

    /// Set a deterministic injected event stream instead of OS notifications.
    #[must_use]
    pub fn with_event_source(mut self, source: EventSource) -> Self {
        self.event_source = Some(source);
        self
    }

    #[cfg(test)]
    fn with_reconcile_result_delay(mut self, delay: std::time::Duration) -> Self {
        self.reconcile_result_delay = delay;
        self
    }

    #[cfg(test)]
    fn with_reconcile_scan_signal(mut self, signal: Sender<()>) -> Self {
        self.reconcile_scan_signal = Some(signal);
        self
    }

    #[cfg(test)]
    fn with_watcher_start_failure(mut self, message: &str) -> Self {
        self.watcher_start_failure = Some(message.to_owned());
        self
    }

    /// Send lossy reconcile pulses to the Session's shared channel.
    #[must_use]
    pub fn with_pulse_sender(mut self, sender: Sender<Pulse>) -> Self {
        self.pulse_sender = Some(sender);
        self
    }

    /// Apply watcher, verification, and coalescing settings from the vault.
    #[must_use]
    pub fn with_runtime_config(mut self, config: crate::types::VaultConfig) -> Self {
        self.runtime_config = Some(config);
        self
    }

    /// Build the VaultActor.
    pub fn build(self) -> Result<VaultActor, String> {
        let mut actor = VaultActor::new(
            self.fs.ok_or("Missing filesystem")?,
            self.identity.ok_or("Missing identity")?,
            self.index.ok_or("Missing structural index")?,
        );
        actor.event_source = self.event_source;
        actor.pulse_sender = self.pulse_sender;
        if let Some(config) = self.runtime_config {
            actor.watcher_enabled = config.watcher_enabled;
            actor.verification_interval = config.verification_interval;
            actor.coalescer = EventCoalescer::new(crate::coalesce::CoalescerConfig {
                quiet_window: config.coalesce_quiet_window,
                max_window: config.coalesce_max_window,
                bulk_threshold: config.bulk_threshold,
            });
        }
        actor.reconcile_result_delay = self.reconcile_result_delay;
        actor.reconcile_scan_signal = self.reconcile_scan_signal;
        actor.watcher_start_failure = self.watcher_start_failure;
        Ok(actor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::WriteTokenManager;
    use flavite_fs::{Fs, MemFs};

    fn start_test_actor(fs: &MemFs) -> (VaultActorHandle, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()))
            .with_identity(VaultIdentity::new(
                uuid::Uuid::now_v7(),
                PathBuf::from("/vault"),
                vec![1, 2, 3, 4],
            ))
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        reconcile_test_actor(&actor);
        (actor, temp)
    }

    fn reconcile_test_actor(actor: &VaultActorHandle) {
        let response = actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(response, VaultActorResp::ReconcileComplete { .. }));
    }

    fn collect_rename_events(actor: &VaultActorHandle) -> Vec<VaultActorEvent> {
        actor.events().try_iter().collect()
    }

    #[test]
    fn test_write_token() {
        use std::path::PathBuf;
        let manager = WriteTokenManager::new();
        let hash = [0u8; 32];
        let path = PathBuf::from("/test/file.md");

        let token = manager.acquire(path.clone(), hash);

        assert!(manager.has_token(&path));
        assert!(token.matches(&hash));
        assert!(!token.is_expired());

        manager.release(&token);
        assert!(!manager.has_token(&path));
    }

    #[test]
    fn test_vault_actor_builder() {
        use std::path::PathBuf;
        let fs = Box::new(MemFs::new()) as Box<dyn Fs + Send + Sync>;
        let identity = VaultIdentity::new(
            uuid::Uuid::now_v7(),
            PathBuf::from("/test/vault"),
            vec![1, 2, 3, 4],
        );

        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db_path).unwrap());
        let index = IndexActor::new(&db_path, 1).unwrap();
        let actor = VaultActorBuilder::new()
            .with_fs(fs)
            .with_identity(identity)
            .with_index(index)
            .build()
            .unwrap();

        // The actor should be created but not started
        assert!(!actor.is_running.load(Ordering::SeqCst));
    }

    #[test]
    fn actor_save_reaches_disk_history_and_structural_sqlite_before_ack() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault/notes"))
            .unwrap();
        fs.write_new(std::path::Path::new("/vault/notes/a.md"), b"# Base")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db_path).unwrap());
        let index = IndexActor::new(&db_path, 1).unwrap();
        let identity = VaultIdentity::new(
            uuid::Uuid::now_v7(),
            std::path::PathBuf::from("/vault"),
            vec![1, 2, 3, 4],
        );
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()))
            .with_identity(identity)
            .with_index(index)
            .build()
            .unwrap()
            .start()
            .unwrap();
        reconcile_test_actor(&actor);
        assert!(actor.get_version().is_ok());
        assert_eq!(
            actor
                .read_file(std::path::PathBuf::from("/vault/notes/a.md"))
                .unwrap(),
            Some(b"# Base".to_vec())
        );
        assert_eq!(
            actor
                .read_file(std::path::PathBuf::from("/vault/missing.md"))
                .unwrap(),
            None
        );
        let snapshot = actor
            .snapshot_history(
                b"notes/a.md".to_vec(),
                b"pre-conflict".to_vec(),
                HistoryReason::ConflictResolution,
            )
            .unwrap();
        assert_eq!(snapshot.note_key, "6e6f7465732f612e6d64");
        let merge_base = actor
            .pin_merge_base(b"notes/a.md".to_vec(), b"# Base".to_vec())
            .unwrap();
        assert!(
            actor
                .unpin_merge_base(b"notes/a.md".to_vec(), merge_base.hash)
                .is_ok()
        );
        let prior_hash = *blake3::hash(b"# Base").as_bytes();
        let note_key = fs
            .encode_path_key(std::path::Path::new("notes/a.md"))
            .unwrap();
        let request = SaveRequest {
            request_id: 17,
            path: std::path::PathBuf::from("/vault/notes/a.md"),
            note_key,
            bytes: b"# Saved\n\n[[Other]]".to_vec(),
            merge_base: b"# Base".to_vec(),
            precondition: crate::save::DiskPrecondition::Existing(prior_hash),
            buffer_id: 5,
            revision: 8,
        };
        let response = actor.submit_save(request).unwrap();
        let receipt = match response.recv().unwrap() {
            VaultActorResp::Saved(Ok(receipt)) => receipt,
            other => panic!("unexpected save reply: {other:?}"),
        };
        // Startup scan indexes the pre-existing base before the save applies.
        assert_eq!((receipt.epoch, receipt.generation), (1, 2));
        assert_eq!(
            fs.read_if_exists(std::path::Path::new("/vault/notes/a.md"))
                .unwrap()
                .unwrap(),
            b"# Saved\n\n[[Other]]"
        );
        let check = flavite_index::Writer::new(&db_path).unwrap();
        let indexed: String = check
            .conn()
            .query_row("SELECT body_text FROM search_docs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(indexed, "# Saved\n\n[[Other]]");
        drop(actor);
    }

    #[test]
    fn session_actor_rename_rewrites_all_indexed_inbound_links() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault/notes"))
            .unwrap();
        fs.write_new(std::path::Path::new("/vault/notes/a.md"), b"# A\n[[a]]\n")
            .unwrap();
        for i in 0..14 {
            fs.write_new(
                &std::path::PathBuf::from(format!("/vault/notes/ref-{i}.md")),
                b"[[a]]\n",
            )
            .unwrap();
        }
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db_path).unwrap());
        let index = IndexActor::new(&db_path, 1).unwrap();
        let identity = VaultIdentity::new(
            uuid::Uuid::now_v7(),
            std::path::PathBuf::from("/vault"),
            vec![1, 2, 3, 4],
        );
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()))
            .with_identity(identity)
            .with_index(index)
            .build()
            .unwrap()
            .start()
            .unwrap();
        reconcile_test_actor(&actor);

        let response = actor
            .submit_rename(
                std::path::PathBuf::from("/vault/notes/a.md"),
                std::path::PathBuf::from("/vault/notes/b.md"),
                RenameStrategy::Rewrite,
                HashMap::new(),
                HashMap::new(),
            )
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(
            matches!(response, VaultActorResp::Renamed(Ok(_))),
            "{response:?}"
        );
        assert_eq!(
            fs.read_if_exists(std::path::Path::new("/vault/notes/a.md"))
                .unwrap(),
            None
        );
        assert_eq!(
            fs.read_if_exists(std::path::Path::new("/vault/notes/b.md"))
                .unwrap(),
            Some(b"# A\n[[b]]\n".to_vec())
        );
        for i in 0..14 {
            let rewritten = fs
                .read_if_exists(&std::path::PathBuf::from(format!(
                    "/vault/notes/ref-{i}.md"
                )))
                .unwrap()
                .unwrap();
            assert_eq!(rewritten, b"[[b]]\n", "inbound file {i}");
        }
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn reconcile_infers_t2_t3_t4_with_tier_specific_events() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault/notes"))
            .unwrap();
        fs.write_new(
            std::path::Path::new("/vault/notes/old-id.md"),
            b"---\nid: Stable-X\n---\n# Same identity\n",
        )
        .unwrap();
        fs.write_new(
            std::path::Path::new("/vault/notes/old-copy.md"),
            b"# A copied note with enough content to qualify for content hash inference.\n",
        )
        .unwrap();
        fs.write_new(
            std::path::Path::new("/vault/notes/proposal.md"),
            b"# Different contents, similar filename.\n",
        )
        .unwrap();
        let (actor, _temp) = start_test_actor(&fs);
        let old_id = std::path::Path::new("/vault/notes/old-id.md");
        let old_copy = std::path::Path::new("/vault/notes/old-copy.md");
        let proposal = std::path::Path::new("/vault/notes/proposal.md");
        fs.remove(old_id).unwrap();
        fs.remove(old_copy).unwrap();
        fs.remove(proposal).unwrap();
        fs.write_new(
            std::path::Path::new("/vault/notes/new-id.md"),
            b"---\nid: stable-x\n---\n# Same identity, changed filename\n",
        )
        .unwrap();
        fs.create_dir_all(std::path::Path::new("/vault/archive"))
            .unwrap();
        fs.write_new(
            std::path::Path::new("/vault/archive/new-copy.md"),
            b"# A copied note with enough content to qualify for content hash inference.\n",
        )
        .unwrap();
        fs.write_new(
            std::path::Path::new("/vault/notes/proposals.md"),
            b"# Entirely different contents, similar filename.\n",
        )
        .unwrap();

        reconcile_test_actor(&actor);
        let events = collect_rename_events(&actor);
        let suggestions = events
            .iter()
            .filter_map(|event| match event {
                VaultActorEvent::RenameSuggested { from, to, tier, .. } => Some((from, to, *tier)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(suggestions.iter().any(|(from, to, tier)| {
            *tier == RenameTier::T2 && from.ends_with("old-id.md") && to.ends_with("new-id.md")
        }));
        assert!(suggestions.iter().any(|(from, to, tier)| {
            *tier == RenameTier::T3 && from.ends_with("old-copy.md") && to.ends_with("new-copy.md")
        }));
        assert!(suggestions.iter().any(|(from, to, tier)| {
            *tier == RenameTier::T4 && from.ends_with("proposal.md") && to.ends_with("proposals.md")
        }));
        assert!(events.iter().any(|event| matches!(
            event,
            VaultActorEvent::Renamed { from, to, tier: RenameTier::T2 }
                if from.ends_with("old-id.md") && to.ends_with("new-id.md")
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            VaultActorEvent::Renamed {
                tier: RenameTier::T3 | RenameTier::T4,
                ..
            }
        )));
        assert!(!events.iter().any(|event| matches!(
            event,
            VaultActorEvent::Deleted { path } if path.ends_with("old-id.md")
        )));
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn actor_rename_rejection_comes_from_rename_planner() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault/notes"))
            .unwrap();
        fs.write_new(std::path::Path::new("/vault/notes/a.md"), b"# A")
            .unwrap();
        fs.write_new(std::path::Path::new("/vault/notes/b.md"), b"# B")
            .unwrap();
        let (actor, _temp) = start_test_actor(&fs);

        let response = actor
            .submit_rename(
                PathBuf::from("/vault/notes/a.md"),
                PathBuf::from("/vault/notes/b.md"),
                RenameStrategy::Rewrite,
                HashMap::new(),
                HashMap::new(),
            )
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            response,
            VaultActorResp::Renamed(Err(message))
                if message == "destination is occupied: /vault/notes/b.md"
        ));
        assert!(
            fs.read_if_exists(std::path::Path::new("/vault/notes/a.md"))
                .unwrap()
                .is_some()
        );
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn injected_watcher_event_reconciles_and_emits_external_change() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        fs.write_new(std::path::Path::new("/vault/a.md"), b"before")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watch-index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let index = IndexActor::new(&db, 1).unwrap();
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), "/vault".into(), vec![1, 2, 3, 4]);
        let (event_tx, event_rx) = event_source_channel();
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()))
            .with_identity(identity)
            .with_index(index)
            .with_event_source(event_rx)
            .build()
            .unwrap()
            .start()
            .unwrap();
        reconcile_test_actor(&actor);
        assert_eq!(actor.get_version().unwrap().generation.0, 1);
        let _ = actor.events().try_iter().count();
        fs.write_new(std::path::Path::new("/vault/.next"), b"after")
            .unwrap();
        fs.rename_replace(
            std::path::Path::new("/vault/.next"),
            std::path::Path::new("/vault/a.md"),
        )
        .unwrap();
        event_tx
            .send(WatcherEvent::Modified("/vault/a.md".into()))
            .unwrap();
        let events = actor.events();
        let changed = events
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(
            matches!(changed, VaultActorEvent::ExternalChange { ref bytes, .. } if bytes == b"after")
        );
        let version = actor.get_version().unwrap();
        assert!(version.generation.0 >= 2);
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn watcher_disconnect_reports_loss_and_does_not_busy_spin() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watch-loss.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), "/vault".into(), vec![1, 2, 3, 4]);
        let (event_tx, event_rx) = event_source_channel();
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs))
            .with_identity(identity)
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_event_source(event_rx)
            .build()
            .unwrap()
            .start()
            .unwrap();
        actor.get_version().unwrap();
        drop(event_tx);
        assert!(matches!(
            actor
                .events()
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            VaultActorEvent::WatcherLost
        ));
        assert_eq!(actor.drift_found_total(), 1);
        assert!(actor.get_version().is_ok());
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn save_reply_completes_while_initial_reconcile_is_in_flight() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        fs.write_new(std::path::Path::new("/vault/a.md"), b"before")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("slow-reconcile.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let note_key = fs.encode_path_key(std::path::Path::new("a.md")).unwrap();
        let (scan_signal, scan_finished) = bounded(1);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()))
            .with_identity(VaultIdentity::new(
                uuid::Uuid::now_v7(),
                "/vault".into(),
                vec![1],
            ))
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_reconcile_result_delay(std::time::Duration::from_millis(500))
            .with_reconcile_scan_signal(scan_signal)
            .build()
            .unwrap()
            .start()
            .unwrap();
        scan_finished
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let response = actor
            .submit_save(SaveRequest {
                request_id: 71,
                path: PathBuf::from("/vault/a.md"),
                note_key,
                bytes: b"saved while scan waits".to_vec(),
                merge_base: b"before".to_vec(),
                precondition: crate::save::DiskPrecondition::Existing(
                    *blake3::hash(b"before").as_bytes(),
                ),
                buffer_id: 4,
                revision: 2,
            })
            .unwrap();
        assert!(matches!(
            response
                .recv_timeout(std::time::Duration::from_millis(350))
                .unwrap(),
            VaultActorResp::Saved(Ok(_))
        ));
        assert_eq!(
            fs.read_if_exists(std::path::Path::new("/vault/a.md"))
                .unwrap()
                .unwrap(),
            b"saved while scan waits"
        );
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn queued_reconcile_gets_explicit_superseded_terminal_reply() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        fs.write_new(std::path::Path::new("/vault/a.md"), b"a")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("superseded.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let (scan_started, scan_finished) = bounded(1);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs))
            .with_identity(VaultIdentity::new(
                uuid::Uuid::now_v7(),
                "/vault".into(),
                vec![1],
            ))
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_reconcile_result_delay(std::time::Duration::from_millis(500))
            .with_reconcile_scan_signal(scan_started)
            .build()
            .unwrap()
            .start()
            .unwrap();
        scan_finished
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let first = actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap();
        let second = actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap();
        assert!(matches!(
            first
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            VaultActorResp::ReconcileSuperseded { by_request: 3 }
        ));
        assert!(matches!(
            second
                .recv_timeout(std::time::Duration::from_secs(3))
                .unwrap(),
            VaultActorResp::ReconcileComplete { .. }
        ));
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn watcher_start_failure_surfaces_watcher_lost_and_verifies() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watch-start-failure.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs))
            .with_identity(VaultIdentity::new(
                uuid::Uuid::now_v7(),
                "/vault".into(),
                vec![1],
            ))
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_watcher_start_failure("injected watcher construction failure")
            .build()
            .unwrap()
            .start()
            .unwrap();
        assert!(matches!(
            actor
                .events()
                .recv_timeout(std::time::Duration::from_secs(1))
                .unwrap(),
            VaultActorEvent::WatcherLost
        ));
        assert_eq!(actor.drift_found_total(), 1);
        assert!(matches!(
            actor
                .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
                .unwrap()
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            VaultActorResp::ReconcileComplete { .. }
        ));
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn initial_scan_does_not_emit_external_changes_and_indexes_attachments() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        fs.write_new(std::path::Path::new("/vault/note.md"), b"![[diagram.png]]")
            .unwrap();
        fs.write_new(std::path::Path::new("/vault/diagram.png"), b"image-bytes")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("attachment-index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), "/vault".into(), vec![1, 2, 3, 4]);
        let (event_tx, event_rx) = event_source_channel();
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs))
            .with_identity(identity)
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_event_source(event_rx)
            .build()
            .unwrap()
            .start()
            .unwrap();
        reconcile_test_actor(&actor);
        assert_eq!(actor.get_version().unwrap().generation.0, 2);
        assert!(actor.events().try_recv().is_err());
        let writer = flavite_index::Writer::new(&db).unwrap();
        let count: i64 = writer
            .conn()
            .query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let (dst_att, state): (Option<i64>, String) = writer
            .conn()
            .query_row(
                "SELECT dst_att,res_state FROM links WHERE kind='embed'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(dst_att.is_some());
        assert_eq!(state, "resolved");
        drop(event_tx);
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn initial_scan_lists_oversize_notes_as_unparsed_rows() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        let bytes = vec![b'x'; MAX_NOTE_BYTES + 1];
        fs.write_new(std::path::Path::new("/vault/large.md"), &bytes)
            .unwrap();
        assert_eq!(
            fs.metadata(std::path::Path::new("/vault/large.md"))
                .unwrap()
                .size,
            (MAX_NOTE_BYTES + 1) as u64
        );
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("oversize-index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), "/vault".into(), vec![1, 2, 3, 4]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs))
            .with_identity(identity)
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        reconcile_test_actor(&actor);
        let writer = flavite_index::Writer::new(&db).unwrap();
        let row: (bool, Option<Vec<u8>>, i64) = writer
            .conn()
            .query_row(
                "SELECT parse_ok,content_hash,size FROM notes WHERE path='large.md'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (false, None, i64::try_from(MAX_NOTE_BYTES + 1).unwrap())
        );
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn focus_verification_catches_a_suppressed_watcher_change() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/focus")).unwrap();
        fs.write_new(std::path::Path::new("/focus/a.md"), b"before")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("focus-index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let channels = crate::channels::ChannelSet::new();
        let pulse_rx = channels.pulse.receiver();
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()))
            .with_identity(VaultIdentity::new(
                uuid::Uuid::now_v7(),
                "/focus".into(),
                vec![1],
            ))
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_pulse_sender(channels.pulse.sender())
            .build()
            .unwrap()
            .start()
            .unwrap();
        reconcile_test_actor(&actor);
        let initial = actor.get_version().unwrap().generation.0;
        assert_eq!(actor.drift_found_total(), 0);
        let _ = actor.events().try_iter().count();
        let _ = pulse_rx.try_recv();
        fs.write_new(std::path::Path::new("/focus/.temp"), b"after")
            .unwrap();
        fs.rename_replace(
            std::path::Path::new("/focus/.temp"),
            std::path::Path::new("/focus/a.md"),
        )
        .unwrap();
        actor.focus().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while actor.get_version().unwrap().generation.0 <= initial
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(actor.get_version().unwrap().generation.0 > initial);
        assert_eq!(actor.drift_found_total(), 1);
        assert!(
            matches!(actor.events().try_recv(), Ok(VaultActorEvent::ExternalChange { bytes, .. }) if bytes == b"after")
        );
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn startup_events_queued_during_initial_scan_are_replayed() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        fs.write_new(std::path::Path::new("/vault/a.md"), b"before")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("startup-race.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let (events, source) = event_source_channel();
        let (scan_signal, scan_done) = bounded(1);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()))
            .with_identity(VaultIdentity::new(
                uuid::Uuid::now_v7(),
                "/vault".into(),
                vec![1],
            ))
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_event_source(source)
            .with_reconcile_result_delay(std::time::Duration::from_millis(300))
            .with_reconcile_scan_signal(scan_signal)
            .build()
            .unwrap()
            .start()
            .unwrap();
        scan_done
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        fs.write_new(std::path::Path::new("/vault/.next"), b"after")
            .unwrap();
        fs.rename_replace(
            std::path::Path::new("/vault/.next"),
            std::path::Path::new("/vault/a.md"),
        )
        .unwrap();
        events
            .send(WatcherEvent::Modified("/vault/a.md".into()))
            .unwrap();
        assert!(
            matches!(actor.events().recv_timeout(std::time::Duration::from_secs(3)).unwrap(), VaultActorEvent::ExternalChange { bytes, .. } if bytes == b"after")
        );
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn shutdown_joins_reconcile_worker_before_actor_exits() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("worker-shutdown.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let (signal, scan_finished) = bounded(1);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs))
            .with_identity(VaultIdentity::new(
                uuid::Uuid::now_v7(),
                "/vault".into(),
                vec![1],
            ))
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_reconcile_scan_signal(signal)
            .build()
            .unwrap()
            .start()
            .unwrap();
        scan_finished
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let _ = actor.send(VaultActorMsg::Shutdown);
        assert!(matches!(
            scan_finished.recv_timeout(std::time::Duration::from_secs(2)),
            Err(crossbeam::channel::RecvTimeoutError::Disconnected)
        ));
    }

    #[test]
    fn focus_throttle_within_two_seconds_does_not_rescan() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/focus-throttle"))
            .unwrap();
        fs.write_new(std::path::Path::new("/focus-throttle/a.md"), b"a")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("focus-throttle.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let channels = crate::channels::ChannelSet::new();
        let pulses = channels.pulse.receiver();
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs))
            .with_identity(VaultIdentity::new(
                uuid::Uuid::now_v7(),
                "/focus-throttle".into(),
                vec![1],
            ))
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_pulse_sender(channels.pulse.sender())
            .build()
            .unwrap()
            .start()
            .unwrap();
        reconcile_test_actor(&actor);
        while pulses.try_recv().is_ok() {}
        actor.focus().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if matches!(pulses.try_recv(), Ok(Pulse::VaultChanged { .. })) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        while pulses.try_recv().is_ok() {}
        actor.focus().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(pulses.try_recv().is_err());
        let _ = actor.send(VaultActorMsg::Shutdown);
    }

    #[test]
    fn scan_outcome_for_path_saved_after_enqueue_is_dropped() {
        let fs = MemFs::new();
        fs.create_dir_all(std::path::Path::new("/vault")).unwrap();
        fs.write_new(std::path::Path::new("/vault/a.md"), b"saved bytes")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("watermark.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let mut index = IndexActor::new(&db, 1).unwrap();
        let key = fs.encode_path_key(std::path::Path::new("a.md")).unwrap();
        let scan_ns = 1;
        index
            .apply_saved_note(&key, "a.md", b"saved bytes", 1, scan_ns)
            .unwrap();
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), "/vault".into(), vec![1]);
        let mut actor = VaultActor::new(Box::new(fs.clone()), identity, index);
        actor.save_seq = 1;
        actor.save_paths.insert(PathBuf::from("/vault/a.md"), 1);
        let job = crate::reconcile_worker::ReconcileJob {
            id: 10,
            scope: ReconcileScope::Full,
            mode: ReconcileMode::VerifyContent,
            watermark: 0,
            nominated: Default::default(),
            reply: None,
        };
        let mut context = ReconcileContext {
            job,
            initial_paths: [key.clone()].into_iter().collect(),
            initial_attachments: Default::default(),
            old_notes: Vec::new(),
            inbound_counts: HashMap::new(),
            summary: ReconcileSummary::default(),
            deleted_events: Vec::new(),
        };
        let result = crate::reconcile_worker::ScanResult {
            outcomes: vec![crate::reconcile_worker::ScanOutcome::Note {
                path: PathBuf::from("/vault/a.md"),
                path_key: key.clone(),
                display_path: "a.md".into(),
                size: 10,
                mtime_ns: 1,
                bytes: Some(b"stale scan".to_vec()),
                parsed: flavite_syntax::pulldown_glue::parse_note(b"stale scan").ok(),
            }],
            present_notes: [key.clone()].into_iter().collect(),
            present_attachments: Default::default(),
            incomplete: false,
            total: 1,
            full_scan: false,
            complete: true,
        };
        actor.apply_scan_result(&mut context, result);
        let writer = flavite_index::Writer::new(&db).unwrap();
        let hash: Vec<u8> = writer
            .conn()
            .query_row(
                "SELECT content_hash FROM notes WHERE path='a.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash, blake3::hash(b"saved bytes").as_bytes());
    }

    #[test]
    fn save_watermark_interleavings_preserve_saved_index_bytes() {
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;
        let mut runner = TestRunner::new(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        });
        runner
            .run(
                &(1u64..1_000_000, any::<bool>(), any::<bool>()),
                |(save_seq, saved_before_enqueue, save_before_result)| {
                    // Enqueue precedes worker read. Saves before enqueue are visible
                    // to that read; later saves are either filtered or indexed inline.
                    let watermark = if saved_before_enqueue { save_seq } else { 0 };
                    let scanned = if saved_before_enqueue {
                        b"saved".as_slice()
                    } else {
                        b"old".as_slice()
                    };
                    let indexed = if saved_before_enqueue {
                        scanned
                    } else if save_before_result {
                        let saved_seq_after_enqueue = save_seq;
                        if path_observation_is_stale(saved_seq_after_enqueue, watermark) {
                            b"saved".as_slice()
                        } else {
                            scanned
                        }
                    } else {
                        // The result applies first; the later save indexes inline.
                        b"saved".as_slice()
                    };
                    prop_assert_eq!(indexed, b"saved".as_slice());
                    Ok(())
                },
            )
            .unwrap();
    }
}
