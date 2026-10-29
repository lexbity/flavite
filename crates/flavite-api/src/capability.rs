//! Capability declarations (D16).
//!
//! Dispatch checks each invocation's grants against its registered capability.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Scope granted to a note-writing consumer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WriteScope {
    /// Any note in the active vault.
    Vault,
    /// Notes at or below this vault-relative path.
    PathPrefix(String),
}

/// A capability that can be declared.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    /// Read notes.
    ReadNotes,
    /// Write notes with scope.
    WriteNotes { scope: WriteScope },
    /// Create UI panels.
    UiPanel,
    /// Make network requests.
    Net,
}

impl Capability {
    /// Get description.
    #[must_use]
    pub fn description(&self) -> &'static str {
        match self {
            Self::ReadNotes => "Read notes",
            Self::WriteNotes { .. } => "Write notes",
            Self::UiPanel => "Create UI panels",
            Self::Net => "Make network requests",
        }
    }

    /// Whether this granted capability covers the required capability.
    #[must_use]
    pub fn covers(&self, required: &Self, target: Option<&Path>) -> bool {
        match (self, required) {
            (Self::ReadNotes, Self::ReadNotes)
            | (Self::UiPanel, Self::UiPanel)
            | (Self::Net, Self::Net) => true,
            (Self::WriteNotes { scope: granted }, Self::WriteNotes { scope: needed }) => {
                match (granted, needed) {
                    (WriteScope::Vault, _) => true,
                    (WriteScope::PathPrefix(a), WriteScope::PathPrefix(b)) => {
                        Path::new(b).starts_with(a)
                    }
                    (WriteScope::PathPrefix(prefix), WriteScope::Vault) => {
                        target.is_some_and(|path| path.starts_with(prefix))
                    }
                }
            }
            _ => false,
        }
    }
}

/// Capability declarations for built-in consumers.
pub mod declarations {
    use super::*;

    /// Backlinks panel capabilities.
    pub const BACKLINKS: &[Capability] = &[Capability::ReadNotes];

    /// Graph view capabilities.
    pub const GRAPH: &[Capability] = &[Capability::ReadNotes];

    /// Editor capabilities.
    pub const EDITOR: &[Capability] = &[
        Capability::ReadNotes,
        Capability::WriteNotes {
            scope: WriteScope::Vault,
        },
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_descriptions() {
        assert_eq!(Capability::ReadNotes.description(), "Read notes");
        assert_eq!(
            Capability::WriteNotes {
                scope: WriteScope::Vault
            }
            .description(),
            "Write notes"
        );
    }
}
