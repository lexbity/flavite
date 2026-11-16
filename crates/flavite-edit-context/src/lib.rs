//! Context Mode's pure layer (FEUX-0002 E-2/E-3, FEUX-0003 X01-X02): the finite command
//! vocabulary, the data-driven keymap, the per-editor statechart, and the
//! implicit-effect planner.
//!
//! Everything here is a pure function of inputs. The host
//! (`flavite-ui/src/host`) classifies admission against live application
//! state, forwards events, and interprets the returned state/effect values;
//! the widget (`flavite-editor`) captures native events and converts them to
//! this crate's toolkit-free key identity at its boundary. There is exactly
//! one state machine and one keymap, and nothing here reaches the app.

pub mod effect;
pub mod key;
pub mod keymap;
pub mod labels;
pub mod session;
pub mod text;
pub mod vocabulary;

pub use key::{KeyCode, Modifiers, Motion};
pub use keymap::{BindingId, BindingToken, ContextKeymap, KeymapError, Slot};
pub use labels::Layout;
pub use session::{
    AwaitArgs, BURST_MIN_ACTIVATIONS, BURST_MIN_MUTATING, BURST_SUPPRESS_MS,
    BURST_WINDOW_MS, ContextState, Effect, EscapeOutcome, Event, PendingCommand, reduce,
};
pub use text::{clamp_grapheme_boundary, selected_characters, word_bounds};
pub use vocabulary::{ContextCommand, ContextMenu, ContextMode};
