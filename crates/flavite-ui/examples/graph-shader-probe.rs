//! The S2-1 frame-time harness: run the instanced `shader` widget against a
//! synthetic 50 000-node / 200 000-edge graph and measure what it costs.
//!
//! ```sh
//! cargo run -p flavite-ui --example graph-shader-probe -- \
//!     --nodes 50000 --edges 200000 --seconds 5 --seed 20250101
//! ```
//!
//! # What it measures, and why both numbers matter
//!
//! Two tables are printed, because one of them is misleading on its own:
//!
//! - **Frame interval** — wall-clock time between redraws. On any display
//!   this saturates at the refresh rate, so a probe that renders 50 000
//!   nodes in 0.4 ms and one that renders them in 16 ms both report ~60 fps
//!   at 60 Hz. Vsync makes it a *ceiling*, not a measurement.
//! - **Widget CPU** — time actually spent inside the widget's `prepare` and
//!   `draw`, gathered by [`ProbeTimings`] from the widget itself. This is
//!   the number that moves when the renderer gets faster or slower, and it
//!   is what the spike doc (`devdocs/spikes/S2-shader.md`) reports against
//!   the D15 budget.
//!
//! Frame *interval* is still printed, because a p99 far above p50 is how
//! stutter shows up even when the mean sits comfortably inside budget.
//!
//! # Scripted motion
//!
//! By default the camera sweeps a Lissajous path across the map while the
//! zoom pulses, so the run exercises the code paths that differ between
//! frames: the importance cap resizes the edge draw list as the zoom moves,
//! the per-zoom alpha multiplier shifts, and a different set of nodes falls
//! inside the view. A static camera would measure a best case the real
//! product never sits in.
//!
//! `--manual` turns the script off and leaves pan (drag) and zoom (scroll)
//! to the operator.
//!
//! # Layout under load (B3)
//!
//! The run is not static: a [`LayoutWorker`] from `flavite-graph` runs
//! Barnes-Hut ForceAtlas2 on a background pool while the window renders, so
//! this probe exercises AC-16's *"while layout runs"* clause that S2-1 could
//! not (see `devdocs/spikes/S2-shader.md` §6.3). Three things make that a
//! fair test rather than a token one:
//!
//! - **Geometry is rebuilt off-thread.** The worker publishes a
//!   [`PositionFrame`](flavite_graph::PositionFrame) per step; a builder
//!   thread turns the newest one into a [`ProbeGeometry`] at most every
//!   [`GEOMETRY_THROTTLE`], and the UI thread adopts whatever is newest at
//!   the tick. The render thread therefore never pays the ~0.1 s CPU cost of
//!   rebuilding 50 000 nodes / 200 000 edges — only the one-frame instance
//!   upload the new generation triggers (S2-shader §5(d)).
//! - **Frame p99 is recorded twice**: over the whole run, and over only the
//!   intervals whose tick saw the engine in a non-frozen state. The second
//!   window is the AC-16 number.
//! - **Topology changes are injected every `--delta-every` seconds**, each
//!   one adding a note and two edges. A delta wakes a frozen engine, so a
//!   long run exercises install → warm start → refreeze rather than a single
//!   convergence.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::{Element, Subscription, Task, window};

use flavite_graph::{
    Delta, EdgeDelta, EdgeKind, LayoutCommand, LayoutState, LayoutWorker, LayoutWorkerConfig,
    NodeDelta, NodeId, PositionBuffer,
};
use flavite_ui::graph::synthetic::SyntheticOptions;
use flavite_ui::graph::{
    Camera, FrameStats, ProbeGeometry, ProbeMessage, ProbeProgram, ProbeTimings, TimingsSnapshot,
    generate, report_header,
};

/// Frames held in each rolling window. 2 048 spans roughly 34 s at 60 fps —
/// long enough for a stable p99, short enough that the table describes the
/// *whole* run rather than only its first seconds.
const FRAME_WINDOW: usize = 2_048;

