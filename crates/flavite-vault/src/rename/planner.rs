//! Rename/move planner (D9, §4.15).

use flavite_buffer::revision::BufferId;
use flavite_fs::Fs;
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
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

impl RenamePlanResult {
    /// Return the plan or the planner's precise refusal reason.
    pub fn into_plan(self) -> Result<RenamePlan, String> {
        match self {
            Self::Success(plan) => Ok(plan),
            Self::ValidationFailed(error) => Err(error.to_string()),
            Self::DestinationOccupied(path) => {
                Err(format!("destination is occupied: {}", path.display()))
            }
            Self::CrossFilesystemNotSupported => {
                Err("cross-filesystem move is not supported".into())
            }
            Self::DirectoryMoveNotSupported => Err("directory move is not supported".into()),
            Self::DiskSpaceInsufficient => Err("insufficient disk space".into()),
            Self::PayloadBudgetExceeded => Err("rename exceeds the payload budget".into()),
        }
    }
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
    /// # Errors
    /// This method returns refusals as [`RenamePlanResult`] variants.
    pub fn plan_rename(
        &self,
        from_path: PathBuf,
        to_path: PathBuf,
        buffer_ids: &HashMap<PathBuf, u64>,
        buffer_revisions: &HashMap<u64, u64>,
    ) -> RenamePlanResult {
        let root = match self.fs.canonicalize(&self.vault_root) {
            Ok(root) => root,
            Err(_) => {
                return RenamePlanResult::ValidationFailed(RenameValidationError::PathEscapesVault);
            }
        };
        let from = if from_path.is_absolute() {
            from_path
        } else {
            root.join(from_path)
        };
        let to = if to_path.is_absolute() {
            to_path
        } else {
            root.join(to_path)
        };
        if !safe_lexical_path(&from) || !safe_lexical_path(&to) {
            return RenamePlanResult::ValidationFailed(RenameValidationError::PathEscapesVault);
        }
        if self.fs.validate_in_root(&root, &from).is_err()
            || self
                .fs
                .validate_in_root(&root, to.parent().unwrap_or(&to))
                .is_err()
        {
            return RenamePlanResult::ValidationFailed(RenameValidationError::PathEscapesVault);
        }
        let source = match self.fs.metadata(&from) {
            Ok(metadata) => metadata,
            Err(_) => {
                return RenamePlanResult::ValidationFailed(RenameValidationError::SourceNotFound);
            }
        };
        if source.is_dir {
            return RenamePlanResult::DirectoryMoveNotSupported;
        }
        if self.fs.exists(&to).unwrap_or(false) {
            return RenamePlanResult::DestinationOccupied(to);
        }
        let Some(parent) = to.parent() else {
            return RenamePlanResult::ValidationFailed(
                RenameValidationError::DestinationParentNotFound,
            );
        };
        let parent_metadata = match self.fs.metadata(parent) {
            Ok(metadata) if metadata.is_dir => metadata,
            _ => {
                return RenamePlanResult::ValidationFailed(
                    RenameValidationError::DestinationParentNotFound,
                );
            }
        };
        if source.volume_id != parent_metadata.volume_id {
            return RenamePlanResult::CrossFilesystemNotSupported;
        }
        if source.size > self.payload_budget {
            return RenamePlanResult::PayloadBudgetExceeded;
        }
        if let Err(error) = self.fs.validate_in_root(&root, &from) {
            return RenamePlanResult::ValidationFailed(match error {
                flavite_fs::FsError::PathEscape(_) => RenameValidationError::PathEscapesVault,
                _ => RenameValidationError::SourceNotFound,
            });
        }
        let path_key = |path: &Path| path.strip_prefix(&root).unwrap_or(path).to_path_buf();
        let source_key = path_key(&from);
        let buffer_id = buffer_ids.get(&source_key).copied();
        let buffer_revision = buffer_id.and_then(|id| buffer_revisions.get(&id).copied());
        let item = RenamePlanItem {
            id: 1,
            from_path: source_key,
            to_path: path_key(&to),
            is_open: buffer_id.is_some(),
            buffer_id: buffer_id.map(BufferId::from),
            buffer_revision,
            is_rename: true,
        };
        RenamePlanResult::Success(RenamePlan {
            txn_id: uuid::Uuid::now_v7().to_string(),
            items: vec![item],
            total_payload_size: source.size,
            has_buffer_patches: false,
            is_single_file: true,
        })
    }
}

fn safe_lexical_path(path: &Path) -> bool {
    path.components()
        .all(|component| !matches!(component, Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_accepts_a_constructed_vault_root() {
        let planner = RenamePlanner::new(
            Arc::new(flavite_fs::MemFs::new()),
            PathBuf::from("/vault"),
            256 * 1024 * 1024,
        );
        assert_eq!(planner.vault_root, PathBuf::from("/vault"));
    }

    #[test]
    fn validates_and_plans_same_volume_file_rename() {
        let fs = Arc::new(flavite_fs::MemFs::new());
        fs.create_dir_all(Path::new("/vault/notes")).unwrap();
        fs.write_new(Path::new("/vault/notes/a.md"), b"base")
            .unwrap();
        let planner = RenamePlanner::new(fs, "/vault".into(), 1024);
        let RenamePlanResult::Success(plan) = planner.plan_rename(
            "notes/a.md".into(),
            "notes/b.md".into(),
            &HashMap::new(),
            &HashMap::new(),
        ) else {
            panic!("expected valid rename plan");
        };
        assert!(plan.is_single_file);
        assert_eq!(plan.items[0].from_path, PathBuf::from("notes/a.md"));
        assert_eq!(plan.items[0].to_path, PathBuf::from("notes/b.md"));
        assert_eq!(plan.total_payload_size, 4);
    }

    #[test]
    fn planner_rejects_traversal_and_occupied_destination() {
        let fs = Arc::new(flavite_fs::MemFs::new());
        fs.create_dir_all(Path::new("/vault/notes")).unwrap();
        fs.write_new(Path::new("/vault/notes/a.md"), b"a").unwrap();
        fs.write_new(Path::new("/vault/notes/b.md"), b"b").unwrap();
        let planner = RenamePlanner::new(fs, "/vault".into(), 1024);
        assert!(matches!(
            planner.plan_rename(
                "../outside.md".into(),
                "notes/c.md".into(),
                &HashMap::new(),
                &HashMap::new()
            ),
            RenamePlanResult::ValidationFailed(RenameValidationError::PathEscapesVault)
        ));
        assert!(matches!(
            planner.plan_rename(
                "notes/a.md".into(),
                "notes/b.md".into(),
                &HashMap::new(),
                &HashMap::new()
            ),
            RenamePlanResult::DestinationOccupied(_)
        ));
    }
}
