//! [`GraphModel`] — the CPU-authoritative graph state (D15).
//!
//! The model holds the node set and the edge set as sorted sets and maintains
//! a CSR adjacency cache (`offsets: Vec<u32>` + `indices: Vec<u32>` + degree
//! counts) rebuilt on every effective batch — simple and correct; optimize
//! later if profiling demands (PLAN-B §5). [`GraphModel::snapshot`] produces
//! the owned, plain-data view the layout worker (B3) and the renderer (B6)
//! consume. The GPU pipeline is recreated from the model on device loss, so
//! the model — never the GPU — is the authority (§4.9).

use std::collections::{BTreeSet, HashMap};

use crate::delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId, NodeKind};

/// Cast a count to `u32`.
///
/// CSR arrays are `u32` (D15: `offsets: Vec<u32>` + `indices: Vec<u32>`). A
/// graph with ≥ 2^32 live entities cannot exist in memory — each node
/// occupies ≥ 16 B in the node set, so 2^32 nodes alone need ≥ 64 GiB —
/// which makes the bound physical, not contractual.
fn count_u32(n: usize) -> u32 {
    u32::try_from(n).expect("graph entity count exceeds u32::MAX (≥ 64 GiB of nodes)")
}

/// Statistics about a [`GraphModel::apply`] batch: the *effective* changes
/// (no-ops excluded), counted per delta — an entity added and removed within
/// one batch counts in both the added and the removed counter.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ApplyStats {
    /// Nodes actually added.
    pub nodes_added: u32,
    /// Nodes actually removed (edges cascaded by a removal are counted in
    /// `edges_removed`).
    pub nodes_removed: u32,
    /// Edges actually added.
    pub edges_added: u32,
    /// Edges actually removed (including edges cascaded by a node removal).
    pub edges_removed: u32,
    /// Whether at least one delta in the batch was effective — i.e. whether
    /// [`GraphModel::topology_version`] bumped.
    pub changed: bool,
}

/// The CPU-authoritative graph model (D15).
///
/// - **Authority:** the node and edge sets are the truth; the CSR cache is
///   derived and rebuilt after every effective batch, so it is always
///   consistent with the sets.
/// - **Determinism:** both sets are sorted, and dense indices are assigned in
///   sorted [`NodeId`] order — the same sets always produce the same CSR
///   layout and the same snapshot.
/// - **Versioning:** `topology_version` bumps exactly when a batch changes
///   the node or edge set; the layout worker wakes on it (D15).
/// - **Idempotence (D5):** duplicate adds/removes and edges with absent
///   endpoints are no-ops; node removal cascades incident edges (§4.3 FK
///   semantics).
#[derive(Debug, Clone)]
pub struct GraphModel {
    /// Authoritative node set (sorted).
    nodes: BTreeSet<NodeId>,
    /// Authoritative edge set: `(from, to, kind)` triples (sorted).
    edges: BTreeSet<(NodeId, NodeId, EdgeKind)>,
    /// Bumps on every batch that changed the node or edge set.
    topology_version: u64,
    /// CSR cache — consistent with the sets after every effective `apply`.
    node_ids: Vec<NodeId>, // dense → external (sorted by NodeId)
    dense_of: HashMap<NodeId, u32>, // external → dense
    offsets: Vec<u32>,              // out-adjacency CSR; len = node_ids.len() + 1
    indices: Vec<u32>,              // out-adjacency targets (dense)
    edge_kinds: Vec<EdgeKind>,      // edge kind per out-edge, parallel to indices
    in_degree: Vec<u32>,            // dense → in-edge row count
}

impl Default for GraphModel {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphModel {
    /// A new, empty model.
    ///
    /// The CSR cache is built eagerly so the "cache is consistent with the
    /// sets" invariant holds from construction.
    #[must_use]
    pub fn new() -> Self {
        let mut model = Self {
            nodes: BTreeSet::new(),
            edges: BTreeSet::new(),
            topology_version: 0,
            node_ids: Vec::new(),
            dense_of: HashMap::new(),
            offsets: Vec::new(),
            indices: Vec::new(),
            edge_kinds: Vec::new(),
            in_degree: Vec::new(),
        };
        model.rebuild_csr();
        model
    }

