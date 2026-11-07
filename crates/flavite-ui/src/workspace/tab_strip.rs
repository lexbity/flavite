//! Width-aware tab strip and the items shared by pointer/keyboard menus.
use crate::workspace::chrome::{self, Icon};
use crate::{
    app::{App, Message},
    style,
};
use flavite_workspace::{GroupId, TabId, ViewKind};
use iced::widget::{Space, container, mouse_area, row, text};
use iced::{Element, Length, alignment};

/// Tab-strip action controls reserve width at 100% scale; paint and hit-test
/// paths use the scaled [`crate::style::Metrics::controls_width`].
pub const CONTROLS_WIDTH: f32 = 96.0;

/// The back/forward arrows at the strip's left edge reserve this much
/// tab-slot width; slot math everywhere subtracts it before the first tab.
#[must_use]
pub fn nav_width(metrics: crate::style::Metrics) -> f32 {
    2.0 * metrics.icon_target
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripMenu {
    NewView,
    Actions,
    Tabs,
    /// Standard top menu bar (W10): File.
    File,
    /// Standard top menu bar (W10): Edit.
    Edit,
    /// Standard top menu bar (W10): Selection.
    Selection,
    /// Standard top menu bar (W10): View.
    View,
    /// Standard top menu bar (W10): Go.
    Go,
    /// Standard top menu bar (W10): Graph.
    Graph,
    /// Standard top menu bar (W10): Tools.
    Tools,
    /// Standard top menu bar (W10): Help.
    Help,
}

impl StripMenu {
    /// Horizontal slot of a top menu bar entry, in logical pixels.
    #[must_use]
    pub const fn bar_x(self) -> f32 {
        match self {
            Self::File => 40.0,
            Self::Edit => 86.0,
            Self::Selection => 130.0,
            Self::View => 208.0,
            Self::Go => 256.0,
            Self::Graph => 296.0,
            Self::Tools => 354.0,
            _ => 40.0,
        }
    }

    /// Whether this menu belongs to the top menu bar.
    #[must_use]
    pub const fn is_top_menu(self) -> bool {
        matches!(
            self,
            Self::File
                | Self::Edit
                | Self::Selection
                | Self::View
                | Self::Go
                | Self::Graph
                | Self::Tools
                | Self::Help
        )
    }
}

/// A contiguous window always containing the active tab. Every tab remains
/// available in the complete Tabs menu, regardless of title length.
#[derive(Debug, Clone, PartialEq)]
pub struct StripPlan {
    pub start: usize,
    pub count: usize,
    pub tab_width: f32,
}

#[must_use]
pub fn plan(width: f32, controls_width: f32, count: usize, active: usize) -> StripPlan {
    let available = (width - controls_width).max(0.0);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let visible = ((available / 104.0).floor() as usize)
        .max(1)
        .min(count.max(1));
    let start = active
        .saturating_add(1)
        .saturating_sub(visible)
        .min(count.saturating_sub(visible));
    #[allow(clippy::cast_precision_loss)]
    StripPlan {
        start,
        count: visible.min(count),
        tab_width: (available / visible as f32).min(200.0),
    }
}

pub fn view(app: &App, group: GroupId) -> Element<'_, Message> {
    let Some(g) = app.workspace.group(&group) else {
        return row![].into();
    };
    let metrics = app.appearance.metrics();
    let width = app.group_bounds.get(&group).map_or(400.0, |r| r.width);
    let active_index = g.tabs().iter().position(|t| *t == g.active()).unwrap_or(0);
    let nav = nav_width(*metrics);
    let slots = plan(
        width,
        metrics.controls_width + nav,
        g.tabs().len(),
        active_index,
    );
    let active_tab = g.active();
    let mut tabs = row![].spacing(0);
    tabs = tabs.push(chrome::icon_button(
        *metrics,
        Icon::Back,
        "Back",
        app.nav
            .can_go_back(active_tab)
            .then_some(Message::NavigateBack(active_tab)),
    ));
    tabs = tabs.push(chrome::icon_button(
        *metrics,
        Icon::Forward,
        "Forward",
        app.nav
            .can_go_forward(active_tab)
            .then_some(Message::NavigateForward(active_tab)),
    ));
    for tab in g.tabs().iter().skip(slots.start).take(slots.count) {
        tabs = tabs.push(tab_view(app, *tab, *tab == g.active(), slots.tab_width));
    }
    tabs = tabs.push(Space::new().width(Length::Fill));
    tabs = tabs.push(chrome::icon_button(
        *metrics,
        Icon::Tabs,
        "All tabs",
        Some(Message::GroupMenu(group, StripMenu::Tabs)),
    ));
    tabs = tabs.push(chrome::icon_button(
        *metrics,
        Icon::Add,
        "New view",
        Some(Message::GroupMenu(group, StripMenu::NewView)),
    ));
    tabs = tabs.push(chrome::icon_button(
        *metrics,
        Icon::More,
        "Group actions",
        Some(Message::GroupMenu(group, StripMenu::Actions)),
    ));
    container(tabs.align_y(alignment::Vertical::Center))
        .width(Length::Fill)
        .height(metrics.tab_height)
        .style(style::toolbar)
        .into()
}

