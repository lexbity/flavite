//! Undo tree with time-based coalescing (§4.14, D14).
//!
//! The undo tree stores edit operations and their inverses, supporting:
//! - Linear undo/redo
//! - Branching on undo-after-edit
//! - Time-based coalescing (default 500ms window)
//!
//! Every edit, undo, redo, reload, and journal patch advances the revision
//! even if text returns to previous bytes (§4.5.2).
use crate::budget::{Budget, EvictionReport, LimitExceeded, RESERVATION_LEASE_MS, Reservation};
use crate::history::{
    EditKind, EditorEdit, HistoryCursor, HistoryEntry, HistoryError, HistoryPage, HistoryTarget,
    PAGE_LIMIT, TRAIL_LIMIT,
};
use crate::revision::Revision;
use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use std::time::{Duration, Instant};

pub(crate) fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|time| u64::try_from(time.as_millis()).ok())
        .unwrap_or(0)
}
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
    changed_at_unix_ms: u64,
    segment: Option<u64>,
    pub version: u64,
    pub editor: Option<EditorEdit>,
    pub kind: EditKind,
    /// Unique identifier for this node.
    pub id: u64,
    /// Whether this node was evicted to satisfy a history budget. Evicted
    /// nodes keep their structural links (id/`/children) so surviving
    /// chains stay traversable, but their heavy state (snapshot, editor
    /// record, edit, children) is dropped and they are invisible to
    /// history pages, accounting, undo/redo and restore (FEUX-0002 X01).
    pub evicted: bool,
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
            changed_at_unix_ms: wall_clock_ms(),
            segment: None,
            version: 1,
            editor: None,
            kind: EditKind::Edit,
            id: 0,
            evicted: false,
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
            changed_at_unix_ms: wall_clock_ms(),
            segment: edit.journal.as_ref().map(|_| id),
            version: 1,
            editor: None,
            kind: EditKind::Edit,
            id,
            evicted: false,
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
    /// All nodes in the tree. Evicted nodes remain in place (structural
    /// tombstone) so `id == index` holds and surviving chains traverse.
    nodes: Vec<UndoNode>,
    /// Preferred redo continuation; edits always start at `position_id`.
    preferred_tip: u64,
    /// The node we're currently at in the tree (for undo/redo).
    position_id: u64,
    /// Time window for coalescing edits.
    coalesce_window: Duration,
    /// Next node ID to assign.
    next_id: u64,
    /// Retained-history budget (byte and node ceilings, FEUX-0002 X01).
    budget: Budget,
    /// Total accounted retained bytes (recomputed on every structural
    /// change by walking retained nodes, E-12).
    accounted_bytes: u64,
    /// Issued reservations: byte leases for prospective nodes and restore
    /// target protections. Both expire after the lease window.
    reservations: Vec<Reservation>,
    /// Next reservation id.
    lease_nonce: u64,
    /// Total nodes evicted since the tree (re)started.
    evicted_total: u64,
    /// Smallest surviving node id; ids below it were evicted.
    retained_watermark: u64,
}
impl UndoTree {
    /// Create a new undo tree with a root node.
    #[must_use]
    pub fn new(snapshot: TextSnapshot) -> Self {
        let root = UndoNode::root(Revision::INITIAL, snapshot);
        let mut tree = Self {
            generation: 1,
            context_origin: None,
            nodes: vec![root],
            preferred_tip: 0,
            position_id: 0,
            coalesce_window: DEFAULT_COALESCE_WINDOW,
            next_id: 1,
            budget: Budget::default(),
            accounted_bytes: 0,
            reservations: Vec::new(),
            lease_nonce: 0,
            evicted_total: 0,
            retained_watermark: 0,
        };
        tree.reaccount();
        tree
    }
    /// Create a new undo tree with a custom coalesce window.
    #[must_use]
    pub fn with_coalesce_window(snapshot: TextSnapshot, window: Duration) -> Self {
        let mut tree = Self::new(snapshot);
        tree.coalesce_window = window;
        tree
    }
    /// Create a new undo tree with a custom history budget.
    #[must_use]
    pub fn with_budget(snapshot: TextSnapshot, budget: Budget) -> Self {
        let mut tree = Self::new(snapshot);
        tree.budget = budget;
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
                return (!node.evicted).then_some(child);
            }
            let Some(parent) = node.parent_id else {
                break;
            };
            child = parent;
        }
        self.position()?
            .children
            .iter()
            .rev()
            .copied()
            .find(|id| self.get(*id).is_some_and(|node| !node.evicted))
    }

    /// The ordered movement spans for one transition from `from` to `to`
    /// (both surviving node ids), expressed in the coordinates of the text
    /// before each step. Up-steps (child → parent) contribute the inverse of
    /// the child's edit; down-steps contribute the child's forward edit.
    /// Returns `(spans, truncated)`: `truncated` is true when the transition
    /// crosses an evicted chain link (edits unavailable) or exceeds
    /// [`crate::history::MAX_MOVEMENT_SPANS`] — passive views must clamp
    /// instead of guessing (FEUX-0002 E-13).
    pub fn transition_spans(
        &self,
        from: u64,
        to: u64,
    ) -> (Vec<crate::history::MovementSpan>, bool) {
        use crate::history::MovementSpan;
        // Ancestor set of `to`.
        let mut ancestors = std::collections::HashSet::new();
        let mut node = Some(to);
        while let Some(id) = node {
            ancestors.insert(id);
            node = self.get(id).and_then(|n| n.parent_id);
        }
        // Climb from `from` to the LCA.
        let mut up: Vec<u64> = Vec::new();
        let mut current = from;
        let mut lca = None;
        while let Some(id) = Some(current) {
            if ancestors.contains(&id) {
                lca = Some(id);
                break;
            }
            let Some(parent) = self.get(id).and_then(|n| n.parent_id) else {
                return (Vec::new(), true);
            };
            up.push(id);
            current = parent;
        }
        let Some(lca) = lca else {
            return (Vec::new(), true);
        };
        // Descend from the LCA to `to`.
        let mut down: Vec<u64> = Vec::new();
        let mut current = lca;
        while current != to {
            let Some(child) = self.get(current).and_then(|n| {
                n.children.iter().copied().find(|c| {
                    self.get(*c).is_some_and(|cnode| {
                        !cnode.evicted && {
                            let mut a = Some(*c);
                            while let Some(x) = a {
                                if x == to {
                                    return true;
                                }
                                a = self.get(x).and_then(|n| n.parent_id);
                            }
                            false
                        }
                    })
                })
            }) else {
                return (Vec::new(), true);
            };
            down.push(child);
            current = child;
        }
        let mut spans = Vec::with_capacity(up.len() + down.len());
        for id in up {
            let Some(node) = self.get(id) else {
                return (Vec::new(), true);
            };
            let Some(edit) = &node.edit else {
                return (Vec::new(), true);
            };
            // Inverse: {start, start + replacement_len, end - start}.
            let len = edit.replacement.len();
            spans.push(MovementSpan {
                start: edit.range.start.0,
                end: edit.range.start.0 + len,
                replacement_len: edit.range.end.0 - edit.range.start.0,
            });
        }
        for id in down {
            let Some(node) = self.get(id) else {
                return (Vec::new(), true);
            };
            let Some(edit) = &node.edit else {
                return (Vec::new(), true);
            };
            spans.push(MovementSpan {
                start: edit.range.start.0,
                end: edit.range.end.0,
                replacement_len: edit.replacement.len(),
            });
        }
        if spans.len() > crate::history::MAX_MOVEMENT_SPANS {
            return (Vec::new(), true);
        }
        (spans, false)
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
                {
                    let node = self
                        .nodes
                        .get_mut(position_id as usize)
                        .expect("position node exists");
                    node.changed_at_unix_ms = wall_clock_ms();
                    node.version += 1;
                    node.edit = Some(coalesced);
                    node.snapshot = Some(snapshot);
                    node.timestamp = Instant::now();
                }
                self.reaccount();
                self.auto_evict_if_over_budget(wall_clock_ms());
                return self.nodes[position_id as usize].revision;
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
        self.reaccount();
        self.auto_evict_if_over_budget(wall_clock_ms());
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
    /// Moves the position back to the nearest surviving parent node
    /// (evicted chain links are skipped). Returns the parent's revision and
    /// snapshot. The caller (model) is responsible for advancing the
    /// revision (§4.5.2).
    pub fn undo(&mut self) -> Option<(Revision, Option<TextSnapshot>)> {
        self.seal_current();
        let current = self.position()?;
        current.parent_id?;
        let parent_id = self.nearest_surviving_ancestor(current.id)?;
        let (parent_revision, snapshot) = {
            let parent = self.survivor(parent_id)?;
            (parent.revision, parent.snapshot.clone()?)
        };
        self.position_id = parent_id;
        self.generation += 1;
        Some((parent_revision, Some(snapshot)))
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
        if self.survivor(id).is_some() {
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
                    if self.get(*child).is_some_and(|node| node.evicted) {
                        break;
                    }
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
        self.reaccount();
        self.auto_evict_if_over_budget(wall_clock_ms());
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
        let node = self.survivor(id)?;
        Some(HistoryEntry {
            changed_at_unix_ms: node.changed_at_unix_ms,
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
            // Eviction invalidates cursors with a distinct typed error; any
            // other generation change keeps the delivered stale-page path
            // (FEUX-0002 X01 / E-12).
            return Err(if self.evicted_total > 0 {
                HistoryError::HistoryChanged
            } else {
                HistoryError::StalePage
            });
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
            .filter(|n| n.id < before && !n.evicted)
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
            if node.evicted {
                id = node.parent_id;
                continue;
            }
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
            history_truncated: self.evicted_total > 0 && before > self.retained_watermark,
            retained_watermark: self.retained_watermark,
        })
    }

    /// Prepare before changing the head. Tagged filesystem obligations may
    /// only be crossed by the Session's existing single-step Undo/Redo path.
    pub fn restore_snapshot(&self, target: HistoryTarget) -> Result<TextSnapshot, HistoryError> {
        let node = self.survivor(target.node).ok_or(HistoryError::Expired)?;
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
    pub fn position_selections(&self) -> Option<Vec<[usize; 3]>> {
        let node = self.position()?;
        if node.is_root() {
            Some(vec![[0, 0, 0]])
        } else {
            node.editor.as_ref().map(|record| record.after.clone())
        }
    }

    #[must_use]
    pub fn undo_selections(&self) -> Option<Vec<[usize; 3]>> {
        self.position()?
            .editor
            .as_ref()
            .map(|record| record.before.clone())
    }

    /// The current tree position (node id).
    #[must_use]
    pub fn position_id(&self) -> u64 {
        self.position_id
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
    /// Get the path from the root to the preferred redo tip (evicted chain
    /// links are skipped).
    #[must_use]
    pub fn history(&self) -> Vec<&UndoNode> {
        let mut path = Vec::new();
        let mut preferred_tip = self.preferred_tip;
        while let Some(node) = self.nodes.get(preferred_tip as usize) {
            if node.evicted {
                preferred_tip = match node.parent_id {
                    Some(id) => id,
                    None => break,
                };
                continue;
            }
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
            self.reservations.clear();
            self.lease_nonce = 0;
            self.evicted_total = 0;
            self.retained_watermark = 0;
            self.reaccount();
        }
    }
    /// Get the depth from the root to the preferred redo tip (evicted
    /// chain links do not count).
    #[must_use]
    pub fn depth(&self) -> usize {
        let mut preferred_tip = self.preferred_tip;
        let mut depth = 0;
        while let Some(node) = self.get(preferred_tip) {
            if node.evicted {
                preferred_tip = node.parent_id.unwrap_or(0);
                continue;
            }
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

/// A byte reservation lease returned by [`UndoTree::reserve`]. Released by
/// [`UndoTree::release`] after the install it admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lease {
    id: u64,
}

impl UndoTree {
    /// The active history budget.
    #[must_use]
    pub fn budget(&self) -> Budget {
        self.budget
    }
    /// Replace the history budget and re-account immediately.
    pub fn set_budget(&mut self, budget: Budget) {
        self.budget = budget;
        self.reaccount();
    }
    /// Total accounted retained history bytes.
    #[must_use]
    pub fn accounted_bytes(&self) -> u64 {
        self.accounted_bytes
    }
    /// Current accounted usage.
    #[must_use]
    pub fn account(&self) -> crate::budget::HistoryAccount {
        crate::budget::HistoryAccount {
            bytes: self.accounted_bytes,
            nodes: self.survivor_count() as u64,
        }
    }
    /// Total nodes evicted since the tree started (or was last cleared).
    #[must_use]
    pub fn evicted_total(&self) -> u64 {
        self.evicted_total
    }
    /// Smallest surviving node id; every smaller id was evicted.
    #[must_use]
    pub fn retained_watermark(&self) -> u64 {
        self.retained_watermark
    }
    /// The surviving node with this id, if any.
    #[must_use]
    fn survivor(&self, id: u64) -> Option<&UndoNode> {
        self.get(id).filter(|node| !node.evicted)
    }
    fn survivor_count(&self) -> usize {
        self.nodes.iter().filter(|node| !node.evicted).count()
    }
    /// Number of surviving (retained) nodes.
    #[must_use]
    pub fn retained_node_count(&self) -> usize {
        self.survivor_count()
    }
    /// Ids of all surviving (retained) nodes, ascending. Test/diagnostic
    /// surface for the accounting mirror.
    #[must_use]
    pub fn survivor_ids(&self) -> Vec<u64> {
        self.nodes
            .iter()
            .filter(|node| !node.evicted)
            .map(|node| node.id)
            .collect()
    }
    /// The nearest surviving ancestor of `from` (skips evicted links).
    fn nearest_surviving_ancestor(&self, from: u64) -> Option<u64> {
        let mut id = from;
        loop {
            let parent = self.get(id)?.parent_id?;
            if self.survivor(parent).is_some() {
                return Some(parent);
            }
            id = parent;
        }
    }

    /// Walk retained nodes and recount bytes (unique retained text
    /// allocations + recorded selection sets + fixed metadata).
    fn reaccount(&mut self) {
        let mut bytes = 0u64;
        let mut watermark = u64::MAX;
        for node in &self.nodes {
            if node.evicted {
                continue;
            }
            watermark = watermark.min(node.id);
            let snapshot_len = node.snapshot.as_ref().map_or(0, |s| s.text().len());
            let replacement_len = node.edit.as_ref().map_or(0, |e| e.replacement.len());
            let (before, after) = match &node.editor {
                Some(record) => (&record.before[..], &record.after[..]),
                None => (&[][..], &[][..]),
            };
            bytes += crate::budget::account_node(snapshot_len, replacement_len, before, after);
        }
        self.accounted_bytes = bytes;
        self.retained_watermark = if watermark == u64::MAX {
            self.next_id
        } else {
            watermark
        };
    }

    /// Best-effort eviction when growth exceeded the budget without a
    /// covering reservation (unguarded direct-tree callers). Guarded
    /// facades reserve first, so this is a no-op for them.
    fn auto_evict_if_over_budget(&mut self, now_ms: u64) {
        let pending: u64 = self.reservations.iter().map(|r| r.projected).sum();
        let over = self.accounted_bytes + pending > self.budget.max_bytes
            || self.survivor_count() as u64 > self.budget.max_nodes;
        if over {
            self.evict_to_fit(0, now_ms);
        }
    }

    /// Reserve `projected` bytes for one new node against the budget,
    /// evicting deterministically to make room when needed. Refusal is
    /// complete and precedes any mutation.
    pub fn reserve(&mut self, projected: u64, now_ms: u64) -> Result<Lease, LimitExceeded> {
        self.expire_leases(now_ms);
        let fits = |tree: &Self| {
            let pending: u64 = tree.reservations.iter().map(|r| r.projected).sum();
            tree.accounted_bytes + pending + projected <= tree.budget.max_bytes
                && (tree.survivor_count() as u64) < tree.budget.max_nodes
        };
        if !fits(self) {
            self.evict_to_fit(projected, now_ms);
        }
        if fits(self) {
            self.lease_nonce += 1;
            let id = self.lease_nonce;
            self.reservations.push(Reservation {
                id,
                lease_until_ms: now_ms.saturating_add(RESERVATION_LEASE_MS),
                projected,
                target: None,
            });
            Ok(Lease { id })
        } else {
            Err(LimitExceeded {
                needed: projected,
                budget: self.budget.max_bytes,
            })
        }
    }

    /// Release a byte reservation after its install completed.
    pub fn release(&mut self, lease: &Lease) {
        self.reservations.retain(|r| r.id != lease.id);
    }

    /// Protect `node` from eviction for the lease window (in-flight restore
    /// target). Returns a token to pass to [`UndoTree::unprotect`]; the
    /// lease window protects the target even if the token is never released.
    pub fn protect_token(&mut self, node: u64, now_ms: u64) -> u64 {
        self.expire_leases(now_ms);
        self.lease_nonce += 1;
        let id = self.lease_nonce;
        self.reservations.push(Reservation {
            id,
            lease_until_ms: now_ms.saturating_add(RESERVATION_LEASE_MS),
            projected: 0,
            target: Some(node),
        });
        id
    }

    /// Release a restore-target protection token.
    pub fn unprotect(&mut self, id: u64) {
        self.reservations.retain(|r| r.id != id);
    }

    fn expire_leases(&mut self, now_ms: u64) {
        self.reservations.retain(|r| r.lease_until_ms > now_ms);
    }

    /// Deterministic eviction pass: evict until the budget fits `projected`
    /// (plus pending reservations) or no candidate remains. Order: off-path
    /// tips oldest-first, then off-path branches, then the oldest
    /// checkpointable prefix of the current path; never the current node,
    /// never a protected restore target.
    fn evict_to_fit(&mut self, projected: u64, now_ms: u64) -> EvictionReport {
        self.expire_leases(now_ms);
        let mut report = EvictionReport::default();
        loop {
            let pending: u64 = self.reservations.iter().map(|r| r.projected).sum();
            let fits = self.accounted_bytes + pending + projected <= self.budget.max_bytes
                && (self.survivor_count() as u64) < self.budget.max_nodes;
            if fits {
                break;
            }
            let Some(id) = self.eviction_candidate(now_ms) else {
                break;
            };
            let before = self.accounted_bytes;
            self.remove_node(id);
            self.evicted_total += 1;
            self.generation += 1;
            report.evicted.push(id);
            self.reaccount();
            report.freed_bytes += before.saturating_sub(self.accounted_bytes);
        }
        report
    }

    /// The next eviction candidate, or none. Deterministic: iteration is
    /// derived from stored sequence numbers only.
    fn eviction_candidate(&self, now_ms: u64) -> Option<u64> {
        let protected: std::collections::HashSet<u64> = self
            .reservations
            .iter()
            .filter(|r| r.lease_until_ms > now_ms)
            .filter_map(|r| r.target)
            .chain(std::iter::once(self.position_id))
            .collect();
        let mut on_path = std::collections::HashSet::new();
        let mut id = Some(self.position_id);
        while let Some(node) = id.and_then(|id| self.get(id)) {
            on_path.insert(node.id);
            id = node.parent_id;
        }
        let eligible = |node: &UndoNode| {
            !node.evicted && !on_path.contains(&node.id) && !protected.contains(&node.id)
        };
        // Pass 1: off-path tips (leaves), oldest first.
        if let Some(id) = self
            .nodes
            .iter()
            .filter(|n| eligible(n))
            .filter(|n| n.is_leaf())
            .map(|n| n.id)
            .min()
        {
            return Some(id);
        }
        // Pass 2: off-path subtree roots (parent on path or none), oldest
        // first. Removing the whole subtree never orphans survivors.
        if let Some(id) = self
            .nodes
            .iter()
            .filter(|n| eligible(n))
            .filter(|n| match n.parent_id {
                None => true,
                Some(parent) => on_path.contains(&parent),
            })
            .map(|n| n.id)
            .min()
        {
            return Some(id);
        }
        // Pass 3: oldest checkpointable prefix of the current path — never
        // the current node, never protected.
        self.nodes
            .iter()
            .filter(|n| !n.evicted && !protected.contains(&n.id))
            .filter(|n| on_path.contains(&n.id) && n.id < self.position_id)
            .map(|n| n.id)
            .min()
    }

    /// Evict one node: detach from its parent, re-parent surviving children
    /// onto its parent (keeping surviving chains connected), and drop its
    /// heavy state. The tombstone stays in place so id-index stability and
    /// structural traversal survive.
    fn remove_node(&mut self, id: u64) {
        let Some(node) = self.get(id) else {
            return;
        };
        let parent_id = node.parent_id;
        if let Some(parent) = parent_id
            && let Some(node) = self.nodes.get_mut(parent as usize)
        {
            node.children.retain(|&child| child != id);
        }
        let children = self.nodes[id as usize].children.clone();
        for child in children {
            if let Some(child_node) = self.nodes.get_mut(child as usize) {
                child_node.parent_id = parent_id;
                if let Some(parent) = parent_id
                    && let Some(parent_node) = self.nodes.get_mut(parent as usize)
                    && !parent_node.children.contains(&child)
                {
                    parent_node.children.push(child);
                }
            }
        }
        if let Some(node) = self.nodes.get_mut(id as usize) {
            node.evicted = true;
            node.snapshot = None;
            node.editor = None;
            node.edit = None;
            node.children.clear();
        }
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
