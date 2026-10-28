//! Headless contract shared by the iced surface and its test doubles.

use flavite_api::BufferView;
use flavite_buffer::undo::EditOp;
use flavite_syntax::text::{Span, TextByte};
use std::path::PathBuf;

/// Immutable buffer data required to render and dispatch an editor command.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorView {
    /// Buffer identity.
    pub buffer_id: u64,
    /// Revision represented by this snapshot.
    pub revision: u64,
    /// Normalized text.
    pub text: String,
    /// Vault-relative path.
    pub path: PathBuf,
    /// State-machine label.
    pub state: String,
}

impl From<&BufferView> for EditorView {
    fn from(view: &BufferView) -> Self {
        Self {
            buffer_id: view.buffer_id,
            revision: view.revision,
            text: view.text.clone(),
            path: view.path.clone(),
            state: view.state.clone(),
        }
    }
}

/// A revision guarded edit emitted by a surface.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorAction {
    /// Buffer instance identity.
    pub buffer_id: u64,
    /// Revision that the edit positions refer to.
    pub expected_revision: u64,
    /// Atomic edit batch.
    pub edits: Vec<EditOp>,
    /// Optional undo/redo request carried by this surface action.
    pub history: Option<bool>,
}

impl EditorAction {
    /// Convert to the Session's authoritative mutation command.
    #[must_use]
    pub fn into_session_command(self) -> flavite_api::lattice::session::SessionCommand {
        if let Some(redo) = self.history {
            return if redo {
                flavite_api::lattice::session::SessionCommand::Redo {
                    buffer_id: self.buffer_id.into(),
                }
            } else {
                flavite_api::lattice::session::SessionCommand::Undo {
                    buffer_id: self.buffer_id.into(),
                }
            };
        }
        flavite_api::lattice::session::SessionCommand::ApplyEdits {
            buffer_id: self.buffer_id.into(),
            revision: flavite_buffer::revision::Revision::new(self.expected_revision),
            ops: self.edits,
        }
    }

    /// Build a single insertion/replacement edit.
    #[must_use]
    pub fn replace(view: &EditorView, start: usize, end: usize, text: String) -> Self {
        Self {
            buffer_id: view.buffer_id,
            expected_revision: view.revision,
            edits: vec![EditOp::new(Span::new(TextByte(start), TextByte(end)), text)],
            history: None,
        }
    }

    /// Build an undo or redo action.
    #[must_use]
    pub fn history(view: &EditorView, redo: bool) -> Self {
        Self {
            buffer_id: view.buffer_id,
            expected_revision: view.revision,
            edits: Vec::new(),
            history: Some(redo),
        }
    }
}

/// UI-independent editor surface behavior.
pub trait EditorSurface {
    /// Current immutable view shown by this surface.
    fn view(&self) -> &EditorView;

    /// Translate a selection replacement into a revision-guarded command.
    fn replace_selection(&self, start: usize, end: usize, text: String) -> EditorAction {
        EditorAction::replace(self.view(), start, end, text)
    }
}
