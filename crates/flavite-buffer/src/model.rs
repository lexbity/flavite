//! EditorModel — the headless text buffer (§4.14, D14).
//!
//! This is the core of the buffer crate: a pure, testable, headless model
//! that owns:
//! - A `ropey::Rope` for the text content
//! - Multi-selection support
//! - Undo/redo via an undo tree
//! - Per-line EOL metadata
//! - One authoritative [`SourceFormat`] descriptor plus its [`FormatStamp`]
//!   (RFC-0006 D-15): the save format is buffer state, not EOL-table state
//!   and not a filesystem guess.
//!
//! All editing operations produce `EditOp`s and are applied through
//! the model. The model maintains revision numbers and can provide
//! revisioned read views.

use flavite_encoding::{
    CodecError, DecodeLimits, DecodeMap, DecodePolicy, EncodeLimits, FormatStamp, SourceFormat,
    encode as codec_encode,
};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use ropey::{Rope, RopeSlice};
use std::time::Duration;

use crate::budget::Budget;
use crate::eol::EolTable;
use crate::revision::{BaseHash, BufferId, Revision};
use crate::selection::{RawSelection, Selection, SelectionSet};
use crate::undo::{EditOp, UndoTree};

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
    /// The text content (normalized, CRLF folded to LF).
    text: String,
    /// The same content as [`Self::text`], for byte/line mapping. The rope is
    /// only an index: `RopeSlice::as_str` is `None` for any slice spanning
    /// more than one internal chunk, so text must never be read back
    /// through the rope.
    rope: Rope,
    /// Multi-selection: the canonical committed set (FEUX-0002 E-4).
    /// `None` means the default single caret at offset 0.
    selections: Option<SelectionSet>,
    /// The undo tree for edit history.
    undo_tree: UndoTree,
    /// Per-line EOL information.
    eol_table: EolTable,
    /// The authoritative save-format descriptor (RFC-0006 D-15). Ordinary
    /// edits never change it; only explicit conversion or an acknowledged
    /// reload/reopen does.
    source_format: SourceFormat,
    /// Codec/hint provenance for the descriptor.
    format_stamp: FormatStamp,
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
/// happens before any state change: Rope, EOL table, revision and the Undo
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
/// plus the after-state Undo snapshot, both computed off the interactive
/// turn from immutable state. The install is O(edit) and infallible.
#[derive(Debug)]
pub struct PreparedBatch {
    /// The combined, boundary-isolated edit over the prepare-time text.
    pub op: EditOp,
    /// The after-state snapshot recorded as the isolated Undo node, carrying
    /// text, spliced EOL state and the descriptor.
    pub undo_snapshot: TextSnapshot,
    /// The original per-op batch geometry in pre-change coordinates
    /// (FEUX-0002 E-13): retained on the undo node so movement across the
    /// transition maps carets in unchanged gaps exactly.
    pub spans: Vec<crate::selection::SpanEdit>,
}

