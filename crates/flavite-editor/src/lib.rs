//! Tier 3. Custom iced editor widget implementing `EditorSurface` over the
//! headless `EditorModel`. iced/winit/wgpu/cosmic-text are permitted in
//! this crate and `flavite-ui` only (§4.1, D14, ADR-0001).

#![forbid(unsafe_code)]

pub mod keymap;
pub mod surface;
pub mod viewport;
pub mod widget;

pub use surface::{EditorAction, EditorSurface, EditorView};
pub use widget::EditorWidget;
