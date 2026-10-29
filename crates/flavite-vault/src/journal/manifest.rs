//! Journal manifest format and versioning (§4.15).

use flavite_buffer::revision::BufferId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Current journal manifest version.
pub const MANIFEST_VERSION: u32 = 2;

/// Kind of work represented by a journal item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionItemKind {
    /// Rename or move a file.
    Rename,
    /// Replace bytes in an existing file.
    Patch,
    /// Add an alias to a renamed note.
    Alias,
}

/// Transaction phase (§4.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    /// Transaction is being planned.
    Plan,
    /// Preconditions are being verified.
    Verify,
    /// Durable manifest and payloads are being prepared.
    Prepare,
    /// Temporary files are being staged.
    Stage,
    /// Transaction is committed (roll-forward point).
    Commit,
    /// Files are being replaced on disk.
    Apply,
    /// Waiting for buffer patches to be applied.
    WaitBuffers,
    /// Transaction is closed.
    Close,
}

/// Item state within a transaction (§4.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionItemState {
    /// Item has not started.
    Pending,
    /// Item is prepared (manifest written).
    Prepared,
    /// Item payload is staged.
    Staged,
    /// Item is committed.
    Committed,
    /// Item file is applied on disk.
    FileApplied,
    /// Item buffer patch is applied.
    BufferApplied,
    /// Item is saved.
    Saved,
    /// Item was skipped.
    Skipped,
    /// Item was cancelled.
    Cancelled,
}

/// Overall transaction state (§4.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionState {
    /// Transaction is in progress.
    Preparing,
    /// Transaction is committed and ready for apply.
    Committed,
    /// Transaction is partially applied.
    Partial,
    /// Transaction is complete.
    Complete,
    /// Transaction was abandoned.
    Abandoned,
}

/// A single item in a rename/move transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionItem {
    /// Unique item ID within the transaction.
    pub id: u64,
    /// Kind of transaction item.
    pub kind: TransactionItemKind,
    /// Source path (for rename/move).
    pub from_path: PathBuf,
    /// Destination path (for rename/move).
    pub to_path: PathBuf,
    /// Current phase.
    pub phase: TransactionPhase,
    /// Current state.
    pub state: TransactionItemState,
    /// Expected base hash for verification.
    pub expected_base_hash: Option<[u8; 32]>,
    /// Expected new hash after apply.
    pub expected_new_hash: Option<[u8; 32]>,
    /// Actual observed base hash (for external divergence detection).
    pub observed_base_hash: Option<[u8; 32]>,
    /// Actual observed new hash (for external divergence detection).
    pub observed_new_hash: Option<[u8; 32]>,
    /// Buffer ID for open buffers.
    pub buffer_id: Option<BufferId>,
    /// Buffer revision for open buffers.
    pub buffer_revision: Option<u64>,
    /// Checkpoint sequence for buffer recovery.
    pub checkpoint_seq: Option<u64>,
    /// Operation markers for checkpoint lineage.
    pub operation_markers: Vec<u64>,
    /// Original link/embed stem for a located buffer patch.
    #[serde(default)]
    pub old_stem: Option<String>,
    /// Replacement link/embed stem for a located buffer patch.
    #[serde(default)]
    pub new_stem: Option<String>,
    /// Folded target key used to locate current-buffer links.
    #[serde(default)]
    pub target_key: Option<String>,
    /// Indexed source hash used to reject stale buffer patches.
    #[serde(default)]
    pub indexed_hash: Option<[u8; 32]>,
    /// Native note key used to locate the recovery file for R7 comparison.
    #[serde(default)]
    pub recovery_key: Option<Vec<u8>>,
    /// Error message if the item failed.
    pub error: Option<String>,
}

/// The complete journal manifest for a transaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalManifest {
    /// Manifest version.
    pub version: u32,
    /// Monotonic sequence number.
    pub seq: u64,
    /// Transaction ID.
    pub txn_id: String,
    /// Transaction state.
    pub state: TransactionState,
    /// Native path endpoints (for validation).
    pub native_paths: Vec<PathBuf>,
    /// Items in this transaction.
    pub items: Vec<TransactionItem>,
    /// Timestamp when the manifest was created.
    pub created_timestamp: u64,
    /// Timestamp when the manifest was committed.
    pub committed_timestamp: Option<u64>,
    /// Transaction metadata.
    pub metadata: HashMap<String, String>,
}

