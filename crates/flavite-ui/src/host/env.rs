//! Vault selection for example/fixture launches (RFC-0003 TD-02).
//!
//! Production startup owns its own boot path (W13). The example shell
//! reads `FLAVITE_VAULT`; the legacy `LATTICE_VAULT` name stays supported
//! for two releases with a redacted deprecation notice.

use std::path::PathBuf;

/// The vault root named by the environment, if any.
#[must_use]
pub fn vault_root_from_env() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("FLAVITE_VAULT") {
        return Some(PathBuf::from(root));
    }
    if let Some(root) = std::env::var_os("LATTICE_VAULT") {
        // Environment variables are host-local configuration, never vault
        // content, so echoing the variable name is safe; the value is not.
        eprintln!(
            "LATTICE_VAULT is deprecated; use FLAVITE_VAULT instead (two-release compatibility)"
        );
        return Some(PathBuf::from(root));
    }
    None
}

/// The disposable default vault used when no environment root is set.
#[must_use]
pub fn default_vault_root() -> PathBuf {
    PathBuf::from("target/ui-visual/app-vault")
}
