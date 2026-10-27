//! Property tests (proptest, in-crate — §10.6).
//!
//! - **Model oracle**: random delta batches over a small id space; the
//!   [`GraphModel`] must equal a naive set-based oracle after every batch,
//!   CSR invariants must hold, and `topology_version` must bump exactly
//!   when the sets change (D5 idempotence, D15 versioning).
//! - **Incremental == batch**: a model built from a random delta sequence
//!   must snapshot identically to a model built from the final sets in one
//!   batch (the CSR derives from the sets alone → deterministic).
//! - **Spatial oracle**: random insert/remove/zoom ops + queries; the
//!   [`SpatialGrid`] must agree with a brute-force linear oracle on every
//!   hit (nearest within radius, ties to the smallest [`NodeId`]).

#[cfg(test)]
mod model_oracle {
    use crate::delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId, NodeKind};
    use crate::model::{GraphModel, assert_csr_invariants};
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    /// The naive oracle: the documented delta semantics transcribed over
    /// plain sets (no CSR, no version bookkeeping).
    #[derive(Default)]
    struct Oracle {
        nodes: BTreeSet<NodeId>,
        edges: BTreeSet<(NodeId, NodeId, EdgeKind)>,
    }

    /// Per-delta effective-change counts — the documented `ApplyStats`
    /// semantics, transcribed independently of the model: an entity added
    /// and removed within one batch counts in both counters.
    #[derive(Default, Debug, PartialEq, Eq)]
    struct OracleStats {
        nodes_added: u32,
        nodes_removed: u32,
        edges_added: u32,
        edges_removed: u32,
    }

    impl OracleStats {
        fn changed(&self) -> bool {
            self.nodes_added > 0
                || self.nodes_removed > 0
                || self.edges_added > 0
                || self.edges_removed > 0
        }
    }

    impl Oracle {
        fn apply(&mut self, deltas: &[Delta]) -> OracleStats {
            let mut stats = OracleStats::default();
            for &delta in deltas {
                match delta {
                    Delta::Node(NodeDelta::Added { id }) => {
                        if self.nodes.insert(id) {
                            stats.nodes_added += 1;
                        }
                    }
                    Delta::Node(NodeDelta::Removed { id }) => {
                        if self.nodes.remove(&id) {
                            stats.nodes_removed += 1;
                            let before = self.edges.len();
                            self.edges.retain(|&(from, to, _)| from != id && to != id);
                            stats.edges_removed += (before - self.edges.len()) as u32;
                        }
                    }
                    Delta::Edge(EdgeDelta::Added { from, to, kind }) => {
                        if self.nodes.contains(&from)
                            && self.nodes.contains(&to)
                            && self.edges.insert((from, to, kind))
                        {
                            stats.edges_added += 1;
                        }
                    }
                    Delta::Edge(EdgeDelta::Removed { from, to, kind }) => {
                        if self.edges.remove(&(from, to, kind)) {
                            stats.edges_removed += 1;
                        }
                    }
                }
            }
            stats
        }
    }

    fn node_id_strategy() -> impl Strategy<Value = NodeId> {
        (0..2u8, 0..6u64).prop_map(|(kind, id)| NodeId {
            kind: if kind == 0 {
                NodeKind::Note
            } else {
                NodeKind::Attachment
            },
            id,
        })
    }

    fn edge_kind_strategy() -> impl Strategy<Value = EdgeKind> {
        any::<bool>().prop_map(|embed| {
            if embed {
                EdgeKind::Embed
            } else {
                EdgeKind::Link
            }
        })
    }

    fn delta_strategy() -> impl Strategy<Value = Delta> {
        let node_add = node_id_strategy().prop_map(|id| Delta::Node(NodeDelta::Added { id }));
        let node_remove = node_id_strategy().prop_map(|id| Delta::Node(NodeDelta::Removed { id }));
        let edge_add = (node_id_strategy(), node_id_strategy(), edge_kind_strategy())
            .prop_map(|(from, to, kind)| Delta::Edge(EdgeDelta::Added { from, to, kind }));
        let edge_remove = (node_id_strategy(), node_id_strategy(), edge_kind_strategy())
            .prop_map(|(from, to, kind)| Delta::Edge(EdgeDelta::Removed { from, to, kind }));
        prop_oneof![
            3 => node_add,
            2 => node_remove,
            4 => edge_add,
            3 => edge_remove,
        ]
    }

    fn batch_strategy() -> impl Strategy<Value = Vec<Delta>> {
        proptest::collection::vec(delta_strategy(), 0..8)
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Random delta batches: the model equals the naive oracle after
        /// every batch — sets, degrees, stats, version, CSR invariants, and
        /// snapshot content.
        #[test]
        fn model_matches_naive_oracle(
            batches in proptest::collection::vec(batch_strategy(), 0..40),
        ) {
            let mut model = GraphModel::new();
            let mut oracle = Oracle::default();
            let mut expected_version = 0u64;

            for batch in &batches {
                let stats = model.apply(batch);
                let oracle_stats = oracle.apply(batch);

                // Stats == the oracle's per-delta effective counts.
                prop_assert_eq!(stats.changed, oracle_stats.changed(), "changed flag");
                prop_assert_eq!(stats.nodes_added, oracle_stats.nodes_added);
                prop_assert_eq!(stats.nodes_removed, oracle_stats.nodes_removed);
                prop_assert_eq!(stats.edges_added, oracle_stats.edges_added);
                prop_assert_eq!(stats.edges_removed, oracle_stats.edges_removed);
                let changed = oracle_stats.changed();

                // Topology version: +1 per effective batch, never otherwise.
                if changed {
                    expected_version += 1;
                }
                prop_assert_eq!(
                    model.topology_version(),
                    expected_version,
                    "version bumps exactly on change"
                );

                // Model == oracle, set-wise.
                prop_assert_eq!(model.node_count(), oracle.nodes.len());
                prop_assert_eq!(model.edge_count(), oracle.edges.len());
                prop_assert_eq!(
                    model.nodes().collect::<BTreeSet<_>>(),
                    oracle.nodes.clone()
                );
                prop_assert_eq!(
                    model.edges().collect::<BTreeSet<_>>(),
                    oracle.edges.clone()
                );
                for &id in &oracle.nodes {
                    prop_assert!(model.contains_node(id));
                    prop_assert_eq!(model.node_kind(id), Some(id.kind));
                }

                // Degrees match the oracle edge set.
                for &id in &oracle.nodes {
                    let out = oracle.edges.iter().filter(|&&(from, _, _)| from == id).count() as u32;
                    let inc = oracle.edges.iter().filter(|&&(_, to, _)| to == id).count() as u32;
                    prop_assert_eq!(model.out_degree(id), Some(out), "out_degree {:?}", id);
                    prop_assert_eq!(model.in_degree(id), Some(inc), "in_degree {:?}", id);
                    prop_assert_eq!(model.degree(id), Some(out + inc), "degree {:?}", id);
                }

                // CSR cache invariants.
                assert_csr_invariants(&model);

                // Snapshot content matches the oracle.
                let snap = model.snapshot();
                prop_assert_eq!(
                    snap.node_ids.iter().copied().collect::<BTreeSet<_>>(),
                    oracle.nodes.clone()
                );
                prop_assert_eq!(snap.edge_count(), oracle.edges.len());
                prop_assert_eq!(snap.out_indices.len(), oracle.edges.len());
                prop_assert!(
                    snap.node_ids.windows(2).all(|w| w[0] < w[1]),
                    "dense ids sorted"
                );
                for (i, &id) in snap.node_ids.iter().enumerate() {
                    prop_assert_eq!(snap.node_index(id), Some(i as u32));
                    prop_assert_eq!(snap.node_kinds[i], id.kind);
                    // Out-edges match the oracle (as (dense target, kind) sets).
                    let start = snap.out_offsets[i] as usize;
                    let end = snap.out_offsets[i + 1] as usize;
                    let got: BTreeSet<(u32, EdgeKind)> = snap.out_indices[start..end]
                        .iter()
                        .copied()
                        .zip(snap.out_edge_kinds[start..end].iter().copied())
                        .collect();
                    let want: BTreeSet<(u32, EdgeKind)> = oracle
                        .edges
                        .iter()
                        .filter(|&&(from, _, _)| from == id)
                        .map(|&(_, to, kind)| (snap.node_index(to).unwrap(), kind))
                        .collect();
                    prop_assert_eq!(got, want, "out-edges of {:?}", id);
                    // Symmetrized adjacency = union of in/out neighbors, deduped.
                    let sstart = snap.adj_offsets[i] as usize;
                    let send = snap.adj_offsets[i + 1] as usize;
                    let adj: BTreeSet<u32> =
                        snap.adj_indices[sstart..send].iter().copied().collect();
                    let mut want_adj: BTreeSet<u32> = oracle
                        .edges
                        .iter()
                        .filter(|&&(from, _, _)| from == id)
                        .map(|&(_, to, _)| snap.node_index(to).unwrap())
                        .collect();
                    want_adj.extend(
                        oracle
                            .edges
                            .iter()
                            .filter(|&&(_, to, _)| to == id)
                            .map(|&(from, _, _)| snap.node_index(from).unwrap()),
                    );
                    prop_assert_eq!(adj, want_adj, "adjacency of {:?}", id);
                    // Degree parallel array = in + out edge rows.
                    let out = end - start;
                    let inc = oracle.edges.iter().filter(|&&(_, to, _)| to == id).count() as u32;
                    prop_assert_eq!(snap.degrees[i], out as u32 + inc);
                }
            }
        }

        /// The CSR derives from the final sets alone: a model built
        /// incrementally snapshots identically to one built from the final
        /// sets in a single batch.
        #[test]
        fn incremental_equals_single_batch(
            batches in proptest::collection::vec(batch_strategy(), 0..40),
        ) {
            let mut model = GraphModel::new();
            for batch in &batches {
                model.apply(batch);
            }
            let mut rebuilt = GraphModel::new();
            let final_deltas: Vec<Delta> = model
                .nodes()
                .map(|id| Delta::Node(NodeDelta::Added { id }))
                .chain(model.edges().map(|(from, to, kind)| {
                    Delta::Edge(EdgeDelta::Added { from, to, kind })
                }))
                .collect();
            rebuilt.apply(&final_deltas);
            // One batch: version 0 (empty) or 1 (non-empty).
            prop_assert_eq!(
                rebuilt.topology_version(),
                u64::from(!rebuilt.is_empty())
            );
            let a = model.snapshot();
            let b = rebuilt.snapshot();
            // topology_version differs (1 batch vs N effective batches) —
            // the topology CONTENT must be identical.
            prop_assert_eq!(a.node_ids, b.node_ids);
            prop_assert_eq!(a.node_kinds, b.node_kinds);
            prop_assert_eq!(a.degrees, b.degrees);
            prop_assert_eq!(a.out_offsets, b.out_offsets);
            prop_assert_eq!(a.out_indices, b.out_indices);
            prop_assert_eq!(a.out_edge_kinds, b.out_edge_kinds);
            prop_assert_eq!(a.adj_offsets, b.adj_offsets);
            prop_assert_eq!(a.adj_indices, b.adj_indices);
        }
    }
}

