//! Software-renderer fallback for the graph view (D15, §4.9).
//!
//! When the wgpu device cannot be created (tiny-skia software adapter, or a
//! headless environment), the graph view falls back to a simple software
//! renderer with a hard node cap and a visible "reduced mode" notice. This
//! module owns that fallback path: the node cap, the notice text, and the
//! geometry description for software rendering.
//!
//! # Design
//!
//! - **Node cap**: [`FALLBACK_NODE_CAP`] — 10 000 nodes maximum. A software
//!   draw of 10k circles at 60 fps is feasible on integrated GPUs; 50k
//!   would be a slide show.
//! - **Edge cap**: same number, edges drawn as simple lines (software does not
//!   get the GPU's instanced geometry).
//! - **Notice**: a persistent text that says "Reduced mode: software
//!   renderer".
//!
//! # Implementation note (B4)
//!
//! For the B4 prototype, this module provides the data structures and status
//! reporting. The actual canvas rendering lands in B6 when integrated with
//! the wgpu software adapter. The B4 probe demonstrates the fallback by
//! showing a text status and a notice.

use std::sync::Arc;

use iced::widget::{Column, text};
use iced::{Color, Element, Point, Rectangle};

use crate::graph::instance::{NodeShape, color_of};
use flavite_graph::{GraphSnapshot, NodeKind};

/// Maximum nodes drawn in fallback mode (D15 reversal: if 50k cannot hold
/// 60 fps on integrated GPUs, aggregate clusters at low zoom — this is
/// the aggregate).
pub const FALLBACK_NODE_CAP: usize = 10_000;

/// The notice shown in reduced mode.
const REDUCED_MODE_NOTICE: &str = "Reduced mode: software renderer";

/// Stroke for edges in fallback mode.
pub const EDGE_COLOR: Color = Color::from_rgb8(0x9e, 0xa6, 0xb8);

/// Background gradient from.
pub const BACKGROUND_FROM: Color = Color::from_rgb8(0xf8, 0xf8, 0xf8);

/// Background gradient to.
pub const BACKGROUND_TO: Color = Color::from_rgb8(0xe0, 0xe0, 0xe0);

/// Error building fallback geometry from a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackGeometryError {
    /// The position buffer must match the snapshot's node count.
    PositionCountMismatch { nodes: usize, positions: usize },
}

/// CPU-side geometry for the fallback renderer.
///
/// Plain data: node positions, node kinds, node sizes, edge endpoints.
/// All in world space; the camera converts to screen.
#[derive(Debug, Clone, PartialEq)]
pub struct FallbackGeometry {
    generation: u64,
    nodes: Vec<FallbackNode>,
    edges: Vec<FallbackEdge>,
}

impl FallbackGeometry {
    /// Build from a snapshot and dense positions, capping at
    /// [`FALLBACK_NODE_CAP`] nodes and edges.
    pub fn from_snapshot(
        snapshot: &GraphSnapshot,
        positions: &[[f32; 2]],
        generation: u64,
    ) -> Result<Self, FallbackGeometryError> {
        let node_count = snapshot.node_count();
        if positions.len() != node_count {
            return Err(FallbackGeometryError::PositionCountMismatch {
                nodes: node_count,
                positions: positions.len(),
            });
        }

        let cap = FALLBACK_NODE_CAP.min(node_count);

        let mut nodes = Vec::with_capacity(cap);
        let mut edges = Vec::new();

        // First pass: cap the nodes.
        for i in 0..cap {
            let kind = snapshot
                .node_kinds
                .get(i)
                .copied()
                .unwrap_or(NodeKind::Note);
            let degree = snapshot.degrees.get(i).copied().unwrap_or(0);
            nodes.push(FallbackNode {
                position: positions[i],
                size: node_radius(degree),
                shape: NodeShape::from(kind),
                color: color_of(kind),
            });
        }

        // Second pass: cap the edges, and skip edges that reference nodes
        // beyond the cap (those nodes do not exist in the fallback geometry).
        let mut cursor = 0usize;
        for src in 0..cap {
            let start = snapshot.out_offsets[src] as usize;
            let end = snapshot.out_offsets[src + 1] as usize;
            assert!(start >= cursor, "CSR invariant");
            cursor = end;
            for row in start..end {
                let dst = snapshot.out_indices[row] as usize;
                if edges.len() < FALLBACK_NODE_CAP {
                    if dst < cap {
                        edges.push(FallbackEdge {
                            from: positions[src],
                            to: positions[dst],
                        });
                    }
                } else {
                    break;
                }
            }
        }

        Ok(Self {
            generation,
            nodes,
            edges,
        })
    }

