//! Undo tree with time-based coalescing (§4.14, D14).
//!
//! The undo tree stores edit operations and their inverses, supporting:
//! - Linear undo/redo
//! - Branching on undo-after-edit
//! - Time-based coalescing (default 500ms window)
//!
//! Every edit, undo, redo, reload, and journal patch advances the revision
//! even if text returns to previous bytes (§4.5.2).
use crate::history::{
    EditKind, EditorEdit, HistoryCursor, HistoryEntry, HistoryError, HistoryPage, HistoryTarget,
    PAGE_LIMIT, TRAIL_LIMIT,
};
use crate::revision::Revision;
use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use std::time::{Duration, Instant};
/// Default time window for coalescing consecutive edits into a single undo group.
///
/// Edits within this window of the previous edit are coalesced.
pub const DEFAULT_COALESCE_WINDOW: Duration = Duration::from_millis(500);

/// Undo-unit policy for a batch application (RFC-0006 D-06/TD-8).
///
/// `Isolated` installs the batch as one boundary node that can never merge
/// with neighboring typing (Replace next/all, structure edits, history
/// restore). `Typing` is the ordinary captured-input path and keeps the
/// existing coalescing rules. The default is `Isolated`: clients cannot
/// silently bypass boundary isolation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UndoPolicy {
    /// One isolated boundary — never coalesces with adjacent typing.
    #[default]
    Isolated,
    /// Ordinary typing — existing coalescing rules apply.
    Typing,
    /// Explicit Context Mode typing, closed by an interaction boundary.
    Context { origin: u128 },
}

/// An edit operation over the normalized text.
#[derive(Debug, Clone, PartialEq)]
pub struct EditOp {
    /// The range to replace in TextByte coordinates.
    pub range: Span<TextByte>,
    /// The replacement text (normalized, CRLF folded).
    pub replacement: String,
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
    /// does not rely on this predicate for text correctness; `coalesce`
    /// does, which is why adjacency is the whole contract.)
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
/// A node in the undo tree.
///
/// Each node represents a state of the buffer and can have:
/// - A parent (for linear history)
/// - Children (for branching on undo-after-edit)
#[derive(Debug, Clone)]
pub struct UndoNode {
    segment: Option<u64>,
    pub version: u64,
    pub editor: Option<EditorEdit>,
    pub kind: EditKind,
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
            segment: None,
            version: 1,
            editor: None,
            kind: EditKind::Edit,
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
            segment: edit.journal.as_ref().map(|_| id),
            version: 1,
            editor: None,
            kind: EditKind::Edit,
            id,
            revision,
            edit: Some(edit),
            snapshot: Some(snapshot),
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
    generation: u64,
    /// A continuous run can only coalesce edits from its own editor view.
    context_origin: Option<u128>,
    /// All nodes in the tree.
    nodes: Vec<UndoNode>,
    /// Preferred redo continuation; edits always start at `position_id`.
    preferred_tip: u64,
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
            generation: 1,
            context_origin: None,
            nodes: vec![root],
            preferred_tip: 0,
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
    /// Get the preferred redo tip.
    #[must_use]
    pub fn current(&self) -> Option<&UndoNode> {
        self.nodes.get(self.preferred_tip as usize)
    }
    /// Get the preferred redo tip mutably.
    #[must_use]
    pub fn current_mut(&mut self) -> Option<&mut UndoNode> {
        self.nodes.get_mut(self.preferred_tip as usize)
    }
    /// Get a node by ID.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&UndoNode> {
        self.nodes.get(usize::try_from(id).ok()?)
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
        let mut child = self.preferred_tip;
        while let Some(node) = self.get(child) {
            if node.parent_id == Some(self.position_id) {
                return Some(child);
            }
            let Some(parent) = node.parent_id else {
                break;
            };
            child = parent;
        }
        self.position()?.children.last().copied()
    }

    /// Apply an edit and advance the tree.
    ///
    /// If the edit can be coalesced with the current node's edit, it is merged.
    /// Otherwise, a new node is created.
    ///
    /// Returns the new revision.
    pub fn apply(&mut self, edit: EditOp, snapshot: TextSnapshot) -> Revision {
        if self.context_origin.is_some() || self.position().is_some_and(|n| n.editor.is_some()) {
            self.seal_current();
        }
        self.apply_with_coalescing(edit, snapshot, true, false)
    }

