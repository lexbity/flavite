//! CPU-side instance geometry for the instanced graph renderer (D15).
//!
//! Everything in this module is plain data and pure functions: it is the
//! part of the renderer that can be proven without a GPU. The widget in
//! [`crate::graph::shader_probe`] copies [`NodeInstance`]/[`EdgeInstance`]
//! arrays verbatim into wgpu vertex buffers, so the D15 render tunables —
//! degree-based node sizing, edge alpha by zoom, edge count capped by
//! importance — are decided *here*, on the CPU, where they are unit-tested
//! rather than re-derived (and re-implemented) in WGSL.

use std::cmp::Ordering;
use std::mem::size_of;

use flavite_graph::{GraphSnapshot, NodeId, NodeKind};

/// Vertex-layout stride of [`NodeInstance`], in bytes.
///
/// Enforced by [`const_assert`] at compile time so the Rust layout and the
/// WGSL attribute offsets in [`crate::graph::shader_probe`] can never drift
/// apart silently.
pub const NODE_INSTANCE_SIZE: usize = 32;

/// Vertex-layout stride of [`EdgeInstance`], in bytes.
pub const EDGE_INSTANCE_SIZE: usize = 32;

const _: () = assert!(
    size_of::<NodeInstance>() == NODE_INSTANCE_SIZE,
    "NodeInstance must stay 32 bytes to match the WGSL vertex layout"
);
const _: () = assert!(
    size_of::<EdgeInstance>() == EDGE_INSTANCE_SIZE,
    "EdgeInstance must stay 32 bytes to match the WGSL vertex layout"
);
const _: () = assert!(
    size_of::<CameraUniform>() == CAMERA_UNIFORM_SIZE,
    "CameraUniform must stay 48 bytes to match the WGSL uniform layout"
);

/// Byte size of the camera uniform (`shader_probe` writes it to the GPU).
pub const CAMERA_UNIFORM_SIZE: usize = 48;

/// Base node radius, in world units, for a degree-0 node (D15: nodes are
/// sized by degree).
pub const NODE_RADIUS_BASE: f32 = 2.0;

/// Additional node radius granted per degree *octave* — a node with degree
/// `2^k` is `k` octaves larger. Logarithmic so a hub with degree 5 000 does
/// not swallow the map the way a linear rule would, while still keeping
/// every leaf visible.
pub const NODE_RADIUS_PER_OCTAVE: f32 = 1.25;

/// Smallest alpha an edge may ever reach (`edge_base_alpha`).
pub const EDGE_ALPHA_MIN: f32 = 0.15;

/// Alpha granted per importance octave in [`edge_base_alpha`].
pub const EDGE_ALPHA_PER_OCTAVE: f32 = 0.16;

/// Importance at (or above) which [`edge_base_alpha`] saturates at 1.0.
pub const EDGE_ALPHA_SATURATE: f32 = 1_000_000.0;

/// Alpha multiplier applied to every edge when fully zoomed out; alpha by
/// zoom (D15) fades edges as the viewport grows but never removes them.
pub const EDGE_ALPHA_ZOOM_FLOOR: f32 = 0.25;

/// Edge instances drawn at the reference zoom of 1.0 (D15: edge count
/// capped by importance).
pub const EDGE_CAP_AT_REFERENCE_ZOOM: usize = 100_000;

/// Zoom below which [`edge_cap`] stops shrinking the edge draw.
pub const EDGE_CAP_ZOOM_MIN: f32 = 0.25;

/// Zoom above which [`edge_cap`] stops growing the edge draw.
pub const EDGE_CAP_ZOOM_MAX: f32 = 4.0;

/// Base edge stroke width, in physical pixels.
pub const EDGE_WIDTH_BASE: f32 = 1.0;

/// Note nodes — circular dots (D15: `instanced dots`).
pub const NOTE_COLOR: [f32; 4] = [0.42, 0.66, 1.00, 1.00];

/// Attachment nodes — squares (D15, v2/R3: squares sized by degree).
pub const ATTACHMENT_COLOR: [f32; 4] = [1.00, 0.78, 0.35, 1.00];

/// The shape a node is drawn with.
///
/// `repr(u32)` so the value is uploaded directly as a WGSL `u32` vertex
/// attribute; the fragment shader masks circles off and leaves squares alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum NodeShape {
    /// A filled circle (notes).
    Circle = 0,
    /// A filled square (attachments).
    Square = 1,
}

impl From<NodeKind> for NodeShape {
    fn from(kind: NodeKind) -> Self {
        match kind {
            NodeKind::Note => Self::Circle,
            NodeKind::Attachment => Self::Square,
        }
    }
}

/// One drawn node: 32 bytes, uploaded as a per-instance vertex attribute.
///
/// Layout (little-endian, matches the WGSL vertex layout in
/// `shader_probe`):
///
/// | offset | field | WGSL type |
/// |---|---|---|
/// | 0 | `center` | `vec2<f32>` |
/// | 8 | `size` | `f32` |
/// | 12 | `shape` | `u32` |
/// | 16 | `color` | `vec4<f32>` |
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct NodeInstance {
    /// World-space centre of the node.
    pub center: [f32; 2],
    /// Radius in world units (see [`node_radius`]).
    pub size: f32,
    /// Circle (notes) or square (attachments).
    pub shape: NodeShape,
    /// Premultiplied-ready RGBA in linear space.
    pub color: [f32; 4],
}

impl NodeInstance {
    /// The 32 little-endian bytes this instance uploads as.
    #[must_use]
    pub fn to_bytes(self) -> [u8; NODE_INSTANCE_SIZE] {
        let mut out = [0u8; NODE_INSTANCE_SIZE];
        out[0..8].copy_from_slice(&pack_f32x2(self.center));
        out[8..12].copy_from_slice(&self.size.to_le_bytes());
        out[12..16].copy_from_slice(&(self.shape as u32).to_le_bytes());
        out[16..32].copy_from_slice(&pack_f32x4(self.color));
        out
    }
}

