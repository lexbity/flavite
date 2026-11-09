//! Production instanced wgpu renderer for the graph view (RFC-0003 W07,
//! TD-10, D15).
//!
//! This is the shared production shader core: the WGSL module, the GPU
//! pipeline, the per-frame primitive, and the typed gesture program. The S2
//! probe (`shader_probe`) is now a *consumer* of this core that adds frame
//! instrumentation — production no longer delegates to probe types, which
//! is the dependency inversion TD-10 named.
//!
//! Architecture invariants preserved from the probe measurements:
//!
//! - Geometry is behind an `Arc`; uploads are gated on a generation; the
//!   frame path takes no locks (D15).
//! - All visual decisions (degree-based node radius, edge alpha by zoom,
//!   importance-capped edge prefix) are made on the CPU in
//!   [`super::geometry`]/[`super::instance`] — the WGSL only projects,
//!   rasterizes, and blends, so the tunables stay unit-testable.
//! - The widget holds no mutable camera authority: input becomes a typed
//!   [`GraphMessage`] and the application owns the per-view camera, applies
//!   it, and rebuilds the widget in every `view` (§7.2: one camera per
//!   graph view).
//!
//! Hit-testing happens against the *drawn* geometry with the identity
//! table, so a published [`crate::graph::geometry::HitNode`] always names
//! the node under the cursor for this frame's [`RenderKey`](flavite_graph::RenderKey);
//! the host refuses gesture arbitration when the key moved mid-gesture
//! (§9.3).

use std::sync::Arc;

use iced::mouse;
use iced::wgpu;
use iced::widget::shader::{
    Action, Pipeline as ShaderPipeline, Primitive as ShaderPrimitive, Program as ShaderProgram,
    Shader, Viewport,
};
use iced::{Event, Length, Point, Rectangle, window};

use super::geometry::{HitNode, RenderGeometry};
use super::instance::{
    self, CAMERA_UNIFORM_SIZE, Camera, CameraUniform, EDGE_INSTANCE_SIZE, NODE_INSTANCE_SIZE,
};
use crate::style::tokens::{self, Scheme};

/// The production WGSL module: camera uniform, instanced nodes, instanced
/// edges. The `Camera` struct here must match [`CameraUniform`]
/// field-for-field; `tests/shader_validation.rs` parses this string with
/// `naga` and asserts exactly that.
pub const GRAPH_WGSL: &str = r#"
struct Camera {
    // offsets are asserted against the Rust `CameraUniform` by
    // tests/shader_validation.rs
    center: vec2<f32>,           //  0
    zoom: f32,                   //  8  world -> logical widget px
    edge_alpha: f32,             // 12  per-zoom edge multiplier (D15)
    bounds_position: vec2<f32>,  // 16  local viewport origin (0, 0)
    bounds_size: vec2<f32>,      // 24  widget size, physical px
    viewport_size: vec2<f32>,    // 32  widget viewport size, physical px
    edge_instances: u32,         // 40  importance-capped draw count
    node_instances: u32,         // 44
    edge_color: vec4<f32>,       // 48  scheme token (W11), written per frame
}

@group(0) @binding(0) var<uniform> camera: Camera;

// --- projection -----------------------------------------------------------
// The camera works in *logical* widget pixels; `prepare` scales it into the
// physical pixels the render target actually uses.
fn to_screen(world: vec2<f32>) -> vec2<f32> {
    return (world - camera.center) * camera.zoom
        + camera.bounds_position + camera.bounds_size * 0.5;
}

fn to_ndc(screen: vec2<f32>) -> vec4<f32> {
    let size = max(camera.viewport_size, vec2<f32>(1.0, 1.0));
    let x = screen.x / size.x * 2.0 - 1.0;
    let y = 1.0 - screen.y / size.y * 2.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

// --- nodes ----------------------------------------------------------------
// Buffer 0: the four corners of a unit quad, triangle-strip order.
// Buffer 1: one NodeInstance per node (32 bytes, see `graph::instance`).

struct NodeInput {
    @location(0) corner: vec2<f32>,
    @location(1) center: vec2<f32>,
    @location(2) size: f32,
    @location(3) shape: u32,
    @location(4) color: vec4<f32>,
};

struct NodeOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) corner: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) shape: u32,
};

@vertex
fn vs_nodes(input: NodeInput) -> NodeOutput {
    var out: NodeOutput;
    out.position = to_ndc(to_screen(input.center + input.corner * input.size));
    out.corner = input.corner;
    out.color = input.color;
    out.shape = input.shape;
    return out;
}

@fragment
fn fs_nodes(input: NodeOutput) -> @location(0) vec4<f32> {
    // 0u = circle (notes), 1u = square (attachments). Masking in the
    // fragment shader keeps one pipeline for both shapes.
    if (input.shape == 0u && dot(input.corner, input.corner) > 1.0) {
        discard;
    }
    return vec4<f32>(input.color.rgb, input.color.a);
}

