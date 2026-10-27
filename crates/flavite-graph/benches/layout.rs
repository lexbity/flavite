//! Zero-dependency benchmark harness for the B3 layout (PLAN-B §5:
//! `cargo bench -p flavite-graph`).
//!
//! ```sh
//! cargo bench -p flavite-graph
//! cargo bench -p flavite-graph -- --quick     # fewer samples
//! ```
//!
//! # Why not criterion
//!
//! The measurement this slice needs is a *percentile table over one fixed
//! fixture*, not a comparison of two implementations of the same function.
//! Criterion's value is the statistics it applies to an A/B comparison; here
//! there is no B, and the numbers that matter (p95/p99 of a layout step, the
//! wall clock of a topology change) are exactly what a warmup + N timed
//! samples + a percentile summary produces. A dependency that exists to
//! compare is not justified to report one table — and the fixture itself has
//! to be written either way, since `flavite-graph` deliberately has no
//! synthetic-graph module of its own (that lives in `flavite-ui` for the
//! probe).
//!
//! # What is measured
//!
//! | scenario | why |
//! |---|---|
//! | `step` at 5 k / 20 k / 50 k / 100 k nodes | the per-step cost the render thread must never wait for; p95/p99 are the numbers that show whether one step fits a frame budget when the pool is shared with the UI |
//! | `retarget` at 50 k nodes | the wake path: what a topology change costs before stepping resumes |
//! | `converge` at 5 k nodes | wall clock from a cold start to `Frozen` — the layout's own "done" signal |
//!
//! Every scenario runs on a warm pool, warms up before sampling, times each
//! step individually (so a GC-like hiccup shows up in p99 instead of being
//! averaged away), and reports p50/p95/p99/mean/max.

use std::sync::Arc;
use std::time::{Duration, Instant};

use flavite_graph::{
    Delta, EdgeDelta, EdgeKind, GraphModel, GraphSnapshot, Layout, LayoutParams,
    LayoutWorkerConfig, NodeDelta, NodeId,
};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// xorshift64\* — the same generator the B2 fixture uses, so a seed produces
/// the same *kind* of graph here as it does in the probe.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next_u64() % bound.max(1)
    }
}

/// A hub-heavy fixture: Barabási–Albert preferential attachment, `m` edges
/// per arriving node, chosen from a stub list so the degree distribution is
/// power-law — the shape a real vault has, and the shape that stresses the
/// degree-weighted forces.
fn fixture(nodes: usize, edges: usize, seed: u64) -> Arc<GraphSnapshot> {
    let mut model = GraphModel::new();
    let node_deltas: Vec<Delta> = (0..nodes as u64)
        .map(|id| {
            Delta::Node(NodeDelta::Added {
                id: NodeId::note(id),
            })
        })
        .collect();
    for batch in node_deltas.chunks(50_000) {
        model.apply(batch);
    }

    let mut rng = Rng::new(seed);
    let core = nodes.min(8);
    let m = if nodes > core && edges > 0 {
        (edges / (nodes - core)).clamp(1, core)
    } else {
        0
    };

    let mut stubs: Vec<u64> = Vec::with_capacity(edges * 2);
    let mut edge_deltas: Vec<Delta> = Vec::with_capacity(edges);
    for i in 0..core as u64 {
        for j in (i + 1)..core as u64 {
            edge_deltas.push(edge(i, j));
            stubs.push(i);
            stubs.push(j);
        }
    }
    for new_node in core as u64..nodes as u64 {
        if m == 0 {
            break;
        }
        let mut chosen: Vec<u64> = Vec::with_capacity(m);
        for _ in 0..64 {
            if chosen.len() == m {
                break;
            }
            let pick = stubs[rng.below(stubs.len() as u64) as usize];
            if !chosen.contains(&pick) {
                chosen.push(pick);
            }
        }
        for candidate in 0..new_node {
            if chosen.len() == m {
                break;
            }
            if !chosen.contains(&candidate) {
                chosen.push(candidate);
            }
        }
        for target in chosen {
            edge_deltas.push(edge(new_node, target));
            stubs.push(new_node);
            stubs.push(target);
        }
    }
    for batch in edge_deltas.chunks(50_000) {
        model.apply(batch);
    }

    Arc::new(model.snapshot())
}

