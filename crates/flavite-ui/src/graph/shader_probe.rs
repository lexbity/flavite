//! The S2-1 probe: an instanced wgpu `shader` widget (PLAN-B §5, D15).
//!
//! This is the GPU half of the graph view. It is deliberately wired as the
//! production widget will be in B6 — same WGSL, same pipeline layout, same
//! "geometry is behind an `Arc`, uploads are gated on a generation, the frame
//! path takes no locks" shape — so that the probe's measured frame rate is a
//! genuine forecast rather than a throwaway demo:
//!
//! - [`PROBE_WGSL`] — one shader module, four entry points: instanced nodes
//!   (circle for notes, square for attachments) and instanced edges (screen
//!   -space constant-width segments). Parsed and validated headlessly in
//!   `tests/shader_validation.rs`, so a WGSL regression fails CI even on a
//!   machine with no GPU.
//! - [`ProbePipeline`] — the per-`Shader`-type GPU state that `iced` caches
//!   for us: pipelines, the camera uniform, and the two instance buffers.
//!   Instance buffers are (re)uploaded only when the geometry generation
//!   moves ([`instance::needs_upload`]); a steady-state frame is one
//!   [`Queue::write_buffer`] of 48 bytes plus two instanced draws.
//! - [`ProbePrimitive`] — the per-frame value handed back by
//!   [`Program::draw`]. It borrows the geometry through an `Arc` (D15: the
//!   render thread swaps a pointer and never waits on layout) and carries the
//!   pan/zoom [`Camera`] for that frame.
//! - [`ProbeProgram`] — the [`Program`] implementation: input handling and
//!   redraw sustainment. It never mutates its own camera; it publishes
//!   [`ProbeMessage`]s and lets the application own the single camera
//!   authority, which is rebuilt into the widget on every `view`.
//!
//! [`Queue::write_buffer`]: wgpu::Queue::write_buffer
//! [`Program::draw`]: shader::Program::draw

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use iced::mouse;
use iced::wgpu;
use iced::widget::shader::{
    Action, Pipeline as ShaderPipeline, Primitive as ShaderPrimitive, Program as ShaderProgram,
    Shader, Viewport,
};
use iced::{Event, Length, Point, Rectangle, window};

use crate::graph::instance::{
    self, CAMERA_UNIFORM_SIZE, Camera, CameraUniform, EDGE_INSTANCE_SIZE, NODE_INSTANCE_SIZE,
    ProbeGeometry,
};
#[allow(unused_imports)]
use flavite_graph::NodeId;

/// The probe's WGSL module: camera uniform, instanced nodes, instanced edges.
///
/// Everything the renderer needs is decided on the CPU in
/// [`crate::graph::instance`] and handed over through [`CameraUniform`] and
/// the instance buffers — this module only projects, rasterizes, and blends.
/// That is what makes the D15 tunables unit-testable without a GPU.
///
/// # Layout contract
///
/// The `Camera` struct here must match [`CameraUniform`] field-for-field and
/// offset-for-offset; `tests/shader_validation.rs` parses this string with
/// `naga` and asserts exactly that, so the two can never drift apart
/// silently.
pub const PROBE_WGSL: &str = r#"
struct Camera {
    // offsets are asserted against the Rust `CameraUniform` by
    // tests/shader_validation.rs
    center: vec2<f32>,           //  0
    zoom: f32,                   //  8  world -> logical widget px
    edge_alpha: f32,             // 12  per-zoom edge multiplier (D15)
    bounds_position: vec2<f32>,  // 16  widget origin, physical px
    bounds_size: vec2<f32>,      // 24  widget size, physical px
    viewport_size: vec2<f32>,    // 32  window size, physical px
    edge_instances: u32,         // 40  importance-capped draw count
    node_instances: u32,         // 44
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
// (The headless naga gate in `tests/shader_validation.rs` is what caught
// this; the attribute locations, which are what actually address the
// instance buffer, are unaffected.)
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
    // A zero-length segment (self-loop, or both ends culled to the same
    // pixel) produces a zero-area quad: it rasterizes to nothing rather than
    // dividing by zero.
    direction = select(direction / span, vec2<f32>(0.0, 0.0), span <= 0.0);

    let normal = vec2<f32>(-direction.y, direction.x) * (input.width * 0.5);
    let screen = mix(a, b, input.corner.x) + normal * input.corner.y;

    out.position = to_ndc(screen);
    out.alpha = input.alpha * camera.edge_alpha;
    return out;
}

const EDGE_COLOR: vec3<f32> = vec3<f32>(0.62, 0.66, 0.74);

@fragment
fn fs_edges(input: EdgeOutput) -> @location(0) vec4<f32> {
    return vec4<f32>(EDGE_COLOR, input.alpha);
}
"#;

