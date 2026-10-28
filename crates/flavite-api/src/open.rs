//! Vault open re-exports (D16 dogfooding).
//!
//! This module re-exports vault opening functionality from flavite-vault
//! for the CLI to use through flavite-api.

/// Identity manager used by `init` to mint vault UUIDs and registry records.
pub use flavite_vault::IdentityManager;
pub use flavite_vault::open::{Vault, vault_exists};