    /// Build directly (for tests).
    #[must_use]
    pub fn new(generation: u64, nodes: Vec<FallbackNode>, edges: Vec<FallbackEdge>) -> Self {
        Self {
            generation,
            nodes,
            edges,
        }
    }

    /// The geometry generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Number of nodes (≤ [`FALLBACK_NODE_CAP`]).
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Number of edges (≤ [`FALLBACK_NODE_CAP`]).
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Whether the geometry exceeds the fallback caps.
    #[must_use]
    pub fn is_capped(&self) -> bool {
        self.node_count() == FALLBACK_NODE_CAP || self.edge_count() == FALLBACK_NODE_CAP
    }
}

/// One drawn node in fallback mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FallbackNode {
    pub position: [f32; 2],
    pub size: f32,
    pub shape: NodeShape,
    pub color: [f32; 4],
}

/// One drawn edge in fallback mode.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FallbackEdge {
    pub from: [f32; 2],
    pub to: [f32; 2],
}

/// The pan/zoom camera for the fallback (world → screen).
///
/// Logical pixels: the conversion to physical happens in the renderer,
/// so the camera never needs to know the scale factor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FallbackCamera {
    /// World point at the centre of the bounds.
    pub center: [f32; 2],
    /// World units → logical pixels.
    pub zoom: f32,
}

impl FallbackCamera {
    /// Minimum zoom clamp.
    pub const ZOOM_MIN: f32 = 0.02;
    /// Maximum zoom clamp.
    pub const ZOOM_MAX: f32 = 64.0;

    /// The camera looking at the origin at 1:1.
    #[must_use]
    pub fn centered() -> Self {
        Self {
            center: [0.0, 0.0],
            zoom: 1.0,
        }
    }

    /// Translate the view in world units.
    pub fn pan_by(&mut self, dx: f32, dy: f32) {
        self.center[0] += dx;
        self.center[1] += dy;
    }

    /// World coordinates → widget-local logical pixels.
    #[must_use]
    pub fn world_to_screen(&self, world: [f32; 2], bounds: Rectangle) -> Point {
        Point {
            x: (world[0] - self.center[0]) * self.zoom + bounds.x + bounds.width * 0.5,
            y: (world[1] - self.center[1]) * self.zoom + bounds.y + bounds.height * 0.5,
        }
    }

    /// Widget-local logical pixels → world coordinates.
    #[must_use]
    pub fn screen_to_world(&self, screen: Point, bounds: Rectangle) -> [f32; 2] {
        [
            (screen.x - bounds.x - bounds.width * 0.5) / self.zoom + self.center[0],
            (screen.y - bounds.y - bounds.height * 0.5) / self.zoom + self.center[1],
        ]
    }

    /// Fit a world-space extent into the bounds.
    pub fn fit_extent(&mut self, extent: f32, bounds: Rectangle) {
        self.center = [0.0, 0.0];
        let safe_extent = extent.max(1.0);
        let zx = bounds.width as f32 / safe_extent;
        let zy = bounds.height as f32 / safe_extent;
        self.zoom = zx.min(zy).clamp(Self::ZOOM_MIN, Self::ZOOM_MAX);
    }