/// The four entry points [`PROBE_WGSL`] must expose.
///
/// Named once here so the pipeline construction in [`ProbePipeline`] and the
/// headless validation in `tests/shader_validation.rs` cannot disagree about
/// what "the shader" is.
pub const NODE_VERTEX_ENTRY: &str = "vs_nodes";
/// See [`NODE_VERTEX_ENTRY`].
pub const NODE_FRAGMENT_ENTRY: &str = "fs_nodes";
/// See [`NODE_VERTEX_ENTRY`].
pub const EDGE_VERTEX_ENTRY: &str = "vs_edges";
/// See [`NODE_VERTEX_ENTRY`].
pub const EDGE_FRAGMENT_ENTRY: &str = "fs_edges";

/// The WGSL name of the uniform block bound at group 0, binding 0.
pub const CAMERA_UNIFORM_NAME: &str = "Camera";

/// The WGSL name of the vertex buffer slot holding the quad corners.
pub const CORNER_LOCATION: u32 = 0;

/// Convert a scroll-wheel movement into a multiplicative zoom factor.
///
/// Trackpads report sub-line deltas; mice report whole lines. Both land in
/// the same smooth range here, and the result is always strictly positive so
/// [`Camera::scaled_zoom`] can never be handed a factor that would collapse
/// the zoom.
#[must_use]
pub fn scroll_factor(lines: f32) -> f32 {
    if !lines.is_finite() {
        return 1.0;
    }
    (1.0 + lines * 0.1).clamp(0.2, 5.0)
}

/// Messages the widget publishes to its owner.
///
/// The widget holds no mutable camera of its own: `Program::update` takes
/// `&self`, so input is turned into a message and the application — which
/// owns the single camera authority — applies it and rebuilds the widget in
/// the next `view`. That is the shape B6 will use, and it keeps the "one
/// camera, many views" invariant honest from the first slice.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProbeMessage {
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
    /// A drag moved the cursor by this many *logical* widget-local pixels.
    ///
    /// Pixels, not world units: converting to world units needs the zoom,
    /// which the application owns.
    PanByPixels {
        /// Horizontal movement, logical pixels.
        dx: f32,
        /// Vertical movement, logical pixels.
        dy: f32,
    },
    /// Zoom by `factor` while pinning `anchor` (logical widget-local pixels).
    ZoomAt {
        /// Widget-local logical pixel under which to anchor the zoom.
        anchor: [f32; 2],
        /// Multiplicative zoom step; must be > 0.
        factor: f32,
    },
}

impl ProbeMessage {
    /// The world-space pan this message amounts to at `zoom`.
    ///
    /// Dragging the cursor right by `dx` logical pixels must drag the world
    /// *left*, hence the negation.
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

/// The internal widget state: which drag, if any, is in flight.
///
/// `Default` is required by [`ShaderProgram::State`]. It holds only input
/// bookkeeping — the geometry and camera live elsewhere on purpose.
#[derive(Debug, Default, Clone, PartialEq)]
#[allow(clippy::new_without_default)]
pub struct ProbeState {
    dragging: bool,
    drag_anchor: Option<Point>,
    last_cursor: Option<Point>,
}

impl ProbeState {
    /// Whether a drag is currently in progress.
    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }
}

/// Per-frame CPU cost of the widget, in nanoseconds.
///
/// [`Primitive::prepare`] and [`Primitive::draw`] take `&self`, so these are
/// the *only* mutation points available on the frame path. Atomics (not a
/// mutex) keep the "no locks on the frame path" rule literally true — the
/// increment itself is the whole cost.
///
/// [`Primitive::prepare`]: shader::Primitive::prepare
/// [`Primitive::draw`]: shader::Primitive::draw
#[derive(Debug, Default)]
pub struct ProbeTimings {
    prepare_ns: AtomicU64,
    draw_ns: AtomicU64,
    prepare_calls: AtomicU64,
    draw_calls: AtomicU64,
}

impl ProbeTimings {
    /// Record the duration of one [`Primitive::prepare`] call.
    ///
    /// [`Primitive::prepare`]: shader::Primitive::prepare
    pub fn record_prepare(&self, elapsed: Duration) {
        self.prepare_ns
            .fetch_add(elapsed.as_nanos() as u64, AtomicOrdering::Relaxed);
        self.prepare_calls.fetch_add(1, AtomicOrdering::Relaxed);
    }

    /// Record the duration of one [`Primitive::draw`] call.
    ///
    /// [`Primitive::draw`]: shader::Primitive::draw
    pub fn record_draw(&self, elapsed: Duration) {
        self.draw_ns
            .fetch_add(elapsed.as_nanos() as u64, AtomicOrdering::Relaxed);
        self.draw_calls.fetch_add(1, AtomicOrdering::Relaxed);
    }

    /// Read the accumulated totals.
    #[must_use]
    pub fn snapshot(&self) -> TimingsSnapshot {
        TimingsSnapshot {
            prepare_total: Duration::from_nanos(self.prepare_ns.load(AtomicOrdering::Relaxed)),
            draw_total: Duration::from_nanos(self.draw_ns.load(AtomicOrdering::Relaxed)),
            prepare_calls: self.prepare_calls.load(AtomicOrdering::Relaxed),
            draw_calls: self.draw_calls.load(AtomicOrdering::Relaxed),
        }
    }

