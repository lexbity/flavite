//! Capability declarations (D16).
//!
//! Capabilities are declared now even though nothing enforces them yet.

use serde::{Deserialize, Serialize};

/// A capability that can be declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    /// Read notes.
    ReadNotes,
    /// Write notes with scope.
    WriteNotes,
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
            Self::WriteNotes => "Write notes",
            Self::UiPanel => "Create UI panels",
            Self::Net => "Make network requests",
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
    pub const EDITOR: &[Capability] = &[Capability::ReadNotes, Capability::WriteNotes];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_descriptions() {
        assert_eq!(Capability::ReadNotes.description(), "Read notes");
        assert_eq!(Capability::WriteNotes.description(), "Write notes");
    }
}
