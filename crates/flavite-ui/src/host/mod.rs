//! Host-side composition of core and UI command surfaces (RFC-0003 D-16).
//!
//! One catalog exposes core registry descriptors plus UI-only workspace
//! commands; palette, keyboard, buttons, and accessibility actions all
//! resolve through the same availability logic. UI commands reduce the
//! workspace; core commands stay `Invocation`s through `flavite-api`.

pub mod commands;
pub mod effects;
pub mod env;