    /// Apply a zoom factor, clamped.
    #[must_use]
    pub fn scaled_zoom(&self, factor: f32) -> f32 {
        if !factor.is_finite() || factor <= 0.0 {
            return self.zoom.clamp(Self::ZOOM_MIN, Self::ZOOM_MAX);
        }
        (self.zoom * factor).clamp(Self::ZOOM_MIN, Self::ZOOM_MAX)
    }
}

impl Default for FallbackCamera {
    fn default() -> Self {
        Self::centered()
    }
}

/// The per-frame input to the fallback renderer.
#[derive(Debug, Clone)]
pub struct FallbackPrimitive {
    /// The geometry to render.
    geometry: Arc<FallbackGeometry>,
    /// Camera: pan/zoom in world space.
    camera: FallbackCamera,
    /// Logical pixel bounds of the widget.
    #[allow(dead_code)]
    bounds: Rectangle,
}

impl FallbackPrimitive {
    /// Build a primitive for the current state.
    #[must_use]
    pub fn new(geometry: Arc<FallbackGeometry>, camera: FallbackCamera, bounds: Rectangle) -> Self {
        Self {
            geometry,
            camera,
            bounds,
        }
    }

    /// The geometry generation.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.geometry.generation()
    }
}

/// Node radius for degree-based sizing.
fn node_radius(degree: u32) -> f32 {
    const BASE: f32 = 2.0;
    const PER_OCTAVE: f32 = 1.25;
    BASE + PER_OCTAVE * (degree as f32 + 1.0).log2()
}

/// A status indicator for the fallback mode.
#[derive(Debug, Clone)]
pub struct FallbackStatus {
    node_count: usize,
    edge_count: usize,
    is_capped: bool,
    zoom: f32,
    pan: [f32; 2],
}

impl FallbackStatus {
    /// Create a status indicator for the fallback geometry.
    #[must_use]
    pub fn new(geometry: &FallbackGeometry, camera: &FallbackCamera) -> Self {
        Self {
            node_count: geometry.node_count(),
            edge_count: geometry.edge_count(),
            is_capped: geometry.is_capped(),
            zoom: camera.zoom,
            pan: camera.center,
        }
    }

    /// Build a status widget.
    #[must_use]
    pub fn widget(&self) -> Element<'static, ()> {
        let mut col = Column::new().spacing(8).padding(12);
        col = col.push(text("Graph View — Reduced Mode").size(20));
        col = col.push(text(REDUCED_MODE_NOTICE).size(16));
        let cap_text = if self.is_capped {
            format!(" (capped at {})", FALLBACK_NODE_CAP)
        } else {
            String::new()
        };
        col = col.push(
            text(format!(
                "{} nodes, {} edges{}",
                self.node_count, self.edge_count, cap_text
            ))
            .size(14)
            .color(Color::from_rgb8(0x66, 0x66, 0x66)),
        );
        col = col.push(
            text(format!(
                "Zoom: {:.2}× | Pan: ({:.0}, {:.0})",
                self.zoom, self.pan[0], self.pan[1]
            ))
            .size(12)
            .color(Color::from_rgb8(0x88, 0x88, 0x88)),
        );
        col.into()
    }

    /// A simple text-only notice for reduced mode.
    #[must_use]
    pub fn notice_widget(&self) -> Element<'_, ()> {
        text(REDUCED_MODE_NOTICE)
            .size(16)
            .color(Color::from_rgb8(0xcc, 0x00, 0x00))
            .into()
    }
}

/// A simple display for the fallback state (B4 prototype).
///
/// Shows the geometry status and notice. B6 integrates this with an
/// actual software renderer.
#[derive(Debug, Clone)]
pub struct FallbackDisplay {
    primitive: FallbackPrimitive,
}

impl FallbackDisplay {
    /// A new fallback display for `primitive`.
    #[must_use]
    pub fn new(primitive: FallbackPrimitive) -> Self {
        Self { primitive }
    }

