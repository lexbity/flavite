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
pub struct Preconditions {
    /// Optional buffer ID this command requires.
    pub buffer_id: Option<u64>,
    /// Optional expected revision.
    pub expected_revision: Option<u64>,
    /// Whether this command requires write access.
    pub requires_write: bool,
    /// Whether this command can run in read-only mode.
    pub allow_read_only: bool,
}

impl Preconditions {
    /// Create empty preconditions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set buffer ID requirement.
    #[must_use]
    pub fn with_buffer_id(mut self, buffer_id: u64) -> Self {
        self.buffer_id = Some(buffer_id);
        self
    }

    /// Set expected revision.
    #[must_use]
    pub fn with_revision(mut self, revision: u64) -> Self {
        self.expected_revision = Some(revision);
        self
    }

    /// Mark as requiring write access.
    #[must_use]
    pub fn requires_write(mut self) -> Self {
        self.requires_write = true;
        self
    }

    /// Allow in read-only mode.
    #[must_use]
    pub fn allow_read_only(mut self) -> Self {
        self.allow_read_only = true;
        self
    }
}

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