/// One drawn edge: 32 bytes, uploaded as a per-instance vertex attribute.
///
/// Layout (little-endian, matches the WGSL vertex layout in
/// `shader_probe`):
///
/// | offset | field | WGSL type |
/// |---|---|---|
/// | 0 | `from` | `vec2<f32>` |
/// | 8 | `to` | `vec2<f32>` |
/// | 16 | `alpha` | `f32` |
/// | 20 | `width` | `f32` |
/// | 24 | `pad` | (unused, keeps the stride at 32) |
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct EdgeInstance {
    /// World-space source endpoint.
    pub from: [f32; 2],
    /// World-space target endpoint.
    pub to: [f32; 2],
    /// Base alpha before the per-zoom multiplier (see [`edge_base_alpha`]).
    pub alpha: f32,
    /// Stroke width in physical pixels.
    pub width: f32,
    /// Padding to the 32-byte stride.
    pub pad: [f32; 2],
}

impl EdgeInstance {
    /// The 32 little-endian bytes this instance uploads as.
    #[must_use]
    pub fn to_bytes(self) -> [u8; EDGE_INSTANCE_SIZE] {
        let mut out = [0u8; EDGE_INSTANCE_SIZE];
        out[0..8].copy_from_slice(&pack_f32x2(self.from));
        out[8..16].copy_from_slice(&pack_f32x2(self.to));
        out[16..20].copy_from_slice(&self.alpha.to_le_bytes());
        out[20..24].copy_from_slice(&self.width.to_le_bytes());
        out[24..32].copy_from_slice(&pack_f32x2(self.pad));
        out
    }
}

/// The pan/zoom camera (world-space state; the GPU uniform is
/// [`CameraUniform`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Camera {
    /// World-space point shown at the centre of the widget.
    pub center: [f32; 2],
    /// World units → *logical* widget pixels. 1.0 shows the world 1:1 on a
    /// non-HiDPI display.
    ///
    /// Logical, not physical: every camera operation ([`Self::pan_by`],
    /// [`Self::zoom_at`], [`Self::screen_to_world`]) therefore works in the
    /// same units the cursor events arrive in, and none of them need to know
    /// the display scale factor. [`ProbePrimitive::prepare`] is the single
    /// place that multiplies by `Viewport::scale_factor` on the way to
    /// [`CameraUniform`].
    ///
    /// [`ProbePrimitive::prepare`]: crate::graph::shader_probe::ProbePrimitive
    pub zoom: f32,
}

impl Camera {
    /// Zoom clamps — wide enough to see the whole probe map and to inspect a
    /// single note, tight enough to keep `f32` world precision.
    pub const ZOOM_MIN: f32 = 0.02;
    /// See [`Self::ZOOM_MIN`].
    pub const ZOOM_MAX: f32 = 64.0;

    /// The camera looking at the world origin at 1:1.
    #[must_use]
    pub fn centered() -> Self {
        Self {
            center: [0.0, 0.0],
            zoom: 1.0,
        }
    }

    /// Translate the view centre in world units.
    pub fn pan_by(&mut self, dx_world: f32, dy_world: f32) {
        self.center[0] += dx_world;
        self.center[1] += dy_world;
    }

    /// Zoom by `factor` while keeping the world point currently under
    /// `anchor_screen` (widget-local physical pixels) pinned on screen.
    ///
    /// This is what makes a scroll-wheel zoom feel anchored to the cursor
    /// instead of to the window centre.
    pub fn zoom_at(
        &mut self,
        anchor_screen: [f32; 2],
        bounds_position: [f32; 2],
        bounds_size: [f32; 2],
        factor: f32,
    ) {
        let before = self.screen_to_world(anchor_screen, bounds_position, bounds_size);
        self.zoom = self.scaled_zoom(factor);
        let after = self.screen_to_world(anchor_screen, bounds_position, bounds_size);
        self.center[0] += before[0] - after[0];
        self.center[1] += before[1] - after[1];
    }

    /// Apply `factor` to the zoom, clamped to [`Self::ZOOM_MIN`] /
    /// [`Self::ZOOM_MAX`]. A non-finite factor is a no-op.
    #[must_use]
    pub fn scaled_zoom(&self, factor: f32) -> f32 {
        if !factor.is_finite() || factor <= 0.0 {
            return self.zoom.clamp(Self::ZOOM_MIN, Self::ZOOM_MAX);
        }
        (self.zoom * factor).clamp(Self::ZOOM_MIN, Self::ZOOM_MAX)
    }

    /// Fit a world-space rectangle of `extent` × `extent` centred on the
    /// origin into `bounds_size` widget pixels, at `bounds_position`.
    pub fn fit_extent(&mut self, extent: f32, bounds_position: [f32; 2], bounds_size: [f32; 2]) {
        let _ = bounds_position;
        self.center = [0.0, 0.0];
        let safe_extent = if extent.is_finite() && extent > 0.0 {
            extent
        } else {
            1.0
        };
        let zx = bounds_size[0] / safe_extent;
        let zy = bounds_size[1] / safe_extent;
        let z = zx.min(zy);
        self.zoom = if z.is_finite() && z > 0.0 {
            z.clamp(Self::ZOOM_MIN, Self::ZOOM_MAX)
        } else {
            Self::ZOOM_MIN
        };
    }

    /// Widget-local physical pixels → world coordinates.
    #[must_use]
    pub fn screen_to_world(
        &self,
        screen: [f32; 2],
        bounds_position: [f32; 2],
        bounds_size: [f32; 2],
    ) -> [f32; 2] {
        let local = [
            screen[0] - bounds_position[0] - bounds_size[0] * 0.5,
            screen[1] - bounds_position[1] - bounds_size[1] * 0.5,
        ];
        [
            local[0] / self.zoom + self.center[0],
            local[1] / self.zoom + self.center[1],
        ]
    }

    /// World coordinates → widget-local physical pixels.
    #[must_use]
    pub fn world_to_screen(
        &self,
        world: [f32; 2],
        bounds_position: [f32; 2],
        bounds_size: [f32; 2],
    ) -> [f32; 2] {
        [
            (world[0] - self.center[0]) * self.zoom + bounds_position[0] + bounds_size[0] * 0.5,
            (world[1] - self.center[1]) * self.zoom + bounds_position[1] + bounds_size[1] * 0.5,
        ]
    }
}

