//! Host-side composition of core and UI command surfaces (RFC-0003 D-16).
//!
//! One catalog exposes core registry descriptors plus UI-only workspace
//! commands; palette, keyboard, buttons, and accessibility actions all
//! resolve through the same availability logic. UI commands reduce the
//! workspace; core commands stay `Invocation`s through `flavite-api`.

pub mod appearance;
pub mod closing;
pub mod commands;
pub mod context;
pub mod dialogs;
pub mod documents;
pub mod encoding;
pub mod env;
pub mod graph;
pub mod linking;
pub mod navigation;
pub mod note_search;
pub mod occurrences;
pub mod open_intents;
pub mod persistence;
pub mod shell;
pub mod shutdown;

pub mod reading;

pub mod input;
