//! Headless contract shared by the iced surface and its test doubles.

use crate::widget::EditorInteractionSnapshot;
use flavite_api::{BufferView, SharedBufferView};
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
    pub text: std::sync::Arc<str>,
    /// Vault-relative path.
    pub path: PathBuf,
    /// State-machine label.
    pub state: flavite_api::BufferStatus,
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
            text: view.text.clone().into(),
            path: view.path.clone(),
            state: view.state,
        }
    }
}

impl From<&SharedBufferView> for EditorView {
    fn from(view: &SharedBufferView) -> Self {
        Self {
            buffer_id: view.buffer_id,
            revision: view.revision,
            text: view.text.clone(),
            path: view.path.clone(),
            state: view.state,
        }
    }
}

/// Direction for a revision-guarded history action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryDirection {
    /// Move to the parent edit state.
    Undo,
    /// Reapply the selected child edit state.
    Redo,
}

/// One host-visible editor action. Variants make invalid combinations
/// impossible: navigation cannot carry edits, history cannot carry a link,
/// and input acknowledgements cannot masquerade as document mutations.
#[derive(Debug, Clone, PartialEq)]
pub enum EditorAction {
    /// Apply a revision-guarded edit batch through Session.
    Edit {
        /// Buffer instance identity.
        buffer_id: u64,
        /// Revision whose byte offsets the edits reference.
        expected_revision: u64,
        /// Atomic edit batch.
        edits: Vec<EditOp>,
    },
    /// Undo or redo through Session.
    History {
        /// Buffer instance identity.
        buffer_id: u64,
        /// Revision expected by the history command.
        expected_revision: u64,
        /// Direction in the undo tree.
        direction: HistoryDirection,
    },
    /// Navigate from an exact scanner-confirmed link occurrence.
    Navigate {
        /// Source buffer identity.
        buffer_id: u64,
        /// Revision represented by the link span.
        expected_revision: u64,
        /// Link syntax and occurrence.
        navigation: LinkNavigation,
    },
    /// Host-visible selection, viewport, focus, or composition update.
    Interaction(EditorInteractionSnapshot),
}

/// A link activation request. It intentionally carries syntax, not a path:
/// only the host/index can resolve a vault target safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkNavigation {
    /// Exact target syntax from the scanner.
    pub target: String,
    /// Optional display alias.
    pub alias: Option<String>,
    /// Optional heading or block anchor.
    pub anchor: Option<String>,
    /// Stable-ID target form.
    pub by_id: bool,
    /// Link source span in the revisioned view.
    pub span: Span<TextByte>,
    /// Unresolved note links should create a note when activated.
    pub create_if_unresolved: bool,
}

impl LinkNavigation {
    /// Build a note-link activation from its source byte range without making
    /// shell callers depend on syntax-crate coordinate types.
    #[must_use]
    pub fn note(target: impl Into<String>, start: usize, end: usize) -> Self {
        Self {
            target: target.into(),
            alias: None,
            anchor: None,
            by_id: false,
            span: Span::new(TextByte(start), TextByte(end)),
            create_if_unresolved: false,
        }
    }
}

impl EditorAction {
    /// Convert to the Session's authoritative mutation command.
    #[must_use]
    pub fn into_session_command(self) -> flavite_api::session::SessionCommand {
        match self {
            Self::History {
                buffer_id,
                expected_revision,
                direction,
            } => {
                if direction == HistoryDirection::Redo {
                    flavite_api::session::SessionCommand::Redo {
                        buffer_id: buffer_id.into(),
                        revision: flavite_buffer::revision::Revision::new(expected_revision),
                    }
                } else {
                    flavite_api::session::SessionCommand::Undo {
                        buffer_id: buffer_id.into(),
                        revision: flavite_buffer::revision::Revision::new(expected_revision),
                    }
                }
            }
            Self::Edit {
                buffer_id,
                expected_revision,
                edits,
            } => flavite_api::session::SessionCommand::ApplyEdits {
                buffer_id: buffer_id.into(),
                revision: flavite_buffer::revision::Revision::new(expected_revision),
                ops: edits,
            },
            Self::Navigate { .. } | Self::Interaction(_) => {
                unreachable!("non-mutation editor action has no Session command")
            }
        }
    }

