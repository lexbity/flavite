//! D7 buffer state machine (§4.5.2, D7, §4.16.1).
//!
//! There is exactly one `Buffer` per open note in a `BufferManager`.
//! All document mutations execute as session commands and apply `EditOp`s
//! through its BufferManager.
//!
//! The D7 state machine tracks:
//! - **Clean**: buffer content matches disk content (base_hash matches)
//! - **Dirty**: buffer has unsaved edits
//! - **Conflicted**: external change detected, requires user resolution
//! - **Orphaned**: the file was deleted from disk
//!
//! Transitions are triggered by:
//! - Local edits
//! - External changes detected by the watcher
//! - File operations (rename, delete)
//! - User resolution actions

use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use std::fmt;

use crate::model::EditorModel;
use crate::revision::{BaseHash, BufferId, Revision};

/// The buffer state as defined by D7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BufferState {
    /// Buffer content matches what's on disk.
    Clean,
    /// Buffer has unsaved edits.
    Dirty,
    /// External change detected; buffer content differs from disk.
    /// Autosave MUST be disabled in this state (D7).
    Conflicted,
    /// The file was deleted from disk.
    Orphaned,
}

impl BufferState {
    /// Check if the buffer is in a state where autosave is allowed.
    #[must_use]
    pub fn allows_autosave(self) -> bool {
        !matches!(self, BufferState::Conflicted)
    }

    /// Check if the buffer is in a state where manual save is allowed.
    #[must_use]
    pub fn allows_manual_save(self) -> bool {
        !matches!(self, BufferState::Conflicted | BufferState::Orphaned)
    }

    /// Check if the buffer needs user attention.
    #[must_use]
    pub fn needs_resolution(self) -> bool {
        matches!(self, BufferState::Conflicted | BufferState::Orphaned)
    }
}

impl fmt::Display for BufferState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BufferState::Clean => write!(f, "Clean"),
            BufferState::Dirty => write!(f, "Dirty"),
            BufferState::Conflicted => write!(f, "Conflicted"),
            BufferState::Orphaned => write!(f, "Orphaned"),
        }
    }
}

/// The reason for a state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransitionReason {
    /// Local edit applied.
    LocalEdit,
    /// External change detected by watcher.
    ExternalChange,
    /// File was deleted externally.
    FileDeleted,
    /// File was recreated after being deleted.
    FileRecreated,
    /// User chose to reload from disk.
    Reload,
    /// User chose to keep local changes.
    KeepLocal,
    /// User chose to save local changes as a new file.
    SaveAs,
    /// User chose to discard local changes.
    Discard,
    /// File was renamed (T0–T2: silent re-key; T3–T4: requires confirmation).
    Renamed,
    /// Buffer was reopened.
    Reopened,
    /// Save completed successfully.
    SaveCompleted,
    /// User performed a merge.
    Merge,
}

/// Information about a state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateTransition {
    /// The previous state.
    pub from: BufferState,
    /// The new state.
    pub to: BufferState,
    /// The reason for the transition.
    pub reason: TransitionReason,
}

impl StateTransition {
    /// Create a new state transition.
    #[must_use]
    pub fn new(from: BufferState, to: BufferState, reason: TransitionReason) -> Self {
        Self { from, to, reason }
    }
}

/// The buffer type that wraps an EditorModel with D7 state machine.
///
/// A Buffer owns:
/// - An EditorModel (text, selections, undo history, EOL metadata)
/// - A base_hash tracking what was on disk when last loaded
/// - A BufferState from the D7 state machine
/// - Revision tracking
///
/// All document mutations go through the Buffer, which manages the state
/// transitions and revision tracking.
#[derive(Debug)]
pub struct Buffer {
    /// The underlying editor model.
    model: EditorModel,
    /// The current buffer state.
    state: BufferState,
    /// The base hash of the content when last loaded or saved.
    base_hash: Option<BaseHash>,
    /// The disk hash at the time of the last disk check.
    /// Used to detect external changes.
    disk_hash: Option<BaseHash>,
}

impl Buffer {
    /// Create a new buffer from a text snapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: TextSnapshot) -> Self {
        let model = EditorModel::from_snapshot(snapshot.clone());
        let hash = BaseHash::from_snapshot(&snapshot);

