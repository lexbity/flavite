//! Persistent copy-on-write chunk tree (RFC-0011 D-01 / S1).
//!
//! The single document representation: a balanced B-tree of fixed-size byte
//! leaves with additive summaries. Every edit path-copies O(log n) nodes and
//! 1–3 leaves; old roots are immutable and retained by whoever holds them
//! (`Arc`). There is exactly one representation — no mirror `String`, no
//! rope — so snapshots are free and undo nodes can hold roots.
//!
//! Invariants:
//! - All leaves sit at the same depth (the tree is perfectly balanced).
//! - Every leaf is ≤ [`MAX_CHUNK`] bytes; leaves are only ever merged when
//!   their combined size stays ≤ [`TARGET_CHUNK`] (amortized O(chunk)
//!   appends via the seam-merge in [`ChunkRoot::concat`]).
//! - `Summary` is additive: `byte_len`/`newlines` of a parent equal the sum
//!   over children. Bytes outside an edited range are bit-identical across
//!   versions (proven by the oracle proptests, `tests/chunks_oracle.rs`).
//!
//! All public operations are total on valid in-range inputs and panic on
//! out-of-range inputs (same contract as `str` slicing); callers validate
//! ranges upstream.

use flavite_syntax::text_window::StructureText;
use std::ops::Range;
use std::sync::Arc;

/// Target leaf size in bytes.
pub const TARGET_CHUNK: usize = 4096;
/// Absolute leaf ceiling; a leaf is split when it would exceed this.
pub const MAX_CHUNK: usize = 8192;
/// Leaves below this size are eligible to be merged with a neighbor.
pub const MIN_MERGE: usize = 1024;
/// Maximum children per internal node.
pub const FANOUT: usize = 8;

/// Additive node summary: byte length and newline count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Summary {
    pub byte_len: u32,
    pub newlines: u32,
}

impl Summary {
    pub const ZERO: Summary = Summary {
        byte_len: 0,
        newlines: 0,
    };

    /// Fold two adjacent summaries (associative, commutative-safe).
    #[must_use]
    pub const fn fold(self, other: Summary) -> Summary {
        Summary {
            byte_len: self.byte_len + other.byte_len,
            newlines: self.newlines + other.newlines,
        }
    }
}

fn summary_of_bytes(bytes: &str) -> Summary {
    Summary {
        byte_len: u32::try_from(bytes.len()).expect("document exceeds 4 GiB"),
        newlines: u32::try_from(bytes.bytes().filter(|&b| b == b'\n').count())
            .expect("document exceeds 4 GiB newlines"),
    }
}

/// A single immutable leaf of bytes.
#[derive(Debug, Clone)]
pub struct Leaf {
    bytes: Arc<str>,
    summary: Summary,
}

impl Leaf {
    fn new(bytes: &str) -> Self {
        let summary = summary_of_bytes(bytes);
        Self {
            bytes: Arc::from(bytes),
            summary,
        }
    }

    /// Split at byte offset `at` (must be a char boundary).
    #[must_use]
    fn split(&self, at: usize) -> (Leaf, Leaf) {
        debug_assert!(at <= self.bytes.len());
        debug_assert!(self.bytes.is_char_boundary(at));
        (Leaf::new(&self.bytes[..at]), Leaf::new(&self.bytes[at..]))
    }

    /// Merge with a following leaf.
    #[must_use]
    fn merge(&self, other: &Leaf) -> Leaf {
        let mut out = String::with_capacity(self.bytes.len() + other.bytes.len());
        out.push_str(&self.bytes);
        out.push_str(&other.bytes);
        Leaf::new(&out)
    }
}

/// Tree node: a leaf or an internal node of children.
#[derive(Debug)]
struct Node {
    summary: Summary,
    kind: NodeKind,
}

#[derive(Debug)]
enum NodeKind {
    Leaf(Leaf),
    Inner(Vec<Arc<Node>>),
}

impl Node {
    fn leaf(leaf: Leaf) -> Arc<Node> {
        let summary = leaf.summary;
        Arc::new(Node {
            summary,
            kind: NodeKind::Leaf(leaf),
        })
    }

    fn leaf_str(bytes: &str) -> Arc<Node> {
        Self::leaf(Leaf::new(bytes))
    }

