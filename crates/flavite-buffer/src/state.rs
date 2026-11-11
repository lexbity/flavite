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

use flavite_encoding::{DecodePolicy, SourceFormat};
use flavite_syntax::text::{TextError, TextSnapshot};
use serde::{Deserialize, Serialize};
use std::fmt;

use crate::model::EditorModel;
use crate::revision::{BaseHash, BufferId, Revision};

/// The buffer state as defined by D7.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// Descriptor the base bytes were encoded under (§6.10: merge decodes
    /// base/mine/theirs under their own captured descriptors, never a
    /// current guess).
    base_format: Option<SourceFormat>,
    /// The disk hash at the time of the last disk check.
    /// Used to detect external changes.
    disk_hash: Option<BaseHash>,
    /// A replacement reached disk but its durability/index milestone failed.
    /// Matching those bytes must not make history navigation mark us clean.
    save_requires_completion: bool,
}

/// Outcome of an external observation that had to be strictly decoded
/// (RFC-0006 §6.10). A malformed or ambiguous source NEVER replaces existing
/// buffer text: the typed codec error is exposed for an actionable
/// `EncodingChoiceRequired`/`InvalidEncoding` presentation instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalDecode {
    /// Strict decode succeeded; carries the state transition if one occurred.
    Decoded(Option<StateTransition>),
    /// Strict decode failed under the applied policy. Buffer state is
    /// unchanged; the error kind/position is enough for the UI repair flow
    /// (no bytes are replaced with U+FFFD, FR-22).
    InvalidEncoding(flavite_encoding::CodecError),
}

impl Buffer {
    /// Create a new buffer from a text snapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: TextSnapshot) -> Self {
        let model = EditorModel::from_snapshot(snapshot.clone());
        let hash = BaseHash::from_snapshot(&snapshot);
        let base_format = snapshot.format();