fn tab_view(app: &App, tab: TabId, active: bool, width: f32) -> Element<'_, Message> {
    let Some(view) = app.workspace.tab(&tab) else {
        return row![].into();
    };
    let metrics = app.appearance.metrics();
    let kind = match view.kind() {
        ViewKind::Notes => Icon::Notes,
        ViewKind::Graph(_) => Icon::Graph,
        _ => Icon::Note,
    };
    let label = view.title();
    let title = container(
        row![
            chrome::icon(*metrics, kind),
            text(label)
                .size(metrics.text_sm)
                .wrapping(iced::widget::text::Wrapping::None)
        ]
        .spacing(6)
        .align_y(alignment::Vertical::Center),
    )
    .width((width - metrics.icon_target).max(0.0))
    .height(metrics.tab_height)
    .padding([0, 8])
    .align_y(alignment::Vertical::Center)
    .clip(true);
    let title = mouse_area(title)
        .on_press(Message::TabPressed(tab))
        .interaction(iced::mouse::Interaction::Pointer);
    let focused = active && app.workspace.focused_tab() == tab;
    let tab: Element<'_, Message> = container(row![
        title,
        chrome::icon_button(
            *metrics,
            Icon::Close,
            "Close tab",
            Some(Message::CloseTab(tab))
        )
    ])
    .width(width)
    .height(metrics.tab_height)
    .style(move |theme| style::tab(theme, active))
    .into();
    if focused {
        iced::widget::stack![
            tab,
            container(
                container(Space::new().width(Length::Fill).height(style::FOCUS_WIDTH))
                    .style(style::tab_focus)
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .align_y(alignment::Vertical::Bottom)
        ]
        .width(width)
        .height(metrics.tab_height)
        .into()
    } else {
        tab
    }
}

pub fn chip_padding() -> iced::Padding {
    iced::Padding::from([4, 8])
}

#[derive(Debug, Clone)]
pub struct MenuEntry {
    pub label: String,
    pub action: Option<Message>,
}

