//! Geometry, docking admission and native input use the same mounted views.
use crate::{
    app::{App, Message},
    workspace::{gestures::Target, tab_strip},
};
use flavite_workspace::{Arrangement, LayoutAction, LayoutNode, TabId};

impl App {
    pub(crate) fn split_hits(
        &self,
    ) -> Vec<(
        flavite_workspace::SplitId,
        iced::Rectangle,
        iced::widget::pane_grid::Axis,
    )> {
        if self.maximized_group().is_some()
            || !flavite_workspace::fits(
                &self.workspace,
                flavite_workspace::Size {
                    width: self.workspace_bounds.width,
                    height: self.workspace_bounds.height,
                },
                1.0,
            )
        {
            return vec![];
        }
        self.adapter
            .panes
            .layout()
            .split_regions(4.0, 0.0, self.workspace_bounds.size())
            .into_iter()
            .filter_map(|(split, (axis, rect, ratio))| {
                let id = self.adapter.workspace_split(split)?;
                let mut corridor = axis.split_line_bounds(rect, ratio, 4.0);
                match axis {
                    iced::widget::pane_grid::Axis::Vertical => {
                        corridor.x -= 2.0;
                        corridor.width += 4.0;
                    }
                    iced::widget::pane_grid::Axis::Horizontal => {
                        corridor.y -= 2.0;
                        corridor.height += 4.0;
                    }
                }
                corridor.x += self.workspace_bounds.x;
                corridor.y += self.workspace_bounds.y;
                Some((id, corridor, axis))
            })
            .collect()
    }

    pub(crate) fn pointer_resize_ratio(
        &self,
        split: flavite_workspace::SplitId,
        position: iced::Point,
    ) -> Option<f32> {
        let toolkit = self.adapter.split_map.get(&split)?;
        let regions =
            self.adapter
                .panes
                .layout()
                .split_regions(4.0, 0.0, self.workspace_bounds.size());
        let (axis, rect, _) = regions.get(toolkit)?;
        let (position, extent) = match axis {
            iced::widget::pane_grid::Axis::Vertical => {
                (position.x - self.workspace_bounds.x - rect.x, rect.width)
            }
            iced::widget::pane_grid::Axis::Horizontal => {
                (position.y - self.workspace_bounds.y - rect.y, rect.height)
            }
        };
        Some((position - 2.0) / (extent - 4.0).max(1.0))
    }

    pub(crate) fn project_geometry(&mut self) {
        self.adapter.project(
            &self.workspace,
            self.workspace_bounds.size(),
            &self.resize_preview,
        );
        self.group_bounds
            .retain(|id, _| self.workspace.group(id).is_some());
        self.editor_bounds
            .retain(|id, _| self.workspace.tab(id).is_some());
    }

    #[must_use]
    pub fn is_tab_visible(&self, tab: TabId) -> bool {
        let Some(group) = self.group_of(tab) else {
            return false;
        };
        if self
            .workspace
            .group(&group)
            .is_none_or(|g| g.active() != tab)
        {
            return false;
        }
        let adaptive = !flavite_workspace::fits(
            &self.workspace,
            flavite_workspace::Size {
                width: self.workspace_bounds.width,
                height: self.workspace_bounds.height,
            },
            1.0,
        );
        self.maximized_group()
            .or_else(|| adaptive.then(|| self.focused_group()).flatten())
            .is_none_or(|id| id == group)
    }

    pub(crate) fn layout_fits(&self, action: &LayoutAction) -> bool {
        match action {
            LayoutAction::SplitGroup {
                group, arrangement, ..
            } => {
                let Some(g) = self.workspace.group(group) else {
                    return false;
                };
                self.split_fits(g.active(), *group, *arrangement)
            }
            LayoutAction::MoveTab {
                tab,
                target:
                    flavite_workspace::TabDropTarget::SplitFrom {
                        group, arrangement, ..
                    },
                ..
            } => self.split_fits(*tab, *group, *arrangement),
            _ => true,
        }
    }

    fn split_fits(
        &self,
        tab: TabId,
        group: flavite_workspace::GroupId,
        arrangement: Arrangement,
    ) -> bool {
        let Some(rect) = self.group_bounds.get(&group) else {
            return true;
        };
        let Some(view) = self.workspace.tab(&tab) else {
            return false;
        };
        let a = flavite_workspace::view_minimum(view.kind(), 1.0);
        let b = flavite_workspace::geometry::subtree_minimum(
            &self.workspace,
            &LayoutNode::Group(group),
            1.0,
        );
        match arrangement {
            Arrangement::SideBySide => {
                rect.width >= a.width + b.width + flavite_workspace::SPLITTER_WIDTH
                    && rect.height >= a.height.max(b.height)
            }
            Arrangement::Stacked => {
                rect.height >= a.height + b.height + flavite_workspace::SPLITTER_WIDTH
                    && rect.width >= a.width.max(b.width)
            }
        }
    }

