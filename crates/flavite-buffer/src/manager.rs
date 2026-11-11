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

use flavite_syntax::text::{TextError, TextSnapshot};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// Retained-history budget applied to buffers created after it is set
    /// (FEUX-0002 E-12/X01: the harness shrinks budgets at startup, before
    /// any note opens; `None` keeps the model defaults for every buffer).
    default_history_budget: Option<crate::budget::Budget>,
    /// Recovery interval in seconds.
    recovery_interval_secs: u64,
    /// RFC-0005 S5: monotonic counter bumped on every transition that can
    /// change the graph buffer stamp (revision, state, path, base hash, the
    /// open set). Consumers (the graph service's wake loop) compare it
    /// against a value captured at their last pump — cheap, lock-free
    /// "stamp unchanged" without materializing the stamp. Over-bumping is
    /// safe (one extra pump at worst); never bumping would strand a stale
    /// dirty-overlay until the sweep. S6's `BufferSignal` fires at exactly
    /// these choke points.
    graph_stamp: Arc<AtomicU64>,
    /// RFC-0005 S6: the lossy stamp-transition wake channel
    /// (`flavite_vault::session::BufferSignal`'s sending end, installed by
    /// `Session::new`). Every `touch_graph_stamp` also `try_send`s here:
    /// bounded(1), drop-on-full — a wake only says "a transition happened";
    /// consumers re-read the stamp themselves. Absent for bare managers
    /// (CLI/test pools): the bump alone is the contract.
    graph_stamp_notifier: Option<crossbeam::channel::Sender<()>>,
}