fn edge(from: u64, to: u64) -> Delta {
    Delta::Edge(EdgeDelta::Added {
        from: NodeId::note(from),
        to: NodeId::note(to),
        kind: EdgeKind::Link,
    })
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// Nearest-rank percentile over an already-sorted slice.
fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let rank = ((p / 100.0) * sorted.len() as f64).ceil() as usize;
    sorted[sorted.len().saturating_sub(1).min(rank.saturating_sub(1))]
}

#[derive(Clone, Copy)]
struct Timing {
    label: &'static str,
    nodes: usize,
    edges: usize,
    samples: usize,
    p50: f64,
    p95: f64,
    p99: f64,
    mean: f64,
    max: f64,
}

fn summarise(label: &'static str, nodes: usize, edges: usize, mut samples: Vec<f64>) -> Timing {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples.len();
    let mean = samples.iter().sum::<f64>() / n.max(1) as f64;
    Timing {
        label,
        nodes,
        edges,
        samples: n,
        p50: percentile(&samples, 50.0),
        p95: percentile(&samples, 95.0),
        p99: percentile(&samples, 99.0),
        mean,
        max: samples.last().copied().unwrap_or(0.0),
    }
}

fn print_table(title: &str, rows: &[Timing]) {
    println!();
    println!("{title}");
    println!(
        "{:<10} {:>8} {:>9} {:>8} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "scenario", "nodes", "edges", "samples", "p50 ms", "p95 ms", "p99 ms", "mean ms", "max ms"
    );
    println!("{}", "-".repeat(86));
    for row in rows {
        println!(
            "{:<10} {:>8} {:>9} {:>8} {:>9.3} {:>9.3} {:>9.3} {:>9.3} {:>9.3}",
            row.label,
            row.nodes,
            row.edges,
            row.samples,
            row.p50,
            row.p95,
            row.p99,
            row.mean,
            row.max
        );
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Args {
    quick: bool,
}

impl Args {
    fn parse() -> Self {
        Self {
            quick: std::env::args().any(|arg| arg == "--quick"),
        }
    }

    /// Samples per scenario (after warmup).
    fn samples(&self) -> usize {
        if self.quick { 40 } else { 200 }
    }

    /// Warmup duration per scenario.
    fn warmup(&self) -> Duration {
        if self.quick {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(400)
        }
    }
}

fn main() {
    let args = Args::parse();
    let cores = std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(4);
    let threads = LayoutWorkerConfig::default().resolved_threads();

    println!("flavite-graph layout benchmark (B3)");
    println!(
        "cores {cores} · rayon pool {threads} (D15: cores − 1) · samples {} · warmup {:?}{}",
        args.samples(),
        args.warmup(),
        if args.quick { " · --quick" } else { "" }
    );
    println!(
        "θ = {:.2} · repulsion 2.0 · attraction 1.0 · gravity 1.0",
        LayoutParams::default().theta
    );

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .expect("the rayon pool builds");

    // --- 1. Per-step cost across the reference scales --------------------
    let mut step_rows = Vec::new();
    for &(nodes, edges) in &[
        (5_000, 15_000),
        (20_000, 60_000),
        (50_000, 200_000),
        (100_000, 400_000),
    ] {
        let snapshot = fixture(nodes, edges, 20_250_101);
        let mut layout = Layout::from_snapshot(&snapshot, LayoutParams::default());

        // Warm up: the pool's threads spin up, the tree arena grows to its
        // working size, and the adaptive speed reaches its operating point —
        // none of which belongs in the sample.
        let warmup_deadline = Instant::now() + args.warmup();
        let mut scratch = Vec::new();
        while Instant::now() < warmup_deadline {
            layout.step_into(&pool, &mut scratch);
        }

        let mut samples = Vec::with_capacity(args.samples());
        for _ in 0..args.samples() {
            let started = Instant::now();
            layout.step_into(&pool, &mut scratch);
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        }
        step_rows.push(summarise("step", nodes, snapshot.edge_count(), samples));
    }
    print_table(
        "layout step (ms) — one Barnes-Hut step, parallel over the pool",
        &step_rows,
    );

    // --- 2. Topology change (the wake path) ------------------------------
    let snapshot = fixture(50_000, 200_000, 20_250_101);
    let mut layout = Layout::from_snapshot(&snapshot, LayoutParams::default());
    let mut scratch = Vec::new();
    for _ in 0..20 {
        layout.step_into(&pool, &mut scratch);
    }
    // A realistic churn: 25 new nodes arrive on top of the existing 50 000.
    // Built once, outside the timer — only the re-target is measured.
    let mut churn = GraphModel::new();
    churn.apply(
        &(0..50_025u64)
            .map(|id| {
                Delta::Node(NodeDelta::Added {
                    id: NodeId::note(id),
                })
            })
            .collect::<Vec<_>>(),
    );
    let next = Arc::new(churn.snapshot());
    let mut retarget_samples = Vec::with_capacity(args.samples());
    for _ in 0..args.samples() {
        let started = Instant::now();
        layout.retarget(&next);
        retarget_samples.push(started.elapsed().as_secs_f64() * 1_000.0);
        // Alternate back so the survivor set keeps moving: 25 arrive, then 25
        // leave, on every round.
        layout.retarget(&snapshot);
    }
    print_table(
        "retarget (ms) — a topology change before stepping resumes",
        &[summarise(
            "retarget",
            50_025,
            snapshot.edge_count(),
            retarget_samples,
        )],
    );

    // --- 3. Cold start to frozen ----------------------------------------
    let mut converge_rows = Vec::new();
    for &(nodes, edges) in &[(2_000, 6_000), (5_000, 15_000)] {
        let snapshot = fixture(nodes, edges, 7);
        let mut samples = Vec::new();
        for _ in 0..if args.quick { 2 } else { 5 } {
            let mut layout = Layout::from_snapshot(&snapshot, LayoutParams::default());
            let started = Instant::now();
            let mut steps = 0_u64;
            while !layout.is_converged() && steps < 200_000 {
                layout.step_into(&pool, &mut scratch);
                steps += 1;
            }
            assert!(
                layout.is_converged(),
                "the layout must freeze within budget"
            );
            samples.push(started.elapsed().as_secs_f64() * 1_000.0);
            print!(
                "  {nodes} nodes: {steps} steps to freeze in {:.3} ms\r",
                samples.last().unwrap()
            );
        }
        println!();
        converge_rows.push(summarise("converge", nodes, edges, samples));
    }
    print_table(
        "cold start → frozen (ms) — wall clock from a fresh layout to the first freeze",
        &converge_rows,
    );

    // --- 4. Throughput summary -------------------------------------------
    println!();
    println!("throughput (from the step table):");
    for row in &step_rows {
        let per_step = row.mean / 1_000.0;
        println!(
            "  {:>7} nodes / {:>7} edges: {:>8.1} steps/s · {:>10.0} node-updates/s · p99 {:>7.3} ms",
            row.nodes,
            row.edges,
            1.0 / per_step,
            row.nodes as f64 / per_step,
            row.p99
        );
    }
    println!();
    println!(
        "AC-16 reference: a layout step must not monopolise a frame; p99 of the \
         50 000-node step is {:.3} ms against a 16.6 ms budget.",
        step_rows
            .iter()
            .find(|row| row.nodes == 50_000)
            .map_or(0.0, |row| row.p99)
    );
}
