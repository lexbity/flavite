//! Deterministic, hub-heavy synthetic graphs for the S2 probe.
//!
//! A uniform random graph is the *wrong* fixture for a knowledge-graph
//! renderer: every node looks alike, the degree distribution is a narrow
//! band, and both the D15 tunables under test (degree-based sizing, edge cap
//! by importance) have nothing to bite on. Real vaults are hub-heavy, so the
//! probe uses **Barabási–Albert preferential attachment**: each new node
//! attaches to `m` already-connected nodes chosen in proportion to their
//! degree, which produces the power-law tail that makes a few notes link to
//! hundreds and most notes link to a handful.
//!
//! Everything here is deterministic for a given [`SyntheticOptions`]: the
//! generator owns its PRNG (xorshift64\*) and no external source of
//! randomness is consulted, so two runs of the probe — on two machines, on
//! two days — measure the *same* graph.

use flavite_graph::{
    Delta, EdgeDelta, EdgeKind, GraphModel, GraphSnapshot, NodeDelta, NodeId, NodeKind,
};

/// Nodes in the initial fully-connected core. Small on purpose: the first
/// handful of nodes become the hubs, exactly as the earliest notes in a vault
/// do.
const INITIAL_CORE: usize = 8;

/// Rotation between successive phyllotaxis placements: the golden angle,
/// `π(3 − √5) ≈ 2.3999632297` radians. A sunflower disc packs points
/// uniformly with no clumping, and — usefully — the *first* few points sit
/// near the centre, so the hubs (which are also the earliest nodes) land in
/// the middle of the map without needing a force layout.
///
/// Written as a literal because `f32::sqrt` is not a `const fn`; the closed
/// form is carried in the doc so the two never drift unnoticed.
const GOLDEN_ANGLE: f32 = 2.399_963_2;

/// World units between the origin and the last node of a 1-node-spaced disc;
/// scales as `WORLD_SCALE · √i`.
const WORLD_SCALE: f32 = 6.0;

/// Options for [`generate`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyntheticOptions {
    /// Number of nodes. 0 yields an empty graph.
    pub nodes: usize,
    /// *Target* edge count. It sizes the attachment factor `m`; the realized
    /// count is reported in [`SyntheticGraph::edges_generated`] and computed
    /// exactly by [`SyntheticOptions::expected_edge_count`] (the initial
    /// clique is added on top of `m·growth`, so a small graph can land above
    /// a small target — the AC-16 fixture lands at 199 996 against 200 000).
    pub edges: usize,
    /// PRNG seed — the only input that changes the graph.
    pub seed: u64,
    /// Make every `attachment_every`-th note an attachment node (squares,
    /// D15). 0 disables attachment nodes.
    pub attachment_every: usize,
    /// Make every `embed_every`-th edge an `![[embed]]` rather than a
    /// `[[link]]`. 0 disables embed edges.
    pub embed_every: usize,
}

impl SyntheticOptions {
    /// The AC-16 probe fixture: 50 000 nodes, ~200 000 edges, 5 % attachment
    /// nodes, every 8th edge an embed.
    #[must_use]
    pub fn probe_default() -> Self {
        Self {
            nodes: 50_000,
            edges: 200_000,
            seed: 20_250_101,
            attachment_every: 20,
            embed_every: 8,
        }
    }

    /// The attachment factor `m` implied by the target: each arriving node
    /// brings `m` new edges, so `m = edges / (nodes − core)`.
    ///
    /// Capped at the core size, because the *first* arriving node may only
    /// choose among the `core` founding nodes — a larger `m` would make
    /// "pick `m` distinct targets" impossible rather than merely unlikely.
    /// Returns 0 when the graph cannot grow (fewer nodes than the core, or no
    /// edges requested).
    #[must_use]
    pub fn attachment_factor(&self) -> usize {
        if self.nodes <= INITIAL_CORE || self.edges == 0 {
            return 0;
        }
        let core = self.nodes.min(INITIAL_CORE);
        (self.edges / (self.nodes - core)).clamp(1, core)
    }