// --- edges ----------------------------------------------------------------
// Buffer 0: corner.x runs 0..1 along the segment, corner.y -1..1 across it,
// so a quad is built in screen space at a constant stroke width.
// Buffer 1: one EdgeInstance per edge (32 bytes, see `graph::instance`).

// NB: the member names cannot be `from`/`to` — those are WGSL keywords.
struct EdgeInput {
    @location(0) corner: vec2<f32>,
    @location(1) edge_a: vec2<f32>,
    @location(2) edge_b: vec2<f32>,
    @location(3) alpha: f32,
    @location(4) width: f32,
};

struct EdgeOutput {
    @builtin(position) position: vec4<f32>,
    @location(0) alpha: f32,
};

@vertex
fn vs_edges(input: EdgeInput) -> EdgeOutput {
    var out: EdgeOutput;

    let a = to_screen(input.edge_a);
    let b = to_screen(input.edge_b);

    var direction = b - a;
    let span = length(direction);
    // A zero-length segment (self-loop, or both ends at the same pixel)
    // rasterizes to nothing rather than dividing by zero.
    direction = select(direction / span, vec2<f32>(0.0, 0.0), span <= 0.0);

    let normal = vec2<f32>(-direction.y, direction.x) * (input.width * 0.5);
    let screen = mix(a, b, input.corner.x) + normal * input.corner.y;

    out.position = to_ndc(screen);
    out.alpha = input.alpha * camera.edge_alpha;
    return out;
}

// The edge line color is no longer a shader constant: it arrives in the
// per-frame uniform (`camera.edge_color`) from the active scheme's semantic
// edge token (W11/D-01), so a light/dark switch recolors edges without a
// re-layout or a pipeline rebuild.

@fragment
fn fs_edges(input: EdgeOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(camera.edge_color.rgb, input.alpha);
}
"#;

/// The four entry points [`GRAPH_WGSL`] must expose — named once so the
/// pipeline construction and the headless validation in
/// `tests/shader_validation.rs` cannot disagree about "the shader".
pub const NODE_VERTEX_ENTRY: &str = "vs_nodes";
/// See [`NODE_VERTEX_ENTRY`].
pub const NODE_FRAGMENT_ENTRY: &str = "fs_nodes";
/// See [`NODE_VERTEX_ENTRY`].
pub const EDGE_VERTEX_ENTRY: &str = "vs_edges";
/// See [`NODE_VERTEX_ENTRY`].
pub const EDGE_FRAGMENT_ENTRY: &str = "fs_edges";

/// The WGSL name of the uniform block bound at group 0, binding 0.
pub const CAMERA_UNIFORM_NAME: &str = "Camera";

/// The WGSL location of the vertex buffer slot holding the quad corners.
pub const CORNER_LOCATION: u32 = 0;

/// Convert a scroll-wheel movement into a multiplicative zoom factor.
///
/// Trackpads report sub-line deltas; mice report whole lines. Both land in
/// the same smooth range here, and the result is always strictly positive
/// so a zoom step can never collapse or invert the camera.
#[must_use]
pub fn scroll_factor(lines: f32) -> f32 {
    if !lines.is_finite() {
        return 1.0;
    }
    (1.0 + lines * 0.1).clamp(0.2, 5.0)
}

/// How a pointer release ended, from the widget's point of view (§9.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleaseKind {
    /// No drag happened: the release is a click candidate for the arbiter.
    Click,
    /// The background was dragged (pan) — suppresses click recognition.
    PanCompleted,
    /// A node body was dragged — spatial only; the host commits a pin and
    /// click recognition is suppressed (§9.3: node drag never implies an
    /// edge or an insert).
    NodeDragCompleted {
        /// The node that was dragged.
        node: HitNode,
    },
}

/// Messages the graph widget publishes to its owner.
///
/// The widget holds no mutable camera and no selection authority of its
/// own: input becomes a message, the application (which owns the
/// per-view camera, the selection, and the arbiter) applies it and
/// rebuilds the widget on the next `view`. Camera and pin state therefore
/// survive reparenting, and a stale widget can never overwrite newer
/// host state (D-03, §8.2).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GraphMessage {
    BoxPreview {
        first: [f32; 2],
        second: [f32; 2],
    },
    BoxSelected {
        first: [f32; 2],
        second: [f32; 2],
    },
    /// The window changed size; `size` is in logical pixels.
    Viewport {
        /// Logical window width/height.
        size: [f32; 2],
    },
    /// The display scale factor changed.
    ScaleFactor {
        /// New device-pixel-per-logical-pixel ratio.
        scale: f32,
    },
    /// A background drag moved the cursor by this many *logical* widget-
    /// local pixels (the application converts with its camera authority).
    PanByPixels {
        /// Horizontal movement, logical pixels.
        dx: f32,
        /// Vertical movement, logical pixels.
        dy: f32,
    },
    /// Zoom by `factor` while pinning `anchor` (logical widget-local px).
    ZoomAt {
        /// Widget-local logical pixel under which to anchor the zoom.
        anchor: [f32; 2],
        /// Multiplicative zoom step; must be > 0.
        factor: f32,
    },
    /// A node body is being dragged; its preview world position is
    /// `world` (spatial-only — edges follow visually, nothing semantic
    /// happens until release, §9.3).
    NodeDragged {
        /// Dragged node identity.
        node: HitNode,
        /// Current cursor world position.
        world: [f32; 2],
    },
    /// A primary-button release with everything the host needs to
    /// arbitrate exactly one interaction: `node` is the identity under
    /// the cursor *at release* (the hit table of the current frame), and
    /// `kind` says whether the press-to-release pair was a click or a
    /// completed drag.
    Released {
        /// Hit node under the release point, if any.
        node: Option<HitNode>,
        /// World position of the release.
        world: [f32; 2],
        /// How the gesture ended.
        kind: ReleaseKind,
    },
}

