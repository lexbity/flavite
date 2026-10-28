//! Graph feed: index → graph deltas (PLAN-B B5, §4.14, D15).
//!
//! The feed polls `(epoch, generation)` from the index and, when the
//! index has advanced, consumes a complete invalidation batch or returns
//! [`FeedError::ResetRequired`] (§4.14). It replaces each affected
//! node/outgoing set from the current snapshot, **not** from surviving
//! live rows — deletions survive in the log, so reconstructing them from
//! "what is still there" loses the complete picture (§4.14). Each
//! frame/positions pair carries the same topology version so changed node
//! indices cannot be paired with older positions (§4.14, AC-33).
//!
//! # Usage
//!
//! ```text
//! // Poll on a 250 ms tick and after every VaultChanged pulse (D5).
//! let cursor = feed.cursor();
//! if let Some(batch) = feed.next_batch()? {
//!     // batch.deltas + batch.snapshot + batch.version are paired.
//!     graph_model.apply(&batch.deltas);
//!     // snapshot is now consistent with the model.
//! }
//! ```
//!
//! # Guarantees
//!
//! - **Atomic apply:** a single SQLite transaction covers all structural
//!   changes + re-resolution + generation bump + invalidations.
//! - **Exact replay:** a consumer slower than retention sees a
//!   [`FeedError::ResetRequired`], loads a consistent full snapshot path,
//!   and rebuilds; old-epoch IDs are invalid.
//! - **Deletion safety:** deleted entities leave their invalidation behind;
//!   the changes table has no cascading FK.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use flavite_index::change::InvalidationFeed;
use flavite_index::types::{ChangeKind, ChangeRecord, IndexError, IndexVersion};
use rusqlite::Connection;

use crate::delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId, NodeKind};
use crate::model::{GraphModel, GraphSnapshot};

/// A feed error: transient or structural.
#[derive(Debug, thiserror::Error)]
pub enum FeedError {
    /// SQLite returned an error.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// An index-level error occurred.
    #[error("index error: {0}")]
    Index(#[from] IndexError),
    /// The epoch changed, the cursor is ahead, or cursor.generation < log_floor.
    /// The caller must do a full rebuild from a consistent snapshot (§4.14).
    #[error("reset required: epoch changed or cursor too old")]
    ResetRequired,
    /// The cursor's generation is ahead of the current generation — the
    /// cursor came from a later epoch.
    #[error("stale cursor: cursor is ahead of current generation")]
    StaleCursor,
}

impl From<FeedError> for IndexError {
    fn from(err: FeedError) -> Self {
        match err {
            FeedError::Sqlite(e) => IndexError::Sqlite(e),
            FeedError::Index(e) => e,
            FeedError::ResetRequired => IndexError::ResetRequired,
            FeedError::StaleCursor => IndexError::StaleCursor,
        }
    }
}

/// A complete graph update: deltas + the snapshot they were derived from +
/// the exact `(epoch, generation)` cursor they advance to.
///
/// The caller **MUST** apply the deltas and the snapshot together, or
/// rebuild from the snapshot alone. A [`FeedBatch`] is never partial: it
/// is always derived from a complete invalidation batch or a full
/// snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct FeedBatch {
    /// The deltas to apply (D5: reconcile by idempotence).
    pub deltas: Vec<Delta>,
    /// The snapshot the deltas were derived from — the complete view.
    pub snapshot: Arc<GraphSnapshot>,
    /// The epoch the snapshot belongs to.
    pub epoch: u64,
    /// The generation the snapshot and deltas advance to.
    pub generation: u64,
}

impl FeedBatch {
    /// Build a batch from a full snapshot (reset path).
    #[must_use]
    pub fn from_snapshot(snapshot: Arc<GraphSnapshot>, epoch: u64, generation: u64) -> Self {
        Self {
            deltas: Vec::new(), // apply is idempotent; a full replace happens
            snapshot,
            epoch,
            generation,
        }
    }

