//! Headless contract shared by the iced surface and its test doubles.

use crate::widget::EditorInteractionSnapshot;
use flavite_api::{BufferView, SharedBufferView};
use flavite_buffer::undo::EditOp;
use flavite_syntax::text::{Span, TextByte};
use std::path::PathBuf;

/// Single-edit changed-range hint (RFC-0008 D1): defined in flavite-syntax
/// (the lowest crate shared by vault/api/editor — the session produces it,
/// the editor consumes it), re-exported here under the editor-facing name.
pub use flavite_syntax::EditRange;

/// Immutable buffer data required to render and dispatch an editor command.
#[derive(Debug, Clone, PartialEq)]
pub struct EditorView {
    /// Buffer identity.
    pub buffer_id: u64,
    /// Revision represented by this snapshot.
    pub revision: u64,
    /// Normalized text as an immutable chunk-tree root (RFC-0011 D-10).
    /// Widgets slice bands and query line geometry via the tree; full
    /// materialization is user-paced, never on the session's keystroke
    /// path.
    pub text: flavite_buffer::chunks::DocumentText,
    /// Vault-relative path.
    pub path: PathBuf,
    /// State-machine label.
    pub state: flavite_buffer::state::BufferState,
    /// Single-edit changed-range hint, session-computed (RFC-0008 D1):
    /// `None` for external reloads, merges, journal patches, first open,
    /// and multi-op batches.
    pub last_edit: Option<EditRange>,
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
            // Serialized surface: build a tree from the delivered String
            // (client-paced; RFC-0011 FR-9).
            text: flavite_buffer::chunks::DocumentText::from_text(&view.text),
            path: view.path.clone(),
            state: view.state,
            // The serialized BufferView surface carries no session-computed
            // hint (D9: it never crosses the JSON command surface).
            last_edit: None,
        }
    }
}

impl From<&SharedBufferView> for EditorView {
    fn from(view: &SharedBufferView) -> Self {
        Self {
            buffer_id: view.buffer_id,
            revision: view.revision,
            // O(1) root clone; materialization is the widget's choice.
            text: view.text.clone(),
            path: view.path.clone(),
            state: view.state,
            last_edit: view.last_edit,
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
    /// Complete an exact source prefix using an indexed entity identity.
    Complete {
        buffer_id: u64,
        expected_revision: u64,
        start: usize,
        end: usize,
        prefix: String,
        candidate: flavite_api::CompletionCandidate,
    },
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
                    "buffer.undo"
                } else {
                    "buffer.redo"
                };
                (
                    command,
                    buffer_id,
                    expected_revision,
                    serde_json::json!({ "buffer_id": buffer_id, "revision": expected_revision }),
                )
            }
            Self::Complete { .. } | Self::Navigate { .. } | Self::Interaction(_) => return None,
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

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_buffer::state::BufferState;

    fn shared_view(last_edit: Option<EditRange>) -> SharedBufferView {
        SharedBufferView {
            buffer_id: 7,
            revision: 42,
            text: flavite_buffer::chunks::DocumentText::from("one\ntwo"),
            base_hash: Vec::new(),
            state: BufferState::Dirty,
            path: PathBuf::from("note.md"),
            source_format: flavite_syntax::SourceFormat::utf8_with_bom(),
            last_edit,
        }
    }

    fn serialized_view() -> BufferView {
        BufferView {
            buffer_id: 7,
            revision: 42,
            text: "one\ntwo".into(),
            base_hash: Vec::new(),
            state: BufferState::Dirty,
            path: PathBuf::from("note.md"),
            source_format: flavite_syntax::SourceFormat::utf8_with_bom(),
        }
    }

    /// S7-T2: the session-computed hint survives the `From<&SharedBufferView>`
    /// conversion; the serialized `BufferView` surface carries none (D9).
    #[test]
    fn editor_view_preserves_shared_hint_and_omits_serialized_hint() {
        let hint = EditRange {
            start: 2,
            old_end: 2,
            new_end: 4,
        };
        let from_shared = EditorView::from(&shared_view(Some(hint)));
        assert_eq!(from_shared.last_edit, Some(hint));

        let from_serialized = EditorView::from(&serialized_view());
        assert_eq!(from_serialized.last_edit, None);

        let none_shared = EditorView::from(&shared_view(None));
        assert_eq!(none_shared.last_edit, None);
    }
}
