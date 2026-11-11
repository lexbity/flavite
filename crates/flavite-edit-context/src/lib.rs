//! Context Mode's pure layer (FEUX-0002 E-2/E-3): the finite command
//! vocabulary, the physical-key grammar, the per-editor statechart, and the
//! implicit-effect planner.
//!
//! Everything here is a pure function of inputs. The host
//! (`flavite-ui/src/host`) classifies admission against live application
//! state, forwards events, and interprets the returned state/effect values;
//! the widget (`flavite-editor`) captures native events and converts them to
//! this crate's toolkit-free key identity at its boundary. There is exactly
//! one state machine and one grammar, and nothing here reaches the app.

pub mod effect;
pub mod grammar;
pub mod key;
pub mod session;
pub mod vocabulary;

pub use grammar::{command_key, menu_key, motion_key, selected_characters, word_bounds};
pub use key::{KeyCode, Modifiers, Motion};
pub use session::{ContextState, Effect, EscapeOutcome, Event, reduce};
pub use vocabulary::{ContextCommand, ContextMenu, ContextMode};
