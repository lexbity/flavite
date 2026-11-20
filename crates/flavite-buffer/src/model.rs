//! EditorModel — the headless text buffer (§4.14, D14).
//!
//! This is the core of the buffer crate: a pure, testable, headless model
//! that owns:
//! - A persistent COW chunk-tree root (RFC-0011 D-01) as the single text
//!   representation — no `String` mirror, no rope
//! - Multi-selection support
//! - Undo/redo via an undo tree whose nodes hold immutable version roots
//! - Per-line EOL metadata carried on the version
//! - One authoritative [`SourceFormat`] descriptor plus its [`FormatStamp`]
//!   (RFC-0006 D-15): the save format is buffer state, not EOL-table state
//!   and not a filesystem guess.
//!
//! All editing operations produce `EditOp`s and are applied through
//! the model. The model maintains revision numbers and can provide
//! revisioned read views.
//!
//! Per-keystroke work is O(log n + chunk + window): the keystroke path
//! NEVER materializes the full document. Text accessors (`text()`,
//! `slice_str()`, `snapshot()`, `try_to_source_bytes()`) materialize on
//! demand and are user-paced APIs.

use flavite_encoding::{
    CodecError, DecodeLimits, DecodeMap, EncodeLimits, FormatStamp, SourceFormat,
    encode as codec_encode,
};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use std::cell::OnceCell;
use std::sync::Arc;
use std::time::Duration;

use crate::budget::Budget;
use crate::eol::{EolTable, LineEnding};
use crate::revision::{BaseHash, BufferId, Revision};
use crate::selection::{RawSelection, Selection, SelectionSet};
use crate::undo::{EditOp, UndoTree, VersionState};

/// The headless editor model.
///
/// This is the core buffer type that owns the text, selections, undo history,
/// and EOL metadata. It is pure and has zero UI dependencies (D14).
#[derive(Debug)]
pub struct EditorModel {
    history_epoch: crate::history::HistoryEpoch,
    history_movement: Option<crate::history::HistoryMovement>,
    /// Unique identity for this buffer instance.
    buffer_id: BufferId,
    /// The immutable current document version: chunk-tree root + EOL state
    /// + save descriptor (RFC-0011 D-01/D-04).
    version: Arc<VersionState>,
    /// Lazy full-text materialization cache for the `&str` accessors. This
    /// is a derived convenience cache, NOT a second representation: it is
    /// invalidated on every mutation and never touched by the keystroke
    /// path.
    cached_text: OnceCell<String>,
    /// Multi-selection: the canonical committed set (FEUX-0002 E-4).
    /// `None` means the default single caret at offset 0.
    selections: Option<SelectionSet>,
    /// The undo tree for edit history (nodes hold version roots).
    undo_tree: UndoTree,
    /// Current revision number.
    revision: Revision,
    /// Base hash of the content when last loaded or saved.
    base_hash: Option<BaseHash>,
    /// Whether the buffer has unsaved changes.
    is_dirty: bool,
    /// Operation represented by the most recent undo/redo transition.
    last_transition_ops: Vec<EditOp>,
}

/// Why a prepared batch was refused (typed, RFC-0006 D-06). The refusal
/// happens before any state change: tree, EOL table, revision and the Undo
/// tree are untouched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchError {
    /// The batch contains no edits.
    Empty,
    /// A range endpoint is outside the current text.
    OutOfBounds { at: usize, len: usize },
    /// A range endpoint is not a UTF-8 scalar boundary.
    CharBoundary { at: usize },
    /// Two edits overlap after ordering (or two empty edits share a start).
    Overlapping { at: usize },
    /// The retained-history budget would be exceeded. Refused before any
    /// mutation (FEUX-0002 X01 / E-12).
    LimitExceeded { needed: u64, budget: u64 },
}

/// Refusal of a recorded (editor-annotated) batch. The refusal happens
/// before any state change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordedBatchError {
    /// The edit batch itself was refused.
    Batch(BatchError),
    /// The recorded editor metadata (origin, selections) was invalid.
    InvalidForm(String),
    /// The retained-history budget would be exceeded. Refused before any
    /// mutation.
    LimitExceeded { needed: u64, budget: u64 },
}

impl From<BatchError> for RecordedBatchError {
    fn from(error: BatchError) -> Self {
        match error {
            BatchError::LimitExceeded { needed, budget } => Self::LimitExceeded { needed, budget },
            other => Self::Batch(other),
        }
    }
}

impl std::fmt::Display for RecordedBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Batch(error) => write!(f, "Invalid edit batch: {error:?}"),
            Self::InvalidForm(message) => f.write_str(message),
            Self::LimitExceeded { needed, budget } => write!(
                f,
                "History memory budget exceeded: this edit needs ~{needed} bytes; the limit is {budget} bytes. Reduce or save the note to free history memory."
            ),
        }
    }
}

/// A fully prepared batch candidate (D-06/TD-7): the single combined edit
/// plus the after-state geometry, both computed off the interactive turn
/// from immutable state. The install is O(edit) and infallible. No
/// full-document text is materialized (RFC-0011 S2: the full after-state
/// text copy is deleted).
#[derive(Debug)]
pub struct PreparedBatch {
    /// The combined, boundary-isolated edit over the prepare-time text.
    pub op: EditOp,
    /// The after-state byte length (used for recorded validation and delta
    /// accounting without building a snapshot).
    pub after_len: usize,
    /// The original per-op batch geometry in pre-change coordinates
    /// (FEUX-0002 E-13): retained on the undo node so movement across the
    /// transition maps carets in unchanged gaps exactly.
    pub spans: Vec<crate::selection::SpanEdit>,
}

impl EditorModel {
    /// Create a new editor model from a text snapshot.
    ///
    /// A disk-backed snapshot contributes its authoritative descriptor; a
    /// synthetic normalized view defaults to UTF-8 without BOM and never
    /// claims another format (§6.10).
    #[must_use]
    pub fn from_snapshot(snapshot: TextSnapshot) -> Self {
        let snapshot = if snapshot.format().is_none() {
            TextSnapshot::from_normalized_with_descriptor(snapshot.text(), SourceFormat::default())
        } else {
            snapshot
        };
        let version = Arc::new(VersionState::from_snapshot(&snapshot));
        let base_hash = BaseHash::from_snapshot(&snapshot);
        Self {
            history_epoch: crate::history::HistoryEpoch::new(),
            history_movement: None,
            buffer_id: BufferId::new(),
            version,
            cached_text: OnceCell::new(),
            selections: None,
            undo_tree: UndoTree::new(snapshot),
            revision: Revision::INITIAL,
            base_hash: Some(base_hash),
            is_dirty: false,
            last_transition_ops: Vec::new(),
        }
    }

    /// Build a version from a snapshot, preserving the current descriptor
    /// when the snapshot is a synthetic normalized view (no format).
    fn version_from_snapshot_preserving_descriptor(
        &self,
        snapshot: &TextSnapshot,
    ) -> Arc<VersionState> {
        if snapshot.format().is_some() {
            return Arc::new(VersionState::from_snapshot(snapshot));
        }
        let mut state = VersionState::from_snapshot(snapshot);
        state.format = self.version.format;
        state.stamp = self.version.stamp;
        Arc::new(state)
    }

    /// Replace the document snapshot while preserving buffer identity and
    /// advancing its revision. Cursor positions are mapped through the common
    /// prefix/suffix, with positions inside the changed range clamped to the
    /// replacement. Used for silent reloads and durable recovery.
    pub fn replace_snapshot(&mut self, snapshot: TextSnapshot) {
        let old_text = self.text().to_owned();
        let new_text = snapshot.text().to_owned();
        let (prefix, suffix) = common_text_edges(&old_text, &new_text);
        let old_middle_end = old_text.len().saturating_sub(suffix);
        let new_middle_end = new_text.len().saturating_sub(suffix);
        if let Some(set) = self.selections.take() {
            let candidates = set
                .members()
                .iter()
                .map(|selection| RawSelection {
                    id: Some(selection.id().as_raw()),
                    anchor: map_position(
                        TextByte(selection.anchor()),
                        prefix,
                        old_middle_end,
                        new_middle_end,
                        old_text.len(),
                        &new_text,
                    )
                    .0,
                    head: map_position(
                        TextByte(selection.head()),
                        prefix,
                        old_middle_end,
                        new_middle_end,
                        old_text.len(),
                        &new_text,
                    )
                    .0,
                })
                .collect();
            if let Ok((mapped, _)) = SelectionSet::normalize(&new_text, candidates, None) {
                self.selections = Some(mapped);
            }
        }
        self.version = self.version_from_snapshot_preserving_descriptor(&snapshot);
        self.cached_text.take();
        self.undo_tree = UndoTree::new(snapshot.clone());
        self.history_epoch = crate::history::HistoryEpoch::new();
        self.history_movement = None;
        self.revision = self.revision.next();
        self.base_hash = Some(BaseHash::from_snapshot(&snapshot));
        self.is_dirty = false;
    }

    /// Create a new empty editor model (UTF-8, no BOM, LF).
    #[must_use]
    pub fn new_empty() -> Self {
        let snapshot = TextSnapshot::new(b"").unwrap();
        Self::from_snapshot(snapshot)
    }

