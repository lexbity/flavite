//! Journal recovery implementation (§4.15).

use crate::journal::manifest::TransactionState;
use thiserror::Error;

/// Journal recovery engine.
pub struct JournalRecovery;

/// Result of journal recovery.
#[derive(Debug, Clone)]
pub enum JournalRecoveryResult {
    /// No journals to recover.
    None,
    /// Recovery completed successfully.
    Success { recovered_transactions: Vec<String> },
    /// Recovery encountered errors.
    Error(JournalRecoveryError),
}

/// Error during journal recovery.
#[derive(Debug, Clone, Error)]
pub enum JournalRecoveryError {
    #[error("journal filesystem error: {0}")]
    FsError(String),
    #[error("manifest is corrupt or has unsupported version")]
    CorruptManifest,
}

/// Outcome of classifying a journal manifest.
#[derive(Debug, Clone, PartialEq)]
pub enum JournalRecoveryOutcome {
    /// Transaction is preparing - no truth mutation, can discard.
    Preparing,
    /// Transaction is committed - needs to be applied.
    Committed,
    /// Transaction is partially applied.
    Partial,
    /// Transaction is complete.
    Complete,
    /// Transaction was abandoned.
    Abandoned,
}

impl JournalRecoveryOutcome {
    /// Convert from transaction state.
    pub fn from_state(state: TransactionState) -> Self {
        match state {
            TransactionState::Preparing => Self::Preparing,
            TransactionState::Committed => Self::Committed,
            TransactionState::Partial => Self::Partial,
            TransactionState::Complete => Self::Complete,
            TransactionState::Abandoned => Self::Abandoned,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outcome_from_state() {
        assert_eq!(
            JournalRecoveryOutcome::from_state(TransactionState::Preparing),
            JournalRecoveryOutcome::Preparing
        );
        assert_eq!(
            JournalRecoveryOutcome::from_state(TransactionState::Committed),
            JournalRecoveryOutcome::Committed
        );
    }
}
