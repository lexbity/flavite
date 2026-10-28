//! Versioned events (§4.5, D16).
//!
//! Wire-level event enum for plugin compatibility.

use crate::types::ApiVaultRef;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// API version for events.
pub const EVENT_API_VERSION: &str = "0.1.0";

/// A versioned event.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum Event {
    /// Vault changed.
    VaultChanged {
        vault: ApiVaultRef,
        epoch: u64,
        generation: u64,
        added: u32,
        changed: u32,
        removed: u32,
    },
    /// Reconcile progress (lossy pulse; consumers should re-query on completion).
    ReconcileProgress { done: u32, total: u32 },
    /// A specific note's indexed view is stale.
    Invalidate {
        vault: ApiVaultRef,
        note_path: String,
    },
    /// A note changed outside the current Session.
    ExternalChangeDetected {
        vault: ApiVaultRef,
        note_path: String,
        deleted: bool,
    },
    /// Buffer state changed.
    BufferStateChanged {
        vault: ApiVaultRef,
        buffer_id: u64,
        revision: u64,
        state: String,
    },
    /// Rename suggested.
    RenameSuggested {
        vault: ApiVaultRef,
        from: PathBuf,
        to: PathBuf,
        tier: String,
        inbound: u32,
    },
    /// Conflict detected.
    Conflict {
        vault: ApiVaultRef,
        note_path: String,
    },
    /// Recovery offer.
    RecoveryOffer {
        vault: ApiVaultRef,
        note_path: String,
    },
    /// Action required.
    ActionRequired {
        vault: ApiVaultRef,
        code: String,
        note_path: Option<String>,
    },
}

impl Event {
    /// Get the event type.
    #[must_use]
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::VaultChanged { .. } => "vault_changed",
            Self::ReconcileProgress { .. } => "reconcile_progress",
            Self::Invalidate { .. } => "invalidate",
            Self::ExternalChangeDetected { .. } => "external_change_detected",
            Self::BufferStateChanged { .. } => "buffer_state_changed",
            Self::RenameSuggested { .. } => "rename_suggested",
            Self::Conflict { .. } => "conflict",
            Self::RecoveryOffer { .. } => "recovery_offer",
            Self::ActionRequired { .. } => "action_required",
        }
    }

    /// Returns true if this is a notice-level event.
    #[must_use]
    pub fn is_notice(&self) -> bool {
        matches!(
            self,
            Self::RenameSuggested { .. }
                | Self::Conflict { .. }
                | Self::RecoveryOffer { .. }
                | Self::ActionRequired { .. }
        )
    }

    /// Returns true if this is a pulse-level event.
    #[must_use]
    pub fn is_pulse(&self) -> bool {
        !self.is_notice()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn event_types() {
        let vault = ApiVaultRef::new(Uuid::new_v4(), PathBuf::from("/test"));

        let event = Event::VaultChanged {
            vault: vault.clone(),
            epoch: 1,
            generation: 2,
            added: 0,
            changed: 0,
            removed: 0,
        };
        assert_eq!(event.event_type(), "vault_changed");
        assert!(event.is_pulse());
        assert!(!event.is_notice());

        let notice = Event::Conflict {
            vault,
            note_path: "test.md".to_string(),
        };
        assert_eq!(notice.event_type(), "conflict");
        assert!(notice.is_notice());
        assert!(!notice.is_pulse());
    }
}
