//! AccessKit semantic tree and action integration for the Flavite shell.

pub mod program;
pub mod tree;

pub use tree::NodeTarget;

use crate::app::App;
use crate::app::Message;
use accesskit::{Action, ActionData, ActionRequest};
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
    match target {
        NodeTarget::Editor {
            tab,
            incarnation,
            revision,
        } => {
            if app.picker.open
                || app.palette_open
                || app.strip_menu.is_some()
                || !app.is_tab_visible(tab)
            {
                return None;
            }
            let view = app.note_editor_view(tab)?;
            if view.revision != revision
                || app.notes.incarnation(tab) != Some(incarnation)
                || app.has_composition()
            {
                return None;
            }
            let (anchor, head) = match request.action {
                Action::Focus => {
                    let s = app.editor_interaction_snapshot(tab)?;
                    (s.selection.0, s.selection.1)
                }
                Action::SetTextSelection => {
                    let Some(ActionData::SetTextSelection(selection)) = request.data else {
                        return None;
                    };
                    (
                        tree::position_byte(app, tab, &view.text, selection.anchor)?,
                        tree::position_byte(app, tab, &view.text, selection.focus)?,
                    )
                }
                _ => return None,
            };
            Some(Message::AccessibleSelection {
                tab,
                incarnation,
                revision,
                anchor,
                head,
            })
        }
        NodeTarget::ResizeSplit { split, .. } => match request.action {
            Action::Increment => resize_by(app, split, 1),
            Action::Decrement => resize_by(app, split, -1),
            _ => None,
        },
        NodeTarget::MenuItem(index) => match request.action {
            Action::Click => Some(Message::MenuPick(index)),
            Action::Focus => Some(Message::MenuFocus(index)),
            _ => None,
        },
        NodeTarget::OverlayQuery => match (request.action, request.data) {
            (Action::SetValue, Some(ActionData::Value(value))) => Some(if app.picker.open {
                Message::PickerQuery(value.into())
            } else {
                Message::PaletteQuery(value.into())
            }),
            _ => None,
        },
        NodeTarget::OverlayItem(index) if request.action == Action::Click => {
            Some(if app.picker.open {
                Message::PickerPick(index)
            } else {
                Message::PalettePick(index)
            })
        }
        NodeTarget::DismissOverlay if request.action == Action::Click => Some(if app.picker.open {
            Message::PickerClose
        } else {
            Message::PaletteClose
        }),
        target if matches!(request.action, Action::Click | Action::Focus) => match target {
            NodeTarget::FocusGroup(group) => Some(Message::Layout(LayoutAction::FocusGroup {
                expected_revision: app.workspace.revision(),
                group,
            })),
            NodeTarget::ActivateTab(tab) => Some(Message::Layout(LayoutAction::ActivateTab {
                expected_revision: app.workspace.revision(),
                tab,
            })),
            NodeTarget::CloseTab(tab) if request.action == Action::Click => {
                Some(Message::CloseTab(tab))
            }
            NodeTarget::OpenNote(path) if request.action == Action::Click => {
                Some(Message::OpenNote {
                    locator: flavite_workspace::NoteLocator::from_relative(&path).ok()?,
                    group: None,
                })
            }
            NodeTarget::OpenPicker => Some(Message::PickerOpen),
            NodeTarget::OpenPalette => Some(Message::PaletteOpen),
            NodeTarget::MenuClose => Some(Message::MenuClose),
            NodeTarget::GroupMenu(group, menu) => Some(Message::GroupMenu(group, menu)),
            _ => None,
        },
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
