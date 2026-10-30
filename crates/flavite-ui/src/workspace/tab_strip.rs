//! Width-aware tab strip and the items shared by pointer/keyboard menus.
use crate::workspace::chrome::{self, Icon};
use crate::{
    app::{App, Message},
    style,
};
use flavite_workspace::{GroupId, TabId, ViewKind};
use iced::widget::{Space, container, mouse_area, row, text};
use iced::{Element, Length, alignment};

pub const TAB_HEIGHT: f32 = 32.0;
pub const CONTROLS_WIDTH: f32 = 96.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripMenu {
    NewView,
    Actions,
    Tabs,
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
pub fn plan(width: f32, count: usize, active: usize) -> StripPlan {
    let available = (width - CONTROLS_WIDTH).max(0.0);
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
    let width = app.group_bounds.get(&group).map_or(400.0, |r| r.width);
    let active_index = g.tabs().iter().position(|t| *t == g.active()).unwrap_or(0);
    let slots = plan(width, g.tabs().len(), active_index);
    let mut tabs = row![].spacing(0);
    for tab in g.tabs().iter().skip(slots.start).take(slots.count) {
        tabs = tabs.push(tab_view(app, *tab, *tab == g.active(), slots.tab_width));
    }
    tabs = tabs.push(Space::new().width(Length::Fill));
    tabs = tabs.push(chrome::icon_button(
        Icon::Tabs,
        "All tabs",
        Some(Message::GroupMenu(group, StripMenu::Tabs)),
    ));
    tabs = tabs.push(chrome::icon_button(
        Icon::Add,
        "New view",
        Some(Message::GroupMenu(group, StripMenu::NewView)),
    ));
    tabs = tabs.push(chrome::icon_button(
        Icon::More,
        "Group actions",
        Some(Message::GroupMenu(group, StripMenu::Actions)),
    ));
    container(tabs.align_y(alignment::Vertical::Center))
        .width(Length::Fill)
        .height(TAB_HEIGHT)
        .style(style::toolbar)
        .into()
}

fn tab_view(app: &App, tab: TabId, active: bool, width: f32) -> Element<'_, Message> {
    let Some(view) = app.workspace.tab(&tab) else {
        return row![].into();
    };
    let kind = match view.kind() {
        ViewKind::Notes => Icon::Notes,
        _ => Icon::Note,
    };
    let label = view.title();
    let title = container(
        row![
            chrome::icon(kind),
            text(label)
                .size(style::TEXT_SM)
                .wrapping(iced::widget::text::Wrapping::None)
        ]
        .spacing(6)
        .align_y(alignment::Vertical::Center),
    )
    .width((width - 32.0).max(0.0))
    .height(TAB_HEIGHT)
    .padding([0, 8])
    .align_y(alignment::Vertical::Center)
    .clip(true);
    let title = mouse_area(title)
        .on_press(Message::TabPressed(tab))
        .interaction(iced::mouse::Interaction::Pointer);
    container(row![
        title,
        chrome::icon_button(Icon::Close, "Close tab", Some(Message::CloseTab(tab)))
    ])
    .width(width)
    .height(TAB_HEIGHT)
    .style(move |theme| style::tab(theme, active))
    .into()
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
            ("Preview", "workspace.open_preview"),
            ("Backlinks", "workspace.open_backlinks"),
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_active_tab_is_visible_at_narrow_and_wide_widths() {
        for width in [200.0, 240.0, 320.0, 640.0, 1200.0] {
            for count in 1..=128 {
                for active in 0..count {
                    let p = plan(width, count, active);
                    assert!(p.start <= active && active < p.start + p.count);
                    #[allow(clippy::cast_precision_loss)]
                    let occupied = p.count as f32 * p.tab_width + CONTROLS_WIDTH;
                    assert!(occupied <= width + 0.01);
                }
            }
        }
    }
}
