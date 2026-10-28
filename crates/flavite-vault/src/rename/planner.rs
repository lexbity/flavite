//! Rename/move planner (D9, §4.15).

use flavite_buffer::revision::BufferId;
use flavite_fs::Fs;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use thiserror::Error;

/// Result of rename planning.
#[derive(Debug, Clone)]
pub enum RenamePlanResult {
    /// Plan was created successfully.
    Success(RenamePlan),
    /// Validation failed.
    ValidationFailed(RenameValidationError),
    /// Destination is occupied.
    DestinationOccupied(PathBuf),
    /// Cross-filesystem move not supported.
    CrossFilesystemNotSupported,
    /// Directory move not supported.
    DirectoryMoveNotSupported,
    /// Disk space insufficient.
    DiskSpaceInsufficient,
    /// Payload budget exceeded.
    PayloadBudgetExceeded,
}

/// Error during rename validation.
#[derive(Debug, Clone, Error)]
pub enum RenameValidationError {
    #[error("source does not exist")]
    SourceNotFound,
    #[error("source is not a file")]
    SourceNotFile,
    #[error("destination parent does not exist")]
    DestinationParentNotFound,
    #[error("path escapes vault root")]
    PathEscapesVault,
}

/// A rename/move plan.
#[derive(Debug, Clone)]
pub struct RenamePlan {
    /// Transaction ID.
    pub txn_id: String,
    /// Plan items.
    pub items: Vec<RenamePlanItem>,
    /// Total estimated payload size.
    pub total_payload_size: u64,
    /// Whether the plan includes buffer patches.
    pub has_buffer_patches: bool,
    /// Whether the plan is a single-file operation.
    pub is_single_file: bool,
}

/// A single item in a rename/move plan.
#[derive(Debug, Clone)]
pub struct RenamePlanItem {
    /// Item ID.
    pub id: u64,
    /// Source path.
    pub from_path: PathBuf,
    /// Destination path.
    pub to_path: PathBuf,
    /// Whether the source is open in a buffer.
    pub is_open: bool,
    /// Buffer ID if open.
    pub buffer_id: Option<BufferId>,
    /// Buffer revision if open.
    pub buffer_revision: Option<u64>,
    /// Whether this is a rename (same filesystem) or move (cross-filesystem).
    pub is_rename: bool,
}

/// Planner for rename/move operations.
pub struct RenamePlanner {
    pub fs: Arc<dyn Fs + Send + Sync>,
    pub vault_root: PathBuf,
    pub payload_budget: u64,
}

impl RenamePlanner {
    /// Create a new rename planner.
    pub fn new(fs: Arc<dyn Fs + Send + Sync>, vault_root: PathBuf, payload_budget: u64) -> Self {
        Self {
            fs,
            vault_root,
            payload_budget,
        }
    }

    /// Plan a rename/move operation.
    ///
    /// For now, this is a stub that returns an error.
    /// Full implementation will be added in subsequent work.
    pub fn plan_rename(
        &self,
        _from_path: PathBuf,
        _to_path: PathBuf,
        _buffer_ids: &HashMap<PathBuf, u64>,
        _buffer_revisions: &HashMap<u64, u64>,
    ) -> RenamePlanResult {
        // Stub implementation - always returns not supported
        // This will be fully implemented when the rename infrastructure is complete
        RenamePlanResult::DirectoryMoveNotSupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_planner_creation() {
        let planner = RenamePlanner {
            fs: Arc::new(flavite_fs::MemFs::new()),
            vault_root: PathBuf::from("/vault"),
            payload_budget: 256 * 1024 * 1024,
        };

        // Stub test - planner should be creatable
        assert_eq!(planner.vault_root, PathBuf::from("/vault"));
    }

    #[test]
    fn unfinished_planner_reports_unsupported_directory_move() {
        let planner = RenamePlanner::new(
            Arc::new(flavite_fs::MemFs::new()),
            PathBuf::from("/vault"),
            1024,
        );
        let result = planner.plan_rename(
            PathBuf::from("old.md"),
            PathBuf::from("new.md"),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert!(matches!(
            result,
            RenamePlanResult::DirectoryMoveNotSupported
        ));
    }
}
