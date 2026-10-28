//! BufferManager — owns one Buffer per open note (§4.14, D14, D7).
//!
//! The BufferManager is owned by the Session and:
//! - Manages the lifetime of buffers
//! - Routes commands to the appropriate buffer
//! - Tracks buffer state changes
//! - Handles buffer open/close
//!
//! Widgets receive read-only views; ownership and the command protocol
//! are defined in §4.5. All document mutations execute as session commands
//! and apply `EditOp`s through its BufferManager.

use flavite_syntax::text::TextSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

use crate::revision::{BufferId, Revision};
use crate::state::{Buffer, BufferState, StateTransition};
use crate::undo::EditOp;

/// Event emitted when a buffer's state changes.
#[derive(Debug, Clone, PartialEq)]
pub struct BufferStateChanged {
    /// The note reference (path).
    pub note_path: PathBuf,
    /// The buffer identity.
    pub buffer_id: BufferId,
    /// The new revision.
    pub revision: Revision,
    /// The new state.
    pub state: BufferState,
    /// The transition that occurred, if any.
    pub transition: Option<StateTransition>,
}

impl BufferStateChanged {
    /// Create a new buffer state changed event.
    #[must_use]
    pub fn new(
        note_path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        state: BufferState,
        transition: Option<StateTransition>,
    ) -> Self {
        Self {
            note_path,
            buffer_id,
            revision,
            state,
            transition,
        }
    }
}

/// Reference to a note (path + optional stable_id).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NoteRef {
    /// The path to the note.
    pub path: PathBuf,
    /// Optional stable ID for the note.
    pub stable_id: Option<String>,
}

impl NoteRef {
    /// Create a new note reference from a path.
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            stable_id: None,
        }
    }

    /// Create a new note reference with a stable ID.
    #[must_use]
    pub fn with_stable_id(path: PathBuf, stable_id: String) -> Self {
        Self {
            path,
            stable_id: Some(stable_id),
        }
    }
}

/// The BufferManager owns one Buffer per open note.
///
/// It is owned by the Session and provides the interface for:
/// - Opening and closing buffers
/// - Applying edits to buffers
/// - Querying buffer state
/// - Receiving external change notifications
#[derive(Debug)]
pub struct BufferManager {
    /// Map from note path to buffer.
    buffers: HashMap<PathBuf, Buffer>,
    /// Map from buffer ID to note path (for reverse lookup).
    buffer_ids: HashMap<BufferId, PathBuf>,
    /// Default coalesce window for new buffers.
    default_coalesce_window: std::time::Duration,
    /// Recovery interval in seconds.
    recovery_interval_secs: u64,
}

