//! `flavite-settings` — Pure, IO-free crate for settings descriptors, registry,
//! and the v2 TOML document codec.
//!
//! This crate is the authoritative source for:
//! - The `SettingRegistry` mechanism by which the backend expresses its settings
//! - The v2 namespaced TOML document codec with schema gating
//! - Unknown field preservation at document and section level
//! - The chord codec (parse/format/validation of keyboard chord strings)
//!
//! # Architecture
//!
//! This crate is Tier 2, pure and IO-free. It has zero internal crate dependencies
//! (only external `toml` and `thiserror`). Storage lives in `flavite-api` (`SettingsStore`
//! over `VaultUserStateStore` machinery), consumption in `flavite-ui` and `flavite-cli`.
//!
//! # Design
//!
//! The v2 settings document is a single TOML file with sections:
//!
//! ```toml
//! schema = 2
//!
//! [appearance]
//! theme = "dark"
//! theme_package = ""
//! scale_pct = 100
//! reduced_motion = false
//!
//! [editor]
//! font_size = 14
//! line_width = 80
//!
//! # ... other sections
//! ```
//!
//! See RFC-0007 for the complete specification.

#![forbid(unsafe_code)]

pub mod chord;
pub mod codec;
pub mod descriptor;
pub mod document;
pub mod error;
pub mod registry;
pub mod resolve;
pub mod value;

// Re-export key types for convenience
pub use chord::{chord_string, parse_chord, validate_shortcuts, Chord, ShortcutError};
pub use codec::{parse, serialize, MAX_DOCUMENT_SIZE};
pub use descriptor::{Apply, SCHEMA_VERSION, SettingDescriptor, SettingKind, Visibility};
pub use document::{Parsed, SettingsDoc};
pub use error::SettingsCodecError;
pub use registry::SettingRegistry;
pub use resolve::Resolved;
pub use value::SettingValue;
