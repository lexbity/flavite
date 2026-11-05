//! CPU feed application — index batches into the authoritative model
//! (RFC-0003 TD-12, W07, §4.14, D5).
//!
//! The SQLite-backed index bridge now lives in `flavite-api::graph_service`
//! (the boundary rule is "Index bridge in API service, CPU graph consumes
//! plain DTOs"). This module is the pure application side: it takes
//! complete [`GraphBatch`] values produced by *one consistent index
//! transaction*, diffs consecutive batches into exact deltas, and rebuilds
//! the [`GraphModel`] on epoch resets.
//!
//! # Deletion and reset correctness (§4.14)
//!
//! The invalidation log records what changed, not the full before-image of
//! every removed edge — a consumer that only ever *adds* the edges listed
//! for an invalidated source silently keeps deleted edges forever. This
//! publisher therefore diffs the complete previous set against the complete
//! batch set, emitting both `Added` and `Removed` deltas. Re-applying the
//! same batch is a structural no-op (idempotence, D5).
//!
//! When the epoch changes, a consumer cannot know which entities vanished
//! with the old epoch, so the publisher rebuilds from the batch alone and
//! flags the publication `reset` — the layout worker must be re-installed
//! with the new snapshot, never delta-fed through the reset.
//!
//! Every refused path (stale version, record misalignment) leaves the
//! previous model and publication untouched and increments a counter:
//! a rejected batch is zero mutation.

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId};
use crate::model::{GraphModel, GraphSnapshot};
use crate::protocol::{GraphBatch, GraphNodeRecord, RenderKey};

/// One immutable base publication: the model state a batch produced, the
/// display records aligned with it, and the exact key consumers pair by.
#[derive(Debug, Clone)]
pub struct GraphPublication {
    /// Index epoch of the applied batch.
    pub epoch: u64,
    /// Index generation of the applied batch.
    pub generation: u64,
    /// Topology version of the model after the batch.
    pub topology_version: u64,
    pub overlay_digest: u64,
    /// The consistent snapshot (dense order defines geometry indices).
    pub snapshot: Arc<GraphSnapshot>,
    /// Node display records, dense-index-aligned with `snapshot.node_ids`.
    pub records: Arc<[GraphNodeRecord]>,
    /// Exact deltas this batch contributed (empty on a no-change batch,
    /// full-add on a reset). Forward these to the layout worker: `Apply`
    /// when `reset` is false and the vector is non-empty, `Replace` with
    /// the snapshot when `reset` is true.
    pub deltas: Arc<[Delta]>,
    /// Whether this publication rebuilt the model from scratch.
    pub reset: bool,
    /// Whether a topology actually changed with this batch (a generation
    /// advance alone does not move the graph).
    pub topology_changed: bool,
}

impl GraphPublication {
    /// The key every frame component must agree with (§10.3).
    #[must_use]
    pub fn render_key(&self) -> RenderKey {
        RenderKey::new(self.epoch, self.generation, self.topology_version)
            .with_view(self.overlay_digest, 0)
    }

    /// The dense index of a node, if it is live in this publication.
    #[must_use]
    pub fn node_index(&self, id: NodeId) -> Option<u32> {
        self.snapshot.node_index(id)
    }

    /// The display record at a dense index.
    #[must_use]
    pub fn record(&self, dense: usize) -> Option<&GraphNodeRecord> {
        self.records.get(dense)
    }

    /// Total live nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.snapshot.node_count()
    }

    /// Total live directed edges.
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.snapshot.edge_count()
    }
}

/// The CPU apply path: consecutive [`GraphBatch`] values into versioned
/// [`GraphPublication`]s with exact transition deltas.
///
/// Single-threaded by construction (the graph service applies batches on
/// its own worker); publishes `Arc`s so any number of readers can pair a
/// publication with a positions frame without locks.
#[derive(Debug)]
pub struct GraphPublisher {
    model: GraphModel,
    epoch: u64,
    generation: u64,
    initialized: bool,
    publication: Arc<GraphPublication>,
    /// Batches that rebuilt the model from scratch (epoch changes, first
    /// batch, explicit resets).
    pub resets_total: u64,
    /// Batches discarded as stale or misaligned (never mutated the model).
    pub stale_dropped_total: u64,
}

