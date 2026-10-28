//! Tier 3. iced application shell: panes, palette, quick switcher,
//! backlinks, search panel, diagnostics, conflict banners, read mode,
//! settings. Hosts `Lattice` through `flavite-api` (D16, §4.1, §4.5).
//!
//! This crate owns the graph *view* (PLAN-B): the wgpu `shader` widget, the
//! `Zoomable` container, and the `CardLayer` live in [`graph`]. The CPU-side
//! model, layout, and spatial index live in `flavite-graph` (§4.1).

pub mod app;
pub mod graph;
pub mod operation;
pub mod palette;
pub mod pane;
pub mod settings;
pub mod style;
pub mod subscription;
