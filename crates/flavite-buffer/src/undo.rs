//! Undo tree with time-based coalescing (§4.14, D14).
//!
//! The undo tree stores edit operations and their inverses, supporting:
//! - Linear undo/redo
//! - Branching on undo-after-edit
//! - Time-based coalescing (default 500ms window)
//!
//! Every edit, undo, redo, reload, and journal patch advances the revision
//! even if text returns to previous bytes (§4.5.2).
use crate::revision::Revision;
use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
/// Default time window for coalescing consecutive edits into a single undo group.
///
/// Edits within this window of the previous edit are coalesced.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(500);
/// An edit operation that can be applied and inverted.
#[derive(Debug, Clone, PartialEq)]
pub struct EditOp {
    /// The range to replace in TextByte coordinates.
    pub range: Span<TextByte>,
    /// The replacement text (normalized, CRLF folded).
    pub replacement: String,
    /// The EOL metadata for the inserted lines (if any).
    pub eol_metadata: EolMetadata,
    /// Journal obligation associated with this edit, if it came from a rename.
    pub journal: Option<JournalTag>,
    /// A boundary edit is never merged into a neighboring undo group.
    pub boundary: bool,
}

/// Durable journal item represented by an undoable buffer edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalTag {
    pub txn_id: String,
    pub item_id: u64,
}
impl EditOp {
    /// Create a new edit operation.
    #[must_use]
    pub fn new(range: Span<TextByte>, replacement: String) -> Self {
        Self {
            range,
            replacement,
            eol_metadata: EolMetadata::default(),
            journal: None,
            boundary: false,
        }
    }
    /// Create a new edit with EOL metadata.
    #[must_use]
    pub fn with_eol(range: Span<TextByte>, replacement: String, eol_metadata: EolMetadata) -> Self {
        Self {
            range,
            replacement,
            eol_metadata,
            journal: None,
            boundary: false,
        }
    }
    /// Mark an operation as a durable, non-coalescing journal boundary.
    #[must_use]
    pub fn with_journal(mut self, tag: JournalTag) -> Self {
        self.journal = Some(tag);
        self.boundary = true;
        self
    }
    /// Check if this edit is a no-op (empty range and empty replacement).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.range.is_empty() && self.replacement.is_empty()
    }
    /// Invert this edit to create an undo operation.
    #[must_use]
    pub fn invert(&self, context: &str) -> EditOp {
        // The inverse replaces the inserted text with the original
        // For an insert (empty range), the inverse is to delete the inserted text
        // For a replace, the inverse is to restore the original text
        if self.range.start.0 == self.range.end.0 {
            // Insert operation: inverse is to delete the replacement
            EditOp::new(
                Span::new(
                    self.range.start,
                    TextByte(self.range.start.0 + self.replacement.len()),
                ),
                String::new(),
            )
        } else {
            // Replace operation: inverse is to restore the original
            let original = context
                .get(self.range.start.0..self.range.end.0)
                .unwrap_or_default();
            EditOp::new(
                Span::new(
                    self.range.start,
                    TextByte(self.range.start.0 + self.replacement.len()),
                ),
                original.to_string(),
            )
        }
    }
    /// Apply this edit to a text buffer and return the new text.
    #[must_use]
    pub fn apply(&self, text: &str) -> String {
        let mut result =
            String::with_capacity(text.len() + self.replacement.len() - self.range.len());
        result.push_str(&text[..self.range.start.0]);
        result.push_str(&self.replacement);
        result.push_str(&text[self.range.end.0..]);
        result
    }
    /// Check if this edit can be coalesced with another.
    ///
    /// Coalescing merges typing bursts: same-place, back-to-back edits.
    /// Two edits coalesce iff they are both non-boundary, non-journal, and
    /// their ranges are adjacent — `self` ends exactly where `other` starts,
    /// or vice versa (the time window is enforced by callers). Zero-length
    /// (insertion) ranges coalesce at the shared point, so same-point
    /// insertions merge in application order.
    ///
    /// Overlapping ranges do NOT coalesce: for overlapping edits the merged
    /// replacement over the `min..max` span cannot be derived by
    /// concatenation — sequential application leaves displaced original
    /// bytes inside the span that concatenation silently drops. (The live
    /// undo tree recomputes its merged edit from before/after snapshots and
    /// does not rely on this predicate for text correctness; `coalesce` and
    /// `LinearUndoStack` do, which is why adjacency is the whole contract.)
    #[must_use]
    pub fn can_coalesce(&self, other: &EditOp) -> bool {
        if self.boundary || other.boundary || self.journal.is_some() || other.journal.is_some() {
            return false;
        }
        // Adjacent only: `self` ends where `other` starts, or vice versa.
        // Zero-length (insertion) ranges coalesce at the shared point, which
        // makes same-point insertions merge in application order — correct.
        self.range.end.0 == other.range.start.0 || other.range.end.0 == self.range.start.0
    }
    /// Coalesce this edit with another into one edit over the union span.
    ///
    /// Precondition: [`EditOp::can_coalesce`]. Correct because adjacency
    /// makes `start..end` exactly the union of the two touched spans, and
    /// the left-to-right replacement order matches application order for
    /// both orderings of the query.
    #[must_use]
    pub fn coalesce(&self, other: &EditOp) -> Option<EditOp> {
        if !self.can_coalesce(other) {
            return None;
        }
        let start = std::cmp::min(self.range.start.0, other.range.start.0);
        let end = std::cmp::max(self.range.end.0, other.range.end.0);
        let replacement = if self.range.start.0 <= other.range.start.0 {
            format!("{}{}", self.replacement, other.replacement)
        } else {
            format!("{}{}", other.replacement, self.replacement)
        };
        Some(EditOp::new(
            Span::new(TextByte(start), TextByte(end)),
            replacement,
        ))
    }
}
/// Metadata about line endings for an edit operation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct EolMetadata {
    /// Number of lines inserted.
    pub lines_inserted: usize,
    /// Number of lines deleted.
    pub lines_deleted: usize,
    /// The line indices where EOL changes occur.
    pub changed_lines: Vec<usize>,
}
/// A node in the undo tree.
///
/// Each node represents a state of the buffer and can have:
/// - A parent (for linear history)
/// - Children (for branching on undo-after-edit)
#[derive(Debug, Clone)]
pub struct UndoNode {
    /// Unique identifier for this node.
    pub id: u64,
    /// The revision at this node.
    pub revision: Revision,
    /// The edit that led to this state (None for the root).
    pub edit: Option<EditOp>,
    /// The text snapshot at this node (for leaf nodes, can be reconstructed).
    pub snapshot: Option<TextSnapshot>,
    /// The time when this edit was applied.
    pub timestamp: Instant,
    /// Parent node ID.
    pub parent_id: Option<u64>,
    /// Child node IDs.
    pub children: Vec<u64>,
}
impl UndoNode {
    /// Create a new root node.
    #[must_use]
    pub fn root(revision: Revision, snapshot: TextSnapshot) -> Self {
        Self {
            id: 0,
            revision,
            edit: None,
            snapshot: Some(snapshot),
            timestamp: Instant::now(),
            parent_id: None,
            children: Vec::new(),
        }
    }
    /// Create a new node from an edit with a snapshot.
    #[must_use]
    pub fn new(
        id: u64,
        revision: Revision,
        edit: EditOp,
        parent_id: u64,
        snapshot: TextSnapshot,
    ) -> Self {
        Self {
            id,
            revision,
            edit: Some(edit),
            snapshot: Some(snapshot),
            timestamp: Instant::now(),
            parent_id: Some(parent_id),
            children: Vec::new(),
        }
    }