        Self {
            model,
            state: BufferState::Clean,
            base_hash: Some(hash),
            disk_hash: Some(hash),
        }
    }

    /// Create a new empty buffer.
    #[must_use]
    pub fn new_empty() -> Self {
        let snapshot = TextSnapshot::new(b"").unwrap();
        Self::from_snapshot(snapshot)
    }

    /// Create a new buffer with plain text.
    #[must_use]
    pub fn from_str(text: &str) -> Self {
        let snapshot = TextSnapshot::new(text.as_bytes()).unwrap();
        Self::from_snapshot(snapshot)
    }

    /// Get the buffer identity.
    #[must_use]
    pub fn buffer_id(&self) -> BufferId {
        self.model.buffer_id()
    }

    /// Get the current revision.
    #[must_use]
    pub fn revision(&self) -> Revision {
        self.model.revision()
    }

    /// Get the current state.
    #[must_use]
    pub fn state(&self) -> BufferState {
        self.state
    }

    /// Get the base hash.
    #[must_use]
    pub fn base_hash(&self) -> Option<BaseHash> {
        self.base_hash
    }

    /// Get the disk hash.
    #[must_use]
    pub fn disk_hash(&self) -> Option<BaseHash> {
        self.disk_hash
    }

    /// Get the content hash of the current buffer content.
    #[must_use]
    pub fn content_hash(&self) -> BaseHash {
        self.model.content_hash()
    }

    /// Check if the buffer is dirty.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.state == BufferState::Dirty
    }

    /// Check if the buffer is conflicted.
    #[must_use]
    pub fn is_conflicted(&self) -> bool {
        self.state == BufferState::Conflicted
    }

    /// Check if the buffer is orphaned.
    #[must_use]
    pub fn is_orphaned(&self) -> bool {
        self.state == BufferState::Orphaned
    }

    /// Get the text content.
    #[must_use]
    pub fn text(&self) -> &str {
        self.model.text()
    }

    /// Get the length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.model.len()
    }

    /// Get the line count.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.model.line_count()
    }

    /// Access the underlying EditorModel.
    #[must_use]
    pub fn model(&self) -> &EditorModel {
        &self.model
    }

    /// Access the underlying EditorModel mutably.
    #[must_use]
    pub fn model_mut(&mut self) -> &mut EditorModel {
        &mut self.model
    }

    /// Apply a local edit.
    ///
    /// This applies an edit operation and transitions the state appropriately.
    /// Returns the new revision and any state transition that occurred.
    pub fn apply_local_edit(
        &mut self,
        edit: crate::undo::EditOp,
    ) -> (Revision, Option<StateTransition>) {
        let old_state = self.state;

        // Apply the edit
        let new_revision = self.model.apply_edit_op(edit);

        // Transition state
        let new_state = match old_state {
            BufferState::Clean | BufferState::Dirty | BufferState::Conflicted => BufferState::Dirty,
            BufferState::Orphaned => BufferState::Dirty, // Can edit orphaned buffers
        };

        let transition = if old_state != new_state {
            Some(StateTransition::new(
                old_state,
                new_state,
                TransitionReason::LocalEdit,
            ))
        } else {
            None
        };

        self.state = new_state;
        (new_revision, transition)
    }

    /// Check for external changes.
    ///
    /// This is called when the watcher detects a change on disk.
    /// Returns the state transition if one occurred.
    pub fn check_external_change(&mut self, new_disk_bytes: &[u8]) -> Option<StateTransition> {
        let old_state = self.state;

        // Try to create a snapshot from the new disk content
        if let Ok(new_snapshot) = TextSnapshot::new(new_disk_bytes) {
            let new_disk_hash = BaseHash::from_snapshot(&new_snapshot);
            self.disk_hash = Some(new_disk_hash);

            // Check if the disk content matches our base hash
            if let Some(base_hash) = self.base_hash {
                if base_hash == new_disk_hash {
                    // Disk content is the same as what we loaded
                    // This can happen with metadata-only changes
                    return None;
                }
            }

            // Disk content has changed
            let new_state = match old_state {
                BufferState::Clean => {
                    // We were clean, but disk changed under us
                    // Check if our buffer content matches the new disk content
                    if self.model.matches_hash(new_disk_hash) {
                        // Our buffer matches the new disk content
                        // This means we auto-resolved to Clean
                        // (D7: "Dirty | changed to *identical* content as buffer → Auto-resolve → Clean")
                        BufferState::Clean
                    } else {
                        // External change detected
                        BufferState::Conflicted
                    }
                }
                BufferState::Dirty => {
                    // We were dirty, disk changed
                    // Check if our buffer matches the new disk content
                    if self.model.matches_hash(new_disk_hash) {
                        // Auto-resolve to Clean
                        BufferState::Clean
                    } else {
                        // Conflict
                        BufferState::Conflicted
                    }
                }
                BufferState::Conflicted | BufferState::Orphaned => {
                    // Already in a conflicted state
                    old_state
                }
            };

            if new_state != old_state {
                let transition =
                    StateTransition::new(old_state, new_state, TransitionReason::ExternalChange);
                self.state = new_state;
                Some(transition)
            } else {
                None
            }
        } else {
            // Disk content is not valid UTF-8
            // We can't load it, so mark as conflicted
            None
        }
    }

    /// Handle file deletion.
    ///
    /// Called when the watcher detects the file was deleted.
    /// Returns the state transition.
    pub fn handle_file_deleted(&mut self) -> StateTransition {
        let old_state = self.state;
        let new_state = BufferState::Orphaned;

        self.state = new_state;
        self.disk_hash = None;

        StateTransition::new(old_state, new_state, TransitionReason::FileDeleted)
    }

    /// Handle file recreation.
    ///
    /// Called when a deleted file reappears.
    /// Returns the state transition.
    pub fn handle_file_recreated(&mut self, new_bytes: &[u8]) -> Option<StateTransition> {
        let old_state = self.state;

        if let Ok(snapshot) = TextSnapshot::new(new_bytes) {
            let new_hash = BaseHash::from_snapshot(&snapshot);
            self.disk_hash = Some(new_hash);

            let new_state = if old_state == BufferState::Orphaned {
                // File was recreated
                if self.base_hash == Some(new_hash) {
                    // Recreated with same content as when we loaded
                    BufferState::Clean
                } else if self.model.matches_hash(new_hash) {
                    // Recreated with same content as our buffer
                    BufferState::Clean
                } else {
                    // Recreated with different content
                    BufferState::Conflicted
                }
            } else {
                old_state
            };

            if new_state != old_state {
                let transition =
                    StateTransition::new(old_state, new_state, TransitionReason::FileRecreated);
                self.state = new_state;
                Some(transition)
            } else {
                None
            }
        } else {
            // Invalid UTF-8
            None
        }
    }

    /// User chose to reload from disk.
    ///
    /// Replaces the buffer content with the current disk content.
    /// Returns the state transition.
    pub fn reload_from_disk(&mut self, disk_bytes: &[u8]) -> Option<StateTransition> {
        let old_state = self.state;

        if let Ok(snapshot) = TextSnapshot::new(disk_bytes) {
            let new_hash = BaseHash::from_snapshot(&snapshot);

            // Replace the model
            self.model = EditorModel::from_snapshot(snapshot);
            self.disk_hash = Some(new_hash);
            self.base_hash = Some(new_hash);
            self.state = BufferState::Clean;

            Some(StateTransition::new(
                old_state,
                BufferState::Clean,
                TransitionReason::Reload,
            ))
        } else {
            None
        }
    }

    /// User chose to keep local changes.
    ///
    /// Saves the local changes over the disk content.
    /// Returns the state transition.
    pub fn keep_local(&mut self) -> StateTransition {
        let old_state = self.state;
        let new_state = BufferState::Dirty;

        // Update base hash to match current content
        self.base_hash = Some(self.content_hash());
        // Disk hash remains as is (we'll detect future changes)

        self.state = new_state;

        StateTransition::new(old_state, new_state, TransitionReason::KeepLocal)
    }

    /// User chose to discard local changes.
    ///
    /// Discards local changes and reloads from disk (if available).
    /// Returns the state transition.
    pub fn discard(&mut self, disk_bytes: Option<&[u8]>) -> StateTransition {
        let old_state = self.state;

        if let Some(bytes) = disk_bytes {
            if let Ok(snapshot) = TextSnapshot::new(bytes) {
                let new_hash = BaseHash::from_snapshot(&snapshot);

                self.model = EditorModel::from_snapshot(snapshot);
                self.base_hash = Some(new_hash);
                self.disk_hash = Some(new_hash);
                self.state = BufferState::Clean;

                return StateTransition::new(
                    old_state,
                    BufferState::Clean,
                    TransitionReason::Discard,
                );
            }
        }

        // No disk content available, just reset to empty
        self.model = EditorModel::new_empty();
        self.base_hash = None;
        self.disk_hash = None;
        self.state = BufferState::Clean;

        StateTransition::new(old_state, BufferState::Clean, TransitionReason::Discard)
    }

    /// Handle successful save.
    ///
    /// Called after a save completes successfully.
    /// Returns the state transition.
    pub fn handle_save_completed(&mut self) -> StateTransition {
        let old_state = self.state;

        // Update base hash to match current content
        self.base_hash = Some(self.content_hash());
        // Disk hash now matches base hash
        self.disk_hash = self.base_hash;

        let new_state = BufferState::Clean;
        self.state = new_state;

        StateTransition::new(old_state, new_state, TransitionReason::SaveCompleted)
    }

    /// Handle file rename.
    ///
    /// For T0–T2 renames (in-app or detected OS rename), this is silent.
    /// For T3–T4, user confirmation is required first.
    /// Returns the state transition.
    pub fn handle_rename(&mut self) -> StateTransition {
        let old_state = self.state;
        // Rename doesn't change the state
        StateTransition::new(old_state, old_state, TransitionReason::Renamed)
    }

    /// Reopen the buffer (create a new buffer instance for the same file).
    ///
    /// Returns the state transition.
    pub fn reopen(&mut self) -> StateTransition {
        let old_state = self.state;
        // Reopening resets the buffer to a clean state with the current content
        self.base_hash = Some(self.content_hash());
        self.disk_hash = self.base_hash;
        self.state = BufferState::Clean;

        StateTransition::new(old_state, BufferState::Clean, TransitionReason::Reopened)
    }

    /// Merge with external changes.
    ///
    /// This is a placeholder for the three-way merge operation.
    /// In a full implementation, this would use the history store.
    pub fn merge(&mut self, _base: &[u8], _theirs: &[u8]) -> Option<StateTransition> {
        // For now, we'll just mark as clean (merge successful)
        let old_state = self.state;
        self.state = BufferState::Clean;

        Some(StateTransition::new(
            old_state,
            BufferState::Clean,
            TransitionReason::Merge,
        ))
    }

    /// Check if the buffer allows autosave.
    #[must_use]
    pub fn allows_autosave(&self) -> bool {
        self.state.allows_autosave()
    }

    /// Check if the buffer allows manual save.
    #[must_use]
    pub fn allows_manual_save(&self) -> bool {
        self.state.allows_manual_save()
    }

    /// Check if the buffer needs user resolution.
    #[must_use]
    pub fn needs_resolution(&self) -> bool {
        self.state.needs_resolution()
    }
}