    /// Zero every counter (the example does this between measured runs).
    pub fn reset(&self) {
        self.prepare_ns.store(0, AtomicOrdering::Relaxed);
        self.draw_ns.store(0, AtomicOrdering::Relaxed);
        self.prepare_calls.store(0, AtomicOrdering::Relaxed);
        self.draw_calls.store(0, AtomicOrdering::Relaxed);
    }
}

/// An immutable read of [`ProbeTimings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimingsSnapshot {
    /// Total time spent inside `prepare`, across every frame so far.
    pub prepare_total: Duration,
    /// Total time spent inside `draw`, across every frame so far.
    pub draw_total: Duration,
    /// Frames whose `prepare` ran.
    pub prepare_calls: u64,
    /// Frames whose `draw` ran.
    pub draw_calls: u64,
}

impl TimingsSnapshot {
    /// Widget CPU time for the whole run — the number that actually moves
    /// when the renderer gets faster or slower. Frame *interval* saturates at
    /// the display refresh rate, so it cannot show this.
    #[must_use]
    pub fn widget_total(&self) -> Duration {
        self.prepare_total + self.draw_total
    }

    /// Mean `prepare` cost per call, in microseconds (`0.0` if never called).
    #[must_use]
    pub fn mean_prepare_us(&self) -> f64 {
        Self::mean_us(self.prepare_total, self.prepare_calls)
    }

    /// Mean `draw` cost per call, in microseconds (`0.0` if never called).
    #[must_use]
    pub fn mean_draw_us(&self) -> f64 {
        Self::mean_us(self.draw_total, self.draw_calls)
    }

    /// Mean widget cost per frame, in microseconds.
    #[must_use]
    pub fn mean_widget_us(&self) -> f64 {
        self.mean_prepare_us() + self.mean_draw_us()
    }

    fn mean_us(total: Duration, calls: u64) -> f64 {
        if calls == 0 {
            return 0.0;
        }
        total.as_micros() as f64 / calls as f64
    }
}

/// The per-frame value [`ProbeProgram::draw`] produces.
///
/// Cheap to construct and clone-free to hand around: it holds the geometry
/// behind an `Arc`, so the render thread never blocks on the layout worker
/// that owns the model (D15).
#[derive(Debug, Clone)]
pub struct ProbePrimitive {
    geometry: Arc<ProbeGeometry>,
    camera: Camera,
    /// Node instances to draw this frame.
    node_count: u32,
    /// Edge instances to draw this frame, after the importance cap (D15).
    edge_count: u32,
    timings: Arc<ProbeTimings>,
}

impl ProbePrimitive {
    /// Build the primitive for `camera` over `geometry`.
    ///
    /// The edge count is the importance-capped prefix of the sorted edge
    /// array — the whole point of sorting on the CPU in
    /// [`crate::graph::instance`].
    #[must_use]
    pub fn new(geometry: Arc<ProbeGeometry>, camera: Camera, timings: Arc<ProbeTimings>) -> Self {
        let node_count = count_u32(geometry.node_count());
        let edge_count = count_u32(geometry.visible_edge_count(camera.zoom));
        Self {
            geometry,
            camera,
            node_count,
            edge_count,
            timings,
        }
    }

    /// The camera this frame renders with.
    #[must_use]
    pub fn camera(&self) -> Camera {
        self.camera
    }

    /// Node instances drawn.
    #[must_use]
    pub fn node_count(&self) -> u32 {
        self.node_count
    }

    /// Edge instances drawn (importance-capped).
    #[must_use]
    pub fn edge_count(&self) -> u32 {
        self.edge_count
    }

    /// The geometry generation backing this frame.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.geometry.generation()
    }
}

/// Cast a count to `u32` for a GPU draw call.
///
/// Saturating rather than panicking: a graph with more than 2³2 entities
/// cannot exist in memory (see the same argument in `flavite-graph`), but a
/// renderer must never take the whole application down on a bad cast.
fn count_u32(count: usize) -> u32 {
    u32::try_from(count).unwrap_or(u32::MAX)
}

/// The GPU state behind every [`ProbePrimitive`]: one per shader type,
/// created by `iced` on first use and cached for the window's lifetime.
///
/// Ownership note: `iced` gives us `&mut self` only in
/// [`ShaderPrimitive::prepare`] — that is the single place buffers may be
/// (re)allocated. [`ShaderPrimitive::draw`] receives `&self`, which is what
/// forces uploads and draws to be separated, and is why the upload is gated
/// on [`instance::needs_upload`] rather than happening every frame.
pub struct ProbePipeline {
    camera_buffer: wgpu::Buffer,
    camera_bind_group: wgpu::BindGroup,
    nodes: wgpu::RenderPipeline,
    edges: wgpu::RenderPipeline,
    node_corners: wgpu::Buffer,
    edge_corners: wgpu::Buffer,
    node_slot: InstanceSlot,
    edge_slot: InstanceSlot,
    uploaded_generation: Option<u64>,
}

/// A grow-only instance buffer.
///
/// Grown only when a new geometry generation needs more bytes than the
/// current buffer holds; otherwise the same `wgpu::Buffer` is rewritten in
/// place with [`wgpu::Queue::write_buffer`].
#[derive(Debug, Default)]
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