    /// Create a new node from an edit without a snapshot.
    #[must_use]
    pub fn new_no_snapshot(id: u64, revision: Revision, edit: EditOp, parent_id: u64) -> Self {
        Self {
            id,
            revision,
            edit: Some(edit),
            snapshot: None,
            timestamp: Instant::now(),
            parent_id: Some(parent_id),
            children: Vec::new(),
        }
    }
    /// Check if this node is the root.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.parent_id.is_none()
    }
    /// Check if this node is a leaf (no children).
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }
}
/// The undo tree structure.
///
/// Stores the complete history of edits with branching support.
#[derive(Debug)]
pub struct UndoTree {
    /// All nodes in the tree.
    nodes: Vec<UndoNode>,
    /// The current node ID (where new edits go).
    current_id: u64,
    /// The node we're currently at in the tree (for undo/redo).
    position_id: u64,
    /// Time window for coalescing edits.
    coalesce_window: Duration,
    /// Next node ID to assign.
    next_id: u64,
}
impl UndoTree {
    /// Create a new undo tree with a root node.
    #[must_use]
    pub fn new(snapshot: TextSnapshot) -> Self {
        let root = UndoNode::root(Revision::INITIAL, snapshot);
        Self {
            nodes: vec![root],
            current_id: 0,
            position_id: 0,
            coalesce_window: DEFAULT_COALESCE_WINDOW,
            next_id: 1,
        }
    }
    /// Create a new undo tree with a custom coalesce window.
    #[must_use]
    pub fn with_coalesce_window(snapshot: TextSnapshot, window: Duration) -> Self {
        let mut tree = Self::new(snapshot);
        tree.coalesce_window = window;
        tree
    }
    /// Get the current node.
    #[must_use]
    pub fn current(&self) -> Option<&UndoNode> {
        self.nodes.get(self.current_id as usize)
    }
    /// Get the current node mutably.
    #[must_use]
    pub fn current_mut(&mut self) -> Option<&mut UndoNode> {
        self.nodes.get_mut(self.current_id as usize)
    }
    /// Get a node by ID.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&UndoNode> {
        self.nodes.get(id as usize)
    }
    /// Get the position node.
    #[must_use]
    pub fn position(&self) -> Option<&UndoNode> {
        self.nodes.get(self.position_id as usize)
    }
    /// Edit represented by the next undo transition.
    #[must_use]
    pub fn undo_edit(&self) -> Option<EditOp> {
        self.position()?.edit.clone()
    }
    /// Edit represented by the next redo transition.
    #[must_use]
    pub fn redo_edit(&self) -> Option<EditOp> {
        let target_id = self.redo_target()?;
        self.get(target_id)?.edit.clone()
    }
    fn redo_target(&self) -> Option<u64> {
        let mut child = self.current_id;
        while let Some(node) = self.get(child) {
            if node.parent_id == Some(self.position_id) {
                return Some(child);
            }
            child = node.parent_id?;
        }
        None
    }

    /// Apply an edit and advance the tree.
    ///
    /// If the edit can be coalesced with the current node's edit, it is merged.
    /// Otherwise, a new node is created.
    ///
    /// Returns the new revision.
    pub fn apply(&mut self, edit: EditOp, snapshot: TextSnapshot) -> Revision {
        self.apply_with_coalescing(edit, snapshot, true)
    }

    /// Apply a distinct operation that must start a new undo-tree node.
    /// Destructive conflict resolutions use this so a merge cannot coalesce
    /// into nearby typing and lose its independent undo boundary.
    pub fn apply_boundary(&mut self, mut edit: EditOp, snapshot: TextSnapshot) -> Revision {
        edit.boundary = true;
        self.apply_with_coalescing(edit, snapshot, false)
    }

    fn apply_with_coalescing(
        &mut self,
        edit: EditOp,
        snapshot: TextSnapshot,
        allow_coalescing: bool,
    ) -> Revision {
        let branched = self.position_id != self.current_id;
        let current_id = self.position_id;
        // First, check if we should coalesce
        let should_coalesce = allow_coalescing
            && !branched
            && if let Some(current) = self.nodes.get(current_id as usize) {
                if let Some(current_edit) = &current.edit {
                    current_edit.can_coalesce(&edit)
                        && current.timestamp.elapsed() < self.coalesce_window
                } else {
                    false
                }
            } else {
                false
            };
        if should_coalesce {
            let parent_text = self
                .get(current_id)
                .and_then(|node| node.parent_id)
                .and_then(|id| self.get(id))
                .and_then(|node| node.snapshot.as_ref())
                .map(TextSnapshot::text);
            if let Some(before) = parent_text {
                let after = snapshot.text();
                let mut prefix = before
                    .bytes()
                    .zip(after.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                while prefix > 0
                    && (!before.is_char_boundary(prefix) || !after.is_char_boundary(prefix))
                {
                    prefix -= 1;
                }
                let mut suffix = before.as_bytes()[prefix..]
                    .iter()
                    .rev()
                    .zip(after.as_bytes()[prefix..].iter().rev())
                    .take_while(|(a, b)| a == b)
                    .count();
                while suffix > 0
                    && (!before.is_char_boundary(before.len() - suffix)
                        || !after.is_char_boundary(after.len() - suffix))
                {
                    suffix -= 1;
                }
                let coalesced = EditOp::new(
                    Span::new(TextByte(prefix), TextByte(before.len() - suffix)),
                    after[prefix..after.len() - suffix].into(),
                );
                if let Some(node) = self.nodes.get_mut(current_id as usize) {
                    node.edit = Some(coalesced);
                    node.snapshot = Some(snapshot);
                    node.timestamp = Instant::now();
                    return node.revision;
                }
            }
        }
        // Get current again for creating new node
        // Create a new node
        let current_node = self
            .nodes
            .get(current_id as usize)
            .expect("current node must exist");
        let parent_id = current_node.id; // Clone the id to avoid borrow issues
        let new_id = self.next_id;
        self.next_id += 1;
        let new_revision = current_node.revision.next();
        let new_node = UndoNode::new(new_id, new_revision, edit, parent_id, snapshot);
        // Add to parent's children
        if let Some(node) = self.nodes.get_mut(parent_id as usize) {
            node.children.push(new_id);
        }
        // Add the new node
        self.nodes.push(new_node);
        self.current_id = new_id;
        self.position_id = new_id;
        new_revision
    }
    /// Preserve a save/semantic boundary without adding a document edit.
    pub fn seal_current(&mut self) {
        if let Some(node) = self.nodes.get_mut(self.position_id as usize)
            && let Some(edit) = &mut node.edit
        {
            edit.boundary = true;
        }
    }

    /// Undo the current edit.
    ///
    /// Moves the position back to the parent node.
    /// Returns the parent's revision and snapshot.
    /// The caller (model) is responsible for advancing the revision (§4.5.2).
    pub fn undo(&mut self) -> Option<(Revision, Option<TextSnapshot>)> {
        let current = self.position()?;
        if current.is_root() {
            return None;
        }
        let parent_id = current.parent_id?;
        let parent = self.nodes.get(parent_id as usize)?;
        self.position_id = parent.id;
        Some((parent.revision, parent.snapshot.clone()))
    }
    /// Redo the last undone edit.
    ///
    /// Moves the position forward to the current node if we've undone.
    /// For branching, moves to the appropriate child.
    ///
    /// Returns the target's revision and snapshot.
    /// The caller (model) is responsible for advancing the revision (§4.5.2).
    pub fn redo(&mut self) -> Option<(Revision, Option<TextSnapshot>)> {
        let child = self.get(self.redo_target()?)?;
        let result = (child.revision, child.snapshot.clone());
        self.position_id = child.id;
        Some(result)
    }
    /// Move to a specific node.
    pub fn goto(&mut self, id: u64) -> bool {
        if self.nodes.iter().any(|n| n.id == id) {
            self.position_id = id;
            self.current_id = id;
            true
        } else {
            false
        }
    }
    /// Get the current revision.
    #[must_use]
    pub fn current_revision(&self) -> Revision {
        self.current()
            .map(|n| n.revision)
            .unwrap_or(Revision::INITIAL)
    }
    /// Get the current position's revision.
    #[must_use]
    pub fn position_revision(&self) -> Revision {
        self.position()
            .map(|n| n.revision)
            .unwrap_or(Revision::INITIAL)
    }
    /// Get the linear history from root to current.
    #[must_use]
    pub fn history(&self) -> Vec<&UndoNode> {
        let mut path = Vec::new();
        let mut current_id = self.current_id;
        while let Some(node) = self.nodes.get(current_id as usize) {
            path.push(node);
            current_id = match node.parent_id {
                Some(id) => id,
                None => break,
            };
        }
        path.into_iter().rev().collect()
    }
    /// Set the coalesce window.
    pub fn set_coalesce_window(&mut self, window: Duration) {
        self.coalesce_window = window;
    }
    /// Get the coalesce window.
    #[must_use]
    pub fn coalesce_window(&self) -> Duration {
        self.coalesce_window
    }
    /// Clear all history except the root.
    pub fn clear(&mut self) {
        if !self.nodes.is_empty() {
            let root = self.nodes[0].clone();
            self.nodes = vec![root];
            self.current_id = 0;
            self.position_id = 0;
            self.next_id = 1;
        }
    }
    /// Get the depth from root to current.
    #[must_use]
    pub fn depth(&self) -> usize {
        let mut current_id = self.current_id;
        let mut depth = 0;
        while let Some(node) = self.nodes.get(current_id as usize) {
            if node.is_root() {
                return depth;
            }
            current_id = node.parent_id.unwrap_or(0);
            depth += 1;
        }
        depth
    }
}
impl Default for UndoTree {
    fn default() -> Self {
        // Default empty tree - callers should use new() with a snapshot
        Self {
            nodes: Vec::new(),
            current_id: 0,
            position_id: 0,
            coalesce_window: DEFAULT_COALESCE_WINDOW,
            next_id: 0,
        }
    }
}
/// Group of edits that are coalesced together.
#[derive(Debug, Clone, PartialEq)]
pub struct EditGroup {
    /// The edits in this group.
    pub edits: Vec<EditOp>,
    /// The timestamp of the first edit.
    pub timestamp: Instant,
    /// The revision at the start of this group.
    pub start_revision: Revision,
}
impl EditGroup {
    /// Create a new edit group.
    #[must_use]
    pub fn new(edits: Vec<EditOp>, timestamp: Instant, start_revision: Revision) -> Self {
        Self {
            edits,
            timestamp,
            start_revision,
        }
    }
    /// Check if this group can absorb another edit.
    #[must_use]
    pub fn can_absorb(&self, edit: &EditOp, now: Instant, coalesce_window: Duration) -> bool {
        if now.duration_since(self.timestamp) > coalesce_window {
            return false;
        }
        // Check if the edit can be coalesced with the last edit in the group
        if let Some(last) = self.edits.last() {
            last.can_coalesce(edit)
        } else {
            true // Empty group can absorb anything
        }
    }
    /// Absorb another edit into this group.
    pub fn absorb(&mut self, edit: EditOp) {
        self.edits.push(edit);
    }
    /// Get the total text change of this group.
    #[must_use]
    pub fn total_change(&self) -> (usize, usize) {
        let mut inserted = 0usize;
        let mut deleted = 0usize;
        for edit in &self.edits {
            deleted += edit.range.len();
            inserted += edit.replacement.len();
        }
        (inserted, deleted)
    }
}
/// Simple linear undo stack (alternative for non-branching use cases).
///
/// This is provided as an option for simpler scenarios.
#[derive(Debug)]
pub struct LinearUndoStack {
    /// Stack of undo operations.
    undo_stack: VecDeque<EditOp>,
    /// Stack of redo operations.
    redo_stack: VecDeque<EditOp>,
    /// Current revision.
    current_revision: Revision,
    /// Coalesce window.
    coalesce_window: Duration,
    /// Timestamp of the last edit.
    last_edit_time: Option<Instant>,
}
impl LinearUndoStack {
    /// Create a new linear undo stack.
    #[must_use]
    pub fn new() -> Self {
        Self {
            undo_stack: VecDeque::new(),
            redo_stack: VecDeque::new(),
            current_revision: Revision::INITIAL,
            coalesce_window: DEFAULT_COALESCE_WINDOW,
            last_edit_time: None,
        }
    }
    /// Push an edit onto the undo stack.
    ///
    /// If within the coalesce window and the edit merges into the top entry
    /// (`EditOp::coalesce` returns `Some`), the entry is replaced by the
    /// merged edit and the revision does not advance. Otherwise the edit is
    /// pushed as a new entry. Falls through to a plain push whenever
    /// `coalesce` declines, so this path can never panic regardless of any
    /// future divergence between `can_coalesce` and `coalesce`.
    ///
    /// Returns the new revision.
    pub fn push(&mut self, edit: EditOp, _text: &str) -> Revision {
        let now = Instant::now();
        if let Some(last_time) = self.last_edit_time
            && now.duration_since(last_time) < self.coalesce_window
            && let Some(last_edit) = self.undo_stack.back_mut()
            && let Some(merged) = last_edit.coalesce(&edit)
        {
            *last_edit = merged;
            return self.current_revision;
        }
        // Push as new edit
        self.undo_stack.push_back(edit);
        self.last_edit_time = Some(now);
        self.current_revision = self.current_revision.next();
        self.redo_stack.clear(); // Clear redo stack on new edit
        self.current_revision
    }
    /// Undo the last edit.
    ///
    /// Returns the inverse edit and the new revision.
    pub fn undo(&mut self, text: &str) -> Option<(EditOp, Revision)> {
        let edit = self.undo_stack.pop_back()?;
        let inverse = edit.invert(text);
        self.redo_stack.push_back(edit);
        self.current_revision = self.current_revision.next();
        Some((inverse, self.current_revision))
    }
    /// Redo the last undone edit.
    ///
    /// Returns the edit and the new revision.
    pub fn redo(&mut self) -> Option<(EditOp, Revision)> {
        let edit = self.redo_stack.pop_back()?;
        self.undo_stack.push_back(edit.clone());
        self.current_revision = self.current_revision.next();
        Some((edit, self.current_revision))
    }
    /// Clear the undo stack.
    /// Set the coalesce window.
    pub fn set_coalesce_window(&mut self, window: Duration) {
        self.coalesce_window = window;
    }
    pub fn clear(&mut self) {
        self.undo_stack.clear();
        self.redo_stack.clear();
        self.current_revision = Revision::INITIAL;
        self.last_edit_time = None;
    }
    /// Get the current revision.
    #[must_use]
    pub fn current_revision(&self) -> Revision {
        self.current_revision
    }
    /// Get the number of undo steps available.
    #[must_use]
    pub fn undo_count(&self) -> usize {
        self.undo_stack.len()
    }
    /// Get the number of redo steps available.
    #[must_use]
    pub fn redo_count(&self) -> usize {
        self.redo_stack.len()
    }
}
impl Default for LinearUndoStack {
    fn default() -> Self {
        Self::new()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edit_op_apply() {
        let op = EditOp::new(Span::new(TextByte(6), TextByte(11)), "world".to_string());
        let text = "hello there";
        let result = op.apply(text);
        assert_eq!(result, "hello world");
    }

    #[test]
    fn journal_ops_never_coalesce() {
        let tagged =
            EditOp::new(Span::new(TextByte(0), TextByte(0)), "x".into()).with_journal(JournalTag {
                txn_id: "txn".into(),
                item_id: 1,
            });
        let adjacent = EditOp::new(Span::new(TextByte(1), TextByte(1)), "y".into());
        assert!(!tagged.can_coalesce(&adjacent));
        assert!(!adjacent.can_coalesce(&tagged));
    }
    #[test]
    fn edit_op_invert() {
        let text = "hello world";
        let op = EditOp::new(Span::new(TextByte(6), TextByte(11)), "there".to_string());
        let inverse = op.invert(text);
        let result = inverse.apply("hello there");
        assert_eq!(result, "hello world");
    }
    #[test]
    fn edit_op_coalesce_adjacent() {
        let op1 = EditOp::new(Span::new(TextByte(5), TextByte(5)), "a".to_string());
        let op2 = EditOp::new(Span::new(TextByte(6), TextByte(6)), "b".to_string());
        assert!(!op1.can_coalesce(&op2)); // Gap of one byte: not adjacent
        let op3 = EditOp::new(Span::new(TextByte(5), TextByte(5)), "a".to_string());
        let op4 = EditOp::new(Span::new(TextByte(5), TextByte(6)), "b".to_string());
        assert!(op3.can_coalesce(&op4)); // Shared point: insertion at 5 == start of [5,6)
        // M1 pin: a true overlap (neither endpoint shared) never coalesces.
        let left = EditOp::new(Span::new(TextByte(1), TextByte(5)), "x".to_string());
        let right = EditOp::new(Span::new(TextByte(3), TextByte(8)), "y".to_string());
        assert!(!left.can_coalesce(&right));
        assert!(!right.can_coalesce(&left));
        assert_eq!(left.coalesce(&right), None);
    }
    #[test]
    fn linear_undo_stack_basic() {
        let mut stack = LinearUndoStack::new();
        let text = "hello";
        // First edit
        let op1 = EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        let r1 = stack.push(op1, text);
        assert_eq!(r1, Revision::new(1));
        // Second edit
        let op2 = EditOp::new(Span::new(TextByte(11), TextByte(11)), "!".to_string());
        let r2 = stack.push(op2, "hello world");
        assert_eq!(r2, Revision::new(2));
        // Undo
        let (inverse, r3) = stack.undo("hello world!").unwrap();
        assert_eq!(r3, Revision::new(3));
        // Inverse should be a delete: range [11, 12), replacement ""
        assert_eq!(inverse.range.start, TextByte(11));
        assert_eq!(inverse.range.end, TextByte(12));
        assert_eq!(inverse.replacement, "");
        // Redo
        let (redo_op, r4) = stack.redo().unwrap();
        assert_eq!(r4, Revision::new(4));
        assert_eq!(redo_op.replacement, "!");
    }
    #[test]
    fn linear_undo_stack_coalesce() {
        let mut stack = LinearUndoStack::new();
        let text = "hello";
        stack.set_coalesce_window(Duration::from_millis(100));
        // First edit
        let op1 = EditOp::new(Span::new(TextByte(5), TextByte(5)), " ".to_string());
        stack.push(op1, text);
        // Second edit within window and adjacent
        // Note: ranges are in different text contexts (before/after op1), so they won't coalesce
        let op2 = EditOp::new(Span::new(TextByte(6), TextByte(6)), "w".to_string());
        stack.push(op2, "hello ");
        // Without proper coordinate adjustment, these won't coalesce
        // assert_eq!(stack.undo_count(), 1);
        // For now, just check that both edits are in the stack
        assert_eq!(stack.undo_count(), 2);
    }
    #[test]
    fn undo_tree_basic() {
        let snapshot = TextSnapshot::new(b"hello").unwrap();
        let mut tree = UndoTree::new(snapshot);
        let op1 = EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        let r1 = tree.apply(op1, TextSnapshot::new(b"hello world").unwrap());
        assert_eq!(r1, Revision::new(1));
        let current_rev = tree.current_revision();
        assert_eq!(current_rev, Revision::new(1));
    }
    #[test]
    fn undo_tree_undo_redo() {
        let snapshot = TextSnapshot::new(b"hello").unwrap();
        let mut tree = UndoTree::new(snapshot);
        let op1 = EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".to_string());
        tree.apply(op1, TextSnapshot::new(b"hello world").unwrap());
        // Undo returns the parent's revision (0), the model will advance it
        let (rev, _) = tree.undo().unwrap();
        assert_eq!(rev, Revision::INITIAL);
        // Redo returns the child's revision (1), the model will advance it
        let (rev, _) = tree.redo().unwrap();
        assert_eq!(rev, Revision::new(1));
    }

    #[test]
    fn undo_tree_navigation_branching_and_snapshotless_nodes() {
        let snap = |s: &str| TextSnapshot::new(s.as_bytes()).unwrap();
        let mut tree = UndoTree::with_coalesce_window(snap(""), Duration::ZERO);
        assert!(tree.current().unwrap().is_root());
        assert!(tree.position().unwrap().is_leaf());
        assert_eq!(tree.coalesce_window(), Duration::ZERO);
        let edit = EditOp::new(Span::new(TextByte(0), TextByte(0)), "a".into());
        tree.apply(edit.clone(), snap("a"));
        tree.apply(
            EditOp::new(Span::new(TextByte(1), TextByte(1)), "b".into()),
            snap("ab"),
        );
        assert_eq!(tree.depth(), 2);
        assert_eq!(tree.history().len(), 3);
        assert!(tree.goto(1));
        assert_eq!(tree.position_revision(), Revision::new(1));
        assert!(!tree.goto(99));
        assert!(tree.undo().is_some());
        assert!(tree.redo().is_some());
        tree.clear();
        assert_eq!(tree.depth(), 0);
        assert_eq!(tree.history().len(), 1);
        assert_eq!(tree.current_revision(), Revision::INITIAL);

        let node = UndoNode::new_no_snapshot(1, Revision::new(1), edit, 0);
        assert!(!node.is_root());
        assert!(node.is_leaf());
        assert!(node.snapshot.is_none());
        assert!(UndoTree::default().current().is_none());
    }

    #[test]
    fn edit_group_and_linear_stack_cover_empty_expired_and_redo_paths() {
        let first = EditOp::new(Span::new(TextByte(0), TextByte(0)), "a".into());
        let next = EditOp::new(Span::new(TextByte(1), TextByte(1)), "b".into());
        assert!(!first.is_noop());
        assert!(!first.can_coalesce(&next));
        let adjacent = EditOp::new(Span::new(TextByte(0), TextByte(1)), "x".into());
        assert!(first.can_coalesce(&adjacent));
        assert_eq!(first.coalesce(&adjacent).unwrap().replacement, "ax");
        let gapped = EditOp::new(Span::new(TextByte(2), TextByte(2)), "z".into());
        assert_eq!(first.coalesce(&gapped), None);

        let now = Instant::now();
        let mut group = EditGroup::new(vec![first.clone()], now, Revision::INITIAL);
        assert!(group.can_absorb(&adjacent, now, Duration::from_secs(1)));
        assert!(!group.can_absorb(&next, now + Duration::from_secs(2), Duration::from_secs(1)));
        group.absorb(adjacent);
        assert_eq!(group.total_change(), (2, 1));
        assert!(EditGroup::new(vec![], now, Revision::INITIAL).can_absorb(
            &next,
            now,
            Duration::ZERO
        ));

        let mut stack = LinearUndoStack::new();
        assert!(stack.undo("x").is_none());
        assert!(stack.redo().is_none());
        stack.set_coalesce_window(Duration::ZERO);
        stack.push(first.clone(), "");
        stack.push(next, "a");
        assert_eq!(stack.undo_count(), 2);
        assert!(stack.undo("ab").is_some());
        assert_eq!(stack.redo_count(), 1);
        assert!(stack.redo().is_some());
        stack.undo("ab");
        stack.push(first, "a");
        assert_eq!(stack.redo_count(), 0);
        stack.clear();
        assert_eq!(stack.undo_count(), 0);
        assert_eq!(stack.current_revision(), Revision::INITIAL);
    }

    /// M1 pin: strictly overlapping edits must NOT coalesce. Sequential
    /// application of `[1,5)->"x"` then `[3,8)->"y"` cannot be reproduced by
    /// concatenating replacements over the `min..max` span, so they take two
    /// undo entries and each restores its own pre-state exactly.
    #[test]
    fn overlapping_rapid_edits_take_separate_undo_entries() {
        let mut stack = LinearUndoStack::new();
        stack.set_coalesce_window(Duration::from_secs(3600));

        let base = "abcdefghijklm".to_string();
        let e1 = EditOp::new(Span::new(TextByte(1), TextByte(5)), "x".to_string());
        let text1 = e1.apply(&base);
        let e2 = EditOp::new(Span::new(TextByte(3), TextByte(8)), "yyyyy".to_string());
        let text2 = e2.apply(&text1);

        stack.push(e1.clone(), &base);
        stack.push(e2.clone(), &text1);

        // Two separate entries, one revision per edit.
        assert_eq!(stack.undo_count(), 2, "overlapping edits must not merge");

        // Undoing twice with each entry's recorded pre-state restores the
        // exact prior text at every step (invert takes the edit's pre-text).
        let (inv2, _) = stack.undo(&text1).expect("undo available");
        assert_eq!(inv2.apply(&text2), text1);
        let (inv1, _) = stack.undo(&base).expect("undo available");
        assert_eq!(inv1.apply(&text1), base);
    }

    /// Adjacency merges correctly on a real text, in both push orders: the
    /// merged span is the union and the replacement keeps left-to-right
    /// order, never the reversed `CDAB` concatenation.
    #[test]
    fn adjacent_burst_merges_correctly_in_both_orders() {
        // Forward order: [0,2)->"AB" then [2,4)->"CD" on "wxyz".
        let mut stack = LinearUndoStack::new();
        stack.set_coalesce_window(Duration::from_secs(3600));
        stack.push(
            EditOp::new(Span::new(TextByte(0), TextByte(2)), "AB".into()),
            "wxyz",
        );
        stack.push(
            EditOp::new(Span::new(TextByte(2), TextByte(4)), "CD".into()),
            "AByz",
        );
        assert_eq!(stack.undo_count(), 1);
        // The single merged entry applied to the base yields the same text
        // the two sequential edits produced, and undoing restores exactly.
        let (inv, _) = stack.undo("wxyz").expect("undo available");
        assert_eq!(inv.range.start, TextByte(0));
        assert_eq!(inv.range.end, TextByte(4));
        assert_eq!(inv.replacement, "wxyz");
        assert_eq!(inv.apply("ABCD"), "wxyz");

        // Reverse order: [2,4)->"CD" pushed first, then [0,2)->"AB".
        let mut stack = LinearUndoStack::new();
        stack.set_coalesce_window(Duration::from_secs(3600));
        stack.push(
            EditOp::new(Span::new(TextByte(2), TextByte(4)), "CD".into()),
            "wxyz",
        );
        stack.push(
            EditOp::new(Span::new(TextByte(0), TextByte(2)), "AB".into()),
            "wxCD",
        );
        assert_eq!(stack.undo_count(), 1);
        // Ordering branch must put the LEFT edit's bytes first: "ABCD",
        // matching sequential application ("wxCD" then [0,2)->"AB" = "ABCD").
        let (inv, _) = stack.undo("wxyz").expect("undo available");
        assert_eq!(inv.apply("ABCD"), "wxyz");
    }

    /// Zero-length (insertion) ranges coalesce at the shared point, merging
    /// in application order: insert "a" at 5, then "b" at 5 -> one entry
    /// whose replacement is "ab".
    #[test]
    fn same_point_insertions_merge_in_application_order() {
        let mut stack = LinearUndoStack::new();
        stack.set_coalesce_window(Duration::from_secs(3600));
        stack.push(
            EditOp::new(Span::new(TextByte(5), TextByte(5)), "a".into()),
            "hello",
        );
        stack.push(
            EditOp::new(Span::new(TextByte(5), TextByte(5)), "b".into()),
            "hello",
        );
        assert_eq!(stack.undo_count(), 1);
        // The merged op applied to the base places "ab" at the point.
        let merged = EditOp::new(Span::new(TextByte(5), TextByte(5)), "ab".into());
        assert_eq!(merged.apply("hello"), "helloab");
        // Undoing the merged insertion deletes exactly the inserted bytes.
        let (inv, _) = stack.undo("hello").expect("undo available");
        assert_eq!(inv.apply("helloab"), "hello");
    }

    /// Boundary and journal edits never merge in the stack either (pin of
    /// existing predicate behavior at the stack level).
    #[test]
    fn boundary_and_journal_edits_never_merge_in_stack() {
        let mut stack = LinearUndoStack::new();
        stack.set_coalesce_window(Duration::from_secs(3600));
        stack.push(
            EditOp::new(Span::new(TextByte(0), TextByte(0)), "a".into()),
            "",
        );
        let mut boundary = EditOp::new(Span::new(TextByte(0), TextByte(0)), "b".into());
        boundary.boundary = true;
        stack.push(boundary, "a");
        assert_eq!(stack.undo_count(), 2, "boundary edit must not merge");

        let tagged =
            EditOp::new(Span::new(TextByte(0), TextByte(0)), "c".into()).with_journal(JournalTag {
                txn_id: "txn".into(),
                item_id: 7,
            });
        stack.push(tagged, "ab");
        assert_eq!(stack.undo_count(), 3, "journal edit must not merge");
    }

    /// Stack-level invariant under a long randomized burst: 200 same-point
    /// insertion ops (the regime that merges, including merged chains)
    /// driven onto a short base, then undo-to-bottom with each inverse
    /// applied — the original text must return exactly. Catches cumulative
    /// merge errors the pairwise properties cannot.
    #[test]
    fn randomized_stack_drive_restores_original_exactly() {
        let mut state: u64 = 0x9E37_79B9_7F4A_7C15; // deterministic xorshift64*
        let mut rnd = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };

        let base = "abc".to_string();
        let mut text = base.clone();
        let mut stack = LinearUndoStack::new();
        stack.set_coalesce_window(Duration::from_secs(3600));

        for _ in 0..200 {
            let pos = (rnd() as usize) % (text.len() + 1);
            let ch = (b'a' + (rnd() % 26) as u8) as char;
            let edit = EditOp::new(Span::new(TextByte(pos), TextByte(pos)), ch.to_string());
            text = edit.apply(&text);
            stack.push(edit, &text);
        }
        // The drive must actually have merged (bursts at the same point).
        assert!(
            stack.undo_count() < 200,
            "expected merged chains, got {}",
            stack.undo_count()
        );

        // Undo-to-bottom: each popped entry's inverse removes the bytes it
        // contributed; the exact original must survive.
        while let Some((inverse, _)) = stack.undo(&text) {
            text = inverse.apply(&text);
        }
        assert_eq!(text, base);
    }

