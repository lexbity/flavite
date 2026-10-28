#![forbid(unsafe_code)]

//! Tier 2. `Lattice` facade owning the `Session` and command registry;
//! typed commands/results, revisioned read views, versioned event bus,
//! `Query` AST, capability declarations (§4.1, §4.5, D16).
//!
//! This crate provides the public API for interacting with Lattice vaults.
//! It is the single point through which all external consumers (GUI, CLI, future
//! plugins) interact with the core system.
//!
//! # Architecture
//!
//! The API consists of:
//! - **Lattice**: Main facade owning Session and command registry
//! - **Command System**: Typed command specification and dispatch
//! - **Event System**: Versioned events for plugin compatibility
//! - **Query AST**: Re-exported from flavite-index
//! - **Capabilities**: Declared capability system
//!
//! # Dogfooding (D16)
//!
//! `flavite-ui` MUST talk to the core only through this API. The CLI
//! is migrated to use this API as the dogfooding proof.

pub mod capability;
pub mod channel;
pub mod command;
pub mod event;
pub mod lattice;
pub mod result;
pub mod types;

// Re-export key types for convenience
pub use capability::*;
pub use channel::*;
pub use command::*;
pub use event::*;
pub use lattice::*;
pub use result::*;
pub use types::*;

// Re-export Query AST from flavite-index (D13)
pub use flavite_index::query::Query;
pub use flavite_index::types::{Cursor as IndexCursor, NoteRef as IndexNoteRef};

// Re-export types from vault for consumer convenience
pub use flavite_vault::types::{Epoch, Generation, ReconcileMode, ReconcileScope, Version};

/// API version for wire compatibility (D16).
pub const API_VERSION: &str = "0.1.0";

/// Maximum API version supported.
pub const API_VERSION_MAX: &str = "0.1.0";

/// Minimum API version supported.
pub const API_VERSION_MIN: &str = "0.1.0";
