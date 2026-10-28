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

use flavite_syntax::text::TextSnapshot;
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
    /// User restored a crash recovery snapshot.
    RecoveryRestored,
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
    /// Exact bytes corresponding to `base_hash`, retained as the merge base.
    base_bytes: Vec<u8>,
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
            base_bytes: snapshot.source().to_vec(),
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
    pub fn from_text(text: &str) -> Self {
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

    /// Exact source bytes for the current merge base.
    #[must_use]
    pub fn base_bytes(&self) -> &[u8] {
        &self.base_bytes
    }

    /// Record the exact source bytes known to be the current disk base.
    pub fn set_base_snapshot(&mut self, bytes: Vec<u8>) {
        let hash = BaseHash::from_bytes(*blake3::hash(&bytes).as_bytes());
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = bytes;
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

    /// Whether the buffer contains no text bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.model.is_empty()
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
            if let Some(base_hash) = self.base_hash
                && base_hash == new_disk_hash
            {
                // Disk content is the same as what we loaded
                // This can happen with metadata-only changes
                return None;
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
            self.base_bytes = self.model.to_source_bytes();
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

    /// Restore dirty text from a validated recovery snapshot while keeping
    /// the current disk bytes as the concurrency and merge base.
    pub fn restore_recovery(
        &mut self,
        recovered_bytes: &[u8],
        disk_bytes: &[u8],
        recovered_base_hash: Option<BaseHash>,
    ) -> Option<StateTransition> {
        let recovered = TextSnapshot::new(recovered_bytes).ok()?;
        let disk = TextSnapshot::new(disk_bytes).ok()?;
        let old_state = self.state;
        let recovered_hash = BaseHash::from_snapshot(&recovered);
        let disk_hash = BaseHash::from_snapshot(&disk);
        self.model.replace_snapshot(recovered);
        self.base_hash = Some(disk_hash);
        self.disk_hash = Some(disk_hash);
        self.base_bytes = disk_bytes.to_vec();
        self.state = if recovered_hash == disk_hash {
            BufferState::Clean
        } else if recovered_base_hash.is_some_and(|base| base != disk_hash) {
            BufferState::Conflicted
        } else {
            BufferState::Dirty
        };
        Some(StateTransition::new(
            old_state,
            self.state,
            TransitionReason::RecoveryRestored,
        ))
    }

    /// User chose to keep local changes.
    ///
    /// Saves the local changes over the disk content.
    /// Returns the state transition.
    pub fn keep_local(&mut self) -> StateTransition {
        let old_state = self.state;
        let new_state = BufferState::Dirty;

        // This legacy transition has no observed disk bytes. A caller that
        // resolves a real conflict must use `resolve_keep_local` below.

        self.state = new_state;

        StateTransition::new(old_state, new_state, TransitionReason::KeepLocal)
    }

    /// Resolve a detected conflict by retaining mine against the exact disk
    /// bytes observed and snapshotted by the caller.
    pub fn resolve_keep_local(&mut self, disk_bytes: &[u8]) -> Option<StateTransition> {
        let old_state = self.state;
        let snapshot = TextSnapshot::new(disk_bytes).ok()?;
        let hash = BaseHash::from_snapshot(&snapshot);
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = disk_bytes.to_vec();
        self.state = BufferState::Dirty;
        Some(StateTransition::new(
            old_state,
            BufferState::Dirty,
            TransitionReason::KeepLocal,
        ))
    }

    /// User chose to discard local changes.
    ///
    /// Discards local changes and reloads from disk (if available).
    /// Returns the state transition.
    pub fn discard(&mut self, disk_bytes: Option<&[u8]>) -> StateTransition {
        let old_state = self.state;

        if let Some(bytes) = disk_bytes
            && let Ok(snapshot) = TextSnapshot::new(bytes)
        {
            let new_hash = BaseHash::from_snapshot(&snapshot);

            self.model = EditorModel::from_snapshot(snapshot);
            self.base_hash = Some(new_hash);
            self.disk_hash = Some(new_hash);
            self.base_bytes = self.model.to_source_bytes();
            self.state = BufferState::Clean;

            return StateTransition::new(old_state, BufferState::Clean, TransitionReason::Discard);
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
        self.base_bytes = self.model.to_source_bytes();
        // Disk hash now matches base hash
        self.disk_hash = self.base_hash;

        let new_state = BufferState::Clean;
        self.state = new_state;

        StateTransition::new(old_state, new_state, TransitionReason::SaveCompleted)
    }

    /// Record a save receipt for the exact revision that was serialized.
    ///
    /// The disk base advances even if the user typed while that save was in
    /// flight. In that case the buffer remains dirty and its newer contents
    /// continue to compare against the bytes now on disk.
    pub fn handle_save_completed_revision(
        &mut self,
        saved_revision: Revision,
        saved_bytes: &[u8],
    ) -> StateTransition {
        let old_state = self.state;
        let hash = BaseHash::from_bytes(*blake3::hash(saved_bytes).as_bytes());
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = saved_bytes.to_vec();

        let new_state = if self.model.revision() == saved_revision {
            BufferState::Clean
        } else {
            BufferState::Dirty
        };
        self.state = new_state;
        StateTransition::new(old_state, new_state, TransitionReason::SaveCompleted)
    }

    /// Record bytes known to have reached disk after a save whose later
    /// durability/index stage failed. The buffer remains dirty until the full
    /// save milestone is acknowledged, but retries compare against this disk
    /// state instead of blindly replaying an obsolete base precondition.
    pub fn record_replaced_disk_hash(&mut self, hash: BaseHash) {
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = self.model.to_source_bytes();
    }

    /// Record a particular saved revision as the disk base without replacing
    /// the current buffer text, which may contain newer edits.
    pub fn record_replaced_disk_snapshot(&mut self, bytes: &[u8]) {
        let hash = BaseHash::from_bytes(*blake3::hash(bytes).as_bytes());
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = bytes.to_vec();
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
    use flavite_syntax::text::{Span, TextByte};

    #[test]
    fn new_buffer() {
        let buffer = Buffer::from_text("hello");
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.text(), "hello");
        assert!(!buffer.is_dirty());
    }

    #[test]
    fn local_edit_transition() {
        let mut buffer = Buffer::from_text("hello");

        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());

        let (_rev, transition) = buffer.apply_local_edit(edit);
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
        let mut buffer = Buffer::from_text("hello");

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
        let mut buffer = Buffer::from_text("hello");

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
        let mut buffer = Buffer::from_text("hello");
        let transition = buffer.handle_file_deleted();

        assert_eq!(buffer.state(), BufferState::Orphaned);
        assert_eq!(transition.from, BufferState::Clean);
        assert_eq!(transition.to, BufferState::Orphaned);
        assert_eq!(transition.reason, TransitionReason::FileDeleted);
    }

    #[test]
    fn file_recreated() {
        let mut buffer = Buffer::from_text("hello");
        buffer.handle_file_deleted();

        let transition = buffer.handle_file_recreated(b"hello");
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(transition.unwrap().to, BufferState::Clean);
    }

    #[test]
    fn reload_from_disk() {
        let mut buffer = Buffer::from_text("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // Reload from disk
        let _transition = buffer.reload_from_disk(b"hello");
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.text(), "hello");
    }

    #[test]
    fn keep_local() {
        let mut buffer = Buffer::from_text("hello");

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
        let mut buffer = Buffer::from_text("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // Discard
        let _transition = buffer.discard(Some(b"hello"));
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.text(), "hello");
    }

    #[test]
    fn save_completed() {
        let mut buffer = Buffer::from_text("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // Save completed
        let _transition = buffer.handle_save_completed();
        assert_eq!(buffer.state(), BufferState::Clean);
        assert!(!buffer.is_dirty());
    }

    #[test]
    fn recovery_restore_keeps_disk_as_base_and_marks_buffer_dirty() {
        let mut buffer = Buffer::from_text("disk");
        let id = buffer.buffer_id();
        let revision = buffer.revision();
        buffer
            .restore_recovery(
                b"recovered",
                b"disk",
                Some(BaseHash::from_bytes(*blake3::hash(b"disk").as_bytes())),
            )
            .expect("valid UTF-8 recovery and disk bytes");
        assert_eq!(buffer.buffer_id(), id);
        assert!(buffer.revision() > revision);
        assert_eq!(buffer.text(), "recovered");
        assert_eq!(buffer.base_bytes(), b"disk");
        assert_eq!(buffer.state(), BufferState::Dirty);
    }

    #[test]
    fn recovery_restore_detects_disk_divergence_from_saved_base() {
        let mut buffer = Buffer::from_text("disk changed");
        buffer
            .restore_recovery(
                b"recovered",
                b"disk changed",
                Some(BaseHash::from_bytes(*blake3::hash(b"disk").as_bytes())),
            )
            .expect("valid snapshots");
        assert_eq!(buffer.state(), BufferState::Conflicted);
        assert_eq!(buffer.base_bytes(), b"disk changed");
    }

    #[test]
    fn in_flight_save_advances_disk_base_without_cleaning_newer_edits() {
        let mut buffer = Buffer::from_text("base");
        let base_revision = buffer.revision();
        let first =
            crate::undo::EditOp::new(Span::new(TextByte(4), TextByte(4)), " saved".to_owned());
        buffer.apply_local_edit(first);
        let saved_revision = buffer.revision();
        let saved_bytes = buffer.model().to_source_bytes();
        let second =
            crate::undo::EditOp::new(Span::new(TextByte(10), TextByte(10)), " newer".to_owned());
        buffer.apply_local_edit(second);

        let transition = buffer.handle_save_completed_revision(saved_revision, &saved_bytes);

        assert_eq!(transition.to, BufferState::Dirty);
        assert_eq!(buffer.state(), BufferState::Dirty);
        assert_eq!(buffer.base_bytes(), saved_bytes);
        assert_eq!(
            buffer.base_hash(),
            Some(BaseHash::from_bytes(*blake3::hash(&saved_bytes).as_bytes()))
        );
        assert!(buffer.revision() > saved_revision);
        assert!(buffer.revision() > base_revision);
    }

    #[test]
    fn autosave_disabled_in_conflicted() {
        let mut buffer = Buffer::from_text("hello");
        buffer.check_external_change(b"goodbye");
        assert_eq!(buffer.state(), BufferState::Conflicted);
        assert!(!buffer.allows_autosave());
    }

    #[test]
    fn manual_save_disabled_in_conflicted_and_orphaned() {
        let mut buffer = Buffer::from_text("hello");

        // Conflicted
        buffer.check_external_change(b"goodbye");
        assert!(!buffer.allows_manual_save());

        // Orphaned
        let mut buffer2 = Buffer::from_text("hello");
        buffer2.handle_file_deleted();
        assert!(!buffer2.allows_manual_save());
    }

    #[test]
    fn buffer_accessors_invalid_disk_inputs_and_explicit_base_updates() {
        let mut empty = Buffer::default();
        assert!(empty.is_empty());
        assert_eq!((empty.len(), empty.line_count()), (0, 1));
        empty.set_base_snapshot(b"base".to_vec());
        assert_eq!(empty.base_bytes(), b"base");
        assert_eq!(empty.base_hash(), Some(empty.disk_hash().unwrap()));
        assert!(empty.check_external_change(b"base").is_none());
        assert!(empty.check_external_change(&[0xff]).is_none());
        assert!(empty.handle_file_recreated(&[0xff]).is_none());
        assert!(empty.reload_from_disk(&[0xff]).is_none());
        assert!(empty.resolve_keep_local(&[0xff]).is_none());

        let mut buffer = Buffer::from_text("base");
        buffer.handle_file_deleted();
        assert!(buffer.is_orphaned());
        let recreated = buffer.handle_file_recreated(b"other").unwrap();
        assert_eq!(recreated.to, BufferState::Conflicted);
        assert!(buffer.is_conflicted());
        assert!(buffer.needs_resolution());
        let resolved = buffer.resolve_keep_local(b"other").unwrap();
        assert_eq!(resolved.reason, TransitionReason::KeepLocal);
        assert_eq!(buffer.base_bytes(), b"other");
        assert_eq!(buffer.state(), BufferState::Dirty);

        buffer.record_replaced_disk_snapshot(b"saved");
        assert_eq!(buffer.base_bytes(), b"saved");
        buffer.record_replaced_disk_hash(BaseHash::from_bytes(*blake3::hash(b"hashed").as_bytes()));
        assert_eq!(buffer.base_bytes(), buffer.model().to_source_bytes());
        let reopened = buffer.reopen();
        assert_eq!(reopened.reason, TransitionReason::Reopened);
        assert_eq!(buffer.state(), BufferState::Clean);
        let saved = buffer.handle_save_completed_revision(buffer.revision(), b"disk");
        assert_eq!(saved.to, BufferState::Clean);
        assert_eq!(buffer.base_bytes(), b"disk");
        assert!(format!("{buffer}").starts_with("Buffer(id="));

        assert!(BufferState::Clean.allows_autosave());
        assert!(BufferState::Clean.allows_manual_save());
        assert!(BufferState::Orphaned.allows_autosave());
        assert!(BufferState::Orphaned.needs_resolution());
        assert_eq!(BufferState::Dirty.to_string(), "Dirty");
        assert_eq!(
            StateTransition::new(
                BufferState::Clean,
                BufferState::Dirty,
                TransitionReason::Merge
            )
            .reason,
            TransitionReason::Merge
        );
    }

    #[test]
    fn d7_table_clean_changed() {
        // Clean | changed → Silent reload. Map cursor and scroll through the diff
        let mut buffer = Buffer::from_text("hello");
        let _transition = buffer.check_external_change(b"hello world");

        assert_eq!(buffer.state(), BufferState::Conflicted);
        // This would normally trigger a silent reload in the UI
    }

    #[test]
    fn d7_table_clean_deleted() {
        // Clean | deleted → Keep pane open as *Orphaned*, banner "file deleted"
        let mut buffer = Buffer::from_text("hello");
        let _transition = buffer.handle_file_deleted();

        assert_eq!(buffer.state(), BufferState::Orphaned);
    }

    #[test]
    fn d7_table_dirty_changed() {
        // Dirty | changed → Conflict banner
        let mut buffer = Buffer::from_text("hello");
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        let _transition = buffer.check_external_change(b"goodbye");
        assert_eq!(buffer.state(), BufferState::Conflicted);
    }

    #[test]
    fn d7_table_dirty_changed_to_identical() {
        // Dirty | changed to *identical* content as buffer → Auto-resolve → Clean
        let mut buffer = Buffer::from_text("hello");
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        let _transition = buffer.check_external_change(b"hello world");
        assert_eq!(buffer.state(), BufferState::Clean);
    }

    #[test]
    fn d7_table_dirty_deleted() {
        // Dirty | deleted → Recreate · Save as · Discard
        let mut buffer = Buffer::from_text("hello");
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        let _transition = buffer.handle_file_deleted();
        assert_eq!(buffer.state(), BufferState::Orphaned);
    }

    #[test]
    fn d7_table_conflicted() {
        // Conflicted state blocks autosave and manual save
        let mut buffer = Buffer::from_text("hello");
        buffer.check_external_change(b"goodbye");

        assert!(buffer.is_conflicted());
        assert!(!buffer.allows_autosave());
        assert!(!buffer.allows_manual_save());
        assert!(buffer.needs_resolution());
    }
}
