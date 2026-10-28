//! Vault open re-exports (D16 dogfooding).
//!
//! This module re-exports vault opening functionality from flavite-vault
//! for the CLI to use through flavite-api.

pub use flavite_vault::open::{Vault, vault_exists};
