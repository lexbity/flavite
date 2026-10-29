//! Journal protocol for rename/move transactions (D9, §4.15).
//!
//! This module provides the journal manifest and recovery infrastructure.

pub mod executor;
pub mod manifest;

pub use executor::{FilePatch, JournalExecutor, RecoveryReport};
pub use manifest::{
    JournalManifest, MANIFEST_VERSION, TransactionItem, TransactionItemKind, TransactionItemState,
    TransactionPhase, TransactionState,
};
