//! Toolkit adaptation for the workspace model (RFC-0003 D-02).
//!
//! The adapter owns the disposable iced `pane_grid` projection of the
//! authoritative [`WorkspaceState`]. It never mutates layout truth in
//! response to widget callbacks: widget events become host messages, the
//! reducer commits, and [`Adapter::reconcile`] rebuilds the projection.

pub mod gestures;
pub mod pane_grid;
pub mod tab_strip;

pub use pane_grid::{Adapter, GroupPane};
pub use tab_strip::StripMenu;