/// How often the geometry builder re-checks the position buffer when there
/// is nothing new to build. A layout step costs ~10 ms, so a 25 ms poll adds
/// at most two steps of latency to a rebuild that itself costs ~100 ms.
const GEOMETRY_POLL: Duration = Duration::from_millis(25);

/// Minimum spacing between geometry rebuilds. The rebuild is off-thread, but
/// it is not free: at 50 000 nodes it costs ~0.1 s of CPU that would
/// otherwise go to the layout pool, and every rebuild makes the UI thread
/// re-upload the whole instance buffer. One rebuild per layout *frame* would
/// spend the whole run rebuilding; 100 ms tracks a converged layout's frame
/// rate closely enough that the rendered map stays live.
const GEOMETRY_THROTTLE: Duration = Duration::from_millis(100);

/// How long to wait for the worker's first published frame before falling
/// back to the fixture's own positions. Bounded so a pathological machine
/// degrades to the S2-1 behaviour instead of hanging before the window opens.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// SPEC §8 AC-16's frame budget, in milliseconds: p99 ≤ 16.6 ms, i.e. a
/// 60 Hz frame with a millisecond of slack.
const AC16_BUDGET_MS: f32 = 16.6;

const BOOTSTRAP_WIDTH: f32 = 1280.0;
const BOOTSTRAP_HEIGHT: f32 = 720.0;

/// Centre of the scripted zoom sweep, in world units → logical pixels.
///
/// The sweep is defined in *absolute* zoom, not as a multiple of the fitted
/// zoom, because the D15 importance cap is a function of absolute zoom
/// ([`edge_cap`]): `100 000 · zoom`, so the full ~200 000-edge list is only
/// drawn once `zoom ≥ 2`. A probe that swept relative to the fit zoom would
/// never reach that regime on a large map and would silently measure a
/// trimmed draw list instead of the one the exit criteria are about.
///
/// [`edge_cap`]: flavite_ui::graph::edge_cap
const ZOOM_SWEEP_BASE: f32 = 0.85;
/// Half-width of the sweep, in octaves. 1.4 puts the range at roughly
/// 0.32×–2.24×: the cap runs from ~32 000 edges up to the full 199 996.
const ZOOM_SWEEP_OCTAVES: f32 = 1.4;