impl GraphMessage {
    /// The world-space pan this message amounts to at `zoom`.
    ///
    /// Dragging the cursor right by `dx` logical pixels must drag the
    /// world *left*, hence the negation.
    #[must_use]
    pub fn pan_world_at(self, zoom: f32) -> Option<[f32; 2]> {
        let (dx, dy) = match self {
            Self::PanByPixels { dx, dy } => (dx, dy),
            _ => return None,
        };
        let zoom = if zoom.is_finite() && zoom > 0.0 {
            zoom
        } else {
            1.0
        };
        Some([-dx / zoom, -dy / zoom])
    }
}

/// Pointer state inside the widget: which drag, if any, is in flight.
///
/// Deliberately only input bookkeeping — selection, camera, pins, and
/// gesture arbitration all live in the host so nothing important can be
/// lost when iced recreates widget state after a reparent (D-03).
#[derive(Debug, Default, Clone)]
pub struct GraphProgramState {
    resources: Arc<arc_swap::ArcSwapOption<ViewBuffers>>,
    shift: bool,
    selecting_box: bool,
    box_first: Option<[f32; 2]>,
    pressed: bool,
    press_node: Option<HitNode>,
    drag_anchor: Option<Point>,
    last_cursor: Option<Point>,
    moved_pixels: f32,
}

/// The per-frame primitive the production program hands to `iced`: the
/// geometry behind an `Arc` (the render thread never blocks on the
/// layout worker, D15) plus this view's camera for the frame.
#[derive(Debug, Clone)]
pub struct GraphPrimitive {
    resources: Arc<arc_swap::ArcSwapOption<ViewBuffers>>,
    geometry: Arc<RenderGeometry>,
    camera: Camera,
    /// Active color scheme; only affects per-frame uniform data (edge
    /// color), never the camera, positions, or topology.
    scheme: Scheme,
    node_count: u32,
    edge_count: u32,
}

impl GraphPrimitive {
    /// Build the primitive for `camera` over `geometry`, painting with
    /// `scheme`'s semantic tokens.
    #[must_use]
    pub fn new(geometry: Arc<RenderGeometry>, camera: Camera, scheme: Scheme) -> Self {
        let node_count = count_u32(geometry.node_count());
        let edge_count = count_u32(geometry.visible_edge_count(camera.zoom));
        Self {
            resources: Arc::default(),
            geometry,
            camera,
            scheme,
            node_count,
            edge_count,
        }
    }

    /// The camera this frame renders with.
    #[must_use]
    pub const fn camera(&self) -> Camera {
        self.camera
    }

    /// Node instances drawn.
    #[must_use]
    pub const fn node_count(&self) -> u32 {
        self.node_count
    }

    /// Edge instances drawn (importance-capped).
    #[must_use]
    pub const fn edge_count(&self) -> u32 {
        self.edge_count
    }

    /// The geometry generation backing this frame.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.geometry.generation()
    }
}

/// Cast a count to `u32` for a GPU draw call (saturating: a renderer must
/// never take the application down on a bad cast).
fn count_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The GPU state behind every [`GraphPrimitive`]: created by `iced` on
/// first use and cached for the window's lifetime. Instance buffers are
/// (re)uploaded only when the geometry generation moves; a steady-state
/// frame is one 64-byte uniform write plus two instanced draws.
pub struct GraphPipeline {
    identity: Arc<()>,
    camera_layout: wgpu::BindGroupLayout,
    nodes: wgpu::RenderPipeline,
    edges: wgpu::RenderPipeline,
    node_corners: wgpu::Buffer,
    edge_corners: wgpu::Buffer,
}

/// Buffers belong to a mounted view, never the toolkit's type-global pipeline.
#[derive(Debug)]
struct ViewBuffers {
    pipeline: std::sync::Weak<()>,
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    node_slot: InstanceSlot,
    edge_slot: InstanceSlot,
    geometry: std::sync::Weak<RenderGeometry>,
}
impl ViewBuffers {
    fn new(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, identity: &Arc<()>) -> Self {
        let camera_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("flavite graph camera"),
            size: CAMERA_UNIFORM_SIZE as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("flavite graph camera"),
            layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });
        Self {
            pipeline: Arc::downgrade(identity),
            camera_buffer,
            camera_bind_group,
            node_slot: InstanceSlot::default(),
            edge_slot: InstanceSlot::default(),
            geometry: std::sync::Weak::new(),
        }
    }
}