impl Default for Camera {
    fn default() -> Self {
        Self::centered()
    }
}

/// The per-frame camera uniform handed to the WGSL shaders: 48 bytes.
///
/// WGSL struct (uniform address space — `vec2<f32>` aligns to 8):
///
/// ```wgsl
/// struct Camera {
///     center: vec2<f32>,        // offset 0
///     zoom: f32,                // offset 8
///     edge_alpha: f32,          // offset 12
///     bounds_position: vec2<f32>, // offset 16
///     bounds_size: vec2<f32>,   // offset 24
///     viewport_size: vec2<f32>, // offset 32
///     edge_instances: u32,      // offset 40
///     node_instances: u32,      // offset 44
/// };
/// ```
///
/// The Rust `repr(C)` offsets are identical (every member aligns to 4 in
/// Rust), so [`Self::to_bytes`] can be written straight into the buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct CameraUniform {
    /// World point at the widget centre.
    pub center: [f32; 2],
    /// World units → physical pixels.
    pub zoom: f32,
    /// Per-zoom edge multiplier (see [`edge_alpha_factor`]).
    pub edge_alpha: f32,
    /// Widget bounds origin, physical pixels, in window space.
    pub bounds_position: [f32; 2],
    /// Widget bounds size, physical pixels.
    pub bounds_size: [f32; 2],
    /// Window size, physical pixels.
    pub viewport_size: [f32; 2],
    /// Edge instances to draw this frame (the importance cap).
    pub edge_instances: u32,
    /// Node instances to draw this frame.
    pub node_instances: u32,
}

impl CameraUniform {
    /// Pack the uniform into the 48 little-endian bytes the GPU reads.
    #[must_use]
    pub fn to_bytes(self) -> [u8; CAMERA_UNIFORM_SIZE] {
        let mut out = [0u8; CAMERA_UNIFORM_SIZE];
        let mut at = 0usize;
        let mut put = |bytes: &[u8]| {
            out[at..at + bytes.len()].copy_from_slice(bytes);
            at += bytes.len();
        };
        put(&pack_f32x2(self.center));
        put(&self.zoom.to_le_bytes());
        put(&self.edge_alpha.to_le_bytes());
        put(&pack_f32x2(self.bounds_position));
        put(&pack_f32x2(self.bounds_size));
        put(&pack_f32x2(self.viewport_size));
        put(&self.edge_instances.to_le_bytes());
        put(&self.node_instances.to_le_bytes());
        debug_assert_eq!(at, CAMERA_UNIFORM_SIZE, "uniform packed in order");
        out
    }
}

/// A problem building instance geometry from a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GeometryError {
    /// The position buffer must be indexed by the snapshot's dense order
    /// (§4.14: position/node-id mappings from different topologies may never
    /// be combined).
    #[error(
        "position count ({positions}) does not match node count ({nodes}) — \
         positions and snapshot must come from the same topology version"
    )]
    PositionCountMismatch {
        /// Nodes in the snapshot.
        nodes: usize,
        /// Positions supplied.
        positions: usize,
    },
}

/// The instanced geometry the probe uploads: nodes, then edges ordered by
/// importance (descending) so the per-frame edge cap is a prefix draw.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeGeometry {
    generation: u64,
    nodes: Vec<NodeInstance>,
    edges: Vec<EdgeInstance>,
    /// Node identities, in dense order, parallel to `nodes`.
    /// Added in B6 for hit-testing: mapping dense indices back to [`NodeId`].
    node_ids: Vec<NodeId>,
}

impl ProbeGeometry {
    /// Build geometry from a graph snapshot and dense-indexed positions.
    ///
    /// - One [`NodeInstance`] per node, sized by [`node_radius`] and shaped
    ///   by [`NodeShape::from`].
    /// - One [`EdgeInstance`] per directed out-edge of the snapshot (each
    ///   edge is drawn exactly once), carrying its [`edge_base_alpha`],
    ///   sorted **importance-descending** so that a draw of
    ///   `edges[..edge_cap(total, zoom)]` keeps the most important edges
    ///   first (D15: edge count capped by importance).
    ///
    /// `generation` identifies the position/topology generation: the
    /// renderer only re-uploads instance buffers when it changes (see
    /// [`needs_upload`]), which is the hand-off `PositionBuffer` makes in
    /// B3+.
    pub fn from_snapshot(
        snapshot: &GraphSnapshot,
        positions: &[[f32; 2]],
        generation: u64,
    ) -> Result<Self, GeometryError> {
        let node_count = snapshot.node_count();
        if positions.len() != node_count {
            return Err(GeometryError::PositionCountMismatch {
                nodes: node_count,
                positions: positions.len(),
            });
        }

        let node_ids: Vec<NodeId> = snapshot.node_ids.clone();
        let nodes = (0..node_count)
            .map(|i| {
                let kind = snapshot.node_kinds[i];
                let degree = snapshot.degrees.get(i).copied().unwrap_or(0);
                NodeInstance {
                    center: positions[i],
                    size: node_radius(degree),
                    shape: NodeShape::from(kind),
                    color: color_of(kind),
                }
            })
            .collect();

        let mut keyed: Vec<(f32, EdgeInstance)> = Vec::with_capacity(snapshot.edge_count());
        let mut cursor = 0usize;
        for src in 0..node_count {
            let start = snapshot.out_offsets[src] as usize;
            let end = snapshot.out_offsets[src + 1] as usize;
            assert!(
                start >= cursor,
                "out_offsets must be non-decreasing (CSR invariant)"
            );
            cursor = end;
            for row in start..end {
                let dst = snapshot.out_indices[row] as usize;
                let importance = edge_importance(snapshot.degrees[src], snapshot.degrees[dst]);
                keyed.push((
                    importance,
                    EdgeInstance {
                        from: positions[src],
                        to: positions[dst],
                        alpha: edge_base_alpha(importance),
                        width: EDGE_WIDTH_BASE,
                        pad: [0.0; 2],
                    },
                ));
            }
        }
        assert_eq!(
            cursor,
            snapshot.edge_count(),
            "CSR rows must account for every edge"
        );

        // Stable sort: ties keep snapshot order, so the result is
        // deterministic for a given snapshot.
        keyed.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(Ordering::Equal));
        let edges = keyed.into_iter().map(|(_, edge)| edge).collect();