impl BufferManager {
    /// Create a new BufferManager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            buffer_ids: HashMap::new(),
            default_coalesce_window: std::time::Duration::from_millis(500),
            recovery_interval_secs: 2, // Default 2 seconds
        }
    }

    /// Create a new BufferManager with custom settings.
    #[must_use]
    pub fn with_settings(coalesce_window: std::time::Duration, recovery_interval: u64) -> Self {
        Self {
            buffers: HashMap::new(),
            buffer_ids: HashMap::new(),
            default_coalesce_window: coalesce_window,
            recovery_interval_secs: recovery_interval,
        }
    }

    /// Get the recovery interval in seconds.
    #[must_use]
    pub const fn recovery_interval(&self) -> u64 {
        self.recovery_interval_secs
    }

    /// Set the recovery interval.
    pub fn set_recovery_interval(&mut self, interval: u64) {
        self.recovery_interval_secs = interval;
    }

    /// Check if a buffer is open for a note.
    #[must_use]
    pub fn has_buffer(&self, note_path: &PathBuf) -> bool {
        self.buffers.contains_key(note_path)
    }

    /// Get a buffer for a note.
    #[must_use]
    pub fn get_buffer(&self, note_path: &PathBuf) -> Option<&Buffer> {
        self.buffers.get(note_path)
    }

    /// Get a buffer for a note mutably.
    #[must_use]
    pub fn get_buffer_mut(&mut self, note_path: &PathBuf) -> Option<&mut Buffer> {
        self.buffers.get_mut(note_path)
    }

    /// Get a buffer by buffer ID.
    #[must_use]
    pub fn get_buffer_by_id(&self, buffer_id: BufferId) -> Option<&Buffer> {
        let path = self.buffer_ids.get(&buffer_id)?;
        self.buffers.get(path)
    }

    /// Return the vault-relative note path associated with a live buffer.
    #[must_use]
    pub fn path_for_id(&self, buffer_id: BufferId) -> Option<&PathBuf> {
        self.buffer_ids.get(&buffer_id)
    }

    /// Get a buffer by buffer ID mutably.
    #[must_use]
    pub fn get_buffer_by_id_mut(&mut self, buffer_id: BufferId) -> Option<&mut Buffer> {
        let path = self.buffer_ids.get(&buffer_id)?;
        self.buffers.get_mut(path)
    }

    /// Open a note and return its buffer.
    ///
    /// If the note is already open, returns the existing buffer.
    /// Otherwise, creates a new buffer from the note's content.
    ///
    /// Returns the buffer and any state transition that occurred.
    pub fn open_note(
        &mut self,
        note_path: PathBuf,
        content: &[u8],
    ) -> (&Buffer, Option<StateTransition>) {
        // Use entry API to avoid borrow issues
        let entry = self.buffers.entry(note_path.clone());

        match entry {
            std::collections::hash_map::Entry::Occupied(occupied) => {
                // Buffer already exists, return it
                (occupied.into_mut(), None)
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                // Create new buffer
                let mut buffer = match TextSnapshot::new(content) {
                    Ok(snapshot) => Buffer::from_snapshot(snapshot),
                    Err(_) => {
                        // Invalid UTF-8, create empty buffer
                        Buffer::new_empty()
                    }
                };

                // Configure the buffer
                buffer
                    .model_mut()
                    .set_coalesce_window(self.default_coalesce_window);

                // Get buffer_id
                let buffer_id = buffer.buffer_id();

                // Insert into both maps
                self.buffer_ids.insert(buffer_id, vacant.key().clone());
                let buffer_ref = vacant.insert(buffer);
                (buffer_ref, None)
            }
        }
    }

    /// Close a note and return its buffer.
    ///
    /// The buffer is removed from the manager. If the buffer is dirty,
    /// a recovery file should be written before closing (handled by the caller).
    ///
    /// Returns the closed buffer, if any.
    pub fn close_note(&mut self, note_path: &PathBuf) -> Option<Buffer> {
        if let Some(buffer) = self.buffers.remove(note_path) {
            self.buffer_ids.remove(&buffer.buffer_id());
            Some(buffer)
        } else {
            None
        }
    }
    pub fn close_buffer(&mut self, buffer_id: BufferId) -> Option<Buffer> {
        let path = self.buffer_ids.get(&buffer_id)?.clone();
        self.close_note(&path)
    }

    /// Apply an edit to a buffer.
    ///
    /// Returns the new revision, the buffer's new state, and any state transition.
    pub fn apply_edit(
        &mut self,
        note_path: &PathBuf,
        edit: EditOp,
    ) -> Option<(Revision, BufferState, Option<StateTransition>)> {
        let buffer = self.buffers.get_mut(note_path)?;
        let (rev, transition) = buffer.apply_local_edit(edit);
        Some((rev, buffer.state(), transition))
    }

    /// Apply an edit to a buffer by ID.
    pub fn apply_edit_by_id(
        &mut self,
        buffer_id: BufferId,
        edit: EditOp,
    ) -> Option<(Revision, BufferState, Option<StateTransition>)> {
        let path = self.buffer_ids.get(&buffer_id)?.clone();
        self.apply_edit(&path, edit)
    }

    /// Notify the manager of an external change to a note.
    ///
    /// This is called by the VaultActor when it detects a file change.
    /// Returns any state transitions that occurred.
    pub fn notify_external_change(
        &mut self,
        note_path: &PathBuf,
        new_content: &[u8],
    ) -> Vec<StateTransition> {
        let mut transitions = Vec::new();

        if let Some(buffer) = self.buffers.get_mut(note_path)
            && let Some(transition) = buffer.check_external_change(new_content)
        {
            transitions.push(transition);
        }

        transitions
    }

    /// Notify the manager that a file was deleted.
    pub fn notify_file_deleted(&mut self, note_path: &PathBuf) -> Vec<StateTransition> {
        let mut transitions = Vec::new();

        if let Some(buffer) = self.buffers.get_mut(note_path) {
            let transition = buffer.handle_file_deleted();
            transitions.push(transition);
        }

        transitions
    }

    /// Notify the manager that a file was recreated.
    pub fn notify_file_recreated(
        &mut self,
        note_path: &PathBuf,
        new_content: &[u8],
    ) -> Vec<StateTransition> {
        let mut transitions = Vec::new();

        if let Some(buffer) = self.buffers.get_mut(note_path)
            && let Some(transition) = buffer.handle_file_recreated(new_content)
        {
            transitions.push(transition);
        }

        transitions
    }

    /// Re-key an open buffer after an authoritative T0–T2 rename while
    /// preserving its buffer identity, revision, and undo history.
    pub fn rename_buffer(&mut self, from: &PathBuf, to: PathBuf) -> bool {
        if self.buffers.contains_key(&to) {
            return false;
        }
        let Some(buffer) = self.buffers.remove(from) else {
            return false;
        };
        let id = buffer.buffer_id();
        self.buffer_ids.insert(id, to.clone());
        self.buffers.insert(to, buffer);
        true
    }

    /// Handle a save completion for a buffer.
    pub fn notify_save_completed(&mut self, note_path: &PathBuf) -> Vec<StateTransition> {
        let mut transitions = Vec::new();

        if let Some(buffer) = self.buffers.get_mut(note_path) {
            let transition = buffer.handle_save_completed();
            transitions.push(transition);
        }

        transitions
    }

    /// Get all open note paths.
    #[must_use]
    pub fn open_notes(&self) -> Vec<&PathBuf> {
        self.buffers.keys().collect()
    }

    /// Get all buffers.
    #[must_use]
    pub fn all_buffers(&self) -> Vec<&Buffer> {
        self.buffers.values().collect()
    }

    /// Get dirty buffers.
    #[must_use]
    pub fn dirty_buffers(&self) -> Vec<(&PathBuf, &Buffer)> {
        self.buffers
            .iter()
            .filter(|(_, buffer)| buffer.is_dirty())
            .collect()
    }

    /// Get conflicted buffers.
    #[must_use]
    pub fn conflicted_buffers(&self) -> Vec<(&PathBuf, &Buffer)> {
        self.buffers
            .iter()
            .filter(|(_, buffer)| buffer.is_conflicted())
            .collect()
    }

    /// Get orphaned buffers.
    #[must_use]
    pub fn orphaned_buffers(&self) -> Vec<(&PathBuf, &Buffer)> {
        self.buffers
            .iter()
            .filter(|(_, buffer)| buffer.is_orphaned())
            .collect()
    }

    /// Check if any buffer needs resolution.
    #[must_use]
    pub fn has_buffers_needing_resolution(&self) -> bool {
        self.buffers
            .values()
            .any(|buffer| buffer.needs_resolution())
    }

    /// Get the number of open buffers.
    #[must_use]
    pub fn buffer_count(&self) -> usize {
        self.buffers.len()
    }

    /// Clear all buffers.
    pub fn clear(&mut self) {
        self.buffers.clear();
        self.buffer_ids.clear();
    }
}

