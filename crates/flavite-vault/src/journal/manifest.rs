//! Journal manifest format and versioning (§4.15).

use flavite_buffer::revision::BufferId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

/// Current journal manifest version.
pub const MANIFEST_VERSION: u32 = 1;

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
    /// Expected identities/hashes (for validation).
    pub expected_identities: HashMap<String, [u8; 32]>,
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
            expected_identities: HashMap::new(),
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
            TransactionState::Complete | TransactionState::Abandoned
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
}