/// A grow-only instance buffer.
#[derive(Debug, Default, Clone)]
struct InstanceSlot {
    buffer: Option<wgpu::Buffer>,
    capacity_bytes: usize,
}

impl InstanceSlot {
    fn upload(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, label: &str, bytes: &[u8]) {
        if self.capacity_bytes < bytes.len() {
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: bytes.len() as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity_bytes = bytes.len();
        }
        if let Some(buffer) = &self.buffer {
            queue.write_buffer(buffer, 0, bytes);
        }
    }
}

impl ShaderPipeline for GraphPipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("flavite graph"),
            source: wgpu::ShaderSource::Wgsl(GRAPH_WGSL.into()),
        });

        let camera_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("flavite graph camera"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(CAMERA_UNIFORM_SIZE as u64),
                    },
                    count: None,
                }],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("flavite graph"),
            bind_group_layouts: &[&camera_bind_group_layout],
            push_constant_ranges: &[],
        });

        let node_corners = quad_buffer(
            device,
            queue,
            "flavite graph node corners",
            [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]],
        );
        let edge_corners = quad_buffer(
            device,
            queue,
            "flavite graph edge corners",
            [[0.0, -1.0], [1.0, -1.0], [0.0, 1.0], [1.0, 1.0]],
        );

        let corner_attributes = [wgpu::VertexAttribute {
            format: wgpu::VertexFormat::Float32x2,
            offset: 0,
            shader_location: CORNER_LOCATION,
        }];

        let node_attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 8,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Uint32,
                offset: 12,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x4,
                offset: 16,
                shader_location: 4,
            },
        ];
        let node_layout = wgpu::VertexBufferLayout {
            array_stride: NODE_INSTANCE_SIZE as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &node_attributes,
        };

        let edge_attributes = [
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 0,
                shader_location: 1,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32x2,
                offset: 8,
                shader_location: 2,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 16,
                shader_location: 3,
            },
            wgpu::VertexAttribute {
                format: wgpu::VertexFormat::Float32,
                offset: 20,
                shader_location: 4,
            },
        ];
        let edge_layout = wgpu::VertexBufferLayout {
            array_stride: EDGE_INSTANCE_SIZE as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &edge_attributes,
        };

        let blend = Some(wgpu::BlendState::ALPHA_BLENDING);

        let nodes = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("flavite graph nodes"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some(NODE_VERTEX_ENTRY),
                compilation_options: Default::default(),
                buffers: &[corner_layout(&corner_attributes), node_layout],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(NODE_FRAGMENT_ENTRY),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        let edges = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("flavite graph edges"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some(EDGE_VERTEX_ENTRY),
                compilation_options: Default::default(),
                buffers: &[corner_layout(&corner_attributes), edge_layout],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some(EDGE_FRAGMENT_ENTRY),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        Self {
            identity: Arc::new(()),
            camera_layout: camera_bind_group_layout,
            nodes,
            edges,
            node_corners,
            edge_corners,
        }
    }
}

/// The per-vertex layout of the shared quad-corner buffer.
fn corner_layout(attributes: &[wgpu::VertexAttribute]) -> wgpu::VertexBufferLayout<'_> {
    wgpu::VertexBufferLayout {
        array_stride: 8,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes,
    }
}

/// Create the shared four-vertex corner buffer for a triangle strip.
fn quad_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    label: &str,
    corners: [[f32; 2]; 4],
) -> wgpu::Buffer {
    let mut bytes = [0u8; 32];
    for (index, corner) in corners.iter().enumerate() {
        bytes[index * 8..index * 8 + 4].copy_from_slice(&corner[0].to_le_bytes());
        bytes[index * 8 + 4..index * 8 + 8].copy_from_slice(&corner[1].to_le_bytes());
    }
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: bytes.len() as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&buffer, 0, &bytes);
    buffer
}

fn widget_camera_uniform(
    camera: Camera,
    size: iced::Size,
    scale: f32,
    nodes: u32,
    edges: u32,
    scheme: Scheme,
) -> CameraUniform {
    let physical = [size.width * scale, size.height * scale];
    let [r, g, b] = tokens::graph_edge_color(scheme);
    CameraUniform {
        center: camera.center,
        zoom: camera.zoom * scale,
        edge_alpha: instance::edge_alpha_factor(camera.zoom),
        bounds_position: [0.0, 0.0],
        bounds_size: physical,
        viewport_size: physical,
        edge_instances: edges,
        node_instances: nodes,
        edge_color: [r, g, b, 1.0],
    }
}

