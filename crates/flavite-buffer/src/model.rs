//! EditorModel — the headless text buffer (§4.14, D14).
//!
//! This is the core of the buffer crate: a pure, testable, headless model
//! that owns:
//! - A `ropey::Rope` for the text content
//! - Multi-selection support
//! - Undo/redo via an undo tree
//! - Per-line EOL metadata
//!
//! All editing operations produce `EditOp`s and are applied through
//! the model. The model maintains revision numbers and can provide
//! revisioned read views.

use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use ropey::{Rope, RopeSlice};
use std::collections::HashMap;
use std::time::Duration;

use crate::eol::EolTable;
use crate::revision::{BaseHash, BufferId, Revision};
use crate::undo::{EditOp, UndoTree};

/// A selection range in TextByte coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Anchor position (where the selection started).
    pub anchor: TextByte,
    /// Head position (where the selection ended).
    pub head: TextByte,
}

impl Selection {
    /// Create a new selection from anchor to head.
    #[must_use]
    pub fn new(anchor: TextByte, head: TextByte) -> Self {
        Self { anchor, head }
    }

    /// Create a collapsed selection (caret) at a position.
    #[must_use]
    pub fn caret(at: TextByte) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    /// Check if this is a collapsed selection (caret).
    #[must_use]
    pub fn is_caret(&self) -> bool {
        self.anchor == self.head
    }

    /// Get the start of the selection (min of anchor and head).
    #[must_use]
    pub fn start(&self) -> TextByte {
        TextByte(std::cmp::min(self.anchor.0, self.head.0))
    }

    /// Get the end of the selection (max of anchor and head).
    #[must_use]
    pub fn end(&self) -> TextByte {
        TextByte(std::cmp::max(self.anchor.0, self.head.0))
    }

    /// Get the span of the selection.
    #[must_use]
    pub fn span(&self) -> Span<TextByte> {
        Span::new(self.start(), self.end())
    }

    /// Check if the selection contains a position.
    #[must_use]
    pub fn contains(&self, pos: TextByte) -> bool {
        let start = self.start().0;
        let end = self.end().0;
        pos.0 >= start && pos.0 <= end
    }

    /// Move both anchor and head by an offset.
    #[must_use]
    pub fn offset(self, by: i64) -> Self {
        let shift = |position: usize| {
            if by >= 0 {
                position.saturating_add(by as usize)
            } else {
                let magnitude = usize::try_from(by.unsigned_abs()).unwrap_or(usize::MAX);
                position.saturating_sub(magnitude)
            }
        };
        let new_anchor = shift(self.anchor.0);
        let new_head = shift(self.head.0);
        Self {
            anchor: TextByte(new_anchor),
            head: TextByte(new_head),
        }
    }
}

/// The headless editor model.
///
/// This is the core buffer type that owns the text, selections, undo history,
/// and EOL metadata. It is pure and has zero UI dependencies (D14).
#[derive(Debug)]
pub struct EditorModel {
    /// Unique identity for this buffer instance.
    buffer_id: BufferId,
    /// The text content (normalized, CRLF folded to LF).
    rope: Rope,
    /// Multi-selection: map from selection index to selection.
    /// The "primary" selection is always at index 0.
    selections: HashMap<usize, Selection>,
    /// The undo tree for edit history.
    undo_tree: UndoTree,
    /// Per-line EOL information.
    eol_table: EolTable,
    /// Current revision number.
    revision: Revision,
    /// Base hash of the content when last loaded or saved.
    base_hash: Option<BaseHash>,
    /// Whether the buffer has unsaved changes.
    is_dirty: bool,
}

impl EditorModel {
    /// Create a new editor model from a text snapshot.
    #[must_use]
    pub fn from_snapshot(snapshot: TextSnapshot) -> Self {
        let text = snapshot.text().to_string();
        let rope = Rope::from_str(&text);
        let eol_table = EolTable::from_snapshot(&snapshot);

        Self {
            buffer_id: BufferId::new(),
            rope,
            selections: HashMap::new(),
            undo_tree: UndoTree::new(snapshot.clone()),
            eol_table,
            revision: Revision::INITIAL,
            base_hash: Some(BaseHash::from_snapshot(&snapshot)),
            is_dirty: false,
        }
    }

