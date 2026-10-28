#![forbid(unsafe_code)]

//! Tier 1 — durability-critical. `EditorModel` over ropey text with
//! multi-selection, undo tree with time-based coalescing, per-line EOL side
//! table, and the D7 buffer state machine. Zero UI dependencies (§4.1, D14,
//! §4.16.1).

pub mod eol;
pub mod manager;
pub mod model;
pub mod recovery;
pub mod revision;
pub mod state;
pub mod undo;

// Re-export key types for convenience.
pub use eol::{EolTable, LineEnding};
pub use manager::{BufferManager, BufferStateChanged, NoteRef};
pub use model::{EditorModel, EditorModelBuilder, Selection};
pub use recovery::{
    RecoveryConfig, RecoveryFile, RecoveryHeader, RecoveryWriter, RecoveryWriterBuilder,
};
pub use revision::{
    BaseHash, BufferId, ContentHash, EditPrecondition, PreconditionResult, Revision,
};
pub use state::{Buffer, BufferState, StateTransition, TransitionReason};
pub use undo::{EditGroup, EditOp, EolMetadata, LinearUndoStack, UndoNode, UndoTree};
