//! Vault source Find and sealed replacement preview (RFC-0006 S07).
//!
//! S07 is the complete read-only inspection capability: exact source
//! matching across a frozen note-catalog scope (D-08), per-note guarded
//! reads, typed exclusions, sealed before/after payloads with a selection
//! revision (D-09). Destructive Apply/Cancel/Resume/Revert/forget are
//! S08/S11 commands and are never exposed here.

pub mod context;
pub mod coordinator;
pub mod log;
pub mod plan;
pub mod preview;
pub mod store;

pub use crate::operation_key::{KeyStoreError, OperationKey, load_or_create_key};
pub use coordinator::{
    CoordinatorCtx, ItemEffect, JobItem, JobPhase, JobTotals, ReplaceItemOutcome,
    ReplaceItemRequest, read_draft_state, run_apply, run_revert, write_draft_state,
};
pub use log::{
    GENESIS_AUTH, ItemAction, JobLog, LogError, LogRecord, MAX_LOG_FRAME_BYTES,
    PersistedItemOutcome, PersistedRevertOutcome,
};
pub use plan::{
    DraftLineage, ENGINE_VERSION, ExclusionReason, MAX_PREVIEW_PAYLOAD_BYTES, MAX_SCOPE_NOTES,
    MAX_SOURCE_MATCHES, NoteCandidateRecord, NoteExclusion, OpenLineage, PreviewManifest,
    PreviewTotals, ScopeVersion, SealedPayloadRef, SourceKind,
};
pub use preview::{
    PreviewError, PreviewRequest, SourceFindOutcome, SourceFindRequest, run_find, run_preview,
};
pub use store::{OpStoreIo, ReplaceStore, StoreError};

/// Bounded contained operation-store I/O through the vault actor. All paths
/// are validated inside the vault's private `.flavite/operations` namespace
/// by the actor; UI code never reaches files directly (gated).
#[derive(Debug, Clone)]
pub struct ActorOpStoreIo {
    actor: crate::actor::VaultActorHandle,
}

impl ActorOpStoreIo {
    /// The actor-backed I/O used by vault preview workers.
    #[must_use]
    pub fn new(actor: crate::actor::VaultActorHandle) -> Self {
        Self { actor }
    }
}

impl OpStoreIo for ActorOpStoreIo {
    fn mkdir_all(&self, path: &std::path::Path) -> Result<(), String> {
        self.actor.op_store_mkdir_all(path.to_path_buf())
    }

    fn write_new(&self, path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
        self.actor
            .op_store_write_new(path.to_path_buf(), bytes.to_vec())
    }

    fn write_replace(&self, path: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
        self.actor
            .op_store_write_replace(path.to_path_buf(), bytes.to_vec())
    }

    fn read(&self, path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
        self.actor.op_store_read(path.to_path_buf())
    }

    fn list_dir(&self, path: &std::path::Path) -> Result<Option<Vec<String>>, String> {
        self.actor.op_store_list_dir(path.to_path_buf())
    }

    fn fsync_file(&self, path: &std::path::Path) -> Result<(), String> {
        self.actor.op_store_fsync_file(path.to_path_buf())
    }

    fn fsync_dir(&self, path: &std::path::Path) -> Result<(), String> {
        self.actor.op_store_fsync_dir(path.to_path_buf())
    }

    fn remove(&self, path: &std::path::Path) -> Result<(), String> {
        self.actor.op_store_remove(path.to_path_buf())
    }
}
