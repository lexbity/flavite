//! Tier 3 iced application shell. The current modules provide the app shell,
//! graph view, operations, palette, panes, settings, styling, and subscriptions.
//!
//! The graph view uses the wgpu shader widget in [`graph`]. The CPU-side
//! model, layout, and spatial index live in `flavite-graph` (§4.1).

pub mod app;
pub mod graph;
pub mod operation;
pub mod palette;
pub mod pane;
pub mod settings;
pub mod style;
pub mod subscription;