    fn inner(children: Vec<Arc<Node>>) -> Arc<Node> {
        debug_assert!(!children.is_empty());
        debug_assert!(children.len() <= FANOUT);
        let mut summary = Summary::ZERO;
        let mut height: Option<usize> = None;
        for child in &children {
            summary = summary.fold(child.summary);
            let h = height_of(child);
            if let Some(h0) = height {
                debug_assert_eq!(h0, h, "children must share a height");
            } else {
                height = Some(h);
            }
        }
        Arc::new(Node {
            summary,
            kind: NodeKind::Inner(children),
        })
    }

    fn as_leaf(&self) -> Option<&Leaf> {
        match &self.kind {
            NodeKind::Leaf(l) => Some(l),
            NodeKind::Inner(_) => None,
        }
    }

    fn as_inner(&self) -> Option<&Vec<Arc<Node>>> {
        match &self.kind {
            NodeKind::Inner(children) => Some(children),
            NodeKind::Leaf(_) => None,
        }
    }
}

fn height_of(node: &Arc<Node>) -> usize {
    match &node.kind {
        NodeKind::Leaf(_) => 0,
        NodeKind::Inner(children) => 1 + height_of(&children[0]),
    }
}

/// The immutable root handle of one document version.
#[derive(Debug, Clone)]
pub struct ChunkRoot {
    node: Arc<Node>,
}

impl ChunkRoot {
    /// The empty document (one empty leaf).
    #[must_use]
    pub fn empty() -> Self {
        Self {
            node: Node::leaf_str(""),
        }
    }

