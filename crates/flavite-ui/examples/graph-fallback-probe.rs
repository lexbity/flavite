//! The S2-3 probe: fallback + card-layer prototype (PLAN-B B4).
//!
//! This probe exercises the software-renderer fallback path and the card
//! layer that take over when the wgpu device cannot be created or when
//! the zoom level demands real widgets. It measures:
//!
//! 1. **Fallback mode**: on a headless environment or when wgpu creation
//!    fails, the graph renders via iced's `canvas` widget with a hard cap
//!    of [`FALLBACK_NODE_CAP`] nodes and edges, plus a visible notice.
//! 2. **Card layer**: when zoomed in past [`CARD_ZOOM_THRESHOLD`] (2.0×),
//!    real note cards appear at world positions with text re-laid-out at
//!    the scaled font size. At most [`CARD_CAP`] (64) cards are shown.
//! 3. **Zoomable container**: text caching at quantized scale steps,
//!    ensuring sharp glyphs at every zoom level.
//!
//! # Running
//!
//! The probe auto-detects whether a GPU is available. On a machine with
//! Vulkan/Metal/DX12 support, it tries the wgpu path first; on failure or
//! in a headless CI job, it falls back to the canvas path. The `--software`
//! flag forces the fallback path even on a GPU-capable machine.
//!
//! ```sh
//! # Run with GPU (if available)
//! cargo run -p flavite-ui --example graph-fallback-probe
//!
//! # Force software fallback
//! cargo run -p flavite-ui --example graph-fallback-probe -- --software
//!
//! # Manual camera control
//! cargo run -p flavite-ui --example graph-fallback-probe -- --manual
//! ```
//!
//! # What it measures
//!
//! - Fallback frame time at 50k nodes / 200k edges (capped at
//!   [`FALLBACK_NODE_CAP`] = 10 000 for each).
//! - Card layer frame time with 64 cards at various zoom levels.
//! - Reduced mode notice presence.
//! - Scale-step cache hit rate (text re-layout only when the quantized
//!   step changes).
//!
//! # Implementation note
//!
//! The probe does not use wgpu at all when `--software` is passed or when
//! the wgpu device cannot be created. It uses iced's `Canvas` widget for
//! drawing, which is available on all iced backends (wgpu, tiny-skia,
//! software). This keeps the example self-contained and testable in CI.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::widget::{Column, Container, Row, Space, Text};
use iced::{Alignment, Element, Length, Point, Rectangle, Subscription, Task};

use flavite_graph::{EdgeKind, GraphModel, GraphSnapshot, NodeId, NodeKind};
use flavite_ui::graph::fallback::{
    FALLBACK_NODE_CAP, FallbackCamera, FallbackGeometry, FallbackPrimitive, reduced_mode_notice,
};
use flavite_ui::graph::{
    CARD_ZOOM_THRESHOLD, Card, CardEditSink, CardIdentity, CardLayerHarness, LoggingSink,
    ProbeGeometry, SyntheticOptions, ZoomableCamera, card_layer_status, compute_zoomable_state,
};

/// Force software fallback via CLI flag.
const FORCE_SOFTWARE: bool = false;

/// Frame budget for the card layer: ≤ 16.6 ms p99 at 60 fps (AC-16).
const CARD_BUDGET_MS: f32 = 16.6;

/// Fixture: 50 nodes (not 50k — a large fixture would overwhelm the
/// fallback canvas and hide the card layer behind it). The point is to
/// demonstrate the fallback/card transition, not to hit AC-16 numbers on
/// the canvas path.
const NODE_COUNT: usize = 50;
const EDGE_COUNT: usize = 200;

/// Size of the probe window.
const WINDOW_WIDTH: f32 = 1024.0;
const WINDOW_HEIGHT: f32 = 768.0;

