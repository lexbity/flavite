#![forbid(unsafe_code)]

//! Tier 2. Graph model (CSR adjacency), Barnes-Hut `ForceAtlas2` layout
//! worker, spatial grid for hit-testing. CPU only — no GPU dependencies
//! (§4.1, D15).
//!
//! - [`delta`] — the plain-data delta interface ([`NodeDelta`]/[`EdgeDelta`])
//!   the index feed produces (wired in B5).
//! - [`model`] — [`GraphModel`], the CPU-authoritative graph state with CSR
//!   adjacency and topology versioning; the GPU pipeline is recreated from
//!   its snapshots on device loss (§4.9).
//! - [`layout`] — [`Layout`], the Barnes-Hut ForceAtlas2 simulation: forces,
//!   adaptive speed, cooling, freeze on convergence.
//! - [`position`] — [`PositionBuffer`], the lock-free frame hand-off between
//!   the layout thread and the render thread (D15: no locks on the frame
//!   path).
//! - [`worker`] — [`LayoutWorker`], the layout's own thread: rayon pool at
//!   `cores − 1`, a command loop, `catch_unwind` panic isolation (§4.2), and
//!   freeze-on-cool / wake-on-topology-change (D15).
//! - [`spatial`] — [`SpatialGrid`], uniform-grid hit-testing over world
//!   coordinates.

pub mod delta;
pub mod feed;
pub mod layout;
pub mod model;
pub mod position;
pub mod spatial;
pub mod worker;

#[cfg(test)]
mod proptests;

pub use delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId, NodeKind};
pub use feed::{FeedBatch, FeedError, GraphFeed, IndexCursor};
pub use layout::{
    CONVERGENCE_STEPS, DEFAULT_THETA, Layout, LayoutParams, MAX_TREE_DEPTH, MIN_SEPARATION,
    POSITION_LIMIT, PROGRESS_MIN_IMPROVEMENT, PROGRESS_STALL_FRACTION, PROGRESS_STALL_WINDOW,
    RetargetStats, Step, StepStats,
};
pub use model::{ApplyStats, GraphModel, GraphSnapshot};
pub use position::{LayoutState, PositionBuffer, PositionFrame};
pub use spatial::{BASE_CELL_SIZE, SpatialGrid};
pub use worker::{
    LayoutCommand, LayoutEvent, LayoutTelemetry, LayoutWorker, LayoutWorkerConfig,
    LayoutWorkerError, TelemetrySnapshot,
};
