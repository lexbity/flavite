#![forbid(unsafe_code)]

//! Tier 2. `Flavite` facade owning the `Session` and command registry;
//! typed commands/results, revisioned read views, event types,
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
//! - **Event Types**: Versioned event records
//! - **Query AST**: Re-exported from flavite-index
//! - **Capabilities**: Declared capability system
//! - **Doctor**: Vault diagnostics
//!
//! # Dogfooding (D16)
//!
//! `flavite-ui` MUST talk to the core only through this API. The CLI
//! is migrated to use this API as the dogfooding proof.

pub mod capability;
pub mod command;
pub mod doctor;
pub mod documents;
pub mod event;
pub mod flavite;
pub mod graph_mirror;
pub mod graph_projection;
pub mod graph_service;
pub mod note_encoding;
pub mod recovery;
pub mod text_edit;
pub use graph_projection::{
    GraphProjection, GraphViewToken, LinkOccurrence, OccurrenceSet, OccurrenceSource, OverlayState,
    ScopeRequest,
};
// RFC-0006 §6.10: the facade re-exports the sole source-format descriptor
// so hosts/DTO consumers keep one spelling without a direct codec edge.
pub use flavite_encoding::SourceFormat;
// XPI themes: the facade re-exports the pure theme-package codec so hosts
// (the CLI converter especially) keep one spelling without a codec edge.
pub use flavite_theme;
// Buffer identity types used by facade view APIs.
pub use flavite_buffer::revision::{BufferId, Revision};
pub mod attachments;
pub mod links;
pub mod open;
pub use attachments::*;
pub mod reading;
pub use reading::*;
pub mod properties;
pub use properties::*;
pub mod queries;
pub mod query_service;
pub mod replace;
pub mod result;
pub mod search_sessions;
pub mod themes;
pub mod types;
pub mod user_state;
pub use user_state::{
    StoredUserState, UserStateError, UserStateKind, read_last_vault_device, write_last_vault_device,
};
pub mod settings_store;
pub use settings_store::{defaults_document, ResolvedSettings, SettingsStore, SettingsStoreError, StoredSettings, MAX_SETTINGS_BYTES};
pub mod settings_cmds;
pub use settings_cmds::*;

// Re-export key types for convenience
pub use capability::*;
pub use command::*;
pub use doctor::*;
pub use event::*;
pub use flavite::*;
pub use graph_service::{
    GRAPH_BUFFER_DEBOUNCE, GRAPH_SWEEP_INTERVAL, GraphFrame, GraphService, GraphServiceMetrics,
    GraphServiceStatus,
};
pub use queries::*;
pub use query_service::*;
pub use result::*;
pub use themes::{InstalledTheme, LoadedTheme, ThemeStore, ThemeStoreError};
pub use types::*;

// Re-export Query AST from flavite-index (D13)
pub use flavite_index::query::Query;

// Re-export types from vault for consumer convenience
pub use flavite_vault::open::{Vault, vault_exists};
pub use flavite_vault::types::{
    Epoch, Generation, ReconcileMode, ReconcileScope, VaultConfig, VaultIdentity, Version,
};

// Re-export Fs trait for CLI convenience
pub use flavite_fs::OsFs;

/// API version for wire compatibility (D16).
pub const API_VERSION: &str = "0.2.0";

/// Maximum API version supported.
pub const API_VERSION_MAX: &str = "0.2.0";

/// Minimum API version supported.
pub const API_VERSION_MIN: &str = "0.1.0";

pub use flavite_vault::link::LinkTargetIdentity;

pub use flavite_vault::link::CompletionRequest;

pub mod catalog_service;
pub use catalog_service::CompletionCandidate;