impl ShaderPipeline for ProbePipeline {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("flavite graph probe"),
            source: wgpu::ShaderSource::Wgsl(PROBE_WGSL.into()),
        });

        let camera_bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("flavite graph probe camera"),
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
            label: Some("flavite graph probe"),
            bind_group_layouts: &[&camera_bind_group_layout],
            push_constant_ranges: &[],
        });

        // Two shared quads, four vertices each, in triangle-strip order. The
        // node quad is the unit square; the edge quad is parameterized along
        // (0..1) and across (-1..1) the segment.
        let node_corners = quad_buffer(
            device,
            queue,
            "flavite graph probe node corners",
            [[-1.0, -1.0], [1.0, -1.0], [-1.0, 1.0], [1.0, 1.0]],
        );
        let edge_corners = quad_buffer(
            device,
            queue,
            "flavite graph probe edge corners",
            [[0.0, -1.0], [1.0, -1.0], [0.0, 1.0], [1.0, 1.0]],
        );

        // The corner layout is shared; only the per-instance layout differs.
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
            label: Some("flavite graph probe nodes"),
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
            label: Some("flavite graph probe edges"),
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

        let camera_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("flavite graph probe camera"),
            size: CAMERA_UNIFORM_SIZE as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let camera_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("flavite graph probe camera"),
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: camera_buffer.as_entire_binding(),
            }],
        });

        Self {
            camera_buffer,
            camera_bind_group,
            nodes,
            edges,
            node_corners,
            edge_corners,
            node_slot: InstanceSlot::default(),
            edge_slot: InstanceSlot::default(),
            uploaded_generation: None,
        }
    }
}

/// The per-vertex layout of the shared quad-corner buffer.
///
/// Separate from [`ProbePipeline::new`] because each `RenderPipelineDescriptor`
/// takes ownership of its `buffers` slice: the node pipeline and the edge
/// pipeline each need their own value, built from the same attribute.
fn corner_layout(attributes: &[wgpu::VertexAttribute]) -> wgpu::VertexBufferLayout<'_> {
    wgpu::VertexBufferLayout {
        array_stride: 8,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes,
    }
}

/// Create the shared four-vertex corner buffer for a triangle strip.
///
/// Written immediately through the queue so the buffer is valid from the
/// moment `Pipeline::new` returns — `iced` never calls back into us to
/// initialize it.
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

impl ShaderPrimitive for ProbePrimitive {
    type Pipeline = ProbePipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let started = Instant::now();

        // Camera: logical pixels in, physical pixels to the GPU.
        let scale = viewport.scale_factor();
        let physical = viewport.physical_size();
        let camera = CameraUniform {
            center: self.camera.center,
            zoom: self.camera.zoom * scale,
            edge_alpha: instance::edge_alpha_factor(self.camera.zoom),
            bounds_position: [bounds.x * scale, bounds.y * scale],
            bounds_size: [bounds.width * scale, bounds.height * scale],
            viewport_size: [physical.width as f32, physical.height as f32],
            edge_instances: self.edge_count,
            node_instances: self.node_count,
        };
        queue.write_buffer(&pipeline.camera_buffer, 0, &camera.to_bytes());

        // Instance buffers: only when the geometry generation moved, or when
        // the existing buffers are too small for the new arrays.
        let node_bytes = self.geometry.node_count() * NODE_INSTANCE_SIZE;
        let edge_bytes = self.geometry.edge_count() * EDGE_INSTANCE_SIZE;
        let capacity_ok = pipeline.node_slot.capacity_bytes >= node_bytes
            && pipeline.edge_slot.capacity_bytes >= edge_bytes;

        if instance::needs_upload(
            pipeline.uploaded_generation,
            self.geometry.generation(),
            capacity_ok,
        ) {
            if self.geometry.node_count() > 0 {
                let bytes: Vec<u8> = self
                    .geometry
                    .nodes()
                    .iter()
                    .flat_map(|node| node.to_bytes())
                    .collect();
                pipeline
                    .node_slot
                    .upload(device, queue, "flavite graph probe nodes", &bytes);
            }
            if self.geometry.edge_count() > 0 {
                let bytes: Vec<u8> = self
                    .geometry
                    .edges()
                    .iter()
                    .flat_map(|edge| edge.to_bytes())
                    .collect();
                pipeline
                    .edge_slot
                    .upload(device, queue, "flavite graph probe edges", &bytes);
            }
            pipeline.uploaded_generation = Some(self.geometry.generation());
        }

        self.timings.record_prepare(started.elapsed());
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let started = Instant::now();

        // A zero-instance draw is skipped entirely rather than issued with a
        // count of zero: wgpu would still require every declared vertex
        // buffer slot to be bound.
        if let Some(buffer) = &pipeline.node_slot.buffer
            && self.node_count > 0
        {
            render_pass.set_pipeline(&pipeline.nodes);
            render_pass.set_bind_group(0, &pipeline.camera_bind_group, &[]);
            render_pass.set_vertex_buffer(0, pipeline.node_corners.slice(..));
            render_pass.set_vertex_buffer(1, buffer.slice(..));
            render_pass.draw(0..4, 0..self.node_count);
        }

