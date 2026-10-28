//! Undo handling for rename transactions (D9, §4.15).

use flavite_buffer::manager::BufferManager;
use std::sync::{Arc, Mutex};

/// Undo handler for rename transactions.
pub struct RenameUndo;

/// Result of undoing a rename.
#[derive(Debug, Clone)]
pub enum RenameUndoResult {
    /// Undo was successful.
    Success,
    /// Undo failed.
    Failed(String),
}

impl RenameUndo {
    /// Apply undo for a rename transaction.
    ///
    /// Stub implementation for now.
    pub fn apply_undo(
        _buffer_manager: &Arc<Mutex<BufferManager>>,
        _item: &crate::journal::manifest::TransactionItem,
        _txn_id: &str,
    ) -> Result<RenameUndoResult, String> {
        // Stub - will be implemented when rename is fully integrated
        Ok(RenameUndoResult::Success)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::manifest::{TransactionItem, TransactionItemState, TransactionPhase};
    use std::path::PathBuf;

    #[test]
    fn undo_stub_accepts_transaction_context() {
        let manager = Arc::new(Mutex::new(BufferManager::new()));
        let item = TransactionItem {
            id: 1,
            from_path: PathBuf::from("old.md"),
            to_path: PathBuf::from("new.md"),
            phase: TransactionPhase::Apply,
            state: TransactionItemState::FileApplied,
            expected_base_hash: None,
            expected_new_hash: None,
            observed_base_hash: None,
            observed_new_hash: None,
            buffer_id: None,
            buffer_revision: None,
            checkpoint_seq: None,
            operation_markers: Vec::new(),
            error: None,
        };
        assert!(matches!(
            RenameUndo::apply_undo(&manager, &item, "txn-1"),
            Ok(RenameUndoResult::Success)
        ));
    }
}