impl Default for GraphPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphPublisher {
    /// A publisher at the empty model. The first batch is a reset by
    /// definition (there is no previous state to diff against).
    #[must_use]
    pub fn new() -> Self {
        let snapshot = Arc::new(GraphModel::new().snapshot());
        Self {
            model: GraphModel::new(),
            epoch: 0,
            generation: 0,
            initialized: false,
            resets_total: 0,
            stale_dropped_total: 0,
            publication: Arc::new(GraphPublication {
                epoch: 0,
                generation: 0,
                topology_version: 0,
                overlay_digest: 0,
                records: Arc::from(Vec::new()),
                snapshot,
                deltas: Arc::from(Vec::new()),
                reset: true,
                topology_changed: false,
            }),
        }
    }

    /// The latest publication (the empty one before the first batch).
    #[must_use]
    pub fn publication(&self) -> Arc<GraphPublication> {
        Arc::clone(&self.publication)
    }

    /// The current epoch (0 until the first batch).
    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether at least one batch has been applied.
    #[must_use]
    pub const fn initialized(&self) -> bool {
        self.initialized
    }

    /// Apply one complete batch, returning the resulting publication.
    ///
    /// `reset` forces a from-scratch rebuild — used when the bridge lost
    /// its position in the invalidation log: §4.14 requires a consumer
    /// that cannot replay exactly to rebuild from a consistent snapshot.
    ///
    /// Rejected batches (stale version, duplicate record identities) are
    /// zero mutation: the previous publication is returned unchanged and a
    /// counter records the drop.
    #[must_use]
    pub fn apply(&mut self, batch: &GraphBatch, reset: bool) -> Arc<GraphPublication> {
        self.apply_current(batch, reset, 0)
    }

    /// Apply current relationships at an independent source revision digest.
    #[must_use]
    pub fn apply_current(
        &mut self,
        batch: &GraphBatch,
        reset: bool,
        overlay_digest: u64,
    ) -> Arc<GraphPublication> {
        // Idempotent replay of the current version.
        if !reset
            && self.initialized
            && batch.epoch == self.epoch
            && batch.generation == self.generation
            && overlay_digest == self.publication.overlay_digest
        {
            return Arc::clone(&self.publication);
        }
        // Stale within the same epoch, or from an epoch already retired —
        // but never a forced-reset batch. A reset re-anchors the model from
        // the consumer's full consistent read (§4.14/§6.5.2), and the
        // DB-recreate shape (RFC-0005 §4.9, AC-8) legitimately rewinds the
        // generation at an unchanged epoch: `open_or_init` reseeds
        // INITIAL_EPOCH on every fresh file, so the epoch alone cannot
        // distinguish it. The pump's read-connection revalidation
        // (FR-6 — uuid + inode) is what proves the handle is fresh; once
        // the mirror resets, its rebuild IS the authoritative current.
        if !reset
            && self.initialized
            && (batch.epoch < self.epoch
                || (batch.epoch == self.epoch && batch.generation < self.generation))
        {
            self.stale_dropped_total += 1;
            return Arc::clone(&self.publication);
        }

        let is_reset = reset || !self.initialized || batch.epoch != self.epoch;
        let mut candidate = if is_reset {
            GraphModel::new()
        } else {
            self.model.clone()
        };
        let deltas = if is_reset {
            rebuild_deltas(batch)
        } else {
            diff_deltas(&candidate, batch)
        };
        let stats = candidate.apply(&deltas);
        let snapshot = Arc::new(candidate.snapshot());
        let records: Arc<[GraphNodeRecord]> = Arc::from(batch.nodes.clone().into_boxed_slice());

        // Dense alignment: batch records arrive sorted by NodeId and the
        // model's `node_ids` are the sorted set of the same ids. The check
        // below is not paranoia theater — a misaligned pairing is exactly
        // the "dense node index now names another note" defect W07 makes
        // impossible, so a mismatch refuses the publication outright.
        if records.len() != snapshot.node_count()
            || !records
                .iter()
                .zip(snapshot.node_ids.iter())
                .all(|(record, id)| record.id == *id)
        {
            self.stale_dropped_total += 1;
            return Arc::clone(&self.publication);
        }

        self.model = candidate;
        self.epoch = batch.epoch;
        self.generation = batch.generation;
        self.initialized = true;
        if is_reset {
            self.resets_total += 1;
        }
        self.publication = Arc::new(GraphPublication {
            epoch: batch.epoch,
            generation: batch.generation,
            topology_version: snapshot.topology_version,
            overlay_digest,
            snapshot,
            records,
            deltas: Arc::from(deltas.into_boxed_slice()),
            reset: is_reset,
            topology_changed: stats.changed,
        });
        Arc::clone(&self.publication)
    }