        if let Some(buffer) = &pipeline.edge_slot.buffer
            && self.edge_count > 0
        {
            render_pass.set_pipeline(&pipeline.edges);
            render_pass.set_bind_group(0, &pipeline.camera_bind_group, &[]);
            render_pass.set_vertex_buffer(0, pipeline.edge_corners.slice(..));
            render_pass.set_vertex_buffer(1, buffer.slice(..));
            render_pass.draw(0..4, 0..self.edge_count);
        }

        self.timings.record_draw(started.elapsed());

        // `true` = "I used the given render pass", so `iced` will not call
        // `render` with a fresh encoder.
        true
    }
}

/// The [`Shader`] program: input handling and redraw sustainment.
///
/// Cloned into the widget by the application's `view`, so the camera it
/// carries is always the application's authoritative one for that frame.
#[derive(Debug, Clone)]
pub struct ProbeProgram {
    geometry: Arc<ProbeGeometry>,
    camera: Camera,
    timings: Arc<ProbeTimings>,
}

impl ProbeProgram {
    /// A program that renders `geometry` through `camera`, reporting into
    /// `timings`.
    #[must_use]
    pub fn new(geometry: Arc<ProbeGeometry>, camera: Camera, timings: Arc<ProbeTimings>) -> Self {
        Self {
            geometry,
            camera,
            timings,
        }
    }

    /// The camera this program will render with.
    #[must_use]
    pub fn camera(&self) -> Camera {
        self.camera
    }

    /// The geometry this program renders.
    #[must_use]
    pub fn geometry(&self) -> &Arc<ProbeGeometry> {
        &self.geometry
    }

    /// The shared instrumentation this program reports into.
    #[must_use]
    pub fn timings(&self) -> &Arc<ProbeTimings> {
        &self.timings
    }
}

