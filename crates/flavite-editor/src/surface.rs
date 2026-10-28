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
    /// Registry command that owns this action (`apply_edits` or `link.insert`).
    pub command_id: &'static str,
    /// Atomic edit batch.
    pub edits: Vec<EditOp>,
    /// Optional undo/redo request carried by this surface action.
    pub history: Option<bool>,
    /// Navigation request emitted by ctrl/cmd-click. The host resolves it
    /// through the vault index and either opens the note or creates it.
    pub navigation: Option<LinkNavigation>,
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

impl EditorAction {
    /// Convert to the Session's authoritative mutation command.
    #[must_use]
    pub fn into_session_command(self) -> flavite_api::lattice::session::SessionCommand {
        if let Some(redo) = self.history {
            return if redo {
                flavite_api::lattice::session::SessionCommand::Redo {
                    buffer_id: self.buffer_id.into(),
                    revision: flavite_buffer::revision::Revision::new(self.expected_revision),
                }
            } else {
                flavite_api::lattice::session::SessionCommand::Undo {
                    buffer_id: self.buffer_id.into(),
                    revision: flavite_buffer::revision::Revision::new(self.expected_revision),
                }
            };
        }
        flavite_api::lattice::session::SessionCommand::ApplyEdits {
            buffer_id: self.buffer_id.into(),
            revision: flavite_buffer::revision::Revision::new(self.expected_revision),
            ops: self.edits,
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
        let navigation = self.navigation?;
        Some(flavite_api::Invocation::with_preconditions(
            request_id,
            "note.open",
            serde_json::json!({
                "path": resolved_path,
                "target": navigation.target,
                "anchor": navigation.anchor,
                "by_id": navigation.by_id,
                "create_if_unresolved": navigation.create_if_unresolved,
                "source_buffer_id": self.buffer_id,
                "source_revision": self.expected_revision,
                "source_span": [navigation.span.start.0, navigation.span.end.0],
            }),
            flavite_api::Origin::Gui,
            flavite_api::Preconditions::new()
                .with_buffer_id(self.buffer_id)
                .with_revision(self.expected_revision),
        ))
    }

    /// Build a single insertion/replacement edit.
    #[must_use]
    pub fn replace(view: &EditorView, start: usize, end: usize, text: String) -> Self {
        Self {
            buffer_id: view.buffer_id,
            expected_revision: view.revision,
            command_id: "apply_edits",
            edits: vec![EditOp::new(Span::new(TextByte(start), TextByte(end)), text)],
            history: None,
            navigation: None,
        }
    }

    /// Build an undo or redo action.
    #[must_use]
    pub fn history(view: &EditorView, redo: bool) -> Self {
        Self {
            buffer_id: view.buffer_id,
            expected_revision: view.revision,
            command_id: if redo { "redo" } else { "undo" },
            edits: Vec::new(),
            history: Some(redo),
            navigation: None,
        }
    }

    /// Build a navigation request from a scanner-confirmed note link.
    #[must_use]
    pub fn open_link(view: &EditorView, navigation: LinkNavigation) -> Self {
        Self {
            buffer_id: view.buffer_id,
            expected_revision: view.revision,
            command_id: "note.open",
            edits: Vec::new(),
            history: None,
            navigation: Some(navigation),
        }
    }

    /// Build a link-insertion edit dispatched through the registry's
    /// `link.insert` command.
    #[must_use]
    pub fn insert_link(view: &EditorView, start: usize, end: usize, text: String) -> Self {
        let mut action = Self::replace(view, start, end, text);
        action.command_id = "link.insert";
        action
    }

    /// Convert a local edit into the public command invocation contract.
    #[must_use]
    pub fn into_invocation(
        self,
        request_id: flavite_api::RequestId,
    ) -> Option<flavite_api::Invocation> {
        if self.navigation.is_some() || self.history.is_some() {
            return None;
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
        Some(flavite_api::Invocation::with_preconditions(
            request_id,
            self.command_id,
            serde_json::json!({ "buffer_id": self.buffer_id, "revision": self.expected_revision, "ops": ops }),
            flavite_api::Origin::Gui,
            flavite_api::Preconditions::new()
                .with_buffer_id(self.buffer_id)
                .with_revision(self.expected_revision)
                .requires_write(),
        ))
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
