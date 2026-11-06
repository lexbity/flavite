//! Vault selection for example/fixture launches (RFC-0003 TD-02).
//!
//! Production startup owns its own boot path (W13). The example shell
//! reads the canonical `FLAVITE_VAULT` setting.

use std::path::PathBuf;

/// The vault root named by the environment, if any.
#[must_use]
pub fn vault_root_from_env() -> Option<PathBuf> {
    if let Some(root) = std::env::var_os("FLAVITE_VAULT") {
        return Some(PathBuf::from(root));
    }

    None
}

/// The disposable default vault used when no environment root is set.
#[must_use]
pub fn default_vault_root() -> PathBuf {
    PathBuf::from("target/ui-visual/app-vault")
}