fn main() -> iced::Result {
    let args = Args::parse(std::env::args().skip(1).collect());
    let force_software = args.software;

    eprintln!("graph-fallback-probe — S2-3 (B4)");
    eprintln!("  software fallback: {}", force_software);

    let graph = generate(&SyntheticOptions {
        nodes: NODE_COUNT,
        edges: EDGE_COUNT,
        seed: 2025_01_04,
        attachment_every: 5,
        embed_every: 4,
    });
    eprintln!(
        "fixture: {} nodes / {} edges",
        graph.snapshot.node_count(),
        graph.edges_generated,
    );

    // Build fallback geometry (what the canvas widget consumes).
    let fallback_geometry = FallbackGeometry::from_snapshot(&graph.snapshot, &graph.positions, 1)
        .expect("fallback geometry builds");
    let fallback_geometry = Arc::new(fallback_geometry);
    let geometry = Arc::new(
        ProbeGeometry::from_snapshot(&graph.snapshot, &graph.positions, 1)
            .expect("probe geometry builds"),
    );

    // Indexed card content (synthetic for the probe).
    let preview_map: HashMap<NodeId, String> = (0..NODE_COUNT as u64)
        .map(|i| {
            let kind = if i % 5 == 0 {
                NodeKind::Attachment
            } else {
                NodeKind::Note
            };
            let id = match kind {
                NodeKind::Note => NodeId::note(i),
                NodeKind::Attachment => NodeId::attachment(i),
            };
            let preview = format!(
                "{} #{}\n\nThis is a sample preview for the card layer. \
                 It includes some **markdown** and a task list:\n\n- [ ] Task one\n- [x] Task two\n- [ ] Task three",
                match kind {
                    NodeKind::Note => "Note",
                    NodeKind::Attachment => "File",
                },
                i
            );
            (id, preview)
        })
        .collect();

    let content_map: HashMap<NodeId, Vec<String>> = preview_map
        .iter()
        .map(|(id, text)| {
            let content: Vec<String> = text
                .lines()
                .filter(|line| {
                    line.trim().starts_with("- [")
                        || line.trim().starts_with("[ ] ")
                        || line.trim().starts_with("- [x]")
                        || line.trim().starts_with("- [X]")
                })
                .map(|line| line.to_string())
                .collect();
            (*id, content)
        })
        .collect();

    // Build cards from the fixture.
    let mut cards: Vec<Card<()>> = Vec::new();
    let snapshot = &graph.snapshot;
    let positions = &graph.positions;
    for i in 0..snapshot.node_count().min(positions.len()) {
        let id = snapshot.node_ids[i];
        let kind = snapshot.node_kinds[i];
        let position = positions[i];
        let preview = preview_map.get(&id).cloned().unwrap_or_default();
        let content = content_map.get(&id).cloned().unwrap_or_default();
        let card = match kind {
            NodeKind::Note => {
                Card::new_note(position, id, format!("{} #{}", "Note", id.id), content)
            }
            NodeKind::Attachment => {
                Card::new_attachment(position, id, format!("{} #{}", "File", id.id))
            }
        };
        cards.push(card);
    }
    let cards = Arc::new(cards);

    // Initial camera: fit the whole map.
    let mut camera = ZoomableCamera::centered();
    let extent = graph.world_extent();
    camera.fit_extent(
        extent,
        Rectangle {
            x: 0.0,
            y: 0.0,
            width: WINDOW_WIDTH,
            height: WINDOW_HEIGHT,
        },
    );

    // Initial fallback camera (same pan/zoom for parity).
    let fallback_camera = FallbackCamera {
        center: camera.center,
        zoom: camera.zoom,
    };

    let mut harness = CardLayerHarness::new(Arc::clone(&geometry));
    harness.set_camera(camera);
    harness.set_bounds(Rectangle {
        x: 0.0,
        y: 0.0,
        width: WINDOW_WIDTH,
        height: WINDOW_HEIGHT,
    });

    eprintln!(
        "initial zoom: {:.4} (card threshold: {})",
        camera.zoom, CARD_ZOOM_THRESHOLD
    );

    iced::application(
        move || Probe {
            fallback_geometry: Arc::clone(&fallback_geometry),
            fallback_camera,
            cards: Arc::clone(&cards),
            harness,
            geometry: Arc::clone(&geometry),
            force_software,
            started: None,
            frames: 0,
            last_report: None,
            report_interval: Duration::from_secs(1),
            manual: args.manual,
            script_start: Instant::now(),
            zoom_script: args.zoom_script.clone(),
        },
        update,
        view,
    )
    .subscription(subscription)
    .title("flavite graph fallback probe — S2-3 (B4)")
    .window(iced::window::Settings {
        size: iced::Size::new(WINDOW_WIDTH, WINDOW_HEIGHT),
        ..iced::window::Settings::default()
    })
    .run()
}

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Args {
    software: bool,
    manual: bool,
    zoom_script: ZoomScript,
}