#[must_use]
pub fn menu_entries(app: &App, group: GroupId, menu: StripMenu) -> Vec<MenuEntry> {
    let Some(g) = app.workspace.group(&group) else {
        return vec![];
    };
    match menu {
        StripMenu::Tabs => g
            .tabs()
            .iter()
            .filter_map(|tab| {
                app.workspace.tab(tab).map(|v| MenuEntry {
                    label: format!(
                        "{}{}",
                        if *tab == g.active() { "●  " } else { "   " },
                        v.title()
                    ),
                    action: Some(Message::ActivateTab(*tab)),
                })
            })
            .collect(),
        StripMenu::NewView => [
            ("Empty tab", "workspace.new_tab"),
            ("Notes list", "workspace.open_notes_view"),
            ("Graph", "workspace.open_graph"),
            ("Preview", "workspace.open_preview"),
            ("Backlinks", "workspace.open_backlinks"),
            ("Outline", "workspace.open_outline"),
            ("Properties", "workspace.open_properties"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: crate::host::commands::available(app, id)
                .then_some(Message::RunHostCommand(id)),
        })
        .collect(),
        StripMenu::Actions => [
            ("Duplicate tab", "workspace.duplicate_tab"),
            ("Split right", "workspace.split_right"),
            ("Split down", "workspace.split_down"),
            (
                if app.maximized_group().is_some() {
                    "Restore group"
                } else {
                    "Maximize group"
                },
                "workspace.maximize_group",
            ),
            ("Move tab to next group", "workspace.move_tab_next_group"),
            ("Merge next group", "workspace.merge_next_group"),
            ("Close group", "workspace.close_group"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: crate::host::commands::available(app, id)
                .then_some(Message::RunHostCommand(id)),
        })
        .collect(),
        // ---- W10 standard menu bar ----
        StripMenu::File => [
            ("New note…", "workspace.new_note"),
            ("Open note…", "workspace.open_note_dialog"),
            ("Save note", "note.save"),
            ("Rename or move…", "workspace.rename_note"),
            ("Note history", "workspace.open_history"),
            ("Close tab", "workspace.close_tab"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: menu_action(app, id),
        })
        .collect(),
        StripMenu::Edit => [
            ("Undo", "buffer.undo"),
            ("Redo", "buffer.redo"),
            ("Find in note", "workspace.find"),
            ("Command palette", "workspace.command_palette"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: menu_action(app, id),
        })
        .collect(),
        StripMenu::Selection => [
            ("Select all", "workspace.select_all"),
            ("Move caret to start", "workspace.caret_start"),
            ("Move caret to end", "workspace.caret_end"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: menu_action(app, id),
        })
        .collect(),
        StripMenu::View => [
            ("Writing layout", "workspace.writing_layout"),
            ("Toggle notes sidebar", "workspace.toggle_sidebar"),
            ("Notes list", "workspace.open_notes_view"),
            ("Search", "workspace.open_search_view"),
            ("Graph", "workspace.open_graph"),
            ("Preview", "workspace.open_preview"),
            ("Backlinks", "workspace.open_backlinks"),
            ("Outline", "workspace.open_outline"),
            ("Properties", "workspace.open_properties"),
            ("New tab", "workspace.new_tab"),
            ("Split right", "workspace.split_right"),
            ("Split down", "workspace.split_down"),
            ("Maximize group", "workspace.maximize_group"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: menu_action(app, id),
        })
        .collect(),
        StripMenu::Go => [
            ("Quick switch", "workspace.quick_switch"),
            ("Navigate back", "workspace.navigate_back"),
            ("Navigate forward", "workspace.navigate_forward"),
            ("Open note in this tab…", "workspace.open_in_tab"),
            ("Next tab", "workspace.next_tab"),
            ("Previous tab", "workspace.previous_tab"),
            ("Next group", "workspace.next_group"),
            ("Previous group", "workspace.previous_group"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: menu_action(app, id),
        })
        .collect(),
        StripMenu::Graph => [
            ("Open graph", "workspace.open_graph"),
            ("Freeze layout", "graph.freeze"),
            ("Fit to view", "graph.fit"),
            ("List view", "graph.list"),
            ("Filters", "graph.filters"),
            ("Open selected note", "graph.open_selected"),
            ("Insert selected link", "graph.insert_selected"),
            ("Clear selection", "graph.clear_selection"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: menu_action(app, id),
        })
        .collect(),
        StripMenu::Tools => [
            ("Rename or move…", "workspace.rename_note"),
            ("Note history", "workspace.open_history"),
            ("Refresh catalog", "workspace.refresh_catalog"),
            ("Save workspace layout", "workspace.save_layout"),
            ("Reset workspace layout", "workspace.reset_layout"),
        ]
        .into_iter()
        .map(|(label, id)| MenuEntry {
            label: label.into(),
            action: menu_action(app, id),
        })
        .collect(),
        StripMenu::Help => [("About Flavite", "help.about")]
            .into_iter()
            .map(|(label, id)| MenuEntry {
                label: label.into(),
                action: menu_action(app, id),
            })
            .collect(),
    }
}

/// Resolve one menu entry through the shared catalog: UI commands dispatch
/// as host commands, core command ids as typed invocations (D-16).
fn menu_action(app: &App, id: &'static str) -> Option<Message> {
    if !crate::host::commands::available(app, id) {
        return None;
    }
    Some(match id {
        "note.save" | "buffer.undo" | "buffer.redo" => Message::RunCoreCommand(id),
        "help.about" => Message::AboutDialog,
        "workspace.new_note" => Message::NewNoteDialog,
        "workspace.open_note_dialog" => Message::OpenNoteDialog,
        "workspace.rename_note" => Message::RenameDialog,
        "workspace.refresh_catalog" => Message::RefreshCatalog,
        _ => Message::RunHostCommand(id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_active_tab_is_visible_at_narrow_and_wide_widths() {
        for width in [200.0, 240.0, 320.0, 640.0, 1200.0] {
            for count in 1..=128 {
                for active in 0..count {
                    let p = plan(width, CONTROLS_WIDTH, count, active);
                    assert!(p.start <= active && active < p.start + p.count);
                    #[allow(clippy::cast_precision_loss)]
                    let occupied = p.count as f32 * p.tab_width + CONTROLS_WIDTH;
                    assert!(occupied <= width + 0.01);
                }
            }
        }
    }
}