    /// Build a tree from text.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        let mut leaves: Vec<Arc<Node>> = Vec::new();
        if text.is_empty() {
            leaves.push(Node::leaf_str(""));
        } else {
            let mut start = 0usize;
            while start < text.len() {
                let mut end = (start + TARGET_CHUNK).min(text.len());
                while end < text.len() && !text.is_char_boundary(end) {
                    end -= 1;
                }
                leaves.push(Node::leaf_str(&text[start..end]));
                start = end;
            }
        }
        while leaves.len() > 1 {
            let mut next = Vec::with_capacity(leaves.len().div_ceil(FANOUT));
            for chunk in leaves.chunks(FANOUT) {
                next.push(Node::inner(chunk.to_vec()));
            }
            leaves = next;
        }
        Self {
            node: leaves.into_iter().next().expect("at least one leaf"),
        }
    }

    /// Byte length of the document.
    #[must_use]
    pub fn len(&self) -> usize {
        self.node.summary.byte_len as usize
    }

    /// Whether the document has zero bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of lines (LF-counted; an empty document has one line).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.node.summary.newlines as usize + 1
    }

    // ------------------------------------------------------------------
    // Layout queries
    // ------------------------------------------------------------------

    /// The path (child indices from the root) to the leaf containing byte
    /// `pos`, the leaf itself, and the global byte offset of the leaf's
    /// start. `pos` must be `< len`.
    fn locate(&self, pos: usize) -> (Vec<usize>, Arc<Leaf>, usize) {
        debug_assert!(pos < self.len());
        let mut node = self.node.clone();
        let mut path = Vec::new();
        let mut base = 0usize;
        loop {
            match &node.kind {
                NodeKind::Leaf(leaf) => return (path, Arc::new(leaf.clone()), base),
                NodeKind::Inner(children) => {
                    let mut acc = base;
                    let mut found = None;
                    for (idx, child) in children.iter().enumerate() {
                        if pos < acc + child.summary.byte_len as usize {
                            found = Some(idx);
                            break;
                        }
                        acc += child.summary.byte_len as usize;
                    }
                    let idx = found.expect("pos within document");
                    path.push(idx);
                    node = children[idx].clone();
                    base = acc;
                }
            }
        }
    }

    /// The leaf segments intersecting `[start, end)`, reported as
    /// `(leaf, local_start, local_end, global_base)` (in document order).
    fn segments_in(&self, start: usize, end: usize) -> Vec<(Arc<str>, usize, usize, usize)> {
        let mut out = Vec::new();
        self.collect_segments(&self.node, start, end, 0, &mut out);
        out
    }

    fn collect_segments(
        &self,
        node: &Arc<Node>,
        start: usize,
        end: usize,
        base: usize,
        out: &mut Vec<(Arc<str>, usize, usize, usize)>,
    ) {
        match &node.kind {
            NodeKind::Leaf(leaf) => {
                let lo = start.saturating_sub(base);
                let hi = end.saturating_sub(base).min(leaf.bytes.len());
                if lo < hi {
                    out.push((leaf.bytes.clone(), lo, hi, base));
                }
            }
            NodeKind::Inner(children) => {
                let mut acc = base;
                for child in children {
                    let child_end = acc + child.summary.byte_len as usize;
                    if child_end > start && acc < end {
                        self.collect_segments(child, start, end, acc, out);
                    }
                    acc = child_end;
                    if acc >= end {
                        break;
                    }
                }
            }
        }
    }

    /// The bytes `[start, end)` (fresh allocation).
    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> String {
        let (start, end) = (range.start.min(range.end), range.start.max(range.end));
        assert!(end <= self.len(), "slice end out of range");
        if start >= end {
            return String::new();
        }
        let mut out = String::with_capacity(end - start);
        for (bytes, lo, hi, _base) in self.segments_in(start, end) {
            out.push_str(&bytes[lo..hi]);
        }
        out
    }

    /// Materialize the whole document.
    #[must_use]
    pub fn materialize(&self) -> String {
        self.slice(0..self.len())
    }

    /// Whether `pos` is a UTF-8 scalar boundary in this tree (O(log n)).
    /// `0` and `len` are boundaries; a position inside a multi-byte
    /// character is not.
    #[must_use]
    pub fn is_char_boundary(&self, pos: usize) -> bool {
        if pos == 0 {
            return true;
        }
        if pos >= self.len() {
            return pos == self.len();
        }
        let (_, leaf, base) = self.locate(pos);
        let local = pos - base;
        leaf.bytes.is_char_boundary(local.min(leaf.bytes.len()))
    }

    /// The number of newlines strictly before byte `pos` (i.e. the line
    /// index of the line containing `pos`, clamped).
    #[must_use]
    pub fn line_index_of(&self, pos: usize) -> usize {
        let pos = pos.min(self.len());
        if pos == self.len() {
            return self.node.summary.newlines as usize;
        }
        let (path, leaf, base) = self.locate(pos);
        let mut lines = 0u32;
        let mut node = self.node.clone();
        for &i in &path {
            if let NodeKind::Inner(children) = &node.kind {
                for child in &children[..i] {
                    lines += child.summary.newlines;
                }
                node = children[i].clone();
            }
        }
        let local = pos - base;
        lines += leaf.bytes.as_bytes()[..local.min(leaf.bytes.len())]
            .iter()
            .filter(|&&b| b == b'\n')
            .count() as u32;
        lines as usize
    }

    /// Byte offset of the start of `line` (clamped to the document end).
    #[must_use]
    pub fn line_start_of(&self, line: usize) -> usize {
        if line == 0 {
            return 0;
        }
        let mut remaining = line;
        let mut node = self.node.clone();
        let mut acc = 0usize;
        loop {
            match &node.kind {
                NodeKind::Leaf(leaf) => {
                    for (i, b) in leaf.bytes.bytes().enumerate() {
                        if b == b'\n' {
                            remaining -= 1;
                            if remaining == 0 {
                                return acc + i + 1;
                            }
                        }
                    }
                    return self.len();
                }
                NodeKind::Inner(children) => {
                    let mut chosen = None;
                    for (idx, child) in children.iter().enumerate() {
                        if remaining <= child.summary.newlines as usize {
                            chosen = Some(idx);
                            break;
                        }
                        remaining -= child.summary.newlines as usize;
                        acc += child.summary.byte_len as usize;
                    }
                    let idx = chosen.unwrap_or(children.len() - 1);
                    node = children[idx].clone();
                }
            }
        }
    }

    /// Line-start offsets relevant to `[start, end)`: the start of the line
    /// containing `start`, plus every line start at or after `start` and at
    /// or before `end`.
    #[must_use]
    pub fn line_starts_in(&self, range: Range<usize>) -> Vec<usize> {
        let (start, end) = (range.start.min(range.end), range.start.max(range.end));
        assert!(end <= self.len());
        let first = self.line_start_of(self.line_index_of(start));
        let mut out = vec![first];
        for (bytes, lo, hi, base) in self.segments_in(start, end) {
            for (i, b) in bytes[lo..hi].bytes().enumerate() {
                if b == b'\n' {
                    let at = base + lo + i + 1;
                    if at >= start && at <= end && at > first {
                        out.push(at);
                    }
                }
            }
        }
        out
    }

    /// The byte offset of the first `\n` at or after `from`, if any.
    #[must_use]
    pub fn find_next_newline(&self, from: usize) -> Option<usize> {
        if from >= self.len() {
            return None;
        }
        self.search_newline(&self.node, from, 0)
    }

    fn search_newline(&self, node: &Arc<Node>, from: usize, base: usize) -> Option<usize> {
        match &node.kind {
            NodeKind::Leaf(leaf) => {
                let local = from.saturating_sub(base);
                leaf.bytes
                    .as_bytes()
                    .iter()
                    .enumerate()
                    .skip(local)
                    .find(|&(_, &b)| b == b'\n')
                    .map(|(i, _)| base + i)
            }
            NodeKind::Inner(children) => {
                let mut acc = base;
                for child in children {
                    let child_end = acc + child.summary.byte_len as usize;
                    if child_end > from {
                        if let Some(at) = self.search_newline(child, from, acc) {
                            return Some(at);
                        }
                        return None;
                    }
                    acc = child_end;
                }
                None
            }
        }
    }

    /// Whether the document ends with a newline byte.
    #[must_use]
    pub fn ends_with_newline(&self) -> bool {
        let mut node = self.node.clone();
        loop {
            match &node.kind {
                NodeKind::Leaf(leaf) => return leaf.bytes.ends_with('\n'),
                NodeKind::Inner(children) => node = children.last().expect("non-empty").clone(),
            }
        }
    }

    /// Leaf ranges intersecting `[start, end)`, in document order.
    #[must_use]
    pub fn leaf_ranges_in(&self, range: Range<usize>) -> Vec<(Arc<str>, Range<usize>)> {
        self.segments_in(range.start.min(range.end), range.start.max(range.end))
            .into_iter()
            .map(|(bytes, lo, hi, base)| (bytes, base + lo..base + hi))
            .collect()
    }

    /// The leaf segment containing byte `pos`, as `(bytes, local_offset)`
    /// where `local_offset` indexes into `bytes` (RFC-0011 S2 boundary
    /// probes; safe for mid-character positions).
    pub(crate) fn leaf_context_at(&self, pos: usize) -> Option<(Arc<str>, usize)> {
        let (bytes, lo, _hi, _base) = self.segments_in(pos, pos + 1).into_iter().next()?;
        Some((bytes, lo))
    }

    // ------------------------------------------------------------------
    // Editing
    // ------------------------------------------------------------------

    /// Apply `replacement` over `[start, end)` and return the new root.
    /// The old root is untouched.
    #[must_use]
    pub fn edit(&self, range: Range<usize>, replacement: &str) -> ChunkRoot {
        let (start, end) = (range.start.min(range.end), range.start.max(range.end));
        assert!(end <= self.len(), "edit end out of range");
        if start == end && replacement.is_empty() {
            return self.clone();
        }
        let (left, mid) = self.split_at(start);
        let (_middle, right) = mid.split_at(end - start);
        let repl = ChunkRoot::from_text(replacement);
        ChunkRoot::concat(ChunkRoot::concat(left, repl), right)
    }

    /// Split at byte `pos`: `(bytes [0, pos), bytes [pos, len))`.
    #[must_use]
    pub fn split_at(&self, pos: usize) -> (ChunkRoot, ChunkRoot) {
        if pos == 0 {
            return (ChunkRoot::empty(), self.clone());
        }
        if pos >= self.len() {
            return (self.clone(), ChunkRoot::empty());
        }
        let (path, leaf, base) = self.locate(pos);
        let local = pos - base;
        if local == 0 {
            // `pos` aligns with the leaf start: the leaf itself belongs to
            // the right half.
            let left = prefix_strict(&self.node, &path);
            let right = suffix_from_inclusive(&self.node, &path, leaf);
            (
                left.map_or_else(ChunkRoot::empty, |n| ChunkRoot {
                    node: normalize_root(n),
                }),
                ChunkRoot {
                    node: normalize_root(right),
                },
            )
        } else {
            let (l1, l2) = leaf.split(local);
            let left = prefix_upto_inclusive(&self.node, &path, Arc::new(l1));
            let right = suffix_from_inclusive(&self.node, &path, Arc::new(l2));
            (
                ChunkRoot {
                    node: normalize_root(left),
                },
                ChunkRoot {
                    node: normalize_root(right),
                },
            )
        }
    }

    /// Concatenate two trees (bytes of `a` followed by bytes of `b`).
    #[must_use]
    pub fn concat(a: ChunkRoot, b: ChunkRoot) -> ChunkRoot {
        if a.is_empty() {
            return b;
        }
        if b.is_empty() {
            return a;
        }
        let forest = concat_nodes(a.node, b.node);
        debug_assert!(forest.len() <= 2);
        if forest.len() == 1 {
            ChunkRoot {
                node: normalize_root(forest[0].clone()),
            }
        } else {
            ChunkRoot {
                node: normalize_root(Node::inner(forest)),
            }
        }
    }

    // ------------------------------------------------------------------
    // Debug/test introspection
    // ------------------------------------------------------------------

    /// All leaves in document order (deep debug/tests).
    #[must_use]
    pub fn leaves(&self) -> Vec<Arc<str>> {
        let mut out = Vec::new();
        self.collect_leaves(&self.node, &mut out);
        out
    }

    fn collect_leaves(&self, node: &Arc<Node>, out: &mut Vec<Arc<str>>) {
        match &node.kind {
            NodeKind::Leaf(leaf) => out.push(leaf.bytes.clone()),
            NodeKind::Inner(children) => {
                for child in children {
                    self.collect_leaves(child, out);
                }
            }
        }
    }

    /// The height of the tree (0 for a single leaf).
    #[must_use]
    pub fn height(&self) -> usize {
        height_of(&self.node)
    }
}

