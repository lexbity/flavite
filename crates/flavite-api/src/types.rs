//! Core types for the public API (§4.5, D16).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// A request ID for tracking commands through the system.
pub type RequestId = u64;

/// The origin of a command invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Origin {
    /// Command came from the GUI.
    Gui,
    /// Command came from the CLI.
    Cli,
    /// Command came from a plugin (future).
    Plugin,
    /// Command came from an internal source.
    #[default]
    Internal,
}

impl Origin {
    /// Returns true if this origin is from a user-facing source.
    #[must_use]
    pub fn is_user_facing(&self) -> bool {
        matches!(self, Self::Gui | Self::Cli)
    }
}

/// A vault reference for the API surface.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApiVaultRef {
    /// The vault UUID.
    pub uuid: Uuid,
    /// The root path of the vault.
    pub root: PathBuf,
}

impl ApiVaultRef {
    /// Create a new API vault reference.
    #[must_use]
    pub fn new(uuid: Uuid, root: PathBuf) -> Self {
        Self { uuid, root }
    }
}

/// A buffer reference for the API surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ApiBufferRef {
    /// The buffer ID.
    pub buffer_id: u64,
    /// The buffer revision.
    pub revision: u64,
}

impl ApiBufferRef {
    /// Create a new API buffer reference.
    #[must_use]
    pub fn new(buffer_id: u64, revision: u64) -> Self {
        Self {
            buffer_id,
            revision,
        }
    }
}

/// Preconditions for command execution.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Preconditions;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_vault_ref_creation() {
        let uuid = Uuid::new_v4();
        let ref_ = ApiVaultRef::new(uuid, PathBuf::from("/test"));
        assert_eq!(ref_.uuid, uuid);
        assert_eq!(ref_.root, PathBuf::from("/test"));
    }

    #[test]
    fn origin_is_user_facing() {
        assert!(Origin::Gui.is_user_facing());
        assert!(Origin::Cli.is_user_facing());
        assert!(!Origin::Plugin.is_user_facing());
        assert!(!Origin::Internal.is_user_facing());
    }
}