impl ShaderPrimitive for GraphPrimitive {
    type Pipeline = GraphPipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        // iced sets the render-pass viewport to this widget's rectangle.
        // NDC must be local to it; adding the window origin here applies the
        // position/scale twice and separates painted nodes from hit targets.
        let camera = widget_camera_uniform(
            self.camera,
            bounds.size(),
            viewport.scale_factor(),
            self.node_count,
            self.edge_count,
            self.scheme,
        );
        // A recreated pipeline/device must not inherit buffers or bind groups
        // belonging to the previous GPU device.
        let previous = self.resources.load_full().filter(|resources| {
            resources
                .pipeline
                .upgrade()
                .is_some_and(|identity| Arc::ptr_eq(&identity, &pipeline.identity))
        });
        if let Some(resources) = &previous {
            queue.write_buffer(&resources.camera_buffer, 0, &camera.to_bytes());
            if resources
                .geometry
                .upgrade()
                .is_some_and(|geometry| Arc::ptr_eq(&geometry, &self.geometry))
            {
                return;
            }
        }
        let mut resources = previous.as_ref().map_or_else(
            || ViewBuffers::new(device, &pipeline.camera_layout, &pipeline.identity),
            |old| ViewBuffers {
                pipeline: old.pipeline.clone(),
                camera_buffer: old.camera_buffer.clone(),
                camera_bind_group: old.camera_bind_group.clone(),
                node_slot: old.node_slot.clone(),
                edge_slot: old.edge_slot.clone(),
                geometry: std::sync::Weak::new(),
            },
        );
        queue.write_buffer(&resources.camera_buffer, 0, &camera.to_bytes());
        if self.geometry.node_count() > 0 {
            let bytes: Vec<u8> = self
                .geometry
                .nodes()
                .iter()
                .flat_map(|node| node.to_bytes())
                .collect();
            resources
                .node_slot
                .upload(device, queue, "flavite graph nodes", &bytes);
        }
        if self.geometry.edge_count() > 0 {
            let bytes: Vec<u8> = self
                .geometry
                .edges()
                .iter()
                .flat_map(|edge| edge.to_bytes())
                .collect();
            resources
                .edge_slot
                .upload(device, queue, "flavite graph edges", &bytes);
        }
        resources.geometry = Arc::downgrade(&self.geometry);
        self.resources.store(Some(Arc::new(resources)));
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let Some(resources) = self.resources.load_full() else {
            return false;
        };
        // A zero-instance draw is skipped rather than issued with count
        // zero: wgpu would still require every declared vertex buffer slot.
        if let Some(buffer) = &resources.node_slot.buffer
            && self.node_count > 0
        {
            render_pass.set_pipeline(&pipeline.nodes);
            render_pass.set_bind_group(0, &resources.camera_bind_group, &[]);
            render_pass.set_vertex_buffer(0, pipeline.node_corners.slice(..));
            render_pass.set_vertex_buffer(1, buffer.slice(..));
            render_pass.draw(0..4, 0..self.node_count);
        }

        if let Some(buffer) = &resources.edge_slot.buffer
            && self.edge_count > 0
        {
            render_pass.set_pipeline(&pipeline.edges);
            render_pass.set_bind_group(0, &resources.camera_bind_group, &[]);
            render_pass.set_vertex_buffer(0, pipeline.edge_corners.slice(..));
            render_pass.set_vertex_buffer(1, buffer.slice(..));
            render_pass.draw(0..4, 0..self.edge_count);
        }

        true
    }
}

/// The movement past which a press becomes a drag (§9.3: 4 logical px
/// starts node drag or pan and cancels click recognition).
pub const DRAG_THRESHOLD_LOGICAL: f32 = 4.0;

/// The production graph program: renders [`RenderGeometry`]
/// through the view camera and turns real pointer events into typed
/// [`GraphMessage`]s with identity-safe hits.
#[derive(Debug, Clone)]
pub struct GraphProgram {
    geometry: Arc<RenderGeometry>,
    camera: Camera,
    scheme: Scheme,
}

impl GraphProgram {
    /// A program that renders `geometry` through `camera`, painting with
    /// `scheme`'s semantic tokens.
    #[must_use]
    pub const fn new(geometry: Arc<RenderGeometry>, camera: Camera, scheme: Scheme) -> Self {
        Self {
            geometry,
            camera,
            scheme,
        }
    }

    /// The camera this program renders with.
    #[must_use]
    pub const fn camera(&self) -> Camera {
        self.camera
    }

    /// The color scheme this program paints with.
    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The geometry this program renders.
    #[must_use]
    pub fn geometry(&self) -> &Arc<RenderGeometry> {
        &self.geometry
    }

    /// Widget-local logical cursor → world coordinates.
    #[must_use]
    fn world_at(&self, bounds: &Rectangle, position: Point) -> [f32; 2] {
        self.camera.screen_to_world(
            [position.x - bounds.x, position.y - bounds.y],
            [0.0, 0.0],
            [bounds.width, bounds.height],
        )
    }

    /// Identity-safe hit under the logical cursor.
    #[must_use]
    fn hit_at(&self, bounds: &Rectangle, position: Point) -> Option<HitNode> {
        self.geometry
            .hit(self.world_at(bounds, position), self.camera.zoom)
    }