    /// The exact edge count [`generate`] will realize: the initial clique
    /// over `min(nodes, 8)` nodes plus one edge per arriving node per
    /// attachment target.
    ///
    /// Kept beside the generator so the contract is stated once and asserted
    /// against directly, instead of being re-derived (and drifting) in tests.
    #[must_use]
    pub fn expected_edge_count(&self) -> usize {
        if self.nodes == 0 {
            return 0;
        }
        let core = self.nodes.min(INITIAL_CORE);
        let clique = core * (core - 1) / 2;
        clique + self.attachment_factor() * (self.nodes - core)
    }
}

/// A generated fixture: the model snapshot, dense-indexed positions, and the
/// realized edge count.
#[derive(Debug, Clone, PartialEq)]
pub struct SyntheticGraph {
    /// The CPU-authoritative snapshot the renderer consumes (D15).
    pub snapshot: GraphSnapshot,
    /// World position per dense node index — exactly what
    /// [`ProbeGeometry::from_snapshot`] wants.
    ///
    /// [`ProbeGeometry`]: crate::graph::instance::ProbeGeometry
    pub positions: Vec<[f32; 2]>,
    /// Edges actually generated (≤ [`Self::options`]'s target).
    pub edges_generated: usize,
    /// The options this graph was built from, for reporting.
    pub options: SyntheticOptions,
}

impl SyntheticGraph {
    /// Whether this graph has no nodes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.snapshot.is_empty()
    }

    /// Highest node degree — the hub the D15 sizing tunable must handle.
    #[must_use]
    pub fn max_degree(&self) -> u32 {
        self.snapshot.degrees.iter().copied().max().unwrap_or(0)
    }

    /// Mean node degree (`0.0` for an empty graph).
    #[must_use]
    pub fn mean_degree(&self) -> f32 {
        let n = self.snapshot.degrees.len();
        if n == 0 {
            return 0.0;
        }
        let total: u64 = self.snapshot.degrees.iter().map(|&d| u64::from(d)).sum();
        total as f32 / n as f32
    }

    /// Number of attachment nodes.
    #[must_use]
    pub fn attachment_count(&self) -> usize {
        self.snapshot
            .node_kinds
            .iter()
            .filter(|&&kind| kind == NodeKind::Attachment)
            .count()
    }

    /// The world-space radius the map spans (the extent to `fit_extent`).
    #[must_use]
    pub fn world_extent(&self) -> f32 {
        self.positions
            .iter()
            .map(|[x, y]| x.hypot(*y))
            .fold(0.0f32, f32::max)
    }
}

/// xorshift64\* — a 64-bit generator with a period of 2⁶⁴−1.
///
/// Chosen over `StdRng` so the fixture does not depend on a particular
/// `rand` release's algorithm: the *same seed must produce the same graph*
/// is a property of this module, and pinning the algorithm here is what makes
/// that hold across dependency bumps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rng(u64);

impl Rng {
    /// Seed the generator. A zero state is rejected (xorshift is a fixed
    /// point at 0) by substituting the golden-ratio constant.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    /// The next raw 64-bit value.
    #[must_use]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A uniformly-ish distributed value in `0..bound`.
    ///
    /// Modulo reduction leaves a bias of at most `(2⁶⁴ mod bound)/2⁶⁴` — under
    /// 10⁻¹⁴ for every bound the generator uses (≤ 50 000). The bias does not
    /// affect determinism, which is the property that matters here.
    #[must_use]
    pub fn below(&mut self, bound: u64) -> u64 {
        let bound = bound.max(1);
        self.next_u64() % bound
    }