impl JournalManifest {
    /// Create a new manifest for a transaction.
    pub fn new(txn_id: String, seq: u64) -> Self {
        Self {
            version: MANIFEST_VERSION,
            seq,
            txn_id,
            state: TransactionState::Preparing,
            native_paths: Vec::new(),
            items: Vec::new(),
            created_timestamp: now_secs(),
            committed_timestamp: None,
            metadata: HashMap::new(),
        }
    }

    /// Get the path for this manifest file.
    pub fn manifest_path(&self, journal_dir: &std::path::Path) -> PathBuf {
        journal_dir.join(format!("{}.json", self.txn_id))
    }

    /// Get the data directory for this transaction.
    pub fn data_dir(&self, journal_dir: &std::path::Path) -> PathBuf {
        journal_dir.join(format!("{}.data", self.txn_id))
    }

    /// Check if the manifest is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.state,
            TransactionState::Partial | TransactionState::Complete | TransactionState::Abandoned
        )
    }

    /// Check if all items are in a terminal state.
    pub fn all_items_terminal(&self) -> bool {
        self.items.iter().all(|item| {
            matches!(
                item.state,
                TransactionItemState::Saved
                    | TransactionItemState::Skipped
                    | TransactionItemState::Cancelled
            )
        })
    }

    /// Get items that are not in a terminal state.
    pub fn pending_items(&self) -> Vec<&TransactionItem> {
        self.items
            .iter()
            .filter(|item| {
                !matches!(
                    item.state,
                    TransactionItemState::Saved
                        | TransactionItemState::Skipped
                        | TransactionItemState::Cancelled
                )
            })
            .collect()
    }
}

/// Get current timestamp in seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn manifest_creation() {
        let manifest = JournalManifest::new("txn-001".to_string(), 1);
        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.seq, 1);
        assert_eq!(manifest.txn_id, "txn-001");
        assert_eq!(manifest.state, TransactionState::Preparing);
    }

    #[test]
    fn manifest_paths() {
        let manifest = JournalManifest::new("txn-001".to_string(), 1);
        let journal_dir = Path::new("/test/vault/.flavite/journal");

        let manifest_path = manifest.manifest_path(journal_dir);
        assert_eq!(
            manifest_path,
            Path::new("/test/vault/.flavite/journal/txn-001.json")
        );

        let data_dir = manifest.data_dir(journal_dir);
        assert_eq!(
            data_dir,
            Path::new("/test/vault/.flavite/journal/txn-001.data")
        );
    }

    #[test]
    fn terminal_state_and_pending_items_track_partial_progress() {
        let mut manifest = JournalManifest::new("txn-status".into(), 2);
        let item = |id, state| TransactionItem {
            id,
            kind: TransactionItemKind::Rename,
            from_path: PathBuf::from(format!("{id}-old.md")),
            to_path: PathBuf::from(format!("{id}-new.md")),
            phase: TransactionPhase::Apply,
            state,
            expected_base_hash: None,
            expected_new_hash: None,
            observed_base_hash: None,
            observed_new_hash: None,
            buffer_id: None,
            buffer_revision: None,
            checkpoint_seq: None,
            operation_markers: Vec::new(),
            old_stem: None,
            new_stem: None,
            target_key: None,
            indexed_hash: None,
            recovery_key: None,
            error: None,
        };
        manifest.items = vec![
            item(1, TransactionItemState::Committed),
            item(2, TransactionItemState::Saved),
            item(3, TransactionItemState::Skipped),
            item(4, TransactionItemState::Cancelled),
        ];
        assert!(!manifest.is_terminal());
        assert!(!manifest.all_items_terminal());
        assert_eq!(manifest.pending_items().len(), 1);
        manifest.items[0].state = TransactionItemState::Saved;
        assert!(manifest.all_items_terminal());
        manifest.state = TransactionState::Complete;
        assert!(manifest.is_terminal());
        manifest.state = TransactionState::Abandoned;
        assert!(manifest.is_terminal());
    }
}