/// Keep the subtree up to and including the leaf addressed by `path`,
/// replacing that leaf with `leaf`; drop everything after it. Single-child
/// ancestors are preserved as inner nodes so sibling heights stay uniform;
/// only the root may collapse (see [`normalize_root`]).
fn prefix_upto_inclusive(node: &Arc<Node>, path: &[usize], leaf: Arc<Leaf>) -> Arc<Node> {
    if path.is_empty() {
        return Node::leaf((*leaf).clone());
    }
    let i = path[0];
    let children = node.as_inner().expect("path descends through inner");
    let mut new_children = Vec::with_capacity(i + 1);
    for c in &children[..i] {
        new_children.push(c.clone());
    }
    new_children.push(prefix_upto_inclusive(&children[i], &path[1..], leaf));
    Node::inner(new_children)
}

/// Keep the subtree from the leaf addressed by `path` onward (that leaf
/// replaced by `leaf`); drop everything before it.
fn suffix_from_inclusive(node: &Arc<Node>, path: &[usize], leaf: Arc<Leaf>) -> Arc<Node> {
    if path.is_empty() {
        return Node::leaf((*leaf).clone());
    }
    let i = path[0];
    let children = node.as_inner().expect("path descends through inner");
    let mut new_children = Vec::with_capacity(children.len() - i);
    new_children.push(suffix_from_inclusive(&children[i], &path[1..], leaf));
    for c in &children[i + 1..] {
        new_children.push(c.clone());
    }
    Node::inner(new_children)
}