impl ShaderProgram<ProbeMessage> for ProbeProgram {
    type State = ProbeState;
    type Primitive = ProbePrimitive;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<ProbeMessage>> {
        match event {
            // Keep the display refreshing at the window's rate. `iced`
            // converts this into `RedrawRequest::NextFrame`, which wins
            // against any `Wait`, so returning it here sustains the loop
            // that the frame-time harness measures.
            Event::Window(window::Event::RedrawRequested(_)) => Some(Action::request_redraw()),

            // Window size / scale: the application owns the camera and the
            // layout it derives from, so it hears about it rather than us.
            Event::Window(window::Event::Opened { size, .. }) => {
                Some(Action::publish(ProbeMessage::Viewport {
                    size: [size.width, size.height],
                }))
            }
            Event::Window(window::Event::Resized(size)) => {
                Some(Action::publish(ProbeMessage::Viewport {
                    size: [size.width, size.height],
                }))
            }
            Event::Window(window::Event::Rescaled(scale)) => {
                Some(Action::publish(ProbeMessage::ScaleFactor { scale: *scale }))
            }

            // Left-drag pans. Capture it so a parent container cannot also
            // react to the drag.
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let mouse::Cursor::Available(position) = cursor {
                    state.dragging = true;
                    state.drag_anchor = Some(position);
                    state.last_cursor = Some(position);
                }
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.dragging = false;
                state.drag_anchor = None;
                state.last_cursor = None;
                Some(Action::capture())
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let previous = state.last_cursor.replace(*position);
                if !state.dragging {
                    return None;
                }
                let delta = match previous {
                    Some(previous) => [position.x - previous.x, position.y - previous.y],
                    None => [0.0, 0.0],
                };
                if delta == [0.0, 0.0] {
                    return None;
                }
                Some(
                    Action::publish(ProbeMessage::PanByPixels {
                        dx: delta[0],
                        dy: delta[1],
                    })
                    .and_capture(),
                )
            }

            // Scroll zooms about the cursor, which is what makes a zoom feel
            // anchored to the pointer instead of to the window centre.
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                // `Levitating` is "hovering over something" — same pointer,
                // so it anchors the same way. Anything else (no pointer at
                // all) anchors to the widget centre.
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
                    Action::publish(ProbeMessage::ZoomAt {
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
        _state: &Self::State,
        _cursor: mouse::Cursor,
        _bounds: Rectangle,
    ) -> Self::Primitive {
        ProbePrimitive::new(
            Arc::clone(&self.geometry),
            self.camera,
            Arc::clone(&self.timings),
        )
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if state.dragging {
            return mouse::Interaction::Grabbing;
        }
        match cursor {
            mouse::Cursor::Available(position) if bounds.contains(position) => {
                mouse::Interaction::Grab
            }
            _ => mouse::Interaction::default(),
        }
    }
}

impl ProbeProgram {
    /// Build the `iced` widget for this program, filling the available space.
    ///
    /// Kept as a method so the application's `view` reads as "the graph view,
    /// sized to its container" rather than as widget plumbing.
    #[must_use]
    pub fn widget(self) -> Shader<ProbeMessage, Self> {
        Shader::new(self).width(Length::Fill).height(Length::Fill)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::instance::{NOTE_COLOR, NodeInstance, NodeShape};

    fn tiny_geometry() -> Arc<ProbeGeometry> {
        Arc::new(ProbeGeometry::new(
            1,
            vec![
                NodeInstance {
                    center: [0.0, 0.0],
                    size: 4.0,
                    shape: NodeShape::Circle,
                    color: NOTE_COLOR,
                },
                NodeInstance {
                    center: [10.0, 0.0],
                    size: 4.0,
                    shape: NodeShape::Square,
                    color: NOTE_COLOR,
                },
            ],
            vec![
                crate::graph::instance::EdgeInstance {
                    from: [0.0, 0.0],
                    to: [10.0, 0.0],
                    alpha: 0.9,
                    width: 1.0,
                    pad: [0.0; 2],
                };
                10
            ],
            vec![NodeId::note(1), NodeId::note(2)],
        ))
    }

    #[test]
    fn scroll_factor_is_always_positive_and_bounded() {
        assert_eq!(scroll_factor(0.0), 1.0, "no movement, no zoom");
        assert_eq!(scroll_factor(-1.0), 0.9);
        assert_eq!(scroll_factor(1.0), 1.1);
        // Hard bounds: a single gesture can never collapse or explode zoom.
        assert_eq!(scroll_factor(-10_000.0), 0.2);
        assert_eq!(scroll_factor(10_000.0), 5.0);
        assert_eq!(scroll_factor(f32::NAN), 1.0, "junk input is a no-op");
        assert_eq!(
            scroll_factor(f32::INFINITY),
            1.0,
            "non-finite means \"no movement\", never \"extreme movement\""
        );
        assert_eq!(scroll_factor(f32::NEG_INFINITY), 1.0);
        // Strictly monotonic inside the bounds.
        let mut previous = 0.0;
        for lines in [-1.9f32, -1.0, 0.0, 1.0, 2.0] {
            let factor = scroll_factor(lines);
            assert!(factor > previous, "factor must increase with scroll");
            previous = factor;
        }
    }

    #[test]
    fn pan_message_converts_through_the_zoom_and_inverts() {
        let message = ProbeMessage::PanByPixels {
            dx: 40.0,
            dy: -20.0,
        };
        // Dragging right by 40 logical px at zoom 2 moves the view 20 world
        // units the other way.
        assert_eq!(message.pan_world_at(2.0), Some([-20.0, 10.0]));
        // Zoom in further, same drag covers fewer world units.
        assert_eq!(message.pan_world_at(4.0), Some([-10.0, 5.0]));
        // A degenerate zoom must not divide by zero.
        assert_eq!(message.pan_world_at(0.0), Some([-40.0, 20.0]));
        assert_eq!(message.pan_world_at(f32::NAN), Some([-40.0, 20.0]));

        // Only pan messages convert.
        assert_eq!(
            ProbeMessage::ZoomAt {
                anchor: [0.0, 0.0],
                factor: 2.0
            }
            .pan_world_at(1.0),
            None
        );
        assert_eq!(
            ProbeMessage::Viewport { size: [1.0, 2.0] }.pan_world_at(1.0),
            None
        );
        assert_eq!(
            ProbeMessage::ScaleFactor { scale: 2.0 }.pan_world_at(1.0),
            None
        );
    }

    #[test]
    fn timings_accumulate_and_snapshot() {
        let timings = ProbeTimings::default();
        assert_eq!(timings.snapshot(), TimingsSnapshot::default());

        timings.record_prepare(Duration::from_micros(100));
        timings.record_prepare(Duration::from_micros(300));
        timings.record_draw(Duration::from_micros(50));

        let snapshot = timings.snapshot();
        assert_eq!(snapshot.prepare_calls, 2);
        assert_eq!(snapshot.draw_calls, 1);
        assert_eq!(snapshot.prepare_total, Duration::from_micros(400));
        assert_eq!(snapshot.draw_total, Duration::from_micros(50));
        assert_eq!(snapshot.widget_total(), Duration::from_micros(450));
        assert_eq!(snapshot.mean_prepare_us(), 200.0);
        assert_eq!(snapshot.mean_draw_us(), 50.0);
        assert_eq!(snapshot.mean_widget_us(), 250.0);

        timings.reset();
        assert_eq!(timings.snapshot(), TimingsSnapshot::default());
    }

    #[test]
    fn timings_snapshot_of_a_never_used_counter_is_zero() {
        let snapshot = TimingsSnapshot::default();
        assert_eq!(snapshot.widget_total(), Duration::ZERO);
        assert_eq!(snapshot.mean_prepare_us(), 0.0);
        assert_eq!(snapshot.mean_draw_us(), 0.0);
        assert_eq!(snapshot.mean_widget_us(), 0.0);
    }

    #[test]
    fn primitive_is_send_sync_and_cheap_to_share() {
        // D15: the geometry is handed to the render thread behind an `Arc`,
        // so it must be shareable across threads.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProbePrimitive>();
        assert_send_sync::<ProbeGeometry>();
        assert_send_sync::<ProbeTimings>();
        assert_send_sync::<Arc<ProbeGeometry>>();
    }

    #[test]
    fn primitive_counts_follow_the_geometry_and_the_zoom_cap() {
        let geometry = tiny_geometry();
        let timings = Arc::new(ProbeTimings::default());

        let camera = Camera::default();
        let primitive = ProbePrimitive::new(Arc::clone(&geometry), camera, Arc::clone(&timings));
        assert_eq!(primitive.node_count(), 2, "one instance per node");
        assert_eq!(primitive.edge_count(), 10, "under the cap, so all edges");
        assert_eq!(primitive.generation(), 1);
        assert_eq!(primitive.camera(), camera);

        // Zoomed out past the cap floor: the cap shrinks to 25 000 at the
        // floor, still above 10 edges. A deliberately large graph proves the
        // prefix truncation actually bites.
        let large = Arc::new(ProbeGeometry::new(
            2,
            vec![],
            vec![
                crate::graph::instance::EdgeInstance {
                    from: [0.0; 2],
                    to: [1.0; 2],
                    alpha: 0.5,
                    width: 1.0,
                    pad: [0.0; 2],
                };
                300_000
            ],
            vec![],
        ));
        let zoomed_out = Camera {
            center: [0.0, 0.0],
            zoom: 0.25,
        };
        let primitive = ProbePrimitive::new(Arc::clone(&large), zoomed_out, Arc::clone(&timings));
        assert_eq!(
            primitive.edge_count(),
            count_u32(instance::EDGE_CAP_AT_REFERENCE_ZOOM / 4),
            "zoomed out, only the most important quarter of edges draw"
        );

        let zoomed_in = Camera {
            center: [0.0, 0.0],
            zoom: 1.0,
        };
        let primitive = ProbePrimitive::new(Arc::clone(&large), zoomed_in, Arc::clone(&timings));
        assert_eq!(
            primitive.edge_count(),
            count_u32(instance::EDGE_CAP_AT_REFERENCE_ZOOM)
        );

        // An empty geometry draws nothing.
        let empty = Arc::new(ProbeGeometry::new(3, vec![], vec![], vec![]));
        let primitive = ProbePrimitive::new(empty, Camera::default(), timings);
        assert_eq!(primitive.node_count(), 0);
        assert_eq!(primitive.edge_count(), 0);
    }

    #[test]
    fn program_state_starts_idle_and_tracks_a_drag() {
        let state = ProbeState::default();
        assert!(!state.is_dragging());

        let program = ProbeProgram::new(
            tiny_geometry(),
            Camera::default(),
            Arc::new(ProbeTimings::default()),
        );
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };

        let mut state = ProbeState::default();
        let pressed = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                bounds,
                mouse::Cursor::Available(Point { x: 100.0, y: 100.0 }),
            )
            .expect("pressing the primary button must be handled");
        let (press_message, _, press_status) = pressed.into_inner();
        assert_eq!(
            press_message, None,
            "pressing publishes nothing, it just claims the event"
        );
        assert_eq!(
            press_status,
            iced::event::Status::Captured,
            "the probe owns the drag from the moment the button goes down"
        );
        assert!(state.is_dragging());

        let moved = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: Point { x: 140.0, y: 90.0 },
                }),
                bounds,
                mouse::Cursor::Available(Point { x: 140.0, y: 90.0 }),
            )
            .expect("dragging must publish a pan");
        let (message, _, status) = moved.into_inner();
        assert_eq!(
            message,
            Some(ProbeMessage::PanByPixels {
                dx: 40.0,
                dy: -10.0
            })
        );
        assert_eq!(
            status,
            iced::event::Status::Captured,
            "a drag must not also pan a parent container"
        );

        let released = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                bounds,
                mouse::Cursor::Available(Point { x: 140.0, y: 90.0 }),
            )
            .expect("releasing must be handled");
        assert_eq!(released.into_inner().1, iced::window::RedrawRequest::Wait);
        assert!(!state.is_dragging());
    }

    #[test]
    fn moving_the_cursor_without_a_drag_is_ignored() {
        let program = ProbeProgram::new(
            tiny_geometry(),
            Camera::default(),
            Arc::new(ProbeTimings::default()),
        );
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut state = ProbeState::default();

        assert!(
            program
                .update(
                    &mut state,
                    &Event::Mouse(mouse::Event::CursorMoved {
                        position: Point { x: 10.0, y: 10.0 },
                    }),
                    bounds,
                    mouse::Cursor::Available(Point { x: 10.0, y: 10.0 }),
                )
                .is_none()
        );
        assert!(!state.is_dragging());
    }

    #[test]
    fn scroll_zooms_about_the_cursor_and_is_captured() {
        let program = ProbeProgram::new(
            tiny_geometry(),
            Camera::default(),
            Arc::new(ProbeTimings::default()),
        );
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut state = ProbeState::default();

        let action = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
                }),
                bounds,
                mouse::Cursor::Available(Point { x: 300.0, y: 200.0 }),
            )
            .expect("scrolling must publish a zoom");
        let (message, _, status) = action.into_inner();
        assert_eq!(
            message,
            Some(ProbeMessage::ZoomAt {
                anchor: [300.0, 200.0],
                factor: scroll_factor(1.0),
            })
        );
        assert_eq!(status, iced::event::Status::Captured);

        // With no cursor, the zoom anchors to the middle of the widget.
        let action = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 20.0, y: 0.0 },
                }),
                bounds,
                mouse::Cursor::Unavailable,
            )
            .expect("scrolling without a cursor must still zoom");
        assert_eq!(
            action.into_inner().0,
            Some(ProbeMessage::ZoomAt {
                anchor: [400.0, 300.0],
                factor: scroll_factor(1.0),
            })
        );
    }

    #[test]
    fn redraw_requested_sustains_the_frame_loop() {
        let program = ProbeProgram::new(
            tiny_geometry(),
            Camera::default(),
            Arc::new(ProbeTimings::default()),
        );
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let mut state = ProbeState::default();

        let action = program
            .update(
                &mut state,
                &Event::Window(window::Event::RedrawRequested(Instant::now())),
                bounds,
                mouse::Cursor::Unavailable,
            )
            .expect("a redraw request must be answered with another redraw");
        let (message, redraw_request, status) = action.into_inner();
        assert_eq!(message, None, "sustaining the loop publishes nothing");
        assert_eq!(
            redraw_request,
            iced::window::RedrawRequest::NextFrame,
            "`NextFrame` is the minimum variant, so it beats any `Wait`"
        );
        assert_eq!(status, iced::event::Status::Ignored);
    }

    #[test]
    fn window_size_and_scale_reach_the_owner() {
        let program = ProbeProgram::new(
            tiny_geometry(),
            Camera::default(),
            Arc::new(ProbeTimings::default()),
        );
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let mut state = ProbeState::default();

        for (event, expected) in [
            (
                Event::Window(window::Event::Opened {
                    position: None,
                    size: iced::Size::new(1024.0, 768.0),
                }),
                ProbeMessage::Viewport {
                    size: [1024.0, 768.0],
                },
            ),
            (
                Event::Window(window::Event::Resized(iced::Size::new(800.0, 600.0))),
                ProbeMessage::Viewport {
                    size: [800.0, 600.0],
                },
            ),
            (
                Event::Window(window::Event::Rescaled(2.0)),
                ProbeMessage::ScaleFactor { scale: 2.0 },
            ),
        ] {
            let action = program
                .update(&mut state, &event, bounds, mouse::Cursor::Unavailable)
                .expect("window changes must be forwarded");
            assert_eq!(action.into_inner().0, Some(expected));
        }
    }

    #[test]
    fn unrelated_events_are_left_alone() {
        let program = ProbeProgram::new(
            tiny_geometry(),
            Camera::default(),
            Arc::new(ProbeTimings::default()),
        );
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let mut state = ProbeState::default();

        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)),
            Event::Window(window::Event::Focused),
            Event::Window(window::Event::Moved(Point { x: 5.0, y: 5.0 })),
        ] {
            assert!(
                program
                    .update(&mut state, &event, bounds, mouse::Cursor::Unavailable)
                    .is_none(),
                "the probe must not claim events it does not handle"
            );
        }
        assert!(
            !state.is_dragging(),
            "none of those should have started a drag"
        );
    }

    #[test]
    fn mouse_interaction_reflects_drag_state() {
        let program = ProbeProgram::new(
            tiny_geometry(),
            Camera::default(),
            Arc::new(ProbeTimings::default()),
        );
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        };
        let mut state = ProbeState::default();

        assert_eq!(
            program.mouse_interaction(&state, bounds, mouse::Cursor::Unavailable),
            mouse::Interaction::default()
        );
        assert_eq!(
            program.mouse_interaction(
                &state,
                bounds,
                mouse::Cursor::Available(Point { x: 50.0, y: 50.0 })
            ),
            mouse::Interaction::Grab,
            "hovering the graph offers a grab"
        );
        assert_eq!(
            program.mouse_interaction(
                &state,
                bounds,
                mouse::Cursor::Available(Point { x: 500.0, y: 500.0 })
            ),
            mouse::Interaction::default(),
            "outside the bounds it does not"
        );

        state.dragging = true;
        assert_eq!(
            program.mouse_interaction(
                &state,
                bounds,
                mouse::Cursor::Available(Point { x: 50.0, y: 50.0 })
            ),
            mouse::Interaction::Grabbing
        );
    }

    #[test]
    fn program_exposes_its_dependencies() {
        let geometry = tiny_geometry();
        let camera = Camera {
            center: [3.0, 4.0],
            zoom: 2.0,
        };
        let timings = Arc::new(ProbeTimings::default());
        let program = ProbeProgram::new(Arc::clone(&geometry), camera, Arc::clone(&timings));

        assert_eq!(program.camera(), camera);
        assert_eq!(program.geometry().generation(), geometry.generation());
        assert!(
            Arc::ptr_eq(program.timings(), &timings),
            "the program shares the caller's instrumentation, not a fresh one"
        );
    }
}
