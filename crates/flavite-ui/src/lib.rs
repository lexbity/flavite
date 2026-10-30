//! Tier 3 iced application shell for the Flavite workspace.
//!
//! The shell renders movable tab groups in a pane grid (D-02), the note
//! picker and command palette overlays, and hosts the graph view modules.
//! Layout truth lives in `flavite-workspace`; the adapter here projects it.
//! The graph view uses the wgpu shader widget in [`graph`]. The CPU-side
//! model, layout, and spatial index live in `flavite-graph` (§4.1).

pub mod accessibility;
pub mod app;
pub mod graph;
pub mod harness;
pub mod host;
pub mod operation;
pub mod palette;
pub mod settings;
pub mod style;
pub mod subscription;
pub mod workspace;
pub mod workspace_shell;
