//! Rename/move transaction implementation (D9, §4.15).
//!
//! This module provides the infrastructure for rename/move transactions.
//!
//! For now, this is a stub implementation that provides the type definitions.
//! Full implementation will be added in subsequent slices.

pub mod buffer_patch;
pub mod planner;
pub mod undo;

pub use buffer_patch::RenameBufferPatcher;
pub use planner::{
    RenamePlan, RenamePlanItem, RenamePlanResult, RenamePlanner, RenameValidationError,
};
pub use undo::{RenameUndo, RenameUndoResult};