impl BufferManager {
    /// Create a new BufferManager.
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffers: HashMap::new(),
            buffer_ids: HashMap::new(),
            default_coalesce_window: std::time::Duration::from_millis(500),
            default_history_budget: None,
            recovery_interval_secs: 2, // Default 2 seconds
            graph_stamp: Arc::new(AtomicU64::new(0)),
            graph_stamp_notifier: None,
        }
    }

    /// Create a new BufferManager with custom settings.
    #[must_use]
    pub fn with_settings(coalesce_window: std::time::Duration, recovery_interval: u64) -> Self {
        Self {
            buffers: HashMap::new(),
            buffer_ids: HashMap::new(),
            default_coalesce_window: coalesce_window,
            default_history_budget: None,
            recovery_interval_secs: recovery_interval,
            graph_stamp: Arc::new(AtomicU64::new(0)),
            graph_stamp_notifier: None,
        }
    }

    /// Set the retained-history budget applied to every buffer created after
    /// this call (test/harness tuning alongside `Session`'s per-buffer
    /// application to already-open buffers; FEUX-0002 X01).
    pub fn set_default_history_budget(&mut self, budget: crate::budget::Budget) {
        self.default_history_budget = Some(budget);
        for buffer in self.buffers.values_mut() {
            buffer.model_mut().set_history_budget(budget);
        }
    }

    /// The shared graph-stamp counter handle. `Session` clones this once at
    /// construction so stamp-version reads never need the manager lock
    /// (RFC-0005 §5.2's "read without an actor hop").
    #[must_use]
    pub fn graph_stamp_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.graph_stamp)
    }

    /// Install the graph-stamp wake notifier (the sending end of
    /// `BufferSignal`). Called once by `Session::new`; a manager without a
    /// session (or with the watch taken by a test spy) simply bumps the
    /// stamp version and fires no wake — lossy by design (RFC-0005 §4.2).
    pub fn set_graph_stamp_notifier(&mut self, notifier: crossbeam::channel::Sender<()>) {
        self.graph_stamp_notifier = Some(notifier);
    }

    /// Test seam (RFC-0005 S6): a manager whose stamp transitions fire an
    /// externally held watch. The spy receives `()` wakes from the shared
    /// bounded(1) slot — coalescing under bursts is expected and harmless:
    /// wakes only advertise, the stamp version carries the truth. (A
    /// manager wrapped in a `Session` gets the session's own notifier and
    /// hands its watch out through `Session::graph_buffer_watch`.)
    #[must_use]
    pub fn with_buffer_spy() -> (Self, crossbeam::channel::Receiver<()>) {
        let mut manager = Self::new();
        let (tx, rx) = crossbeam::channel::bounded(1);
        manager.graph_stamp_notifier = Some(tx);
        (manager, rx)
    }

    /// Record a stamp-relevant transition: bump the version and advertise a
    /// wake. Conservative by construction — called at every manager
    /// mutation choke point, including mutable-handle hand-offs whose
    /// caller may or may not touch the stamp inputs. Over-notifying is
    /// safe (the consumer's debounce coalesces and the rebuild dedups);
    /// under-notifying would strand a stale dirty-overlay until the sweep.
    fn touch_graph_stamp(&self) {
        self.graph_stamp.fetch_add(1, Ordering::Relaxed);
        if let Some(notifier) = &self.graph_stamp_notifier {
            // Drop-on-full: latest-wins coalescing, exactly the
            // GenerationSignal discipline (§4.2). Never blocks.
            let _ = notifier.try_send(());
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

    /// Get a buffer for a note mutably. Handing out a mutable reference is
    /// itself a stamp-relevant event (RFC-0005 S5 audit: every workspace
    /// mutation of a `Buffer` enters through this door).
    #[must_use]
    pub fn get_buffer_mut(&mut self, note_path: &PathBuf) -> Option<&mut Buffer> {
        self.touch_graph_stamp();
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

    /// Get a buffer by buffer ID mutably. Stamp-relevant hand-off, same door
    /// contract as [`Self::get_buffer_mut`].
    #[must_use]
    pub fn get_buffer_by_id_mut(&mut self, buffer_id: BufferId) -> Option<&mut Buffer> {
        self.touch_graph_stamp();
        let path = self.buffer_ids.get(&buffer_id)?;
        self.buffers.get_mut(path)
    }

    /// Open a note and return its buffer.
    ///
    /// If the note is already open, returns the existing buffer.
    /// Otherwise, creates a new buffer from the note's content.
    ///
    /// # Errors
    ///
    /// Strict-decode failure under `policy`: NO buffer is created or
    /// replaced, and the typed codec error is returned so the caller can
    /// present an encoding choice or read-only inspection instead of a
    /// silently-empty editable note (§6.10, FR-22/24 — the retired
    /// empty-buffer fallback is a gate-forbidden pattern).
    pub fn open_note(
        &mut self,
        note_path: PathBuf,
        content: &[u8],
        policy: flavite_encoding::DecodePolicy,
    ) -> Result<(&Buffer, Option<StateTransition>), flavite_encoding::CodecError> {
        // Decode before touching any state: an existing buffer is never
        // replaced on decode failure and a new one is never half-created.
        let snapshot = TextSnapshot::with_policy(content, policy).map_err(|error| match error {
            TextError::Codec(err) => err,
        })?;
        self.touch_graph_stamp();
        // Use entry API to avoid borrow issues
        let entry = self.buffers.entry(note_path.clone());

        match entry {
            std::collections::hash_map::Entry::Occupied(occupied) => {
                // Buffer already exists, return it
                Ok((occupied.into_mut(), None))
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                // Create new buffer from the one authoritative decode.
                let mut buffer = Buffer::from_snapshot(snapshot);

                // Configure the buffer
                buffer
                    .model_mut()
                    .set_coalesce_window(self.default_coalesce_window);
                if let Some(budget) = self.default_history_budget {
                    buffer.model_mut().set_history_budget(budget);
                }

                // Get buffer_id
                let buffer_id = buffer.buffer_id();

                // Insert into both maps (the open-set change stamped at
                // the top of this method).
                self.buffer_ids.insert(buffer_id, vacant.key().clone());
                let buffer_ref = vacant.insert(buffer);
                Ok((buffer_ref, None))
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
            self.touch_graph_stamp();
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
        self.touch_graph_stamp();
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
    /// This is called by the VaultActor when it detects a file change. The
    /// bytes are strictly decoded under `policy`; a malformed or ambiguous
    /// source leaves every buffer untouched and is returned as `Err` for the
    /// caller to surface an actionable encoding choice (§6.10, FR-24).
    pub fn notify_external_change(
        &mut self,
        note_path: &PathBuf,
        new_content: &[u8],
        policy: flavite_encoding::DecodePolicy,
    ) -> Result<Vec<StateTransition>, flavite_encoding::CodecError> {
        let mut transitions = Vec::new();

        self.touch_graph_stamp();
        if let Some(buffer) = self.buffers.get_mut(note_path) {
            match buffer.check_external_change(new_content, policy) {
                crate::state::ExternalDecode::Decoded(Some(transition)) => {
                    transitions.push(transition);
                }
                crate::state::ExternalDecode::Decoded(None) => {}
                crate::state::ExternalDecode::InvalidEncoding(error) => {
                    return Err(error);
                }
            }
        }

        Ok(transitions)
    }

    /// Notify the manager that a file was deleted.
    pub fn notify_file_deleted(&mut self, note_path: &PathBuf) -> Vec<StateTransition> {
        let mut transitions = Vec::new();

        self.touch_graph_stamp();
        if let Some(buffer) = self.buffers.get_mut(note_path) {
            let transition = buffer.handle_file_deleted();
            transitions.push(transition);
        }

        transitions
    }

    /// Notify the manager that a file was recreated.
    ///
    /// The bytes are strictly decoded under `policy`; malformed/ambiguous
    /// content never replaces the buffer and surfaces as `Err`.
    pub fn notify_file_recreated(
        &mut self,
        note_path: &PathBuf,
        new_content: &[u8],
        policy: flavite_encoding::DecodePolicy,
    ) -> Result<Vec<StateTransition>, flavite_encoding::CodecError> {
        let mut transitions = Vec::new();

        self.touch_graph_stamp();
        if let Some(buffer) = self.buffers.get_mut(note_path) {
            match buffer.handle_file_recreated(new_content, policy) {
                crate::state::ExternalDecode::Decoded(Some(transition)) => {
                    transitions.push(transition);
                }
                crate::state::ExternalDecode::Decoded(None) => {}
                crate::state::ExternalDecode::InvalidEncoding(error) => {
                    return Err(error);
                }
            }
        }

        Ok(transitions)
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
        self.touch_graph_stamp();
        self.buffer_ids.insert(id, to.clone());
        self.buffers.insert(to, buffer);
        true
    }

    /// Handle a save completion for a buffer.
    pub fn notify_save_completed(&mut self, note_path: &PathBuf) -> Vec<StateTransition> {
        let mut transitions = Vec::new();

        self.touch_graph_stamp();
        if let Some(buffer) = self.buffers.get_mut(note_path) {
            let transition = buffer.handle_save_completed();
            transitions.push(transition);
        }

        transitions
    }

    /// Attach an already strictly decoded snapshot (RFC-0006 §6.10): the
    /// decode happened exactly once in the caller's codec contract, so this
    /// path never re-normalizes and can never fail. An existing buffer is
    /// returned unchanged.
    pub fn open_note_snapshot(
        &mut self,
        note_path: PathBuf,
        snapshot: flavite_syntax::text::TextSnapshot,
    ) -> (&mut Buffer, Option<StateTransition>) {
        self.touch_graph_stamp();
        match self.buffers.entry(note_path.clone()) {
            std::collections::hash_map::Entry::Occupied(occupied) => {
                // Buffer already exists, return it
                (occupied.into_mut(), None)
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                let mut buffer = Buffer::from_snapshot(snapshot);
                buffer
                    .model_mut()
                    .set_coalesce_window(self.default_coalesce_window);
                if let Some(budget) = self.default_history_budget {
                    buffer.model_mut().set_history_budget(budget);
                }
                let buffer_id = buffer.buffer_id();
                self.buffer_ids.insert(buffer_id, vacant.key().clone());
                let buffer_ref = vacant.insert(buffer);
                (buffer_ref, None)
            }
        }
    }

    /// Get all open note paths.
    #[must_use]
    pub fn open_notes(&self) -> Vec<&PathBuf> {
        self.buffers.keys().collect()
    }

    /// Return every open path and its stable buffer identity.
    #[must_use]
    pub fn open_paths(&self) -> Vec<(PathBuf, BufferId)> {
        self.buffers
            .iter()
            .map(|(path, buffer)| (path.clone(), buffer.buffer_id()))
            .collect()
    }

    /// Get all buffers.
    #[must_use]
    pub fn all_buffers(&self) -> Vec<&Buffer> {
        self.buffers.values().collect()
    }

    /// Get all buffers, mutably.
    #[must_use]
    pub fn all_buffers_mut(&mut self) -> Vec<&mut Buffer> {
        self.buffers.values_mut().collect()
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
        self.touch_graph_stamp();
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
        let (buffer, _) = manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();

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
        let (buffer, _) = manager
            .open_note(
                from.clone(),
                b"content",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();
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
        manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();

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
        manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        manager.apply_edit(
            &path,
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " mine".into()),
        );

        // External change
        let transitions = manager
            .notify_external_change(&path, b"goodbye", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, BufferState::Conflicted);
    }

    #[test]
    fn save_completed_notification() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();

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
        manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();

        let transitions = manager.notify_file_deleted(&path);
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, BufferState::Orphaned);
    }

    #[test]
    fn file_recreated_notification() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        manager.notify_file_deleted(&path);

        let transitions = manager
            .notify_file_recreated(&path, b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].to, BufferState::Clean);
    }

    #[test]
    fn dirty_buffers() {
        let mut manager = BufferManager::new();

        let path1 = PathBuf::from("test1.md");
        let path2 = PathBuf::from("test2.md");

        manager
            .open_note(
                path1.clone(),
                b"hello",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();
        manager
            .open_note(
                path2.clone(),
                b"world",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();

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
        let (buffer, _) = manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
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
        manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();

        // Open again
        let (buffer, _) = manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        assert_eq!(buffer.text(), "hello");
        assert_eq!(manager.buffer_count(), 1);
    }

    #[test]
    fn has_buffers_needing_resolution() {
        let mut manager = BufferManager::new();

        let path = PathBuf::from("test.md");
        manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();

        assert!(!manager.has_buffers_needing_resolution());

        // Create a conflict
        manager.apply_edit(
            &path,
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " mine".into()),
        );
        manager
            .notify_external_change(&path, b"goodbye", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        assert!(manager.has_buffers_needing_resolution());
    }

    #[test]
    fn all_buffers() {
        let mut manager = BufferManager::new();

        let path1 = PathBuf::from("test1.md");
        let path2 = PathBuf::from("test2.md");

        manager
            .open_note(
                path1.clone(),
                b"hello",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();
        manager
            .open_note(
                path2.clone(),
                b"world",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();

        let buffers = manager.all_buffers();
        assert_eq!(buffers.len(), 2);
    }

    #[test]
    fn open_notes() {
        let mut manager = BufferManager::new();

        let path1 = PathBuf::from("test1.md");
        let path2 = PathBuf::from("test2.md");

        manager
            .open_note(
                path1.clone(),
                b"hello",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();
        manager
            .open_note(
                path2.clone(),
                b"world",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();

        let paths = manager.open_notes();
        assert_eq!(paths.len(), 2);
    }

    #[test]
    fn open_paths_returns_path_and_buffer_identity() {
        let mut manager = BufferManager::new();
        let path = PathBuf::from("note.md");
        let (buffer, _) = manager
            .open_note(path.clone(), b"hello", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        let id = buffer.buffer_id();
        assert_eq!(manager.open_paths(), vec![(path, id)]);
    }

    #[test]
    fn id_routes_invalid_utf8_and_missing_notifications_are_safe() {
        let mut manager = BufferManager::with_settings(std::time::Duration::from_millis(10), 7);
        let path = PathBuf::from("invalid.md");
        // A malformed source is refused with a typed error and NO buffer is
        // installed — the retired empty-buffer fallback is forbidden (§6.10).
        let err = manager
            .open_note(path.clone(), &[0xff], flavite_encoding::DecodePolicy::Auto)
            .expect_err("malformed bytes must not install a buffer");
        assert!(matches!(
            err,
            flavite_encoding::CodecError::InvalidUtf8 { .. }
        ));
        assert!(manager.get_buffer(&path).is_none());
        assert_eq!(manager.buffer_count(), 0);

        // A valid buffer still routes by id and reports dirty.
        let good = PathBuf::from("good.md");
        let (buffer, _) = manager
            .open_note(good.clone(), b"hi", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        let id = buffer.buffer_id();
        assert_eq!(buffer.text(), "hi");
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
                .notify_external_change(
                    &PathBuf::from("missing.md"),
                    b"x",
                    flavite_encoding::DecodePolicy::Auto
                )
                .unwrap()
                .is_empty()
        );
        assert!(
            manager
                .notify_file_deleted(&PathBuf::from("missing.md"))
                .is_empty()
        );
        assert!(
            manager
                .notify_file_recreated(
                    &PathBuf::from("missing.md"),
                    b"x",
                    flavite_encoding::DecodePolicy::Auto
                )
                .unwrap()
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
        manager
            .open_note(
                PathBuf::from("clear.md"),
                b"x",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();
        manager.clear();
        assert_eq!(manager.buffer_count(), 0);
        assert!(manager.get_buffer_by_id(id).is_none());
    }

    #[test]
    fn stamp_transitions_bump_the_version_and_fire_the_spy_watch() {
        use crate::undo::EditOp;
        let (mut manager, spy) = BufferManager::with_buffer_spy();
        let version = manager.graph_stamp_handle();
        assert_eq!(version.load(Ordering::Relaxed), 0);
        assert!(spy.try_recv().is_err(), "no wake before any transition");

        let (buffer, _) = manager
            .open_note(
                PathBuf::from("s.md"),
                b"body",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();
        let id = buffer.buffer_id();
        assert_eq!(version.load(Ordering::Relaxed), 1, "open bumps");
        assert!(spy.recv_timeout(std::time::Duration::from_secs(1)).is_ok());

        manager
            .apply_edit_by_id(
                id,
                EditOp::new(
                    flavite_syntax::text::Span::new(TextByte(0), TextByte(0)),
                    "x".into(),
                ),
            )
            .expect("edit applies");
        assert_eq!(version.load(Ordering::Relaxed), 2, "edit bumps");
        assert!(spy.recv_timeout(std::time::Duration::from_secs(1)).is_ok());

        // A burst of transitions through the mutable-handle door: the
        // version keeps climbing, the bounded(1) slot never grows past
        // one pending wake (latest-wins coalescing, §4.2 discipline).
        for _ in 0..50 {
            assert!(manager.get_buffer_by_id_mut(id).is_some());
        }
        assert!(version.load(Ordering::Relaxed) > 50);
        let mut woken = 0;
        while spy.try_recv().is_ok() {
            woken += 1;
        }
        assert!(woken <= 1, "lossy slot must never queue a backlog: {woken}");

        manager.close_note(&PathBuf::from("s.md"));
        assert!(
            spy.recv_timeout(std::time::Duration::from_secs(1)).is_ok(),
            "close wakes"
        );

        // A manager without a notifier (bare pool, session-less) still
        // bumps the version — the counter is the authority, wakes are
        // latency only.
        let mut plain = BufferManager::new();
        let plain_version = plain.graph_stamp_handle();
        plain
            .open_note(
                PathBuf::from("p.md"),
                b"x",
                flavite_encoding::DecodePolicy::Auto,
            )
            .unwrap();
        assert_eq!(plain_version.load(Ordering::Relaxed), 1);
    }
}
