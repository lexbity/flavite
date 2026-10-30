//! AccessKit semantic tree and action integration for the Flavite shell.

pub mod program;
pub mod tree;

use crate::app::{App, Message};
use crate::pane::Pane;
use accesskit::{Action, ActionData, ActionRequest};

/// Convert a native text-selection action into the ordinary app message path.
/// Invalid node IDs, stale/missing editor state, and malformed positions are
/// rejected without changing the editor.
pub fn route_action(app: &App, request: ActionRequest) -> Option<Message> {
    if request.target_tree != accesskit::TreeId::ROOT {
        return None;
    }
    if request.action == Action::Focus
        && request.target_node == tree::EDITOR
        && app.accessibility_pane() == Pane::Editor
    {
        return Some(Message::EditorFocusRequested);
    }
    if request.action == Action::Click {
        let pane = match request.target_node {
            accesskit::NodeId(10) => Pane::Workspace,
            accesskit::NodeId(11) => Pane::Editor,
            accesskit::NodeId(12) => Pane::Read,
            accesskit::NodeId(13) => Pane::Graph,
            accesskit::NodeId(14) => Pane::Settings,
            _ => return None,
        };
        return Some(Message::Navigate(pane));
    }
    if request.action != Action::SetTextSelection
        || request.target_node != tree::EDITOR
        || app.accessibility_pane() != Pane::Editor
    {
        return None;
    }
    let ActionData::SetTextSelection(selection) = request.data? else {
        return None;
    };
    if selection.anchor.node != tree::EDITOR_TEXT || selection.focus.node != tree::EDITOR_TEXT {
        return None;
    }
    let (_, contents, interaction) = app.accessibility_document();
    let interaction = interaction?;
    let anchor = tree::char_index_to_byte(&contents, selection.anchor.character_index)?;
    let focus = tree::char_index_to_byte(&contents, selection.focus.character_index)?;
    Some(Message::EditorSelectionRequested {
        revision: interaction.revision,
        anchor,
        focus,
    })
}