        Self {
            model,
            state: BufferState::Clean,
            base_hash: Some(hash),
            base_bytes: snapshot.source().to_vec(),
            base_format,
            disk_hash: Some(hash),
            save_requires_completion: false,
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

    /// Descriptor the current merge base bytes were encoded under.
    #[must_use]
    pub const fn base_format(&self) -> Option<SourceFormat> {
        self.base_format
    }

    /// Record the exact source bytes known to be the current disk base.
    pub fn set_base_snapshot(&mut self, bytes: Vec<u8>) {
        let hash = BaseHash::from_bytes(*blake3::hash(&bytes).as_bytes());
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = bytes;
        // The base carries the buffer's authoritative descriptor.
        self.base_format = Some(self.model.source_format());
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

    /// Apply a validated multi-selection command as one edit and one undo
    /// unit, then update the D7 state in the same transition.
    /// Apply a batch under an explicit undo policy (RFC-0006 D-06):
    /// `Isolated` prepares and installs one boundary candidate (invalid
    /// batches change nothing), `Typing` keeps the existing coalescing
    /// path. A `BatchError` refusal is complete — zero state change.
    pub fn apply_local_edits_with_policy(
        &mut self,
        edits: &[crate::undo::EditOp],
        policy: crate::undo::UndoPolicy,
    ) -> Result<(Revision, Option<StateTransition>), crate::model::BatchError> {
        let revision = self.model.try_apply_batch(edits, policy)?;
        let old_state = self.state;
        self.state = BufferState::Dirty;
        let transition = (old_state != BufferState::Dirty).then(|| {
            StateTransition::new(old_state, BufferState::Dirty, TransitionReason::LocalEdit)
        });
        Ok((revision, transition))
    }

    pub fn apply_recorded_edits(
        &mut self,
        edits: &[crate::undo::EditOp],
        policy: crate::undo::UndoPolicy,
        record: crate::history::EditorEdit,
    ) -> Result<Revision, crate::model::RecordedBatchError> {
        let revision = self.model.try_apply_recorded_batch(edits, policy, record)?;
        if matches!(self.state, BufferState::Clean | BufferState::Dirty) {
            self.state = BufferState::Dirty;
        }
        Ok(revision)
    }

    pub fn restore_editing_state(
        &mut self,
        target: crate::history::HistoryTarget,
    ) -> Result<Revision, crate::history::HistoryError> {
        let revision = self.model.restore_state(target)?;
        self.sync_history_state();
        Ok(revision)
    }

    pub fn undo(&mut self) -> Option<Revision> {
        let revision = self.model.undo()?;
        self.sync_history_state();
        Some(revision)
    }

    pub fn redo(&mut self) -> Option<Revision> {
        let revision = self.model.redo()?;
        self.sync_history_state();
        Some(revision)
    }

    fn sync_history_state(&mut self) {
        let dirty = self.save_requires_completion || self.base_hash != Some(self.content_hash());
        if let Some(hash) = self.base_hash {
            self.model.set_base_hash(hash);
        }
        self.model.set_dirty(dirty);
        if matches!(self.state, BufferState::Clean | BufferState::Dirty) {
            self.state = if dirty {
                BufferState::Dirty
            } else {
                BufferState::Clean
            };
        }
    }

    pub fn apply_local_edits(
        &mut self,
        edits: &[crate::undo::EditOp],
    ) -> Option<(Revision, Option<StateTransition>)> {
        if edits.is_empty() {
            return Some((self.revision(), None));
        }
        if !self.model.edit_ops_valid(edits) {
            return None;
        }
        let old_state = self.state;
        let revision = self.model.apply_edit_ops(edits);
        let new_state = BufferState::Dirty;
        self.state = new_state;
        let transition = (old_state != new_state)
            .then(|| StateTransition::new(old_state, new_state, TransitionReason::LocalEdit));
        Some((revision, transition))
    }

    /// Check for external changes.
    ///
    /// This is called when the watcher detects a change on disk.
    ///
    /// The observed bytes are strictly decoded under `policy`: clean buffers
    /// reload only after a successful decode (adopting an unambiguously new
    /// source format), and malformed or ambiguous input never replaces the
    /// existing text — it is exposed as `InvalidEncoding` for an actionable
    /// choice at the UI (§6.10, FR-22/24).
    #[allow(clippy::needless_pass_by_value)]
    pub fn check_external_change(
        &mut self,
        new_disk_bytes: &[u8],
        policy: DecodePolicy,
    ) -> ExternalDecode {
        let old_state = self.state;

        // Strict decode under the applied policy; failure is typed, never lossy.
        let new_snapshot = match TextSnapshot::with_policy(new_disk_bytes, policy) {
            Ok(snapshot) => snapshot,
            Err(TextError::Codec(codec)) => return ExternalDecode::InvalidEncoding(codec),
        };
        let new_disk_hash = BaseHash::from_snapshot(&new_snapshot);
        self.disk_hash = Some(new_disk_hash);

        // Check if the disk content matches our base hash
        if let Some(base_hash) = self.base_hash
            && base_hash == new_disk_hash
        {
            // Disk content is the same as what we loaded
            // This can happen with metadata-only changes
            return ExternalDecode::Decoded(None);
        }

        // Disk content has changed
        let new_state = match old_state {
            BufferState::Clean => {
                // A clean buffer has no local edits to protect. Reload the
                // changed bytes silently and advance its disk base (D7). The
                // snapshot's descriptor is adopted: an unambiguous format
                // change may be taken silently only while clean (§6.10).
                self.model.replace_snapshot(new_snapshot.clone());
                self.save_requires_completion = false;
                self.base_hash = Some(new_disk_hash);
                self.base_bytes = new_disk_bytes.to_vec();
                self.base_format = new_snapshot.format();
                BufferState::Clean
            }
            BufferState::Dirty => {
                // We were dirty, disk changed
                // Check if our buffer matches the new disk content
                if self.model.matches_hash(new_disk_hash) {
                    // Auto-resolve to Clean
                    BufferState::Clean
                } else {
                    // Conflict — including a dirty external FORMAT change:
                    // resolution must decide the output format explicitly
                    // (merge flow), never silently (D-15).
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
            ExternalDecode::Decoded(Some(transition))
        } else {
            ExternalDecode::Decoded(None)
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
    /// Called when a deleted file reappears. Strictly decodes the observed
    /// bytes; malformed/ambiguous content leaves state untouched and is
    /// exposed as `InvalidEncoding` (§6.10).
    pub fn handle_file_recreated(
        &mut self,
        new_bytes: &[u8],
        policy: DecodePolicy,
    ) -> ExternalDecode {
        let old_state = self.state;

        let snapshot = match TextSnapshot::with_policy(new_bytes, policy) {
            Ok(snapshot) => snapshot,
            Err(TextError::Codec(err)) => return ExternalDecode::InvalidEncoding(err),
        };
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
            ExternalDecode::Decoded(Some(transition))
        } else {
            ExternalDecode::Decoded(None)
        }
    }

    /// User chose to reload from disk.
    ///
    /// Replaces the buffer content with the current disk content, strictly
    /// decoded under `policy`. A decode failure means NO replacement: the
    /// existing text is kept (§6.10).
    pub fn reload_from_disk(
        &mut self,
        disk_bytes: &[u8],
        policy: DecodePolicy,
    ) -> Option<StateTransition> {
        let old_state = self.state;

        if let Ok(snapshot) = TextSnapshot::with_policy(disk_bytes, policy) {
            let new_hash = BaseHash::from_snapshot(&snapshot);

            // Replace the model
            self.model.replace_snapshot(snapshot);
            self.save_requires_completion = false;
            self.disk_hash = Some(new_hash);
            self.base_hash = Some(new_hash);
            self.base_bytes = disk_bytes.to_vec();
            self.base_format = Some(self.model.source_format());
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
    ///
    /// The recovered source bytes decode under their recorded descriptor
    /// (`recovered_policy` — the recovery record's authoritative format),
    /// never a current guess; the disk base is decoded under its own
    /// captured descriptor (`disk_policy`). A decode failure on either side
    /// leaves the buffer untouched (§6.10).
    pub fn restore_recovery(
        &mut self,
        recovered_bytes: &[u8],
        recovered_policy: DecodePolicy,
        disk_bytes: &[u8],
        recovered_base_hash: Option<BaseHash>,
        disk_policy: DecodePolicy,
    ) -> Option<StateTransition> {
        let recovered = TextSnapshot::with_policy(recovered_bytes, recovered_policy).ok()?;
        let disk = TextSnapshot::with_policy(disk_bytes, disk_policy).ok()?;
        let old_state = self.state;
        let recovered_hash = BaseHash::from_snapshot(&recovered);
        let disk_hash = BaseHash::from_snapshot(&disk);
        self.model.replace_snapshot(recovered);
        self.save_requires_completion = false;
        self.base_hash = Some(disk_hash);
        self.disk_hash = Some(disk_hash);
        self.base_bytes = disk_bytes.to_vec();
        self.base_format = disk.format();
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

    /// Reopen one shared buffer under a new interpretation of the same disk
    /// bytes (RFC-0006 §6.10). The caller has already validated the decode:
    /// the snapshot carries the authoritative descriptor, the buffer keeps
    /// its identity, selections are remapped, and the Undo tree restarts
    /// from a clean disk-backed root — never an edit masquerading as
    /// previously saved text.
    pub fn reopen_with_snapshot(
        &mut self,
        snapshot: TextSnapshot,
        disk_bytes: &[u8],
    ) -> StateTransition {
        let old_state = self.state;
        self.model.replace_snapshot(snapshot);
        self.save_requires_completion = false;
        self.base_hash = self.model.base_hash();
        self.disk_hash = self.base_hash;
        self.base_bytes = disk_bytes.to_vec();
        self.base_format = Some(self.model.source_format());
        self.state = BufferState::Clean;
        StateTransition::new(old_state, BufferState::Clean, TransitionReason::Reopened)
    }

    /// Resolve a detected conflict by retaining mine against the exact disk
    /// bytes observed and snapshotted by the caller (decoded under
    /// `disk_policy`; a decode failure performs no transition).
    pub fn resolve_keep_local(
        &mut self,
        disk_bytes: &[u8],
        disk_policy: DecodePolicy,
    ) -> Option<StateTransition> {
        let old_state = self.state;
        let snapshot = TextSnapshot::with_policy(disk_bytes, disk_policy).ok()?;
        let hash = BaseHash::from_snapshot(&snapshot);
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = disk_bytes.to_vec();
        self.base_format = snapshot.format();
        self.save_requires_completion = false;
        self.state = BufferState::Dirty;
        Some(StateTransition::new(
            old_state,
            BufferState::Dirty,
            TransitionReason::KeepLocal,
        ))
    }

    /// Install an explicitly resolved merge against the exact disk snapshot
    /// that was reviewed. The caller must durably snapshot the displaced
    /// buffer and verify the disk hash before invoking this transition.
    ///
    /// The merged result is already canonical UTF-8 (`merged_bytes`); the disk
    /// base is decoded under `disk_policy` so a BOM-less UTF-16 base maps back
    /// to the right source bytes (§6.10). Returns `None` (no partial write,
    /// no state change) if either decode fails or the merged result is too
    /// large.
    pub fn apply_resolved_merge(
        &mut self,
        merged_bytes: &[u8],
        disk_bytes: &[u8],
        disk_policy: DecodePolicy,
    ) -> Option<Revision> {
        if merged_bytes.len() > 32 * 1024 * 1024 {
            return None;
        }
        let merged = TextSnapshot::new(merged_bytes).ok()?;
        let disk = TextSnapshot::with_policy(disk_bytes, disk_policy).ok()?;
        self.base_hash = Some(BaseHash::from_snapshot(&disk));
        self.disk_hash = self.base_hash;
        self.base_bytes = disk_bytes.to_vec();
        self.base_format = disk.format();
        let revision = self.model.apply_snapshot_boundary(merged);
        self.save_requires_completion = false;
        self.state = if self
            .model
            .try_to_source_bytes()
            .is_ok_and(|bytes| bytes == disk_bytes)
        {
            BufferState::Clean
        } else {
            BufferState::Dirty
        };
        Some(revision)
    }

    /// User chose to discard local changes.
    ///
    /// Discards local changes and reloads from disk, strictly decoded under
    /// `policy`. A malformed disk source is NOT adopted and the buffer is NOT
    /// replaced: the typed codec error is returned with the buffer untouched
    /// so the caller can present the encoding decision (§6.10, FR-22 — an
    /// invalid source never constructs an empty buffer, S00).
    ///
    /// # Errors
    ///
    /// The typed [`flavite_encoding::CodecError`] of the strict decode; no
    /// state change occurs.
    pub fn discard(
        &mut self,
        disk_bytes: Option<&[u8]>,
        policy: DecodePolicy,
    ) -> Result<StateTransition, flavite_encoding::CodecError> {
        let old_state = self.state;

        if let Some(bytes) = disk_bytes {
            let snapshot =
                TextSnapshot::with_policy(bytes, policy).map_err(|TextError::Codec(e)| e)?;
            let new_hash = BaseHash::from_snapshot(&snapshot);
            let format = snapshot.format();

            self.model = EditorModel::from_snapshot(snapshot);
            self.save_requires_completion = false;
            self.base_hash = Some(new_hash);
            self.disk_hash = Some(new_hash);
            self.base_bytes = bytes.to_vec();
            self.base_format = format;
            self.state = BufferState::Clean;

            return Ok(StateTransition::new(
                old_state,
                BufferState::Clean,
                TransitionReason::Discard,
            ));
        }

        // No disk content available: reset to a fresh untitled buffer.
        self.model = EditorModel::new_empty();
        self.save_requires_completion = false;
        self.base_hash = None;
        self.base_format = None;
        self.disk_hash = None;
        self.state = BufferState::Clean;

        Ok(StateTransition::new(
            old_state,
            BufferState::Clean,
            TransitionReason::Discard,
        ))
    }

    /// Handle successful save.
    ///
    /// Called after a save completes successfully.
    /// Returns the state transition.
    pub fn handle_save_completed(&mut self) -> StateTransition {
        let old_state = self.state;

        // Update base hash to match current content
        self.base_hash = Some(self.content_hash());
        self.base_bytes = self.model.source_bytes_for_hash();
        self.base_format = Some(self.model.source_format());
        // Disk hash now matches base hash
        self.disk_hash = self.base_hash;

        self.save_requires_completion = false;
        if let Some(hash) = self.base_hash {
            self.model.set_base_hash(hash);
        }
        self.model.set_dirty(false);

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
        self.base_format = Some(self.model.source_format());

        self.save_requires_completion = false;
        self.model.set_base_hash(hash);
        let new_state = if self.model.revision() == saved_revision {
            BufferState::Clean
        } else {
            BufferState::Dirty
        };
        self.model.set_dirty(new_state == BufferState::Dirty);
        self.state = new_state;
        StateTransition::new(old_state, new_state, TransitionReason::SaveCompleted)
    }

    /// Record bytes known to have reached disk after a save whose later
    /// durability/index stage failed. The buffer remains dirty until the full
    /// save milestone is acknowledged, but retries compare against this disk
    /// state instead of blindly replaying an obsolete base precondition.
    pub fn record_replaced_disk_hash(&mut self, hash: BaseHash) {
        self.require_save_completion();
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = self.model.source_bytes_for_hash();
        self.base_format = Some(self.model.source_format());
    }

    /// Record a particular saved revision as the disk base without replacing
    /// the current buffer text, which may contain newer edits.
    pub fn record_replaced_disk_snapshot(&mut self, bytes: &[u8]) {
        self.require_save_completion();
        let hash = BaseHash::from_bytes(*blake3::hash(bytes).as_bytes());
        self.base_hash = Some(hash);
        self.disk_hash = Some(hash);
        self.base_bytes = bytes.to_vec();
        self.base_format = Some(self.model.source_format());
    }

    fn require_save_completion(&mut self) {
        self.save_requires_completion = true;
        self.model.set_dirty(true);
        if self.state == BufferState::Clean {
            self.state = BufferState::Dirty;
        }
    }

    /// Mark a dirty buffer conflicted from a verified disk hash when the
    /// worker can report identity but cannot return the current bytes in the
    /// save completion. Reload/merge obtains bytes through a separate worker
    /// read before making a destructive choice.
    pub fn mark_conflicted_disk_hash(&mut self, hash: [u8; 32]) {
        self.disk_hash = Some(BaseHash::from_bytes(hash));
        if self.state != BufferState::Orphaned && self.base_hash != Some(BaseHash::from_bytes(hash))
        {
            self.state = BufferState::Conflicted;
        }
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
    fn resolved_merge_preserves_exact_snapshot_eols_and_disk_base() {
        let mine = b"mine\r\nkeep\n";
        let theirs = b"base\r\nkeep\nexternal\r\n";
        let merged = b"mine\r\nkeep\nexternal\r\n";
        let mut buffer = Buffer::from_snapshot(TextSnapshot::new(mine).unwrap());

        let revision = buffer
            .apply_resolved_merge(merged, theirs, flavite_encoding::DecodePolicy::Auto)
            .unwrap();

        assert!(revision > Revision::INITIAL);
        assert_eq!(buffer.model().to_source_bytes(), merged);
        assert_eq!(buffer.base_bytes(), theirs);
        assert_eq!(buffer.state(), BufferState::Dirty);
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
        let transition =
            buffer.check_external_change(new_content, flavite_encoding::DecodePolicy::Auto);

        // Should auto-resolve to Clean
        assert_eq!(buffer.state(), BufferState::Clean);
        let ExternalDecode::Decoded(transition) = transition else {
            panic!("valid UTF-8 must decode");
        };
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
        buffer.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(5), TextByte(5)),
            " mine".into(),
        ));

        // External change to different content
        let new_content = b"goodbye";
        let transition =
            buffer.check_external_change(new_content, flavite_encoding::DecodePolicy::Auto);

        assert_eq!(buffer.state(), BufferState::Conflicted);
        let ExternalDecode::Decoded(transition) = transition else {
            panic!("valid UTF-8 must decode");
        };
        assert_eq!(
            transition,
            Some(StateTransition::new(
                BufferState::Dirty,
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

        let transition =
            buffer.handle_file_recreated(b"hello", flavite_encoding::DecodePolicy::Auto);
        assert_eq!(buffer.state(), BufferState::Clean);
        let ExternalDecode::Decoded(Some(transition)) = transition else {
            panic!("recreation must transition");
        };
        assert_eq!(transition.to, BufferState::Clean);
    }

    #[test]
    fn reload_from_disk() {
        let mut buffer = Buffer::from_text("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // Reload from disk
        let _transition = buffer.reload_from_disk(b"hello", flavite_encoding::DecodePolicy::Auto);
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.text(), "hello");
    }

    #[test]
    fn resolve_keep_local_restores_the_disk_base() {
        let mut buffer = Buffer::from_text("hello");

        // Make it dirty
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        // External change
        buffer.check_external_change(b"goodbye", flavite_encoding::DecodePolicy::Auto);
        assert_eq!(buffer.state(), BufferState::Conflicted);

        // Resolve against the observed disk bytes
        let _transition = buffer
            .resolve_keep_local(b"goodbye", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        assert_eq!(buffer.state(), BufferState::Dirty);
        assert_eq!(buffer.base_bytes(), b"goodbye");
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
        buffer
            .discard(Some(b"hello"), flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.text(), "hello");

        // An invalid disk source refuses the discard with the typed error:
        // the buffer is never replaced with an empty one (S00, §6.10).
        let mut conflicted = Buffer::from_text("hello");
        conflicted.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(5), TextByte(5)),
            " mine".into(),
        ));
        conflicted.check_external_change(b"goodbye", flavite_encoding::DecodePolicy::Auto);
        assert!(matches!(
            conflicted.discard(Some(&[0xff]), flavite_encoding::DecodePolicy::Auto),
            Err(flavite_encoding::CodecError::InvalidUtf8 { .. })
        ));
        assert_eq!(conflicted.text(), "hello mine");
        assert_eq!(conflicted.state(), BufferState::Conflicted);
        assert_eq!(conflicted.base_bytes(), b"hello");

        // Discarding a buffer with no disk file resets to a fresh untitled
        // buffer.
        let mut untitled = Buffer::from_text("scratch");
        untitled
            .discard(None, flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        assert!(untitled.is_empty());
        assert_eq!(untitled.state(), BufferState::Clean);
        assert!(untitled.base_hash().is_none());
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
                flavite_encoding::DecodePolicy::Auto,
                b"disk",
                Some(BaseHash::from_bytes(*blake3::hash(b"disk").as_bytes())),
                flavite_encoding::DecodePolicy::Auto,
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
                flavite_encoding::DecodePolicy::Auto,
                b"disk changed",
                Some(BaseHash::from_bytes(*blake3::hash(b"disk").as_bytes())),
                flavite_encoding::DecodePolicy::Auto,
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
        buffer.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(5), TextByte(5)),
            " mine".into(),
        ));
        buffer.check_external_change(b"goodbye", flavite_encoding::DecodePolicy::Auto);
        assert_eq!(buffer.state(), BufferState::Conflicted);
        assert!(!buffer.allows_autosave());
    }

    #[test]
    fn manual_save_disabled_in_conflicted_and_orphaned() {
        let mut buffer = Buffer::from_text("hello");

        // Conflicted
        buffer.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(5), TextByte(5)),
            " mine".into(),
        ));
        buffer.check_external_change(b"goodbye", flavite_encoding::DecodePolicy::Auto);
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
        assert_eq!(
            empty.check_external_change(b"base", flavite_encoding::DecodePolicy::Auto),
            ExternalDecode::Decoded(None)
        );
        // Malformed disk bytes are a TYPED invalid-encoding outcome and
        // never a silent no-op that could later be mistaken for "unchanged".
        assert!(matches!(
            empty.check_external_change(&[0xff], flavite_encoding::DecodePolicy::Auto),
            ExternalDecode::InvalidEncoding(_)
        ));
        assert!(matches!(
            empty.handle_file_recreated(&[0xff], flavite_encoding::DecodePolicy::Auto),
            ExternalDecode::InvalidEncoding(_)
        ));
        assert!(
            empty
                .reload_from_disk(&[0xff], flavite_encoding::DecodePolicy::Auto)
                .is_none()
        );
        assert!(
            empty
                .resolve_keep_local(&[0xff], flavite_encoding::DecodePolicy::Auto)
                .is_none()
        );

        let mut buffer = Buffer::from_text("base");
        buffer.handle_file_deleted();
        assert!(buffer.is_orphaned());
        let ExternalDecode::Decoded(Some(recreated)) =
            buffer.handle_file_recreated(b"other", flavite_encoding::DecodePolicy::Auto)
        else {
            panic!("valid UTF-8 recreation must transition");
        };
        assert_eq!(recreated.to, BufferState::Conflicted);
        assert!(buffer.is_conflicted());
        assert!(buffer.needs_resolution());
        let resolved = buffer
            .resolve_keep_local(b"other", flavite_encoding::DecodePolicy::Auto)
            .unwrap();
        assert_eq!(resolved.reason, TransitionReason::KeepLocal);
        assert_eq!(buffer.base_bytes(), b"other");
        assert_eq!(buffer.state(), BufferState::Dirty);

        buffer.record_replaced_disk_snapshot(b"saved");
        assert_eq!(buffer.base_bytes(), b"saved");
        buffer.record_replaced_disk_hash(BaseHash::from_bytes(*blake3::hash(b"hashed").as_bytes()));
        assert_eq!(buffer.base_bytes(), buffer.model().to_source_bytes());
        let reopened = buffer.reopen_with_snapshot(TextSnapshot::new(b"base").unwrap(), b"base");
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
        // Clean | changed → silent reload.
        let mut buffer = Buffer::from_text("hello");
        let buffer_id = buffer.buffer_id();
        let revision = buffer.revision();
        buffer.model_mut().set_primary_selection(3, 3);
        let _transition =
            buffer.check_external_change(b"hello world", flavite_encoding::DecodePolicy::Auto);

        assert_eq!(buffer.state(), BufferState::Clean);
        assert_eq!(buffer.model().text(), "hello world");
        assert_eq!(buffer.base_bytes(), b"hello world");
        assert_eq!(buffer.buffer_id(), buffer_id);
        assert!(buffer.revision() > revision);
        assert_eq!(buffer.model().primary_selection().head(), 3);
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

        let _transition =
            buffer.check_external_change(b"goodbye", flavite_encoding::DecodePolicy::Auto);
        assert_eq!(buffer.state(), BufferState::Conflicted);
    }

    #[test]
    fn d7_table_dirty_changed_to_identical() {
        // Dirty | changed to *identical* content as buffer → Auto-resolve → Clean
        let mut buffer = Buffer::from_text("hello");
        let edit =
            crate::undo::EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        buffer.apply_local_edit(edit);

        let _transition =
            buffer.check_external_change(b"hello world", flavite_encoding::DecodePolicy::Auto);
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
        buffer.apply_local_edit(crate::undo::EditOp::new(
            Span::new(TextByte(5), TextByte(5)),
            " mine".into(),
        ));
        buffer.check_external_change(b"goodbye", flavite_encoding::DecodePolicy::Auto);

        assert!(buffer.is_conflicted());
        assert!(!buffer.allows_autosave());
        assert!(!buffer.allows_manual_save());
        assert!(buffer.needs_resolution());
    }
}
