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

use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::{Element, Subscription, Task, window};

use flavite_ui::graph::synthetic::SyntheticOptions;
use flavite_ui::graph::{
    Camera, FrameStats, ProbeGeometry, ProbeMessage, ProbeProgram, ProbeTimings, TimingsSnapshot,
    generate, report_header,
};

/// Frames held in each rolling window. 2 048 spans roughly 34 s at 60 fps —
/// long enough for a stable p99, short enough that the table describes the
/// *whole* run rather than only its first seconds.
const FRAME_WINDOW: usize = 2_048;

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

    let geometry = Arc::new(
        ProbeGeometry::from_snapshot(&graph.snapshot, &graph.positions, 1)
            .expect("the synthetic fixture builds geometry cleanly"),
    );

    // Fit the whole map into the boot window, and remember the extent so the
    // script can sweep across all of it rather than a corner.
    let world_extent = graph
        .positions
        .iter()
        .flat_map(|[x, y]| [*x, *y])
        .fold(0.0f32, |extremum, coordinate| {
            extremum.max(coordinate.abs())
        })
        * 2.0;
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
        },
        update,
        view,
    )
    .subscription(subscription)
    .title("flavite graph shader probe — S2-1")
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
}

impl Args {
    fn parse(raw: Vec<String>) -> Self {
        let mut args = Self {
            nodes: 50_000,
            edges: 200_000,
            seconds: 5.0,
            seed: 20_250_101,
            manual: false,
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
                "--manual" => args.manual = true,
                "--help" | "-h" => {
                    println!(
                        "graph-shader-probe [--nodes N] [--edges N] \
                         [--seconds S] [--seed N] [--manual]"
                    );
                    std::process::exit(0);
                }
                other => eprintln!("ignoring unknown argument {other:?}"),
            }
            index += 1;
        }

        args.seconds = args.seconds.clamp(0.25, 3_600.0);
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
    /// World extent the camera fits into the window at boot.
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

    if let Some(previous) = probe.last_frame.replace(at) {
        let gap_ms = (at - previous).as_secs_f32() * 1000.0;
        if gap_ms.is_finite() {
            probe.frame_stats.record(gap_ms);
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
    }

    if !probe.args.manual {
        probe.camera = scripted_camera(started.elapsed(), probe.world_extent);
    }

    // How much of the edge list the current zoom admits. Pure CPU arithmetic
    // on the geometry, so this costs nothing next to the frame it samples.
    let drawn = probe.geometry.visible_edge_count(probe.camera.zoom);
    probe.edges_drawn_min = probe.edges_drawn_min.min(drawn);
    probe.edges_drawn_max = probe.edges_drawn_max.max(drawn);

    if started.elapsed().as_secs_f64() >= probe.args.seconds {
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
    println!("flavite graph shader probe — S2-1");
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
    println!();
}

fn geometry_edges(probe: &Probe) -> usize {
    probe.geometry.edge_count()
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