    /// Build the `iced` widget for this program, filling its container.
    #[must_use]
    pub fn widget(self) -> Shader<GraphMessage, Self> {
        Shader::new(self).width(Length::Fill).height(Length::Fill)
    }
}

impl ShaderProgram<GraphMessage> for GraphProgram {
    type State = GraphProgramState;
    type Primitive = GraphPrimitive;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<GraphMessage>> {
        match event {
            Event::Keyboard(iced::keyboard::Event::ModifiersChanged(modifiers)) => {
                state.shift = modifiers.shift();
                None
            }
            Event::Window(window::Event::Opened { size, .. })
            | Event::Window(window::Event::Resized(size)) => {
                Some(Action::publish(GraphMessage::Viewport {
                    size: [size.width, size.height],
                }))
            }
            Event::Window(window::Event::Rescaled(scale)) => {
                Some(Action::publish(GraphMessage::ScaleFactor { scale: *scale }))
            }

            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let mouse::Cursor::Available(position) = cursor {
                    if !bounds.contains(position) {
                        return None;
                    }
                    state.pressed = true;
                    state.selecting_box = state.shift;
                    state.box_first = state.shift.then(|| self.world_at(&bounds, position));
                    state.press_node = self.hit_at(&bounds, position);
                    state.drag_anchor = Some(position);
                    state.last_cursor = Some(position);
                    state.moved_pixels = 0.0;
                    Some(Action::capture())
                } else {
                    None
                }
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if !state.pressed {
                    return None;
                }
                state.pressed = false;
                if state.selecting_box {
                    state.selecting_box = false;
                    let first = state.box_first.take()?;
                    let position = cursor.position()?;
                    state.press_node = None;
                    state.drag_anchor = None;
                    state.last_cursor = None;
                    return Some(
                        Action::publish(GraphMessage::BoxSelected {
                            first,
                            second: self.world_at(&bounds, position),
                        })
                        .and_capture(),
                    );
                }

                let dragged_node = state
                    .press_node
                    .filter(|_| state.moved_pixels > DRAG_THRESHOLD_LOGICAL);
                let kind = match dragged_node {
                    Some(node) => ReleaseKind::NodeDragCompleted { node },
                    None if state.moved_pixels > DRAG_THRESHOLD_LOGICAL => {
                        ReleaseKind::PanCompleted
                    }
                    None => ReleaseKind::Click,
                };
                let (node, world) = match cursor {
                    mouse::Cursor::Available(position) | mouse::Cursor::Levitating(position) => (
                        self.hit_at(&bounds, position),
                        self.world_at(&bounds, position),
                    ),
                    _ => (None, [0.0; 2]),
                };
                let press_node = state.press_node;
                let dragging_a_node = matches!(kind, ReleaseKind::NodeDragCompleted { .. });
                // A click's identity must come from the *press* target when
                // the release drifted by a sub-threshold distance (§9.3:
                // two releases on the same EntityRef).
                let release_node = if dragging_a_node {
                    node.or(press_node)
                } else {
                    node
                };
                state.press_node = None;
                state.drag_anchor = None;
                state.last_cursor = None;
                state.moved_pixels = 0.0;
                Some(
                    Action::publish(GraphMessage::Released {
                        node: release_node,
                        world,
                        kind,
                    })
                    .and_capture(),
                )
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let previous = state.last_cursor.replace(*position);
                if !state.pressed {
                    return None;
                }
                if let Some(anchor) = state.drag_anchor {
                    let dx = position.x - anchor.x;
                    let dy = position.y - anchor.y;
                    state.moved_pixels = (dx * dx + dy * dy).sqrt().max(state.moved_pixels);
                    if state.moved_pixels == 0.0 {
                        return None;
                    }
                }
                if state.moved_pixels <= DRAG_THRESHOLD_LOGICAL {
                    return None;
                }
                if state.selecting_box {
                    return Some(
                        Action::publish(GraphMessage::BoxPreview {
                            first: state.box_first?,
                            second: self.world_at(&bounds, *position),
                        })
                        .and_capture(),
                    );
                }
                if let Some(node) = state.press_node {
                    // Node drag: spatial preview only, captured so the drag
                    // never reaches a parent container (§9.3).
                    let world = self.world_at(&bounds, *position);
                    return Some(
                        Action::publish(GraphMessage::NodeDragged { node, world }).and_capture(),
                    );
                }
                let delta = previous.map_or([0.0, 0.0], |previous| {
                    [position.x - previous.x, position.y - previous.y]
                });
                if delta == [0.0, 0.0] {
                    return None;
                }
                Some(
                    Action::publish(GraphMessage::PanByPixels {
                        dx: delta[0],
                        dy: delta[1],
                    })
                    .and_capture(),
                )
            }

            // Scroll zooms about the cursor (§9.3: wheel zoom is anchored
            // at the pointer).
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if !cursor.is_over(bounds) {
                    return None;
                }
                let anchor = match cursor {
                    mouse::Cursor::Available(position) | mouse::Cursor::Levitating(position) => {
                        [position.x - bounds.x, position.y - bounds.y]
                    }
                    _ => [bounds.width * 0.5, bounds.height * 0.5],
                };
                let lines = match delta {
                    mouse::ScrollDelta::Lines { x, y } => x + y,
                    mouse::ScrollDelta::Pixels { x, y } => (x + y) * 0.05,
                };
                Some(
                    Action::publish(GraphMessage::ZoomAt {
                        anchor,
                        factor: scroll_factor(lines),
                    })
                    .and_capture(),
                )
            }

