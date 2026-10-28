#![forbid(unsafe_code)]
#![allow(dead_code)] // Stub implementation for C8

//! Tier 1 — durability-critical. Headless `Session` owning `BufferManager`
//! and pending operations; `VaultActor` (watcher, write tokens, journal,
//! history); `IndexActor` (sole SQLite + tantivy writer, read pool);
//! `ParsePool` (§4.1, §4.2, §4.5, D3, D6, D9).

pub mod actor;
pub mod channels;
pub mod coalesce;
pub mod history;
pub mod identity;
pub mod instance;
pub mod merge;
pub mod open;
pub mod reconcile;
pub mod save;
pub mod session;
pub mod tokens;
pub mod types;

// Re-export key types for convenience.
pub use actor::{
    VaultActor, VaultActorBuilder, VaultActorError, VaultActorHandle, VaultActorMsg, VaultActorResp,
};
pub use channels::{
    ChannelSet, Notice, NoticeChannel, NoticeKey, NoticeKind, Pulse, PulseChannel, RenameTier,
};
pub use coalesce::{
    CoalescerConfig, CoalescerEvent, EventBatch, EventCoalescer, VaultWatcher, WatcherEvent,
};
pub use history::{HistoryError, HistoryReason, HistoryStore, SnapshotRef};
pub use identity::{
    IdentityError, IdentityManager, IdentityResult, RegistryEntry, compute_root_key,
    ensure_flavite_dir,
};
pub use instance::{
    AdvisoryLock, FocusIpc, InstanceError, InstanceManager, InstanceResult, is_instance_running,
};
pub use merge::{MergeConflict, MergeResult, three_way_merge};
pub use open::{OpenVaultError, OpenVaultResult, Vault, vault_exists};
pub use reconcile::{ReconcileConfig, ReconcileContext, ReconcileResult, verify_content_full};
pub use save::{
    DiskPrecondition, DurabilityOutcome, SaveFailure, SaveFailureStage, SaveReceipt, SaveRequest,
    save,
};
pub use session::{
    CommandResult, ConflictResolution, Session, SessionBuilder, SessionCommand, SessionError,
    SessionEvent, SessionResult,
};
pub use tokens::{TokenGuard, WriteToken, WriteTokenManager};
pub use types::{
    AtomicVersion, ChangeKind, Epoch, Generation, ReconcileMode, ReconcileScope, ReconcileSummary,
    VaultConfig, VaultRef, Version,
};

/// Placeholder for Index - will be implemented in flavite-index
/// This is just to allow compilation of vault code
#[derive(Debug, Clone, Default)]
pub struct IndexPlaceholder;

impl IndexPlaceholder {
    /// Get notes in scope - stub
    pub fn get_notes_in_scope(
        &self,
        _scope: &crate::types::ReconcileScope,
    ) -> std::collections::HashMap<std::path::PathBuf, crate::reconcile::FileEntry> {
        std::collections::HashMap::new()
    }

    /// Apply a note to the index - stub
    pub fn apply_note(
        &mut self,
        _path: &std::path::Path,
        _snapshot: &flavite_syntax::text::TextSnapshot,
        _scan_time_ns: i64,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Update a note in the index - stub
    pub fn update_note(
        &mut self,
        _path: &std::path::Path,
        _snapshot: &flavite_syntax::text::TextSnapshot,
        _scan_time_ns: i64,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Remove a note from the index - stub
    pub fn remove_note(&mut self, _path: &std::path::Path) -> Result<(), String> {
        Ok(())
    }
}
