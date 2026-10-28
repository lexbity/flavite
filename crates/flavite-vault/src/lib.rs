#![forbid(unsafe_code)]

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
pub mod journal;
pub mod merge;
pub mod open;
pub mod rename;
pub mod save;
pub mod session;
pub mod tokens;
pub mod types;

// Re-export key types for convenience.
pub use actor::{
    EventSource, VaultActor, VaultActorBuilder, VaultActorError, VaultActorEvent, VaultActorHandle,
    VaultActorMsg, VaultActorResp, event_source_channel,
};
pub use channels::{
    ChannelSet, Notice, NoticeChannel, NoticeKey, NoticeKind, Pulse, PulseChannel, RenameTier,
};
pub use coalesce::{
    Clock, CoalescerConfig, CoalescerEvent, EventBatch, EventCoalescer, VaultWatcher, WatcherEvent,
};
pub use history::{HistoryError, HistoryReason, HistoryStore, SnapshotRef};
pub use identity::{
    IdentityError, IdentityManager, IdentityResult, RegistryEntry, compute_root_key,
    ensure_flavite_dir,
};
pub use instance::{
    AdvisoryLock, FocusIpc, InstanceError, InstanceManager, InstanceResult, is_instance_running,
};
pub use journal::executor::{FilePatch, JournalExecutor, RecoveryReport};
pub use journal::manifest::{
    JournalManifest, MANIFEST_VERSION, TransactionItem, TransactionItemState, TransactionPhase,
    TransactionState,
};
pub use journal::recover::{
    JournalRecovery, JournalRecoveryError, JournalRecoveryOutcome, JournalRecoveryResult,
};
pub use merge::{MergeConflict, MergeResult, three_way_merge};
pub use open::{OpenVaultError, OpenVaultResult, Vault, vault_exists};
pub use rename::{
    AttachmentRename, AttachmentRenameConfig, AttachmentRenameManager, AttachmentRenameResult,
    DuplicateIdInfo, DuplicateIdManager, DuplicateIdResolution, EmbedCountProvider,
    InboundLinkProvider, RenameBufferPatcher, RenameCandidate, RenameInference,
    RenameInferenceConfig, RenameInferenceResult, RenamePlan, RenamePlanItem, RenamePlanResult,
    RenamePlanner, RenameUndo, RenameUndoResult, RenameValidationError, can_use_t2_identity,
    should_auto_rewrite_links, should_carry_identity, should_rewrite_embeds,
};
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