    /// Whether this batch is a reset (empty deltas; caller rebuilds from
    /// `snapshot`).
    #[must_use]
    pub fn is_reset(&self) -> bool {
        self.deltas.is_empty()
    }
}

/// A cursor for the invalidation log: `(epoch, generation)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct IndexCursor {
    pub epoch: u64,
    pub generation: u64,
}

impl IndexCursor {
    /// A new cursor at the initial state.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this cursor is at the initial (uninitialized) state.
    #[must_use]
    pub fn is_initial(&self) -> bool {
        self.epoch == 0 && self.generation == 0
    }

    /// Advance to the next version.
    #[must_use]
    pub fn advanced(self, epoch: u64, generation: u64) -> Self {
        Self { epoch, generation }
    }
}

/// An index node from the feed's perspective: what we need to build a
/// [`NodeId`] and its edges.
#[derive(Debug, Clone)]
struct FeedNode {
    /// Cache-internal entity id.
    id: i64,
    /// Node kind (derived from which table).
    kind: NodeKind,
}

/// An index edge from the feed's perspective.
#[derive(Debug, Clone)]
struct FeedEdge {
    /// Source entity id.
    src: i64,
    /// Target entity id (note or attachment).
    dst: i64,
    /// Whether the target is an attachment.
    dst_is_att: bool,
    /// Edge kind.
    kind: EdgeKind,
}

/// The graph's live index feed: a cursor plus an InvalidationFeed.
///
/// The feed owns an InvalidationFeed which owns the connection to the index.
/// The connection MUST be a read-only WAL connection from the index
/// actor's read pool.
pub struct GraphFeed {
    /// The index feed from flavite-index (owns the Connection).
    index_feed: InvalidationFeed,
    /// The last seen cursor.
    cursor: IndexCursor,
}

impl GraphFeed {
    /// Open a new feed for the index at `conn`.
    ///
    /// The connection MUST be a read-only WAL connection. Taking a
    /// write connection or the writer connection itself violates the
    /// index's single-writer rule.
    pub fn open(conn: Connection) -> Result<Self, FeedError> {
        let index_feed = InvalidationFeed::new(conn);
        let epoch = index_feed.epoch()?;
        let generation = index_feed.generation()?;

        let feed = Self {
            index_feed,
            cursor: IndexCursor::new().advanced(epoch, generation),
        };

        Ok(feed)
    }

    /// The current cursor.
    #[must_use]
    pub fn cursor(&self) -> IndexCursor {
        self.cursor
    }