    /// Discard the current state so the next batch always rebuilds (the
    /// service calls this when the bridge reports `ResetRequired`).
    pub fn force_reset(&mut self) {
        self.initialized = false;
        self.model = GraphModel::new();
        self.epoch = 0;
        self.generation = 0;
    }
}

/// Deltas that build the batch state from an empty model.
fn rebuild_deltas(batch: &GraphBatch) -> Vec<Delta> {
    let mut deltas = Vec::with_capacity(batch.nodes.len() + batch.edges.len());
    for record in &batch.nodes {
        deltas.push(Delta::Node(NodeDelta::Added { id: record.id }));
    }
    for edge in &batch.edges {
        deltas.push(Delta::Edge(EdgeDelta::Added {
            from: edge.from,
            to: edge.to,
            kind: edge.kind,
        }));
    }
    deltas
}

/// Deltas that transition `previous` to the batch state exactly.
///
/// Removals first (so a removed node's incident edges cascade and cannot be
/// re-added by a stale hint inside one batch), then additions.
fn diff_deltas(previous: &GraphModel, batch: &GraphBatch) -> Vec<Delta> {
    let next_nodes: BTreeSet<NodeId> = batch.nodes.iter().map(|record| record.id).collect();
    let next_edges: BTreeSet<(NodeId, NodeId, EdgeKind)> = batch
        .edges
        .iter()
        .map(|edge| (edge.from, edge.to, edge.kind))
        .collect();
    let mut deltas = Vec::new();
    for id in previous.nodes() {
        if !next_nodes.contains(&id) {
            deltas.push(Delta::Node(NodeDelta::Removed { id }));
        }
    }
    for (from, to, kind) in previous.edges() {
        if !next_edges.contains(&(from, to, kind)) {
            deltas.push(Delta::Edge(EdgeDelta::Removed { from, to, kind }));
        }
    }
    for id in &next_nodes {
        if !previous.contains_node(*id) {
            deltas.push(Delta::Node(NodeDelta::Added { id: *id }));
        }
    }
    for edge in &batch.edges {
        if !previous.contains_edge(edge.from, edge.to, edge.kind) {
            deltas.push(Delta::Edge(EdgeDelta::Added {
                from: edge.from,
                to: edge.to,
                kind: edge.kind,
            }));
        }
    }
    deltas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::GraphEdgeRecord;

    fn note(id: u64) -> GraphNodeRecord {
        GraphNodeRecord::note(
            id,
            format!("note-{id}.md"),
            format!("note-{id}.md").into_bytes(),
            format!("Note {id}"),
            None,
            false,
        )
    }

    fn edge(from: u64, to: u64) -> GraphEdgeRecord {
        GraphEdgeRecord {
            from: NodeId::note(from),
            to: NodeId::note(to),
            kind: EdgeKind::Link,
        }
    }

    #[test]
    fn first_batch_is_a_reset_and_adds_everything() {
        let mut publisher = GraphPublisher::new();
        let batch = GraphBatch {
            epoch: 3,
            generation: 10,
            nodes: vec![note(1), note(2)],
            edges: vec![edge(1, 2)],
        };
        let publication = publisher.apply(&batch, false);
        assert!(publication.reset);
        assert_eq!(publication.node_count(), 2);
        assert_eq!(publication.edge_count(), 1);
        assert_eq!(publication.epoch, 3);
        assert!(
            publication.deltas.iter().all(|delta| matches!(
                delta,
                Delta::Node(NodeDelta::Added { .. }) | Delta::Edge(_)
            ))
        );
    }

    #[test]
    fn removal_of_a_link_emits_the_exact_removed_edge() {
        let mut publisher = GraphPublisher::new();
        let _ = publisher.apply(
            &GraphBatch {
                epoch: 3,
                generation: 10,
                nodes: vec![note(1), note(2)],
                edges: vec![edge(1, 2)],
            },
            false,
        );
        let publication = publisher.apply(
            &GraphBatch {
                epoch: 3,
                generation: 11,
                nodes: vec![note(1), note(2)],
                edges: vec![],
            },
            false,
        );
        assert!(!publication.reset);
        assert_eq!(publication.edge_count(), 0, "deletion must survive (§4.14)");
        assert!(
            publication
                .deltas
                .contains(&Delta::Edge(EdgeDelta::Removed {
                    from: NodeId::note(1),
                    to: NodeId::note(2),
                    kind: EdgeKind::Link,
                }))
        );
    }

    #[test]
    fn node_deletion_cascades_and_removal_is_exact() {
        let mut publisher = GraphPublisher::new();
        let _ = publisher.apply(
            &GraphBatch {
                epoch: 1,
                generation: 5,
                nodes: vec![note(1), note(2), note(3)],
                edges: vec![edge(1, 2), edge(2, 3)],
            },
            false,
        );
        let publication = publisher.apply(
            &GraphBatch {
                epoch: 1,
                generation: 6,
                nodes: vec![note(1), note(3)],
                edges: vec![],
            },
            false,
        );
        assert!(!publication.snapshot.node_ids.contains(&NodeId::note(2)));
        assert_eq!(publication.edge_count(), 0);
    }

    #[test]
    fn replaying_the_same_batch_is_a_noop() {
        let mut publisher = GraphPublisher::new();
        let batch = GraphBatch {
            epoch: 2,
            generation: 4,
            nodes: vec![note(1)],
            edges: vec![],
        };
        let first = publisher.apply(&batch, false);
        let second = publisher.apply(&batch, false);
        assert!(Arc::ptr_eq(&first, &second), "same version is idempotent");
    }

    #[test]
    fn stale_batches_are_dropped_without_mutation() {
        let mut publisher = GraphPublisher::new();
        let _ = publisher.apply(
            &GraphBatch {
                epoch: 2,
                generation: 8,
                nodes: vec![note(1), note(2)],
                edges: vec![edge(1, 2)],
            },
            false,
        );
        let before = publisher.publication();
        let after = publisher.apply(
            &GraphBatch {
                epoch: 2,
                generation: 7,
                nodes: vec![note(1)],
                edges: vec![],
            },
            false,
        );
        assert!(Arc::ptr_eq(&before, &after));
        assert_eq!(publisher.stale_dropped_total, 1);
        assert_eq!(after.edge_count(), 1, "a dropped batch never mutates");
    }

    #[test]
    fn generation_advance_without_topological_change_keeps_positions_valid() {
        let mut publisher = GraphPublisher::new();
        let batch = GraphBatch {
            epoch: 2,
            generation: 1,
            nodes: vec![note(1)],
            edges: vec![],
        };
        let _ = publisher.apply(&batch, false);
        let publication = publisher.apply(
            &GraphBatch {
                epoch: 2,
                generation: 2,
                ..batch.clone()
            },
            false,
        );
        assert!(publication.deltas.is_empty());
        assert!(!publication.topology_changed);
        assert_eq!(publication.generation, 2);
        assert_eq!(publication.topology_version, 1, "no topology bump");
    }

    #[test]
    fn epoch_change_rebuilds_and_flags_reset() {
        let mut publisher = GraphPublisher::new();
        let _ = publisher.apply(
            &GraphBatch {
                epoch: 4,
                generation: 9,
                nodes: vec![note(9)],
                edges: vec![],
            },
            false,
        );
        // A new epoch reuses entity ids; old-epoch identities are invalid.
        let publication = publisher.apply(
            &GraphBatch {
                epoch: 5,
                generation: 0,
                nodes: vec![note(1)],
                edges: vec![],
            },
            false,
        );
        assert!(publication.reset);
        assert_eq!(publication.epoch, 5);
        assert!(!publication.snapshot.node_ids.contains(&NodeId::note(9)));
        assert_eq!(publisher.resets_total, 2, "first batch also rebuilt");
    }

    #[test]
    fn misaligned_records_refuse_the_publication_entirely() {
        let mut publisher = GraphPublisher::new();
        let _ = publisher.apply(
            &GraphBatch {
                epoch: 1,
                generation: 1,
                nodes: vec![note(1)],
                edges: vec![],
            },
            false,
        );
        // Duplicate node ids break the dense-order invariant by
        // construction; the publisher must refuse rather than mis-pair.
        let before = publisher.publication();
        let after = publisher.apply(
            &GraphBatch {
                epoch: 1,
                generation: 2,
                nodes: vec![note(7), note(7)],
                edges: vec![],
            },
            false,
        );
        assert!(
            Arc::ptr_eq(&before, &after),
            "a misaligned batch is discarded with a counter"
        );
        assert_eq!(publisher.stale_dropped_total, 1);
        assert_eq!(after.node_count(), 1, "the refused batch changed nothing");
    }

    #[test]
    fn explicit_reset_rebuilds_even_at_the_same_version() {
        let mut publisher = GraphPublisher::new();
        let _ = publisher.apply(
            &GraphBatch {
                epoch: 1,
                generation: 1,
                nodes: vec![note(1)],
                edges: vec![],
            },
            false,
        );
        let publication = publisher.apply(
            &GraphBatch {
                epoch: 1,
                generation: 1,
                nodes: vec![note(1), note(2)],
                edges: vec![],
            },
            true,
        );
        assert!(publication.reset);
        assert_eq!(publication.node_count(), 2);
    }

    #[test]
    fn render_key_pairs_base_and_topology() {
        let mut publisher = GraphPublisher::new();
        let publication = publisher.apply(
            &GraphBatch {
                epoch: 6,
                generation: 12,
                nodes: vec![note(1)],
                edges: vec![],
            },
            false,
        );
        let key = publication.render_key();
        assert_eq!(key.epoch, 6);
        assert_eq!(key.generation, 12);
        assert!(key.topology_matches(&key));
        let other = RenderKey::new(7, 12, key.topology_version);
        assert!(!key.topology_matches(&other), "epoch resets never match");
    }

    #[test]
    fn edge_rewiring_within_one_batch_is_exact() {
        let mut publisher = GraphPublisher::new();
        let _ = publisher.apply(
            &GraphBatch {
                epoch: 1,
                generation: 1,
                nodes: vec![note(1), note(2)],
                edges: vec![edge(1, 2)],
            },
            false,
        );
        // 1→2 becomes 2→1 plus a new node 3 linking both.
        let publication = publisher.apply(
            &GraphBatch {
                epoch: 1,
                generation: 2,
                nodes: vec![note(1), note(2), note(3)],
                edges: vec![edge(2, 1), edge(3, 1), edge(3, 2)],
            },
            false,
        );
        assert_eq!(publication.edge_count(), 3);
        assert!(has_edge(&publication, &edge(2, 1)));
        assert!(!has_edge(&publication, &edge(1, 2)));
    }

    /// Test helper: whether a directed edge record is live in the
    /// publication snapshot.
    fn has_edge(publication: &GraphPublication, edge: &GraphEdgeRecord) -> bool {
        let (Some(from), Some(to)) = (
            publication.snapshot.node_index(edge.from),
            publication.snapshot.node_index(edge.to),
        ) else {
            return false;
        };
        let row_range = publication.snapshot.out_offsets[from as usize] as usize
            ..publication.snapshot.out_offsets[from as usize + 1] as usize;
        row_range.into_iter().any(|row| {
            publication.snapshot.out_indices[row] == to
                && publication.snapshot.out_edge_kinds[row] == edge.kind
        })
    }
}