    /// One explicit typing run, independent of the ordinary idle timer.
    pub fn apply_context(
        &mut self,
        edit: EditOp,
        snapshot: TextSnapshot,
        origin: u128,
    ) -> Revision {
        // Unannotated callers cannot extend a recorded editor operation: its
        // after-selection would then describe a state that no longer exists.
        if self.position().is_some_and(|node| node.editor.is_some()) {
            self.seal_current();
        }
        self.apply_context_inner(edit, snapshot, origin)
    }

    fn apply_context_inner(
        &mut self,
        edit: EditOp,
        snapshot: TextSnapshot,
        origin: u128,
    ) -> Revision {
        if self.context_origin != Some(origin) {
            self.seal_current();
        }
        self.context_origin = Some(origin);
        self.apply_with_coalescing(edit, snapshot, true, true)
    }

    /// Apply a distinct operation that must start a new undo-tree node.
    /// Destructive conflict resolutions use this so a merge cannot coalesce
    /// into nearby typing and lose its independent undo boundary.
    pub fn apply_boundary(&mut self, mut edit: EditOp, snapshot: TextSnapshot) -> Revision {
        edit.boundary = true;
        self.context_origin = None;
        self.apply_with_coalescing(edit, snapshot, false, false)
    }

    fn apply_with_coalescing(
        &mut self,
        edit: EditOp,
        snapshot: TextSnapshot,
        allow_coalescing: bool,
        continuous: bool,
    ) -> Revision {
        self.generation += 1;
        let branched = self.position_id != self.preferred_tip;
        let position_id = self.position_id;
        // First, check if we should coalesce
        let should_coalesce = allow_coalescing
            && !branched
            && if let Some(current) = self.nodes.get(position_id as usize) {
                if let Some(current_edit) = &current.edit {
                    !current_edit.boundary
                        && !edit.boundary
                        && (continuous
                            || (current_edit.can_coalesce(&edit)
                                && current.timestamp.elapsed() < self.coalesce_window))
                } else {
                    false
                }
            } else {
                false
            };
        if should_coalesce {
            let parent_text = self
                .get(position_id)
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
                if let Some(node) = self.nodes.get_mut(position_id as usize) {
                    node.version += 1;
                    node.edit = Some(coalesced);
                    node.snapshot = Some(snapshot);
                    node.timestamp = Instant::now();
                    return node.revision;
                }
            }
        }
        let current_node = self
            .nodes
            .get(position_id as usize)
            .expect("current node must exist");
        let parent_id = current_node.id;
        let new_id = self.next_id;
        self.next_id += 1;
        let new_revision = current_node.revision.next();
        let mut new_node = UndoNode::new(new_id, new_revision, edit, parent_id, snapshot);
        if new_node.segment.is_none() {
            new_node.segment = current_node.segment;
        }
        // Add to parent's children
        if let Some(node) = self.nodes.get_mut(parent_id as usize) {
            node.children.push(new_id);
        }
        // Add the new node
        self.nodes.push(new_node);
        self.preferred_tip = new_id;
        self.position_id = new_id;
        new_revision
    }
    /// Preserve a save/semantic boundary without adding a document edit.
    pub fn seal_current(&mut self) {
        self.context_origin = None;
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
        self.seal_current();
        let current = self.position()?;
        if current.is_root() {
            return None;
        }
        let parent_id = current.parent_id?;
        let parent = self.nodes.get(parent_id as usize)?;
        let snapshot = parent.snapshot.clone()?;
        self.position_id = parent.id;
        self.generation += 1;
        Some((parent.revision, Some(snapshot)))
    }
    /// Redo the last undone edit.
    ///
    /// Moves the position forward to the current node if we've undone.
    /// For branching, moves to the appropriate child.
    ///
    /// Returns the target's revision and snapshot.
    /// The caller (model) is responsible for advancing the revision (§4.5.2).
    pub fn redo(&mut self) -> Option<(Revision, Option<TextSnapshot>)> {
        self.seal_current();
        let child = self.get(self.redo_target()?)?;
        let result = (child.revision, Some(child.snapshot.clone()?));
        self.position_id = child.id;
        self.generation += 1;
        Some(result)
    }
    /// Move to a specific node.
    fn goto(&mut self, id: u64) -> bool {
        if self.get(id).is_some() {
            self.seal_current();
            // Keep the current continuation when jumping to its ancestor.
            // Selecting another branch must adopt that branch's continuation,
            // otherwise Undo followed by Redo would return to an unrelated
            // state from the previously preferred branch.
            let mut ancestor = Some(self.preferred_tip);
            while ancestor.is_some_and(|ancestor| ancestor != id) {
                ancestor = ancestor.and_then(|ancestor| self.get(ancestor)?.parent_id);
            }
            if ancestor != Some(id) {
                let mut tail = id;
                while let Some(child) = self.get(tail).and_then(|node| node.children.last()) {
                    tail = *child;
                }
                self.preferred_tip = tail;
            }
            self.position_id = id;
            self.generation += 1;
            true
        } else {
            false
        }
    }

    /// Install an editor edit and its source-free history record together.
    pub fn apply_recorded(
        &mut self,
        edit: EditOp,
        snapshot: TextSnapshot,
        policy: UndoPolicy,
        record: EditorEdit,
    ) -> Revision {
        if self
            .position()
            .and_then(|n| n.editor.as_ref())
            .is_none_or(|previous| previous.origin != record.origin)
        {
            self.seal_current();
        }
        let revision = match policy {
            UndoPolicy::Isolated => self.apply_boundary(edit, snapshot),
            UndoPolicy::Context { origin } => self.apply_context_inner(edit, snapshot, origin),
            UndoPolicy::Typing => {
                if self.context_origin.is_some() {
                    self.seal_current();
                }
                self.apply_with_coalescing(edit, snapshot, true, false)
            }
        };
        let node = &mut self.nodes[self.position_id as usize];
        if let Some(previous) = &mut node.editor {
            previous.after = record.after;
        } else {
            node.kind = record.kind;
            node.editor = Some(record);
        }
        revision
    }

    fn segment(&self, id: u64) -> Option<u64> {
        self.get(id).and_then(|node| node.segment)
    }

    fn entry(
        &self,
        id: u64,
        epoch: crate::history::HistoryEpoch,
        on_path: bool,
    ) -> Option<HistoryEntry> {
        let node = self.get(id)?;
        Some(HistoryEntry {
            on_path,
            selection_count: if node.is_root() {
                Some(1)
            } else {
                node.editor.as_ref().map(|record| record.after.len())
            },
            target: HistoryTarget {
                epoch,
                node: id,
                version: node.version,
            },
            parent: node.parent_id,
            kind: node.kind,
            current: id == self.position_id,
            open: node.edit.as_ref().is_some_and(|edit| !edit.boundary) && id == self.position_id,
            restorable: self.segment(id) == self.segment(self.position_id),
            initial: node.is_root(),
        })
    }

    pub fn history_page(
        &self,
        epoch: crate::history::HistoryEpoch,
        cursor: Option<HistoryCursor>,
        limit: usize,
    ) -> Result<HistoryPage, HistoryError> {
        if cursor.is_some_and(|c| c.epoch != epoch || c.generation != self.generation) {
            return Err(HistoryError::StalePage);
        }
        let before = cursor.map_or(self.next_id, |c| c.before);
        let limit = limit.clamp(1, PAGE_LIMIT);
        let mut ancestors = std::collections::BTreeSet::new();
        let mut ancestor = Some(self.position_id);
        while let Some(node) = ancestor.and_then(|id| self.get(id)) {
            ancestors.insert(node.id);
            ancestor = node.parent_id;
        }
        let mut entries: Vec<_> = self
            .nodes
            .iter()
            .rev()
            .filter(|n| n.id < before)
            .take(limit + 1)
            .filter_map(|n| self.entry(n.id, epoch, ancestors.contains(&n.id)))
            .collect();
        let next = (entries.len() > limit).then(|| HistoryCursor {
            epoch,
            generation: self.generation,
            before: entries[limit - 1].target.node,
        });
        entries.truncate(limit);
        let mut trail = Vec::new();
        let mut id = Some(self.position_id);
        while let Some(node) = id.and_then(|id| self.get(id)) {
            trail.push(
                self.entry(node.id, epoch, true)
                    .expect("existing history node"),
            );
            if trail.len() == TRAIL_LIMIT {
                break;
            }
            id = node.parent_id;
        }
        trail.reverse();
        Ok(HistoryPage {
            epoch,
            generation: self.generation,
            current: self.position_id,
            entries,
            trail,
            redo: self
                .redo_target()
                .and_then(|id| self.entry(id, epoch, false)),
            next,
        })
    }

    /// Prepare before changing the head. Tagged filesystem obligations may
    /// only be crossed by the Session's existing single-step Undo/Redo path.
    pub fn restore_snapshot(&self, target: HistoryTarget) -> Result<TextSnapshot, HistoryError> {
        let node = self.get(target.node).ok_or(HistoryError::Expired)?;
        if node.version != target.version {
            return Err(HistoryError::Expired);
        }
        if self.segment(target.node) != self.segment(self.position_id) {
            return Err(HistoryError::JournalBoundary);
        }
        node.snapshot.clone().ok_or(HistoryError::Expired)
    }

    pub(crate) fn restore_position(&mut self, id: u64) {
        assert!(self.goto(id), "validated history state must exist");
    }

    #[must_use]
    pub fn position_selections(&self) -> Option<Vec<[usize; 2]>> {
        let node = self.position()?;
        if node.is_root() {
            Some(vec![[0, 0]])
        } else {
            node.editor.as_ref().map(|record| record.after.clone())
        }
    }

    #[must_use]
    pub fn undo_selections(&self) -> Option<Vec<[usize; 2]>> {
        self.position()?
            .editor
            .as_ref()
            .map(|record| record.before.clone())
    }
    /// Get the preferred redo tip's revision, not the live buffer revision.
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
    /// Get the path from the root to the preferred redo tip.
    #[must_use]
    pub fn history(&self) -> Vec<&UndoNode> {
        let mut path = Vec::new();
        let mut preferred_tip = self.preferred_tip;
        while let Some(node) = self.nodes.get(preferred_tip as usize) {
            path.push(node);
            preferred_tip = match node.parent_id {
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
        self.context_origin = None;
        if !self.nodes.is_empty() {
            let root = self.nodes[0].clone();
            self.nodes = vec![root];
            self.preferred_tip = 0;
            self.position_id = 0;
            self.next_id = 1;
        }
    }
    /// Get the depth from the root to the preferred redo tip.
    #[must_use]
    pub fn depth(&self) -> usize {
        let mut preferred_tip = self.preferred_tip;
        let mut depth = 0;
        while let Some(node) = self.nodes.get(preferred_tip as usize) {
            if node.is_root() {
                return depth;
            }
            preferred_tip = node.parent_id.unwrap_or(0);
            depth += 1;
        }
        depth
    }
}
impl Default for UndoTree {
    fn default() -> Self {
        Self::new(TextSnapshot::from_normalized(""))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_runs_ignore_idle_but_never_cross_origins_or_explicit_boundaries() {
        let snap = TextSnapshot::from_normalized;
        let edit =
            |at, value: &str| EditOp::new(Span::new(TextByte(at), TextByte(at)), value.into());
        let mut tree = UndoTree::with_coalesce_window(snap("café"), Duration::ZERO);
        tree.apply_context(edit(0, "A"), snap("Acafé"), 1);
        tree.apply_context(edit(6, "雪"), snap("Acafé雪"), 1);
        assert_eq!(
            tree.history().len(),
            2,
            "one explicit run despite idle and distance"
        );
        assert_eq!(
            tree.current().unwrap().edit.as_ref().unwrap().apply("café"),
            "Acafé雪"
        );
        tree.apply_context(edit(9, "B"), snap("Acafé雪B"), 2);
        assert_eq!(
            tree.history().len(),
            3,
            "a different view cannot join the run"
        );
        tree.seal_current();
        tree.seal_current();
        assert_eq!(tree.history().len(), 3, "sealing creates no document edit");
        tree.apply_context(edit(10, "C"), snap("Acafé雪BC"), 2);
        assert_eq!(tree.undo().unwrap().1.unwrap().text(), "Acafé雪B");
        assert_eq!(tree.undo().unwrap().1.unwrap().text(), "Acafé雪");
        assert_eq!(tree.undo().unwrap().1.unwrap().text(), "café");
        assert_eq!(tree.redo().unwrap().1.unwrap().text(), "Acafé雪");
        tree.apply_context(edit(9, "branch"), snap("Acafé雪branch"), 1);
        assert_eq!(tree.undo().unwrap().1.unwrap().text(), "Acafé雪");
    }

    #[test]
    fn ordinary_edits_and_history_navigation_close_context_runs() {
        let snap = TextSnapshot::from_normalized;
        let edit =
            |at, value: &str| EditOp::new(Span::new(TextByte(at), TextByte(at)), value.into());
        let mut tree = UndoTree::new(snap(""));
        tree.apply_context(edit(0, "A"), snap("A"), 1);
        tree.apply(edit(1, "B"), snap("AB"));
        assert_eq!(tree.history().len(), 3);
        tree.apply_context(edit(2, "C"), snap("ABC"), 1);
        let id = tree.current().unwrap().id;
        assert!(tree.goto(id));
        tree.apply_context(edit(3, "D"), snap("ABCD"), 1);
        assert_eq!(tree.undo().unwrap().1.unwrap().text(), "ABC");
        assert_eq!(tree.undo().unwrap().1.unwrap().text(), "AB");
        assert_eq!(tree.undo().unwrap().1.unwrap().text(), "A");
    }

    proptest::proptest! {
        #[test]
        fn context_groups_match_an_independent_snapshot_history(
            operations in proptest::collection::vec((".{0,24}", 0u128..3, proptest::bool::ANY), 1..80)
        ) {
            let mut tree = UndoTree::with_coalesce_window(TextSnapshot::from_normalized(""), Duration::ZERO);
            let mut states = vec![String::new()];
            let mut text = String::new();
            let mut origin = None;
            for (next, view, seal) in operations {
                if seal { tree.seal_current(); origin = None; }
                let op = EditOp::new(Span::new(TextByte(0), TextByte(text.len())), next.clone());
                tree.apply_context(op, TextSnapshot::from_normalized(&next), view);
                if origin == Some(view) { *states.last_mut().unwrap() = next.clone(); }
                else { states.push(next.clone()); }
                origin = Some(view);
                text = next;
                proptest::prop_assert_eq!(tree.depth(), states.len() - 1);
                let current = tree.current().unwrap();
                let parent = tree.get(current.parent_id.unwrap()).unwrap().snapshot.as_ref().unwrap().text();
                proptest::prop_assert_eq!(current.edit.as_ref().unwrap().apply(parent), text.clone());
            }
            for expected in states[..states.len()-1].iter().rev() {
                let snapshot = tree.undo().unwrap().1.unwrap();
                proptest::prop_assert_eq!(snapshot.text(), expected);
            }
            proptest::prop_assert!(tree.undo().is_none());
            for expected in &states[1..] {
                let snapshot = tree.redo().unwrap().1.unwrap();
                proptest::prop_assert_eq!(snapshot.text(), expected);
            }
            proptest::prop_assert!(tree.redo().is_none());
        }
    }
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
    fn undo_tree_navigation_branching_and_defaults() {
        let snap = |s: &str| TextSnapshot::new(s.as_bytes()).unwrap();
        let mut tree = UndoTree::with_coalesce_window(snap(""), Duration::ZERO);
        assert!(tree.current().unwrap().is_root());
        assert!(tree.position().unwrap().is_leaf());
        assert_eq!(tree.coalesce_window(), Duration::ZERO);
        let edit = EditOp::new(Span::new(TextByte(0), TextByte(0)), "a".into());
        tree.apply(edit, snap("a"));
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
        assert!(UndoTree::default().current().unwrap().is_root());
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
