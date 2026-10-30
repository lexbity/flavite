//! AccessKit semantic tree and action integration for the Flavite shell.

pub mod program;
pub mod tree;

pub use tree::NodeTarget;

use crate::app::App;
use crate::app::Message;
use accesskit::{Action, ActionRequest};
use flavite_workspace::LayoutAction;

/// Convert a native accessibility action into the ordinary app message
/// path. Unknown nodes, stale state, and malformed data are rejected
/// without changing the workspace.
#[must_use]
pub fn route_action(app: &App, request: ActionRequest) -> Option<Message> {
    if request.target_tree != accesskit::TreeId::ROOT {
        return None;
    }
    let target = app.a11y_nodes.get(&request.target_node)?.clone();
    let revision = app.workspace.revision();
    match request.action {
        Action::Click => match target {
            tree::NodeTarget::ActivateTab(tab) => {
                Some(Message::Layout(LayoutAction::ActivateTab {
                    expected_revision: revision,
                    tab,
                }))
            }
            tree::NodeTarget::CloseTab(tab) => Some(Message::Layout(LayoutAction::CloseTab {
                expected_revision: revision,
                tab,
            })),
            tree::NodeTarget::OpenNote(path) => {
                let locator = flavite_workspace::NoteLocator::from_relative(&path).ok()?;
                Some(Message::OpenNote {
                    locator,
                    group: None,
                })
            }
            tree::NodeTarget::OpenPicker => Some(Message::PickerOpen),
            tree::NodeTarget::OpenPalette => Some(Message::PaletteOpen),
            tree::NodeTarget::ResizeSplit { .. } => None,
        },
        Action::Increment => match target {
            tree::NodeTarget::ResizeSplit { split, .. } => resize_by(app, split, 1),
            _ => None,
        },
        Action::Decrement => match target {
            tree::NodeTarget::ResizeSplit { split, .. } => resize_by(app, split, -1),
            _ => None,
        },
        Action::Focus => match target {
            tree::NodeTarget::ActivateTab(tab) => {
                Some(Message::Layout(LayoutAction::ActivateTab {
                    expected_revision: revision,
                    tab,
                }))
            }
            _ => None,
        },
        // Text selection arrives with the editor integration (W03);
        // source-reading text is not editable, so no other action applies.
        _ => None,
    }
}

fn resize_by(app: &App, split: flavite_workspace::SplitId, direction: i8) -> Option<Message> {
    let current = current_ratio(app, split)?;
    let adjusted = if direction >= 0 {
        (current + tree::RESIZE_STEP_PPM).min(flavite_workspace::MAX_RATIO_PPM)
    } else {
        current
            .saturating_sub(tree::RESIZE_STEP_PPM)
            .max(flavite_workspace::MIN_RATIO_PPM)
    };
    if adjusted == current {
        return None;
    }
    Some(Message::Layout(LayoutAction::ResizeSplit {
        expected_revision: app.workspace.revision(),
        split,
        desired_ratio_ppm: adjusted,
    }))
}

fn current_ratio(app: &App, split: flavite_workspace::SplitId) -> Option<u32> {
    fn find(
        node: &flavite_workspace::LayoutNode,
        split: flavite_workspace::SplitId,
    ) -> Option<u32> {
        match node {
            flavite_workspace::LayoutNode::Group(_) => None,
            flavite_workspace::LayoutNode::Split {
                id,
                desired_ratio_ppm,
                first,
                second,
                ..
            } => {
                if *id == split {
                    Some(*desired_ratio_ppm)
                } else {
                    find(first, split).or_else(|| find(second, split))
                }
            }
        }
    }
    find(app.workspace.tree(), split)
}