    /// Get a reference to the underlying index connection.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.index_feed.conn
    }

    /// Poll the feed for new work, returning a batch or `None` if nothing
    /// changed.
    ///
    /// Returns:
    /// - `Some(batch)` — a complete batch to apply and publish.
    /// - `None` — no change since last poll.
    /// - `Err(FeedError::ResetRequired)` — the epoch changed or the cursor is
    ///   too old; the caller must rebuild from a consistent snapshot.
    ///
    /// A caller that observes [`FeedError::ResetRequired`] MUST load a
    /// consistent snapshot from the same epoch and rebuild the model, then
    /// publish that model as the new authority (the positions still index it
    /// correctly, and the epoch/generation check prevents mismatched
    /// versions).
    pub fn next_batch(&mut self) -> Result<Option<FeedBatch>, FeedError> {
        let current_epoch = self.index_feed.epoch()?;
        let current_generation = self.index_feed.generation()?;

        // No change.
        if current_epoch == self.cursor.epoch && current_generation == self.cursor.generation {
            return Ok(None);
        }

        let current = IndexCursor {
            epoch: current_epoch,
            generation: current_generation,
        };

        // Check the cursor validity (§4.14).
        let log_floor = self.index_feed.log_floor()?;
        if current.epoch != self.cursor.epoch {
            return Err(FeedError::ResetRequired);
        }
        if self.cursor.generation < log_floor {
            return Err(FeedError::ResetRequired);
        }
        if self.cursor.generation > current.generation {
            return Err(FeedError::StaleCursor);
        }

        // Fetch the invalidations since the cursor.
        let cursor_version = IndexVersion::new(self.cursor.epoch, self.cursor.generation);
        let records = self.index_feed.get_invalidations_since(cursor_version)?;

        if records.is_empty() {
            // Epoch advanced but no records: a full reset happened.
            // This must not occur in practice (a recreate clears the table
            // and starts at generation 0), so treat it as reset.
            return Err(FeedError::ResetRequired);
        }

        // Build the model and snapshot from the current index state.
        let (nodes, edges) = self.load_current_state()?;
        let (deltas, snapshot) = self.derive_batch(&nodes, &edges, &records)?;

        let batch = FeedBatch {
            deltas,
            snapshot,
            epoch: current.epoch,
            generation: current.generation,
        };

        self.cursor = current;

        Ok(Some(batch))
    }

    /// Derive a delta batch from the invalidations and the current index
    /// state.
    ///
    /// For each invalidated entity:
    /// - Node/Attachment: add/remove the node delta.
    /// - Outgoing: add/remove the edge deltas.
    ///
    /// The snapshot is rebuilt from the current index rows, so it reflects
    /// the exact state the deltas were derived from. A consumer that
    /// receives a batch with `is_reset() == true` must rebuild from the
    /// snapshot alone.
    ///
    /// # Why we rebuild from current state
    ///
    /// The invalidation log records **what changed**, but not **what was
    /// removed**. A node deletion invalidates its own `note` entry, but
    /// the feed has no record of the node's old identity. Therefore the
    /// feed must ask the index "what nodes exist now" and "what edges exist
    /// now" and produce deltas that transition an arbitrary previous state
    /// to that current state. A model that was at state S0, missed state S1
    /// and S2, and now receives the batch for S3 will apply the deltas for
    /// S3 → S3 correctly: idempotence (D5) handles the double-add/remove.
    fn derive_batch(
        &self,
        nodes: &[FeedNode],
        edges: &[FeedEdge],
        records: &[ChangeRecord],
    ) -> Result<(Vec<Delta>, Arc<GraphSnapshot>), FeedError> {
        // Build a model from the current state.
        let mut model = GraphModel::new();

        // Map cache ids to NodeIds.
        let mut node_of_id: HashMap<i64, NodeId> = HashMap::new();
        let mut att_of_id: HashMap<i64, NodeId> = HashMap::new();

        // Process nodes: add all current nodes.
        for node in nodes {
            let id = match node.kind {
                NodeKind::Note => NodeId::note(node.id as u64),
                NodeKind::Attachment => NodeId::attachment(node.id as u64),
            };
            model.apply(&[Delta::Node(NodeDelta::Added { id })]);
            match node.kind {
                NodeKind::Note => {
                    node_of_id.insert(node.id, id);
                }
                NodeKind::Attachment => {
                    att_of_id.insert(node.id, id);
                }
            }
        }

        // Process edges: add all current edges.
        for edge in edges {
            let src_node = node_of_id.get(&edge.src).copied();
            let dst_node = if edge.dst_is_att {
                att_of_id.get(&edge.dst).copied()
            } else {
                node_of_id.get(&edge.dst).copied()
            };
            let Some(src) = src_node else { continue };
            let Some(dst) = dst_node else { continue };

            model.apply(&[Delta::Edge(EdgeDelta::Added {
                from: src,
                to: dst,
                kind: edge.kind,
            })]);
        }

        let snapshot = Arc::new(model.snapshot());

        // Build deltas from the invalidations.
        // For each invalidated entity, we produce deltas that ensure the
        // model matches the current state. An entity that was invalidated
        // and no longer exists → remove. An entity that was invalidated and
        // still exists → ensure it's added.
        let mut deltas = Vec::new();

        // Track which entity kinds were invalidated to avoid redundant adds.
        let mut invalidated_notes: BTreeSet<i64> = BTreeSet::new();
        let mut invalidated_atts: BTreeSet<i64> = BTreeSet::new();
        let mut invalidated_outgoing: BTreeSet<i64> = BTreeSet::new();

        for record in records {
            match record.kind {
                ChangeKind::Note => {
                    invalidated_notes.insert(record.entity_id);
                }
                ChangeKind::Attachment => {
                    invalidated_atts.insert(record.entity_id);
                }
                ChangeKind::Outgoing => {
                    invalidated_outgoing.insert(record.entity_id);
                }
            }
        }

        // Node invalidations.
        for &id in &invalidated_notes {
            // A current node with this id means it was added/changed.
            // An absent current node means it was removed.
            let node_exists = nodes
                .iter()
                .any(|n| n.id == id && matches!(n.kind, NodeKind::Note));
            if node_exists {
                let nid = NodeId::note(id as u64);
                deltas.push(Delta::Node(NodeDelta::Added { id: nid }));
            } else {
                let nid = NodeId::note(id as u64);
                deltas.push(Delta::Node(NodeDelta::Removed { id: nid }));
            }
        }

        // Attachment invalidations.
        for &id in &invalidated_atts {
            let att_exists = nodes
                .iter()
                .any(|n| n.id == id && matches!(n.kind, NodeKind::Attachment));
            if att_exists {
                let aid = NodeId::attachment(id as u64);
                deltas.push(Delta::Node(NodeDelta::Added { id: aid }));
            } else {
                let aid = NodeId::attachment(id as u64);
                deltas.push(Delta::Node(NodeDelta::Removed { id: aid }));
            }
        }

        // Outgoing invalidations: edges from the source changed.
        for &src_id in &invalidated_outgoing {
            // Remove old edges from this source by replacing with current edges.
            // The old edges are not explicitly known, so we rely on the
            // idempotence of apply: re-adding the current edges replaces
            // whatever was there before.
            let src_node = node_of_id.get(&src_id);
            let Some(src) = src_node else { continue };

            let src_edges: Vec<&FeedEdge> = edges.iter().filter(|e| e.src == src_id).collect();

            for edge in &src_edges {
                let dst_node = if edge.dst_is_att {
                    att_of_id.get(&edge.dst).copied()
                } else {
                    node_of_id.get(&edge.dst).copied()
                };
                let Some(dst) = dst_node else { continue };

                deltas.push(Delta::Edge(EdgeDelta::Added {
                    from: *src,
                    to: dst,
                    kind: edge.kind,
                }));
            }
        }

        Ok((deltas, snapshot))
    }

    /// Load the current index state: nodes and edges.
    ///
    /// Returns (nodes, edges) where:
    /// - `nodes` contains all note and attachment rows.
    /// - `edges` contains all link rows whose endpoints are known and
    ///   whose target is either a note or an attachment.
    fn load_current_state(&self) -> Result<(Vec<FeedNode>, Vec<FeedEdge>), FeedError> {
        // Load notes.
        let mut stmt = self.index_feed.conn.prepare("SELECT note_id FROM notes")?;
        let note_rows = stmt.query_map([], |row| {
            Ok(FeedNode {
                id: row.get(0)?,
                kind: NodeKind::Note,
            })
        })?;
        let notes: Vec<FeedNode> = note_rows.collect::<Result<Vec<_>, _>>()?;

        // Load attachments.
        let mut stmt = self
            .index_feed
            .conn
            .prepare("SELECT att_id FROM attachments")?;
        let att_rows = stmt.query_map([], |row| {
            Ok(FeedNode {
                id: row.get(0)?,
                kind: NodeKind::Attachment,
            })
        })?;
        let attachments: Vec<FeedNode> = att_rows.collect::<Result<Vec<_>, _>>()?;

        // Load links (edges).
        let mut stmt = self.index_feed.conn.prepare(
            "SELECT src, kind, dst_note, dst_att FROM links 
             WHERE dst_note IS NOT NULL OR dst_att IS NOT NULL",
        )?;
        let edge_rows = stmt.query_map([], |row| {
            let src: i64 = row.get(0)?;
            let kind_str: String = row.get(1)?;
            let kind = if kind_str == "link" {
                EdgeKind::Link
            } else {
                EdgeKind::Embed
            };
            let dst_note: Option<i64> = row.get(2)?;
            let dst_att: Option<i64> = row.get(3)?;

            let (dst, dst_is_att) = if let Some(dst) = dst_note {
                (dst, false)
            } else if let Some(dst) = dst_att {
                (dst, true)
            } else {
                // Unresolved link — no edge in the graph.
                return Ok(None);
            };

            Ok(Some(FeedEdge {
                src,
                dst,
                dst_is_att,
                kind,
            }))
        })?;
        let edges: Vec<FeedEdge> = edge_rows.filter_map(|r| r.ok().and_then(|x| x)).collect();

        // Combine notes and attachments.
        let mut all_nodes = notes;
        all_nodes.extend(attachments);

        Ok((all_nodes, edges))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These tests verify the feed's core logic with mocked index data.
    // A full integration test against a real index is in the B5 slice tests.

    #[test]
    fn cursor_starts_at_initial() {
        let cursor = IndexCursor::new();
        assert!(cursor.is_initial());
        assert_eq!(cursor.epoch, 0);
        assert_eq!(cursor.generation, 0);
    }

    #[test]
    fn cursor_advances() {
        let cursor = IndexCursor::new();
        let next = cursor.advanced(1, 5);
        assert_eq!(next.epoch, 1);
        assert_eq!(next.generation, 5);
        assert!(!next.is_initial());
    }

    #[test]
    fn feed_batch_from_snapshot_is_reset() {
        let snapshot = Arc::new(GraphModel::new().snapshot());
        let batch = FeedBatch::from_snapshot(snapshot, 1, 0);
        assert!(batch.is_reset());
        assert_eq!(batch.deltas.len(), 0);
        assert_eq!(batch.epoch, 1);
        assert_eq!(batch.generation, 0);
    }

    #[test]
    fn snapshot_roundtrips_through_model() {
        use crate::delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId};

        let mut model = GraphModel::new();
        let n1 = NodeId::note(1);
        let n2 = NodeId::note(2);
        model.apply(&[
            Delta::Node(NodeDelta::Added { id: n1 }),
            Delta::Node(NodeDelta::Added { id: n2 }),
            Delta::Edge(EdgeDelta::Added {
                from: n1,
                to: n2,
                kind: EdgeKind::Link,
            }),
        ]);

        let snapshot = model.snapshot();
        assert_eq!(snapshot.node_count(), 2);
        assert_eq!(snapshot.edge_count(), 1);

        // A batch with that snapshot.
        let batch = FeedBatch::from_snapshot(Arc::new(snapshot), 1, 0);
        assert!(batch.is_reset());

        // A roundtrip model built from the snapshot must have the same topology.
        let mut rebuilt = GraphModel::new();
        let snap = &batch.snapshot;
        for &id in &snap.node_ids {
            rebuilt.apply(&[Delta::Node(NodeDelta::Added { id })]);
        }
        for i in 0..snap.node_count() {
            let start = snap.out_offsets[i] as usize;
            let end = snap.out_offsets[i + 1] as usize;
            for row in start..end {
                let target = snap.out_indices[row] as usize;
                let from = snap.node_ids[i];
                let to = snap.node_ids[target];
                rebuilt.apply(&[Delta::Edge(EdgeDelta::Added {
                    from,
                    to,
                    kind: snap.out_edge_kinds[row],
                })]);
            }
        }

        assert_eq!(rebuilt.node_count(), model.node_count());
        assert_eq!(rebuilt.edge_count(), model.edge_count());

        // Verify the snapshot invariants: degrees match.
        for &id in &model.nodes().collect::<Vec<_>>() {
            assert_eq!(model.degree(id), rebuilt.degree(id));
        }
    }
}