impl Default for BufferManager {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for BufferManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "BufferManager(buffers={}, dirty={}, conflicted={}, orphaned={})",
            self.buffer_count(),
            self.dirty_buffers().len(),
            self.conflicted_buffers().len(),
            self.orphaned_buffers().len()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_syntax::text::{Span, TextByte};
    use std::path::PathBuf;

    #[test]
    fn open_and_close() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");

        // Open the note
        let (buffer, _) = manager.open_note(path.clone(), b"hello");

        // Check buffer text immediately
        assert_eq!(buffer.text(), "hello");

        // Now check if buffer exists (separate statement to avoid borrow issues)
        assert!(manager.has_buffer(&path));

        let closed = manager.close_note(&path);
        assert!(closed.is_some());
        assert!(!manager.has_buffer(&path));
    }

    #[test]
    fn rename_buffer_preserves_identity_and_updates_reverse_lookup() {
        let mut manager = BufferManager::new();
        let from = PathBuf::from("old.md");
        let to = PathBuf::from("new.md");
        let (buffer, _) = manager.open_note(from.clone(), b"content");
        let id = buffer.buffer_id();
        assert!(manager.rename_buffer(&from, to.clone()));
        assert!(!manager.has_buffer(&from));
        assert_eq!(manager.path_for_id(id), Some(&to));
        assert_eq!(manager.get_buffer(&to).unwrap().buffer_id(), id);
    }

    #[test]
    fn apply_edit() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager.open_note(path.clone(), b"hello");

        let edit = EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());

        let result = manager.apply_edit(&path, edit);
        assert!(result.is_some());

        let (_, state, _) = result.unwrap();
        assert_eq!(state, BufferState::Dirty);
    }

    #[test]
    fn external_change_notification() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager.open_note(path.clone(), b"hello");
        manager.apply_edit(
            &path,
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " mine".into()),
        );

        // External change
        let transitions = manager.notify_external_change(&path, b"goodbye");
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, BufferState::Conflicted);
    }

    #[test]
    fn save_completed_notification() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager.open_note(path.clone(), b"hello");

        // Make it dirty
        let edit = EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        manager.apply_edit(&path, edit);

        // Save completed
        let transitions = manager.notify_save_completed(&path);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, BufferState::Clean);
    }

    #[test]
    fn file_deleted_notification() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager.open_note(path.clone(), b"hello");

        let transitions = manager.notify_file_deleted(&path);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, BufferState::Orphaned);
    }

    #[test]
    fn file_recreated_notification() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager.open_note(path.clone(), b"hello");
        manager.notify_file_deleted(&path);

        let transitions = manager.notify_file_recreated(&path, b"hello");
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, BufferState::Clean);
    }

    #[test]
    fn dirty_buffers() {
        let mut manager = BufferManager::new();

        let path1 = PathBuf::from("test1.md");
        let path2 = PathBuf::from("test2.md");

        manager.open_note(path1.clone(), b"hello");
        manager.open_note(path2.clone(), b"world");

        // Make one dirty
        let edit = EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        manager.apply_edit(&path1, edit);

        let dirty = manager.dirty_buffers();
        assert_eq!(dirty.len(), 1);
        assert!(dirty[0].0 == &path1);
    }

    #[test]
    fn buffer_by_id() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        let (buffer, _) = manager.open_note(path.clone(), b"hello");
        let buffer_id = buffer.buffer_id();

        let retrieved = manager.get_buffer_by_id(buffer_id);
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().text(), "hello");
    }

    #[test]
    fn recovery_interval() {
        let manager = BufferManager::with_settings(std::time::Duration::from_millis(500), 5);
        assert_eq!(manager.recovery_interval(), 5);
    }

    #[test]
    fn open_existing_buffer() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager.open_note(path.clone(), b"hello");

        // Open again
        let (buffer, _) = manager.open_note(path.clone(), b"hello");
        assert_eq!(buffer.text(), "hello");
        assert_eq!(manager.buffer_count(), 1);
    }

    #[test]
    fn has_buffers_needing_resolution() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager.open_note(path.clone(), b"hello");

        assert!(!manager.has_buffers_needing_resolution());

        // Create a conflict
        manager.apply_edit(
            &path,
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " mine".into()),
        );
        manager.notify_external_change(&path, b"goodbye");
        assert!(manager.has_buffers_needing_resolution());
    }

    #[test]
    fn all_buffers() {
        let mut manager = BufferManager::new();

        let path1 = PathBuf::from("test1.md");
        let path2 = PathBuf::from("test2.md");

        manager.open_note(path1.clone(), b"hello");
        manager.open_note(path2.clone(), b"world");

        let buffers = manager.all_buffers();
        assert_eq!(buffers.len(), 2);
    }

    #[test]
    fn open_notes() {
        let mut manager = BufferManager::new();

        let path1 = PathBuf::from("test1.md");
        let path2 = PathBuf::from("test2.md");

        manager.open_note(path1.clone(), b"hello");
        manager.open_note(path2.clone(), b"world");

        let paths = manager.open_notes();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn id_routes_invalid_utf8_and_missing_notifications_are_safe() {
        let mut manager = BufferManager::with_settings(std::time::Duration::from_millis(10), 7);
        let path = PathBuf::from("invalid.md");
        let (buffer, _) = manager.open_note(path.clone(), &[0xff]);
        let id = buffer.buffer_id();
        assert_eq!(buffer.text(), "");
        assert!(manager.get_buffer_by_id_mut(id).is_some());
        assert!(
            manager
                .apply_edit_by_id(
                    id,
                    EditOp::new(Span::new(TextByte(0), TextByte(0)), "ok".into())
                )
                .is_some()
        );
        assert_eq!(manager.dirty_buffers().len(), 1);
        assert!(
            manager
                .notify_external_change(&PathBuf::from("missing.md"), b"x")
                .is_empty()
        );
        assert!(
            manager
                .notify_file_deleted(&PathBuf::from("missing.md"))
                .is_empty()
        );
        assert!(
            manager
                .notify_file_recreated(&PathBuf::from("missing.md"), b"x")
                .is_empty()
        );
        assert!(
            manager
                .notify_save_completed(&PathBuf::from("missing.md"))
                .is_empty()
        );
        assert!(manager.close_buffer(id).is_some());
        assert!(manager.get_buffer_by_id(id).is_none());
        assert!(manager.close_buffer(id).is_none());
        assert!(
            manager
                .apply_edit_by_id(
                    id,
                    EditOp::new(Span::new(TextByte(0), TextByte(0)), "x".into())
                )
                .is_none()
        );
        assert!(manager.get_buffer(&path).is_none());
        assert_eq!(manager.recovery_interval(), 7);
        assert_eq!(
            format!("{manager}"),
            "BufferManager(buffers=0, dirty=0, conflicted=0, orphaned=0)"
        );
        manager.open_note(PathBuf::from("clear.md"), b"x");
        manager.clear();
        assert_eq!(manager.buffer_count(), 0);
        assert!(manager.get_buffer_by_id(id).is_none());
    }
}
