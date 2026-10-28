//! The graph view: an instanced wgpu shader widget and CPU-side helpers.
//!
//! The data preparation that does not need a GPU remains plain CPU code:
//!
//! - [`instance`] — instance-buffer construction plus the D15 render
//!   tunables (degree-based node sizing, edge alpha by zoom, edge count
//!   capped by importance). Pure data and pure functions; unit-tested
//!   headless.
//! - [`stats`] — rolling frame-time percentiles (p50/p95/p99).
//! - [`synthetic`] — deterministic, hub-heavy fixture generator for the
//!   probe (Barabási–Albert preferential attachment).
//! - [`shader_probe`] — the `shader` widget itself: WGSL, the wgpu
//!   pipelines, the pan/zoom camera, and per-frame CPU instrumentation.
//!
//! §4.1: `flavite-ui` is one of only two crates permitted to depend on
//! `iced`/`wgpu`; `cargo xtask arch check` enforces that `flavite-graph`
//! never gains them. The CPU model still lives there — this module only
//! *consumes* [`flavite_graph::GraphSnapshot`] (D15: the render thread swaps
//! an `Arc` and never waits on layout).

pub mod instance;
pub mod nodes;
pub mod pins;
pub mod shader_probe;
pub mod shader_view;
pub mod stats;
pub mod synthetic;
pub use instance::{
    ATTACHMENT_COLOR, Camera, CameraUniform, EDGE_ALPHA_MIN, EDGE_ALPHA_PER_OCTAVE,
    EDGE_ALPHA_SATURATE, EDGE_ALPHA_ZOOM_FLOOR, EDGE_CAP_AT_REFERENCE_ZOOM, EDGE_CAP_ZOOM_MAX,
    EDGE_CAP_ZOOM_MIN, EDGE_INSTANCE_SIZE, EDGE_WIDTH_BASE, EdgeInstance, GeometryError,
    NODE_INSTANCE_SIZE, NODE_RADIUS_BASE, NODE_RADIUS_PER_OCTAVE, NOTE_COLOR, NodeInstance,
    NodeShape, ProbeGeometry, edge_alpha_factor, edge_base_alpha, edge_cap, edge_importance,
    needs_upload, node_radius,
};
pub use nodes::{GraphNodeVisual, node_visuals};
pub use pins::{PinCommand, PinKey, PinRecord, PinStore, PinStoreError, carry_pin_for_tier};
pub use shader_probe::{
    ProbeMessage, ProbePrimitive, ProbeProgram, ProbeState, ProbeTimings, TimingsSnapshot,
};
pub use shader_view::{HitNode, LOD_ZOOM_THRESHOLD, ShaderViewMessage, ShaderViewProgram};
pub use stats::{FrameStats, StatsSnapshot, percentile, report_header};
pub use synthetic::{Rng, SyntheticGraph, SyntheticOptions, generate};