    /// Apply a batch of deltas (D5: reconcile by idempotence).
    ///
    /// Returns the effective changes, counted **per delta**: an entity added
    /// and removed within one batch counts in both `edges_added` and
    /// `edges_removed` (the index feed diffs consecutive snapshots and never
    /// produces that, §4.14). `stats.changed` is whether at least one delta
    /// was effective; only then does `topology_version` bump (and the CSR
    /// cache rebuild).
    pub fn apply(&mut self, deltas: &[Delta]) -> ApplyStats {
        let mut stats = ApplyStats::default();
        let mut changed = false;
        for &delta in deltas {
            match delta {
                Delta::Node(NodeDelta::Added { id }) => {
                    if self.nodes.insert(id) {
                        changed = true;
                        stats.nodes_added += 1;
                    }
                }
                Delta::Node(NodeDelta::Removed { id }) => {
                    if self.nodes.remove(&id) {
                        changed = true;
                        stats.nodes_removed += 1;
                        // Cascade: an edge needs both endpoints alive (§4.3).
                        let before = self.edges.len();
                        self.edges.retain(|&(from, to, _)| from != id && to != id);
                        stats.edges_removed += count_u32(before - self.edges.len());
                    }
                }
                Delta::Edge(EdgeDelta::Added { from, to, kind }) => {
                    // An edge requires both endpoints. A delta naming a
                    // vanished endpoint is a stale hint, not an error.
                    if self.nodes.contains(&from)
                        && self.nodes.contains(&to)
                        && self.edges.insert((from, to, kind))
                    {
                        changed = true;
                        stats.edges_added += 1;
                    }
                }
                Delta::Edge(EdgeDelta::Removed { from, to, kind }) => {
                    if self.edges.remove(&(from, to, kind)) {
                        changed = true;
                        stats.edges_removed += 1;
                    }
                }
            }
        }
        if changed {
            self.topology_version += 1;
            self.rebuild_csr();
        }
        stats.changed = changed;
        stats
    }

    /// The topology version — bumps on every structural change; the layout
    /// worker wakes on it (D15).
    #[must_use]
    pub fn topology_version(&self) -> u64 {
        self.topology_version
    }

    /// Number of live nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of live edges (link + embed rows; §4.3 `links`).
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Whether the graph has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Whether `id` is a live node.
    #[must_use]
    pub fn contains_node(&self, id: NodeId) -> bool {
        self.nodes.contains(&id)
    }

    /// The kind of a live node (the kind is part of [`NodeId`]).
    #[must_use]
    pub fn node_kind(&self, id: NodeId) -> Option<NodeKind> {
        self.nodes.contains(&id).then_some(id.kind)
    }

    /// Whether the `(from, to, kind)` edge exists.
    #[must_use]
    pub fn contains_edge(&self, from: NodeId, to: NodeId, kind: EdgeKind) -> bool {
        self.edges.contains(&(from, to, kind))
    }

    /// Live nodes in deterministic (sorted) order.
    pub fn nodes(&self) -> impl Iterator<Item = NodeId> {
        self.nodes.iter().copied()
    }

    /// Live edges as `(from, to, kind)` triples in deterministic order.
    pub fn edges(&self) -> impl Iterator<Item = (NodeId, NodeId, EdgeKind)> {
        self.edges.iter().copied()
    }

    /// Total degree (in + out edge rows) of a live node.
    ///
    /// Degrees count edge rows: a pair that is both linked and embedded
    /// contributes 2 (§4.3 `links` rows).
    #[must_use]
    pub fn degree(&self, id: NodeId) -> Option<u32> {
        let dense = *self.dense_of.get(&id)?;
        Some(self.in_degree[dense as usize] + self.out_degree_dense(dense))
    }

    /// In-edge row count of a live node.
    #[must_use]
    pub fn in_degree(&self, id: NodeId) -> Option<u32> {
        self.dense_of
            .get(&id)
            .map(|&dense| self.in_degree[dense as usize])
    }

    /// Out-edge row count of a live node.
    #[must_use]
    pub fn out_degree(&self, id: NodeId) -> Option<u32> {
        self.dense_of
            .get(&id)
            .map(|&dense| self.out_degree_dense(dense))
    }

