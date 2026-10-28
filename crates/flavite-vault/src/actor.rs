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

use crate::IndexPlaceholder;
use crate::channels::{NoticeChannel, PulseChannel};
use crate::coalesce::{CoalescerConfig, EventCoalescer, VaultWatcher, WatcherEvent};
use crate::reconcile::{ReconcileConfig, ReconcileState};
use crate::tokens::WriteTokenManager;
use crate::types::{
    ReconcileMode, ReconcileScope, ReconcileSummary, VaultConfig, VaultIdentity, Version,
};
use crossbeam::channel::{Receiver, Sender, unbounded};
use flavite_fs::Fs;
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
    /// Notify of a filesystem event.
    FsEvent(WatcherEvent),
    /// Get the current version.
    GetVersion,
    /// Get vault identity.
    GetIdentity,
    /// Shutdown.
    Shutdown,
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
        self.sender
            .try_send(msg)
            .map_err(|_| VaultActorError::ChannelDisconnected)
    }

    /// Get the current version.
    pub fn get_version(&self) -> Result<Version, VaultActorError> {
        match self.send(VaultActorMsg::GetVersion)? {
            VaultActorResp::Version(v) => Ok(v),
            VaultActorResp::Error(_e) => Err(VaultActorError::NotRunning),
            _ => Err(VaultActorError::NotRunning),
        }
    }
}

/// The VaultActor itself.
pub struct VaultActor {
    /// The filesystem.
    fs: Box<dyn Fs + Send + Sync>,
    /// Vault identity.
    identity: VaultIdentity,
    /// The index.
    index: IndexPlaceholder,
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
        _index: IndexPlaceholder,
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

        Self {
            fs,
            identity,
            index: IndexPlaceholder,
            config,
            coalescer,
            _watcher,
            pulse,
            notice,
            token_manager: WriteTokenManager::new(),
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
                self.handle_reconcile(scope, mode);
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
        }
    }

    /// Handle a reconcile request.
    fn handle_reconcile(&mut self, _scope: ReconcileScope, _mode: ReconcileMode) {
        // For now, just return a dummy result
        // In production, this would call the actual reconcile logic
        let summary = ReconcileSummary::default();
        let version = Version::new(self.reconcile_state.epoch, self.reconcile_state.generation);

        let _ = self
            .resp_sender
            .try_send(VaultActorResp::ReconcileComplete { summary, version });
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
    index: Option<IndexPlaceholder>,
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
    pub fn with_index(mut self, index: IndexPlaceholder) -> Self {
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
            self.index.unwrap_or_default(),
            self.config.unwrap_or_default(),
            PulseChannel::new(1024),
            NoticeChannel::new(std::time::Duration::from_millis(300)),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_fs::MemFs;

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

        let actor = VaultActorBuilder::new()
            .with_fs(fs)
            .with_identity(identity)
            .with_index(IndexPlaceholder)
            .build()
            .unwrap();

        // The actor should be created but not started
        assert!(!actor.is_running.load(Ordering::SeqCst));
    }
}