/// The subtree of all leaves strictly before the leaf addressed by `path`.
fn prefix_strict(node: &Arc<Node>, path: &[usize]) -> Option<Arc<Node>> {
    if path.is_empty() {
        return None;
    }
    let i = path[0];
    let children = node.as_inner().expect("path descends through inner");
    let mut new_children = Vec::with_capacity(i + 1);
    for c in &children[..i] {
        new_children.push(c.clone());
    }
    if let Some(rest) = prefix_strict(&children[i], &path[1..]) {
        new_children.push(rest);
    }
    if new_children.is_empty() {
        None
    } else {
        Some(Node::inner(new_children))
    }
}

/// Collapse single-child root chains (a root with one child IS that child).
fn normalize_root(node: Arc<Node>) -> Arc<Node> {
    let mut node = node;
    while let NodeKind::Inner(children) = &node.kind {
        if children.len() == 1 {
            node = children[0].clone();
        } else {
            break;
        }
    }
    node
}

/// Concatenate two nodes into a forest of 1–2 roots of equal height.
fn concat_nodes(a: Arc<Node>, b: Arc<Node>) -> Vec<Arc<Node>> {
    let ha = height_of(&a);
    let hb = height_of(&b);
    match ha.cmp(&hb) {
        std::cmp::Ordering::Equal => {
            if ha == 0 {
                // Both leaves: merge when the seam stays small.
                let total = a.summary.byte_len as usize + b.summary.byte_len as usize;
                if total <= TARGET_CHUNK {
                    let merged = a.as_leaf().expect("leaf").merge(b.as_leaf().expect("leaf"));
                    return vec![Node::leaf(merged)];
                }
            }
            vec![Node::inner(vec![a, b])]
        }
        std::cmp::Ordering::Greater => insert_into_spine(a, b, true),
        std::cmp::Ordering::Less => insert_into_spine(b, a, false),
    }
}

