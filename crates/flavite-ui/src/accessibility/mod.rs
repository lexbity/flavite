//! AccessKit semantic tree and action integration for the Flavite shell.

pub mod program;
pub mod reading;
pub(crate) mod text;
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
        NodeTarget::EditorHistory { source, action } if request.action == Action::Click => {
            Some(Message::EditorHistory(
                crate::host::editor_history::HistoryMessage::Act(source, action),
            ))
        }
        NodeTarget::EditorContextExpand(target) if request.action == Action::Click => {
            Some(Message::ContextMode(
                crate::host::context_mode::ContextMessage::ExpandPane(target),
            ))
        }
        NodeTarget::EditorContextCommand { target, command } if request.action == Action::Click => {
            Some(Message::ContextMode(
                crate::host::context_mode::ContextMessage::Run(target, command),
            ))
        }
        NodeTarget::EditorContextMenu { target, menu } if request.action == Action::Click => {
            Some(Message::ContextMode(
                crate::host::context_mode::ContextMessage::OpenMenu(target, menu),
            ))
        }
        NodeTarget::FindQuery(tab)
            if app.is_tab_visible(tab) && app.note_search.finds.contains_key(&tab) =>
        {
            match (request.action, request.data) {
                (Action::SetValue, Some(ActionData::Value(value))) => Some(Message::NoteSearch(
                    crate::host::note_search::NoteSearchMessage::FindQuery {
                        tab,
                        query: value.into(),
                    },
                )),
                (Action::Focus, _) => Some(Message::NoteSearch(
                    crate::host::note_search::NoteSearchMessage::FindOpen(tab),
                )),
                _ => None,
            }
        }
        NodeTarget::NoteSearchCommand(tab, command) if request.action == Action::Click => {
            crate::host::commands::note_search_command(app, tab, command)
        }
        NodeTarget::FindNext { tab, forward }
            if request.action == Action::Click && app.is_tab_visible(tab) =>
        {
            Some(Message::NoteSearch(
                crate::host::note_search::NoteSearchMessage::FindNext { tab, forward },
            ))
        }
        NodeTarget::FindClose(tab)
            if request.action == Action::Click && app.is_tab_visible(tab) =>
        {
            Some(Message::NoteSearch(
                crate::host::note_search::NoteSearchMessage::FindClose(tab),
            ))
        }
        NodeTarget::SelectionSearchQuery(tab)
            if app.is_tab_visible(tab) && app.selection_search.tabs.contains_key(&tab) =>
        {
            match (request.action, request.data) {
                (Action::SetValue, Some(ActionData::Value(value))) => {
                    Some(Message::SelectionSearch(
                        crate::host::selection_search::SelectionSearchMessage::DraftAction {
                            tab,
                            action: iced::widget::text_editor::Action::Edit(
                                iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
                                    value.into(),
                                )),
                            ),
                        },
                    ))
                }
                (Action::Focus, _) => Some(Message::SelectionSearch(
                    crate::host::selection_search::SelectionSearchMessage::Open {
                        tab,
                        kind: crate::host::selection_search::ApplyKind::Select,
                    },
                )),
                _ => None,
            }
        }
        NodeTarget::SelectionSearchCommand(tab, command)
            if request.action == Action::Click && app.is_tab_visible(tab) =>
        {
            Some(match command {
                "apply" => Message::SelectionSearch(
                    crate::host::selection_search::SelectionSearchMessage::Apply(tab),
                ),
                "close" => Message::SelectionSearch(
                    crate::host::selection_search::SelectionSearchMessage::Close(tab),
                ),
                "regex" => Message::SelectionSearch(
                    crate::host::selection_search::SelectionSearchMessage::ToggleRegex(tab),
                ),
                "case" => Message::SelectionSearch(
                    crate::host::selection_search::SelectionSearchMessage::ToggleCase(tab),
                ),
                _ => return None,
            })
        }
        NodeTarget::ReplaceInput(tab)
            if app.is_tab_visible(tab) && app.note_search.replaces.contains_key(&tab) =>
        {
            match (request.action, request.data) {
                (Action::SetValue, Some(ActionData::Value(value))) => Some(Message::NoteSearch(
                    crate::host::note_search::NoteSearchMessage::ReplaceText {
                        tab,
                        text: value.into(),
                    },
                )),
                (Action::Focus, _) => Some(Message::NoteSearch(
                    crate::host::note_search::NoteSearchMessage::ReplaceOpen(tab),
                )),
                _ => None,
            }
        }
        NodeTarget::ReplaceNext(tab)
            if request.action == Action::Click && app.is_tab_visible(tab) =>
        {
            Some(Message::NoteSearch(
                crate::host::note_search::NoteSearchMessage::ReplaceNext(tab),
            ))
        }
        NodeTarget::ReplaceAll(tab)
            if request.action == Action::Click && app.is_tab_visible(tab) =>
        {
            Some(Message::NoteSearch(
                crate::host::note_search::NoteSearchMessage::ReplaceAll(tab),
            ))
        }

        NodeTarget::NoteMode(tab) if request.action == Action::Click && !app.has_composition() => {
            Some(Message::ToggleNoteMode(tab))
        }
        NodeTarget::ContextBinding { tab, binding } if request.action == Action::Click => {
            Some(Message::SetContextBinding { tab, binding })
        }
        NodeTarget::Reading {
            tab,
            document,
            action,
        } => reading::route(app, tab, document, action, request),
        NodeTarget::Editor {
            tab,
            incarnation,
            revision,
        } => {
            if !crate::host::reading::is_edit(app, tab)
                || app.picker.open
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
        NodeTarget::SidebarFilter => match (request.action, request.data) {
            (Action::SetValue, Some(ActionData::Value(value))) => {
                Some(Message::SidebarFilter(value.into()))
            }
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
            NodeTarget::OpenNote(path) if request.action == Action::Click => Some(
                Message::OpenIntent(crate::host::open_intents::OpenIntent::reuse(
                    flavite_workspace::NoteLocator::from_relative(&path).ok()?,
                    None,
                )),
            ),
            NodeTarget::OpenPicker => Some(Message::PickerOpen),
            NodeTarget::ToggleSidebar => Some(Message::ToggleSidebar),
            NodeTarget::SidebarToTab => Some(Message::SidebarToTab),
            NodeTarget::OpenPalette => Some(Message::PaletteOpen),
            NodeTarget::MenuClose => Some(Message::MenuClose),
            NodeTarget::GroupMenu(group, menu) => Some(Message::GroupMenu(group, menu)),
            NodeTarget::GraphSelect {
                tab,
                node,
                key,
                incarnation,
            } if request.action == Action::Click => Some(Message::GraphNodeCommand {
                tab,
                key,
                incarnation,
                command: crate::host::graph::GraphCommand::Select(node),
            }),
            NodeTarget::GraphOpen {
                tab,
                node,
                key,
                incarnation,
            } if request.action == Action::Click => Some(Message::GraphNodeCommand {
                tab,
                key,
                incarnation,
                command: crate::host::graph::GraphCommand::Open(node),
            }),
            NodeTarget::GraphInsert {
                tab,
                node,
                key,
                incarnation,
            } if request.action == Action::Click => Some(Message::GraphNodeCommand {
                tab,
                key,
                incarnation,
                command: crate::host::graph::GraphCommand::Insert(node),
            }),
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