    /// An owned, consistent snapshot of the whole graph for the layout
    /// worker (B3) and the renderer (B6).
    ///
    /// Plain data, safe to send across threads: the render thread swaps an
    /// `Arc` to it and never waits on layout (D15). Dense indices are
    /// positions in `node_ids` (sorted by [`NodeId`]). Position/node-id
    /// mappings from different topology versions may never be combined
    /// (§4.14 — discard stale layout results on a version change).
    ///
    /// The CSR arrays are built in flat `O(n + m)` passes: the in-rows by
    /// counting sort (sources iterate in dense order, so every in-row comes
    /// out sorted with no per-node sort), and the symmetrized union by a
    /// merge of the two sorted rows. The earlier per-node `Vec` buckets
    /// allocated one `Vec` per node — 50 000 allocations on the AC-16 path.
    #[must_use]
    pub fn snapshot(&self) -> GraphSnapshot {
        let n = self.node_ids.len();
        let edge_rows = self.indices.len();

        // In-rows by counting sort: for a fixed target, sources land in
        // ascending dense order because sources iterate in dense order.
        let mut in_offsets = vec![0_u32; n + 1];
        for &dst in &self.indices {
            in_offsets[dst as usize + 1] += 1;
        }
        for i in 0..n {
            in_offsets[i + 1] += in_offsets[i];
        }
        let mut in_indices = vec![0_u32; edge_rows];
        {
            let mut cursor: Vec<u32> = in_offsets[..n].to_vec();
            for (source, window) in self.offsets.windows(2).enumerate() {
                let (start, end) = (window[0] as usize, window[1] as usize);
                for &dst in &self.indices[start..end] {
                    let row = cursor[dst as usize] as usize;
                    in_indices[row] = count_u32(source);
                    cursor[dst as usize] += 1;
                }
            }
        }

        // Row size of the symmetrized union (out ∪ in, deduped) — a stamp
        // walk over both rows, so a self-loop (present in both) counts once.
        let mut row_len = vec![0_u32; n];
        {
            let mut seen = vec![u32::MAX; n];
            for i in 0..n {
                let stamp = count_u32(i);
                let out_row = &self.indices[self.offsets[i] as usize..self.offsets[i + 1] as usize];
                let in_row = &in_indices[in_offsets[i] as usize..in_offsets[i + 1] as usize];
                let mut len = 0_u32;
                for &neighbor in out_row.iter().chain(in_row.iter()) {
                    let slot = neighbor as usize;
                    if seen[slot] != stamp {
                        seen[slot] = stamp;
                        len += 1;
                    }
                }
                row_len[i] = len;
            }
        }
        let mut adj_offsets = Vec::with_capacity(n + 1);
        let mut total = 0_usize;
        adj_offsets.push(0);
        for &len in &row_len {
            total += len as usize;
            adj_offsets.push(count_u32(total));
        }

        // Fill by merging the two sorted rows, dropping duplicates: a
        // self-loop sits in both rows, and parallel kinds (`Link` + `Embed`
        // on one pair) duplicate a target within the out-row. Both rows are
        // sorted, so the merge is the dedup — no per-node sort.
        let mut adj_indices = vec![0_u32; total];
        {
            let mut write = 0_usize;
            for i in 0..n {
                let out_row = &self.indices[self.offsets[i] as usize..self.offsets[i + 1] as usize];
                let in_row = &in_indices[in_offsets[i] as usize..in_offsets[i + 1] as usize];
                let (mut a, mut b) = (0, 0);
                let mut last: Option<u32> = None;
                while a < out_row.len() || b < in_row.len() {
                    let next =
                        if b >= in_row.len() || (a < out_row.len() && out_row[a] <= in_row[b]) {
                            let value = out_row[a];
                            a += 1;
                            value
                        } else {
                            let value = in_row[b];
                            b += 1;
                            value
                        };
                    if last != Some(next) {
                        adj_indices[write] = next;
                        write += 1;
                        last = Some(next);
                    }
                }
            }
            debug_assert_eq!(write, total, "union merge wrote every row");
        }

        let mut degrees = Vec::with_capacity(n);
        for (i, window) in self.offsets.windows(2).enumerate() {
            degrees.push(self.in_degree[i] + (window[1] - window[0]));
        }
        GraphSnapshot {
            topology_version: self.topology_version,
            node_ids: self.node_ids.clone(),
            node_kinds: self.node_ids.iter().map(|id| id.kind).collect(),
            degrees,
            out_offsets: self.offsets.clone(),
            out_indices: self.indices.clone(),
            out_edge_kinds: self.edge_kinds.clone(),
            adj_offsets,
            adj_indices,
        }
    }

    /// Out-edge row count by dense index (CSR offsets difference).
    fn out_degree_dense(&self, dense: u32) -> u32 {
        let i = dense as usize;
        self.offsets[i + 1] - self.offsets[i]
    }