    /// Turn an activated link into the common API command after the host has
    /// resolved its raw target through the vault index.
    #[must_use]
    pub fn into_link_invocation(
        self,
        request_id: flavite_api::RequestId,
        resolved_path: PathBuf,
    ) -> Option<flavite_api::Invocation> {
        let Self::Navigate {
            buffer_id,
            expected_revision,
            navigation,
        } = self
        else {
            return None;
        };
        Some(flavite_api::Invocation::with_preconditions(
            request_id,
            "note.open",
            serde_json::json!({
                "path": resolved_path,
                "target": navigation.target,
                "anchor": navigation.anchor,
                "by_id": navigation.by_id,
                "create_if_unresolved": navigation.create_if_unresolved,
                "source_buffer_id": buffer_id,
                "source_revision": expected_revision,
                "source_span": [navigation.span.start.0, navigation.span.end.0],
            }),
            flavite_api::Origin::Gui,
            flavite_api::Preconditions::new()
                .with_buffer_id(buffer_id)
                .with_revision(expected_revision),
        ))
    }

    /// Build a single insertion/replacement edit.
    #[must_use]
    pub fn replace(view: &EditorView, start: usize, end: usize, text: String) -> Self {
        Self::Edit {
            buffer_id: view.buffer_id,
            expected_revision: view.revision,
            edits: vec![EditOp::new(Span::new(TextByte(start), TextByte(end)), text)],
        }
    }

    /// Build an undo or redo action.
    #[must_use]
    pub fn history(view: &EditorView, redo: bool) -> Self {
        Self::History {
            buffer_id: view.buffer_id,
            expected_revision: view.revision,
            direction: if redo {
                HistoryDirection::Redo
            } else {
                HistoryDirection::Undo
            },
        }
    }

    /// Build a navigation request from a scanner-confirmed note link.
    #[must_use]
    pub fn open_link(view: &EditorView, navigation: LinkNavigation) -> Self {
        Self::Navigate {
            buffer_id: view.buffer_id,
            expected_revision: view.revision,
            navigation,
        }
    }

    /// Build an explicit autocomplete text replacement. Semantic graph link
    /// insertion uses its dedicated API plan and is not represented here.
    #[must_use]
    pub fn insert_link(view: &EditorView, start: usize, end: usize, text: String) -> Self {
        Self::replace(view, start, end, text)
    }

    /// Convert a local edit into the public command invocation contract.
    #[must_use]
    pub fn into_invocation(
        self,
        request_id: flavite_api::RequestId,
    ) -> Option<flavite_api::Invocation> {
        let (command_id, buffer_id, expected_revision, args) = match self {
            Self::Edit {
                buffer_id,
                expected_revision,
                edits,
            } => {
                let ops = edits
                    .into_iter()
                    .map(|edit| {
                        serde_json::json!({
                            "range": [edit.range.start.0, edit.range.end.0],
                            "replacement": edit.replacement,
                        })
                    })
                    .collect::<Vec<_>>();
                (
                    "apply_edits",
                    buffer_id,
                    expected_revision,
                    serde_json::json!({ "buffer_id": buffer_id, "revision": expected_revision, "ops": ops }),
                )
            }
            Self::History {
                buffer_id,
                expected_revision,
                direction,
            } => {
                let command = if direction == HistoryDirection::Undo {
                    "undo"
                } else {
                    "redo"
                };
                (
                    command,
                    buffer_id,
                    expected_revision,
                    serde_json::json!({ "buffer_id": buffer_id, "revision": expected_revision }),
                )
            }
            Self::Navigate { .. } | Self::Interaction(_) => return None,
        };
        Some(flavite_api::Invocation::with_preconditions(
            request_id,
            command_id,
            args,
            flavite_api::Origin::Gui,
            flavite_api::Preconditions::new()
                .with_buffer_id(buffer_id)
                .with_revision(expected_revision)
                .requires_write(),
        ))
    }

    /// Borrow the edit payload when this is an edit action.
    #[must_use]
    pub fn as_edit(&self) -> Option<(u64, u64, &[EditOp])> {
        match self {
            Self::Edit {
                buffer_id,
                expected_revision,
                edits,
            } => Some((*buffer_id, *expected_revision, edits)),
            _ => None,
        }
    }

    /// Borrow the navigation payload when this is a navigation action.
    #[must_use]
    pub fn navigation(&self) -> Option<&LinkNavigation> {
        match self {
            Self::Navigate { navigation, .. } => Some(navigation),
            _ => None,
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