    /// Replace the document snapshot while preserving buffer identity and
    /// advancing its revision. Used when restoring a durable recovery file.
    pub fn replace_snapshot(&mut self, snapshot: TextSnapshot) {
        self.rope = Rope::from_str(snapshot.text());
        self.eol_table = EolTable::from_snapshot(&snapshot);
        self.selections.clear();
        self.undo_tree = UndoTree::new(snapshot.clone());
        self.revision = self.revision.next();
        self.base_hash = Some(BaseHash::from_snapshot(&snapshot));
        self.is_dirty = false;
    }

    /// Create a new empty editor model.
    #[must_use]
    pub fn new_empty() -> Self {
        let snapshot = TextSnapshot::new(b"").unwrap();
        Self::from_snapshot(snapshot)
    }

    /// Create a new editor model with plain text (no EOL info, defaults to LF).
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let snapshot = TextSnapshot::new(text.as_bytes()).unwrap();
        Self::from_snapshot(snapshot)
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
        // Rope::slice(..) returns a RopeSlice which can be converted to &str
        // We use the Display trait which RopeSlice implements, but we need a &'static str
        // Actually, Rope has a method to get the full string
        // For now, we'll convert via String
        // This is not ideal for performance but works for now
        // A better approach would be to store the normalized text separately
        // or use ropey's chunk access
        self.rope.slice(..).as_str().unwrap_or("")
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
        self.rope
            .byte_slice(range.start.0..range.end.0)
            .as_str()
            .unwrap_or("")
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

        // Count newlines in the inserted text
        let newlines = text.chars().filter(|&c| c == '\n').count();

        let edit = EditOp::new(Span::new(pos, pos), text.to_string());

        // Update EOL metadata
        self.update_eol_for_insert(pos, newlines);

