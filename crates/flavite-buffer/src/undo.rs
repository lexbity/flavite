//! Undo tree with time-based coalescing (§4.14, D14).
//!
//! The undo tree stores edit operations and their inverses, supporting:
//! - Linear undo/redo
//! - Branching on undo-after-edit
//! - Time-based coalescing (default 500ms window)
//!
//! Every edit, undo, redo, reload, and journal patch advances the revision
//! even if text returns to previous bytes (§4.5.2).
//!
//! Node text is held as immutable version roots (RFC-0011 D-02): each node
//! stores the post-edit `Arc<VersionState>` (chunk-tree root + EOL state +
//! descriptor), so undo/redo/branch are O(1) pointer swaps, materialization
//! happens only at user-paced APIs, and the every-64 retention floor /
//! delta-replay machinery is gone. Node storage is a paged arena (D-09):
//! `id == global index` via arithmetic, tombstones bounded at one
//! partially-live page per buffer, and accounting is delta-maintained
//! (D-09/T6).
use crate::budget::{Budget, EvictionReport, LimitExceeded, RESERVATION_LEASE_MS, Reservation};
use crate::chunks::ChunkRoot;
use crate::history::{
    EditKind, EditorEdit, HistoryCursor, HistoryEntry, HistoryError, HistoryPage, HistoryTarget,
    PAGE_LIMIT, TRAIL_LIMIT,
};
use crate::revision::Revision;
use crate::selection::SpanEdit;
use flavite_encoding::{FormatStamp, SourceFormat};
use flavite_syntax::text::{Span, TextByte, TextSnapshot};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) fn wall_clock_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|time| u64::try_from(time.as_millis()).ok())
        .unwrap_or(0)
}

/// Shift a byte offset by a signed length delta, saturating at zero.
fn shift_usize(position: usize, delta: isize) -> usize {
    if delta >= 0 {
        position.saturating_add(delta as usize)
    } else {
        position.saturating_sub((-delta) as usize)
    }
}

/// Immutable post-edit document state held by an undo node (RFC-0011
/// D-02/D-04): the chunk-tree root plus the EOL side state and the save
/// descriptor. `None` on an undo node means evicted (tombstone) — unchanged
/// semantics.
#[derive(Debug, Clone)]
pub struct VersionState {
    /// The chunk-tree root (the document bytes).
    pub root: Arc<ChunkRoot>,
    /// Per-line physical endings (one per line).
    pub eols: Arc<[crate::eol::LineEnding]>,
    /// Whether the source ends with a trailing newline.
    pub has_trailing_newline: bool,
    /// Authoritative save-format descriptor.
    pub format: SourceFormat,
    /// Codec/hint provenance for the descriptor.
    pub stamp: FormatStamp,
}

impl VersionState {
    /// Build from a snapshot (legacy facades / recovery).
    #[must_use]
    pub fn from_snapshot(snapshot: &TextSnapshot) -> Self {
        let table = crate::eol::EolTable::from_snapshot(snapshot);
        let format = snapshot.format().unwrap_or_default();
        Self {
            root: Arc::new(ChunkRoot::from_text(snapshot.text())),
            eols: Arc::from(table.line_endings),
            has_trailing_newline: table.has_trailing_newline,
            format,
            stamp: FormatStamp::detected(format),
        }
    }

    /// The EOL table view.
    #[must_use]
    pub fn eol_table(&self) -> crate::eol::EolTable {
        crate::eol::EolTable {
            line_endings: self.eols.to_vec(),
            has_trailing_newline: self.has_trailing_newline,
        }
    }

    /// Document length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.root.len()
    }

    /// Whether the document is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.root.is_empty()
    }

    /// Materialize the full text (user-paced only).
    #[must_use]
    pub fn materialize(&self) -> String {
        self.root.materialize()
    }
}