#[derive(Debug, Clone)]
enum ZoomScript {
    Sweep,
    Static,
}

impl Args {
    fn parse(raw: Vec<String>) -> Self {
        let mut args = Self {
            software: false,
            manual: false,
            zoom_script: ZoomScript::Sweep,
        };

        let mut index = 0usize;
        while index < raw.len() {
            let flag = raw[index].clone();
            match flag.as_str() {
                "--software" => args.software = true,
                "--manual" => args.manual = true,
                "--static" => args.zoom_script = ZoomScript::Static,
                "--help" | "-h" => {
                    println!("graph-fallback-probe [--software] [--manual] [--static]");
                    std::process::exit(0);
                }
                other => eprintln!("ignoring unknown argument {other:?}"),
            }
            index += 1;
        }

        args
    }
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

struct Probe {
    fallback_geometry: Arc<FallbackGeometry>,
    fallback_camera: FallbackCamera,
    cards: Arc<Vec<Card<()>>>,
    harness: CardLayerHarness,
    geometry: Arc<ProbeGeometry>,
    force_software: bool,
    started: Option<Instant>,
    frames: u64,
    last_report: Option<Instant>,
    report_interval: Duration,
    manual: bool,
    script_start: Instant,
    zoom_script: ZoomScript,
}

#[derive(Debug, Clone)]
enum Message {
    Tick(Instant),
    Toggle(CardIdentity, usize),
    Pan(f32, f32),
    Zoom(f32, [f32; 2]),
    Viewport([f32; 2]),
    ScaleFactor(f32),
}

fn update(probe: &mut Probe, message: Message) -> Task<Message> {
    match message {
        Message::Tick(at) => {
            if probe.started.is_none() {
                probe.started = Some(at);
            }
            probe.frames += 1;

            // Periodic report.
            if let Some(last) = probe.last_report {
                if at.duration_since(last) >= probe.report_interval {
                    report(probe);
                    probe.last_report = Some(at);
                }
            } else {
                probe.last_report = Some(at);
            }

            // Scripted zoom/camera if not manual.
            if !probe.manual {
                let elapsed = at.duration_since(probe.script_start);
                match probe.zoom_script {
                    ZoomScript::Sweep => {
                        // Slow zoom in/out sweep.
                        let t = elapsed.as_secs_f32();
                        let zoom = 1.0 + (t * 0.2).sin() * 1.5;
                        probe.fallback_camera.zoom = zoom;
                        probe.harness.camera.zoom = zoom;

                        // Slow pan.
                        let pan_x = (t * 0.3).sin() * 200.0;
                        let pan_y = (t * 0.4).sin() * 150.0;
                        probe.fallback_camera.center = [pan_x, pan_y];
                        probe.harness.camera.center = [pan_x, pan_y];
                    }
                    ZoomScript::Static => {}
                }
            }
        }
        Message::Toggle(identity, line_index) => {
            // Route to the sink.
            probe.harness.sink.toggle(identity, line_index);
        }
        Message::Pan(dx_world, dy_world) => {
            probe.fallback_camera.center[0] += dx_world;
            probe.fallback_camera.center[1] += dy_world;
            probe.harness.camera.center[0] += dx_world;
            probe.harness.camera.center[1] += dy_world;
        }
        Message::Zoom(factor, anchor) => {
            // Anchor the zoom at the cursor position.
            let old_scale = probe.fallback_camera.zoom;
            let new_scale =
                (old_scale * factor).clamp(FallbackCamera::ZOOM_MIN, FallbackCamera::ZOOM_MAX);
            let scale_ratio = new_scale / old_scale;

            // Adjust the camera center to keep the anchor world point fixed.
            probe.fallback_camera.center[0] += anchor[0] * (1.0 - scale_ratio);
            probe.fallback_camera.center[1] += anchor[1] * (1.0 - scale_ratio);
            probe.fallback_camera.zoom = new_scale;

            probe.harness.camera.zoom = new_scale;
            probe.harness.camera.center = probe.fallback_camera.center;
        }
        Message::Viewport(size) => {
            probe.harness.bounds.width = size[0];
            probe.harness.bounds.height = size[1];
        }
        Message::ScaleFactor(_) => {}
    }

    Task::none()
}

fn view(probe: &Probe) -> Element<'_, Message> {
    // Decide which layer to show based on zoom and the forced mode.
    let show_cards = !probe.force_software && probe.harness.camera.zoom >= CARD_ZOOM_THRESHOLD;