fn main() -> iced::Result {
    let args = Arc::new(Args::parse(std::env::args().skip(1).collect()));

    eprintln!(
        "generating {} nodes / {} edges (seed {})…",
        args.nodes, args.edges, args.seed
    );
    let started = Instant::now();
    let graph = generate(&SyntheticOptions {
        nodes: args.nodes,
        edges: args.edges,
        seed: args.seed,
        ..SyntheticOptions::probe_default()
    });
    eprintln!(
        "…{} nodes / {} edges in {:.2}s",
        graph.snapshot.node_count(),
        graph.edges_generated,
        started.elapsed().as_secs_f32()
    );

    // --- B3: the layout runs before the window opens --------------------
    //
    // The worker is spawned already running, then handed the fixture as its
    // first snapshot. Booting from the layout's *first published frame*
    // rather than from the fixture's phyllotaxis means the very first frame
    // the window draws is one the product would actually show.
    let snapshot = Arc::new(graph.snapshot.clone());
    let worker = LayoutWorker::spawn_running(LayoutWorkerConfig {
        threads: args.layout_threads,
        ..LayoutWorkerConfig::default()
    })
    .expect("the layout worker spawns");
    worker
        .send(LayoutCommand::Replace {
            snapshot: Arc::clone(&snapshot),
            positions: graph.positions.clone(),
            warm: true,
        })
        .expect("the initial snapshot installs");

    let buffer = worker.positions();
    let deadline = Instant::now() + FIRST_FRAME_TIMEOUT;
    let boot_generation;
    let geometry = {
        let first = loop {
            let frame = buffer.load();
            if frame.generation >= 1 {
                break Some(frame);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        match first {
            Some(frame) => {
                boot_generation = frame.generation;
                ProbeGeometry::from_snapshot(&frame.snapshot, &frame.positions, frame.generation)
            }
            None => {
                eprintln!(
                    "no layout frame within {FIRST_FRAME_TIMEOUT:?} — \
                     booting from the fixture's own positions"
                );
                boot_generation = 1;
                ProbeGeometry::from_snapshot(&snapshot, &graph.positions, 1)
            }
        }
        .expect("the fixture builds geometry cleanly")
    };
    let geometry = Arc::new(geometry);

    // The builder thread's hand-off: newest geometry wins, and the UI thread
    // only ever *reads* the slot, so a rebuild in progress can never stall a
    // frame (D15's single-authority rule, applied to the geometry instead of
    // the positions).
    let geometry_slot = Arc::new(Mutex::new(Arc::clone(&geometry)));
    let geometry_stats = Arc::new(Mutex::new(GeometryStats {
        published_generation: boot_generation,
        ..GeometryStats::default()
    }));
    let builder_shutdown = Arc::new(AtomicBool::new(false));
    {
        let slot = Arc::clone(&geometry_slot);
        let stats = Arc::clone(&geometry_stats);
        let shutdown = Arc::clone(&builder_shutdown);
        let buffer = Arc::clone(&buffer);
        std::thread::Builder::new()
            .name("probe-geometry".into())
            .spawn(move || build_geometry_loop(buffer, slot, stats, shutdown))
            .expect("the geometry builder thread spawns");
    }

    // Fit the whole map into the boot window, and remember the extent so the
    // script can sweep across all of it rather than a corner. The extent is
    // refreshed whenever a newer geometry lands: a layout that spreads the
    // fixture wider than the phyllotaxis disc would otherwise leave the
    // camera sweeping a corner of the map.
    let world_extent = extent_of(&geometry);
    let mut camera = Camera::centered();
    camera.fit_extent(
        world_extent,
        [0.0, 0.0],
        [BOOTSTRAP_WIDTH, BOOTSTRAP_HEIGHT],
    );
    let fit_zoom = camera.zoom;

    eprintln!(
        "geometry   {} nodes / {} edges  |  world extent {:.0} units, fitted zoom {:.4}",
        geometry.node_count(),
        geometry.edge_count(),
        world_extent,
        fit_zoom,
    );

    iced::application(
        move || Probe {
            geometry: Arc::clone(&geometry),
            camera,
            timings: Arc::new(ProbeTimings::default()),
            args: Arc::clone(&args),
            fit_zoom,
            world_extent,
            edges_drawn_min: geometry.edge_count(),
            edges_drawn_max: geometry.edge_count(),
            window_size: [BOOTSTRAP_WIDTH, BOOTSTRAP_HEIGHT],
            frame_stats: FrameStats::with_capacity(FRAME_WINDOW),
            widget_stats: FrameStats::with_capacity(FRAME_WINDOW),
            last_frame: None,
            last_widget_total: Duration::ZERO,
            started: None,
            reported: false,
            worker,
            geometry_slot,
            geometry_stats,
            builder_shutdown,
            layout_frame_stats: FrameStats::with_capacity(FRAME_WINDOW),
            layout_widget_stats: FrameStats::with_capacity(FRAME_WINDOW),
            last_layout_state: LayoutState::Active,
            last_delta: None,
            deltas_sent: 0,
            next_note: args.nodes as u64,
        },
        update,
        view,
    )
    .subscription(subscription)
    .title("flavite graph shader probe — S2-1/B3")
    .window(window::Settings {
        size: iced::Size::new(BOOTSTRAP_WIDTH, BOOTSTRAP_HEIGHT),
        ..window::Settings::default()
    })
    .run()
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Args {
    nodes: usize,
    edges: usize,
    seconds: f64,
    seed: u64,
    manual: bool,
    /// Seconds between injected topology deltas. 0 disables injection.
    delta_every: f64,
    /// Layout pool size. 0 = auto: `available_parallelism − 1`, the same
    /// rule `LayoutWorkerConfig::resolved_threads` applies, but spelled out
    /// here so a run can compare `n−1` against `n−2` and show what leaving
    /// a core for the UI thread is worth.
    layout_threads: usize,
}

impl Args {
    fn parse(raw: Vec<String>) -> Self {
        let mut args = Self {
            nodes: 50_000,
            edges: 200_000,
            seconds: 5.0,
            seed: 20_250_101,
            manual: false,
            delta_every: 5.0,
            layout_threads: 0,
        };

        let mut index = 0usize;
        while index < raw.len() {
            let flag = raw[index].clone();
            let next = |index: &mut usize| -> Option<String> {
                *index += 1;
                raw.get(*index).cloned()
            };
            match flag.as_str() {
                "--nodes" => {
                    args.nodes = next(&mut index)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(args.nodes)
                }
                "--edges" => {
                    args.edges = next(&mut index)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(args.edges)
                }
                "--seconds" => {
                    args.seconds = next(&mut index)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(args.seconds)
                }
                "--seed" => {
                    args.seed = next(&mut index)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(args.seed)
                }
                "--delta-every" => {
                    args.delta_every = next(&mut index)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(args.delta_every)
                }
                "--layout-threads" => {
                    args.layout_threads = next(&mut index)
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(args.layout_threads)
                }
                "--manual" => args.manual = true,
                "--help" | "-h" => {
                    println!(
                        "graph-shader-probe [--nodes N] [--edges N] \
                         [--seconds S] [--seed N] [--delta-every S] \
                         [--layout-threads N] [--manual]"
                    );
                    std::process::exit(0);
                }
                other => eprintln!("ignoring unknown argument {other:?}"),
            }
            index += 1;
        }

        args.seconds = args.seconds.clamp(0.25, 3_600.0);
        args.delta_every = args.delta_every.clamp(0.0, 3_600.0);
        args
    }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

struct Probe {
    geometry: Arc<ProbeGeometry>,
    camera: Camera,
    timings: Arc<ProbeTimings>,
    args: Arc<Args>,

    /// Zoom that fits the whole map — reported, not scripted against.
    fit_zoom: f32,
    /// World extent the camera fits into the window at boot, refreshed when
    /// a newer geometry lands (see `main`).
    world_extent: f32,
    /// Smallest / largest edge draw list the run actually issued. This is
    /// what proves the run covered the D15 cap curve rather than one point
    /// on it.
    edges_drawn_min: usize,
    edges_drawn_max: usize,
    /// Logical window size, needed to anchor scroll-zoom at the cursor.
    window_size: [f32; 2],

    /// Wall-clock gap between redraws, in milliseconds.
    frame_stats: FrameStats,
    /// Widget CPU per frame, in milliseconds.
    widget_stats: FrameStats,
    last_frame: Option<Instant>,
    last_widget_total: Duration,
    started: Option<Instant>,
    reported: bool,

    // --- B3: layout under load -----------------------------------------
    /// The running engine: telemetry, position buffer, delta injection.
    worker: LayoutWorker,
    /// Newest geometry published by the builder thread.
    geometry_slot: Arc<Mutex<Arc<ProbeGeometry>>>,
    /// Builder-thread counters, read once at report time.
    geometry_stats: Arc<Mutex<GeometryStats>>,
    /// Told to stop when the run reports.
    builder_shutdown: Arc<AtomicBool>,
    /// Frame intervals sampled while the engine was not frozen — the AC-16
    /// window. Separate from `frame_stats` because a p99 over a run whose
    /// last 20 s were frozen would describe a renderer, not a renderer under
    /// layout load.
    layout_frame_stats: FrameStats,
    /// Widget CPU over the same intervals.
    layout_widget_stats: FrameStats,
    /// Engine state at the previous tick, so an interval is attributed to
    /// the layout if *either* end of it saw the engine running.
    last_layout_state: LayoutState,
    /// When the last topology delta was injected.
    last_delta: Option<Instant>,
    deltas_sent: usize,
    /// Next free logical note id for injected deltas.
    next_note: u64,
}

#[derive(Debug, Clone, Copy)]
enum Message {
    Probe(ProbeMessage),
    Tick(Instant),
}

fn update(probe: &mut Probe, message: Message) -> Task<Message> {
    match message {
        Message::Probe(probe_message) => {
            match probe_message {
                ProbeMessage::Viewport { size } => probe.window_size = size,
                ProbeMessage::PanByPixels { .. } => {
                    // Dragging right by `dx` logical pixels must drag the
                    // world left; the message does that division through the
                    // zoom it is given.
                    if let Some([dx_world, dy_world]) =
                        probe_message.pan_world_at(probe.camera.zoom)
                    {
                        probe.camera.center[0] += dx_world;
                        probe.camera.center[1] += dy_world;
                    }
                }
                ProbeMessage::ZoomAt { anchor, factor } => {
                    // The widget's bounds are the whole window, origin 0,0.
                    probe
                        .camera
                        .zoom_at(anchor, [0.0, 0.0], probe.window_size, factor);
                }
                ProbeMessage::ScaleFactor { .. } => {}
            }
            Task::none()
        }
        Message::Tick(at) => tick(probe, at),
    }
}

fn tick(probe: &mut Probe, at: Instant) -> Task<Message> {
    if probe.reported {
        return Task::none();
    }

    // The clock starts on the first redraw, so the window's own first-frame
    // hitch (surface creation, pipeline compilation) is measured rather than
    // hidden — it belongs in p99.
    let started = *probe.started.get_or_insert(at);

    // Adopt whatever the builder published since the last tick. This swaps a
    // pointer and re-fits the sweep's extent; the instance-buffer upload the
    // new generation triggers happens inside the widget's next `prepare`,
    // where it is measured rather than hidden.
    let latest = probe
        .geometry_slot
        .lock()
        .expect("the geometry slot is not poisoned")
        .clone();
    if latest.generation() != probe.geometry.generation() {
        probe.geometry = latest;
        probe.world_extent = extent_of(&probe.geometry);
    }

    // An interval belongs to the *layout* window if either of its endpoints
    // saw the engine running: excluding the frame in which it froze would
    // bias the window towards idle frames and understate p99.
    let telemetry = probe.worker.telemetry();
    let state = LayoutState::from_code(telemetry.state);
    let layout_running =
        state != LayoutState::Frozen || probe.last_layout_state != LayoutState::Frozen;
    probe.last_layout_state = state;

    if let Some(previous) = probe.last_frame.replace(at) {
        let gap_ms = (at - previous).as_secs_f32() * 1000.0;
        if gap_ms.is_finite() {
            probe.frame_stats.record(gap_ms);
            if layout_running {
                probe.layout_frame_stats.record(gap_ms);
            }
        }
    }

    // Widget CPU for the frames that completed since the last tick.
    let snapshot = probe.timings.snapshot();
    let delta = snapshot
        .widget_total()
        .saturating_sub(probe.last_widget_total);
    probe.last_widget_total = snapshot.widget_total();
    let delta_ms = delta.as_secs_f32() * 1000.0;
    if delta_ms.is_finite() {
        probe.widget_stats.record(delta_ms);
        if layout_running {
            probe.layout_widget_stats.record(delta_ms);
        }
    }

    if !probe.args.manual {
        probe.camera = scripted_camera(started.elapsed(), probe.world_extent);
    }

    // How much of the edge list the current zoom admits. Pure CPU arithmetic
    // on the geometry, so this costs nothing next to the frame it samples.
    let drawn = probe.geometry.visible_edge_count(probe.camera.zoom);
    probe.edges_drawn_min = probe.edges_drawn_min.min(drawn);
    probe.edges_drawn_max = probe.edges_drawn_max.max(drawn);

    // --- B3: inject a topology change on a timer ------------------------
    // Each delta adds one note and one or two edges. Against a converged
    // engine that is a wake-up (topology change → unfreeze → warm start),
    // which is the freeze/wake path the layout unit tests cover only in
    // isolation, and it grows the fixture so the rebuild path is exercised
    // with a changing node count too.
    if probe.args.delta_every > 0.0 {
        let interval = Duration::from_secs_f64(probe.args.delta_every);
        let due = match probe.last_delta {
            None => started.elapsed() >= interval,
            Some(previous) => at.duration_since(previous) >= interval,
        };
        if due {
            probe.last_delta = Some(at);
            let delta = next_delta(probe.next_note, probe.next_note > 0);
            if probe.worker.send(LayoutCommand::Apply { delta }).is_ok() {
                probe.deltas_sent += 1;
                probe.next_note += 1;
            }
        }
    }

    if started.elapsed().as_secs_f64() >= probe.args.seconds {
        // Stop the worker and the builder before reporting, so the table
        // describes a run that ended cleanly rather than one the process was
        // killed in the middle of.
        let _ = probe.worker.send(LayoutCommand::Shutdown);
        probe.builder_shutdown.store(true, Ordering::Relaxed);
        report(probe, &snapshot);
        probe.reported = true;
        return iced::exit();
    }

    Task::none()
}

/// Sweep the map and sweep the zoom across the whole D15 cap curve, so no
/// two frames do the same work and the run is not a single-point sample.
fn scripted_camera(elapsed: Duration, world_extent: f32) -> Camera {
    let t = elapsed.as_secs_f32();
    let octaves = ZOOM_SWEEP_OCTAVES * (t * 0.13).sin();
    Camera {
        center: [
            world_extent * 0.30 * (t * 0.19).sin(),
            world_extent * 0.30 * (t * 0.271).sin(),
        ],
        zoom: (ZOOM_SWEEP_BASE * 2.0f32.powf(octaves)).clamp(Camera::ZOOM_MIN, Camera::ZOOM_MAX),
    }
}

fn report(probe: &Probe, timings: &TimingsSnapshot) {
    let args = &*probe.args;

    println!();
    println!("flavite graph shader probe — S2-1 / B3 (layout under load)");
    println!(
        "fixture   {:>7} nodes / {:>7} edges (seed {})",
        probe.geometry.node_count(),
        geometry_edges(probe),
        args.seed,
    );
    println!(
        "window    {:>7} × {:<7} logical  |  zoom {:.4} of fitted {:.4}",
        probe.window_size[0], probe.window_size[1], probe.camera.zoom, probe.fit_zoom,
    );
    println!(
        "mode      {:>8}, ran {:.2}s",
        if args.manual { "manual" } else { "scripted" },
        args.seconds,
    );
    println!(
        "edges drawn {:>6} … {:<6} of {} (D15 importance cap varies with zoom)",
        probe.edges_drawn_min,
        probe.edges_drawn_max,
        geometry_edges(probe),
    );
    println!();

    // --- B3: what the engine and the builder did during the run ---------
    let telemetry = probe.worker.telemetry();
    let builder = probe
        .geometry_stats
        .lock()
        .expect("the geometry stats are not poisoned");
    println!("layout engine (B3)");
    println!(
        "  state {:>8}  |  {} threads  |  {} steps ({:.1}/s)  |  {} topology changes  |  {} panics",
        LayoutState::from_code(telemetry.state),
        telemetry.threads,
        telemetry.steps,
        telemetry.steps_per_second,
        telemetry.topology_changes,
        telemetry.panics,
    );
    println!(
        "  geometry {} rebuilds  |  build total {:?}, max {:?}  |  generation {}",
        builder.builds, builder.total, builder.max, builder.published_generation,
    );
    println!(
        "  deltas   {} injected (one note + 1–2 edges each, every {:.1}s)",
        probe.deltas_sent, args.delta_every,
    );
    drop(builder);

    println!();
    println!("frame interval (ms) — capped by the display refresh rate");
    println!("{}", report_header());
    println!(
        "{}",
        probe.frame_stats.snapshot().report_row("frame interval")
    );

    println!();
    println!("widget cpu per frame (ms) — the number that is not vsync-capped");
    println!("{}", report_header());
    println!("{}", probe.widget_stats.snapshot().report_row("widget cpu"));

    println!();
    println!(
        "frames {}  |  widget total {:?}  |  prepare {} calls  |  draw {} calls",
        probe.frame_stats.len(),
        timings.widget_total(),
        timings.prepare_calls,
        timings.draw_calls,
    );

    let interval = probe.frame_stats.snapshot();
    let widget = probe.widget_stats.snapshot();
    println!(
        "p50 {:.3} ms ({:.0} fps)  p99 {:.3} ms  |  widget p50 {:.3} ms  p99 {:.3} ms",
        interval.p50_ms,
        interval.p50_fps(),
        interval.p99_ms,
        widget.p50_ms,
        widget.p99_ms,
    );

    // --- AC-16: the clause S2-1 could not exercise ----------------------
    println!();
    if probe.layout_frame_stats.is_empty() {
        println!("frame interval while the layout was running: no samples");
        println!("  (the engine reported Frozen at every tick — nothing to measure)");
        return;
    }
    println!("frame interval while the layout was running (ms) — AC-16");
    println!("{}", report_header());
    println!(
        "{}",
        probe
            .layout_frame_stats
            .snapshot()
            .report_row("frame (layout)")
    );
    println!(
        "{}",
        probe
            .layout_widget_stats
            .snapshot()
            .report_row("widget (layout)")
    );
    let layout = probe.layout_frame_stats.snapshot();
    let layout_widget = probe.layout_widget_stats.snapshot();
    let verdict = if layout.p99_ms <= AC16_BUDGET_MS {
        "PASS"
    } else {
        "FAIL"
    };
    println!(
        "layout frames {}  |  p50 {:.3} ms  p99 {:.3} ms  |  widget p99 {:.3} ms",
        probe.layout_frame_stats.len(),
        layout.p50_ms,
        layout.p99_ms,
        layout_widget.p99_ms,
    );
    println!(
        "AC-16  frame p99 {:.3} ms ≤ {AC16_BUDGET_MS} ms while layout runs → {verdict}",
        layout.p99_ms,
    );
    println!(
        "       frame interval is vsync-quantized (S2-shader §5b); widget p99 is \
         the ceiling-free companion number"
    );
}

fn geometry_edges(probe: &Probe) -> usize {
    probe.geometry.edge_count()
}

/// Largest absolute world coordinate in a geometry, doubled to a full width.
///
/// O(nodes), but only ever run when a new geometry lands (a few times per
/// second at most) — never per frame.
fn extent_of(geometry: &ProbeGeometry) -> f32 {
    geometry
        .nodes()
        .iter()
        .flat_map(|node| node.center)
        .fold(0.0f32, |extremum, coordinate| {
            extremum.max(coordinate.abs())
        })
        * 2.0
}

/// What the geometry builder did over the run. Read once, when reporting.
#[derive(Debug, Default)]
struct GeometryStats {
    /// Rebuilds published to the slot (the boot geometry is not counted).
    builds: usize,
    /// Wall-clock time spent inside `ProbeGeometry::from_snapshot`.
    total: Duration,
    /// Slowest single rebuild — the number S2-shader §5(d) is about.
    max: Duration,
    /// Generation of the geometry currently published.
    published_generation: u64,
}

/// The builder thread: turn the newest published position frame into
/// geometry, newest-wins, at most once per [`GEOMETRY_THROTTLE`].
///
/// Everything expensive happens here rather than on the UI thread. The slot
/// is only ever *replaced*, never mutated in place, so a reader holding an
/// `Arc` (the widget, mid-frame) is unaffected by a rebuild finishing.
fn build_geometry_loop(
    buffer: Arc<PositionBuffer>,
    slot: Arc<Mutex<Arc<ProbeGeometry>>>,
    stats: Arc<Mutex<GeometryStats>>,
    shutdown: Arc<AtomicBool>,
) {
    let mut last_built = slot
        .lock()
        .expect("the geometry slot is not poisoned")
        .generation();
    let mut last_finished: Option<Instant> = None;

    while !shutdown.load(Ordering::Relaxed) {
        let mut frame = buffer.load();
        if frame.generation <= last_built {
            std::thread::sleep(GEOMETRY_POLL);
            continue;
        }

        // Throttle: a rebuild costs ~0.1 s of CPU at the AC-16 fixture size,
        // and every rebuild makes the UI re-upload the instance buffers. Pay
        // the remainder of the window, then take whatever frame is newest —
        // waiting for the one that triggered the throttle would add latency
        // for no information.
        if let Some(previous) = last_finished {
            let wait = GEOMETRY_THROTTLE.saturating_sub(previous.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
                let newer = buffer.load();
                if newer.generation > frame.generation {
                    frame = newer;
                }
            }
        }

        let started = Instant::now();
        let built =
            ProbeGeometry::from_snapshot(&frame.snapshot, &frame.positions, frame.generation);
        last_finished = Some(Instant::now());
        last_built = frame.generation;

        match built {
            Ok(geometry) => {
                let took = started.elapsed();
                *slot.lock().expect("the geometry slot is not poisoned") = Arc::new(geometry);
                let mut stats = stats.lock().expect("the geometry stats are not poisoned");
                stats.builds += 1;
                stats.total += took;
                stats.max = stats.max.max(took);
                stats.published_generation = frame.generation;
            }
            Err(error) => {
                // Counted as consumed so a fixture the renderer cannot
                // build cannot spin the loop; the run carries on with the
                // last geometry that did build.
                eprintln!(
                    "geometry rebuild failed at generation {}: {error}",
                    frame.generation
                );
            }
        }
    }
}

/// One injected topology change: a new note plus edges into the graph that
/// already exists.
///
/// `anchor` says whether the fixture had any notes to attach to — a 0-node
/// fixture starts from nothing, so its first delta is a lone note rather
/// than an edge whose endpoint does not exist yet.
fn next_delta(note: u64, anchor: bool) -> Delta {
    let id = NodeId::note(note);
    let mut edges = Vec::new();
    if anchor {
        edges.push(EdgeDelta::Upsert {
            from: NodeId::note(0),
            to: id,
            kind: EdgeKind::Link,
        });
        if note > 0 {
            edges.push(EdgeDelta::Upsert {
                from: NodeId::note(note - 1),
                to: id,
                kind: EdgeKind::Link,
            });
        }
    }
    Delta {
        nodes: vec![NodeDelta::Added { id }],
        edges,
    }
}

fn view(probe: &Probe) -> Element<'_, Message> {
    // The widget publishes `ProbeMessage`s; the application owns the camera,
    // so it maps them back into its own message type rather than letting the
    // widget hold mutable camera state (D15's single-authority rule).
    Element::new(
        ProbeProgram::new(
            Arc::clone(&probe.geometry),
            probe.camera,
            Arc::clone(&probe.timings),
        )
        .widget(),
    )
    .map(Message::Probe)
}

fn subscription(probe: &Probe) -> Subscription<Message> {
    if probe.reported {
        Subscription::none()
    } else {
        window::frames().map(Message::Tick)
    }
}