        Ok(Self {
            generation,
            nodes,
            edges,
            node_ids,
        })
    }

    /// Build geometry directly (used by tests that do not need a snapshot).
    #[must_use]
    pub fn new(
        generation: u64,
        nodes: Vec<NodeInstance>,
        edges: Vec<EdgeInstance>,
        node_ids: Vec<NodeId>,
    ) -> Self {
        Self {
            generation,
            nodes,
            edges,
            node_ids,
        }
    }

    /// The position/topology generation this geometry was built at.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Node instances, in dense order.
    #[must_use]
    pub fn nodes(&self) -> &[NodeInstance] {
        &self.nodes
    }

    /// Edge instances, importance-descending.
    #[must_use]
    pub fn edges(&self) -> &[EdgeInstance] {
        &self.edges
    }

    /// Node identities, in dense order (parallel to [`Self::nodes`]).
    ///
    /// Added in B6 for hit-testing: maps dense indices back to [`NodeId`].
    #[must_use]
    pub fn node_ids(&self) -> &[NodeId] {
        &self.node_ids
    }

    /// Number of nodes.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Total number of edge instances available (before any zoom cap).
    #[must_use]
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Edge instances to draw at `zoom`: the importance prefix (D15).
    #[must_use]
    pub fn visible_edge_count(&self, zoom: f32) -> usize {
        edge_cap(self.edges.len(), zoom)
    }

    /// Get the node identity for a dense index.
    ///
    /// Returns `None` if the index is out of range.
    #[must_use]
    pub fn node_id(&self, index: usize) -> Option<NodeId> {
        self.node_ids.get(index).copied()
    }
}

/// Whether instance buffers must be (re)uploaded.
///
/// Uploads happen only when the geometry generation moved (or when the
/// existing GPU buffers are too small for the new arrays), so a steady-state
/// frame costs one uniform write and two instanced draws — never a buffer
/// rebuild (D15: no locks on the frame path, and nothing redundant on it
/// either).
#[must_use]
pub fn needs_upload(last_uploaded: Option<u64>, current: u64, capacity_ok: bool) -> bool {
    last_uploaded != Some(current) || !capacity_ok
}

/// Node radius for a node of `degree` (D15: degree-based sizing).
///
/// Strictly increasing and finite for every `u32`, including `u32::MAX`.
#[must_use]
pub fn node_radius(degree: u32) -> f32 {
    NODE_RADIUS_BASE + NODE_RADIUS_PER_OCTAVE * (degree as f32 + 1.0).log2()
}

/// Edge importance from the degrees of its two endpoints.
///
/// Symmetric, strictly positive, and finite for every degree pair: an edge
/// joining two hubs outranks an edge leaving a leaf, which is what makes the
/// [`edge_cap`] prefix meaningful on a hub-heavy graph.
#[must_use]
pub fn edge_importance(degree_a: u32, degree_b: u32) -> f32 {
    (degree_a as f32 + 1.0) * (degree_b as f32 + 1.0)
}

/// Base edge alpha for an `importance` (log-scaled, clamped).
///
/// Leaf-to-leaf edges sit at [`EDGE_ALPHA_MIN`]; the curve saturates at 1.0
/// well before [`EDGE_ALPHA_SATURATE`], so hub-to-hub structure stays solid
/// while the periphery reads as haze rather than as noise.
#[must_use]
pub fn edge_base_alpha(importance: f32) -> f32 {
    if !importance.is_finite() || importance <= 0.0 {
        return EDGE_ALPHA_MIN;
    }
    let octaves = importance.min(EDGE_ALPHA_SATURATE).log2();
    (EDGE_ALPHA_MIN + EDGE_ALPHA_PER_OCTAVE * octaves).clamp(EDGE_ALPHA_MIN, 1.0)
}

/// Per-zoom edge alpha multiplier (D15: edge alpha by zoom).
///
/// Linear in zoom and clamped to `[`EDGE_ALPHA_ZOOM_FLOOR`, 1.0]`: zooming
/// out dims the edge soup, zooming in past 1:1 restores full alpha, and no
/// zoom level can make an edge disappear entirely.
#[must_use]
pub fn edge_alpha_factor(zoom: f32) -> f32 {
    if !zoom.is_finite() || zoom <= 0.0 {
        return EDGE_ALPHA_ZOOM_FLOOR;
    }
    zoom.clamp(EDGE_ALPHA_ZOOM_FLOOR, 1.0)
}

/// How many edge instances to draw for `total` edges at `zoom` (D15: edge
/// count capped by importance).
///
/// The cap scales with zoom between [`EDGE_CAP_ZOOM_MIN`] and
/// [`EDGE_CAP_ZOOM_MAX`] and never exceeds `total`. Combined with the
/// importance ordering of [`ProbeGeometry::edges`], truncating the instance
/// draw to this value is exactly "cap the edge count by importance".
#[must_use]
pub fn edge_cap(total: usize, zoom: f32) -> usize {
    let clamped = if zoom.is_finite() && zoom > 0.0 {
        zoom.clamp(EDGE_CAP_ZOOM_MIN, EDGE_CAP_ZOOM_MAX)
    } else {
        EDGE_CAP_ZOOM_MIN
    };
    // Milli-units keep the scaling integer and exact for the whole clamp
    // range (0.25..=4.0 → 250..=4000).
    let milli = (clamped * 1000.0).round() as usize;
    let scaled = EDGE_CAP_AT_REFERENCE_ZOOM.saturating_mul(milli) / 1000;
    scaled.min(total)
}

/// The base colour for a node kind.
#[must_use]
pub fn color_of(kind: NodeKind) -> [f32; 4] {
    match kind {
        NodeKind::Note => NOTE_COLOR,
        NodeKind::Attachment => ATTACHMENT_COLOR,
    }
}

fn pack_f32x2(v: [f32; 2]) -> [u8; 8] {
    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&v[0].to_le_bytes());
    out[4..8].copy_from_slice(&v[1].to_le_bytes());
    out
}

