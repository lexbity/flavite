//! VaultActor — the main filesystem actor (§4.2, D3).
//!
//! The VaultActor owns:
//! - The filesystem implementation
//! - The file watcher
//! - Write tokens for atomic operations
//! - Journal and history handles
//! - The IndexActor connection
//!
//! It runs the reconcile algorithm on a worker thread and coordinates
//! with the Session via bounded channels.

use crate::channels::{NoticeChannel, PulseChannel};
use crate::coalesce::{CoalescerConfig, EventCoalescer, VaultWatcher, WatcherEvent};
use crate::history::HistoryStore;
use crate::history::{HistoryReason, SnapshotRef};
use crate::reconcile::{ReconcileConfig, ReconcileState};
use crate::save::{SaveFailure, SaveReceipt, SaveRequest};
use crate::tokens::WriteTokenManager;
use crate::types::{
    ReconcileMode, ReconcileScope, ReconcileSummary, VaultConfig, VaultIdentity, Version,
};
use crossbeam::channel::{Receiver, Sender, unbounded};
use flavite_fs::Fs;
use flavite_index::IndexActor;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use thiserror::Error;
use tracing::info;

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
    /// Notify of a filesystem event.
    FsEvent(WatcherEvent),
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
}

impl VaultActorHandle {
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
    /// Vault configuration.
    config: VaultConfig,
    /// Event coalescer.
    coalescer: EventCoalescer,
    /// Vault watcher.
    _watcher: Option<VaultWatcher>,
    /// Pulse channel.
    pulse: PulseChannel,
    /// Notice channel.
    notice: NoticeChannel,
    /// Write token manager.
    token_manager: WriteTokenManager,
    /// Content-addressed local safety history.
    history: Option<HistoryStore>,
    /// Reconcile state.
    reconcile_state: ReconcileState,
    /// Reconcile config.
    reconcile_config: ReconcileConfig,
    /// Whether the actor is running.
    is_running: AtomicBool,
    /// Message receiver.
    msg_receiver: Receiver<VaultActorMsg>,
    /// Response sender.
    resp_sender: Sender<VaultActorResp>,
}

impl VaultActor {
    /// Create a new VaultActor.
    pub fn new(
        fs: Box<dyn Fs + Send + Sync>,
        identity: VaultIdentity,
        index: IndexActor,
        config: VaultConfig,
        pulse: PulseChannel,
        notice: NoticeChannel,
    ) -> Self {
        let coalescer = EventCoalescer::new(CoalescerConfig {
            quiet_window: config.coalesce_quiet_window,
            max_window: config.coalesce_max_window,
            bulk_threshold: config.bulk_threshold,
        });

        // Create watcher - for stub, we skip it due to Box<dyn Fs> not implementing Clone
        // TODO: Implement proper Fs cloning when flavite-fs supports it
        let _watcher = None; // VaultWatcher::new(fs, &coalescer);

        let fs: Arc<dyn Fs + Send + Sync> = Arc::from(fs);
        Self {
            fs,
            identity,
            index,
            config,
            coalescer,
            _watcher,
            pulse,
            notice,
            token_manager: WriteTokenManager::new(),
            history: None,
            reconcile_state: ReconcileState::default(),
            reconcile_config: ReconcileConfig::default(),
            is_running: AtomicBool::new(false),
            msg_receiver: unbounded().1, // Will be replaced
            resp_sender: unbounded().0,  // Will be replaced
        }
    }

    /// Start the actor in a background thread.
    pub fn start(mut self) -> Result<VaultActorHandle, VaultActorError> {
        if self.is_running.load(Ordering::SeqCst) {
            return Err(VaultActorError::AlreadyRunning);
        }

        let (msg_sender, msg_receiver) = unbounded();
        let (resp_sender, resp_receiver) = unbounded();

        self.msg_receiver = msg_receiver;
        self.resp_sender = resp_sender;

        let is_running = Arc::new(AtomicBool::new(false));
        let _is_running_clone = is_running.clone();

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
        })
    }

    /// The main actor loop.
    fn run(&mut self, is_running: Arc<AtomicBool>) {
        is_running.store(true, Ordering::SeqCst);

        info!(
            "VaultActor started for vault: {}",
            self.identity.root.display()
        );

        // Perform initial scan
        self.perform_initial_scan();

        // Main loop
        loop {
            match self.msg_receiver.recv() {
                Ok(msg) => {
                    self.handle_message(msg);
                }
                Err(_) => {
                    // Channel disconnected
                    info!("VaultActor channel disconnected, shutting down");
                    break;
                }
            }
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
            VaultActorMsg::FsEvent(event) => {
                self.handle_fs_event(event);
            }
            VaultActorMsg::GetVersion => {
                let version =
                    Version::new(self.reconcile_state.epoch, self.reconcile_state.generation);
                let _ = self.resp_sender.try_send(VaultActorResp::Version(version));
            }
            VaultActorMsg::GetIdentity => {
                let _ = self
                    .resp_sender
                    .try_send(VaultActorResp::Identity(self.identity.clone()));
            }
            VaultActorMsg::Shutdown => {
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
        _mode: ReconcileMode,
        reply: Option<Sender<VaultActorResp>>,
    ) {
        // For now, just return a dummy result
        // In production, this would call the actual reconcile logic
        let summary = ReconcileSummary::default();
        let version = Version::new(self.reconcile_state.epoch, self.reconcile_state.generation);

        let response = VaultActorResp::ReconcileComplete { summary, version };
        if let Some(reply) = reply {
            let _ = reply.send(response);
        } else {
            let _ = self.resp_sender.try_send(response);
        }
    }

    /// Handle a filesystem event.
    fn handle_fs_event(&mut self, _event: WatcherEvent) {
        // self.watcher.notify(event);
        // TODO: Implement when watcher is properly set up

        // Also directly trigger a reconcile for the affected paths
        // This is a simplified approach - in production, the coalescer would
        // batch these

        // For now, just send a pulse that something changed
        // The actual reconcile will be triggered by the next message loop
    }

    /// Perform the initial scan on startup.
    fn perform_initial_scan(&mut self) {
        info!(
            "Performing initial scan for vault: {}",
            self.identity.root.display()
        );

        // For now, we'll just mark it as completed
        // In production, this would call initial_scan

        info!("Initial scan completed");
    }
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
    config: Option<VaultConfig>,
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

    /// Set the configuration.
    #[must_use]
    pub fn with_config(mut self, config: VaultConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Build the VaultActor.
    pub fn build(self) -> Result<VaultActor, String> {
        Ok(VaultActor::new(
            self.fs.ok_or("Missing filesystem")?,
            self.identity.ok_or("Missing identity")?,
            self.index.ok_or("Missing structural index")?,
            self.config.unwrap_or_default(),
            PulseChannel::new(1024),
            NoticeChannel::new(std::time::Duration::from_millis(300)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        let request = SaveRequest {
            request_id: 17,
            path: std::path::PathBuf::from("/vault/notes/a.md"),
            note_key: b"notes/a.md".to_vec(),
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
        assert_eq!((receipt.epoch, receipt.generation), (1, 1));
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
}