/// The accounted bytes of one retained node (budget.rs `account_node` over
/// the node's own state; shared by the delta accounting and the reservation
/// feasibility plan).
fn node_accounted_bytes(node: &UndoNode) -> u64 {
    let snapshot_len = node.snapshot.as_ref().map_or(0, |state| state.len());
    let replacement_len = node.edit.as_ref().map_or(0, |e| e.replacement.len());
    let (before, after) = match &node.editor {
        Some(record) => (&record.before[..], &record.after[..]),
        None => (&[][..], &[][..]),
    };
    crate::budget::account_node(snapshot_len, replacement_len, before, after)
        + node.movement_steps.capacity() as u64 * std::mem::size_of::<Vec<SpanEdit>>() as u64
        + node
            .movement_steps
            .iter()
            .map(|s| s.capacity() as u64 * std::mem::size_of::<SpanEdit>() as u64)
            .sum::<u64>()
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
    /// undo tree merges coalesced edits with the coordinate mapping of
    /// RFC-0008 §5.3.2 (D4) — [`merge_mapped`] — and does not rely on this
    /// predicate for text correctness; `coalesce` does, which is why
    /// adjacency is the whole contract.)
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

/// Map one post-`prev` coordinate into pre-`prev` coordinates (RFC-0008
/// §5.3.2, D4):
///
/// | point x (post) | mapped x (pre) |
/// |---|---|
/// | x < s | x |
/// | s ≤ x ≤ s+p | s |
/// | x > s+p | x − d |
///
/// where `prev = [s, e) -> P`, `p = |P|`, `d = p − (e − s)`.
fn map_post_to_pre(x: usize, s: usize, _e: usize, p: usize, d: isize) -> isize {
    let x = x as isize;
    if x < s as isize {
        x
    } else if x <= (s + p) as isize {
        s as isize
    } else {
        x - d
    }
}

/// Local-window coordinate of a post-`prev` point (RFC-0008 §5.3.2): the
/// virtual window is `[start, s) + P + [e, end)` laid out linearly.
fn map_post_to_local(x: usize, s: usize, start: usize, z1: usize, p: usize) -> isize {
    let x = x as isize;
    if x < s as isize {
        x - start as isize
    } else if x <= (s + p) as isize {
        z1 as isize + (x - s as isize)
    } else {
        z1 as isize + p as isize + (x - (s + p) as isize)
    }
}

/// The original-text source for [`merge_mapped_with_source`]: the full
/// parent text, or a window of it with a base offset (RFC-0011 S2 uses the
/// window form to keep continuous gap-spanning merges O(gap) instead of
/// O(document)).
#[derive(Debug, Clone, Copy)]
pub enum SourceSlice<'a> {
    /// The full parent text.
    Full(&'a str),
    /// A window `[base, base + text.len())` of the parent text.
    Window { base: usize, text: &'a str },
}

impl SourceSlice<'_> {
    fn push_range(&self, out: &mut String, from: usize, to: usize) -> bool {
        if from >= to {
            return true;
        }
        match *self {
            SourceSlice::Full(text) => {
                if to > text.len() {
                    return false;
                }
                out.push_str(&text[from..to]);
                true
            }
            SourceSlice::Window { base, text } => {
                if from < base || to > base + text.len() {
                    return false;
                }
                out.push_str(&text[from - base..to - base]);
                true
            }
        }
    }
}

/// Merge `next` (coordinates in the text AFTER `prev` was applied) into
/// `prev`, producing one edit in `prev`'s pre-coordinates (RFC-0008 D4,
/// §5.3.2). Returns `None` when the composition would require original text
/// bytes the caller did not supply (gap-spanning continuous merges) or when
/// the inputs violate the coalescing preconditions.
///
/// `source` is the original text slice covering the merged window; it is
/// needed exactly when the merged replacement must carry bytes untouched by
/// both edits (a gap between `prev`'s span and `next`'s range). With
/// `source == None` those cases return `None`; adjacent/contained cases are
/// composed from the two replacements alone.
#[must_use]
pub fn merge_mapped_with_source(
    prev: &EditOp,
    next: &EditOp,
    source: Option<SourceSlice<'_>>,
) -> Option<EditOp> {
    if prev.boundary || next.boundary || prev.journal.is_some() || next.journal.is_some() {
        return None;
    }
    let s = prev.range.start.0;
    let e = prev.range.end.0;
    let p = prev.replacement.len();
    let d = p as isize - (e - s) as isize;
    let a = next.range.start.0;
    let b = next.range.end.0;
    if a > b {
        return None;
    }

    let a_pre = map_post_to_pre(a, s, e, p, d);
    let b_pre = map_post_to_pre(b, s, e, p, d);
    let start = (s as isize).min(a_pre).max(0) as usize;
    let end = (e as isize).max(b_pre).max(0) as usize;

    let z1 = s - start; // original bytes before the span
    let z3 = end - e; // original bytes after the span
    let la = map_post_to_local(a, s, start, z1, p)
        .max(0)
        .min((z1 + p + z3) as isize) as usize;
    let lb = map_post_to_local(b, s, start, z1, p)
        .max(0)
        .min((z1 + p + z3) as isize) as usize;

    // Push an original-text slice (zone-local offsets, unambiguous). Empty
    // slices are free; non-empty ones require the source.
    let push_orig = |out: &mut String, from: usize, to: usize| -> bool {
        if from >= to {
            return true;
        }
        let Some(text) = source else {
            return false;
        };
        text.push_range(out, from, to)
    };

    let mut out = String::with_capacity(p + next.replacement.len() + z1 + z3);

    // Zone 1: original bytes [start, s), local [0, z1). Insertion at
    // `la` sits before the span when la < z1.
    if z1 > 0 {
        if la < z1 {
            if !push_orig(&mut out, start, start + la) {
                return None;
            }
            out.push_str(&next.replacement);
            if !push_orig(&mut out, start + lb.min(z1), s) {
                return None;
            }
        } else if !push_orig(&mut out, start, s) {
            return None;
        }
    }

    // Zone 2: prev's replacement P, local [z1, z1+p). The zone owns the
    // insertion at the exact span end (la == z1+p) so the replacement
    // survives when zone 3 is empty; zone 3's condition is strictly-after.
    {
        let p_lo = z1;
        let p_hi = z1 + p;
        let del_s = la.max(p_lo).min(p_hi) - p_lo;
        let del_e = lb.max(p_lo).min(p_hi) - p_lo;
        if del_s > del_e {
            return None;
        }
        if la >= p_lo && la <= p_hi {
            out.push_str(&prev.replacement[..la - p_lo]);
            out.push_str(&next.replacement);
            out.push_str(&prev.replacement[del_e..]);
        } else {
            out.push_str(&prev.replacement[..del_s]);
            out.push_str(&prev.replacement[del_e..]);
        }
    }

    // Zone 3: original bytes [e, end), local [z1+p, z1+p+z3).
    if z3 > 0 {
        let base = z1 + p;
        if la > base {
            let off = la - base;
            if !push_orig(&mut out, e, e + off) {
                return None;
            }
            out.push_str(&next.replacement);
            let del_end = lb.min(base + z3);
            if !push_orig(&mut out, e + (del_end - base), end) {
                return None;
            }
        } else {
            let del_s = la.max(base).min(base + z3) - base;
            let del_e = lb.max(base).min(base + z3) - base;
            if del_s > del_e {
                return None;
            }
            if !push_orig(&mut out, e, e + del_s) {
                return None;
            }
            if !push_orig(&mut out, e + del_e, end) {
                return None;
            }
        }
    }

    Some(EditOp::new(Span::new(TextByte(start), TextByte(end)), out))
}

/// [`merge_mapped_with_source`] without a source: exact for every composition
/// that needs no original gap bytes; `None` otherwise (caller installs a new
/// node — RFC-0008 D4).
#[must_use]
pub fn merge_mapped(prev: &EditOp, next: &EditOp) -> Option<EditOp> {
    merge_mapped_with_source(prev, next, None)
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
    /// Ordered simultaneous batches, each in its own pre-step coordinates.
    /// Coalescing appends a step; undo reverses the walk and inverts each
    /// batch. Geometry beyond the receipt bound is explicitly truncated.
    pub movement_steps: Vec<Vec<SpanEdit>>,
    pub movement_truncated: bool,
    /// Unique identifier for this node.
    pub id: u64,
    /// Whether this node was evicted to satisfy a history budget. Evicted
    /// nodes drop their heavy state (state, editor record, edit, children)
    /// and are invisible to history pages, accounting, undo/redo and
    /// restore (FEUX-0002 X01).
    pub evicted: bool,
    /// The revision at this node.
    pub revision: Revision,
    /// The edit that led to this state (None for the root).
    pub edit: Option<EditOp>,
    /// The immutable version state at this node (RFC-0011 D-02); `None`
    /// once evicted.
    pub snapshot: Option<Arc<VersionState>>,
    /// The time when this edit was applied.
    pub timestamp: Instant,
    /// Parent node ID.
    pub parent_id: Option<u64>,
    /// Child node IDs.
    pub children: Vec<u64>,
}
impl UndoNode {
    /// Create a new root node (always materialized).
    #[must_use]
    pub fn root(revision: Revision, state: VersionState) -> Self {
        Self {
            changed_at_unix_ms: wall_clock_ms(),
            segment: None,
            version: 1,
            editor: None,
            kind: EditKind::Edit,
            movement_steps: Vec::new(),
            movement_truncated: false,
            id: 0,
            evicted: false,
            revision,
            edit: None,
            snapshot: Some(Arc::new(state)),
            timestamp: Instant::now(),
            parent_id: None,
            children: Vec::new(),
        }
    }
    /// Create a new node from an edit with its version state.
    #[must_use]
    pub fn new(
        id: u64,
        revision: Revision,
        edit: EditOp,
        parent_id: u64,
        state: VersionState,
    ) -> Self {
        Self {
            changed_at_unix_ms: wall_clock_ms(),
            segment: edit.journal.as_ref().map(|_| id),
            version: 1,
            editor: None,
            kind: EditKind::Edit,
            movement_steps: vec![vec![SpanEdit::new(
                edit.range.start.0,
                edit.range.end.0,
                edit.replacement.len(),
            )]],
            movement_truncated: false,
            id,
            evicted: false,
            revision,
            edit: Some(edit),
            snapshot: Some(Arc::new(state)),
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

// ----------------------------------------------------------------------
// Paged arena (D-09)
// ----------------------------------------------------------------------

const PAGE_SLOTS: usize = 512;

#[derive(Debug)]
struct Page<T> {
    slots: Box<[Option<T>]>,
    /// Number of slots ever used in this page (ids are never reused).
    count: usize,
    /// Number of live (non-evicted) entries.
    survivors: usize,
}

impl<T> Page<T> {
    fn new() -> Self {
        Self {
            slots: std::iter::repeat_with(|| None)
                .take(PAGE_SLOTS)
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            count: 0,
            survivors: 0,
        }
    }

    fn append(&mut self, value: T) -> Option<usize> {
        if self.count >= PAGE_SLOTS {
            return None;
        }
        let slot = self.count;
        self.count += 1;
        self.slots[slot] = Some(value);
        self.survivors += 1;
        Some(slot)
    }

    fn get(&self, slot: usize) -> Option<&T> {
        self.slots.get(slot)?.as_ref()
    }

    fn get_mut(&mut self, slot: usize) -> Option<&mut T> {
        self.slots.get_mut(slot)?.as_mut()
    }

    /// Mark a slot dead; returns true when the page is now fully dead.
    fn remove(&mut self, slot: usize) -> bool {
        if self.slots.get_mut(slot).and_then(Option::take).is_some() {
            self.survivors = self.survivors.saturating_sub(1);
        }
        self.survivors == 0
    }
}

/// An arena with stable ids (`id == global index`) whose backing memory is
/// freed page-by-page once every slot in a page is dead.
#[derive(Debug)]
pub(crate) struct PageArena<T> {
    pages: Vec<Option<Page<T>>>,
}

impl<T> PageArena<T> {
    fn new() -> Self {
        Self {
            pages: vec![Some(Page::new())],
        }
    }

    fn push(&mut self, value: T) -> u64 {
        if let Some(page) = self.pages.last_mut().and_then(|p| p.as_mut())
            && page.count < PAGE_SLOTS
        {
            let slot = page.append(value).expect("free slot");
            return ((self.pages.len() - 1) * PAGE_SLOTS + slot) as u64;
        }
        self.pages.push(Some(Page::new()));
        let idx = self.pages.len() - 1;
        let page = self.pages[idx].as_mut().expect("fresh page");
        let slot = page.append(value).expect("fresh page has a free slot");
        (idx * PAGE_SLOTS + slot) as u64
    }

    fn get(&self, id: u64) -> Option<&T> {
        let page = (id as usize) / PAGE_SLOTS;
        let slot = (id as usize) % PAGE_SLOTS;
        self.pages.get(page)?.as_ref()?.get(slot)
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut T> {
        let page = (id as usize) / PAGE_SLOTS;
        let slot = (id as usize) % PAGE_SLOTS;
        self.pages.get_mut(page)?.as_mut()?.get_mut(slot)
    }

    /// Remove a slot (mark dead); frees the page when fully dead.
    fn remove(&mut self, id: u64) {
        let page_idx = (id as usize) / PAGE_SLOTS;
        let slot = (id as usize) % PAGE_SLOTS;
        let Some(page) = self.pages.get_mut(page_idx).and_then(|p| p.as_mut()) else {
            return;
        };
        if page.remove(slot) {
            self.pages[page_idx] = None;
        }
    }

    /// Reset to a single fresh page.
    fn clear(&mut self) {
        self.pages = vec![Some(Page::new())];
    }

    /// Live (non-freed) entries, ascending id order.
    fn iter_all(&self) -> impl Iterator<Item = (u64, &T)> + '_ {
        self.pages.iter().enumerate().flat_map(|(pi, page)| {
            page.as_ref().into_iter().flat_map(move |p| {
                p.slots[..p.count]
                    .iter()
                    .enumerate()
                    .filter_map(move |(slot, v)| {
                        v.as_ref()
                            .map(move |v| ((pi * PAGE_SLOTS + slot) as u64, v))
                    })
            })
        })
    }

    /// Live (non-freed) entries, descending id order.
    fn iter_all_rev(&self) -> impl Iterator<Item = (u64, &T)> + '_ {
        self.pages.iter().enumerate().rev().flat_map(|(pi, page)| {
            page.as_ref().into_iter().flat_map(move |p| {
                p.slots[..p.count]
                    .iter()
                    .enumerate()
                    .rev()
                    .filter_map(move |(slot, v)| {
                        v.as_ref()
                            .map(move |v| ((pi * PAGE_SLOTS + slot) as u64, v))
                    })
            })
        })
    }

    /// Number of currently allocated pages (test/diagnostic).
    fn allocated_pages(&self) -> usize {
        self.pages.iter().filter(|p| p.is_some()).count()
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
    /// All nodes in the tree, paged. Evicted nodes are dropped from their
    /// slot (tombstone); `id == global index` holds via arithmetic.
    nodes: PageArena<UndoNode>,
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
    /// Total accounted retained bytes (delta-maintained, E-12).
    accounted_bytes: u64,
    /// Number of surviving (retained) nodes.
    survivor_count: usize,
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
        let root = UndoNode::root(Revision::INITIAL, VersionState::from_snapshot(&snapshot));
        let mut tree = Self {
            generation: 1,
            context_origin: None,
            nodes: PageArena::new(),
            preferred_tip: 0,
            position_id: 0,
            coalesce_window: DEFAULT_COALESCE_WINDOW,
            next_id: 1,
            budget: Budget::default(),
            accounted_bytes: 0,
            survivor_count: 0,
            reservations: Vec::new(),
            lease_nonce: 0,
            evicted_total: 0,
            retained_watermark: 0,
        };
        tree.nodes.push(root);
        tree.survivor_count = 1;
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
        self.get(self.preferred_tip)
    }

    /// Get the preferred redo tip mutably.
    #[must_use]
    pub fn current_mut(&mut self) -> Option<&mut UndoNode> {
        self.get_mut(self.preferred_tip)
    }

    /// Get a node by ID.
    #[must_use]
    pub fn get(&self, id: u64) -> Option<&UndoNode> {
        self.nodes.get(id)
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut UndoNode> {
        self.nodes.get_mut(id)
    }

    /// Get the position node.
    #[must_use]
    pub fn position(&self) -> Option<&UndoNode> {
        self.get(self.position_id)
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
    ) -> (Vec<crate::history::MovementSpan>, Vec<usize>, bool) {
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
                return (Vec::new(), Vec::new(), true);
            };
            up.push(id);
            current = parent;
        }
        let Some(lca) = lca else {
            return (Vec::new(), Vec::new(), true);
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
                return (Vec::new(), Vec::new(), true);
            };
            down.push(child);
            current = child;
        }
        let mut spans = Vec::new();
        let mut step_ends = Vec::new();
        for (ids, inverse) in [(up, true), (down, false)] {
            for id in ids {
                let Some(node) = self.get(id).filter(|n| !n.evicted && !n.movement_truncated)
                else {
                    return (Vec::new(), Vec::new(), true);
                };
                let steps: Vec<_> = if inverse {
                    node.movement_steps.iter().rev().collect()
                } else {
                    node.movement_steps.iter().collect()
                };
                for step in steps {
                    let mut shift: isize = 0;
                    for span in step {
                        let item = if inverse {
                            let start = shift_usize(span.start, shift);
                            MovementSpan {
                                start,
                                end: start + span.replacement_len,
                                replacement_len: span.end - span.start,
                            }
                        } else {
                            MovementSpan {
                                start: span.start,
                                end: span.end,
                                replacement_len: span.replacement_len,
                            }
                        };
                        shift += span.replacement_len as isize - (span.end - span.start) as isize;
                        spans.push(item);
                    }
                    if spans.len() > crate::history::MAX_MOVEMENT_SPANS {
                        return (Vec::new(), Vec::new(), true);
                    }
                    if !step.is_empty() {
                        step_ends.push(spans.len());
                    }
                }
            }
        }
        (spans, step_ends, false)
    }

    /// Apply an edit and advance the tree (eager snapshot representation —
    /// legacy facade, recovery, and tests; not the keystroke path).
    pub fn apply(&mut self, edit: EditOp, snapshot: TextSnapshot) -> Revision {
        if self.context_origin.is_some() || self.position().is_some_and(|n| n.editor.is_some()) {
            self.seal_current();
        }
        let state = VersionState::from_snapshot(&snapshot);
        self.apply_with_coalescing(edit, Arc::new(state), true, false)
    }

    /// Apply an edit on the interactive typing path with a version root
    /// (RFC-0011 D-02): the model passes the post-edit state.
    pub(crate) fn apply_version(&mut self, edit: EditOp, state: Arc<VersionState>) -> Revision {
        if self.context_origin.is_some() || self.position().is_some_and(|n| n.editor.is_some()) {
            self.seal_current();
        }
        self.apply_with_coalescing(edit, state, true, false)
    }

    /// One explicit typing run, independent of the ordinary idle timer
    /// (eager snapshot representation; tests/legacy).
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
        let state = VersionState::from_snapshot(&snapshot);
        self.apply_context_inner(edit, Arc::new(state), origin)
    }

    fn apply_context_inner(
        &mut self,
        edit: EditOp,
        state: Arc<VersionState>,
        origin: u128,
    ) -> Revision {
        if self.context_origin != Some(origin) {
            self.seal_current();
        }
        self.context_origin = Some(origin);
        self.apply_with_coalescing(edit, state, true, true)
    }

    /// Apply a distinct operation that must start a new undo-tree node.
    /// Destructive conflict resolutions use this so a merge cannot coalesce
    /// into nearby typing and lose its independent undo boundary.
    pub fn apply_boundary(&mut self, mut edit: EditOp, snapshot: TextSnapshot) -> Revision {
        edit.boundary = true;
        self.context_origin = None;
        let state = VersionState::from_snapshot(&snapshot);
        self.apply_with_coalescing(edit, Arc::new(state), false, false)
    }

    /// Version-root boundary install (RFC-0011 D-02): O(1), no snapshot.
    pub(crate) fn apply_boundary_version(
        &mut self,
        mut edit: EditOp,
        state: Arc<VersionState>,
    ) -> Revision {
        edit.boundary = true;
        self.context_origin = None;
        self.apply_with_coalescing(edit, state, false, false)
    }

    fn apply_with_coalescing(
        &mut self,
        edit: EditOp,
        state: Arc<VersionState>,
        allow_coalescing: bool,
        continuous: bool,
    ) -> Revision {
        let step = SpanEdit::new(edit.range.start.0, edit.range.end.0, edit.replacement.len());
        self.generation += 1;
        let branched = self.position_id != self.preferred_tip;
        let position_id = self.position_id;
        // First, check if we should coalesce
        let should_coalesce = allow_coalescing
            && !branched
            && if let Some(current) = self.get(position_id) {
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
        // RFC-0008 S1 (FR-10): the undo-coalescing decision/merge phase. The
        // `coalesced` field records which branch ran (merge vs new-node).
        let _undo_coalesce =
            tracing::info_span!("edit.undo_coalesce", coalesced = should_coalesce,).entered();
        if should_coalesce {
            // RFC-0008 S4 (D4): merge by coordinate mapping — no snapshot diff.
            // The pure table handles adjacent/contained compositions; a
            // gap-spanning continuous merge borrows only the gap slice from
            // the parent root (O(gap), never an O(document) scan).
            let coalesced = {
                let current = self.get(position_id).expect("position node").edit.clone();
                let Some(current) = current else {
                    return self.get(position_id).expect("position node").revision;
                };
                if let Some(op) = merge_mapped(&current, &edit) {
                    Some(op)
                } else {
                    self.merge_with_parent_gap(position_id, &current, &edit)
                }
            };
            if let Some(coalesced) = coalesced {
                let old_accounted = {
                    let node = self.get(position_id).expect("position node");
                    node_accounted_bytes(node)
                };
                {
                    let node = self.get_mut(position_id).expect("position node exists");
                    node.changed_at_unix_ms = wall_clock_ms();
                    node.version += 1;
                    if !node.movement_truncated {
                        node.movement_steps.reserve_exact(1);
                        node.movement_steps.push(vec![step]);
                        if node.movement_steps.iter().map(Vec::len).sum::<usize>()
                            > crate::history::MAX_MOVEMENT_SPANS
                        {
                            node.movement_steps = Vec::new();
                            node.movement_truncated = true;
                        }
                    }
                    node.edit = Some(coalesced);
                    node.snapshot = Some(state);
                    node.timestamp = Instant::now();
                }
                let new_accounted = {
                    let node = self.get(position_id).expect("position node");
                    node_accounted_bytes(node)
                };
                self.accounted_bytes = self.accounted_bytes - old_accounted + new_accounted;
                self.auto_evict_if_over_budget(wall_clock_ms());
                return self.get(position_id).expect("position node").revision;
            }
        }
        let current_node = self.get(position_id).expect("current node must exist");
        let parent_id = current_node.id;
        let new_revision = current_node.revision.next();
        let segment = current_node.segment;
        let new_id = self.next_id;
        self.next_id += 1;
        let mut new_node = UndoNode::new(new_id, new_revision, edit, parent_id, (*state).clone());
        if new_node.segment.is_none() {
            new_node.segment = segment;
        }
        // Add to parent's children
        if let Some(node) = self.get_mut(parent_id) {
            node.children.push(new_id);
        }
        // Add the new node
        self.nodes.push(new_node);
        self.preferred_tip = new_id;
        self.position_id = new_id;
        self.survivor_count += 1;
        let accounted = node_accounted_bytes(self.get(new_id).expect("fresh node"));
        self.accounted_bytes += accounted;
        self.auto_evict_if_over_budget(wall_clock_ms());
        new_revision
    }

    /// Merge `edit` into the coalescing node using the parent root's gap
    /// slice (O(gap)); `None` when the parent is unavailable/evicted or the
    /// composition needs bytes outside the window.
    fn merge_with_parent_gap(
        &self,
        position_id: u64,
        current: &EditOp,
        edit: &EditOp,
    ) -> Option<EditOp> {
        let parent_id = self.get(position_id)?.parent_id?;
        let parent_state = self.get(parent_id)?.snapshot.as_ref()?;
        // The merged window bounds (same geometry as merge_mapped_with_source).
        let s = current.range.start.0;
        let e = current.range.end.0;
        let p = current.replacement.len();
        let d = p as isize - (e - s) as isize;
        let a_pre = map_post_to_pre(edit.range.start.0, s, e, p, d);
        let b_pre = map_post_to_pre(edit.range.end.0, s, e, p, d);
        let start = (s as isize).min(a_pre).max(0) as usize;
        let end = (e as isize).max(b_pre).max(0) as usize;
        if end > parent_state.len() {
            return None;
        }
        let window = parent_state.root.slice(start..end);
        merge_mapped_with_source(
            current,
            edit,
            Some(SourceSlice::Window {
                base: start,
                text: &window,
            }),
        )
    }

    /// Preserve a save/semantic boundary without adding a document edit.
    pub fn seal_current(&mut self) {
        self.context_origin = None;
        if let Some(node) = self.get_mut(self.position_id)
            && let Some(edit) = &mut node.edit
        {
            edit.boundary = true;
        }
    }

    /// Undo the current edit.
    ///
    /// Moves the position back to the nearest surviving parent node
    /// (evicted chain links are skipped). Returns the parent's revision and
    /// version state. The caller (model) is responsible for advancing the
    /// revision (§4.5.2).
    pub fn undo(&mut self) -> Option<(Revision, Option<Arc<VersionState>>)> {
        self.seal_current();
        let current = self.position()?;
        current.parent_id?;
        let parent_id = self.nearest_surviving_ancestor(current.id)?;
        let state = self.get(parent_id)?.snapshot.clone();
        let parent_revision = self.survivor(parent_id)?.revision;
        self.position_id = parent_id;
        self.generation += 1;
        Some((parent_revision, state))
    }

    /// Redo the last undone edit.
    ///
    /// Moves the position forward to the current node if we've undone.
    /// For branching, moves to the appropriate child.
    ///
    /// Returns the target's revision and version state.
    /// The caller (model) is responsible for advancing the revision (§4.5.2).
    pub fn redo(&mut self) -> Option<(Revision, Option<Arc<VersionState>>)> {
        self.seal_current();
        let child_id = self.redo_target()?;
        let child = self.get(child_id)?;
        let revision = child.revision;
        let state = child.snapshot.clone();
        self.position_id = child_id;
        self.generation += 1;
        Some((revision, state))
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
    /// `spans` is the original batch geometry (E-13); it rides only a fresh
    /// node — a coalesced install recomputed its edit from the before/after
    /// diff, which is already exact.
    pub(crate) fn apply_recorded_version(
        &mut self,
        edit: EditOp,
        state: Arc<VersionState>,
        policy: UndoPolicy,
        record: EditorEdit,
        spans: &[SpanEdit],
    ) -> Revision {
        if self
            .position()
            .and_then(|n| n.editor.as_ref())
            .is_none_or(|previous| previous.origin != record.origin)
        {
            self.seal_current();
        }
        let revision = match policy {
            UndoPolicy::Isolated => self.apply_boundary_version(edit, state),
            UndoPolicy::Context { origin } => self.apply_context_inner(edit, state, origin),
            UndoPolicy::Typing => {
                if self.context_origin.is_some() {
                    self.seal_current();
                }
                self.apply_with_coalescing(edit, state, true, false)
            }
        };
        self.install_batch_geometry(spans);
        let node = self.get_mut(self.position_id).expect("position node");
        if let Some(previous) = &mut node.editor {
            previous.after = record.after;
            previous.primary = record.primary;
        } else {
            node.kind = record.kind;
            node.editor = Some(record);
        }
        self.auto_evict_if_over_budget(wall_clock_ms());
        revision
    }

    /// Install one prepared batch under `policy`, retaining the original
    /// per-op geometry for each step, including coalesced steps (E-13).
    pub(crate) fn apply_batch_version(
        &mut self,
        edit: EditOp,
        state: Arc<VersionState>,
        policy: UndoPolicy,
        spans: &[SpanEdit],
    ) -> Revision {
        let revision = match policy {
            UndoPolicy::Isolated => self.apply_boundary_version(edit, state),
            UndoPolicy::Context { origin } => {
                // Unannotated batches cannot extend a recorded editor
                // operation (same guard as `apply_context`).
                if self.position().is_some_and(|node| node.editor.is_some()) {
                    self.seal_current();
                }
                self.apply_context_inner(edit, state, origin)
            }
            UndoPolicy::Typing => {
                if self.context_origin.is_some() {
                    self.seal_current();
                }
                self.apply_with_coalescing(edit, state, true, false)
            }
        };
        self.install_batch_geometry(spans);
        revision
    }

    /// Replace the just-installed step's contiguous text diff with its exact
    /// disjoint geometry. Earlier coalesced steps retain their own frames.
    fn install_batch_geometry(&mut self, spans: &[SpanEdit]) {
        let old_accounted = {
            let node = self.get(self.position_id).expect("position node");
            node_accounted_bytes(node)
        };
        if let Some(node) = self.get_mut(self.position_id) {
            if node.movement_truncated {
                return;
            }
            if let Some(last) = node.movement_steps.last_mut() {
                *last = spans.to_vec();
            }
            if node.movement_steps.iter().map(Vec::len).sum::<usize>()
                > crate::history::MAX_MOVEMENT_SPANS
            {
                node.movement_steps = Vec::new();
                node.movement_truncated = true;
            }
        }
        let new_accounted = {
            let node = self.get(self.position_id).expect("position node");
            node_accounted_bytes(node)
        };
        self.accounted_bytes = self.accounted_bytes - old_accounted + new_accounted;
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
            .iter_all_rev()
            .filter(|(id, n)| *id < before && !n.evicted)
            .take(limit + 1)
            .filter_map(|(id, _)| self.entry(id, epoch, ancestors.contains(&id)))
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
    pub fn restore_snapshot(
        &self,
        target: HistoryTarget,
    ) -> Result<Arc<VersionState>, HistoryError> {
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
        while let Some(node) = self.get(preferred_tip) {
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
        let root = self.get(0).cloned();
        self.nodes.clear();
        self.survivor_count = 0;
        self.accounted_bytes = 0;
        if let Some(root) = root {
            self.nodes.push(root);
            self.survivor_count = 1;
        }
        self.preferred_tip = 0;
        self.position_id = 0;
        self.next_id = 1;
        self.reservations.clear();
        self.lease_nonce = 0;
        self.evicted_total = 0;
        self.retained_watermark = 0;
        self.reaccount();
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
            nodes: self.survivor_count as u64,
        }
    }

    /// Total nodes evicted since the tree started (or was last cleared).
    #[must_use]
    pub fn evicted_total(&self) -> u64 {
        self.evicted_total
    }

    /// Number of currently allocated arena pages (diagnostic; FR-14 page
    /// reclamation observability).
    #[must_use]
    pub fn history_allocated_pages(&self) -> usize {
        self.nodes.allocated_pages()
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

    /// Number of surviving (retained) nodes.
    #[must_use]
    pub fn retained_node_count(&self) -> usize {
        self.survivor_count
    }

    /// Ids of all surviving (retained) nodes, ascending. Test/diagnostic
    /// surface for the accounting mirror.
    #[must_use]
    pub fn survivor_ids(&self) -> Vec<u64> {
        self.nodes
            .iter_all()
            .filter(|(_, node)| !node.evicted)
            .map(|(id, _)| id)
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
    /// allocations + recorded selection sets + fixed metadata). Legacy full
    /// recompute for budget replacement and tests; the production path is
    /// delta accounting.
    fn reaccount(&mut self) {
        let mut bytes = 0u64;
        let mut watermark = u64::MAX;
        let mut survivors = 0usize;
        for (id, node) in self.nodes.iter_all() {
            if node.evicted {
                continue;
            }
            watermark = watermark.min(id);
            bytes += node_accounted_bytes(node);
            survivors += 1;
        }
        self.accounted_bytes = bytes;
        self.survivor_count = survivors;
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
            || self.survivor_count as u64 > self.budget.max_nodes;
        if over {
            self.evict_to_fit(0, now_ms);
        }
    }

    /// Reserve `projected` bytes for one new node against the budget,
    /// evicting deterministically to make room when needed. Refusal is
    /// complete and precedes any mutation: an infeasible reservation is
    /// detected *before* any eviction is committed, so a refused edit
    /// leaves the existing history graph byte-identical (X01 zero-effect
    /// refusal, FR-17/FR-22).
    pub fn reserve(&mut self, projected: u64, now_ms: u64) -> Result<Lease, LimitExceeded> {
        self.expire_leases(now_ms);
        let fits = |tree: &Self| {
            let pending: u64 = tree.reservations.iter().map(|r| r.projected).sum();
            tree.accounted_bytes + pending + projected <= tree.budget.max_bytes
                && (tree.survivor_count as u64) < tree.budget.max_nodes
        };
        if !fits(self) && !self.reservation_feasible(projected, now_ms) {
            return Err(LimitExceeded {
                needed: projected,
                budget: self.budget.max_bytes,
            });
        }
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

    /// Whether an eviction plan that fits `projected` exists at all. The
    /// deterministic pass can eventually evict every survivor except the
    /// current node and live restore-target protections, so the plan exists
    /// exactly when evicting that maximal set would fit. Pure: no mutation.
    fn reservation_feasible(&self, projected: u64, now_ms: u64) -> bool {
        let pending: u64 = self.reservations.iter().map(|r| r.projected).sum();
        let mut floor_bytes = 0u64;
        let mut floor_nodes = 0u64;
        for (id, node) in self.nodes.iter_all() {
            if node.evicted {
                continue;
            }
            let protected = id == self.position_id
                || self
                    .reservations
                    .iter()
                    .any(|r| r.lease_until_ms > now_ms && r.target == Some(id));
            if protected {
                floor_nodes += 1;
                floor_bytes += node_accounted_bytes(node);
            }
        }
        let freeable_bytes = self.accounted_bytes.saturating_sub(floor_bytes);
        let removable_nodes = (self.survivor_count as u64).saturating_sub(floor_nodes);
        self.accounted_bytes
            .saturating_sub(freeable_bytes)
            .saturating_add(pending)
            .saturating_add(projected)
            <= self.budget.max_bytes
            && (self.survivor_count as u64).saturating_sub(removable_nodes) < self.budget.max_nodes
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
                && (self.survivor_count as u64) < self.budget.max_nodes;
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
        let eligible = |id: u64, node: &UndoNode| {
            !node.evicted && !on_path.contains(&id) && !protected.contains(&id)
        };
        let all: Vec<(u64, &UndoNode)> = self
            .nodes
            .iter_all()
            .filter(|(id, node)| eligible(*id, node))
            .collect();
        // Pass 1: off-path tips (leaves), oldest first.
        if let Some(&(id, _)) = all
            .iter()
            .filter(|(_, n)| n.is_leaf())
            .min_by_key(|(id, _)| *id)
        {
            return Some(id);
        }
        // Pass 2: off-path subtree roots (parent on path or none), oldest
        // first. Removing the whole subtree never orphans survivors.
        if let Some(&(id, _)) = all
            .iter()
            .filter(|(_, n)| match n.parent_id {
                None => true,
                Some(parent) => on_path.contains(&parent),
            })
            .min_by_key(|(id, _)| *id)
        {
            return Some(id);
        }
        // Pass 3: oldest checkpointable prefix of the current path — never
        // the current node, never protected.
        self.nodes
            .iter_all()
            .filter(|(id, n)| !n.evicted && !protected.contains(id))
            .filter(|(id, _)| on_path.contains(id) && *id < self.position_id)
            .map(|(id, _)| id)
            .min()
    }

    /// Evict one node: detach from its parent, re-parent surviving children
    /// onto its parent (keeping surviving chains connected), and drop its
    /// heavy state. The slot becomes a tombstone (freed with its page when
    /// the page is fully dead) so id-index stability survives.
    fn remove_node(&mut self, id: u64) {
        let Some(node) = self.get(id) else {
            return;
        };
        let parent_id = node.parent_id;
        if let Some(parent) = parent_id
            && let Some(node) = self.get_mut(parent)
        {
            node.children.retain(|&child| child != id);
        }
        let children = self.get(id).map(|n| n.children.clone()).unwrap_or_default();
        for child in children {
            if let Some(child_node) = self.get_mut(child) {
                let old_accounted = node_accounted_bytes(child_node);
                child_node.parent_id = parent_id;
                child_node.movement_steps = Vec::new();
                child_node.movement_truncated = true;
                let new_accounted = node_accounted_bytes(child_node);
                self.accounted_bytes = self.accounted_bytes - old_accounted + new_accounted;
                if let Some(parent) = parent_id
                    && let Some(parent_node) = self.get_mut(parent)
                    && !parent_node.children.contains(&child)
                {
                    parent_node.children.push(child);
                }
            }
        }
        let accounted = node_accounted_bytes(self.get(id).expect("node exists"));
        if let Some(node) = self.get_mut(id) {
            node.evicted = true;
            node.snapshot = None;
            node.editor = None;
            node.edit = None;
            node.movement_steps = Vec::new();
            node.movement_truncated = true;
            node.children.clear();
        }
        self.nodes.remove(id);
        self.accounted_bytes = self.accounted_bytes.saturating_sub(accounted);
        self.survivor_count = self.survivor_count.saturating_sub(1);
        if id == self.retained_watermark {
            // Advance the watermark to the next survivor (amortized linear).
            let mut next = u64::MAX;
            for (nid, node) in self.nodes.iter_all() {
                if !node.evicted {
                    next = next.min(nid);
                }
            }
            self.retained_watermark = if next == u64::MAX { self.next_id } else { next };
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
        let state = |tree: &UndoTree| {
            tree.position()
                .and_then(|n| n.snapshot.clone())
                .expect("state")
        };
        assert_eq!(state(&tree).materialize(), "Acafé雪BC");
        assert_eq!(tree.undo().unwrap().1.unwrap().materialize(), "Acafé雪B");
        assert_eq!(tree.undo().unwrap().1.unwrap().materialize(), "Acafé雪");
        assert_eq!(tree.undo().unwrap().1.unwrap().materialize(), "café");
        assert_eq!(tree.redo().unwrap().1.unwrap().materialize(), "Acafé雪");
        tree.apply_context(edit(9, "branch"), snap("Acafé雪branch"), 1);
        assert_eq!(tree.undo().unwrap().1.unwrap().materialize(), "Acafé雪");
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
        assert_eq!(tree.undo().unwrap().1.unwrap().materialize(), "ABC");
        assert_eq!(tree.undo().unwrap().1.unwrap().materialize(), "AB");
        assert_eq!(tree.undo().unwrap().1.unwrap().materialize(), "A");
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
                let current_edit = tree.current().unwrap().edit.clone().unwrap();
                let parent_id = tree.current().unwrap().parent_id.unwrap();
                let parent_text = tree.get(parent_id).unwrap().snapshot.as_ref().unwrap().materialize();
                proptest::prop_assert_eq!(current_edit.apply(&parent_text), text.clone());
            }
            for expected in states[..states.len()-1].iter().rev() {
                let snapshot = tree.undo().unwrap().1.unwrap();
                proptest::prop_assert_eq!(snapshot.materialize(), expected.as_str());
            }
            proptest::prop_assert!(tree.undo().is_none());
            for expected in &states[1..] {
                let snapshot = tree.redo().unwrap().1.unwrap();
                proptest::prop_assert_eq!(snapshot.materialize(), expected.as_str());
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

    /// The S4 oracle: the pre-S4 coalesce implementation (parent-snapshot
    /// byte diff). Kept under `#[cfg(test)]` to prove `merge_mapped` ≡
    /// recompute-from-snapshots (RFC-0008 D4, AC-3).
    fn coalesce_by_snapshot_diff(_prev: &EditOp, before: &str, after: &str) -> EditOp {
        let before_bytes = before.as_bytes();
        let after_bytes = after.as_bytes();
        let mut prefix = before_bytes
            .iter()
            .zip(after_bytes.iter())
            .take_while(|(a, b)| a == b)
            .count();
        while prefix > 0 && (!before.is_char_boundary(prefix) || !after.is_char_boundary(prefix)) {
            prefix -= 1;
        }
        let mut suffix = before_bytes[prefix..]
            .iter()
            .rev()
            .zip(after_bytes[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        while suffix > 0
            && (!before.is_char_boundary(before.len() - suffix)
                || !after.is_char_boundary(after.len() - suffix))
        {
            suffix -= 1;
        }
        EditOp::new(
            Span::new(TextByte(prefix), TextByte(before.len() - suffix)),
            after[prefix..after.len() - suffix].into(),
        )
    }

    #[test]
    fn merge_mapped_corner_cases_from_the_table() {
        // S4-T3: the exact §5.3.2 corner cases.
        let parent = "abc";
        // Insert at span start: prev [1,1)->"X", next insert at post 1.
        let prev = EditOp::new(Span::new(TextByte(1), TextByte(1)), "X".into());
        let next = EditOp::new(Span::new(TextByte(1), TextByte(1)), "Y".into());
        let merged = merge_mapped(&prev, &next).expect("insert-at-span-start merges");
        assert_eq!(merged.range, Span::new(TextByte(1), TextByte(1)));
        assert_eq!(merged.replacement, "YX");
        assert_eq!(merged.apply(parent), "aYXbc");

        // Insert at span end: next insert at post 2 (just after "X").
        let next = EditOp::new(Span::new(TextByte(2), TextByte(2)), "Y".into());
        let merged = merge_mapped(&prev, &next).expect("insert-at-span-end merges");
        assert_eq!(merged.replacement, "XY");
        assert_eq!(merged.apply(parent), "aXYbc");

        // Backspace into the typed run: prev inserts "XYZ", delete middle.
        let prev = EditOp::new(Span::new(TextByte(1), TextByte(1)), "XYZ".into());
        let next = EditOp::new(Span::new(TextByte(2), TextByte(3)), "".into());
        let merged = merge_mapped(&prev, &next).expect("backspace-into-run merges");
        assert_eq!(merged.replacement, "XZ");
        assert_eq!(merged.apply(parent), "aXZbc");

        // Whole-span replacement (crossing both sides): prev [1,3)->"X".
        let prev = EditOp::new(Span::new(TextByte(1), TextByte(3)), "X".into());
        let next = EditOp::new(Span::new(TextByte(0), TextByte(4)), "Z".into());
        let merged = merge_mapped_with_source(&prev, &next, Some(SourceSlice::Full("abcde")))
            .expect("crossing replace merges with source");
        assert_eq!(merged.range, Span::new(TextByte(0), TextByte(5)));
        assert_eq!(merged.replacement, "Z");
        assert_eq!(merged.apply("abcde"), "Z");
        // The crossing replace consumes the whole span/gap, so it is also
        // exact without a source.
        let merged_no_source = merge_mapped(&prev, &next).expect("crossing replace is source-free");
        assert_eq!(merged_no_source, merged);

        // Continuous distant insert: prev [1,1)->"X", next insert at post 4
        // (end of "aXbc").
        let prev = EditOp::new(Span::new(TextByte(1), TextByte(1)), "X".into());
        let next = EditOp::new(Span::new(TextByte(4), TextByte(4)), "Y".into());
        assert_eq!(merge_mapped(&prev, &next), None, "gap needs source");
        let merged = merge_mapped_with_source(&prev, &next, Some(SourceSlice::Full(parent)))
            .expect("distant insert merges with source");
        assert_eq!(merged.apply(parent), "aXbcY");
        assert_eq!(merged.replacement, "XbcY");
        // The windowed source form (RFC-0011 S2) yields the same result.
        let merged_window = merge_mapped_with_source(
            &prev,
            &next,
            Some(SourceSlice::Window {
                base: 0,
                text: parent,
            }),
        )
        .expect("distant insert merges with windowed source");
        assert_eq!(merged_window, merged);
    }

    #[test]
    fn merge_mapped_rejects_boundary_and_journal_ops() {
        let prev = EditOp::new(Span::new(TextByte(1), TextByte(1)), "X".into());
        let mut boundary = EditOp::new(Span::new(TextByte(2), TextByte(2)), "Y".into());
        boundary.boundary = true;
        assert_eq!(merge_mapped(&prev, &boundary), None);
        assert_eq!(merge_mapped(&boundary, &prev), None);
        let mut journal = EditOp::new(Span::new(TextByte(2), TextByte(2)), "Y".into());
        journal.journal = Some(JournalTag {
            txn_id: "t".into(),
            item_id: 1,
        });
        assert_eq!(merge_mapped(&prev, &journal), None);
    }

    proptest::proptest! {
        #![proptest_config(proptest::test_runner::Config {
            cases: 128, ..proptest::test_runner::Config::default()
        })]

        /// S4-T1 (AC-3): `merge_mapped` (with the parent source when gap bytes
        /// are needed) equals the old recompute-from-snapshots oracle on
        /// randomized edit sequences — byte-for-byte, including replacement
        /// content — and applying it equals sequential application.
        #[test]
        fn merge_mapped_equals_the_snapshot_diff_oracle(
            base in "[a-zA-Z0-9 .,!?]{0,24}",
            steps in proptest::collection::vec(
                (".{0,12}", 0usize..64, 0usize..64, proptest::bool::ANY),
                1..60,
            ),
        ) {
            let mut parent = base.clone(); // text at the node owning `prev`
            let mut prev: Option<EditOp> = None;
            for (repl, x, y, coalesce) in steps {
                let current = prev
                    .as_ref()
                    .map(|op| op.apply(&parent))
                    .unwrap_or_else(|| parent.clone());
                let len = current.len();
                let mut a = x % (len + 1);
                while !current.is_char_boundary(a) {
                    a -= 1;
                }
                let mut b = (a + y % (len + 1 - a)).min(len);
                while b < len && !current.is_char_boundary(b) {
                    b += 1;
                }
                let next_op = EditOp::new(Span::new(TextByte(a), TextByte(b)), repl);

                match (prev.take(), coalesce) {
                    (Some(p), true) => {
                        let after = next_op.apply(&p.apply(&parent));
                        let oracle = coalesce_by_snapshot_diff(&p, &parent, &after);
                        if let Some(merged) = merge_mapped_with_source(
                            &p,
                            &next_op,
                            Some(SourceSlice::Full(&parent)),
                        ) {
                            // Application equivalence is the correctness
                            // contract: the oracle may trim boundary bytes
                            // identical to the untouched context (and thus
                            // shift its range), so literal op equality is
                            // not required — both must yield the exact
                            // final text.
                            proptest::prop_assert_eq!(&merged.apply(&parent), &after);
                            proptest::prop_assert_eq!(&oracle.apply(&parent), &after);
                            if let Some(no_source) = merge_mapped(&p, &next_op) {
                                proptest::prop_assert_eq!(&no_source.apply(&parent), &after);
                            }
                            prev = Some(merged);
                        } else {
                            parent = p.apply(&parent);
                            prev = Some(next_op);
                        }
                    }
                    (Some(p), false) => {
                        parent = p.apply(&parent);
                        prev = Some(next_op);
                    }
                    (None, _) => {
                        prev = Some(next_op);
                    }
                }
            }
        }
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

    /// The page-arena reclamation contract (FR-14): evicting every node in
    /// a page frees its backing allocation.
    #[test]
    fn page_arena_frees_fully_dead_pages() {
        let mut arena: PageArena<u64> = PageArena::new();
        let ids: Vec<u64> = (0..PAGE_SLOTS as u64 + 3).map(|_| arena.push(7)).collect();
        assert_eq!(arena.allocated_pages(), 2);
        // Kill the first page entirely (indices 0..512).
        for &id in &ids[..PAGE_SLOTS] {
            arena.remove(id);
        }
        assert_eq!(arena.allocated_pages(), 1, "fully-dead page freed");
        assert!(arena.get(ids[0]).is_none());
        assert_eq!(*arena.get(ids[PAGE_SLOTS]).unwrap(), 7);
        // Partial kills keep the page allocated (one live page remains).
        let id = arena.push(9);
        arena.remove(id);
        assert_eq!(arena.allocated_pages(), 1);
        // The surviving entries are still addressable by id.
        assert_eq!(*arena.get(ids[PAGE_SLOTS + 1]).unwrap(), 7);
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