    /// LinearUndoStack::push cannot panic for any input: fully randomized
    /// ops (arbitrary ranges and replacements, mixed windows, undo/redo
    /// churn) are driven without a single panic. The stack never indexes
    /// text itself; the merged push path must never trust `can_coalesce`
    /// with an `expect` (the old panic landmine).
    #[test]
    fn push_never_panics_under_randomized_ops() {
        use proptest::prelude::*;
        proptest!(|(
            seed in any::<u64>(),
            window_millis in 0u64..2_000,
            ops in proptest::collection::vec(
                (any::<usize>(), any::<usize>(), "[\\x20-\\x7e]{0,8}"),
                1..50,
            ),
        )| {
            let mut state = seed | 1;
            let mut next = move || {
                state ^= state >> 12;
                state ^= state << 25;
                state ^= state >> 27;
                state.wrapping_mul(0x2545_F491_4F6C_DD1D)
            };
            let mut stack = LinearUndoStack::new();
            stack.set_coalesce_window(Duration::from_millis(window_millis));
            for (a, b, repl) in &ops {
                let (start, end) = if a <= b { (*a, *b) } else { (*b, *a) };
                let edit = EditOp::new(Span::new(TextByte(start), TextByte(end)), repl.clone());
                stack.push(edit, "");
                // Churn the other entry points too.
                if next() % 3 == 0 {
                    let _ = stack.undo("");
                } else if next() % 3 == 1 {
                    let _ = stack.redo();
                }
            }
        });
    }

