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
//! - [`spatial`] — [`SpatialGrid`], uniform-grid hit-testing over world
//!   coordinates.
//!
//! The layout worker (`layout`, `position`, `worker`) lands in B3.

pub mod delta;
pub mod model;
pub mod spatial;

#[cfg(test)]
mod proptests;

pub use delta::{Delta, EdgeDelta, EdgeKind, NodeDelta, NodeId, NodeKind};
pub use model::{ApplyStats, GraphModel, GraphSnapshot};
pub use spatial::{BASE_CELL_SIZE, SpatialGrid};
