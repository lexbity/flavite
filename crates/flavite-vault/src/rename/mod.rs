//! Rename/move transaction implementation (D9, §4.15).
//!
//! This module provides the infrastructure for rename/move transactions.
//!
//! For now, this is a stub implementation that provides the type definitions.
//! Full implementation will be added in subsequent slices.

pub mod attachments;
pub mod buffer_patch;
pub mod id_conflict;
pub mod infer;
pub mod planner;
pub mod undo;

pub use attachments::{
    AttachmentRename, AttachmentRenameConfig, AttachmentRenameManager, AttachmentRenameResult,
    EmbedCountProvider, should_rewrite_embeds,
};
pub use buffer_patch::RenameBufferPatcher;
pub use id_conflict::{
    DuplicateIdInfo, DuplicateIdManager, DuplicateIdResolution, can_use_t2_identity,
    should_auto_rewrite_links, should_carry_identity,
};
pub use infer::{
    InboundLinkProvider, RenameCandidate, RenameInference, RenameInferenceConfig,
    RenameInferenceResult,
};
pub use planner::{
    RenamePlan, RenamePlanItem, RenamePlanResult, RenamePlanner, RenameValidationError,
};
pub use undo::{RenameUndo, RenameUndoResult};