    /// Create a new editor model with plain text (no EOL info, defaults to LF
    /// and UTF-8 without BOM).
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        // Valid UTF-8 input cannot fail strict decode under Auto: the UTF-32
        // BOM prefixes are not valid UTF-8 sequences, so no codec error path
        // is reachable here.
        let snapshot = TextSnapshot::new(text.as_bytes())
            .unwrap_or_else(|_| TextSnapshot::from_normalized(text));
        Self::from_snapshot(snapshot)
    }

    /// The authoritative save-format descriptor.
    #[must_use]
    pub fn source_format(&self) -> SourceFormat {
        self.version.format
    }

    /// Descriptor provenance (codec version, hint generation).
    #[must_use]
    pub fn format_stamp(&self) -> &FormatStamp {
        &self.version.stamp
    }

    /// Record hint-generation provenance without touching text or revision
    /// (used when the buffer was opened under a confirmed hint).
    pub fn set_format_stamp_generation(&mut self, hint_generation: Option<u64>) {
        let mut version = (*self.version).clone();
        version.stamp.hint_generation = hint_generation;
        self.version = Arc::new(version);
    }

    /// Get the buffer identity.
    #[must_use]
    pub const fn buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    /// Get the current revision.
    #[must_use]
    pub const fn revision(&self) -> Revision {
        self.revision
    }

    /// Get the base hash.
    #[must_use]
    pub fn base_hash(&self) -> Option<BaseHash> {
        self.base_hash
    }

    /// Check if the buffer is dirty (has unsaved changes).
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.is_dirty
    }

    /// Get the text content as a string slice. Materializes the full
    /// document on first access after a mutation (user-paced; never called
    /// on the keystroke path).
    #[must_use]
    pub fn text(&self) -> &str {
        self.cached_text
            .get_or_init(|| self.version.materialize())
            .as_str()
    }

    /// Get the text length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.version.len()
    }

    /// Check if the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get a character at a position.
    #[must_use]
    pub fn char_at(&self, pos: TextByte) -> Option<char> {
        if pos.0 >= self.len() || !self.is_char_boundary(pos.0) {
            return None;
        }
        self.version
            .root
            .slice(pos.0..(pos.0 + 4).min(self.len()))
            .chars()
            .next()
    }

    /// Get a slice of text (fresh allocation).
    #[must_use]
    pub fn slice(&self, range: Span<TextByte>) -> String {
        self.version.root.slice(range.start.0..range.end.0)
    }

    /// Get a slice of text as a string (materializes via the cache;
    /// test/diagnostic use).
    #[must_use]
    pub fn slice_str(&self, range: Span<TextByte>) -> &str {
        let text = self.text();
        &text[range.start.0..range.end.0]
    }

    /// Get the line count.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.version.root.line_count()
    }

    /// Get the byte offset of a line.
    #[must_use]
    pub fn line_start(&self, line: usize) -> TextByte {
        TextByte(self.version.root.line_start_of(line))
    }

    /// Get the byte offset just past the end of a line.
    #[must_use]
    pub fn line_end(&self, line: usize) -> TextByte {
        if line + 1 < self.line_count() {
            self.line_start(line + 1)
        } else {
            TextByte(self.len())
        }
    }

    /// Get the line at a byte offset.
    #[must_use]
    pub fn line_at(&self, pos: TextByte) -> usize {
        self.version.root.line_index_of(pos.0)
    }

    /// Set the base hash.
    pub fn set_base_hash(&mut self, hash: BaseHash) {
        self.base_hash = Some(hash);
    }

    /// Set the dirty flag.
    pub fn set_dirty(&mut self, dirty: bool) {
        self.is_dirty = dirty;
    }

    /// Clear the buffer.
    pub fn clear(&mut self) {
        let old_len = self.len();
        let edit = EditOp::new(Span::new(TextByte(0), TextByte(old_len)), String::new());
        let state = self.apply_text_edit(&edit);
        self.undo_tree.apply_version(edit, state);
        self.revision = self.revision.next();
        self.is_dirty = true;
    }

    /// Insert text at a position.
    ///
    /// Returns the new revision.
    pub fn insert(&mut self, pos: TextByte, text: &str) -> Revision {
        if text.is_empty() {
            return self.revision;
        }

        let edit = EditOp::new(Span::new(pos, pos), text.to_string());
        self.apply_edit_op(edit)
    }

    /// Delete text from start to end.
    ///
    /// Returns the new revision.
    pub fn delete(&mut self, range: Span<TextByte>) -> Revision {
        if range.is_empty() {
            return self.revision;
        }

        let edit = EditOp::new(range, String::new());
        self.apply_edit_op(edit)
    }

    /// Replace text in a range with new text.
    ///
    /// Returns the new revision.
    pub fn replace(&mut self, range: Span<TextByte>, replacement: &str) -> Revision {
        if range.is_empty() && replacement.is_empty() {
            return self.revision;
        }

        let edit = EditOp::new(range, replacement.to_string());
        self.apply_edit_op(edit)
    }

    /// Apply an edit operation.
    ///
    /// This is the core method that applies an edit, updates the chunk-tree
    /// root, handles undo history, and advances the revision.
    pub fn apply_edit_op(&mut self, edit: EditOp) -> Revision {
        let state = self.apply_text_edit(&edit);
        self.undo_tree.apply_version(edit, state);
        let new_revision = self.revision.next();

        // Update revision
        self.revision = new_revision;

        // Mark as dirty
        self.is_dirty = true;

        new_revision
    }

    /// Apply a complete serialized snapshot as one non-coalescing undo step.
    /// The snapshot carries the authoritative BOM, per-line EOL, and trailing
    /// newline metadata; preserving it here is essential for conflict merges.
    pub fn apply_snapshot_boundary(&mut self, snapshot: TextSnapshot) -> Revision {
        self.apply_snapshot_boundary_inner(snapshot, None)
    }

    /// Apply a durable journal snapshot as one tagged undo boundary.
    pub fn apply_journal_snapshot_boundary(
        &mut self,
        snapshot: TextSnapshot,
        tag: crate::undo::JournalTag,
    ) -> Revision {
        self.apply_snapshot_boundary_inner(snapshot, Some(tag))
    }

    fn apply_snapshot_boundary_inner(
        &mut self,
        snapshot: TextSnapshot,
        journal: Option<crate::undo::JournalTag>,
    ) -> Revision {
        let old_len = self.len();
        let new_text = snapshot.text().to_string();
        let mut edit = EditOp::new(Span::new(TextByte(0), TextByte(old_len)), new_text.clone());
        if let Some(tag) = journal {
            edit = edit.with_journal(tag);
        }
        let state = self.version_from_snapshot_preserving_descriptor(&snapshot);
        self.version = state.clone();
        self.cached_text.take();
        self.undo_tree.apply_boundary_version(edit, state);
        let revision = self.revision.next();
        self.revision = revision;
        self.base_hash = Some(BaseHash::from_snapshot(&snapshot));
        self.is_dirty = true;
        revision
    }

    /// Serialize the buffer to its exact on-disk source bytes under the
    /// authoritative descriptor: descriptor BOM, per-line EOL, trailing
    /// newline state and encoding family all preserved (D-15, FR-22).
    ///
    /// # Errors
    ///
    /// `CodecError::Unencodable` when a strict-ASCII policy cannot represent
    /// the current content (checked before any byte effect), or a typed
    /// `LimitExceeded` when the serialized output exceeds the 32 MiB
    /// envelope. A failing conversion never returns partial bytes.
    pub fn try_to_source_bytes(&self) -> Result<Vec<u8>, CodecError> {
        let decoded = self.decoded_text();
        codec_encode(
            &decoded,
            self.source_format(),
            &EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        )
    }

    /// The DECODED UTF-8 layer: normalized text with physical EOLs restored
    /// and no BOM. This is the internal serialization input — NOT disk
    /// source bytes; use [`EditorModel::try_to_source_bytes`] for those.
    #[must_use]
    pub fn decoded_text(&self) -> String {
        decoded_layer(self.text(), &self.version.eol_table())
    }

    /// Change the buffer's save format as one isolated, revision-advancing
    /// Undo boundary without touching disk (FR-25, §6.10).
    ///
    /// Even when the byte representation would not change (e.g. ASCII bytes
    /// kept as UTF-8), the boundary exists and is isolated: plain typing
    /// coalesces with it, Undo/Redo restore the descriptor as well as text
    /// and EOL state. Representability of the current content in the target
    /// is preflighted first, so a refused conversion leaves zero state
    /// change.
    ///
    /// # Errors
    ///
    /// `CodecError::Unencodable`/`LimitExceeded` from the preflight check.
    pub fn try_convert_source_format(
        &mut self,
        target: SourceFormat,
    ) -> Result<Revision, CodecError> {
        // Preflight on the exact decoded layer before any state change.
        codec_encode(
            &self.decoded_text(),
            target,
            &EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        )?;
        // A new version with the new descriptor; the tree/EOL state are
        // unchanged (O(1) Arc clone).
        let mut version = (*self.version).clone();
        version.format = target;
        version.stamp = FormatStamp::detected(target);
        let version = Arc::new(version);
        self.version = version.clone();
        // Isolated undo node: the node state carries the NEW descriptor
        // (its parent keeps the old one), so Redo lands the format change.
        let edit = EditOp::new(Span::new(TextByte(0), TextByte(0)), String::new());
        self.undo_tree.apply_boundary_version(edit, version);
        self.revision = self.revision.next();
        self.is_dirty = true;
        Ok(self.revision)
    }

    /// Apply multiple edit operations as a single atomic edit.
    ///
    /// All edits are applied in order, but they share a single undo entry
    /// and revision advancement.
    pub fn apply_edit_ops(&mut self, edits: &[EditOp]) -> Revision {
        self.apply_edit_ops_inner(edits, None)
    }

    /// Seal the current undo group before a durable snapshot is admitted.
    pub fn seal_undo_group(&mut self) {
        self.undo_tree.seal_current();
    }

    /// Apply a set of edits as one non-coalescing, journal-tagged undo unit.
    pub fn apply_journal_edit_ops(
        &mut self,
        edits: &[EditOp],
        tag: crate::undo::JournalTag,
    ) -> Revision {
        self.apply_edit_ops_inner(edits, Some(tag))
    }

    fn apply_edit_ops_inner(
        &mut self,
        edits: &[EditOp],
        journal: Option<crate::undo::JournalTag>,
    ) -> Revision {
        // RFC-0008 S1 (FR-10): buffer-apply phase. `ops` and `doc_len` are
        // plain integers — no formatting on the hot path.
        let _model_apply =
            tracing::info_span!("edit.model_apply", ops = edits.len(), doc_len = self.len())
                .entered();
        if edits.is_empty() {
            return self.revision;
        }
        let doc_len = self.len();
        let mut ordered = edits.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        if ordered.iter().any(|edit| {
            edit.range.start.0 > edit.range.end.0
                || edit.range.end.0 > doc_len
                || !self.is_char_boundary(edit.range.start.0)
                || !self.is_char_boundary(edit.range.end.0)
        }) {
            return self.revision;
        }
        for pair in ordered.windows(2) {
            if pair[1].range.start.0 < pair[0].range.end.0
                || (pair[1].range.start.0 == pair[0].range.start.0
                    && pair[0].range.is_empty()
                    && pair[1].range.is_empty())
            {
                return self.revision;
            }
        }

        let start = ordered[0].range.start.0;
        let end = ordered.last().unwrap().range.end.0;
        let mut replacement = String::new();
        let mut cursor = start;
        for edit in ordered {
            replacement.push_str(&self.version.root.slice(cursor..edit.range.start.0));
            replacement.push_str(&edit.replacement);
            cursor = edit.range.end.0;
        }
        replacement.push_str(&self.version.root.slice(cursor..end));
        let mut combined = EditOp::new(Span::new(TextByte(start), TextByte(end)), replacement);
        let is_journal = journal.is_some() || edits.iter().any(|edit| edit.boundary);
        if let Some(tag) = journal {
            combined = combined.with_journal(tag);
        }
        let state = self.apply_text_edit(&combined);
        if is_journal {
            // Journal/boundary installs are boundaries; the version root is
            // the state (FR-5).
            self.undo_tree.apply_boundary_version(combined, state)
        } else {
            // Interactive typing: version root install (RFC-0011 D-02).
            self.undo_tree.apply_version(combined, state)
        };
        let revision = self.revision.next();
        self.revision = revision;
        self.is_dirty = true;
        revision
    }

    /// Whether byte offset `pos` sits on a UTF-8 boundary. Exact for
    /// well-formed UTF-8: a position is a boundary iff its byte is not a
    /// continuation byte (0b10xxxxxx) — except the end of text. Probed via
    /// a leaf-local byte read (O(chunk), no full-text materialization).
    /// Whether byte offset `pos` sits on a UTF-8 boundary. Exact for
    /// well-formed UTF-8: a position is a boundary iff its byte is not a
    /// continuation byte (0b10xxxxxx) — except the end of text. Probed via
    /// a leaf-local byte read (O(chunk), no full-text materialization).
    fn is_char_boundary(&self, pos: usize) -> bool {
        if pos >= self.len() {
            return pos == self.len();
        }
        let Some((bytes, local)) = self.version.root.leaf_context_at(pos) else {
            return false;
        };
        (bytes.as_bytes()[local] & 0xC0) != 0x80
    }

    /// Whether edits are non-overlapping ranges against this exact text
    /// snapshot and land on valid UTF-8 boundaries.
    #[must_use]
    pub fn edit_ops_valid(&self, edits: &[EditOp]) -> bool {
        self.batch_error(edits).is_none()
    }

    /// Typed validation for a batch against the current text: ranges must be
    /// in-bounds, UTF-8 boundary aligned, and non-overlapping after ordering.
    /// Used by both the bool validity check and the prepared-candidate path,
    /// so a batch is either fully installable or fully refused (D-06).
    fn batch_error(&self, edits: &[EditOp]) -> Option<BatchError> {
        if edits.is_empty() {
            return Some(BatchError::Empty);
        }
        let len = self.len();
        let mut ordered = edits.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        for edit in &ordered {
            if edit.range.start.0 > edit.range.end.0 {
                return Some(BatchError::OutOfBounds {
                    at: edit.range.start.0,
                    len,
                });
            }
            if edit.range.end.0 > len {
                return Some(BatchError::OutOfBounds {
                    at: edit.range.end.0,
                    len,
                });
            }
            if !self.is_char_boundary(edit.range.start.0)
                || !self.is_char_boundary(edit.range.end.0)
            {
                return Some(BatchError::CharBoundary {
                    at: edit.range.start.0,
                });
            }
        }
        for pair in ordered.windows(2) {
            if pair[1].range.start.0 < pair[0].range.end.0
                || (pair[1].range.start.0 == pair[0].range.start.0
                    && pair[0].range.is_empty()
                    && pair[1].range.is_empty())
            {
                return Some(BatchError::Overlapping {
                    at: pair[1].range.start.0,
                });
            }
        }
        None
    }

    /// Prepare a batch candidate OFF the interactive turn (D-06/TD-7,
    /// RFC-0008 S5): all validation, the combined edit and the after-state
    /// geometry are computed from the immutable current state (no snapshot,
    /// no encode, no `DecodeMap` — the serialization pipeline is reserved
    /// for undo materialization and boundary installs), so the later install
    /// is O(edit) and can never fail or partially apply. No full-document
    /// text is built (RFC-0011 S2).
    ///
    /// # Errors
    ///
    /// [`BatchError`] when the batch is empty, out of bounds, not
    /// char-boundary aligned, or overlapping — with zero state changes.
    pub fn prepare_batch(&self, edits: &[EditOp]) -> Result<PreparedBatch, BatchError> {
        if let Some(error) = self.batch_error(edits) {
            return Err(error);
        }
        let len = self.len();
        let mut ordered = edits.to_vec();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        let start = ordered[0].range.start.0;
        let end = ordered.last().map_or(start, |edit| edit.range.end.0);
        let mut replacement = String::new();
        let mut cursor = start;
        for edit in &ordered {
            replacement.push_str(&self.version.root.slice(cursor..edit.range.start.0));
            replacement.push_str(&edit.replacement);
            cursor = edit.range.end.0;
        }
        replacement.push_str(&self.version.root.slice(cursor..end));
        let combined = EditOp::new(Span::new(TextByte(start), TextByte(end)), replacement);
        let after_len = start + combined.replacement.len() + (len - end);
        Ok(PreparedBatch {
            op: combined,
            after_len,
            spans: ordered
                .iter()
                .map(|edit| {
                    crate::selection::SpanEdit::new(
                        edit.range.start.0,
                        edit.range.end.0,
                        edit.replacement.len(),
                    )
                })
                .collect(),
        })
    }

    /// Install a prepared candidate as one new revision and one isolated
    /// Undo node (D-06). The install cannot fail: validation and the
    /// candidate were completed by [`EditorModel::prepare_batch`].
    pub fn install_prepared(&mut self, batch: PreparedBatch) -> Revision {
        self.install_prepared_inner(batch, None)
    }

    fn install_prepared_inner(
        &mut self,
        batch: PreparedBatch,
        context_origin: Option<u128>,
    ) -> Revision {
        let state = self.apply_text_edit(&batch.op);
        let policy = match context_origin {
            Some(origin) => crate::undo::UndoPolicy::Context { origin },
            None => crate::undo::UndoPolicy::Isolated,
        };
        self.undo_tree
            .apply_batch_version(batch.op, state, policy, &batch.spans);
        let revision = self.revision.next();
        self.revision = revision;
        self.is_dirty = true;
        revision
    }

    /// Validate-then-install one batch under `policy` (D-06): `Isolated`
    /// goes through the prepared candidate (one boundary, no coalescing),
    /// `Typing` keeps the existing coalescing behavior. Zero state change on
    /// any validation failure.
    ///
    /// # Errors
    ///
    /// [`BatchError`] with zero state changes on invalid input.
    pub fn try_apply_batch(
        &mut self,
        edits: &[EditOp],
        policy: crate::undo::UndoPolicy,
    ) -> Result<Revision, BatchError> {
        match policy {
            crate::undo::UndoPolicy::Context { origin } => {
                let batch = self.prepare_batch(edits)?;
                let projected = crate::budget::account_node(
                    batch.after_len,
                    batch.op.replacement.len(),
                    &[],
                    &[],
                ) + crate::budget::movement_bytes(batch.spans.len());
                let lease = self.reserve_history(projected)?;
                let revision = self.install_prepared_inner(batch, Some(origin));
                self.undo_tree.release(&lease);
                Ok(revision)
            }
            crate::undo::UndoPolicy::Isolated => {
                let batch = self.prepare_batch(edits)?;
                let projected = crate::budget::account_node(
                    batch.after_len,
                    batch.op.replacement.len(),
                    &[],
                    &[],
                ) + crate::budget::movement_bytes(batch.spans.len());
                let lease = self.reserve_history(projected)?;
                let revision = self.install_prepared(batch);
                self.undo_tree.release(&lease);
                Ok(revision)
            }
            crate::undo::UndoPolicy::Typing => {
                if let Some(error) = self.batch_error(edits) {
                    return Err(error);
                }
                let projected = self.history_projection(edits, None).unwrap_or(0);
                let lease = self.reserve_history(projected)?;
                let revision = self.apply_edit_ops(edits);
                self.undo_tree.release(&lease);
                Ok(revision)
            }
        }
    }

    /// Reserve history bytes for one prospective node against the buffer
    /// budget. Refusal is complete and precedes any mutation.
    fn reserve_history(&mut self, projected: u64) -> Result<crate::undo::Lease, BatchError> {
        self.undo_tree
            .reserve(projected, crate::undo::wall_clock_ms())
            .map_err(|limit| BatchError::LimitExceeded {
                needed: limit.needed,
                budget: limit.budget,
            })
    }

    /// The history bytes a batch would cost if installed as one new node:
    /// after-state length + combined replacement text + recorded selection
    /// sets + fixed metadata. Returns `None` for an invalid/empty batch.
    /// Used by Session for the app-wide admission check; the per-buffer
    /// reserve uses the prepared candidate when available.
    #[must_use]
    pub fn history_projection(
        &self,
        edits: &[EditOp],
        record: Option<&crate::history::EditorEdit>,
    ) -> Option<u64> {
        if edits.is_empty() {
            return None;
        }
        let len = self.len();
        let mut ordered = edits.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        for edit in &ordered {
            if edit.range.start.0 > edit.range.end.0 || edit.range.end.0 > len {
                return None;
            }
        }
        let start = ordered[0].range.start.0;
        let end = ordered.last().map_or(start, |edit| edit.range.end.0);
        let mut replacement = String::new();
        let mut cursor = start;
        for edit in &ordered {
            replacement.push_str(&self.version.root.slice(cursor..edit.range.start.0));
            replacement.push_str(&edit.replacement);
            cursor = edit.range.end.0;
        }
        replacement.push_str(&self.version.root.slice(cursor..end));
        let after_len = start + replacement.len() + (len - end);
        let (before, after) = match record {
            Some(record) => (&record.before[..], &record.after[..]),
            None => (&[][..], &[][..]),
        };
        Some(
            crate::budget::account_node(after_len, replacement.len(), before, after)
                + crate::budget::movement_bytes(edits.len()),
        )
    }

    /// Exact local grapheme-boundary probe on the current text (O(chunk +
    /// window); ≤8-byte context each side, per RFC-0011 S2.
    fn before_boundary_at(&self, p: usize) -> bool {
        let len = self.len();
        if p == 0 || p >= len {
            return true;
        }
        if !self.is_char_boundary(p) {
            return false;
        }
        let mut lo = p.saturating_sub(8);
        while lo > 0 && !self.is_char_boundary(lo) {
            lo -= 1;
        }
        let mut hi = (p + 8).min(len);
        while hi < len && !self.is_char_boundary(hi) {
            hi += 1;
        }
        let ctx = self.version.root.slice(lo..hi);
        let at = p - lo;
        let mut cursor = unicode_segmentation::GraphemeCursor::new(at, ctx.len(), true);
        cursor.is_boundary(&ctx, 0).unwrap_or(true)
    }

    /// Boundary probe for the after-state (which is hypothetical): positions
    /// in untouched regions map back to the current text; positions inside
    /// the inserted bytes are judged against the replacement; positions at
    /// or past the after-text end are accepted.
    fn after_boundary_at(&self, op: &EditOp, after_len: usize, p: usize) -> bool {
        let len = self.len();
        if p == 0 || p >= after_len {
            return true;
        }
        let s = op.range.start.0;
        let e = op.range.end.0;
        let rl = op.replacement.len();
        if p < s {
            return self.before_boundary_at(p);
        }
        if p < s + rl {
            return op.replacement.is_char_boundary(p - s);
        }
        let delta = rl as i64 - (e as i64 - s as i64);
        let old_p = p as i64 - delta;
        if old_p >= len as i64 {
            return true;
        }
        self.before_boundary_at(old_p.max(0) as usize)
    }

    /// Validate the text and recorded selections before installing either.
    pub fn try_apply_recorded_batch(
        &mut self,
        edits: &[EditOp],
        policy: crate::undo::UndoPolicy,
        record: crate::history::EditorEdit,
    ) -> Result<Revision, RecordedBatchError> {
        // RFC-0008 S1 (FR-10): the interactive recorded-batch apply phase.
        let _model_apply =
            tracing::info_span!("edit.model_apply", ops = edits.len(), doc_len = self.len())
                .entered();
        let batch = self
            .prepare_batch(edits)
            .map_err(RecordedBatchError::from)?;
        let before_len = self.len();
        let after_len = batch.after_len;
        record
            .validate_windowed(
                before_len,
                after_len,
                &|p| self.before_boundary_at(p),
                &|p| self.after_boundary_at(&batch.op, after_len, p),
            )
            .map_err(|e| RecordedBatchError::InvalidForm(e.to_string()))?;
        if let crate::undo::UndoPolicy::Context { origin } = policy
            && u128::from_str_radix(&record.origin, 16).ok() != Some(origin)
        {
            return Err(RecordedBatchError::InvalidForm(
                "Recorded editor origin does not match the undo group".into(),
            ));
        }
        let projected = crate::budget::account_node(
            batch.after_len,
            batch.op.replacement.len(),
            &record.before,
            &record.after,
        ) + crate::budget::movement_bytes(batch.spans.len());
        let lease = match self.reserve_history(projected) {
            Ok(lease) => lease,
            Err(BatchError::LimitExceeded { needed, budget }) => {
                return Err(RecordedBatchError::LimitExceeded { needed, budget });
            }
            Err(_) => unreachable!("prepare succeeded; only the limit refusal remains"),
        };
        let state = self.apply_text_edit(&batch.op);
        self.undo_tree
            .apply_recorded_version(batch.op, state, policy, record, &batch.spans);
        self.undo_tree.release(&lease);
        self.revision = self.revision.next();
        self.is_dirty = true;
        Ok(self.revision)
    }

    pub fn history_page(
        &self,
        cursor: Option<crate::history::HistoryCursor>,
        limit: usize,
    ) -> Result<crate::history::HistoryPage, crate::history::HistoryError> {
        self.undo_tree
            .history_page(self.history_epoch, cursor, limit)
    }

    /// The active retained-history budget.
    #[must_use]
    pub fn history_budget(&self) -> Budget {
        self.undo_tree.budget()
    }

    /// Replace the active retained-history budget (tests, configuration).
    pub fn set_history_budget(&mut self, budget: Budget) {
        self.undo_tree.set_budget(budget);
    }

    /// Accounted retained-history bytes for this buffer.
    #[must_use]
    pub fn history_accounted_bytes(&self) -> u64 {
        self.undo_tree.accounted_bytes()
    }

    /// Number of currently allocated undo-arena pages (diagnostic; FR-14).
    #[must_use]
    pub fn history_allocated_pages(&self) -> usize {
        self.undo_tree.history_allocated_pages()
    }

    /// Retained history node count.
    #[must_use]
    pub fn history_nodes(&self) -> u64 {
        self.undo_tree.account().nodes
    }

    /// Nodes evicted since the buffer (re)started.
    #[must_use]
    pub fn history_evicted_total(&self) -> u64 {
        self.undo_tree.evicted_total()
    }

    /// Smallest surviving history node id; every smaller id was evicted.
    #[must_use]
    pub fn history_retained_watermark(&self) -> u64 {
        self.undo_tree.retained_watermark()
    }

    /// Ids of all surviving history nodes, ascending (test/diagnostic).
    #[must_use]
    pub fn history_survivor_ids(&self) -> Vec<u64> {
        self.undo_tree.survivor_ids()
    }

    /// One validated head movement; never replay intermediate states or touch
    /// the disk baseline. The caller holds the buffer's single-writer turn.
    pub fn restore_state(
        &mut self,
        target: crate::history::HistoryTarget,
    ) -> Result<Revision, crate::history::HistoryError> {
        if target.epoch != self.history_epoch {
            return Err(crate::history::HistoryError::Expired);
        }
        let state = self.undo_tree.restore_snapshot(target)?;
        let target_node = self.undo_tree.get(target.node);
        let selections = target_node.and_then(|node| {
            if node.is_root() {
                Some(vec![[0, 0, 0]])
            } else {
                node.editor.as_ref().map(|r| r.after.clone())
            }
        });
        // The restored set's primary member is explicit in the record
        // (FEUX-0002 E-4/FR-4); the root state's primary is its only member.
        let primary = target_node.and_then(|node| {
            if node.is_root() {
                Some(0)
            } else {
                node.editor.as_ref().map(|r| r.primary)
            }
        });
        // The movement delta for passive views is derived from the transition
        // (position → target) before the head moves.
        let from = self.undo_tree.position_id();
        let (spans, step_ends, truncated) = self.undo_tree.transition_spans(from, target.node);
        // Protect the restore target from budget eviction for the install
        // window; released on completion and on every error exit.
        let protection = self
            .undo_tree
            .protect_token(target.node, crate::undo::wall_clock_ms());
        // Allocate all replacement state before moving the head. Nothing
        // below is fallible; the lease window remains the backstop even if
        // an unexpected panic interrupts the release.
        self.set_history_movement(spans, step_ends, truncated, selections, primary);
        self.undo_tree.restore_position(target.node);
        self.version = state;
        self.cached_text.take();
        let snapshot = self.snapshot();
        self.revision = self.revision.next();
        self.is_dirty = self.base_hash != Some(BaseHash::from_snapshot(&snapshot));
        self.undo_tree.unprotect(protection);
        Ok(self.revision)
    }

    fn set_history_movement(
        &mut self,
        spans: Vec<crate::history::MovementSpan>,
        step_ends: Vec<usize>,
        truncated: bool,
        selections: Option<Vec<[usize; 3]>>,
        primary: Option<usize>,
    ) {
        self.history_movement = Some(crate::history::HistoryMovement {
            selections,
            spans,
            step_ends,
            truncated,
            // The requesting view's exact primary index when the recorded
            // set carries it (undo records do not: the UI keeps its own
            // primary member when it survives).
            primary,
        });
    }

    #[must_use]
    pub fn history_movement(&self) -> Option<&crate::history::HistoryMovement> {
        self.history_movement.as_ref()
    }

    /// The undo tree's current position (node id).
    #[must_use]
    pub fn history_position_id(&self) -> u64 {
        self.undo_tree.position_id()
    }

    /// The exact movement spans for a transition (`from` → `to` node ids).
    #[must_use]
    pub fn history_transition_spans(
        &self,
        from: u64,
        to: u64,
    ) -> (Vec<crate::history::MovementSpan>, Vec<usize>, bool) {
        self.undo_tree.transition_spans(from, to)
    }

    /// The recorded after-set of the current position (triples).
    #[must_use]
    pub fn history_position_selections(&self) -> Option<Vec<[usize; 3]>> {
        self.undo_tree.position_selections()
    }

    /// The recorded after-set's primary member index (FEUX-0002 FR-4): the
    /// primary identity recorded with the current node's editor edit.
    #[must_use]
    pub fn history_position_primary(&self) -> Option<usize> {
        self.undo_tree
            .position()
            .and_then(|node| node.editor.as_ref())
            .map(|record| record.primary)
    }

    /// The text of a retained history node (diagnostic/test); materializes
    /// from the node's version root on demand (user-paced).
    #[must_use]
    pub fn history_node_text(&mut self, id: u64) -> Option<String> {
        let state = self.undo_tree.get(id)?.snapshot.clone()?;
        Some(state.materialize())
    }

    /// Undo the last edit.
    ///
    /// Returns the new revision and whether the undo was successful.
    /// Every undo advances the revision (§4.5.2).
    pub fn undo(&mut self) -> Option<Revision> {
        let primary = self
            .undo_tree
            .position()
            .and_then(|n| n.editor.as_ref())
            .map(|r| r.before_primary);
        let selections = self.undo_tree.undo_selections();
        let from = self.undo_tree.position_id();
        let transition = self.undo_tree.undo_edit();
        let result = self.undo_tree.undo();

        if let Some((_node_rev, Some(state))) = result {
            let to = self.undo_tree.position_id();
            let (spans, step_ends, truncated) = self.undo_tree.transition_spans(from, to);
            self.set_history_movement(spans, step_ends, truncated, selections, primary);
            self.version = state;
            self.cached_text.take();
            let snapshot = self.snapshot();
            self.revision = self.revision.next();
            self.is_dirty = self.base_hash != Some(BaseHash::from_snapshot(&snapshot));
            self.last_transition_ops = transition.into_iter().collect();
            Some(self.revision)
        } else {
            None
        }
    }

    /// Redo the last undone edit.
    ///
    /// Returns the new revision and whether the redo was successful.
    /// Every redo advances the revision (§4.5.2).
    pub fn redo(&mut self) -> Option<Revision> {
        let transition = self.undo_tree.redo_edit();
        let from = self.undo_tree.position_id();
        let result = self.undo_tree.redo();

        if let Some((_node_rev, Some(state))) = result {
            let to = self.undo_tree.position_id();
            let (spans, step_ends, truncated) = self.undo_tree.transition_spans(from, to);
            self.set_history_movement(
                spans,
                step_ends,
                truncated,
                self.undo_tree.position_selections(),
                self.history_position_primary(),
            );
            self.version = state;
            self.cached_text.take();
            let snapshot = self.snapshot();
            self.revision = self.revision.next();
            self.is_dirty = self.base_hash != Some(BaseHash::from_snapshot(&snapshot));
            self.last_transition_ops = transition.into_iter().collect();
            Some(self.revision)
        } else {
            None
        }
    }

    /// Operations reverted or restored by the latest undo/redo transition.
    #[must_use]
    pub fn last_transition_ops(&self) -> &[EditOp] {
        &self.last_transition_ops
    }

    /// Operations that the next redo would restore, if any.
    #[must_use]
    pub fn next_redo_ops(&self) -> Vec<EditOp> {
        self.undo_tree.redo_edit().into_iter().collect()
    }

    /// Get the primary selection (canonical member), or the default caret at
    /// offset 0 when no committed set exists.
    #[must_use]
    pub fn primary_selection(&self) -> Selection {
        self.selections
            .as_ref()
            .map(|set| *set.primary())
            .unwrap_or_else(|| {
                Selection::from_parts(crate::selection::SelectionId::from_raw(0), 0, 0)
            })
    }

    /// Set the committed set from canonical members (FEUX-0002 E-4). The
    /// set must validate against the current text.
    pub fn set_selections(&mut self, set: SelectionSet) {
        debug_assert!(set.validate_against(self.text()).is_ok());
        self.selections = Some(set);
    }

    /// Replace the committed set with one canonical member.
    pub fn set_primary_selection(&mut self, anchor: usize, head: usize) {
        let candidates = vec![RawSelection {
            id: None,
            anchor,
            head,
        }];
        if let Ok((set, _)) = SelectionSet::normalize(self.text(), candidates, None) {
            self.selections = Some(set);
        }
    }

    /// Add a member to the committed set (canonical normalization merges).
    pub fn add_selection(&mut self, anchor: usize, head: usize) {
        let mut candidates: Vec<RawSelection> = self
            .selections
            .as_ref()
            .map(|set| {
                set.members()
                    .iter()
                    .map(|selection| RawSelection {
                        id: Some(selection.id().as_raw()),
                        anchor: selection.anchor(),
                        head: selection.head(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        candidates.push(RawSelection {
            id: None,
            anchor,
            head,
        });
        if let Ok((set, _)) = SelectionSet::normalize(self.text(), candidates, None) {
            self.selections = Some(set);
        }
    }

    /// Clear all selections except the primary.
    pub fn clear_secondary_selections(&mut self) {
        if let Some(set) = &self.selections {
            let primary = set.primary();
            self.set_primary_selection(primary.anchor(), primary.head());
        }
    }

    /// Get all committed selections in canonical order.
    #[must_use]
    pub fn selections(&self) -> Vec<Selection> {
        self.selections
            .as_ref()
            .map(|set| set.members().to_vec())
            .unwrap_or_else(|| {
                vec![Selection::from_parts(
                    crate::selection::SelectionId::from_raw(0),
                    0,
                    0,
                )]
            })
    }

    /// Select all text.
    pub fn select_all(&mut self) {
        self.set_primary_selection(0, self.len());
    }

    /// Get the EOL table (derived from the version; user-paced).
    #[must_use]
    pub fn eol_table(&self) -> EolTable {
        self.version.eol_table()
    }

    /// Internal content-identity bytes: the serialized source when the
    /// content encodes under its descriptor; otherwise the normalized text
    /// (the identity a pending-descriptor synthetic snapshot carries, so
    /// hashes stay comparable across the unencodable-draft case).
    ///
    /// Disk-facing callers MUST use [`EditorModel::try_to_source_bytes`]
    /// and handle `Unencodable`/limit errors typed (RFC-0006 D-15); there
    /// is no silent lossy serialization fallback on any disk path.
    #[must_use]
    pub(crate) fn source_bytes_for_hash(&self) -> Vec<u8> {
        self.try_to_source_bytes()
            .unwrap_or_else(|_| self.text().as_bytes().to_vec())
    }

    /// Test-only exact serialization shim (asserts use representable
    /// content). Production code must use [`EditorModel::try_to_source_bytes`].
    #[cfg(test)]
    #[must_use]
    pub fn to_source_bytes(&self) -> Vec<u8> {
        self.try_to_source_bytes()
            .unwrap_or_else(|_| self.decoded_text().into_bytes())
    }

    /// Create a text snapshot of the current buffer state.
    ///
    /// The snapshot is disk-backed when the content serializes under the
    /// descriptor (the normal case: byte-accurate source, per-line EOL and
    /// BOM preserved). For a strict-ASCII draft with non-ASCII content the
    /// serialization legitimately fails; the snapshot then becomes a pending
    /// descriptor view so Undo/Redo still restore text, EOL and format
    /// exactly (§6.10: the draft stays intact and recoverable, saving it
    /// fails with `Unencodable` before any file effect).
    #[must_use]
    pub fn snapshot(&self) -> TextSnapshot {
        // Fast path (NFR-2): serialize once from the decoded layer, build
        // the boundary map without decoding again, and compose the snapshot
        // from the validated parts — the inverse of
        // [`TextSnapshot::from_decoded`], two O(note) passes instead of a
        // full decode-back + line rescan per keystroke.
        let decoded = self.decoded_text();
        let encoded = codec_encode(
            &decoded,
            self.source_format(),
            &EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        );
        if let Ok(source) = encoded {
            let source: std::sync::Arc<[u8]> = source.into();
            if let Ok(map) = DecodeMap::build(
                source.clone(),
                self.source_format(),
                &DecodeLimits::default(),
                &flavite_encoding::NOT_CANCELLED,
            ) {
                return TextSnapshot::from_parts(source, map, decoded);
            }
        }
        // Pending-descriptor fallback (§6.10): the draft does not serialize
        // under its descriptor, so the snapshot is the explicit synthetic
        // normalized view carrying the save-format intent. `source()` is the
        // decoded UTF-8, NOT serialized form; Undo/Redo still restore text,
        // EOL and format exactly and saving fails with `Unencodable` typed.
        TextSnapshot::from_normalized_with_descriptor(self.text(), self.source_format())
    }

    /// Get the content hash of the current buffer (serialized source when
    /// encodable, otherwise the normalized text identity; see
    /// `EditorModel::source_bytes_for_hash` and
    /// [`EditorModel::snapshot`] for the pending-draft case).
    #[must_use]
    pub fn content_hash(&self) -> BaseHash {
        let source_bytes = self.source_bytes_for_hash();
        let hash = blake3::hash(&source_bytes);
        BaseHash::from_bytes(hash.into())
    }

    /// Check if the buffer content matches a given hash.
    #[must_use]
    pub fn matches_hash(&self, hash: BaseHash) -> bool {
        self.content_hash() == hash
    }

    /// Apply an edit to the chunk-tree root, updating the version's EOL
    /// side state and invalidating the text cache. Returns the new version
    /// state (O(log n + chunk + window); the window is empty for edits that
    /// do not touch newlines).
    fn apply_text_edit(&mut self, edit: &EditOp) -> Arc<VersionState> {
        let start = edit.range.start.0;
        let end = edit.range.end.0;
        let new_root = self.version.root.edit(start..end, &edit.replacement);
        let changes_lines =
            edit.replacement.contains('\n') || self.version.root.slice(start..end).contains('\n');
        let old_table = self.version.eol_table();
        let (eols, has_trailing_newline) = if changes_lines {
            let line_count = self.version.root.line_count();
            let (window_first, window_start, tail_end) = eol_window_for_edit(
                &self.version.root,
                start,
                end,
                &edit.replacement,
                line_count,
            );
            let window_old = self.version.root.slice(window_start..tail_end);
            let window_new = format!(
                "{}{}{}",
                &window_old[..start.saturating_sub(window_start)],
                edit.replacement,
                &window_old[end.saturating_sub(window_start)..],
            );
            let table = old_table.splice_window(
                window_first,
                &window_old,
                &window_new,
                tail_end == self.len(),
            );
            (Arc::from(table.line_endings), table.has_trailing_newline)
        } else {
            let old_trailing = self.version.has_trailing_newline;
            let new_trailing = new_root.ends_with_newline();
            if old_trailing && !new_trailing {
                let mut eols = self.version.eols.to_vec();
                eols.push(LineEnding::None);
                (Arc::from(eols), false)
            } else if !old_trailing && new_trailing {
                let mut eols = self.version.eols.to_vec();
                eols.pop();
                (Arc::from(eols), true)
            } else {
                (self.version.eols.clone(), old_trailing)
            }
        };
        let version = Arc::new(VersionState {
            root: Arc::new(new_root),
            eols,
            has_trailing_newline,
            format: self.version.format,
            stamp: self.version.stamp,
        });
        self.version = version.clone();
        self.cached_text.take();
        version
    }

    /// Set the coalesce window for the undo tree.
    pub fn set_coalesce_window(&mut self, window: Duration) {
        self.undo_tree.set_coalesce_window(window);
    }
}

/// The affected EOL window for one edit (RFC-0008 D6, now tree-based):
/// the first affected line index, its byte start, and the pre-edit
/// `tail_end` byte (line-aligned through the merge boundary). One shared
/// derivation for the interactive text path and prepared batches. Queries
/// are O(log n + chunk).
fn eol_window_for_edit(
    root: &crate::chunks::ChunkRoot,
    start: usize,
    end: usize,
    replacement: &str,
    line_count: usize,
) -> (usize, usize, usize) {
    let raw_first = root.line_index_of(start);
    let window_first = raw_first.min(line_count.saturating_sub(1));
    let window_start = root.line_start_of(window_first);
    let tail_end = if start == end {
        // Pure insertion: the inserted lines join the line at the point.
        root.find_next_newline(start)
            .map_or(root.len(), |at| at + 1)
    } else if replacement.ends_with('\n') {
        // The last replaced old line ends at its own line boundary: the
        // window runs to the line end at or after the edit end.
        root.find_next_newline(end).map_or(root.len(), |at| at + 1)
    } else {
        // The replacement's tail merges the following old line when it lacks
        // a trailing newline: the window runs to the next line boundary at or
        // after the edit end.
        root.find_next_newline(end).map_or(root.len(), |at| at + 1)
    };
    (window_first, window_start, tail_end)
}

fn common_text_edges(old: &str, new: &str) -> (usize, usize) {
    let mut prefix = 0;
    for (left, right) in old.chars().zip(new.chars()) {
        if left != right {
            break;
        }
        prefix += left.len_utf8();
    }
    let mut suffix = 0;
    for (left, right) in old[prefix..]
        .chars()
        .rev()
        .zip(new[prefix.min(new.len())..].chars().rev())
    {
        if left != right {
            break;
        }
        suffix += left.len_utf8();
    }
    (prefix, suffix)
}

fn map_position(
    position: TextByte,
    prefix: usize,
    old_middle_end: usize,
    new_middle_end: usize,
    old_len: usize,
    new_text: &str,
) -> TextByte {
    let mapped = crate::positions::map_through_snapshot(
        position.0,
        prefix,
        old_middle_end,
        new_middle_end,
        old_len,
        new_text.len(),
    );
    TextByte(crate::positions::snap_to_char_boundary(new_text, mapped))
}

/// The physical-EOL decoded layer for a normalized text plus its EOL table
/// (pure form of [`EditorModel::decoded_text`]).
#[must_use]
pub fn decoded_layer(text: &str, table: &EolTable) -> String {
    let mut out = String::with_capacity(text.len() + table.line_count());
    let mut start = 0usize;
    let breaks: Vec<usize> = if text.ends_with('\n') {
        text.match_indices('\n').map(|(i, _)| i).collect()
    } else {
        let mut v: Vec<usize> = text.match_indices('\n').map(|(i, _)| i).collect();
        v.push(text.len());
        v
    };
    for (i, &end) in breaks.iter().enumerate() {
        out.push_str(&text[start..end]);
        match table.line_endings.get(i) {
            Some(LineEnding::Lf) => out.push('\n'),
            Some(LineEnding::Crlf) => out.push_str("\r\n"),
            // No recorded ending for this line (new/last): keep LF when
            // the normalized text carries one, otherwise terminate none.
            _ => {
                if end < text.len() {
                    out.push('\n');
                }
            }
        }
        start = if end < text.len() { end + 1 } else { end };
    }
    out
}

/// Default coalesce window for EditorModel.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(500);

impl Default for EditorModel {
    fn default() -> Self {
        Self::new_empty()
    }
}

impl std::fmt::Display for EditorModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "EditorModel(id={}, rev={}, len={}, lines={}, dirty={})",
            self.buffer_id.as_u64(),
            self.revision.as_u64(),
            self.len(),
            self.line_count(),
            self.is_dirty
        )
    }
}

/// Builder for creating EditorModel with custom settings.
#[derive(Debug)]
pub struct EditorModelBuilder {
    text: String,
    coalesce_window: Duration,
}

impl EditorModelBuilder {
    /// Create a new builder with empty text.
    #[must_use]
    pub fn new() -> Self {
        Self {
            text: String::new(),
            coalesce_window: crate::undo::DEFAULT_COALESCE_WINDOW,
        }
    }

    /// Set the initial text.
    #[must_use]
    pub fn text(mut self, text: &str) -> Self {
        self.text = text.to_string();
        self
    }

    /// Set the coalesce window.
    #[must_use]
    pub fn coalesce_window(mut self, window: Duration) -> Self {
        self.coalesce_window = window;
        self
    }

    /// Build the EditorModel.
    #[must_use]
    pub fn build(self) -> EditorModel {
        let mut model = EditorModel::from_text(&self.text);
        model.set_coalesce_window(self.coalesce_window);
        model
    }
}

impl Default for EditorModelBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_empty_model() {
        let model = EditorModel::new_empty();
        assert_eq!(model.len(), 0);
        assert_eq!(model.line_count(), 1); // Empty file has one line
        assert!(!model.is_dirty());
    }

    #[test]
    fn text_reads_multi_chunk_documents() {
        let text = "filler line for the chunk-boundary test\n".repeat(300);
        let mut model = EditorModel::from_text(&text);
        assert_eq!(model.text(), text);
        assert_eq!(model.len(), text.len());

        // Reads stay correct across every path that rebuilds the tree.
        let replacement = "replaced\n".repeat(400);
        model.replace_snapshot(TextSnapshot::new(replacement.as_bytes()).unwrap());
        assert_eq!(model.text(), replacement);

        model.apply_edit_op(EditOp::new(
            Span::new(TextByte(0), TextByte(9)),
            "EDITED".into(),
        ));
        assert!(model.text().starts_with("EDITED"));
        assert!(model.text().contains("replaced"));

        model.undo();
        assert_eq!(model.text(), replacement);

        model.redo();
        assert!(model.text().starts_with("EDITED"));

        // A slice spanning many chunks reads back byte-exact too.
        let start = TextByte(8);
        let end = TextByte(8 + 500);
        assert_eq!(
            model.slice_str(Span::new(start, end)),
            &model.text()[8..508]
        );
    }

    #[test]
    fn from_str() {
        let model = EditorModel::from_text("hello\nworld");
        assert_eq!(model.text(), "hello\nworld");
        assert_eq!(model.len(), 11);
        assert_eq!(model.line_count(), 2);
    }

    #[test]
    fn insert_text() {
        let mut model = EditorModel::from_text("hello");
        let rev1 = model.insert(TextByte(5), " world");
        assert_eq!(model.text(), "hello world");
        assert_eq!(rev1, Revision::new(1));
        assert!(model.is_dirty());
    }

    #[test]
    fn delete_text() {
        let mut model = EditorModel::from_text("hello world");
        let rev1 = model.delete(Span::new(TextByte(5), TextByte(11)));
        assert_eq!(model.text(), "hello");
        assert_eq!(rev1, Revision::new(1));
    }

    #[test]
    fn replace_text() {
        let mut model = EditorModel::from_text("hello world");
        let rev1 = model.replace(Span::new(TextByte(6), TextByte(11)), "there");
        assert_eq!(model.text(), "hello there");
        assert_eq!(rev1, Revision::new(1));
    }

    #[test]
    fn replacing_multiline_text_rebuilds_eol_metadata_after_the_rope_edit() {
        let snapshot = TextSnapshot::new("\u{FEFF}one\r\ntwo\r\nthree\n".as_bytes()).unwrap();
        let mut model = EditorModel::from_snapshot(snapshot);

        model.apply_edit_ops(&[EditOp::new(
            Span::new(TextByte(0), TextByte(model.len())),
            "single line".into(),
        )]);

        assert_eq!(model.text(), "single line");
        assert_eq!(model.eol_table().line_count(), 1);
        assert!(model.source_format().bom(), "descriptor owns the BOM");
        assert!(!model.eol_table().has_trailing_newline());
        assert_eq!(model.to_source_bytes(), "\u{FEFF}single line".as_bytes());
        let _ = model.content_hash();
    }

    #[test]
    fn character_edits_preserve_mixed_endings_and_terminal_line_history() {
        let mut model = EditorModel::from_snapshot(
            TextSnapshot::new("\u{feff}one\r\ntwo\n".as_bytes()).unwrap(),
        );
        let initial = model.eol_table();
        model.apply_edit_ops(&[EditOp::new(
            Span::new(TextByte(1), TextByte(2)),
            "🧑🏽‍💻".into(),
        )]);
        assert_eq!(model.eol_table(), initial);
        assert_eq!(model.to_source_bytes(), "\u{feff}o🧑🏽‍💻e\r\ntwo\n".as_bytes());
        model.seal_undo_group();
        let end = model.len();
        model.apply_edit_ops(&[EditOp::new(
            Span::new(TextByte(end), TextByte(end)),
            "é".into(),
        )]);
        assert_eq!(model.eol_table().line_count(), 3);
        assert!(!model.eol_table().has_trailing_newline());
        assert_eq!(model.to_source_bytes(), "\u{feff}o🧑🏽‍💻e\r\ntwo\né".as_bytes());
        assert_eq!(model.line_at(TextByte(end)), 2);
        model.seal_undo_group();
        model.apply_edit_ops(&[EditOp::new(
            Span::new(TextByte(end), TextByte(model.len())),
            String::new(),
        )]);
        assert_eq!(model.eol_table(), initial);
        assert!(model.undo().is_some());
        assert!(model.text().ends_with('é'));
        assert_eq!(model.snapshot().source(), model.to_source_bytes());
        assert!(model.redo().is_some());
        assert_eq!(model.eol_table(), initial);
    }

    #[test]
    fn newline_insertion_at_eof_uses_one_eol_update_for_all_edit_apis() {
        for (source, expected) in [("", "\n"), ("a", "a\n"), ("a\r\n", "a\r\n\r\n")] {
            for batch in [false, true] {
                let mut model =
                    EditorModel::from_snapshot(TextSnapshot::new(source.as_bytes()).unwrap());
                let end = TextByte(model.len());
                if batch {
                    model.apply_edit_ops(&[EditOp::new(Span::new(end, end), "\n".into())]);
                } else {
                    model.insert(end, "\n");
                }
                assert_eq!(model.to_source_bytes(), expected.as_bytes());
                assert_eq!(model.snapshot().text(), model.text());
                assert!(model.undo().is_some());
                assert_eq!(model.to_source_bytes(), source.as_bytes());
                assert!(model.redo().is_some());
                assert_eq!(model.to_source_bytes(), expected.as_bytes());
            }
        }
    }

    #[test]
    fn edits_keep_the_chunk_index_in_sync_across_chunks_and_unicode() {
        let mut model = EditorModel::from_text(&"aé\n".repeat(3000));
        for line in [0, 300, 2000, 3000] {
            let at = model.line_start(line).0;
            let before = model.revision();
            model.apply_edit_ops(&[EditOp::new(
                Span::new(TextByte(at), TextByte(at)),
                "🧑🏽‍💻".into(),
            )]);
            assert!(model.revision() > before);
            assert_eq!(model.snapshot().text(), model.text());
            assert_eq!(model.to_source_bytes(), model.text().as_bytes());
        }
    }

    #[test]
    fn undo_redo() {
        let mut model = EditorModel::from_text("hello");

        // Insert
        model.insert(TextByte(5), " world");
        assert_eq!(model.text(), "hello world");

        // Undo
        let _ = model.undo();
        assert_eq!(model.text(), "hello");

        // Redo
        let _ = model.redo();
        assert_eq!(model.text(), "hello world");
    }

    #[test]
    fn multi_range_edit_applies_simultaneously_as_one_undo_step() {
        let mut model = EditorModel::from_text("abcdefgh");
        let before = model.revision();
        let edits = [
            EditOp::new(Span::new(TextByte(1), TextByte(2)), "X".into()),
            EditOp::new(Span::new(TextByte(5), TextByte(6)), "Y".into()),
        ];

        assert!(model.edit_ops_valid(&edits));
        let after = model.apply_edit_ops(&edits);
        assert_eq!(model.text(), "aXcdeYgh");
        assert_eq!(after.as_u64(), before.as_u64() + 1);
        assert_eq!(model.undo(), Some(after.next()));
        assert_eq!(model.text(), "abcdefgh");
    }

    #[test]
    fn journal_edit_is_one_boundary_and_transition_is_reported() {
        let mut model = EditorModel::from_text("[[Ideas]] / [[Ideas]] / [[Other]]");
        let edits = [
            EditOp::new(Span::new(TextByte(2), TextByte(7)), "Plans".into()),
            EditOp::new(Span::new(TextByte(14), TextByte(19)), "Plans".into()),
        ];
        let revision = model.apply_journal_edit_ops(
            &edits,
            crate::undo::JournalTag {
                txn_id: "tx".into(),
                item_id: 9,
            },
        );
        assert_eq!(revision.as_u64(), 1);
        assert_eq!(model.text(), "[[Plans]] / [[Plans]] / [[Other]]");
        assert_eq!(model.undo(), Some(revision.next()));
        assert_eq!(model.text(), "[[Ideas]] / [[Ideas]] / [[Other]]");
        assert_eq!(
            model.last_transition_ops()[0]
                .journal
                .as_ref()
                .unwrap()
                .item_id,
            9
        );
        assert_eq!(
            model.next_redo_ops()[0].journal.as_ref().unwrap().item_id,
            9
        );
        assert_eq!(model.redo(), Some(revision.next().next()));
    }

    #[test]
    fn invalid_multi_range_edit_is_rejected_without_partial_change() {
        let mut model = EditorModel::from_text("abcdef");
        let before = model.revision();
        let edits = [
            EditOp::new(Span::new(TextByte(1), TextByte(4)), "X".into()),
            EditOp::new(Span::new(TextByte(3), TextByte(5)), "Y".into()),
        ];

        assert!(!model.edit_ops_valid(&edits));
        assert_eq!(model.apply_edit_ops(&edits), before);
        assert_eq!(model.text(), "abcdef");
        assert_eq!(model.revision(), before);
    }

    #[test]
    fn line_operations() {
        let model = EditorModel::from_text("line1\nline2\nline3");
        assert_eq!(model.line_count(), 3);
        assert_eq!(model.line_at(TextByte(0)), 0);
        assert_eq!(model.line_at(TextByte(6)), 1);
        assert_eq!(model.line_at(TextByte(12)), 2);
        assert_eq!(model.line_start(0), TextByte(0));
        assert_eq!(model.line_start(1), TextByte(6));
        assert_eq!(model.line_start(2), TextByte(12));
        assert_eq!(model.line_end(0), TextByte(6));
    }

    #[test]
    fn selection_operations() {
        let mut model = EditorModel::from_text("hello world");

        // Set primary selection
        model.set_primary_selection(0, 5);
        let sel = model.primary_selection();
        assert_eq!(sel.start(), 0);
        assert_eq!(sel.end(), 5);
        assert!(!sel.is_caret());

        // Caret selection
        model.set_primary_selection(5, 5);
        let sel = model.primary_selection();
        assert!(sel.is_caret());
        assert_eq!(sel.start(), 5);
        assert_eq!(sel.end(), 5);
    }

    #[test]
    fn select_all() {
        let mut model = EditorModel::from_text("hello");
        model.select_all();
        let sel = model.primary_selection();
        assert_eq!(sel.start(), 0);
        assert_eq!(sel.end(), 5);
    }

    #[test]
    fn snapshot_round_trip() {
        let mut model = EditorModel::from_text("hello\nworld");
        let snapshot = model.snapshot();
        assert_eq!(snapshot.text(), "hello\nworld");

        // Modify and check hash changes
        let hash1 = model.content_hash();
        model.insert(TextByte(11), "!");
        let hash2 = model.content_hash();
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn to_source_bytes() {
        let model = EditorModel::from_text("hello");
        let bytes = model.to_source_bytes();
        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn builder() {
        let model = EditorModelBuilder::new()
            .text("hello")
            .coalesce_window(Duration::from_millis(100))
            .build();
        assert_eq!(model.text(), "hello");
    }

    #[test]
    fn buffer_id_uniqueness() {
        let model1 = EditorModel::new_empty();
        let model2 = EditorModel::new_empty();
        assert_ne!(model1.buffer_id(), model2.buffer_id());
    }

    #[test]
    fn revision_advances() {
        let mut model = EditorModel::new_empty();
        assert_eq!(model.revision(), Revision::INITIAL);

        let r1 = model.insert(TextByte(0), "a");
        assert_eq!(r1, Revision::new(1));
        assert_eq!(model.revision(), Revision::new(1));

        let r2 = model.insert(TextByte(1), "b");
        assert_eq!(r2, Revision::new(2));
        assert_eq!(model.revision(), Revision::new(2));
    }

    #[test]
    fn model_snapshot_slices_positions_and_selection_edges() {
        let mut model = EditorModel::from_text("a\né");
        assert!(!model.is_empty());
        assert_eq!(model.char_at(TextByte(0)), Some('a'));
        assert_eq!(model.char_at(TextByte(2)), Some('é'));
        assert_eq!(model.char_at(TextByte(3)), None);
        assert_eq!(model.slice_str(Span::new(TextByte(0), TextByte(1))), "a");
        assert_eq!(model.line_count(), 2);
        assert_eq!(model.line_start(1), TextByte(2));
        assert_eq!(model.line_end(0), TextByte(2));
        assert_eq!(model.line_end(1), TextByte(4));
        assert_eq!(model.line_at(TextByte(2)), 1);
        assert_eq!(model.primary_selection().head(), 0);
        assert_eq!(model.primary_selection().anchor(), 0);

        model.set_base_hash(BaseHash::from_bytes([9; 32]));
        model.set_dirty(true);
        model.set_primary_selection(4, 1);
        model.add_selection(2, 2);
        model.replace_snapshot(TextSnapshot::new(b"fresh\n").unwrap());
        assert_eq!(model.text(), "fresh\n");
        assert_eq!(model.revision(), Revision::new(1));
        assert!(!model.is_dirty());
        // The caret at 2 is absorbed into the range [1, 4) by canonical
        // normalization, so the set stays a single member.
        assert_eq!(model.selections().len(), 1);
        assert!(model.base_hash().is_some());
    }

    #[test]
    fn edit_batches_empty_operations_and_clear_keep_revision_contract() {
        let mut model = EditorModel::from_text("abc");
        let initial = model.revision();
        assert_eq!(model.apply_edit_ops(&[]), initial);
        assert_eq!(
            model.replace(Span::new(TextByte(1), TextByte(1)), ""),
            initial
        );
        assert_eq!(model.insert(TextByte(0), ""), initial);
        assert_eq!(model.delete(Span::new(TextByte(1), TextByte(1))), initial);
        assert_eq!(
            model.apply_edit_ops(&[EditOp::new(Span::new(TextByte(1), TextByte(2)), "X".into(),)]),
            Revision::new(1)
        );
        assert_eq!(model.text(), "aXc");
        model.clear();
        assert!(model.is_empty());
        assert!(model.to_source_bytes().is_empty());
    }
}

#[cfg(test)]
mod debug_tests {
    use super::*;
    use crate::eol::logical_lines;

    /// The EOL splice contract, verified against an independent whole-doc
    /// oracle defined inline (the old reference oracle).
    fn reference_eol_splice(old: &EolTable, old_text: &str, new_text: &str) -> EolTable {
        let dominant = match old.dominant_eol() {
            LineEnding::None => LineEnding::Lf,
            ending => ending,
        };
        let mut new_table = EolTable::from_text(new_text);
        let old_lines = logical_lines(old_text);
        let new_lines = logical_lines(new_text);
        let mut prefix = 0;
        while prefix < old_lines.len()
            && prefix < new_lines.len()
            && old_lines[prefix] == new_lines[prefix]
        {
            prefix += 1;
        }
        let mut suffix = 0;
        while suffix < old_lines.len().saturating_sub(prefix)
            && suffix < new_lines.len().saturating_sub(prefix)
            && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
        {
            suffix += 1;
        }
        let old_middle = old_lines.len() - prefix - suffix;
        let new_middle = new_lines.len() - prefix - suffix;
        for (line, ending) in new_table.line_endings.iter_mut().enumerate() {
            // A replacement line without a newline is authoritative: do not
            // carry the old file's ending onto the new final unterminated line.
            if *ending == LineEnding::None {
                continue;
            }
            let old_line = if line < prefix {
                Some(line)
            } else if line >= prefix + new_middle {
                Some(old_lines.len() - (new_lines.len() - line))
            } else if old_middle == new_middle {
                Some(line)
            } else {
                None
            };
            *ending = old_line
                .and_then(|old_line| old.get(old_line))
                .unwrap_or(dominant);
        }
        new_table
    }

    #[test]
    fn bounded_eol_splice_matches_the_reference_splice_on_randomized_edits() {
        // Deterministic LCG: repeated runs always cover the same corpus.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Lines carry mixed endings so dominant-vs-recorded divergences are
        // exercised, plus empty units so the empty-text edges appear.
        let units = ["a", "b\r", "\n", "c\r\n", "dd", "\r\n", "e\n", ""];
        for iteration in 0..2_000 {
            let text: String = (0..(next() % 9 + 1))
                .map(|_| units[(next() as usize) % units.len()])
                .collect();
            let model = EditorModel::from_text(&text);
            let old_text = model.text().to_string();
            let old_table = model.eol_table();
            let start = (next() as usize) % (old_text.len() + 1);
            let end = start + (next() as usize) % (old_text.len() + 1 - start);
            // Frequently reproduce the replaced bytes so identity windows
            // (equal-line prefix extension) are exercised too.
            let replacement = if next() % 4 == 0 && end > start {
                old_text[start..end].to_string()
            } else {
                (0..(next() % 4))
                    .map(|_| units[(next() as usize) % units.len()])
                    .collect()
            };
            // The reference: the whole-document splice.
            let expected = reference_eol_splice(
                &old_table,
                &old_text,
                &format!("{}{}{}", &old_text[..start], replacement, &old_text[end..]),
            );
            let mut edited = EditorModel::from_text(&text);
            edited.apply_edit_ops(&[EditOp::new(
                Span::new(TextByte(start), TextByte(end)),
                replacement.clone(),
            )]);
            assert_eq!(
                edited.eol_table(),
                expected,
                "iteration {iteration}: {start}..{end} -> {replacement:?} on {old_text:?}"
            );
        }
    }

    #[test]
    fn batch_composed_bounded_splice_matches_the_reference_oracle() {
        // The prepared-batch path (isolated installs) recomputes the window
        // over the COMPOSED batch edit; it must agree with the whole-document
        // reference for multi-op batches with newline insert/delete/replace
        // mixes.
        let mut state = 0xD1B5_4E32_AB7F_9981u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let units = ["a", "b\r", "\n", "c\r\n", "dd", "\r\n", "e\n", ""];
        for iteration in 0..1_000 {
            let text: String = (0..(next() % 8 + 1))
                .map(|_| units[(next() as usize) % units.len()])
                .collect();
            let model = EditorModel::from_text(&text);
            let old_text = model.text().to_string();
            let old_table = model.eol_table();
            let op_count = next() % 3 + 2;
            let mut edits = Vec::new();
            let mut replacement = String::new();
            let mut cursor = 0usize;
            let mut start = 0usize;
            let mut end = old_text.len();
            for i in 0..op_count {
                // Monotonic, non-overlapping ranges (gaps only INSIDE the
                // combined span, matching prepare_batch).
                let window = old_text.len() - cursor;
                let s = cursor + (next() as usize) % (window + 1);
                let e = (s + (next() as usize) % (old_text.len() - s + 1)).min(old_text.len());
                let piece: String = (0..(next() % 3))
                    .map(|_| units[(next() as usize) % units.len()])
                    .collect();
                if i == 0 {
                    start = s;
                }
                if edits.is_empty() {
                    cursor = start;
                    replacement.clear();
                }
                replacement.push_str(&old_text[cursor..s]);
                replacement.push_str(&piece);
                edits.push(EditOp::new(Span::new(TextByte(s), TextByte(e)), piece));
                cursor = e;
                end = e;
            }
            replacement.push_str(&old_text[cursor..end]);
            let after = format!("{}{}{}", &old_text[..start], replacement, &old_text[end..]);
            let expected = reference_eol_splice(&old_table, &old_text, &after);
            let mut edited = EditorModel::from_text(&text);
            if !edited.edit_ops_valid(&edits) {
                continue; // degenerate same-start-empty overlap: skip
            }
            edited
                .try_apply_batch(&edits, crate::undo::UndoPolicy::Isolated)
                .expect("batch installs");
            assert_eq!(
                edited.eol_table(),
                expected,
                "iteration {iteration}: batch {edits:?} on {old_text:?}"
            );
        }
    }

    #[test]
    fn delta_nodes_account_identically_to_eager_snapshots() {
        // The same edit installed as a typing delta and as an isolated
        // materialized boundary accounts the same bytes.
        let edit = |model: &mut EditorModel| {
            let at = model.text().len();
            let rec = crate::history::EditorEdit {
                before_primary: 0,
                origin: format!("{:032x}", 1),
                kind: crate::history::EditKind::Typing,
                before: vec![[0, at, at]],
                after: vec![[0, at + 1, at + 1]],
                primary: 0,
            };
            model
                .try_apply_recorded_batch(
                    &[EditOp::new(
                        Span::new(TextByte(at), TextByte(at)),
                        "x".into(),
                    )],
                    crate::undo::UndoPolicy::Typing,
                    rec,
                )
                .expect("typing applies");
        };
        let mut typing = EditorModel::from_text("base");
        for _ in 0..5 {
            edit(&mut typing);
        }
        let mut isolated = EditorModel::from_text("base");
        for _ in 0..5 {
            let at = isolated.text().len();
            let rec = crate::history::EditorEdit {
                before_primary: 0,
                origin: format!("{:032x}", 2),
                kind: crate::history::EditKind::Typing,
                before: vec![[0, at, at]],
                after: vec![[0, at + 1, at + 1]],
                primary: 0,
            };
            isolated
                .try_apply_recorded_batch(
                    &[EditOp::new(
                        Span::new(TextByte(at), TextByte(at)),
                        "x".into(),
                    )],
                    crate::undo::UndoPolicy::Isolated,
                    rec,
                )
                .expect("isolated applies");
        }
        // Same content; accounting must match.
        assert_eq!(typing.text(), isolated.text());
        assert_eq!(typing.history_nodes(), isolated.history_nodes());
        assert_eq!(
            typing.history_accounted_bytes(),
            isolated.history_accounted_bytes()
        );
    }
}
