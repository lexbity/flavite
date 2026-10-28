#![forbid(unsafe_code)]

//! Tier 2. `Flavite` facade owning the `Session` and command registry;
//! typed commands/results, revisioned read views, versioned event bus,
//! `Query` AST, capability declarations (§4.1, §4.5, D16).
//!
//! This crate provides the public API for interacting with Flavite vaults.
//! It is the single point through which all external consumers (GUI, CLI, future
//! plugins) interact with the core system.
//!
//! # Architecture
//!
//! The API consists of:
//! - **Flavite**: Main facade owning Session and command registry
//! - **Command System**: Typed command specification and dispatch
//! - **Event System**: Versioned events for plugin compatibility
//! - **Query AST**: Re-exported from flavite-index
//! - **Capabilities**: Declared capability system
//! - **Doctor**: Vault diagnostics
//!
//! # Dogfooding (D16)
//!
//! `flavite-ui` MUST talk to the core only through this API. The CLI
//! is migrated to use this API as the dogfooding proof.

pub mod capability;
pub mod channel;
pub mod command;
pub mod doctor;
pub mod event;
pub mod flavite;
pub mod open;
pub mod result;
pub mod types;

// Re-export key types for convenience
pub use capability::*;
pub use channel::*;
pub use command::*;
pub use doctor::*;
pub use event::*;
pub use flavite::*;
pub use result::*;
pub use types::*;

// Re-export Query AST from flavite-index (D13)
pub use flavite_index::query::Query;
pub use flavite_index::types::CompletionCandidate;
pub use flavite_index::types::{Cursor as IndexCursor, NoteRef as IndexNoteRef};

// Re-export types from vault for consumer convenience
pub use flavite_vault::open::{Vault, vault_exists};
pub use flavite_vault::types::{
    Epoch, Generation, ReconcileMode, ReconcileScope, VaultConfig, VaultIdentity, Version,
};

// Re-export Fs trait for CLI convenience
pub use flavite_fs::OsFs;

/// API version for wire compatibility (D16).
pub const API_VERSION: &str = "0.1.0";

/// Maximum API version supported.
pub const API_VERSION_MAX: &str = "0.1.0";

/// Minimum API version supported.
pub const API_VERSION_MIN: &str = "0.1.0";