    /// A value in `[0.0, 1.0)`.
    #[must_use]
    pub fn unit_f32(&mut self) -> f32 {
        // 24 random mantissa bits — exactly representable, no precision traps.
        ((self.next_u64() >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

/// Generate a deterministic hub-heavy fixture.
///
/// # Algorithm
///
/// 1. Build a `m₀`-node clique (the vault's founding notes).
/// 2. For each remaining node: pick `m` *distinct* targets from a stub list
///    (every node appears there once per incident edge, so the probability of
///    being chosen is proportional to degree — preferential attachment), and
///    connect the new node to them.
/// 3. Assign world positions from a phyllotaxis disc indexed by dense order.
/// 4. Feed everything through [`GraphModel`] so the fixture exercises B1's
///    real CSR path rather than a private imitation of it.
///
/// Because targets are drawn without replacement per arriving node, no
/// `(from, to)` pair repeats: the realized edge count is exactly
/// `clique(m₀) + m·(nodes − m₀)`, always ≤ the requested target.
pub fn generate(options: &SyntheticOptions) -> SyntheticGraph {
    let node_count = options.nodes;
    if node_count == 0 {
        return SyntheticGraph {
            snapshot: GraphModel::new().snapshot(),
            positions: Vec::new(),
            edges_generated: 0,
            options: *options,
        };
    }

    let core = node_count.min(INITIAL_CORE);
    let m = options.attachment_factor();

    // --- Node kinds -----------------------------------------------------
    // Attachment ids share the note id space only up to `NodeId`'s kind
    // tagging; the logical id of note `i` is `i`, so the "every k-th" rule is
    // evaluated over the *generation* order and the model assigns dense
    // indices afterwards (notes sort before attachments).
    let kind_of = |logical: usize| -> NodeKind {
        if options.attachment_every > 0
            && logical % options.attachment_every == options.attachment_every - 1
        {
            NodeKind::Attachment
        } else {
            NodeKind::Note
        }
    };

    // --- Preferential attachment ---------------------------------------
    let mut rng = Rng::new(options.seed);
    let mut edges: Vec<(usize, usize)> = Vec::with_capacity(options.edges);
    let mut stubs: Vec<usize> = Vec::with_capacity(options.edges * 2);

    for i in 0..core {
        for j in (i + 1)..core {
            edges.push((i, j));
            stubs.push(i);
            stubs.push(j);
        }
    }

    for new_node in core..node_count {
        if m == 0 {
            break;
        }
        // Preferential attachment without replacement: each arriving node
        // gets `m` *distinct* targets, so no `(from, to)` pair ever repeats.
        // The stub list grows with the edge count, so the per-attempt chance
        // of redrawing an already-chosen target stays small.
        let mut chosen: Vec<usize> = Vec::with_capacity(m);
        for _attempt in 0..64 {
            if chosen.len() == m {
                break;
            }
            let pick = stubs[rng.below(stubs.len() as u64) as usize];
            if !chosen.contains(&pick) {
                chosen.push(pick);
            }
        }
        // Guaranteed fill: `m ≤ core ≤ new_node`, so scanning the nodes that
        // already exist always finds `m` distinct targets. Without it, a run
        // of redrawn targets would silently realize fewer edges than
        // `expected_edge_count` promises.
        for candidate in 0..new_node {
            if chosen.len() == m {
                break;
            }
            if !chosen.contains(&candidate) {
                chosen.push(candidate);
            }
        }
        debug_assert_eq!(chosen.len(), m, "must pick exactly m distinct targets");
        for target in chosen {
            edges.push((new_node, target));
            stubs.push(new_node);
            stubs.push(target);
        }
    }
    debug_assert_eq!(
        edges.len(),
        options.expected_edge_count(),
        "generator realized a different edge count than the stated contract"
    );

    // --- Positions (phyllotaxis, dense order) ---------------------------
    let positions: Vec<[f32; 2]> = (0..node_count).map(position_of).collect();

    // --- Feed B1's model ------------------------------------------------
    let node_deltas: Vec<Delta> = (0..node_count)
        .map(|logical| {
            let id = node_id(logical, kind_of(logical));
            Delta::Node(NodeDelta::Added { id })
        })
        .collect();

    let mut model = GraphModel::new();
    for batch in node_deltas.chunks(50_000) {
        model.apply(batch);
    }

    let edge_deltas: Vec<Delta> = edges
        .iter()
        .enumerate()
        .map(|(index, &(from, to))| {
            let kind = if options.embed_every > 0 && (index + 1) % options.embed_every == 0 {
                EdgeKind::Embed
            } else {
                EdgeKind::Link
            };
            Delta::Edge(EdgeDelta::Added {
                from: node_id(from, kind_of(from)),
                to: node_id(to, kind_of(to)),
                kind,
            })
        })
        .collect();
    for batch in edge_deltas.chunks(50_000) {
        model.apply(batch);
    }

    let edges_generated = model.edge_count();

    SyntheticGraph {
        snapshot: model.snapshot(),
        positions,
        edges_generated,
        options: *options,
    }
}

/// The `NodeId` for a logical node index of a given kind.
fn node_id(logical: usize, kind: NodeKind) -> NodeId {
    match kind {
        NodeKind::Note => NodeId::note(logical as u64),
        NodeKind::Attachment => NodeId::attachment(logical as u64),
    }
}

/// World position of the node at dense index `i` (phyllotaxis disc).
fn position_of(i: usize) -> [f32; 2] {
    let fi = i as f32;
    let radius = WORLD_SCALE * fi.sqrt();
    let angle = fi * GOLDEN_ANGLE;
    [radius * angle.cos(), radius * angle.sin()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(nodes: usize, edges: usize, seed: u64) -> SyntheticOptions {
        SyntheticOptions {
            nodes,
            edges,
            seed,
            attachment_every: 5,
            embed_every: 4,
        }
    }

    #[test]
    fn rng_is_deterministic_and_handles_the_zero_state() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        let draw_a: Vec<u64> = (0..16).map(|_| a.next_u64()).collect();
        let draw_b: Vec<u64> = (0..16).map(|_| b.next_u64()).collect();
        assert_eq!(draw_a, draw_b, "same seed → same stream");

        let mut c = Rng::new(43);
        let draw_c: Vec<u64> = (0..16).map(|_| c.next_u64()).collect();
        assert_ne!(draw_a, draw_c, "different seed → different stream");

        // Zero is a fixed point of xorshift — the constructor must avoid it.
        let mut zero = Rng::new(0);
        assert_ne!(zero.next_u64(), 0);

        // Consecutive values are never equal (would mean a stuck state).
        let mut d = Rng::new(7);
        let mut previous = 0;
        for _ in 0..64 {
            let next = d.next_u64();
            assert_ne!(next, previous);
            previous = next;
        }
    }

    #[test]
    fn rng_below_and_unit_f32_stay_in_range() {
        let mut rng = Rng::new(1234);
        for _ in 0..5000 {
            let value = rng.below(10);
            assert!(value < 10, "below(10) gave {value}");
            assert!(rng.below(1) == 0, "below(1) is always 0");
            let unit = rng.unit_f32();
            assert!((0.0..1.0).contains(&unit), "unit_f32 gave {unit}");
        }
        // A zero bound must not panic or divide by zero.
        assert!(rng.below(0) < 1);
    }

    #[test]
    fn empty_options_yield_an_empty_graph() {
        let graph = generate(&small(0, 0, 1));
        assert!(graph.is_empty());
        assert_eq!(graph.snapshot.node_count(), 0);
        assert_eq!(graph.snapshot.edge_count(), 0);
        assert_eq!(graph.edges_generated, 0);
        assert!(graph.positions.is_empty());
        assert_eq!(graph.max_degree(), 0);
        assert_eq!(graph.mean_degree(), 0.0);
        assert_eq!(graph.attachment_count(), 0);
        assert_eq!(graph.world_extent(), 0.0);
        assert_eq!(graph.options, small(0, 0, 1));
    }

    #[test]
    fn node_count_is_exact_and_edge_count_matches_the_contract() {
        for &(nodes, target) in &[(4usize, 0usize), (10, 5), (100, 50), (1_000, 4_000)] {
            let options = small(nodes, target, 99);
            let graph = generate(&options);
            assert_eq!(graph.snapshot.node_count(), nodes);
            assert_eq!(graph.positions.len(), nodes);
            assert_eq!(
                graph.edges_generated,
                options.expected_edge_count(),
                "realized count must equal the stated contract"
            );
            assert_eq!(
                graph.snapshot.edge_count(),
                graph.edges_generated,
                "every generated edge survives the model"
            );
            // No duplicate (from, to) pairs: the model would collapse them,
            // which would show up as a shortfall against the contract.
            let snapshot = &graph.snapshot;
            let mut pairs: Vec<(NodeId, NodeId)> = Vec::with_capacity(snapshot.edge_count());
            for src in 0..snapshot.node_count() {
                let start = snapshot.out_offsets[src] as usize;
                let end = snapshot.out_offsets[src + 1] as usize;
                for row in start..end {
                    let dst = snapshot.out_indices[row] as usize;
                    pairs.push((snapshot.node_ids[src], snapshot.node_ids[dst]));
                }
            }
            pairs.sort_unstable();
            let before = pairs.len();
            pairs.dedup();
            assert_eq!(pairs.len(), before, "no duplicate endpoint pairs");
        }
    }

    #[test]
    fn attachment_factor_never_exceeds_the_founding_core() {
        // The first arriving node can only reach the core, so `m` is capped.
        let mut options = SyntheticOptions::probe_default();
        options.edges = usize::MAX;
        assert_eq!(
            options.attachment_factor(),
            INITIAL_CORE,
            "m is capped at the number of founding nodes"
        );
        assert_eq!(
            options.expected_edge_count(),
            INITIAL_CORE * (INITIAL_CORE - 1) / 2 + INITIAL_CORE * (options.nodes - INITIAL_CORE)
        );
    }

    #[test]
    fn attachment_factor_derives_m_from_the_target() {
        let options = SyntheticOptions::probe_default();
        // (200_000 − clique) / (50_000 − 8) → 4
        assert_eq!(options.attachment_factor(), 4);

        let mut degenerate = options;
        degenerate.nodes = 4;
        assert_eq!(degenerate.attachment_factor(), 0, "no growth room");

        degenerate = options;
        degenerate.edges = 0;
        assert_eq!(degenerate.attachment_factor(), 0, "no edges requested");

        degenerate = options;
        degenerate.edges = 1;
        assert_eq!(degenerate.attachment_factor(), 1, "at least one");
    }

    #[test]
    fn degree_distribution_is_hub_heavy() {
        let graph = generate(&small(1_000, 4_000, 7));
        let mean = graph.mean_degree();
        let max = graph.max_degree();
        assert!(mean > 0.0, "graph must actually be connected");
        assert!(
            max as f32 > 10.0 * mean,
            "hub-heavy means the top node is an order of magnitude above the \
             mean: max {max} vs mean {mean}"
        );
        // Preferential attachment: node 0 is in the initial clique and is a
        // hub.
        assert!(
            graph.snapshot.degrees[0] > max / 2,
            "the founding node should be a hub, got degree {}",
            graph.snapshot.degrees[0]
        );
    }

    #[test]
    fn attachments_and_embeds_appear_in_the_mixture() {
        let graph = generate(&small(1_000, 3_000, 11));
        // Every 5th logical node is an attachment: 1000/5 = 200 of them.
        assert_eq!(graph.attachment_count(), 200);
        assert_eq!(
            graph
                .snapshot
                .node_kinds
                .iter()
                .filter(|&&k| k == NodeKind::Note)
                .count(),
            800
        );
        // Attachment ids occupy a separate id space and sort after notes in
        // the model's dense order.
        let last_note = graph.snapshot.node_count() - graph.attachment_count();
        for kind in &graph.snapshot.node_kinds[..last_note] {
            assert_eq!(*kind, NodeKind::Note);
        }
        for kind in &graph.snapshot.node_kinds[last_note..] {
            assert_eq!(*kind, NodeKind::Attachment);
        }

        // Every 4th edge is an embed.
        let embeds = graph
            .snapshot
            .out_edge_kinds
            .iter()
            .filter(|&&k| k == EdgeKind::Embed)
            .count();
        let expected_embeds = graph.edges_generated / 4;
        assert!(
            (embeds as i64 - expected_embeds as i64).abs() <= 1,
            "embed mix: got {embeds}, expected about {expected_embeds}"
        );
        assert!(embeds > 0, "embed edges must be present");
    }

    #[test]
    fn same_seed_reproduces_the_graph_exactly() {
        let options = small(500, 1_500, 20_250_101);
        let a = generate(&options);
        let b = generate(&options);
        assert_eq!(a, b, "same options → byte-identical fixture");
        assert_eq!(a.edges_generated, b.edges_generated);
        assert_eq!(a.positions, b.positions);
        assert_eq!(a.snapshot, b.snapshot);
    }

    #[test]
    fn different_seeds_produce_different_graphs() {
        let base = small(500, 1_500, 1);
        let mut other = base;
        other.seed = 2;
        let a = generate(&base);
        let b = generate(&other);
        assert_eq!(a.snapshot.node_count(), b.snapshot.node_count());
        assert_ne!(
            a.snapshot.out_indices,
            b.out_indices_sentinel(),
            "seed must change the topology"
        );
    }

    #[test]
    fn positions_are_finite_bounded_and_hub_at_the_origin() {
        let graph = generate(&small(2_000, 6_000, 5));
        for (i, [x, y]) in graph.positions.iter().enumerate() {
            assert!(x.is_finite() && y.is_finite(), "node {i} at {x},{y}");
        }
        // Dense index 0 sits at the origin by construction.
        assert_eq!(graph.positions[0], [0.0, 0.0]);
        // Monotonically growing radius: the disc never folds back.
        let extent = graph.world_extent();
        assert!(extent > 0.0 && extent.is_finite());
        assert!(
            graph.positions[graph.positions.len() - 1][0]
                .hypot(graph.positions[graph.positions.len() - 1][1])
                > extent * 0.99,
            "the last node defines the extent"
        );
        // Adjacent dense indices are distinct points.
        assert_ne!(graph.positions[1], graph.positions[2]);
    }

    #[test]
    fn graph_and_positions_are_send_sync() {
        // The render thread swaps an `Arc` to the geometry built from these
        // (D15: no locks on the frame path).
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SyntheticGraph>();
        assert_send_sync::<GraphSnapshot>();
    }

    #[test]
    fn probe_scale_fixture_builds_within_budget() {
        // The B2 reference fixture. The build is CPU work done once, so the
        // budget is generous (CI runners are slow); the point is to catch an
        // accidental O(n²) regression, not to benchmark.
        let options = SyntheticOptions::probe_default();
        assert_eq!(options.attachment_factor(), 4);

        let started = std::time::Instant::now();
        let graph = generate(&options);
        let elapsed = started.elapsed();

        assert_eq!(graph.snapshot.node_count(), 50_000);
        assert_eq!(graph.positions.len(), 50_000);
        // clique(8) + 4 × (50_000 − 8) = 28 + 199_968 = 199_996.
        assert_eq!(graph.edges_generated, 199_996);
        assert_eq!(graph.snapshot.edge_count(), 199_996);
        assert_eq!(graph.attachment_count(), 50_000 / 20);
        assert!(graph.max_degree() > 100, "hubs must be hubs");
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "fixture build took {elapsed:?}"
        );
    }

    /// Helper: the out-adjacency of an unrelated seed, for a "did the seed
    /// matter?" comparison without leaking a public accessor.
    trait OutIndicesExt {
        fn out_indices_sentinel(&self) -> Vec<u32>;
    }

    impl OutIndicesExt for SyntheticGraph {
        fn out_indices_sentinel(&self) -> Vec<u32> {
            self.snapshot.out_indices.clone()
        }
    }
}