/// Insert `sub` (of height `height(sub)`) into the left/right spine of
/// `node` (which is strictly taller), returning a forest of 1–2 roots of
/// the same height as `node`.
fn insert_into_spine(node: Arc<Node>, sub: Arc<Node>, right: bool) -> Vec<Arc<Node>> {
    let child_h = height_of(&node) - 1;
    let sub_h = height_of(&sub);
    if sub_h == child_h {
        // `sub` becomes a direct child of `node`.
        let mut children = node.as_inner().expect("inner").clone();
        if right {
            // Seam-merge: if the neighbor is a leaf and the combined size
            // stays within target, merge instead of adding a child (append
            // locality).
            let last_is_leaf = children
                .last()
                .is_some_and(|c| matches!(c.kind, NodeKind::Leaf(_)));
            if last_is_leaf && sub_h == 0 {
                let last = children.last().expect("last");
                if last.summary.byte_len as usize + sub.summary.byte_len as usize <= TARGET_CHUNK {
                    let merged = last
                        .as_leaf()
                        .expect("leaf")
                        .merge(sub.as_leaf().expect("leaf"));
                    let last_idx = children.len() - 1;
                    children[last_idx] = Node::leaf(merged);
                    return vec![Node::inner(children)];
                }
            }
            children.push(sub);
        } else {
            let first_is_leaf = children
                .first()
                .is_some_and(|c| matches!(c.kind, NodeKind::Leaf(_)));
            if first_is_leaf && sub_h == 0 {
                let first = children[0].clone();
                if first.summary.byte_len as usize + sub.summary.byte_len as usize <= TARGET_CHUNK {
                    let merged = sub
                        .as_leaf()
                        .expect("leaf")
                        .merge(first.as_leaf().expect("leaf"));
                    children[0] = Node::leaf(merged);
                    return vec![Node::inner(children)];
                }
            }
            children.insert(0, sub);
        }
        if children.len() <= FANOUT {
            vec![Node::inner(children)]
        } else {
            let mid = children.len() / 2;
            vec![
                Node::inner(children[..mid].to_vec()),
                Node::inner(children[mid..].to_vec()),
            ]
        }
    } else {
        // Recurse into the extremity child.
        let mut children = node.as_inner().expect("inner").clone();
        let idx = if right { children.len() - 1 } else { 0 };
        let child = children.remove(idx);
        let forest = insert_into_spine(child, sub, right);
        let forest_len = forest.len();
        if right {
            children.extend(forest);
        } else {
            children.splice(0..0, forest);
        }
        if children.len() <= FANOUT {
            vec![Node::inner(children)]
        } else if forest_len == 1 {
            let mid = children.len() / 2;
            vec![
                Node::inner(children[..mid].to_vec()),
                Node::inner(children[mid..].to_vec()),
            ]
        } else {
            // The replaced child became two nodes; the natural cut sits
            // exactly at the forest boundary (`idx + 1`).
            let cut = idx + 1;
            vec![
                Node::inner(children[..cut].to_vec()),
                Node::inner(children[cut..].to_vec()),
            ]
        }
    }
}

// ----------------------------------------------------------------------
// DocumentText
// ----------------------------------------------------------------------

/// A cheap, shareable text view over a chunk-tree root (D-10).
///
/// `len`/`line_count` are captured at construction (O(1) reads); slicing
/// and line queries go to the tree.
#[derive(Debug, Clone)]
pub struct DocumentText {
    root: Arc<ChunkRoot>,
    len: u32,
    line_count: u32,
}

impl DocumentText {
    /// Wrap a root.
    #[must_use]
    pub fn new(root: Arc<ChunkRoot>) -> Self {
        let len = u32::try_from(root.len()).expect("document exceeds 4 GiB");
        let line_count = u32::try_from(root.line_count()).expect("document exceeds 4 GiB lines");
        Self {
            root,
            len,
            line_count,
        }
    }