/// Splice an EOL table from `old_text` to `new_text`: unchanged prefix and
/// suffix lines keep their recorded endings; changed lines inherit the old
/// dominant ending; a new final unterminated line stays unterminated. This
/// is the affected-range splice (§6.9), never a whole-document
/// reconstruction fallback.
#[must_use]
pub fn spliced_eol_table(old: &EolTable, old_text: &str, new_text: &str) -> EolTable {
    let dominant = match old.dominant_eol() {
        crate::eol::LineEnding::None => crate::eol::LineEnding::Lf,
        ending => ending,
    };
    let snapshot = TextSnapshot::from_normalized(new_text);
    let mut new_table = EolTable::from_snapshot(&snapshot);
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
        if *ending == crate::eol::LineEnding::None {
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

/// Bounded affected-range splice (§6.9, S01): the EOL entries of only the
/// edited line run are recomputed. `old_text` is the full pre-edit text,
/// `window_start..tail_end` the pre-edit byte range of the edited run (a
/// line-aligned run through the merge boundary), `window_new` its post-edit
/// text (old head + replacement + unchanged tail), and `window_first` the
/// table index of the run's first line. Unchanged prefix/suffix entries keep
/// their recorded endings, changed lines inherit the old dominant ending,
/// and a new final unterminated line stays unterminated — identical to
/// [`spliced_eol_table`], which stays the reference path for prepared
/// batches (the install path here must agree with it exactly).
fn spliced_eol_table_in_window(
    old: &EolTable,
    old_text: &str,
    window_start: usize,
    tail_end: usize,
    window_new: &str,
    window_first: usize,
) -> EolTable {
    let window_old = &old_text[window_start..tail_end];
    let old_count = old.line_count();
    if window_new.is_empty() {
        // The whole run was deleted. The surviving document is the
        // unchanged, line-aligned head and tail: the run contributes no
        // lines (a wholly deleted run is not a phantom unterminated line).
        if window_start == 0 && tail_end == old_text.len() {
            // The document is now empty: one unterminated line (§6.10).
            return EolTable {
                line_endings: vec![crate::eol::LineEnding::None],
                has_trailing_newline: false,
            };
        }
        let removed = old_count
            .saturating_sub(window_first)
            .min(window_old.split_inclusive('\n').count());
        let mut table = old.clone();
        table
            .line_endings
            .splice(window_first..window_first + removed, []);
        // The surviving document tail is unchanged: the old final line when
        // the tail survives, otherwise the (terminated) last head line.
        table.has_trailing_newline = if tail_end < old_text.len() {
            old.has_trailing_newline
        } else {
            window_start > 0
        };
        return table;
    }
    let dominant = match old.dominant_eol() {
        crate::eol::LineEnding::None => crate::eol::LineEnding::Lf,
        ending => ending,
    };
    // `logical_lines` maps an empty run to its one phantom line, matching
    // the whole-document reference.
    let old_lines = logical_lines(window_old);
    let new_lines = logical_lines(window_new);
    let removed = old_count.saturating_sub(window_first).min(old_lines.len());
    let new_count = old_count - removed + new_lines.len();

    let mut head = 0;
    while head < old_lines.len() && head < new_lines.len() && old_lines[head] == new_lines[head] {
        head += 1;
    }
    let prefix = window_first + head;
    let mut suffix = 0;
    while suffix < old_lines.len().saturating_sub(head)
        && suffix < new_lines.len().saturating_sub(head)
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    // The unchanged lines beyond the window are equal on both sides and
    // always extend the whole-document suffix (the reference compares them
    // line by line); the window tail matches stack on top of them.
    let beyond_window = old_count
        .saturating_sub(window_first)
        .saturating_sub(removed);
    let suffix = (suffix + beyond_window)
        .min(old_count.saturating_sub(prefix))
        .min(new_count.saturating_sub(prefix));
    let old_middle = old_count.saturating_sub(prefix + suffix);
    let new_middle = new_count.saturating_sub(prefix + suffix);

    let mut table = EolTable::default();
    table.line_endings.reserve(new_count);
    for line in 0..new_count {
        // The scanned terminator of the new text at this line: a replacement
        // line without a newline is authoritative and never inherits one.
        let scanned = if line < window_first {
            old.get(line)
        } else if line < window_first + new_lines.len() {
            Some(line_ending_of(new_lines[line - window_first]))
        } else {
            old.get(old_count - (new_count - line))
        };
        if scanned == Some(crate::eol::LineEnding::None) {
            table.line_endings.push(crate::eol::LineEnding::None);
            continue;
        }
        let old_line = if line < prefix {
            Some(line)
        } else if line >= new_count - suffix {
            Some(old_count - (new_count - line))
        } else if old_middle == new_middle {
            Some(line)
        } else {
            None
        };
        let ending = old_line
            .and_then(|old_line| old.get(old_line))
            .unwrap_or(dominant);
        table.line_endings.push(ending);
    }
    // The reference's trailing flag comes from the whole-document scan: the
    // last document line lies beyond the window (the old trailing state
    // stands) or is the window's final new line.
    table.has_trailing_newline = if tail_end < old_text.len() {
        old.has_trailing_newline
    } else {
        new_lines
            .last()
            .is_some_and(|line| line_ending_of(line) != crate::eol::LineEnding::None)
    };
    table
}

/// The recorded ending of one split-inclusive line slice.
fn line_ending_of(line: &str) -> crate::eol::LineEnding {
    if line.ends_with("\r\n") {
        crate::eol::LineEnding::Crlf
    } else if line.ends_with('\n') {
        crate::eol::LineEnding::Lf
    } else {
        crate::eol::LineEnding::None
    }
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
            Some(crate::eol::LineEnding::Lf) => out.push('\n'),
            Some(crate::eol::LineEnding::Crlf) => out.push_str("\r\n"),
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
        let text = snapshot.text().to_string();
        let rope = Rope::from_str(&text);
        let eol_table = EolTable::from_snapshot(&snapshot);
        let source_format = snapshot.format().unwrap_or_default();
        let format_stamp = FormatStamp::detected(source_format);

        Self {
            history_epoch: crate::history::HistoryEpoch::new(),
            history_movement: None,
            buffer_id: BufferId::new(),
            text,
            rope,
            selections: None,
            undo_tree: UndoTree::new(snapshot.clone()),
            eol_table,
            source_format,
            format_stamp,
            revision: Revision::INITIAL,
            base_hash: Some(BaseHash::from_snapshot(&snapshot)),
            is_dirty: false,
            last_transition_ops: Vec::new(),
        }
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
                .into_iter()
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
        self.rope = Rope::from_str(snapshot.text());
        self.text = new_text;
        self.eol_table = EolTable::from_snapshot(&snapshot);
        if let Some(format) = snapshot.format() {
            self.source_format = format;
            self.format_stamp = FormatStamp::detected(format);
        }
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
    pub const fn source_format(&self) -> SourceFormat {
        self.source_format
    }

    /// Descriptor provenance (codec version, hint generation).
    #[must_use]
    pub const fn format_stamp(&self) -> &FormatStamp {
        &self.format_stamp
    }

    /// Record hint-generation provenance without touching text or revision
    /// (used when the buffer was opened under a confirmed hint).
    pub fn set_format_stamp_generation(&mut self, hint_generation: Option<u64>) {
        self.format_stamp.hint_generation = hint_generation;
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

    /// Get the text content as a string slice.
    #[must_use]
    pub fn text(&self) -> &str {
        // The mirrored `text` field is the read path: `RopeSlice::as_str`
        // returns `None` for any slice spanning more than one rope chunk,
        // which silently presented every multi-chunk buffer as empty.
        &self.text
    }

    /// Get the text length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rope.len_bytes()
    }

    /// Check if the buffer is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get a character at a position.
    #[must_use]
    pub fn char_at(&self, pos: TextByte) -> Option<char> {
        self.rope.get_char(pos.0)
    }

    /// Get a slice of text.
    #[must_use]
    pub fn slice(&self, range: Span<TextByte>) -> RopeSlice<'_> {
        self.rope.byte_slice(range.start.0..range.end.0)
    }

    /// Get a slice of text as a string.
    #[must_use]
    pub fn slice_str(&self, range: Span<TextByte>) -> &str {
        // Byte offsets always land on char boundaries (edits are char-safe),
        // and the mirrored text is the read path — see [`Self::text`].
        &self.text[range.start.0..range.end.0]
    }

    /// Get the line count.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.rope.len_lines()
    }

    /// Get the byte offset of a line.
    #[must_use]
    pub fn line_start(&self, line: usize) -> TextByte {
        TextByte(self.rope.line_to_byte(line))
    }

    /// Get the byte offset just past the end of a line.
    #[must_use]
    pub fn line_end(&self, line: usize) -> TextByte {
        if line + 1 < self.line_count() {
            self.line_start(line + 1)
        } else {
            TextByte(self.rope.len_bytes())
        }
    }

    /// Get the line at a byte offset.
    #[must_use]
    pub fn line_at(&self, pos: TextByte) -> usize {
        self.rope.byte_to_line(pos.0)
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
        self.apply_edit_op(edit);
        self.eol_table = EolTable::default();
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
    /// This is the core method that applies an edit, updates the rope,
    /// handles undo history, and advances the revision.
    pub fn apply_edit_op(&mut self, edit: EditOp) -> Revision {
        self.apply_text_edit(&edit);

        // Apply to undo tree
        let snapshot = self.snapshot();
        self.undo_tree.apply(edit, snapshot);
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
        let old_text = self.text();
        let new_text = snapshot.text().to_string();
        let mut edit = EditOp::new(
            Span::new(TextByte(0), TextByte(old_text.len())),
            new_text.clone(),
        );
        if let Some(tag) = journal {
            edit = edit.with_journal(tag);
        }
        self.rope = Rope::from_str(&new_text);
        self.text = new_text;
        self.eol_table = EolTable::from_snapshot(&snapshot);
        if let Some(format) = snapshot.format() {
            self.source_format = format;
            self.format_stamp = FormatStamp::detected(format);
        }
        self.undo_tree.apply_boundary(edit, snapshot.clone());
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
            self.source_format,
            &EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        )
    }

    /// The DECODED UTF-8 layer: normalized text with physical EOLs restored
    /// and no BOM. This is the internal serialization input — NOT disk
    /// source bytes; use [`EditorModel::try_to_source_bytes`] for those.
    #[must_use]
    pub fn decoded_text(&self) -> String {
        decoded_layer(self.text(), &self.eol_table)
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
        self.source_format = target;
        self.format_stamp = FormatStamp::detected(target);
        // Isolated undo node: the node snapshot carries the NEW descriptor
        // (its parent keeps the old one), so Redo lands the format change.
        let snapshot = self.snapshot();
        let edit = EditOp::new(Span::new(TextByte(0), TextByte(0)), String::new());
        self.undo_tree.apply_boundary(edit, snapshot);
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
        if edits.is_empty() {
            return self.revision;
        }
        let old_text = self.text();
        let mut ordered = edits.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        if ordered.iter().any(|edit| {
            edit.range.start.0 > edit.range.end.0
                || edit.range.end.0 > old_text.len()
                || !old_text.is_char_boundary(edit.range.start.0)
                || !old_text.is_char_boundary(edit.range.end.0)
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
            replacement.push_str(&old_text[cursor..edit.range.start.0]);
            replacement.push_str(&edit.replacement);
            cursor = edit.range.end.0;
        }
        replacement.push_str(&old_text[cursor..end]);
        let mut combined = EditOp::new(Span::new(TextByte(start), TextByte(end)), replacement);
        let is_journal = journal.is_some() || edits.iter().any(|edit| edit.boundary);
        if let Some(tag) = journal {
            combined = combined.with_journal(tag);
        }
        self.apply_text_edit(&combined);
        let snapshot = self.snapshot();
        if is_journal {
            self.undo_tree.apply_boundary(combined, snapshot)
        } else {
            self.undo_tree.apply(combined, snapshot)
        };
        let revision = self.revision.next();
        self.revision = revision;
        self.is_dirty = true;
        revision
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
        let text = self.text();
        let mut ordered = edits.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        for edit in &ordered {
            if edit.range.start.0 > edit.range.end.0 {
                return Some(BatchError::OutOfBounds {
                    at: edit.range.start.0,
                    len: text.len(),
                });
            }
            if edit.range.end.0 > text.len() {
                return Some(BatchError::OutOfBounds {
                    at: edit.range.end.0,
                    len: text.len(),
                });
            }
            if !text.is_char_boundary(edit.range.start.0)
                || !text.is_char_boundary(edit.range.end.0)
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

    /// Prepare a batch candidate OFF the interactive turn (D-06/TD-7): all
    /// validation, the combined edit and the after-state undo snapshot are
    /// computed from the immutable current state, so the later install is
    /// O(edit) and can never fail or partially apply. The after-state EOL
    /// table is spliced (prefix/suffix/dominant), never reconstructed from
    /// the whole document.
    ///
    /// # Errors
    ///
    /// [`BatchError`] when the batch is empty, out of bounds, not
    /// char-boundary aligned, or overlapping — with zero state changes.
    pub fn prepare_batch(&self, edits: &[EditOp]) -> Result<PreparedBatch, BatchError> {
        if let Some(error) = self.batch_error(edits) {
            return Err(error);
        }
        let old_text = self.text();
        let mut ordered = edits.to_vec();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        let start = ordered[0].range.start.0;
        let end = ordered.last().map_or(start, |edit| edit.range.end.0);
        let mut replacement = String::new();
        let mut cursor = start;
        for edit in &ordered {
            replacement.push_str(&old_text[cursor..edit.range.start.0]);
            replacement.push_str(&edit.replacement);
            cursor = edit.range.end.0;
        }
        replacement.push_str(&old_text[cursor..end]);
        let combined = EditOp::new(Span::new(TextByte(start), TextByte(end)), replacement);
        let after_text = combined.apply(old_text);
        let after_table = spliced_eol_table(&self.eol_table, old_text, &after_text);
        let decoded = decoded_layer(&after_text, &after_table);
        let undo_snapshot = match codec_encode(
            &decoded,
            self.source_format,
            &EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        ) {
            Ok(source) => {
                TextSnapshot::with_policy(&source, DecodePolicy::Explicit(self.source_format))
                    .unwrap_or_else(|_| {
                        TextSnapshot::from_normalized_with_descriptor(
                            &after_text,
                            self.source_format,
                        )
                    })
            }
            // A strict-ASCII-incompatible after-state still records the
            // exact text/EOL/format intent so Undo/Redo restore it (§6.10).
            Err(_) => {
                TextSnapshot::from_normalized_with_descriptor(&after_text, self.source_format)
            }
        };
        Ok(PreparedBatch {
            op: combined,
            undo_snapshot,
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
        self.apply_text_edit(&batch.op);
        let policy = match context_origin {
            Some(origin) => crate::undo::UndoPolicy::Context { origin },
            None => crate::undo::UndoPolicy::Isolated,
        };
        self.undo_tree
            .apply_batch(batch.op, batch.undo_snapshot, policy, &batch.spans);
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
                    batch.undo_snapshot.text().len(),
                    batch.op.replacement.len(),
                    &[],
                    &[],
                );
                let lease = self.reserve_history(projected)?;
                let revision = self.install_prepared_inner(batch, Some(origin));
                self.undo_tree.release(&lease);
                Ok(revision)
            }
            crate::undo::UndoPolicy::Isolated => {
                let batch = self.prepare_batch(edits)?;
                let projected = crate::budget::account_node(
                    batch.undo_snapshot.text().len(),
                    batch.op.replacement.len(),
                    &[],
                    &[],
                );
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
    /// after-snapshot text + combined replacement text + recorded selection
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
        let text = self.text();
        let mut ordered = edits.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        for edit in &ordered {
            if edit.range.start.0 > edit.range.end.0 || edit.range.end.0 > text.len() {
                return None;
            }
        }
        let start = ordered[0].range.start.0;
        let end = ordered.last().map_or(start, |edit| edit.range.end.0);
        let mut replacement = String::new();
        let mut cursor = start;
        for edit in &ordered {
            replacement.push_str(&text[cursor..edit.range.start.0]);
            replacement.push_str(&edit.replacement);
            cursor = edit.range.end.0;
        }
        replacement.push_str(&text[cursor..end]);
        let after_len = start + replacement.len() + (text.len() - end);
        let (before, after) = match record {
            Some(record) => (&record.before[..], &record.after[..]),
            None => (&[][..], &[][..]),
        };
        Some(crate::budget::account_node(
            after_len,
            replacement.len(),
            before,
            after,
        ))
    }

    /// Validate the text and recorded selections before installing either.
    pub fn try_apply_recorded_batch(
        &mut self,
        edits: &[EditOp],
        policy: crate::undo::UndoPolicy,
        record: crate::history::EditorEdit,
    ) -> Result<Revision, RecordedBatchError> {
        let batch = self
            .prepare_batch(edits)
            .map_err(RecordedBatchError::from)?;
        record
            .validate(self.text(), batch.undo_snapshot.text())
            .map_err(|e| RecordedBatchError::InvalidForm(e.to_string()))?;
        if let crate::undo::UndoPolicy::Context { origin } = policy
            && u128::from_str_radix(&record.origin, 16).ok() != Some(origin)
        {
            return Err(RecordedBatchError::InvalidForm(
                "Recorded editor origin does not match the undo group".into(),
            ));
        }
        let projected = crate::budget::account_node(
            batch.undo_snapshot.text().len(),
            batch.op.replacement.len(),
            &record.before,
            &record.after,
        );
        let lease = match self.reserve_history(projected) {
            Ok(lease) => lease,
            Err(BatchError::LimitExceeded { needed, budget }) => {
                return Err(RecordedBatchError::LimitExceeded { needed, budget });
            }
            Err(_) => unreachable!("prepare succeeded; only the limit refusal remains"),
        };
        self.apply_text_edit(&batch.op);
        self.undo_tree
            .apply_recorded(batch.op, batch.undo_snapshot, policy, record, &batch.spans);
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
        let snapshot = self.undo_tree.restore_snapshot(target)?;
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
        let (spans, truncated) = self.undo_tree.transition_spans(from, target.node);
        // Protect the restore target from budget eviction for the install
        // window; released on completion and on every error exit.
        let protection = self
            .undo_tree
            .protect_token(target.node, crate::undo::wall_clock_ms());
        // Allocate all replacement state before moving the head. Nothing
        // below is fallible; the lease window remains the backstop even if
        // an unexpected panic interrupts the release.
        let text = snapshot.text().to_owned();
        let rope = Rope::from_str(&text);
        let table = EolTable::from_snapshot(&snapshot);
        self.set_history_movement(spans, truncated, selections, primary);
        self.undo_tree.restore_position(target.node);
        self.text = text;
        self.rope = rope;
        self.eol_table = table;
        if let Some(format) = snapshot.format() {
            self.source_format = format;
            self.format_stamp = FormatStamp::detected(format);
        }
        self.revision = self.revision.next();
        self.is_dirty = self.base_hash != Some(BaseHash::from_snapshot(&snapshot));
        self.undo_tree.unprotect(protection);
        Ok(self.revision)
    }

    fn set_history_movement(
        &mut self,
        spans: Vec<crate::history::MovementSpan>,
        truncated: bool,
        selections: Option<Vec<[usize; 3]>>,
        primary: Option<usize>,
    ) {
        self.history_movement = Some(crate::history::HistoryMovement {
            selections,
            spans,
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
    ) -> (Vec<crate::history::MovementSpan>, bool) {
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

    /// The snapshot text of a retained history node (diagnostic/test).
    #[must_use]
    pub fn history_node_text(&self, id: u64) -> Option<String> {
        self.undo_tree
            .get(id)
            .filter(|node| !node.evicted)
            .and_then(|node| node.snapshot.as_ref())
            .map(|snapshot| snapshot.text().to_string())
    }

    /// Undo the last edit.
    ///
    /// Returns the new revision and whether the undo was successful.
    /// Every undo advances the revision (§4.5.2).
    pub fn undo(&mut self) -> Option<Revision> {
        let selections = self.undo_tree.undo_selections();
        let from = self.undo_tree.position_id();
        let transition = self.undo_tree.undo_edit();
        let result = self.undo_tree.undo();

        if let Some((_node_rev, Some(snapshot))) = result {
            let to = self.undo_tree.position_id();
            let (spans, truncated) = self.undo_tree.transition_spans(from, to);
            self.set_history_movement(spans, truncated, selections, None);
            let new_text = snapshot.text().to_string();
            self.rope = Rope::from_str(&new_text);
            self.text = new_text;
            self.eol_table = EolTable::from_snapshot(&snapshot);
            // Undo restores the recorded descriptor too, not just text/EOL
            // (FR-25: format-only boundaries are ordinary isolated undo
            // units over buffer state).
            if let Some(format) = snapshot.format() {
                self.source_format = format;
                self.format_stamp = FormatStamp::detected(format);
            }
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

        if let Some((_node_rev, Some(snapshot))) = result {
            let to = self.undo_tree.position_id();
            let (spans, truncated) = self.undo_tree.transition_spans(from, to);
            self.set_history_movement(
                spans,
                truncated,
                self.undo_tree.position_selections(),
                self.history_position_primary(),
            );
            let new_text = snapshot.text().to_string();
            self.rope = Rope::from_str(&new_text);
            self.text = new_text;
            self.eol_table = EolTable::from_snapshot(&snapshot);
            if let Some(format) = snapshot.format() {
                self.source_format = format;
                self.format_stamp = FormatStamp::detected(format);
            }
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
                    .into_iter()
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
            .map(|set| set.members())
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

    /// Get the EOL table.
    #[must_use]
    pub fn eol_table(&self) -> &EolTable {
        &self.eol_table
    }

    /// Get the EOL table mutably.
    #[must_use]
    pub fn eol_table_mut(&mut self) -> &mut EolTable {
        &mut self.eol_table
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
            self.source_format,
            &EncodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        );
        if let Ok(source) = encoded {
            let source: std::sync::Arc<[u8]> = source.into();
            if let Ok(map) = DecodeMap::build(
                source.clone(),
                self.source_format,
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
        TextSnapshot::from_normalized_with_descriptor(self.text(), self.source_format)
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

    /// Update text and its Rope index without rebuilding either document.
    /// Ordinary character edits keep every line's EOL metadata unchanged;
    /// newline-changing edits splice only the affected line range (S01).
    fn apply_text_edit(&mut self, edit: &EditOp) {
        let range = edit.range.start.0..edit.range.end.0;
        let changes_lines =
            self.text[range.clone()].contains('\n') || edit.replacement.contains('\n');
        // The bounded splice is computed off the pre-edit text and rope, so
        // no whole-document copy is needed.
        let spliced = changes_lines.then(|| self.affected_eol_splice(edit, &range));
        let ended_with_lf = self.text.ends_with('\n');
        let start = self.rope.byte_to_char(range.start);
        let end = self.rope.byte_to_char(range.end);
        self.rope.remove(start..end);
        self.rope.insert(start, &edit.replacement);
        self.text.replace_range(range, &edit.replacement);
        if let Some(table) = spliced {
            self.eol_table = table;
        } else if ended_with_lf && !self.text.ends_with('\n') {
            // Typing in the synthetic empty Rope line creates a real,
            // unterminated source line; it inherits no terminator.
            self.eol_table
                .line_endings
                .push(crate::eol::LineEnding::None);
            self.eol_table.has_trailing_newline = false;
        } else if !ended_with_lf && self.text.ends_with('\n') {
            self.eol_table.line_endings.pop();
            self.eol_table.has_trailing_newline = true;
        }
    }

    /// The bounded affected-range EOL splice for one contiguous edit against
    /// the current text/rope (§6.9, S01). The affected old-line window comes
    /// from the rope's line index and only those entries are recomputed;
    /// everything else is byte-identical to [`spliced_eol_table`], which
    /// remains the reference path for prepared batches.
    fn affected_eol_splice(&self, edit: &EditOp, range: &std::ops::Range<usize>) -> EolTable {
        let (first_rope_line, tail_end) = if range.is_empty() {
            // Pure insertion: the inserted lines join the line at the point.
            let line = self.rope.byte_to_line(range.start);
            (line, self.line_end_byte(line))
        } else {
            let last = self.rope.byte_to_line(range.end - 1);
            let tail_end = if edit.replacement.ends_with('\n') {
                self.line_end_byte(last)
            } else {
                // The replacement's tail merges the following old line when
                // it lacks a trailing newline: the window runs to the next
                // line boundary at or after the edit end.
                match self.text[range.end..].find('\n') {
                    Some(offset) => range.end + offset + 1,
                    None => self.text.len(),
                }
            };
            (self.rope.byte_to_line(range.start), tail_end)
        };
        // Rope line indices double as table indices, except the synthetic
        // trailing line after a final newline (which has no table entry).
        let window_first = first_rope_line.min(self.eol_table.line_count().saturating_sub(1));
        let window_start = self.rope.line_to_byte(window_first);
        let window_new = format!(
            "{}{}{}",
            &self.text[window_start..range.start],
            edit.replacement,
            &self.text[range.end..tail_end],
        );
        spliced_eol_table_in_window(
            &self.eol_table,
            &self.text,
            window_start,
            tail_end,
            &window_new,
            window_first,
        )
    }

    /// Byte offset of the end of `line` (after its newline; the text end for
    /// the final line).
    fn line_end_byte(&self, line: usize) -> usize {
        if line + 1 < self.rope.len_lines() {
            self.rope.line_to_byte(line + 1)
        } else {
            self.text.len()
        }
    }

    /// Set the coalesce window for the undo tree.
    pub fn set_coalesce_window(&mut self, window: Duration) {
        self.undo_tree.set_coalesce_window(window);
    }
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

fn logical_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        vec![""]
    } else {
        text.split_inclusive('\n').collect()
    }
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
        // Ropey splits its internal chunks well below this size; a text read
        // through `RopeSlice::as_str` silently returns "" for any slice
        // spanning more than one chunk, which once blanked every note over
        // ~1KB (and serialized that emptiness on save).
        let text = "filler line for the chunk-boundary test\n".repeat(300);
        let model = EditorModel::from_text(&text);
        assert_eq!(model.text(), text);
        assert_eq!(model.len(), text.len());

        // Reads stay correct across every path that rebuilds the rope.
        let mut model = model;
        let replacement = "replaced\n".repeat(400);
        model.replace_snapshot(TextSnapshot::new(replacement.as_bytes()).unwrap());
        assert_eq!(model.text(), replacement);

        let mut model = model;
        model.apply_edit_op(EditOp::new(
            Span::new(TextByte(0), TextByte(9)),
            "EDITED".into(),
        ));
        assert!(model.text().starts_with("EDITED"));
        assert!(model.text().contains("replaced"));

        let mut model = model;
        model.undo();
        assert_eq!(model.text(), replacement);

        let mut model = model;
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
        let initial = model.eol_table().clone();
        model.apply_edit_ops(&[EditOp::new(
            Span::new(TextByte(1), TextByte(2)),
            "🧑🏽‍💻".into(),
        )]);
        assert_eq!(model.eol_table(), &initial);
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
        assert_eq!(model.eol_table(), &initial);
        assert!(model.undo().is_some());
        assert!(model.text().ends_with('é'));
        assert_eq!(model.snapshot().source(), model.to_source_bytes());
        assert!(model.redo().is_some());
        assert_eq!(model.eol_table(), &initial);
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
    fn edits_keep_the_rope_index_in_sync_across_chunks_and_unicode() {
        let mut model = EditorModel::from_text(&"aé\n".repeat(3000));
        for line in [0, 300, 2000, 3000] {
            let at = model.line_start(line).0;
            let before = model.revision();
            model.apply_edit_ops(&[EditOp::new(
                Span::new(TextByte(at), TextByte(at)),
                "🧑🏽‍💻".into(),
            )]);
            assert!(model.revision() > before);
            assert_eq!(model.rope.to_string(), model.text());
            assert_eq!(model.snapshot().text(), model.text());
            assert_eq!(model.to_source_bytes(), model.text().as_bytes());
        }
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
            let old_table = model.eol_table().clone();
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
            // The reference: the whole-document splice prepare_batch uses.
            let expected = spliced_eol_table(
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
                &expected,
                "iteration {iteration}: {start}..{end} -> {replacement:?} on {old_text:?}"
            );
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
mod splice_debug {
    use super::*;

    #[test]
    fn debug_case_iteration2() {
        let text = "e\ne\nc\n";
        let model = EditorModel::from_text(text);
        eprintln!("old_text={:?} table={:?}", model.text(), model.eol_table());
        let old_table = model.eol_table().clone();
        let old_text = model.text().to_string();
        let replacement = "\r\ne\n".to_string();
        let new_text = format!("{}{}{}", &old_text[..0], replacement, &old_text[1..]);
        eprintln!("new_text={:?}", new_text);
        let expected = spliced_eol_table(&old_table, &old_text, &new_text);
        eprintln!(
            "spliced={:?} dominant={:?}",
            expected,
            old_table.dominant_eol()
        );
        let mut edited = EditorModel::from_text(text);
        edited.apply_edit_ops(&[EditOp::new(
            Span::new(TextByte(0), TextByte(1)),
            replacement.clone(),
        )]);
        eprintln!("bounded={:?}", edited.eol_table());
    }
}