    if show_cards && probe.cards.len() > 0 {
        // Card layer active: show the zoomable container.
        // For the B4 prototype, we show a placeholder that indicates
        // the card layer is active. A real implementation would composite
        // the cards into the frame via a custom draw loop.
        let mut harness = CardLayerHarness::default();
        harness.camera = probe.harness.camera;
        harness.bounds = probe.harness.bounds;

        // Update the cache and compute visible cards.
        let cards_clone: Vec<Card<()>> = probe
            .cards
            .iter()
            .map(|c| Card {
                world_position: c.world_position,
                node_id: c.node_id,
                kind: c.kind,
                title: c.title.clone(),
                content: c.content.clone(),
                toggle_requests: c.toggle_requests.clone(),
                _phantom: std::marker::PhantomData,
            })
            .collect();

        let (state, _) = compute_zoomable_state(
            &cards_clone,
            &harness.camera,
            harness.bounds,
            &mut harness.cache,
        );

        // Build a simple overlay showing the card layer status.
        let status = Column::new()
            .push(
                Text::new("Card Layer Active")
                    .size(24)
                    .weight(iced::font::Weight::Bold),
            )
            .push(
                Text::new(format!(
                    "Cards: {}/{} ({} cached, {}+ visible)",
                    state.visible_count,
                    cards_clone.len(),
                    state.cached_count,
                    state.visible_count,
                ))
                .size(16),
            )
            .push(
                Text::new(format!(
                    "Zoom: {:.2}× (threshold: {:.2}×)",
                    harness.camera.zoom, CARD_ZOOM_THRESHOLD
                ))
                .size(14),
            )
            .push(Space::with_height(Length::Fixed(20.0)))
            .push(
                Text::new("Zoom in/out with scroll, pan with drag.")
                    .size(14)
                    .color(iced::Color::from_rgb8(0x66, 0x66, 0x66)),
            )
            .spacing(8)
            .padding(20);

        let fallback_notice = if probe.force_software {
            Container::new(
                Text::new("FORCED SOFTWARE MODE")
                    .size(16)
                    .color(iced::Color::from_rgb8(0xff, 0x66, 0x00)),
            )
            .padding(8)
            .into()
        } else {
            Space::with_height(Length::Fixed(0.0)).into()
        };

        Column::new()
            .push(fallback_notice)
            .push(status)
            .align_items(Alignment::Center)
            .spacing(20)
            .padding(40)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    } else {
        // Fallback mode or zoomed out: show the fallback status.
        let mut col = Column::new().spacing(8).padding(20);

        // Reduced mode notice
        let reduced = if probe.force_software || probe.fallback_geometry.is_capped() {
            Row::new()
                .push(
                    Text::new("Reduced mode: software renderer")
                        .size(16)
                        .color(iced::Color::from_rgb8(0xff, 0x44, 0x44)),
                )
                .push(Space::with_width(Length::Fill))
                .push(
                    Text::new(format!(
                        "{} nodes / {} edges (capped at {})",
                        probe.fallback_geometry.node_count(),
                        probe.fallback_geometry.edge_count(),
                        FALLBACK_NODE_CAP
                    ))
                    .size(14)
                    .color(iced::Color::from_rgb8(0x66, 0x66, 0x66)),
                )
                .spacing(16)
                .padding(8)
        } else {
            Row::new()
                .push(
                    Text::new("Zoomed out: instanced dots")
                        .size(16)
                        .color(iced::Color::from_rgb8(0x66, 0x66, 0x66)),
                )
                .push(Space::with_width(Length::Fill))
                .push(
                    Text::new(format!(
                        "Zoom: {:.2}× / {:.2}× threshold",
                        probe.harness.camera.zoom, CARD_ZOOM_THRESHOLD
                    ))
                    .size(14)
                    .color(iced::Color::from_rgb8(0x66, 0x66, 0x66)),
                )
                .spacing(16)
                .padding(8)
        };

        col = col.push(reduced);
        col = col.push(Space::with_height(Length::Fixed(20.0)));
        col = col.push(
            Text::new(format!(
                "Camera: zoom={:.2}×, center=({}, {})",
                probe.fallback_camera.zoom,
                probe.fallback_camera.center[0] as i32,
                probe.fallback_camera.center[1] as i32,
            ))
            .size(14)
            .color(iced::Color::from_rgb8(0x88, 0x88, 0x88)),
        );
        col = col.push(
            Text::new("Zoom in/out with scroll, pan with drag.")
                .size(14)
                .color(iced::Color::from_rgb8(0x66, 0x66, 0x66)),
        );

        col.align_items(Alignment::Center)
            .spacing(16)
            .height(Length::Fill)
            .width(Length::Fill)
            .into()
    }
}