fn pack_f32x4(v: [f32; 4]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for (i, component) in v.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&component.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_graph::{Delta, EdgeDelta, EdgeKind, GraphModel, NodeDelta, NodeId};

    fn note(id: u64) -> NodeId {
        NodeId::note(id)
    }

    fn att(id: u64) -> NodeId {
        NodeId::attachment(id)
    }

    /// A 3-node fixture: notes 1,2 and attachment 1; edges 1→2 (link),
    /// 2→1 (link), 1→att1 (embed).
    fn fixture() -> flavite_graph::GraphSnapshot {
        let mut model = GraphModel::new();
        model.apply(&[
            Delta::Node(NodeDelta::Added { id: note(1) }),
            Delta::Node(NodeDelta::Added { id: note(2) }),
            Delta::Node(NodeDelta::Added { id: att(1) }),
        ]);
        model.apply(&[
            Delta::Edge(EdgeDelta::Added {
                from: note(1),
                to: note(2),
                kind: EdgeKind::Link,
            }),
            Delta::Edge(EdgeDelta::Added {
                from: note(2),
                to: note(1),
                kind: EdgeKind::Link,
            }),
            Delta::Edge(EdgeDelta::Added {
                from: note(1),
                to: att(1),
                kind: EdgeKind::Embed,
            }),
        ]);
        model.snapshot()
    }

    #[test]
    fn instance_strides_are_32_bytes() {
        assert_eq!(size_of::<NodeInstance>(), NODE_INSTANCE_SIZE);
        assert_eq!(size_of::<EdgeInstance>(), EDGE_INSTANCE_SIZE);
        assert_eq!(size_of::<CameraUniform>(), CAMERA_UNIFORM_SIZE);
        // repr(C) → the fields sit where the WGSL layout says they do.
        let node = NodeInstance {
            center: [1.0, 2.0],
            size: 3.0,
            shape: NodeShape::Square,
            color: [0.5, 0.25, 0.125, 1.0],
        };
        let bytes = node.to_bytes();
        assert_eq!(&bytes[0..8], &pack_f32x2(node.center));
        assert_eq!(&bytes[8..12], &3.0f32.to_le_bytes());
        assert_eq!(&bytes[12..16], &1u32.to_le_bytes(), "shape is u32 @12");
        assert_eq!(&bytes[16..32], &pack_f32x4(node.color), "color @16");
    }

    #[test]
    fn node_instance_bytes_round_trip_at_the_documented_offsets() {
        let node = NodeInstance {
            center: [-3.5, 7.25],
            size: 4.5,
            shape: NodeShape::Circle,
            color: [0.1, 0.2, 0.3, 0.4],
        };
        let bytes = node.to_bytes();
        assert_eq!(f32::from_le_bytes(bytes[0..4].try_into().unwrap()), -3.5);
        assert_eq!(f32::from_le_bytes(bytes[4..8].try_into().unwrap()), 7.25);
        assert_eq!(f32::from_le_bytes(bytes[8..12].try_into().unwrap()), 4.5);
        assert_eq!(u32::from_le_bytes(bytes[12..16].try_into().unwrap()), 0);
        assert_eq!(f32::from_le_bytes(bytes[16..20].try_into().unwrap()), 0.1);
        assert_eq!(f32::from_le_bytes(bytes[20..24].try_into().unwrap()), 0.2);
        assert_eq!(f32::from_le_bytes(bytes[24..28].try_into().unwrap()), 0.3);
        assert_eq!(f32::from_le_bytes(bytes[28..32].try_into().unwrap()), 0.4);
        assert_eq!(node.shape as u32, NodeShape::Circle as u32);
    }

    #[test]
    fn edge_instance_bytes_round_trip_at_the_documented_offsets() {
        let edge = EdgeInstance {
            from: [1.5, -2.5],
            to: [3.5, 4.5],
            alpha: 0.625,
            width: 2.0,
            pad: [0.0, 0.0],
        };
        let bytes = edge.to_bytes();
        assert_eq!(f32::from_le_bytes(bytes[0..4].try_into().unwrap()), 1.5);
        assert_eq!(f32::from_le_bytes(bytes[4..8].try_into().unwrap()), -2.5);
        assert_eq!(f32::from_le_bytes(bytes[8..12].try_into().unwrap()), 3.5);
        assert_eq!(f32::from_le_bytes(bytes[12..16].try_into().unwrap()), 4.5);
        assert_eq!(f32::from_le_bytes(bytes[16..20].try_into().unwrap()), 0.625);
        assert_eq!(f32::from_le_bytes(bytes[20..24].try_into().unwrap()), 2.0);
        assert_eq!(&bytes[24..32], &[0u8; 8]);
    }

    #[test]
    fn camera_uniform_bytes_follow_the_wgsl_offsets() {
        let camera = CameraUniform {
            center: [1.0, 2.0],
            zoom: 3.0,
            edge_alpha: 0.5,
            bounds_position: [10.0, 20.0],
            bounds_size: [100.0, 200.0],
            viewport_size: [1920.0, 1080.0],
            edge_instances: 7,
            node_instances: 9,
        };
        let b = camera.to_bytes();
        let f = |at: usize| f32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        let u = |at: usize| u32::from_le_bytes(b[at..at + 4].try_into().unwrap());
        assert_eq!(f(0), 1.0);
        assert_eq!(f(4), 2.0);
        assert_eq!(f(8), 3.0, "zoom @8");
        assert_eq!(f(12), 0.5, "edge_alpha @12");
        assert_eq!(f(16), 10.0, "bounds_position.x @16");
        assert_eq!(f(20), 20.0);
        assert_eq!(f(24), 100.0, "bounds_size.x @24");
        assert_eq!(f(28), 200.0);
        assert_eq!(f(32), 1920.0, "viewport_size.x @32");
        assert_eq!(f(36), 1080.0);
        assert_eq!(u(40), 7, "edge_instances @40");
        assert_eq!(u(44), 9, "node_instances @44");
        assert_eq!(b.len(), CAMERA_UNIFORM_SIZE);
    }

    #[test]
    fn node_radius_is_increasing_finite_and_positive() {
        assert_eq!(node_radius(0), NODE_RADIUS_BASE);
        let mut previous = -1.0;
        for degree in [0u32, 1, 3, 7, 31, 1023, 65_535, u32::MAX] {
            let r = node_radius(degree);
            assert!(r.is_finite() && r > 0.0, "radius for {degree} = {r}");
            assert!(r > previous, "radius must grow with degree ({degree})");
            previous = r;
        }
        // One octave of degree ≈ one radius step.
        let base = node_radius(0);
        assert!((node_radius(1) - base - NODE_RADIUS_PER_OCTAVE).abs() < 1e-5);
        assert!((node_radius(1023) - base - 10.0 * NODE_RADIUS_PER_OCTAVE).abs() < 1e-3);
    }

    #[test]
    fn edge_importance_is_symmetric_positive_and_finite() {
        assert_eq!(edge_importance(0, 0), 1.0);
        assert_eq!(edge_importance(3, 7), edge_importance(7, 3));
        assert!(edge_importance(0, 1000) > edge_importance(0, 1));
        assert!(
            edge_importance(1000, 1000) > edge_importance(1000, 1),
            "hub-hub beats hub-leaf"
        );
        assert!(
            edge_importance(1, 1000) > edge_importance(1, 1),
            "hub-leaf beats leaf-leaf"
        );
        let worst = edge_importance(u32::MAX, u32::MAX);
        assert!(worst.is_finite() && worst > 0.0, "worst case = {worst}");
    }

    #[test]
    fn edge_base_alpha_is_clamped_monotonic_and_handles_junk() {
        assert_eq!(edge_base_alpha(1.0), EDGE_ALPHA_MIN);
        assert_eq!(edge_base_alpha(0.0), EDGE_ALPHA_MIN);
        assert_eq!(edge_base_alpha(-5.0), EDGE_ALPHA_MIN);
        assert_eq!(edge_base_alpha(f32::NAN), EDGE_ALPHA_MIN);
        assert_eq!(edge_base_alpha(f32::INFINITY), EDGE_ALPHA_MIN);
        let mut previous = 0.0;
        for importance in [1.0f32, 4.0, 64.0, 1024.0, 1e6, 1e30] {
            let alpha = edge_base_alpha(importance);
            assert!(
                (EDGE_ALPHA_MIN..=1.0).contains(&alpha),
                "alpha {alpha} out of range for {importance}"
            );
            assert!(alpha >= previous, "alpha must not decrease");
            previous = alpha;
        }
        assert_eq!(edge_base_alpha(EDGE_ALPHA_SATURATE), 1.0);
        assert_eq!(edge_base_alpha(1e30), 1.0, "saturates at 1.0");
    }

    #[test]
    fn edge_alpha_factor_follows_zoom_and_never_hides_edges() {
        assert_eq!(edge_alpha_factor(1.0), 1.0);
        assert_eq!(edge_alpha_factor(4.0), 1.0, "zooming in restores alpha");
        assert_eq!(
            edge_alpha_factor(EDGE_ALPHA_ZOOM_FLOOR),
            EDGE_ALPHA_ZOOM_FLOOR
        );
        assert_eq!(edge_alpha_factor(0.0), EDGE_ALPHA_ZOOM_FLOOR);
        assert_eq!(edge_alpha_factor(0.001), EDGE_ALPHA_ZOOM_FLOOR);
        assert_eq!(edge_alpha_factor(f32::NAN), EDGE_ALPHA_ZOOM_FLOOR);
        assert_eq!(edge_alpha_factor(-1.0), EDGE_ALPHA_ZOOM_FLOOR);
        assert!((edge_alpha_factor(0.5) - 0.5).abs() < 1e-6);
        let mut previous = 0.0;
        for zoom in [0.01f32, 0.1, 0.25, 0.5, 0.9, 1.0, 10.0] {
            let factor = edge_alpha_factor(zoom);
            assert!(factor >= previous, "alpha factor must not decrease");
            assert!((EDGE_ALPHA_ZOOM_FLOOR..=1.0).contains(&factor));
            previous = factor;
        }
    }

    #[test]
    fn edge_cap_scales_with_zoom_and_never_exceeds_total() {
        // Zoomed out to the floor: a quarter of the reference cap.
        assert_eq!(
            edge_cap(1_000_000, EDGE_CAP_ZOOM_MIN),
            EDGE_CAP_AT_REFERENCE_ZOOM / 4
        );
        assert_eq!(edge_cap(1_000_000, 1.0), EDGE_CAP_AT_REFERENCE_ZOOM);
        assert_eq!(
            edge_cap(1_000_000, EDGE_CAP_ZOOM_MAX),
            EDGE_CAP_AT_REFERENCE_ZOOM * 4
        );
        // Clamped at both zoom ends.
        assert_eq!(
            edge_cap(1_000_000, 0.001),
            edge_cap(1_000_000, EDGE_CAP_ZOOM_MIN)
        );
        assert_eq!(
            edge_cap(1_000_000, 1_000.0),
            edge_cap(1_000_000, EDGE_CAP_ZOOM_MAX)
        );
        assert_eq!(
            edge_cap(1_000_000, f32::NAN),
            edge_cap(1_000_000, EDGE_CAP_ZOOM_MIN)
        );
        // Never more than what exists — a 200k-edge graph is capped below
        // its own size at reference zoom, and exactly at its own size once
        // the cap would exceed it.
        assert_eq!(edge_cap(200_000, 1.0), EDGE_CAP_AT_REFERENCE_ZOOM);
        assert_eq!(edge_cap(50_000, 1.0), 50_000);
        assert_eq!(edge_cap(0, 1.0), 0);
        // Monotonic in zoom.
        let mut previous = 0;
        for zoom in [0.25f32, 0.5, 1.0, 2.0, 4.0] {
            let cap = edge_cap(1_000_000, zoom);
            assert!(cap >= previous, "cap must not shrink as zoom grows");
            previous = cap;
        }
    }

    #[test]
    fn needs_upload_only_when_generation_or_capacity_moves() {
        // First upload.
        assert!(needs_upload(None, 0, true));
        // Steady state: same generation, capacity already fits.
        assert!(!needs_upload(Some(0), 0, true));
        // New geometry generation.
        assert!(needs_upload(Some(0), 1, true));
        // Same generation but the buffers are too small (a re-generation
        // produced more instances than were ever allocated).
        assert!(needs_upload(Some(0), 0, false));
        // Both moved.
        assert!(needs_upload(Some(3), 4, false));
    }

    #[test]
    fn from_snapshot_builds_one_node_instance_per_node() {
        let snapshot = fixture();
        let positions = [[0.0, 0.0], [10.0, 0.0], [20.0, 20.0]];
        let geometry = ProbeGeometry::from_snapshot(&snapshot, &positions, 7).unwrap();
        assert_eq!(geometry.node_count(), 3);
        assert_eq!(geometry.generation(), 7);
        assert_eq!(geometry.edge_count(), 3);

        // dense 0 = note 1 (degree 3: two out-edges + one in-edge),
        // dense 1 = note 2 (degree 2), dense 2 = attachment 1 (degree 1).
        assert_eq!(geometry.nodes()[0].center, [0.0, 0.0]);
        assert_eq!(geometry.nodes()[0].shape, NodeShape::Circle);
        assert_eq!(geometry.nodes()[1].center, [10.0, 0.0]);
        assert_eq!(geometry.nodes()[2].shape, NodeShape::Square);
        assert_eq!(geometry.nodes()[2].color, ATTACHMENT_COLOR);

        // Degree-based sizing: the hub is largest.
        assert!(geometry.nodes()[0].size > geometry.nodes()[1].size);
        assert!(geometry.nodes()[1].size > geometry.nodes()[2].size);

        // Every edge instance has endpoints drawn from the position array.
        for edge in geometry.edges() {
            assert!(positions.contains(&edge.from));
            assert!(positions.contains(&edge.to));
            assert_eq!(edge.width, EDGE_WIDTH_BASE);
            assert_eq!(edge.pad, [0.0; 2], "padding holds the stride at 32 B");
        }
    }

    #[test]
    fn from_snapshot_rejects_a_mismatched_position_buffer() {
        let snapshot = fixture();
        let err = ProbeGeometry::from_snapshot(&snapshot, &[[0.0, 0.0]], 0).unwrap_err();
        assert_eq!(
            err,
            GeometryError::PositionCountMismatch {
                nodes: 3,
                positions: 1
            }
        );
        // Same topology, exactly one position per node → accepted.
        assert!(ProbeGeometry::from_snapshot(&snapshot, &[[0.0, 0.0]; 3], 0).is_ok());
        // Too many positions is just as stale as too few.
        assert!(ProbeGeometry::from_snapshot(&snapshot, &[[0.0, 0.0]; 4], 0).is_err());
    }

    #[test]
    fn edges_are_sorted_importance_descending() {
        // A star: node 0 is a hub of degree 4, leaves have degree 1.
        let mut model = GraphModel::new();
        model.apply(
            &(0..5u64)
                .map(|i| Delta::Node(NodeDelta::Added { id: note(i) }))
                .collect::<Vec<_>>(),
        );
        model.apply(
            &(1..5u64)
                .map(|i| {
                    Delta::Edge(EdgeDelta::Added {
                        from: note(0),
                        to: note(i),
                        kind: EdgeKind::Link,
                    })
                })
                .collect::<Vec<_>>(),
        );
        model.apply(&[
            Delta::Edge(EdgeDelta::Added {
                from: note(1),
                to: note(2),
                kind: EdgeKind::Link,
            }),
            Delta::Edge(EdgeDelta::Added {
                from: note(3),
                to: note(4),
                kind: EdgeKind::Link,
            }),
        ]);
        let snapshot = model.snapshot();
        let positions: Vec<[f32; 2]> = (0..5).map(|i| [i as f32 * 10.0, 0.0]).collect();
        let geometry = ProbeGeometry::from_snapshot(&snapshot, &positions, 0).unwrap();
        assert_eq!(geometry.edge_count(), 6);

        // Recompute every expected alpha straight from the snapshot's degree
        // array, sort it independently, and demand an exact match — so the
        // test proves the *whole* ordering, not one hand-derived value.
        let mut expected: Vec<f32> = Vec::with_capacity(6);
        for src in 0..snapshot.node_count() {
            let start = snapshot.out_offsets[src] as usize;
            let end = snapshot.out_offsets[src + 1] as usize;
            for row in start..end {
                let dst = snapshot.out_indices[row] as usize;
                expected.push(edge_base_alpha(edge_importance(
                    snapshot.degrees[src],
                    snapshot.degrees[dst],
                )));
            }
        }
        assert_eq!(expected.len(), 6, "one alpha per edge");
        expected.sort_by(|a, b| b.partial_cmp(a).unwrap_or(Ordering::Equal));

        let actual: Vec<f32> = geometry.edges().iter().map(|edge| edge.alpha).collect();
        for (index, (got, want)) in actual.iter().zip(&expected).enumerate() {
            assert!(
                (got - want).abs() < 1e-6,
                "edge {index}: alpha {got} is not the importance-ordered {want}"
            );
        }

        // Node 0 is the hub (degree 4); every edge leaving it outranks every
        // leaf-to-leaf edge, so the sort puts those four first and the two
        // leaf-to-leaf edges last.
        for edge in &geometry.edges()[..4] {
            assert_eq!(edge.from, positions[0], "hub edges lead the array");
        }
        for edge in &geometry.edges()[4..] {
            assert_ne!(edge.from, positions[0], "leaf-to-leaf edges trail");
        }
        // Stable sort: ties keep CSR order, so 1→2 precedes 3→4.
        assert_eq!(geometry.edges()[4].from, positions[1]);
        assert_eq!(geometry.edges()[4].to, positions[2]);
        assert_eq!(geometry.edges()[5].from, positions[3]);
        assert_eq!(geometry.edges()[5].to, positions[4]);
    }

    #[test]
    fn geometry_is_deterministic_for_a_given_snapshot() {
        let snapshot = fixture();
        let positions = [[1.0, 2.0], [3.0, 4.0], [5.0, 6.0]];
        let a = ProbeGeometry::from_snapshot(&snapshot, &positions, 1).unwrap();
        let b = ProbeGeometry::from_snapshot(&snapshot, &positions, 1).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn visible_edge_count_is_a_prefix_of_the_sorted_edges() {
        let snapshot = fixture();
        let positions = [[0.0, 0.0]; 3];
        let geometry = ProbeGeometry::from_snapshot(&snapshot, &positions, 0).unwrap();
        let cap = geometry.visible_edge_count(1.0);
        assert!(cap <= geometry.edge_count());
        // At reference zoom the cap (100k) exceeds this tiny graph, so the
        // whole edge set draws.
        assert_eq!(cap, geometry.edge_count());
        // A graph larger than the cap truncates to the prefix.
        assert_eq!(edge_cap(500_000, 1.0), EDGE_CAP_AT_REFERENCE_ZOOM);
    }

    #[test]
    fn camera_screen_world_round_trips() {
        let camera = Camera {
            center: [120.0, -40.0],
            zoom: 3.5,
        };
        let bounds_position = [17.0, 33.0];
        let bounds_size = [800.0, 600.0];
        for screen in [[0.0f32, 0.0], [417.0, 333.0], [-10.0, 640.0]] {
            let world = camera.screen_to_world(screen, bounds_position, bounds_size);
            let back = camera.world_to_screen(world, bounds_position, bounds_size);
            assert!((back[0] - screen[0]).abs() < 1e-3, "x round trip");
            assert!((back[1] - screen[1]).abs() < 1e-3, "y round trip");
        }
        // The widget centre maps to the camera centre by definition.
        let centre = camera.screen_to_world(
            [
                bounds_position[0] + bounds_size[0] * 0.5,
                bounds_position[1] + bounds_size[1] * 0.5,
            ],
            bounds_position,
            bounds_size,
        );
        assert!((centre[0] - camera.center[0]).abs() < 1e-4);
        assert!((centre[1] - camera.center[1]).abs() < 1e-4);
    }

    #[test]
    fn zoom_at_pins_the_anchor_world_point_to_the_same_screen_point() {
        let mut camera = Camera {
            center: [50.0, -20.0],
            zoom: 2.0,
        };
        let bounds_position = [0.0, 0.0];
        let bounds_size = [1024.0, 768.0];
        let anchor = [300.0, 500.0];
        let world_before = camera.screen_to_world(anchor, bounds_position, bounds_size);
        camera.zoom_at(anchor, bounds_position, bounds_size, 1.6);
        let screen_after = camera.world_to_screen(world_before, bounds_position, bounds_size);
        assert!((camera.zoom - 3.2).abs() < 1e-5, "zoom applied");
        assert!(
            (screen_after[0] - anchor[0]).abs() < 1e-3
                && (screen_after[1] - anchor[1]).abs() < 1e-3,
            "anchor stayed put: {screen_after:?} vs {anchor:?}"
        );
    }

    #[test]
    fn zoom_clamps_and_pan_translates() {
        let mut camera = Camera::default();
        // A non-positive or non-finite factor is junk, not a request to zoom
        // to nothing: it is rejected and the zoom stays put (still clamped).
        assert_eq!(camera.scaled_zoom(0.0), 1.0, "zero is junk, not empty");
        assert_eq!(camera.scaled_zoom(-2.0), 1.0, "negative is junk");
        assert_eq!(camera.scaled_zoom(f32::NAN), 1.0, "junk leaves it alone");
        assert_eq!(camera.scaled_zoom(f32::INFINITY), 1.0, "so does infinity");
        // A genuine zoom in/out still clamps to the documented bounds.
        assert_eq!(camera.scaled_zoom(1e9), Camera::ZOOM_MAX);
        assert_eq!(camera.scaled_zoom(1e-9), Camera::ZOOM_MIN);

        // Already at the floor: a further zoom-out does nothing at all.
        camera.zoom = Camera::ZOOM_MIN;
        assert_eq!(camera.scaled_zoom(0.5), Camera::ZOOM_MIN);
        camera.zoom = Camera::ZOOM_MAX;
        assert_eq!(camera.scaled_zoom(2.0), Camera::ZOOM_MAX);

        let mut camera = Camera::default();
        camera.zoom_at([0.0, 0.0], [0.0, 0.0], [800.0, 600.0], 1e9);
        assert_eq!(camera.zoom, Camera::ZOOM_MAX);

        let mut camera = Camera::default();
        camera.pan_by(15.0, -5.0);
        assert_eq!(camera.center, [15.0, -5.0]);
        camera.pan_by(-15.0, 5.0);
        assert_eq!(camera.center, [0.0, 0.0]);
    }

    #[test]
    fn fit_extent_frames_the_world_rectangle() {
        let mut camera = Camera {
            center: [999.0, 999.0],
            zoom: 7.0,
        };
        camera.fit_extent(2000.0, [0.0, 0.0], [800.0, 600.0]);
        assert_eq!(camera.center, [0.0, 0.0]);
        assert!((camera.zoom - 0.3).abs() < 1e-5, "zoom = 600 / 2000");
        // Degenerate inputs stay finite and in range.
        camera.fit_extent(0.0, [0.0, 0.0], [800.0, 600.0]);
        assert!(camera.zoom.is_finite());
        assert!((Camera::ZOOM_MIN..=Camera::ZOOM_MAX).contains(&camera.zoom));
        camera.fit_extent(f32::NAN, [0.0, 0.0], [0.0, 0.0]);
        assert!(camera.zoom.is_finite());
    }

    #[test]
    fn node_shape_maps_from_node_kind() {
        assert_eq!(NodeShape::from(NodeKind::Note), NodeShape::Circle);
        assert_eq!(NodeShape::from(NodeKind::Attachment), NodeShape::Square);
        assert_eq!(color_of(NodeKind::Note), NOTE_COLOR);
        assert_eq!(color_of(NodeKind::Attachment), ATTACHMENT_COLOR);
    }

    #[test]
    fn empty_snapshot_yields_empty_geometry() {
        let snapshot = flavite_graph::GraphModel::new().snapshot();
        let geometry = ProbeGeometry::from_snapshot(&snapshot, &[], 3).unwrap();
        assert_eq!(geometry.node_count(), 0);
        assert_eq!(geometry.edge_count(), 0);
        assert_eq!(geometry.visible_edge_count(1.0), 0);
        assert_eq!(geometry.generation(), 3);
    }
}