    /// Update the primitive.
    pub fn set_primitive(&mut self, primitive: FallbackPrimitive) {
        self.primitive = primitive;
    }

    /// The geometry generation currently drawn.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.primitive.generation()
    }

    /// Whether the fallback is in effect.
    #[must_use]
    pub fn is_reduced_mode(&self) -> bool {
        true
    }

    /// The status of the fallback geometry.
    #[must_use]
    pub fn status(&self) -> FallbackStatus {
        FallbackStatus::new(&self.primitive.geometry, &self.primitive.camera)
    }

    /// Build a widget showing the fallback state.
    #[must_use]
    pub fn widget(&self) -> Element<'static, ()> {
        self.status().widget()
    }
}

/// The reduced-mode notice as a standalone widget.
#[must_use]
pub fn reduced_mode_notice() -> Element<'static, ()> {
    text(REDUCED_MODE_NOTICE)
        .size(16)
        .color(Color::from_rgb8(0xcc, 0x00, 0x00))
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_node_cap_is_below_the_ac16_target() {
        assert!(FALLBACK_NODE_CAP < 50_000);
    }

    #[test]
    fn geometry_from_snapshot_caps_nodes_and_edges() {
        // Batched deltas — the feed's shape. One `apply` per delta would
        // rebuild the CSR cache per call (O(n) each), turning this fixture
        // into a quadratic debug-build hang.
        let mut model = flavite_graph::GraphModel::new();
        let nodes: Vec<flavite_graph::Delta> = (0..FALLBACK_NODE_CAP + 100)
            .map(|i| {
                flavite_graph::Delta::Node(flavite_graph::NodeDelta::Added {
                    id: flavite_graph::NodeId::note(i as u64),
                })
            })
            .collect();
        model.apply(&nodes);
        let edges: Vec<flavite_graph::Delta> = (0..FALLBACK_NODE_CAP + 50)
            .flat_map(|i| {
                ((i + 1)..(i + 2).min(FALLBACK_NODE_CAP + 100)).map(move |j| {
                    flavite_graph::Delta::Edge(flavite_graph::EdgeDelta::Added {
                        from: flavite_graph::NodeId::note(i as u64),
                        to: flavite_graph::NodeId::note(j as u64),
                        kind: flavite_graph::EdgeKind::Link,
                    })
                })
            })
            .collect();
        model.apply(&edges);
        let snapshot = model.snapshot();
        let positions: Vec<[f32; 2]> = (0..snapshot.node_count() as usize)
            .map(|i| [i as f32 * 10.0, 0.0])
            .collect();
        let geometry = FallbackGeometry::from_snapshot(&snapshot, &positions, 1).unwrap();
        assert_eq!(geometry.node_count(), FALLBACK_NODE_CAP);
        assert!(geometry.edge_count() <= FALLBACK_NODE_CAP);
        assert!(geometry.is_capped());
    }

    #[test]
    fn geometry_rejects_position_mismatch() {
        let snapshot = flavite_graph::GraphModel::new().snapshot();
        let err = FallbackGeometry::from_snapshot(&snapshot, &[[0.0, 0.0]], 0).unwrap_err();
        assert_eq!(
            err,
            FallbackGeometryError::PositionCountMismatch {
                nodes: 0,
                positions: 1
            }
        );
    }

    #[test]
    fn camera_round_trip() {
        let camera = FallbackCamera {
            center: [10.0, -5.0],
            zoom: 2.5,
        };
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        for world in [[0.0, 0.0], [10.0, -5.0], [20.0, -15.0]] {
            let screen = camera.world_to_screen(world, bounds);
            let back = camera.screen_to_world(screen, bounds);
            assert!((back[0] - world[0]).abs() < 0.01, "x round trip");
            assert!((back[1] - world[1]).abs() < 0.01, "y round trip");
        }
    }

    #[test]
    fn camera_fit_extent_centers_and_scales() {
        let mut camera = FallbackCamera::centered();
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 1024.0,
            height: 768.0,
        };
        camera.fit_extent(2000.0, bounds);
        assert_eq!(camera.center, [0.0, 0.0]);
        // min(1024/2000, 768/2000) = min(0.512, 0.384) = 0.384
        assert!((camera.zoom - 768.0 / 2000.0).abs() < 0.001);
    }

    #[test]
    fn camera_default_is_centered() {
        let camera = FallbackCamera::default();
        assert_eq!(camera.center, [0.0, 0.0]);
        assert_eq!(camera.zoom, 1.0);
    }

    #[test]
    fn node_radius_is_increasing_finite_and_positive() {
        assert_eq!(node_radius(0), 2.0);
        let mut previous = -1.0;
        for degree in [0u32, 1, 3, 7, 31, 1023, 65_535, u32::MAX] {
            let r = node_radius(degree);
            assert!(r.is_finite() && r > 0.0, "radius for {degree} = {r}");
            assert!(r > previous, "radius must grow with degree ({degree})");
            previous = r;
        }
    }

    #[test]
    fn fallback_geometry_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FallbackGeometry>();
        assert_send_sync::<FallbackPrimitive>();
        assert_send_sync::<Arc<FallbackGeometry>>();
    }

    #[test]
    fn fallback_display_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<FallbackDisplay>();
    }

    #[test]
    fn reduced_mode_notice_is_not_empty() {
        assert!(!REDUCED_MODE_NOTICE.is_empty());
    }

    #[test]
    fn camera_scaled_zoom_clamps() {
        let camera = FallbackCamera::centered();
        assert_eq!(camera.scaled_zoom(0.0), 1.0);
        assert_eq!(camera.scaled_zoom(-1.0), 1.0);
        assert_eq!(camera.scaled_zoom(f32::NAN), 1.0);
        assert_eq!(camera.scaled_zoom(1e9), FallbackCamera::ZOOM_MAX);
        assert_eq!(camera.scaled_zoom(1e-9), FallbackCamera::ZOOM_MIN);
    }

    #[test]
    fn fallback_status_widget_builds() {
        let geometry = FallbackGeometry::new(1, vec![], vec![]);
        let camera = FallbackCamera::centered();
        let status = FallbackStatus::new(&geometry, &camera);
        let widget = status.widget();
        drop(widget);
    }

    #[test]
    fn fallback_display_widget_builds() {
        let geometry = Arc::new(FallbackGeometry::new(1, vec![], vec![]));
        let camera = FallbackCamera::centered();
        let bounds = Rectangle::new(Point::new(0.0, 0.0), iced::Size::new(800.0, 600.0));
        let primitive = FallbackPrimitive::new(Arc::clone(&geometry), camera, bounds);
        let display = FallbackDisplay::new(primitive);
        let widget = display.widget();
        drop(widget);
    }

    #[test]
    fn fallback_display_generation() {
        let geometry = Arc::new(FallbackGeometry::new(42, vec![], vec![]));
        let camera = FallbackCamera::centered();
        let bounds = Rectangle::new(Point::new(0.0, 0.0), iced::Size::new(800.0, 600.0));
        let primitive = FallbackPrimitive::new(Arc::clone(&geometry), camera, bounds);
        let display = FallbackDisplay::new(primitive);
        assert_eq!(display.generation(), 42);
    }

    #[test]
    fn fallback_display_is_reduced_mode() {
        let geometry = Arc::new(FallbackGeometry::new(1, vec![], vec![]));
        let camera = FallbackCamera::centered();
        let bounds = Rectangle::new(Point::new(0.0, 0.0), iced::Size::new(800.0, 600.0));
        let primitive = FallbackPrimitive::new(Arc::clone(&geometry), camera, bounds);
        let display = FallbackDisplay::new(primitive);
        assert!(display.is_reduced_mode());
    }
}