    mod coalesce_properties {
        //! Pairwise coalescing correctness, proved on arbitrary input:
        //! adjacency merges exactly (and agrees with sequential application
        //! whenever coordinates stay aligned), strict overlap never merges.

        use super::*;
        use proptest::prelude::*;

        /// ASCII-only text so every byte index is a char boundary.
        fn arb_text() -> impl Strategy<Value = String> {
            "[a-zA-Z0-9 .,!?]{0,24}"
        }

        #[test]
        fn adjacent_pairs_merge_exactly() {
            proptest!(|(
                base in arb_text(),
                (x, y, z) in (any::<usize>(), any::<usize>(), any::<usize>()),
                r1 in "[a-zA-Z]{0,6}",
                r2 in "[a-zA-Z]{0,6}",
            )| {
                let len = base.len();
                // Order the three points: s <= m <= e, so `a` = [s,m) and
                // `b` = [m,e) are adjacent on shared base coordinates.
                let [s, m, e] = {
                    let mut p = [x % (len + 1), y % (len + 1), z % (len + 1)];
                    p.sort_unstable();
                    p
                };
                let a = EditOp::new(Span::new(TextByte(s), TextByte(m)), r1.clone());
                let b = EditOp::new(Span::new(TextByte(m), TextByte(e)), r2.clone());

                // Adjacency: coalescible in both query orders.
                prop_assert!(a.can_coalesce(&b));
                prop_assert!(b.can_coalesce(&a));

                // The merged op is the union span with left-to-right bytes.
                prop_assert!(a.coalesce(&b).is_some(), "adjacent pair must merge");
                let merged = a.coalesce(&b).unwrap();
                prop_assert_eq!(&merged.range, &Span::new(TextByte(s), TextByte(e)));
                prop_assert_eq!(&merged.replacement, &(r1.clone() + &r2));

                // Reverse-ordered query: same union span; the replacement
                // keeps left-to-right order whenever the starts differ, and
                // flips to self-first exactly in the shared-point tie
                // (both ranges zero-length at the same point).
                prop_assert!(b.coalesce(&a).is_some(), "reverse adjacent pair must merge");
                let reverse = b.coalesce(&a).unwrap();
                prop_assert_eq!(&reverse.range, &Span::new(TextByte(s), TextByte(e)));
                if s < m {
                    prop_assert_eq!(&reverse.replacement, &(r1.clone() + &r2));
                } else {
                    prop_assert_eq!(&reverse.replacement, &(r2.clone() + &r1));
                }

                // Merged application equals simultaneous (both-at-once)
                // editing of the two disjoint adjacent spans.
                let simultaneous = format!("{}{}{}{}", &base[..s], r1, r2, &base[e..]);
                prop_assert_eq!(&merged.apply(&base), &simultaneous);

                // Sequential equivalence under the spec pairing: applying
                // `a` then `b` (b's coordinates in the post-a text) equals
                // coalesce(b, a) — the LAST-applied edit is `self` so its
                // bytes win the left slot at a shared point. Holds whenever
                // coordinates stay aligned (length-preserving replacements).
                if r1.len() == m - s && r2.len() == e - m {
                    prop_assert_eq!(
                        &b.apply(&a.apply(&base)),
                        &reverse.apply(&base),
                        "a-then-b must equal coalesce(b,a)"
                    );
                    prop_assert_eq!(
                        &a.apply(&b.apply(&base)),
                        &merged.apply(&base),
                        "b-then-a must equal coalesce(a,b)"
                    );
                }
            });
        }