    pub(crate) fn drop_allowed(&self, tab: TabId, target: Target) -> bool {
        let Some(g) = self.workspace.group(&target.group) else {
            return false;
        };
        if target.edge.is_some() && g.tabs().len() == 1 && g.tabs()[0] == tab {
            return false;
        }
        target
            .edge
            .is_none_or(|edge| self.split_fits(tab, target.group, edge.arrangement()))
    }

    /// Pointer hit tests consume the same width-aware tab plan as paint.
    pub(crate) fn dock_target(&self, position: iced::Point) -> Option<Target> {
        let tab = self.gesture.dragged_tab()?;
        for group in self.workspace.group_order() {
            let g = self.workspace.group(&group)?;
            if !self.is_tab_visible(g.active()) {
                continue;
            }
            let Some(rect) = self.group_bounds.get(&group) else {
                continue;
            };
            if !rect.contains(position) {
                continue;
            }
            let x = position.x - rect.x;
            let y = position.y - rect.y;
            let active = g.tabs().iter().position(|t| *t == g.active()).unwrap_or(0);
            let slots = tab_strip::plan(rect.width, g.tabs().len(), active);
            if y < tab_strip::TAB_HEIGHT {
                if x >= rect.width - tab_strip::CONTROLS_WIDTH {
                    return None;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let offset = ((x / slots.tab_width.max(1.0)) + 0.5).floor() as usize;
                return Some(Target {
                    group,
                    index: (slots.start + offset.min(slots.count)).min(g.tabs().len()),
                    edge: None,
                });
            }
            let edge = if x < 32.0 {
                Some(flavite_workspace::Edge::Left)
            } else if x > rect.width - 32.0 {
                Some(flavite_workspace::Edge::Right)
            } else if y < tab_strip::TAB_HEIGHT + 32.0 {
                Some(flavite_workspace::Edge::Top)
            } else if y > rect.height - 32.0 {
                Some(flavite_workspace::Edge::Bottom)
            } else {
                None
            };
            let target = Target {
                group,
                index: g.tabs().len(),
                edge,
            };
            let _ = tab;
            return Some(target);
        }
        None
    }

    pub(crate) fn accessible_selection(
        &mut self,
        tab: TabId,
        incarnation: u64,
        revision: u64,
        anchor: usize,
        head: usize,
    ) {
        if self.picker.open
            || self.palette_open
            || self.strip_menu.is_some()
            || !self.is_tab_visible(tab)
            || self.has_composition()
        {
            return;
        }
        let Some(view) = self.note_editor_view(tab) else {
            return;
        };
        let Some(logical) = self.workspace.tab(&tab).map(|v| v.interaction()) else {
            return;
        };
        if self.notes.incarnation(tab) != Some(incarnation)
            || logical.incarnation != incarnation
            || view.revision != revision
            || !view.text.is_char_boundary(anchor)
            || !view.text.is_char_boundary(head)
        {
            return;
        }
        let boundary = |at| {
            at == 0
                || flavite_editor::viewport::next_grapheme_boundary(
                    &view.text,
                    flavite_editor::viewport::previous_grapheme_boundary(&view.text, at),
                ) == at
        };
        if !boundary(anchor) || !boundary(head) {
            return;
        }
        let Some(mut snapshot) = self.editor_interaction_snapshot(tab) else {
            return;
        };
        self.dispatch_layout(|revision| LayoutAction::ActivateTab {
            expected_revision: revision,
            tab,
        });
        snapshot.sequence = snapshot.sequence.saturating_add(1);
        snapshot.selection = (anchor, head);
        snapshot.caret = head;
        snapshot.focused = true;
        self.workspace.release_editor_focus(Some(tab));
        self.apply_editor_interaction(Message::EditorAction {
            tab,
            incarnation,
            action: flavite_editor::EditorAction::Interaction(snapshot),
        });
    }

    pub(crate) fn has_composition(&self) -> bool {
        self.workspace.tabs().any(|(_, v)| {
            v.interaction().composition == flavite_workspace::CompositionState::Active
        })
    }

    // Keep native actions on the exact normal acknowledgement path.
    fn apply_editor_interaction(&mut self, message: Message) {
        let _ = self.update(message);
    }
}
