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
use crate::save::{SaveFailure, SaveReceipt, SaveRequest};
use crate::types::{
    Epoch, Generation, ReconcileMode, ReconcileScope, ReconcileSummary, VaultIdentity, Version,
};
use crossbeam::channel::{Receiver, Sender, bounded, select, tick, unbounded};
use flavite_fs::Fs;
use flavite_index::{IndexActor, MAX_NOTE_BYTES};
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
    /// Execute a D9 rename after refusing to mutate any open inbound buffer.
    RenameNoteRequest {
        from: std::path::PathBuf,
        to: std::path::PathBuf,
        protected_paths: Vec<std::path::PathBuf>,
        reply: Sender<VaultActorResp>,
    },
    /// Notify of a filesystem event.
    FsEvent(WatcherEvent),
    /// The owning window gained focus; verify disk state after throttling.
    Focus,
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
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Handle to communicate with the VaultActor.
#[derive(Debug, Clone)]
pub struct VaultActorHandle {
    sender: Sender<VaultActorMsg>,
    receiver: Receiver<VaultActorResp>,
    events: Receiver<VaultActorEvent>,
    drift_found_total: Arc<std::sync::atomic::AtomicU64>,
}

impl VaultActorHandle {
    /// Get the lossless external-change stream for Session processing.
    #[must_use]
    pub fn events(&self) -> Receiver<VaultActorEvent> {
        self.events.clone()
    }

    /// Request a throttled full verification after the host window gains focus.
    pub fn focus(&self) -> Result<(), VaultActorError> {
        self.try_send(VaultActorMsg::Focus)
    }

