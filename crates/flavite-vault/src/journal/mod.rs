//! Journal protocol for rename/move transactions (D9, §4.15).
//!
//! This module provides the journal manifest and recovery infrastructure.

pub mod manifest;
pub mod recover;

pub use manifest::{
    JournalManifest, MANIFEST_VERSION, TransactionItem, TransactionItemState, TransactionPhase,
    TransactionState,
};
pub use recover::{
    JournalRecovery, JournalRecoveryError, JournalRecoveryOutcome, JournalRecoveryResult,
};