#[cfg(test)]
mod spatial_oracle {
    use crate::delta::{NodeId, NodeKind};
    use crate::spatial::SpatialGrid;
    use proptest::prelude::*;
    use std::collections::BTreeMap;

    /// Brute-force hit oracle: linear scan, nearest within radius, ties to
    /// the smallest `NodeId` — the documented semantics, transcribed.
    fn oracle_hit(
        entries: &BTreeMap<NodeId, (f64, f64)>,
        x: f64,
        y: f64,
        radius: f64,
    ) -> Option<NodeId> {
        let r2 = radius * radius;
        let mut best: Option<(f64, NodeId)> = None;
        for (&id, &(ex, ey)) in entries {
            let dx = ex - x;
            let dy = ey - y;
            let d2 = dx * dx + dy * dy;
            if d2 > r2 {
                continue;
            }
            let better = match &best {
                None => true,
                Some((bd, bid)) => d2 < *bd || (d2 == *bd && id < *bid),
            };
            if better {
                best = Some((d2, id));
            }
        }
        best.map(|(_, id)| id)
    }

    fn node_id_strategy() -> impl Strategy<Value = NodeId> {
        (0..2u8, 0..4u64).prop_map(|(kind, id)| NodeId {
            kind: if kind == 0 {
                NodeKind::Note
            } else {
                NodeKind::Attachment
            },
            id,
        })
    }