    /// Rebuild the CSR cache from the authoritative sets.
    ///
    /// Invariant: `edges` only contains edges whose endpoints are live
    /// nodes — `apply` only admits edges with both endpoints present and
    /// cascades incident edges on node removal.
    fn rebuild_csr(&mut self) {
        let n = self.nodes.len();
        let mut node_ids = Vec::with_capacity(n);
        let mut dense_of = HashMap::with_capacity(n);
        for (i, &id) in self.nodes.iter().enumerate() {
            dense_of.insert(id, count_u32(i));
            node_ids.push(id);
        }
        let mut offsets = Vec::with_capacity(n + 1);
        let mut indices = Vec::with_capacity(self.edges.len());
        let mut edge_kinds = Vec::with_capacity(self.edges.len());
        let mut in_degree = vec![0u32; n];
        offsets.push(0);
        // Edges iterate in (from, to, kind) order: each node's out-edges are
        // consecutive, in dense order.
        let mut edges = self.edges.iter().peekable();
        for &id in &node_ids {
            while let Some(&&(from, to, kind)) = edges.peek() {
                if from != id {
                    break;
                }
                let _ = edges.next();
                let dst = dense_of[&to];
                indices.push(dst);
                edge_kinds.push(kind);
                in_degree[dst as usize] += 1;
            }
            offsets.push(count_u32(indices.len()));
        }
        debug_assert!(edges.peek().is_none(), "CSR rebuild missed edges");
        self.node_ids = node_ids;
        self.dense_of = dense_of;
        self.offsets = offsets;
        self.indices = indices;
        self.edge_kinds = edge_kinds;
        self.in_degree = in_degree;
    }
}

/// An owned, consistent snapshot of a [`GraphModel`] — plain data.
///
/// Dense indices are positions in `node_ids`. All CSR arrays are `u32`
/// (D15). Layout: `out_offsets[i]..out_offsets[i + 1]` indexes
/// `out_indices`/`out_edge_kinds` for node `i`'s directed out-edges (the
/// renderer draws each edge once, with its kind);
/// `adj_offsets[i]..adj_offsets[i + 1]` indexes `adj_indices` for node `i`'s
/// symmetrized (undirected) neighbor list (the layout worker's input).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSnapshot {
    /// The topology version the snapshot was taken at.
    pub topology_version: u64,
    /// dense → external node identity (sorted by [`NodeId`]).
    pub node_ids: Vec<NodeId>,
    /// dense → node kind, parallel to `node_ids`.
    pub node_kinds: Vec<NodeKind>,
    /// dense → total degree (in + out edge rows), parallel to `node_ids`.
    pub degrees: Vec<u32>,
    /// Out-adjacency CSR offsets; len = `node_ids.len() + 1`.
    pub out_offsets: Vec<u32>,
    /// Out-adjacency targets (dense), parallel to `out_edge_kinds`.
    pub out_indices: Vec<u32>,
    /// Edge kind per out-edge, parallel to `out_indices`.
    pub out_edge_kinds: Vec<EdgeKind>,
    /// Symmetrized adjacency CSR offsets (undirected neighbor lists for
    /// layout); len = `node_ids.len() + 1`.
    pub adj_offsets: Vec<u32>,
    /// Symmetrized adjacency targets (dense).
    pub adj_indices: Vec<u32>,
}

impl GraphSnapshot {
    /// Number of nodes in the snapshot.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.node_ids.len()
    }

    /// Number of directed edges in the snapshot.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.out_indices.len()
    }

    /// Whether the snapshot has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.node_ids.is_empty()
    }

    /// The dense index of a node, via binary search over the sorted
    /// `node_ids` (the layout worker's warm-start mapping, D15).
    #[must_use]
    pub fn node_index(&self, id: NodeId) -> Option<u32> {
        self.node_ids.binary_search(&id).ok().map(count_u32)
    }

    /// Check the CSR invariants every consumer relies on.
    ///
    /// A snapshot is plain data and can arrive malformed — hand-built, or
    /// produced by anything other than [`GraphModel::snapshot`]. This
    /// checks the structural contract: parallel arrays have one entry per
    /// node, both CSR offset arrays are monotone with length `n + 1` and
    /// end exactly at their target arrays' length, and every dense index
    /// is in range. The layout worker runs this at its boundary (§4.2:
    /// nothing a caller can send unwinds into the UI); a snapshot produced
    /// by the model always passes.
    ///
    /// [`GraphModel::snapshot`]: GraphModel::snapshot
    pub fn validate(&self) -> Result<(), String> {
        let n = self.node_count();
        if self.node_kinds.len() != n {
            return Err(format!(
                "node_kinds has {} entries for {n} nodes",
                self.node_kinds.len()
            ));
        }
        if self.degrees.len() != n {
            return Err(format!(
                "degrees has {} entries for {n} nodes",
                self.degrees.len()
            ));
        }
        for (name, offsets, targets) in [
            ("out_offsets", &self.out_offsets, self.out_indices.len()),
            ("adj_offsets", &self.adj_offsets, self.adj_indices.len()),
        ] {
            if offsets.len() != n + 1 {
                return Err(format!(
                    "{name} has {} entries, expected {n} + 1",
                    offsets.len()
                ));
            }
            if offsets.first() != Some(&0) {
                return Err(format!("{name} must start at 0"));
            }
            if let Some(bad) = offsets.windows(2).position(|window| window[1] < window[0]) {
                return Err(format!("{name} is not monotone at row {bad}"));
            }
            let last = offsets.last().copied().unwrap_or(0) as usize;
            if last != targets {
                return Err(format!(
                    "{name} ends at {last}, but its target array holds {targets} entries"
                ));
            }
        }
        if let Some(bad) = self
            .out_indices
            .iter()
            .position(|&index| index as usize >= n)
        {
            return Err(format!(
                "out_indices[{bad}] = {} is out of range",
                self.out_indices[bad]
            ));
        }
        if let Some(bad) = self
            .adj_indices
            .iter()
            .position(|&index| index as usize >= n)
        {
            return Err(format!(
                "adj_indices[{bad}] = {} is out of range",
                self.adj_indices[bad]
            ));
        }
        Ok(())
    }
}

