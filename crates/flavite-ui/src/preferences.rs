//! Interface preferences (W11): re-export of the backend codec.
//!
//! The schema, bounds validation, chord parsing, and shortcut rules live
//! in `flavite-api::device_preferences` beside the user-state store they
//! travel through — the shell is a consumer, not a second authority.

pub use flavite_api::device_preferences::*;