    /// Coordinates on a 10-unit lattice so cells and query radii interact
    /// meaningfully.
    fn coord_strategy() -> impl Strategy<Value = f64> {
        (-20..=20i32).prop_map(|v| v as f64 * 10.0)
    }

    fn zoom_strategy() -> impl Strategy<Value = f64> {
        prop_oneof![
            1 => Just(0.25),
            1 => Just(0.5),
            2 => Just(1.0),
            1 => Just(2.0),
            1 => Just(4.0),
        ]
    }

    #[derive(Debug, Clone)]
    enum Op {
        Insert(NodeId, f64, f64),
        Remove(NodeId),
        SetZoom(f64),
    }

    fn op_strategy() -> impl Strategy<Value = Op> {
        let insert = (node_id_strategy(), coord_strategy(), coord_strategy())
            .prop_map(|(id, x, y)| Op::Insert(id, x, y));
        let remove = node_id_strategy().prop_map(Op::Remove);
        let zoom = zoom_strategy().prop_map(Op::SetZoom);
        prop_oneof![
            6 => insert,
            2 => remove,
            1 => zoom,
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        /// Random ops + queries: the grid agrees with a brute-force oracle.
        #[test]
        fn grid_matches_brute_force_oracle(
            ops in proptest::collection::vec(op_strategy(), 0..60),
            queries in proptest::collection::vec(
                (coord_strategy(), coord_strategy(), 0..150i32),
                0..20,
            ),
        ) {
            let mut grid = SpatialGrid::new(1.0);
            let mut oracle: BTreeMap<NodeId, (f64, f64)> = BTreeMap::new();
            for op in &ops {
                match *op {
                    Op::Insert(id, x, y) => {
                        grid.insert(id, x, y);
                        oracle.insert(id, (x, y));
                    }
                    Op::Remove(id) => {
                        prop_assert_eq!(grid.remove(id), oracle.remove(&id).is_some());
                    }
                    Op::SetZoom(zoom) => {
                        grid.set_zoom(zoom);
                    }
                }
                prop_assert_eq!(grid.len(), oracle.len());
            }
            for (x, y, r) in &queries {
                let radius = *r as f64;
                prop_assert_eq!(
                    grid.hit(*x, *y, radius),
                    oracle_hit(&oracle, *x, *y, radius),
                    "hit({}, {}, {})",
                    x,
                    y,
                    radius
                );
            }
        }
    }
}