            _ => None,
        }
    }

    fn draw(
        &self,
        state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        let mut primitive =
            GraphPrimitive::new(Arc::clone(&self.geometry), self.camera, self.scheme);
        primitive.resources = state.resources.clone();
        primitive
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.pressed {
            return mouse::Interaction::Grabbing;
        }
        match cursor {
            mouse::Cursor::Available(position) if bounds.contains(position) => {
                if self.hit_at(&bounds, position).is_some() {
                    mouse::Interaction::Pointer
                } else {
                    mouse::Interaction::Grab
                }
            }
            _ => mouse::Interaction::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::geometry::RenderGeometry;
    use crate::graph::instance::{NodeInstance, NodeShape, note_color};
    use crate::style::tokens::Scheme;
    use flavite_graph::NodeId;

    #[test]
    fn gpu_widget_viewport_matches_cpu_hits_at_nonzero_origins_and_all_scales() {
        let camera = Camera {
            center: [13.0, -21.0],
            zoom: 1.7,
        };
        let bounds = Rectangle {
            x: 592.0,
            y: 269.0,
            width: 588.0,
            height: 454.0,
        };
        for scale in [1.0, 1.25, 1.5, 2.0] {
            let uniform = widget_camera_uniform(camera, bounds.size(), scale, 3, 2, Scheme::Dark);
            for world in [[0.0, 0.0], [72.5, -30.0], [-15.0, 82.0]] {
                let cpu = camera.world_to_screen(
                    world,
                    [bounds.x, bounds.y],
                    [bounds.width, bounds.height],
                );
                for axis in 0..2 {
                    let screen = (world[axis] - uniform.center[axis]) * uniform.zoom
                        + uniform.bounds_position[axis]
                        + uniform.bounds_size[axis] * 0.5;
                    // shader NDC followed by iced's widget-local GPU viewport.
                    let ndc = screen / uniform.viewport_size[axis] * 2.0 - 1.0;
                    let origin = [bounds.x, bounds.y][axis] * scale;
                    let gpu = origin + (ndc + 1.0) * 0.5 * uniform.viewport_size[axis];
                    assert!((gpu / scale - cpu[axis]).abs() < 0.001);
                }
            }
        }
    }

    #[test]
    fn simultaneous_graph_views_have_independent_gpu_buffers_even_at_equal_generations() {
        let program = GraphProgram::new(tiny_geometry(), Camera::centered(), Scheme::Dark);
        let first = GraphProgramState::default();
        let second = GraphProgramState::default();
        let bounds = Rectangle::new(Point::ORIGIN, iced::Size::new(320.0, 240.0));
        let a = program.draw(&first, mouse::Cursor::Unavailable, bounds);
        let b = program.draw(&second, mouse::Cursor::Unavailable, bounds);
        let again = program.draw(&first, mouse::Cursor::Unavailable, bounds);
        assert!(!Arc::ptr_eq(&a.resources, &b.resources));
        assert!(Arc::ptr_eq(&a.resources, &again.resources));
    }

    fn tiny_geometry() -> Arc<RenderGeometry> {
        Arc::new(RenderGeometry::new(
            1,
            vec![
                NodeInstance {
                    center: [0.0, 0.0],
                    size: 4.0,
                    shape: NodeShape::Circle,
                    color: note_color(Scheme::Dark),
                },
                NodeInstance {
                    center: [10.0, 0.0],
                    size: 4.0,
                    shape: NodeShape::Square,
                    color: note_color(Scheme::Dark),
                },
            ],
            vec![],
            vec![
                flavite_graph::NodeId::note(1),
                flavite_graph::NodeId::attachment(2),
            ],
        ))
    }

    #[test]
    fn another_panes_pointer_input_cannot_select_pan_or_zoom_this_graph() {
        let program = GraphProgram::new(tiny_geometry(), Camera::centered(), Scheme::Dark);
        let bounds = Rectangle {
            x: 10.0,
            y: 50.0,
            width: 320.0,
            height: 240.0,
        };
        let cursor = mouse::Cursor::Available(Point::new(400.0, 100.0));
        let mut state = GraphProgramState::default();
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
            mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            },
        ] {
            assert!(
                program
                    .update(&mut state, &Event::Mouse(event), bounds, cursor)
                    .is_none()
            );
        }
        assert!(!state.pressed);
    }

    #[test]
    fn scroll_factor_is_always_positive_and_bounded() {
        assert_eq!(scroll_factor(0.0), 1.0);
        assert_eq!(scroll_factor(-1.0), 0.9);
        assert_eq!(scroll_factor(1.0), 1.1);
        assert_eq!(scroll_factor(-10_000.0), 0.2);
        assert_eq!(scroll_factor(10_000.0), 5.0);
        assert_eq!(scroll_factor(f32::NAN), 1.0);
        assert_eq!(scroll_factor(f32::INFINITY), 1.0);
    }

    #[test]
    fn pan_message_converts_through_the_zoom_and_inverts() {
        let message = GraphMessage::PanByPixels {
            dx: 40.0,
            dy: -20.0,
        };
        assert_eq!(message.pan_world_at(2.0), Some([-20.0, 10.0]));
        assert_eq!(message.pan_world_at(0.0), Some([-40.0, 20.0]));
        assert_eq!(
            GraphMessage::ZoomAt {
                anchor: [0.0, 0.0],
                factor: 2.0
            }
            .pan_world_at(1.0),
            None
        );
    }

    #[test]
    fn press_over_a_node_captures_a_drag_candidate_and_release_is_a_click() {
        let program = GraphProgram::new(tiny_geometry(), Camera::centered(), Scheme::Dark);
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut state = GraphProgramState::default();
        // Cursor over node 1 at world (0,0): screen center is (400,300).
        let _ = program.update(
            &mut state,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            bounds,
            mouse::Cursor::Available(Point { x: 400.0, y: 300.0 }),
        );
        assert_eq!(state.press_node.map(|node| node.id), Some(NodeId::note(1)));

        let action = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                bounds,
                mouse::Cursor::Available(Point { x: 400.0, y: 300.0 }),
            )
            .expect("release publishes");
        let (message, _, status) = action.into_inner();
        assert_eq!(
            message,
            Some(GraphMessage::Released {
                node: Some(HitNode::new(NodeId::note(1), flavite_graph::NodeKind::Note)),
                world: [0.0, 0.0],
                kind: ReleaseKind::Click,
            })
        );
        assert_eq!(status, iced::event::Status::Captured);
    }

    #[test]
    fn a_four_pixel_move_starts_a_node_drag_and_the_release_completes_it() {
        let program = GraphProgram::new(tiny_geometry(), Camera::centered(), Scheme::Dark);
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut state = GraphProgramState::default();
        let _ = program.update(
            &mut state,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            bounds,
            mouse::Cursor::Available(Point { x: 400.0, y: 300.0 }),
        );
        let action = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: Point { x: 408.0, y: 300.0 },
                }),
                bounds,
                mouse::Cursor::Available(Point { x: 408.0, y: 300.0 }),
            )
            .expect("drag publishes node preview");
        let (message, _, _) = action.into_inner();
        assert!(matches!(
            message,
            Some(GraphMessage::NodeDragged {
                node,
                world: [8.0, 0.0]
            }) if node.id == NodeId::note(1)
        ));
        let action = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                bounds,
                mouse::Cursor::Available(Point { x: 408.0, y: 300.0 }),
            )
            .expect("release completes the node drag");
        let (message, _, _) = action.into_inner();
        assert!(matches!(
            message,
            Some(GraphMessage::Released {
                kind: ReleaseKind::NodeDragCompleted { .. },
                ..
            })
        ));
    }

    #[test]
    fn background_release_reports_pan_completion_not_a_click() {
        let program = GraphProgram::new(tiny_geometry(), Camera::centered(), Scheme::Dark);
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut state = GraphProgramState::default();
        let _ = program.update(
            &mut state,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            bounds,
            mouse::Cursor::Available(Point { x: 50.0, y: 50.0 }),
        );
        assert!(state.press_node.is_none());
        let _ = program.update(
            &mut state,
            &Event::Mouse(mouse::Event::CursorMoved {
                position: Point { x: 60.0, y: 50.0 },
            }),
            bounds,
            mouse::Cursor::Available(Point { x: 60.0, y: 50.0 }),
        );
        let action = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                bounds,
                mouse::Cursor::Available(Point { x: 60.0, y: 50.0 }),
            )
            .expect("release publishes");
        let (message, _, _) = action.into_inner();
        assert!(matches!(
            message,
            Some(GraphMessage::Released {
                kind: ReleaseKind::PanCompleted,
                ..
            })
        ));
    }

    #[test]
    fn primitive_counts_follow_the_geometry_and_the_zoom_cap() {
        let primitive = GraphPrimitive::new(tiny_geometry(), Camera::centered(), Scheme::Dark);
        assert_eq!(primitive.node_count(), 2);
        assert_eq!(primitive.edge_count(), 0);
        assert_eq!(primitive.generation(), 1);
    }

    #[test]
    fn send_sync_proofs_for_the_render_handoff() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<GraphPrimitive>();
        assert_send_sync::<Arc<RenderGeometry>>();
        assert_send_sync::<GraphProgram>();
    }
}