    /// Build from text.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        Self::new(Arc::new(ChunkRoot::from_text(text)))
    }

    /// The wrapped root.
    #[must_use]
    pub fn root(&self) -> &Arc<ChunkRoot> {
        &self.root
    }

    /// Byte length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len as usize
    }

    /// Line count (LF-counted).
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.line_count as usize
    }

    /// Whether empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes `[start, end)`.
    #[must_use]
    pub fn slice(&self, range: Range<usize>) -> String {
        self.root.slice(range)
    }

    /// Line-start offsets within `[start, end)`.
    #[must_use]
    pub fn line_starts_in(&self, range: Range<usize>) -> Vec<usize> {
        self.root.line_starts_in(range)
    }

    /// The line containing byte offset `pos`.
    #[must_use]
    pub fn line_index_of(&self, pos: usize) -> usize {
        self.root.line_index_of(pos)
    }

    /// The byte offset of the start of `line` (clamped to the document end).
    #[must_use]
    pub fn line_start_of(&self, line: usize) -> usize {
        self.root.line_start_of(line)
    }

    /// Whether `pos` is a UTF-8 scalar boundary (O(log n)).
    #[must_use]
    pub fn is_char_boundary(&self, pos: usize) -> bool {
        self.root.is_char_boundary(pos)
    }

    /// Materialize the full text (memoization is the holder's choice).
    #[must_use]
    pub fn materialize(&self) -> Arc<str> {
        Arc::from(self.root.materialize())
    }
}

impl StructureText for DocumentText {
    fn len(&self) -> usize {
        self.len()
    }

    fn line_count(&self) -> usize {
        self.line_count()
    }

    fn line_start(&self, line: usize) -> usize {
        self.root.line_start_of(line)
    }

    fn slice(&self, range: Range<usize>) -> String {
        self.slice(range)
    }
}

/// Semantic equality: identical content, regardless of chunk layout (O(n)
/// materialization; used by tests and user-paced comparisons, never on the
/// keystroke path).
impl PartialEq for DocumentText {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.root, &other.root) || self.materialize() == other.materialize()
    }
}
impl Eq for DocumentText {}

impl From<&str> for DocumentText {
    fn from(value: &str) -> Self {
        Self::from_text(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_document_geometry() {
        let root = ChunkRoot::empty();
        assert_eq!(root.len(), 0);
        assert_eq!(root.line_count(), 1);
        assert_eq!(root.materialize(), "");
        assert_eq!(root.line_start_of(0), 0);
        assert_eq!(root.line_start_of(5), 0);
        assert!(root.leaves().len() == 1);
        assert_eq!(root.slice(0..0), "");
    }

    #[test]
    fn split_at_edges() {
        let root = ChunkRoot::from_text("hello\nworld");
        let (l, r) = root.split_at(0);
        assert_eq!(l.materialize(), "");
        assert_eq!(r.materialize(), "hello\nworld");
        let (l, r) = root.split_at(11);
        assert_eq!(l.materialize(), "hello\nworld");
        assert_eq!(r.materialize(), "");
        let (l, r) = root.split_at(6);
        assert_eq!(l.materialize(), "hello\n");
        assert_eq!(r.materialize(), "world");
    }

    #[test]
    fn edit_basics() {
        let root = ChunkRoot::from_text("hello world");
        let edited = root.edit(5..6, ", ");
        assert_eq!(edited.materialize(), "hello, world");
        assert_eq!(root.materialize(), "hello world", "old root untouched");

        let deleted = edited.edit(5..6, "");
        assert_eq!(deleted.materialize(), "hello world");

        let tail = edited.edit(12..12, "!");
        assert_eq!(tail.materialize(), "hello, world!");
    }

    #[test]
    fn newline_summaries_survive_edits() {
        let mut root = ChunkRoot::from_text("a\nb\nc");
        assert_eq!(root.line_count(), 3);
        for (range, repl, expect) in [
            (0..1, "x", "x\nb\nc"),
            (1..2, "y", "xyb\nc"),
            (4..5, "z", "xyb\nz"),
            (5..5, "\n", "xyb\nz\n"),
        ] {
            root = root.edit(range, repl);
            assert_eq!(root.materialize(), expect);
            assert_eq!(root.len(), expect.len());
        }
        assert_eq!(root.line_count(), 3);
        assert_eq!(root.line_index_of(6), 2);
        assert_eq!(root.line_start_of(1), 4);
    }
}