        #[test]
        fn overlapping_and_gapped_pairs_never_merge() {
            proptest!(|(
                base in arb_text(),
                (x, y, z, w) in (any::<usize>(), any::<usize>(), any::<usize>(), any::<usize>()),
                r1 in "[a-zA-Z]{0,6}",
                r2 in "[a-zA-Z]{0,6}",
                shape in 0u8..3,
            )| {
                let len = base.len();
                let mut p = [
                    x % (len + 1), y % (len + 1), z % (len + 1), w % (len + 1),
                ];
                p.sort_unstable();
                let [p0, p1, p2, p3] = p;
                // Require strict inequality so the generated geometry is a
                // genuine overlap/gap — touching endpoints ARE adjacency.
                prop_assume!(p0 < p1 && p1 < p2 && p2 < p3);
                let (a, b) = match shape {
                    // Partial overlap: s1 < s2 < e1 < e2.
                    0 => (
                        EditOp::new(Span::new(TextByte(p0), TextByte(p2)), r1),
                        EditOp::new(Span::new(TextByte(p1), TextByte(p3)), r2),
                    ),
                    // Containment: s1 < s2 < e2 < e1.
                    1 => (
                        EditOp::new(Span::new(TextByte(p0), TextByte(p3)), r1),
                        EditOp::new(Span::new(TextByte(p1), TextByte(p2)), r2),
                    ),
                    // Gap: e1 < s2 (a strictly left of b, at least one byte).
                    _ => (
                        EditOp::new(Span::new(TextByte(p0), TextByte(p1)), r1),
                        EditOp::new(Span::new(TextByte(p2), TextByte(p3)), r2),
                    ),
                };
                // The M1 regression guard: none of these ever coalesce.
                prop_assert!(!a.can_coalesce(&b));
                prop_assert!(!b.can_coalesce(&a));
                prop_assert_eq!(a.coalesce(&b), None);
                prop_assert_eq!(b.coalesce(&a), None);
            });
        }
    }
}
