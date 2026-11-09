//! The graph view module: production renderer, software fallback, and the
//! pure CPU layers around them.
//!
//! W07 (TD-10/TD-11) settled the ownership direction:
//!
//! - [`renderer`] — the production instanced wgpu core (WGSL, pipeline,
//!   primitive, typed gesture program). The probe consumes it, never the
//!   reverse.
//! - [`geometry`] — [`RenderGeometry`]: the
//!   identity-safe frame both backends draw and hit-test, built only from
//!   a base publication and positions frame that agree on `RenderKey`.
//! - [`instance`] — the plain-data instance buffers, camera, and D15
//!   render tunables (degree-based node sizing, edge alpha by zoom,
//!   importance-capped edges). Pure data and pure functions; unit-tested
//!   headless.
//! - [`canvas`] — the software backend: the same geometry, the same
//!   program logic, rasterized without a GPU.
//! - [`events`] — double-click/insert arbitration (§9.3) with an injected
//!   clock so the gesture boundary is testable.
//! - [`pins`] — in-memory pin store and lossless graph.json codec;
//!   persistence lives behind the API (TD-11).
//! - [`shader_view`] — the widget entry point the graph tab mounts.
//! - [`shader_probe`] — frame-cost instrumentation over the production
//!   core (examples/benchmarks only).
//! - [`stats`] — rolling frame-time percentiles for the probe.
//! - [`synthetic`] — deterministic fixture generator for probe examples;
//!   production never calls it.
//!
//! §4.1: `flavite-ui` is one of only two crates permitted to depend on
//! `iced`/`wgpu`; `cargo xtask arch check` enforces that `flavite-graph`
//! never gains them. The CPU model lives in `flavite-graph`; this module
//! consumes its published snapshots and positions (D15: the render thread
//! swaps an `Arc` and never waits on layout).

pub mod canvas;
pub mod events;
pub mod filters;
pub mod geometry;
pub mod instance;
pub mod nodes;
pub mod pins;
pub mod provenance;
pub mod renderer;
pub mod shader_probe;
pub mod shader_view;
pub mod stats;
pub mod synthetic;

pub use canvas::GraphCanvas;
pub use events::{
    DEFAULT_DOUBLE_CLICK_DISTANCE, DEFAULT_DOUBLE_CLICK_INTERVAL, GestureArbiter, GestureConfig,
    GestureOutcome,
};
pub use geometry::{
    GeometryError, GeometryOverrides, HitNode, RenderGeometry, SELECTED_NODE_COLOR,
    WRITING_TARGET_NODE_COLOR, keys_pairable,
};
pub use instance::{
    Camera, CameraUniform, EDGE_ALPHA_MIN, EDGE_ALPHA_PER_OCTAVE, EDGE_ALPHA_SATURATE,
    EDGE_ALPHA_ZOOM_FLOOR, EDGE_CAP_AT_REFERENCE_ZOOM, EDGE_CAP_ZOOM_MAX, EDGE_CAP_ZOOM_MIN,
    EDGE_INSTANCE_SIZE, EDGE_WIDTH_BASE, NODE_INSTANCE_SIZE, NODE_RADIUS_BASE,
    NODE_RADIUS_PER_OCTAVE, NodeInstance, NodeShape, attachment_color, edge_alpha_factor,
    edge_base_alpha, edge_cap, edge_importance, needs_upload, node_radius, note_color,
};
pub use nodes::{GraphNodeVisual, node_visuals};
pub use pins::{PinCommand, PinKey, PinRecord, PinStore, PinStoreError, carry_pin_for_tier};
pub use renderer::{
    GRAPH_WGSL, GraphMessage, GraphPipeline, GraphPrimitive, GraphProgram, GraphProgramState,
    ReleaseKind, scroll_factor,
};
pub use shader_probe::{
    ProbeMessage, ProbePrimitive, ProbeProgram, ProbeState, ProbeTimings, TimingsSnapshot,
};
pub use shader_view::{ShaderViewMessage, production_widget};
pub use stats::{FrameStats, StatsSnapshot, percentile, report_header};
pub use synthetic::{Rng, SyntheticGraph, SyntheticOptions, generate};