/// Assert the CSR cache invariants (D15: offsets/indices/degree counts).
///
/// Test-only, shared with the property tests.
#[cfg(test)]
pub(crate) fn assert_csr_invariants(model: &GraphModel) {
    let n = model.node_ids.len();
    assert_eq!(model.offsets.len(), n + 1, "offsets len = nodes + 1");
    assert_eq!(
        model.indices.len(),
        model.edges.len(),
        "one CSR entry per edge"
    );
    assert_eq!(model.edge_kinds.len(), model.indices.len());
    assert_eq!(
        model.offsets.last().copied().unwrap_or(0),
        count_u32(model.indices.len())
    );
    let mut prev = 0u32;
    for &off in &model.offsets {
        assert!(off >= prev, "offsets non-decreasing");
        prev = off;
    }
    for &idx in &model.indices {
        assert!((idx as usize) < n, "CSR index in range");
    }
    // dense_of is a bijection onto 0..n.
    assert_eq!(model.dense_of.len(), n);
    for (i, &id) in model.node_ids.iter().enumerate() {
        assert_eq!(model.dense_of[&id], count_u32(i));
    }
    // The dense id list is exactly the node set.
    assert_eq!(
        model.node_ids.iter().copied().collect::<BTreeSet<_>>(),
        model.nodes
    );
    // in_degree counts incoming CSR entries.
    let mut in_degree = vec![0u32; n];
    for &dst in &model.indices {
        in_degree[dst as usize] += 1;
    }
    assert_eq!(in_degree, model.in_degree);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId, NodeKind};
    use crate::spatial::SpatialGrid;
    use std::time::{Duration, Instant};

    fn note(id: u64) -> NodeId {
        NodeId::note(id)
    }

    fn att(id: u64) -> NodeId {
        NodeId::attachment(id)
    }

    fn link(from: NodeId, to: NodeId) -> Delta {
        Delta::Edge(EdgeDelta::Added {
            from,
            to,
            kind: EdgeKind::Link,
        })
    }

    fn embed(from: NodeId, to: NodeId) -> Delta {
        Delta::Edge(EdgeDelta::Added {
            from,
            to,
            kind: EdgeKind::Embed,
        })
    }

    fn add(id: NodeId) -> Delta {
        Delta::Node(NodeDelta::Added { id })
    }

    fn remove(id: NodeId) -> Delta {
        Delta::Node(NodeDelta::Removed { id })
    }

    fn unlink(from: NodeId, to: NodeId) -> Delta {
        Delta::Edge(EdgeDelta::Removed {
            from,
            to,
            kind: EdgeKind::Link,
        })
    }

    #[test]
    fn empty_model() {
        let model = GraphModel::new();
        assert_eq!(model.node_count(), 0);
        assert_eq!(model.edge_count(), 0);
        assert!(model.is_empty());
        assert_eq!(model.topology_version(), 0);
        let snap = model.snapshot();
        assert!(snap.is_empty());
        assert_eq!(snap.node_count(), 0);
        assert_eq!(snap.edge_count(), 0);
        assert_eq!(snap.out_offsets, vec![0]);
        assert_eq!(snap.adj_offsets, vec![0]);
        assert_eq!(snap.degrees, Vec::<u32>::new());
    }

    #[test]
    fn apply_stats_and_version_bump_once_per_effective_batch() {
        let mut model = GraphModel::new();
        let stats = model.apply(&[add(note(1)), add(note(2))]);
        assert_eq!(stats.nodes_added, 2);
        assert!(stats.changed);
        assert_eq!(model.topology_version(), 1);
        // No-op batch: re-adding an existing node changes nothing.
        let stats = model.apply(&[add(note(1))]);
        assert_eq!(stats.nodes_added, 0);
        assert!(!stats.changed);
        assert_eq!(model.topology_version(), 1);
        // Empty batch.
        let stats = model.apply(&[]);
        assert!(!stats.changed);
        assert_eq!(stats, ApplyStats::default());
        assert_eq!(model.topology_version(), 1);
    }

    #[test]
    fn edges_and_degrees() {
        let mut model = GraphModel::new();
        model.apply(&[add(note(1)), add(note(2)), add(note(3))]);
        let stats = model.apply(&[
            link(note(1), note(2)),
            embed(note(1), note(2)),
            link(note(2), note(3)),
        ]);
        assert_eq!(stats.edges_added, 3);
        assert_eq!(model.edge_count(), 3);
        assert_eq!(model.out_degree(note(1)), Some(2));
        assert_eq!(model.in_degree(note(1)), Some(0));
        assert_eq!(model.degree(note(1)), Some(2));
        assert_eq!(model.out_degree(note(2)), Some(1));
        assert_eq!(model.in_degree(note(2)), Some(2));
        assert_eq!(model.degree(note(2)), Some(3));
        assert_eq!(model.degree(note(3)), Some(1));
        assert_eq!(model.degree(note(99)), None);
        assert!(model.contains_edge(note(1), note(2), EdgeKind::Link));
        assert!(model.contains_edge(note(1), note(2), EdgeKind::Embed));
        assert!(!model.contains_edge(note(2), note(1), EdgeKind::Link));
        assert_csr_invariants(&model);
    }

    #[test]
    fn node_removal_cascades_incident_edges() {
        let mut model = GraphModel::new();
        model.apply(&[add(note(1)), add(note(2)), add(note(3)), add(note(4))]);
        model.apply(&[
            link(note(1), note(2)),
            link(note(3), note(2)),
            link(note(2), note(4)),
        ]);
        let stats = model.apply(&[remove(note(2))]);
        assert_eq!(stats.nodes_removed, 1);
        assert_eq!(stats.edges_removed, 3, "inbound + outbound edges cascade");
        assert_eq!(model.node_count(), 3);
        assert_eq!(model.edge_count(), 0);
        assert!(!model.contains_node(note(2)));
        // Removing an absent node is a no-op.
        let stats = model.apply(&[remove(note(2))]);
        assert_eq!(stats.nodes_removed, 0);
        assert!(!stats.changed);
        assert_csr_invariants(&model);
    }

    #[test]
    fn deltas_are_idempotent() {
        let mut model = GraphModel::new();
        model.apply(&[add(note(1)), add(note(2))]);
        let version = model.topology_version();
        // Duplicate add.
        let stats = model.apply(&[link(note(1), note(2))]);
        assert_eq!(stats.edges_added, 1);
        let stats = model.apply(&[link(note(1), note(2))]);
        assert_eq!(stats.edges_added, 0);
        assert!(!stats.changed);
        assert_eq!(model.edge_count(), 1);
        // Duplicate remove.
        model.apply(&[unlink(note(1), note(2))]);
        let stats = model.apply(&[unlink(note(1), note(2))]);
        assert_eq!(stats.edges_removed, 0);
        assert!(!stats.changed);
        assert_eq!(model.edge_count(), 0);
        assert_eq!(
            model.topology_version(),
            version + 2,
            "one bump per effective batch"
        );
    }

    #[test]
    fn edge_with_absent_endpoint_is_ignored() {
        let mut model = GraphModel::new();
        model.apply(&[add(note(1))]);
        let version = model.topology_version();
        // `to` is not in the node set: a stale hint, a no-op (D5).
        let stats = model.apply(&[link(note(1), note(2))]);
        assert_eq!(stats.edges_added, 0);
        assert!(!stats.changed);
        assert_eq!(model.edge_count(), 0);
        assert_eq!(model.topology_version(), version);
        // The delta is not re-evaluated later; the feed re-derives edges
        // from current index rows (§4.14).
        model.apply(&[add(note(2))]);
        assert_eq!(model.edge_count(), 0);
        // A fresh delta now applies.
        model.apply(&[link(note(1), note(2))]);
        assert_eq!(model.edge_count(), 1);
    }

    #[test]
    fn self_loop_counts_in_and_out() {
        let mut model = GraphModel::new();
        model.apply(&[add(note(1))]);
        model.apply(&[link(note(1), note(1))]);
        assert_eq!(model.out_degree(note(1)), Some(1));
        assert_eq!(model.in_degree(note(1)), Some(1));
        assert_eq!(model.degree(note(1)), Some(2));
        assert_csr_invariants(&model);
    }

    #[test]
    fn parallel_kinds_are_distinct_edges() {
        let mut model = GraphModel::new();
        model.apply(&[add(note(1)), add(att(1))]);
        model.apply(&[link(note(1), att(1)), embed(note(1), att(1))]);
        assert_eq!(model.edge_count(), 2);
        model.apply(&[Delta::Edge(EdgeDelta::Removed {
            from: note(1),
            to: att(1),
            kind: EdgeKind::Link,
        })]);
        assert_eq!(model.edge_count(), 1);
        assert!(model.contains_edge(note(1), att(1), EdgeKind::Embed));
        // Note and attachment id spaces are distinct.
        assert_eq!(model.node_kind(note(1)), Some(NodeKind::Note));
        assert_eq!(model.node_kind(att(1)), Some(NodeKind::Attachment));
    }

    #[test]
    fn node_and_edge_iterators_are_sorted() {
        let mut model = GraphModel::new();
        model.apply(&[add(note(3)), add(note(1)), add(note(2))]);
        model.apply(&[link(note(3), note(1)), link(note(1), note(2))]);
        let ids: Vec<_> = model.nodes().collect();
        assert_eq!(ids, vec![note(1), note(2), note(3)]);
        let edges: Vec<_> = model.edges().collect();
        assert_eq!(
            edges,
            vec![
                (note(1), note(2), EdgeKind::Link),
                (note(3), note(1), EdgeKind::Link)
            ]
        );
    }

    #[test]
    fn snapshot_matches_model() {
        // Hand-computed: notes 1,2,3; edges 1→2 (link), 1→2 (embed), 2→3 (link).
        let mut model = GraphModel::new();
        model.apply(&[add(note(1)), add(note(2)), add(note(3))]);
        model.apply(&[
            link(note(1), note(2)),
            embed(note(1), note(2)),
            link(note(2), note(3)),
        ]);
        let snap = model.snapshot();
        assert_eq!(snap.topology_version, model.topology_version());
        assert_eq!(snap.node_ids, vec![note(1), note(2), note(3)]);
        assert_eq!(snap.node_kinds, vec![NodeKind::Note; 3]);
        assert_eq!(snap.degrees, vec![2, 3, 1]);
        // Out-CSR: node 1 → dense 1 twice (link + embed), node 2 → dense 2.
        assert_eq!(snap.out_offsets, vec![0, 2, 3, 3]);
        assert_eq!(snap.out_indices, vec![1, 1, 2]);
        assert_eq!(
            snap.out_edge_kinds,
            vec![EdgeKind::Link, EdgeKind::Embed, EdgeKind::Link]
        );
        // Symmetrized adjacency: 1 → {2}, 2 → {1, 3}, 3 → {2}.
        assert_eq!(snap.adj_offsets, vec![0, 1, 3, 4]);
        assert_eq!(snap.adj_indices, vec![1, 0, 2, 1]);
        assert_eq!(snap.node_count(), 3);
        assert_eq!(snap.edge_count(), 3);
        assert!(!snap.is_empty());
        assert_eq!(snap.node_index(note(2)), Some(1));
        assert_eq!(snap.node_index(note(9)), None);
    }

    #[test]
    fn snapshot_is_deterministic_regardless_of_insertion_order() {
        // Same final sets, different insertion orders → identical snapshots
        // (both models applied exactly two effective batches).
        let mut a = GraphModel::new();
        a.apply(&[add(note(2)), add(note(1)), add(note(3))]);
        a.apply(&[link(note(2), note(1)), link(note(1), note(3))]);
        let mut b = GraphModel::new();
        b.apply(&[add(note(1)), add(note(3)), add(note(2))]);
        b.apply(&[link(note(1), note(3)), link(note(2), note(1))]);
        assert_eq!(a.snapshot(), b.snapshot());
    }

    #[test]
    fn csr_invariants_hold_after_mixed_operations() {
        let mut model = GraphModel::new();
        model.apply(&[
            add(note(5)),
            add(note(1)),
            add(att(2)),
            link(note(5), note(1)),
            embed(note(5), att(2)),
            link(note(1), note(5)),
            link(note(1), note(1)),
        ]);
        assert_csr_invariants(&model);
        model.apply(&[remove(note(5))]);
        assert_csr_invariants(&model);
        assert_eq!(model.edge_count(), 1, "only the self-loop survives");
        model.apply(&[remove(note(1))]);
        assert_csr_invariants(&model);
        assert_eq!(model.edge_count(), 0);
        assert_eq!(model.node_count(), 1);
    }

    #[test]
    fn model_snapshot_and_grid_are_send_sync() {
        // The render thread swaps an `Arc` snapshot; the worker sends them
        // across threads (D15 — no locks on the frame path).
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GraphModel>();
        assert_send_sync::<GraphSnapshot>();
        assert_send_sync::<SpatialGrid>();
    }

    #[test]
    fn validate_accepts_model_snapshots_and_rejects_corruption() {
        // Every snapshot the model produces is valid by construction.
        let mut model = GraphModel::new();
        model.apply(&[
            add(note(1)),
            add(note(2)),
            add(note(3)),
            link(note(1), note(2)),
            link(note(2), note(3)),
            link(note(3), note(3)),
        ]);
        let snap = model.snapshot();
        assert_eq!(snap.validate(), Ok(()));
        let empty = GraphModel::new().snapshot();
        assert_eq!(empty.validate(), Ok(()));

        // degrees truncated.
        let mut corrupt = snap.clone();
        corrupt.degrees = Vec::new();
        assert!(corrupt.validate().is_err(), "truncated degrees rejected");

        // adjacency offsets truncated (the §4.2 test's malformed shape).
        let mut corrupt = snap.clone();
        corrupt.adj_offsets = vec![0];
        assert!(
            corrupt.validate().is_err(),
            "truncated adj_offsets rejected"
        );

        // offsets that do not end at their target array.
        let mut corrupt = snap.clone();
        corrupt.out_offsets.push(99);
        assert!(corrupt.validate().is_err(), "bad out_offsets rejected");

        // non-monotone offsets.
        let mut corrupt = snap.clone();
        corrupt.adj_offsets[2] = 0;
        assert!(corrupt.validate().is_err(), "non-monotone offsets rejected");

        // out-of-range dense indices.
        let mut corrupt = snap.clone();
        corrupt.adj_indices[0] = 99;
        assert!(
            corrupt.validate().is_err(),
            "out-of-range adj index rejected"
        );
        let mut corrupt = snap;
        corrupt.out_indices[0] = 99;
        assert!(
            corrupt.validate().is_err(),
            "out-of-range out index rejected"
        );
    }

    #[test]
    fn fifty_k_node_build_smoke() {
        // AC-16 scale smoke: 50k nodes / 200k edges must build in < 1 s.
        // Edge pattern: (i → (i + k) % n) for k in 1..=4 — exactly 4n
        // distinct edges, no self-loops, no duplicates.
        const N: u64 = 50_000;
        let start = Instant::now();
        let mut model = GraphModel::new();
        let nodes: Vec<Delta> = (0..N).map(|i| add(note(i))).collect();
        let stats = model.apply(&nodes);
        assert_eq!(stats.nodes_added, count_u32(N as usize));
        let mut edges = Vec::with_capacity((4 * N) as usize);
        for k in 1..=4u64 {
            for i in 0..N {
                edges.push(link(note(i), note((i + k) % N)));
            }
        }
        // Apply in batches (the feed's shape, B5).
        for batch in edges.chunks(50_000) {
            model.apply(batch);
        }
        let elapsed = start.elapsed();
        assert_eq!(model.node_count(), N as usize);
        assert_eq!(model.edge_count(), (4 * N) as usize);
        assert_csr_invariants(&model);
        let snap = model.snapshot();
        assert_eq!(snap.node_count(), N as usize);
        assert_eq!(snap.edge_count(), (4 * N) as usize);
        // Ring structure: every node has 4 out-edges and 4 in-edges.
        assert_eq!(model.degree(note(0)), Some(8));
        assert_eq!(model.degree(note(12_345)), Some(8));
        // LLVM source instrumentation adds substantial per-line overhead, so
        // keep the 1 s performance gate for normal runs and allow the measured
        // operation to complete under the coverage runner's slower profile.
        let budget = if std::env::var_os("LLVM_PROFILE_FILE").is_some() {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(1)
        };
        assert!(
            elapsed < budget,
            "50k-node build took {elapsed:?} (budget: {budget:?})"
        );
    }
}