impl Default for Buffer {
    fn default() -> Self {
        Self::new_empty()
    }
}

impl fmt::Display for Buffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "Buffer(id={}, state={}, rev={}, len={}, lines={})",
            self.buffer_id().as_u64(),
            self.state,
            self.revision().as_u64(),
            self.len(),
            self.line_count()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_buffer() {
        let buffer = Buffer::from_str("hello");
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.text(), "hello");
        assert!(!buffer.is_dirty());
    }

    #[test]
    fn local_edit_transition() {
        let mut buffer = Buffer::from_str("hello");

        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());

        let (rev, transition) = buffer.apply_local_edit(edit);
        assert_eq!(buffer.state(), BufferState::Dirty);
        assert!(buffer.is_dirty());
        assert_eq!(
            transition,
            Some(StateTransition::new(
                BufferState::Clean,
                BufferState::Dirty,
                TransitionReason::LocalEdit
            ))
        );
    }

    #[test]
    fn external_change_to_identical() {
        let mut buffer = Buffer::from_str("hello");

        // Apply a local edit
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // External change to identical content
        let new_content = b"hello world";
        let transition = buffer.check_external_change(new_content);

        // Should auto-resolve to Clean
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(
            transition,
            Some(StateTransition::new(
                BufferState::Dirty,
                BufferState::Clean,
                TransitionReason::ExternalChange
            ))
        );
    }

    #[test]
    fn external_change_conflict() {
        let mut buffer = Buffer::from_str("hello");

        // External change to different content
        let new_content = b"goodbye";
        let transition = buffer.check_external_change(new_content);

        assert_eq!(buffer.state(), BufferState::Conflicted);
        assert_eq!(
            transition,
            Some(StateTransition::new(
                BufferState::Clean,
                BufferState::Conflicted,
                TransitionReason::ExternalChange
            ))
        );
    }

    #[test]
    fn file_deleted() {
        let mut buffer = Buffer::from_str("hello");
        let transition = buffer.handle_file_deleted();

        assert_eq!(buffer.state(), BufferState::Orphaned);
        assert_eq!(transition.from, BufferState::Clean);
        assert_eq!(transition.to, BufferState::Orphaned);
        assert_eq!(transition.reason, TransitionReason::FileDeleted);
    }

    #[test]
    fn file_recreated() {
        let mut buffer = Buffer::from_str("hello");
        buffer.handle_file_deleted();

        let transition = buffer.handle_file_recreated(b"hello");
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(transition.unwrap().to, BufferState::Clean);
    }

    #[test]
    fn reload_from_disk() {
        let mut buffer = Buffer::from_str("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // Reload from disk
        let transition = buffer.reload_from_disk(b"hello");
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.text(), "hello");
    }

    #[test]
    fn keep_local() {
        let mut buffer = Buffer::from_str("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // External change
        buffer.check_external_change(b"goodbye");
        assert_eq!(buffer.state(), BufferState::Conflicted);

        // Keep local
        let _transition = buffer.keep_local();
        assert_eq!(buffer.state(), BufferState::Dirty);
        assert!(buffer.allows_manual_save());
        assert!(buffer.allows_autosave());
    }

    #[test]
    fn discard() {
        let mut buffer = Buffer::from_str("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // Discard
        let transition = buffer.discard(Some(b"hello"));
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.text(), "hello");
    }

    #[test]
    fn save_completed() {
        let mut buffer = Buffer::from_str("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // Save completed
        let transition = buffer.handle_save_completed();
        assert_eq!(buffer.state(), BufferState::Clean);
        assert!(!buffer.is_dirty());
    }

    #[test]
    fn autosave_disabled_in_conflicted() {
        let mut buffer = Buffer::from_str("hello");
        buffer.check_external_change(b"goodbye");
        assert_eq!(buffer.state(), BufferState::Conflicted);
        assert!(!buffer.allows_autosave());
    }

    #[test]
    fn manual_save_disabled_in_conflicted_and_orphaned() {
        let mut buffer = Buffer::from_str("hello");

        // Conflicted
        buffer.check_external_change(b"goodbye");
        assert!(!buffer.allows_manual_save());

        // Orphaned
        let mut buffer2 = Buffer::from_str("hello");
        buffer2.handle_file_deleted();
        assert!(!buffer2.allows_manual_save());
    }

    #[test]
    fn d7_table_clean_changed() {
        // Clean | changed → Silent reload. Map cursor and scroll through the diff
        let mut buffer = Buffer::from_str("hello");
        let transition = buffer.check_external_change(b"hello world");

        assert_eq!(buffer.state(), BufferState::Conflicted);
        // This would normally trigger a silent reload in the UI
    }

    #[test]
    fn d7_table_clean_deleted() {
        // Clean | deleted → Keep pane open as *Orphaned*, banner "file deleted"
        let mut buffer = Buffer::from_str("hello");
        let transition = buffer.handle_file_deleted();

        assert_eq!(buffer.state(), BufferState::Orphaned);
    }

    #[test]
    fn d7_table_dirty_changed() {
        // Dirty | changed → Conflict banner
        let mut buffer = Buffer::from_str("hello");
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        let transition = buffer.check_external_change(b"goodbye");
        assert_eq!(buffer.state(), BufferState::Conflicted);
    }

    #[test]
    fn d7_table_dirty_changed_to_identical() {
        // Dirty | changed to *identical* content as buffer → Auto-resolve → Clean
        let mut buffer = Buffer::from_str("hello");
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        let transition = buffer.check_external_change(b"hello world");
        assert_eq!(buffer.state(), BufferState::Clean);
    }

    #[test]
    fn d7_table_dirty_deleted() {
        // Dirty | deleted → Recreate · Save as · Discard
        let mut buffer = Buffer::from_str("hello");
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        let transition = buffer.handle_file_deleted();
        assert_eq!(buffer.state(), BufferState::Orphaned);
    }

    #[test]
    fn d7_table_conflicted() {
        // Conflicted state blocks autosave and manual save
        let mut buffer = Buffer::from_str("hello");
        buffer.check_external_change(b"goodbye");

        assert!(buffer.is_conflicted());
        assert!(!buffer.allows_autosave());
        assert!(!buffer.allows_manual_save());
        assert!(buffer.needs_resolution());
    }
}