    /// Number of indexed disk drifts found by reconciliation.
    #[must_use]
    pub fn drift_found_total(&self) -> u64 {
        self.drift_found_total.load(Ordering::Relaxed)
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
        protected_paths: Vec<std::path::PathBuf>,
    ) -> Result<Receiver<VaultActorResp>, VaultActorError> {
        let (reply, receiver) = crossbeam::channel::bounded(1);
        self.try_send(VaultActorMsg::RenameNoteRequest {
            from,
            to,
            protected_paths,
            reply,
        })?;
        Ok(receiver)
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
    drift_found_total: Arc<std::sync::atomic::AtomicU64>,
    event_source: Option<EventSource>,
    pulse_sender: Option<Sender<Pulse>>,
    coalescer: EventCoalescer,
    watcher_enabled: bool,
    initial_scan_complete: bool,
    verification_interval: std::time::Duration,
}

impl VaultActor {
    fn record_drift(&self) {
        let total = self.drift_found_total.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::info!(drift_found_total = total, "reconcile drift found");
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
            drift_found_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            event_source: None,
            pulse_sender: None,
            coalescer: EventCoalescer::with_defaults(),
            watcher_enabled: true,
            initial_scan_complete: false,
            verification_interval: std::time::Duration::from_secs(600),
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
        let drift_found_total = Arc::clone(&self.drift_found_total);

        self.msg_receiver = msg_receiver;
        self.resp_sender = resp_sender;
        self.event_sender = event_sender;

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
            drift_found_total,
        })
    }

    /// The main actor loop.
    fn run(&mut self, is_running: Arc<AtomicBool>) {
        is_running.store(true, Ordering::SeqCst);

        info!(
            "VaultActor started for vault: {}",
            self.identity.root.display()
        );

        // Start the watcher before scanning, then reconcile once. Events which
        // arrive during the scan remain queued and are replayed afterwards.
        let mut fs_watcher = None;
        let mut event_receiver = if let Some(receiver) = self.event_source.take() {
            receiver
        } else if self.watcher_enabled && self.identity.root.is_dir() {
            match flavite_fs::watch::FsWatcher::with_defaults() {
                Ok(mut watcher) => {
                    if watcher.watch(&self.identity.root).is_err() {
                        crossbeam::channel::never()
                    } else {
                        let std_receiver = watcher.start();
                        let (sender, receiver) = bounded(1024);
                        let bridge = sender.clone();
                        thread::Builder::new()
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
                            })
                            .ok();
                        fs_watcher = Some(watcher);
                        receiver
                    }
                }
                _ => crossbeam::channel::never(),
            }
        } else {
            crossbeam::channel::never()
        };
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
                        self.drift_found_total.fetch_add(1, Ordering::Relaxed);
                        let _ = self.event_sender.send(VaultActorEvent::WatcherLost);
                        self.handle_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent, None);
                    }
                },
                recv(timer) -> _ => {
                    if let Some(batch) = self.coalescer.try_recv_batch() {
                        if batch.is_bulk || batch.paths.is_empty() {
                            if let Some(pulse) = &self.pulse_sender {
                                let _ = pulse.try_send(Pulse::ReconcileProgress { done: 0, total: batch.paths.len() as u32 });
                            }
                            self.handle_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent, None);
                            if let Some(pulse) = &self.pulse_sender {
                                let total = batch.paths.len() as u32;
                                let _ = pulse.try_send(Pulse::ReconcileProgress { done: total, total });
                            }
                        } else {
                            let summary = self.scan_paths(&batch.paths);
                            self.publish_reconcile(summary, None);
                        }
                        for (from, to) in batch.rename_pairs {
                            self.handle_external_rename_pair(from, to);
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
            }
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
                protected_paths,
                reply,
            } => {
                let result = self.execute_journaled_rename(&from, &to, &protected_paths);
                let _ = reply.try_send(VaultActorResp::Renamed(result));
            }
            VaultActorMsg::FsEvent(event) => {
                self.handle_fs_event(event);
            }
            VaultActorMsg::Focus => {}
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
            VaultActorMsg::Save { request, reply } => self.handle_save(request, reply),
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
        _scope: ReconcileScope,
        mode: ReconcileMode,
        reply: Option<Sender<VaultActorResp>>,
    ) {
        let summary = match &_scope {
            ReconcileScope::Single(path) => {
                self.scan_paths(&std::iter::once(path.clone()).collect())
            }
            ReconcileScope::Directory(path) if path.is_dir() => {
                // Directory reconciles use a full walk today so vanished paths
                // in that subtree are discovered safely.
                self.scan_notes_mode(mode)
            }
            ReconcileScope::Directory(_) | ReconcileScope::Full => self.scan_notes_mode(mode),
        };
        self.publish_reconcile(summary, reply);
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
    /// Open inbound buffers are rejected because applying disk patches without
    /// a correlated Session EditOp would violate the single-buffer authority.
    fn execute_journaled_rename(
        &mut self,
        from: &std::path::Path,
        to: &std::path::Path,
        protected_paths: &[std::path::PathBuf],
    ) -> Result<String, String> {
        let from = if from.is_absolute() {
            from.to_path_buf()
        } else {
            self.identity.root.join(from)
        };
        let to = if to.is_absolute() {
            to.to_path_buf()
        } else {
            self.identity.root.join(to)
        };
        if !from.starts_with(&self.identity.root) || !to.starts_with(&self.identity.root) {
            return Err("rename paths must remain inside the vault".into());
        }
        if !from
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            || !to
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
        {
            return Err("Session rename currently accepts Markdown notes only".into());
        }
        let old_stem = from
            .file_stem()
            .and_then(|v| v.to_str())
            .ok_or("source filename is not valid Unicode")?;
        let new_stem = to
            .file_stem()
            .and_then(|v| v.to_str())
            .ok_or("destination filename is not valid Unicode")?;
        let inbound = self.index.inbound_links_for_stem(old_stem)?;
        let mut grouped = std::collections::BTreeMap::new();
        for link in inbound {
            let relative = self
                .fs
                .decode_path_key(&link.source_path_native)
                .map_err(|e| e.to_string())?;
            let source = self.identity.root.join(relative);
            if source == from {
                return Err(
                    "rename has self-referential links; source-buffer remapping is required".into(),
                );
            }
            if protected_paths.iter().any(|path| path == &source) {
                return Err(format!(
                    "inbound link source is open in a buffer: {}",
                    source.display()
                ));
            }
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

        let mut patches = Vec::with_capacity(grouped.len());
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
            let relative_path = path
                .strip_prefix(&self.identity.root)
                .map_err(|_| "inbound link source escapes vault")?
                .to_path_buf();
            patches.push(crate::journal::FilePatch {
                path: relative_path,
                expected_base_hash,
                bytes,
            });
        }

        let relative_from = from
            .strip_prefix(&self.identity.root)
            .map_err(|_| "source escapes vault")?;
        let relative_to = to
            .strip_prefix(&self.identity.root)
            .map_err(|_| "destination escapes vault")?;
        let txn = crate::journal::JournalExecutor::new(&*self.fs, &self.identity.root)
            .rename_with_patches(relative_from, relative_to, &patches)?;
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
        let _ = self.scan_paths(&changed);
        Ok(txn)
    }

    fn perform_initial_scan(&mut self) {
        self.handle_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent, None);
        if self.index.needs_reparse()
            && let Err(error) = self.index.mark_parser_current()
        {
            tracing::warn!(%error, "could not persist current parser version after verification");
        }
        self.initial_scan_complete = true;
    }

    fn scan_notes(&mut self) -> ReconcileSummary {
        self.scan_notes_mode(ReconcileMode::VerifyContent)
    }

    fn scan_notes_mode(&mut self, mode: ReconcileMode) -> ReconcileSummary {
        if let Ok((epoch, generation)) = self.index.current_version() {
            self.epoch = Epoch::new(epoch);
            self.generation = Generation::new(generation);
        }
        let total_notes = count_markdown_notes(&*self.fs, &self.identity.root);
        let mut processed_notes = 0u32;
        let mut stack = vec![self.identity.root.clone()];
        let mut summary = ReconcileSummary::default();
        let mut indexed: std::collections::HashSet<Vec<u8>> = self
            .index
            .indexed_note_paths()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut present = std::collections::HashSet::new();
        while let Some(directory) = stack.pop() {
            let Ok(entries) = self.fs.read_dir(&directory) else {
                continue;
            };
            for entry in entries {
                if entry.is_dir {
                    if !entry.is_symlink && entry.name != ".flavite" {
                        stack.push(entry.path);
                    }
                    continue;
                }
                if entry.is_symlink {
                    continue;
                }
                if !entry
                    .path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
                {
                    continue;
                }
                processed_notes = processed_notes.saturating_add(1);
                if (processed_notes.is_multiple_of(128) || processed_notes == total_notes)
                    && let Some(pulse) = &self.pulse_sender
                {
                    let _ = pulse.try_send(Pulse::ReconcileProgress {
                        done: processed_notes,
                        total: total_notes,
                    });
                }
                let Ok(relative) = entry.path.strip_prefix(&self.identity.root) else {
                    continue;
                };
                let Ok(path_key) = self.fs.encode_path_key(relative) else {
                    continue;
                };
                present.insert(path_key.clone());
                let Ok(metadata) = self.fs.metadata(&entry.path) else {
                    continue;
                };
                let scan_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .min(i64::MAX as u128) as i64;
                if mode == ReconcileMode::FastMetadata
                    && self
                        .index
                        .note_metadata_matches(&path_key, metadata.size, metadata.mtime_ns, scan_ns)
                        .unwrap_or(false)
                {
                    indexed.remove(&path_key);
                    continue;
                }
                let display_path = relative.to_string_lossy().replace('\\', "/");
                if metadata.size > MAX_NOTE_BYTES as u64 {
                    match self.index.apply_unparsed_note(
                        &path_key,
                        &display_path,
                        metadata.size,
                        metadata.mtime_ns,
                        scan_ns,
                    ) {
                        Ok((epoch, generation)) => {
                            let was_indexed = indexed.remove(&path_key);
                            let changed = generation > self.generation.0;
                            self.epoch = Epoch::new(epoch);
                            self.generation = Generation::new(generation);
                            if changed {
                                if was_indexed {
                                    summary.changed = summary.changed.saturating_add(1);
                                    self.record_drift();
                                } else {
                                    summary.added = summary.added.saturating_add(1);
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(path = %entry.path.display(), %error, "oversize note reconcile failed")
                        }
                    }
                    continue;
                }
                let Ok(bytes) = self.fs.read_capped(&entry.path, MAX_NOTE_BYTES as u64) else {
                    continue;
                };
                match self.index.apply_saved_note(
                    &path_key,
                    &display_path,
                    &bytes,
                    metadata.mtime_ns,
                    scan_ns,
                ) {
                    Ok((epoch, generation)) => {
                        let changed = generation > self.generation.0;
                        let was_indexed = indexed.remove(&path_key);
                        if changed && (was_indexed || self.initial_scan_complete) {
                            let _ = self.event_sender.send(VaultActorEvent::ExternalChange {
                                path: entry.path.clone(),
                                bytes: bytes.clone(),
                            });
                        }
                        self.epoch = Epoch::new(epoch);
                        self.generation = Generation::new(generation);
                        if changed {
                            if was_indexed {
                                summary.changed = summary.changed.saturating_add(1);
                                self.record_drift();
                            } else {
                                summary.added = summary.added.saturating_add(1);
                                if self.initial_scan_complete {
                                    self.drift_found_total.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(path = %entry.path.display(), %error, "note reconcile failed")
                    }
                }
            }
        }
        // A present but unreadable/oversized note is not a deletion.
        indexed.retain(|path| !present.contains(path));
        for vanished in indexed {
            match self.index.remove_note_path(&vanished) {
                Ok(true) => {
                    summary.removed = summary.removed.saturating_add(1);
                    self.record_drift();
                    if let Ok(relative) = self.fs.decode_path_key(&vanished) {
                        let _ = self.event_sender.send(VaultActorEvent::Deleted {
                            path: self.identity.root.join(relative),
                        });
                    }
                    // Keep actor-visible generation monotonic after removal.
                    self.generation = Generation::new(self.generation.0.saturating_add(1));
                }
                Ok(false) => {}
                Err(error) => tracing::warn!(%error, "note removal reconcile failed"),
            }
        }
        let attachments = self.scan_attachments(mode);
        summary.added = summary.added.saturating_add(attachments.added);
        summary.changed = summary.changed.saturating_add(attachments.changed);
        summary.removed = summary.removed.saturating_add(attachments.removed);
        summary
    }

    fn scan_attachments(&mut self, mode: ReconcileMode) -> ReconcileSummary {
        let indexed: std::collections::HashSet<Vec<u8>> = self
            .index
            .indexed_attachment_paths()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let mut still_present = std::collections::HashSet::new();
        let mut stack = vec![self.identity.root.clone()];
        let mut summary = ReconcileSummary::default();
        while let Some(directory) = stack.pop() {
            let Ok(entries) = self.fs.read_dir(&directory) else {
                continue;
            };
            for entry in entries {
                if entry.is_dir {
                    if !entry.is_symlink && entry.name != ".flavite" {
                        stack.push(entry.path);
                    }
                    continue;
                }
                if entry.is_symlink {
                    continue;
                }
                let Some(_kind) = attachment_kind(&entry.path) else {
                    continue;
                };
                let Ok(relative) = entry.path.strip_prefix(&self.identity.root) else {
                    continue;
                };
                let Ok(key) = self.fs.encode_path_key(relative) else {
                    continue;
                };
                still_present.insert(key.clone());
                let Ok(metadata) = self.fs.metadata(&entry.path) else {
                    continue;
                };
                let placeholder = self.fs.is_placeholder(&entry.path).unwrap_or(false);
                let scan_ns = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
                    .min(i64::MAX as u128) as i64;
                if mode == ReconcileMode::FastMetadata
                    && self
                        .index
                        .attachment_matches(
                            &key,
                            metadata.size,
                            metadata.mtime_ns,
                            None,
                            !placeholder,
                            scan_ns,
                        )
                        .unwrap_or(false)
                {
                    continue;
                }
                let hash_cap = flavite_index::types::ATTACHMENT_HASH_MAX as u64;
                let hash = if !placeholder && metadata.size <= hash_cap {
                    self.fs
                        .read_capped(&entry.path, hash_cap)
                        .ok()
                        .map(|bytes| *blake3::hash(&bytes).as_bytes())
                } else {
                    None
                };
                let hydrated = !placeholder && (metadata.size > hash_cap || hash.is_some());
                if self
                    .index
                    .attachment_matches(
                        &key,
                        metadata.size,
                        metadata.mtime_ns,
                        hash,
                        hydrated,
                        scan_ns,
                    )
                    .unwrap_or(false)
                {
                    continue;
                }
                let display = relative.to_string_lossy().replace('\\', "/");
                let bytes = if hydrated && metadata.size <= hash_cap {
                    self.fs.read_capped(&entry.path, hash_cap).ok()
                } else {
                    None
                };
                match self.index.apply_saved_attachment(
                    &key,
                    &display,
                    bytes.as_deref(),
                    metadata.size,
                    metadata.mtime_ns,
                    scan_ns,
                    hydrated,
                ) {
                    Ok((_, generation)) => {
                        self.generation = Generation::new(generation);
                        if indexed.contains(&key) {
                            summary.changed = summary.changed.saturating_add(1);
                            self.drift_found_total.fetch_add(1, Ordering::Relaxed);
                        } else {
                            summary.added = summary.added.saturating_add(1);
                            if self.initial_scan_complete {
                                self.drift_found_total.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(path = %entry.path.display(), %error, "attachment reconcile failed")
                    }
                }
            }
        }
        for key in indexed
            .into_iter()
            .filter(|key| !still_present.contains(key))
        {
            if self.index.remove_attachment_path(&key).unwrap_or(false) {
                summary.removed = summary.removed.saturating_add(1);
                self.drift_found_total.fetch_add(1, Ordering::Relaxed);
            }
        }
        summary
    }

    /// Verify just the paths nominated by watcher events. A directory or an
    /// unusable path falls back to a full scan so hints can never hide drift.
    fn scan_paths(
        &mut self,
        paths: &std::collections::HashSet<std::path::PathBuf>,
    ) -> ReconcileSummary {
        if paths.is_empty() || paths.iter().any(|path| path == std::path::Path::new(".")) {
            return self.scan_notes();
        }
        let mut summary = ReconcileSummary::default();
        for nominated in paths {
            let path = if nominated.is_absolute() {
                nominated.clone()
            } else {
                self.identity.root.join(nominated)
            };
            if !path.starts_with(&self.identity.root) {
                continue;
            }
            if !path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("md"))
            {
                if attachment_kind(&path).is_some() {
                    // Attachment changes participate in the same resolver/index transaction.
                    return self.scan_notes_mode(ReconcileMode::VerifyContent);
                }
                continue;
            }
            let Ok(relative) = path.strip_prefix(&self.identity.root) else {
                continue;
            };
            let Ok(path_key) = self.fs.encode_path_key(relative) else {
                continue;
            };
            let exists = match self.fs.exists(&path) {
                Ok(exists) => exists,
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "cannot verify nominated path");
                    continue;
                }
            };
            if !exists {
                if self.index.remove_note_path(&path_key).unwrap_or(false) {
                    summary.removed = summary.removed.saturating_add(1);
                    self.record_drift();
                    let _ = self
                        .event_sender
                        .send(VaultActorEvent::Deleted { path: path.clone() });
                    if let Ok((epoch, generation)) = self.index.current_version() {
                        self.epoch = Epoch::new(epoch);
                        self.generation = Generation::new(generation);
                    }
                }
                continue;
            }
            if self
                .fs
                .validate_in_root(&self.identity.root, &path)
                .is_err()
            {
                continue;
            }
            let Ok(metadata) = self.fs.metadata(&path) else {
                continue;
            };
            if metadata.is_dir {
                return self.scan_notes();
            }
            if metadata.is_symlink {
                continue;
            }
            let was_indexed = self.index.contains_note_path(&path_key).unwrap_or(false);
            let before = self
                .index
                .current_version()
                .map(|(_, generation)| generation)
                .unwrap_or(self.generation.0);
            let scan_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
                .min(i64::MAX as u128) as i64;
            let display_path = relative.to_string_lossy().replace('\\', "/");
            if metadata.size > MAX_NOTE_BYTES as u64 {
                match self.index.apply_unparsed_note(
                    &path_key,
                    &display_path,
                    metadata.size,
                    metadata.mtime_ns,
                    scan_ns,
                ) {
                    Ok((epoch, generation)) => {
                        self.epoch = Epoch::new(epoch);
                        self.generation = Generation::new(generation);
                        if generation > before {
                            if was_indexed {
                                summary.changed = summary.changed.saturating_add(1);
                                self.record_drift();
                            } else {
                                summary.added = summary.added.saturating_add(1);
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(path = %path.display(), %error, "oversize note reconcile failed")
                    }
                }
                continue;
            }
            let Ok(bytes) = self.fs.read_capped(&path, MAX_NOTE_BYTES as u64) else {
                continue;
            };
            match self.index.apply_saved_note(
                &path_key,
                &display_path,
                &bytes,
                metadata.mtime_ns,
                scan_ns,
            ) {
                Ok((epoch, generation)) => {
                    self.epoch = Epoch::new(epoch);
                    self.generation = Generation::new(generation);
                    if generation > before {
                        if was_indexed {
                            summary.changed = summary.changed.saturating_add(1);
                            self.record_drift();
                        } else {
                            summary.added = summary.added.saturating_add(1);
                        }
                        if was_indexed || self.initial_scan_complete {
                            let _ = self.event_sender.send(VaultActorEvent::ExternalChange {
                                path: path.clone(),
                                bytes,
                            });
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(path = %path.display(), %error, "nominated note reconcile failed")
                }
            }
        }
        summary
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

fn count_markdown_notes(fs: &dyn Fs, root: &std::path::Path) -> u32 {
    let mut count = 0u32;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = fs.read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            if entry.is_dir {
                if !entry.is_symlink && entry.name != ".flavite" {
                    stack.push(entry.path);
                }
            } else if !entry.is_symlink
                && entry
                    .path
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            {
                count = count.saturating_add(1);
            }
        }
    }
    count
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
        Ok(actor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::WriteTokenManager;
    use flavite_fs::{Fs, MemFs};

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

        let temp = tempfile::NamedTempFile::new().unwrap();
        drop(flavite_index::open_or_init(temp.path()).unwrap());
        let index = IndexActor::new(temp.path(), 1).unwrap();
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
        fs.write_new(std::path::Path::new("/vault/notes/a.md"), b"# A")
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
        actor.get_version().unwrap();

        let response = actor
            .submit_rename(
                std::path::PathBuf::from("/vault/notes/a.md"),
                std::path::PathBuf::from("/vault/notes/b.md"),
                Vec::new(),
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
            Some(b"# A".to_vec())
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
        actor.get_version().unwrap();
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
        let _ = pulse_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        assert!(actor.get_version().unwrap().generation.0 > initial);
        assert_eq!(actor.drift_found_total(), 1);
        assert!(
            matches!(actor.events().try_recv(), Ok(VaultActorEvent::ExternalChange { bytes, .. }) if bytes == b"after")
        );
        let _ = actor.send(VaultActorMsg::Shutdown);
    }
}
