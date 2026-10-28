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

/// A note candidate offered while completing a wikilink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkTarget {
    /// Plain-text link target (vault-relative path or unique stem).
    pub target: String,
    /// Human-readable note title.
    pub title: String,
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
    /// Convert to a public Flavite invocation with revision preconditions.
    #[must_use]
    pub fn into_invocation(self, request_id: flavite_api::RequestId) -> flavite_api::Invocation {
        use flavite_api::{Origin, Preconditions};
        if let Some(redo) = self.history {
            let (command, args) = if redo {
                (
                    "redo",
                    serde_json::json!({"buffer_id": self.buffer_id, "revision": self.expected_revision}),
                )
            } else {
                (
                    "undo",
                    serde_json::json!({"buffer_id": self.buffer_id, "revision": self.expected_revision}),
                )
            };
            return flavite_api::Invocation::with_preconditions(
                request_id,
                command,
                args,
                Origin::Gui,
                Preconditions::new()
                    .with_buffer_id(self.buffer_id)
                    .with_revision(self.expected_revision)
                    .requires_write(),
            );
        }
        let ops = self
            .edits
            .iter()
            .map(|edit| {
                serde_json::json!({
                    "range": [edit.range.start.0, edit.range.end.0],
                    "replacement": edit.replacement,
                })
            })
            .collect::<Vec<_>>();
        flavite_api::Invocation::with_preconditions(
            request_id,
            "apply_edits",
            serde_json::json!({"buffer_id": self.buffer_id, "revision": self.expected_revision, "ops": ops}),
            Origin::Gui,
            Preconditions::new()
                .with_buffer_id(self.buffer_id)
                .with_revision(self.expected_revision)
                .requires_write(),
        )
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
