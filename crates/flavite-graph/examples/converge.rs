//! Temporary diagnostic: how long does the force model take to freeze?
//!
//! ```sh
//! cargo run -p flavite-graph --example converge --release
//! ```

use std::time::Instant;

use flavite_graph::{
    Delta, EdgeDelta, EdgeKind, GraphModel, Layout, LayoutParams, NodeDelta, NodeId,
};
use rayon::ThreadPoolBuilder;

fn note(id: u64) -> NodeId {
    NodeId::note(id)
}

fn build(nodes: usize, m: usize) -> flavite_graph::GraphSnapshot {
    let mut model = GraphModel::new();
    let mut deltas: Vec<Delta> = (0..nodes as u64)
        .map(|i| Delta::Node(NodeDelta::Added { id: note(i) }))
        .collect();
    // Preferential attachment, deterministic.
    let mut stubs: Vec<u64> = Vec::new();
    let core = 4.min(nodes);
    for i in 0..core as u64 {
        for j in (i + 1)..core as u64 {
            deltas.push(Delta::Edge(EdgeDelta::Added {
                from: note(i),
                to: note(j),
                kind: EdgeKind::Link,
            }));
            stubs.push(i);
            stubs.push(j);
        }
    }
    let mut pick = 0u64;
    for new in core as u64..nodes as u64 {
        for _ in 0..m {
            if stubs.is_empty() {
                break;
            }
            pick = pick.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let idx = (pick >> 33) as usize % stubs.len();
            let target = stubs[idx];
            if target == new {
                continue;
            }
            deltas.push(Delta::Edge(EdgeDelta::Added {
                from: note(target),
                to: note(new),
                kind: EdgeKind::Link,
            }));
            stubs.push(target);
            stubs.push(new);
        }
    }
    model.apply(&deltas);
    model.snapshot()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let trace = args
        .iter()
        .find(|a| a.starts_with("--trace="))
        .map(|a| a.trim_start_matches("--trace=").to_string());
    let pool = ThreadPoolBuilder::new()
        .num_threads(8)
        .build()
        .expect("pool");
    for (label, nodes, m) in [
        ("path-2", 2usize, 1usize),
        ("star-24", 24, 1),
        ("path-50", 50, 1),
        ("ba-1k", 1_000, 3),
        ("ba-5k", 5_000, 3),
        ("ba-20k", 20_000, 3),
    ] {
        if let Some(want) = &trace
            && want != label
        {
            continue;
        }
        let snapshot = build(nodes, m);
        let mut layout = Layout::from_snapshot(&snapshot, LayoutParams::default());
        let started = Instant::now();
        let mut steps = 0u32;
        let mut stats = None;
        while steps < 100_000 {
            let s = layout.step(&pool).stats;
            steps += 1;
            if steps <= 12 || steps.is_multiple_of(50) {
                let positions = layout.positions();
                let extent = positions
                    .iter()
                    .flat_map(|[x, y]| [*x, *y])
                    .fold(0.0f64, |m, c| m.max(c.abs()));
                let mut closest = f64::INFINITY;
                for (i, a) in positions.iter().enumerate() {
                    for b in positions.iter().skip(i + 1) {
                        closest = closest.min((a[0] - b[0]).hypot(a[1] - b[1]));
                    }
                }
                let first = positions[0];
                println!(
                    "    step {steps:>6}  extent {extent:>9.3}  closest {closest:>9.3e}  force {:>9.3e}  speed {:.4}  eff {:.3}  swing {:>9.3e}  mean {:>9.3e}  p0 [{:>8.4}, {:>8.4}]",
                    s.max_force,
                    s.global_speed,
                    s.speed_efficiency,
                    s.swinging_per_mass,
                    s.mean_displacement,
                    first[0],
                    first[1],
                );
            }
            let done = s.frozen;
            stats = Some(s);
            if done {
                break;
            }
        }
        let elapsed = started.elapsed();
        let stats = stats.expect("at least one step");
        let extent = layout
            .positions()
            .iter()
            .flat_map(|[x, y]| [*x, *y])
            .fold(0.0f64, |m, c| m.max(c.abs()));
        println!(
            "{label:>8}  steps {steps:>6}  {:>8.1?}  frozen {}  mean_disp {:.3e}  max_disp {:.3e}  speed {:.4}  eff {:.3}  extent {:.1}",
            elapsed,
            stats.frozen,
            stats.mean_displacement,
            stats.max_displacement,
            stats.global_speed,
            stats.speed_efficiency,
            extent,
        );
    }
}