        self.apply_edit_op(edit)
    }

    /// Delete text from start to end.
    ///
    /// Returns the new revision.
    pub fn delete(&mut self, range: Span<TextByte>) -> Revision {
        if range.is_empty() {
            return self.revision;
        }

        // Count newlines in the deleted range
        let start_line = self.line_at(range.start);
        let end_line = self.line_at(range.end);
        let newlines_deleted = end_line.saturating_sub(start_line);

        let edit = EditOp::new(range, String::new());

        // Update EOL metadata
        self.update_eol_for_delete(range, newlines_deleted);

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
        let old_text = self.text().to_string();
        let revision = self.apply_edit_op(edit);
        self.rebuild_eol_table(&old_text);
        revision
    }

    /// Apply an edit operation.
    ///
    /// This is the core method that applies an edit, updates the rope,
    /// handles undo history, and advances the revision.
    pub fn apply_edit_op(&mut self, edit: EditOp) -> Revision {
        // Apply to rope
        let old_text = self.text().to_string();
        let new_text = edit.apply(&old_text);
        self.rope = Rope::from_str(&new_text);

        // Apply to undo tree
        let snapshot = TextSnapshot::new(new_text.as_bytes()).unwrap();
        let new_revision = self.undo_tree.apply(edit, snapshot);

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
        let old_text = self.text().to_string();
        let new_text = snapshot.text().to_string();
        let edit = EditOp::new(
            Span::new(TextByte(0), TextByte(old_text.len())),
            new_text.clone(),
        );
        self.rope = Rope::from_str(&new_text);
        self.eol_table = EolTable::from_snapshot(&snapshot);
        let revision = self.undo_tree.apply_boundary(edit, snapshot.clone());
        self.revision = revision;
        self.base_hash = Some(BaseHash::from_snapshot(&snapshot));
        self.is_dirty = true;
        revision
    }

    /// Apply multiple edit operations as a single atomic edit.
    ///
    /// All edits are applied in order, but they share a single undo entry
    /// and revision advancement.
    pub fn apply_edit_ops(&mut self, edits: &[EditOp]) -> Revision {
        if edits.is_empty() {
            return self.revision;
        }
        let old_text = self.text().to_string();
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
        let revision = self.apply_edit_op(EditOp::new(
            Span::new(TextByte(start), TextByte(end)),
            replacement,
        ));
        self.rebuild_eol_table(&old_text);
        revision
    }

    /// Whether edits are non-overlapping ranges against this exact text
    /// snapshot and land on valid UTF-8 boundaries.
    #[must_use]
    pub fn edit_ops_valid(&self, edits: &[EditOp]) -> bool {
        if edits.is_empty() {
            return true;
        }
        let text = self.text();
        let mut ordered = edits.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|edit| (edit.range.start.0, edit.range.end.0));
        if ordered.iter().any(|edit| {
            edit.range.start.0 > edit.range.end.0
                || edit.range.end.0 > text.len()
                || !text.is_char_boundary(edit.range.start.0)
                || !text.is_char_boundary(edit.range.end.0)
        }) {
            return false;
        }
        !ordered.windows(2).any(|pair| {
            pair[1].range.start.0 < pair[0].range.end.0
                || (pair[1].range.start.0 == pair[0].range.start.0
                    && pair[0].range.is_empty()
                    && pair[1].range.is_empty())
        })
    }

    /// Undo the last edit.
    ///
    /// Returns the new revision and whether the undo was successful.
    /// Every undo advances the revision (§4.5.2).
    pub fn undo(&mut self) -> Option<Revision> {
        let result = self.undo_tree.undo();

        if let Some((_node_rev, Some(snapshot))) = result {
            let new_text = snapshot.text().to_string();
            self.rope = Rope::from_str(&new_text);
            self.eol_table = EolTable::from_snapshot(&snapshot);
            self.revision = self.revision.next();
            self.is_dirty = true;
            Some(self.revision)
        } else if let Some((_node_rev, None)) = result {
            // We have a revision but no snapshot - this shouldn't happen
            // in normal operation
            self.revision = self.revision.next();
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
        let result = self.undo_tree.redo();

        if let Some((_node_rev, Some(snapshot))) = result {
            let new_text = snapshot.text().to_string();
            self.rope = Rope::from_str(&new_text);
            self.eol_table = EolTable::from_snapshot(&snapshot);
            self.revision = self.revision.next();
            self.is_dirty = true;
            Some(self.revision)
        } else if let Some((_node_rev, None)) = result {
            self.revision = self.revision.next();
            Some(self.revision)
        } else {
            None
        }
    }

    /// Get the primary selection.
    #[must_use]
    pub fn primary_selection(&self) -> Selection {
        self.selections
            .get(&0)
            .copied()
            .unwrap_or_else(|| Selection::caret(TextByte(0)))
    }

    /// Set the primary selection.
    pub fn set_primary_selection(&mut self, selection: Selection) {
        self.selections.insert(0, selection);
    }

    /// Add a selection.
    pub fn add_selection(&mut self, selection: Selection) -> usize {
        let id = self.selections.len();
        self.selections.insert(id, selection);
        id
    }

    /// Remove a selection by ID.
    pub fn remove_selection(&mut self, id: usize) -> bool {
        self.selections.remove(&id).is_some()
    }

    /// Clear all selections except the primary.
    pub fn clear_secondary_selections(&mut self) {
        self.selections.retain(|&k, _| k == 0);
    }

    /// Get all selections.
    #[must_use]
    pub fn selections(&self) -> Vec<Selection> {
        self.selections.values().cloned().collect()
    }

    /// Select all text.
    pub fn select_all(&mut self) {
        let start = TextByte(0);
        let end = TextByte(self.len());
        self.set_primary_selection(Selection::new(start, end));
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

    /// Serialize the buffer to bytes with EOL metadata applied.
    ///
    /// This produces the exact source bytes for the current buffer state,
    /// including BOM (if present in the original), per-line EOL, and
    /// trailing newline state.
    #[must_use]
    pub fn to_source_bytes(&self) -> Vec<u8> {
        // Get the normalized text
        let text = self.text();

        // Build line_breaks as the end of each line (excluding the newline)
        let mut line_breaks = Vec::new();
        // The side table follows source lines, while Ropey's `len_lines`
        // includes a synthetic empty line after a final LF. That synthetic
        // line has no source EOL entry and must not be serialized separately.
        for line in 0..self.eol_table.line_count() {
            let _line_start = self.line_start(line);
            let line_end = self.line_end(line);

            // line_end is the start of the next line (after the newline)
            // If this line has an EOL (not None), subtract 1 to exclude the newline
            let eol = self.eol_table.get(line);
            let break_pos = if eol.is_some_and(|eol| eol != crate::eol::LineEnding::None) {
                // This line has an EOL, so line_end is after the newline
                // Subtract 1 to get the position before the newline
                TextByte(line_end.0 - 1)
            } else {
                // No EOL, line_end is the end of the line
                line_end
            };
            line_breaks.push(break_pos);
        }

        self.eol_table.serialize(text, &line_breaks)
    }

    /// Create a text snapshot of the current buffer state.
    ///
    /// This creates a snapshot with normalized text (CRLF folded) but
    /// preserves the EOL metadata separately.
    #[must_use]
    pub fn snapshot(&self) -> TextSnapshot {
        let source_bytes = self.to_source_bytes();
        TextSnapshot::new(&source_bytes).unwrap()
    }

    /// Get the content hash of the current buffer.
    #[must_use]
    pub fn content_hash(&self) -> BaseHash {
        let source_bytes = self.to_source_bytes();
        let hash = blake3::hash(&source_bytes);
        BaseHash::from_bytes(hash.into())
    }

    /// Check if the buffer content matches a given hash.
    #[must_use]
    pub fn matches_hash(&self, hash: BaseHash) -> bool {
        self.content_hash() == hash
    }

    /// Update EOL metadata for an insert operation.
    fn update_eol_for_insert(&mut self, pos: TextByte, newlines: usize) {
        let line_idx = self.line_at(pos);
        let dominant = self.eol_table.dominant_eol();

        // For newly inserted lines, use the dominant EOL
        for i in 0..newlines {
            self.eol_table.insert(line_idx + 1 + i, dominant);
        }

        // If we inserted a newline, the current line now has an EOL
        // (but we don't change existing lines)
        if newlines > 0 {
            // The line we inserted into might now have content after pos
            // We don't change its EOL unless we're inserting at the end
        }
    }

    /// Update EOL metadata for a delete operation.
    fn update_eol_for_delete(&mut self, range: Span<TextByte>, newlines: usize) {
        let start_line = self.line_at(range.start);

        // Remove the EOL entries for deleted lines
        for _ in 0..newlines {
            if start_line + 1 < self.eol_table.line_count() {
                self.eol_table.remove(start_line + 1);
            }
        }
    }

    /// Rebuild the EOL table from the current rope state.
    ///
    /// This is called when we can't easily update the EOL table incrementally.
    fn rebuild_eol_table(&mut self, old_text: &str) {
        let dominant = self.eol_table.dominant_eol();
        let has_bom = self.eol_table.has_bom();
        let snapshot =
            TextSnapshot::new(self.text().as_bytes()).expect("EditorModel text is valid UTF-8");
        let mut new_table = EolTable::from_snapshot(&snapshot);
        let old_lines = logical_lines(old_text);
        let new_lines = logical_lines(self.text());
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
                .and_then(|old_line| self.eol_table.get(old_line))
                .unwrap_or(dominant);
        }
        new_table.has_bom = has_bom;

        self.eol_table = new_table;
    }

    /// Set the coalesce window for the undo tree.
    pub fn set_coalesce_window(&mut self, window: Duration) {
        self.undo_tree.set_coalesce_window(window);
    }
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
        assert!(model.eol_table().has_bom());
        assert!(!model.eol_table().has_trailing_newline());
        assert_eq!(model.to_source_bytes(), "\u{FEFF}single line".as_bytes());
        let _ = model.content_hash();
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
        model.set_primary_selection(Selection::new(TextByte(0), TextByte(5)));
        let sel = model.primary_selection();
        assert_eq!(sel.start(), TextByte(0));
        assert_eq!(sel.end(), TextByte(5));
        assert!(!sel.is_caret());

        // Caret selection
        model.set_primary_selection(Selection::caret(TextByte(5)));
        let sel = model.primary_selection();
        assert!(sel.is_caret());
        assert_eq!(sel.start(), TextByte(5));
        assert_eq!(sel.end(), TextByte(5));
    }

    #[test]
    fn select_all() {
        let mut model = EditorModel::from_text("hello");
        model.select_all();
        let sel = model.primary_selection();
        assert_eq!(sel.start(), TextByte(0));
        assert_eq!(sel.end(), TextByte(5));
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
        assert_eq!(model.primary_selection(), Selection::caret(TextByte(0)));

        let reversed = Selection::new(TextByte(4), TextByte(1));
        assert!(!reversed.is_caret());
        assert_eq!(reversed.start(), TextByte(1));
        assert_eq!(reversed.end(), TextByte(4));
        assert_eq!(reversed.span(), Span::new(TextByte(1), TextByte(4)));
        assert!(reversed.contains(TextByte(1)) && reversed.contains(TextByte(4)));
        assert!(!reversed.contains(TextByte(5)));
        assert_eq!(reversed.offset(-9), Selection::caret(TextByte(0)));

        model.set_base_hash(BaseHash::from_bytes([9; 32]));
        model.set_dirty(true);
        model.set_primary_selection(reversed);
        model.add_selection(Selection::caret(TextByte(2)));
        model.replace_snapshot(TextSnapshot::new(b"fresh\n").unwrap());
        assert_eq!(model.text(), "fresh\n");
        assert_eq!(model.revision(), Revision::new(1));
        assert!(!model.is_dirty());
        assert_eq!(model.selections().len(), 0);
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