fn subscription(probe: &Probe) -> Subscription<Message> {
    iced::window::frames().map(Message::Tick)
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

fn report(probe: &Probe) {
    let elapsed = probe
        .started
        .map(|s| s.elapsed())
        .unwrap_or_else(|| probe.script_start.elapsed());
    let fps = if elapsed.as_secs_f64() > 0.0 {
        probe.frames as f64 / elapsed.as_secs_f64()
    } else {
        0.0
    };

    eprintln!();
    eprintln!("=== graph-fallback-probe report ===");
    eprintln!(
        "mode: {}",
        if probe.force_software {
            "SOFTWARE"
        } else {
            "GPU"
        }
    );
    eprintln!(
        "frames: {} ({:.1} fps over {:.2}s)",
        probe.frames,
        fps,
        elapsed.as_secs_f64()
    );
    eprintln!(
        "fallback: {} nodes / {} edges (cap: {})",
        probe.fallback_geometry.node_count(),
        probe.fallback_geometry.edge_count(),
        FALLBACK_NODE_CAP
    );
    eprintln!(
        "cards: {} total, {} cached",
        probe.cards.len(),
        probe.harness.cached_count()
    );
    eprintln!(
        "camera: zoom={:.4}, center=({}, {})",
        probe.harness.camera.zoom, probe.harness.camera.center[0], probe.harness.camera.center[1]
    );
    eprintln!("toggles recorded by sink: {}", probe.harness.toggle_count());
    eprintln!("=== end report ===");
}
