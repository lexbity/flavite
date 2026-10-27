//! The graph view (PLAN-B): the instanced wgpu `shader` widget probe and its
//! CPU-side support modules, plus the fallback/card-layer for S2-3 (B4).
//!
//! Slice B2 (S2-1) activates this module with a *probe*; the production
//! widget lands in B6, the zoomable container and card layer in B7. What is
//! here is deliberately split so that everything which does not need a GPU is
//! plain CPU code with no `iced`/`wgpu` dependency at all:
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
//! - [`fallback`] — software-renderer fallback: canvas-based, node-capped,
//!   with a "reduced mode" notice (D15, §4.9; S2-3).
//! - [`zoomable`] — `Zoomable` container: world→screen transform, cached
//!   text layout at quantized scale steps (D15; S2-3).
//! - [`card_layer`] — `CardLayer`: real widgets at world positions, ≤ 64
//!   cards, checkbox toggles via [`CardEditSink`] seam (D15; S2-3).
//!
//! §4.1: `flavite-ui` is one of only two crates permitted to depend on
//! `iced`/`wgpu`; `cargo xtask arch check` enforces that `flavite-graph`
//! never gains them. The CPU model still lives there — this module only
//! *consumes* [`flavite_graph::GraphSnapshot`] (D15: the render thread swaps
//! an `Arc` and never waits on layout).

pub mod card_layer;
pub mod fallback;
pub mod instance;
pub mod shader_probe;
pub mod stats;
pub mod synthetic;
pub mod zoomable;

pub use card_layer::{
    CARD_ZOOM_THRESHOLD, CardEditSink, CardLayerHarness, CardLayerMessage, IndexedCardContent,
    LoggingSink, MAX_CARDS as CARD_LAYER_MAX_CARDS, NoopSink, card_layer_status,
    cards_from_snapshot, compute_card_layer_state,
};
pub use fallback::{
    FALLBACK_NODE_CAP, FallbackCamera, FallbackGeometry, FallbackGeometryError, FallbackPrimitive,
    reduced_mode_notice,
};
pub use instance::{
    ATTACHMENT_COLOR, Camera, CameraUniform, EDGE_ALPHA_MIN, EDGE_ALPHA_PER_OCTAVE,
    EDGE_ALPHA_SATURATE, EDGE_ALPHA_ZOOM_FLOOR, EDGE_CAP_AT_REFERENCE_ZOOM, EDGE_CAP_ZOOM_MAX,
    EDGE_CAP_ZOOM_MIN, EDGE_INSTANCE_SIZE, EDGE_WIDTH_BASE, EdgeInstance, GeometryError,
    NODE_INSTANCE_SIZE, NODE_RADIUS_BASE, NODE_RADIUS_PER_OCTAVE, NOTE_COLOR, NodeInstance,
    NodeShape, ProbeGeometry, edge_alpha_factor, edge_base_alpha, edge_cap, edge_importance,
    needs_upload, node_radius,
};
pub use shader_probe::{
    ProbeMessage, ProbePrimitive, ProbeProgram, ProbeState, ProbeTimings, TimingsSnapshot,
};
pub use stats::{FrameStats, StatsSnapshot, percentile, report_header};
pub use synthetic::{Rng, SyntheticGraph, SyntheticOptions, generate};
pub use zoomable::{
    CARD_CAP as ZOOMABLE_CARD_CAP, CARD_DISTANCE_THRESHOLD, Card, CardIdentity, CardLayout,
    LayoutCache, QuantizedScale, SCALE_MIN, SCALE_STEP_BASE, ZoomableCamera, ZoomableState,
    build_card, compute_zoomable_state,
};
