//! Tab strip rendering for one group (RFC-0003 §13.1).
//!
//! The strip is the pane-grid title bar: active tab, overflow list, new-view
//! row, and group actions. Every control is a real button so keyboard and
//! assistive-technology users operate the same actions the pointer does
//! (FR-02, FR-18).

use crate::app::Message;
use crate::style;
use flavite_workspace::{GroupId, TabId, WorkspaceState};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length, Padding};

/// Height of the tab strip (§13.1: tab height 32 px).
pub const TAB_HEIGHT: f32 = 32.0;

/// How many tabs render inline before the overflow list takes over.
const INLINE_TAB_BUDGET: usize = 8;

/// What an inline secondary row of the strip is showing, if anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripMenu {
    /// The new-view row (Empty tab / Notes list).
    NewView,
    /// The group actions row (move/merge/duplicate/close/maximize).
    Actions,
}

/// Render the tab strip for one group.
#[allow(clippy::too_many_arguments)]
pub fn view(
    state: &WorkspaceState,
    group: GroupId,
    focused: bool,
    inline_menu: Option<StripMenu>,
    has_notes_catalog: bool,
) -> Element<'_, Message> {
    let Some(group_state) = state.group(&group) else {
        return column![].into();
    };
    let tabs = group_state.tabs();
    let active = group_state.active();
    let inline_count = tabs.len().min(INLINE_TAB_BUDGET);

    let mut strip = row![].spacing(2);
    for tab in tabs.iter().take(inline_count) {
        strip = strip.push(tab_button(state, *tab, *tab == active));
    }
    if tabs.len() > inline_count {
        strip = strip.push(
            button(text(format!("+{}", tabs.len() - inline_count)).size(style::TEXT_SM))
                .padding(chip_padding())
                .style(style::ghost())
                .on_press_maybe(Some(Message::GroupMenu(group, StripMenu::Actions))),
        );
    }
    strip = strip.push(
        button(text("+").size(style::TEXT))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press(Message::GroupMenu(group, StripMenu::NewView)),
    );
    strip = strip.push(
        button(text("⋯").size(style::TEXT))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press(Message::GroupMenu(group, StripMenu::Actions)),
    );

    let mut content = column![container(strip).height(Length::Fixed(TAB_HEIGHT))]
        .spacing(2)
        .width(Length::Fill);
    if focused && inline_menu == Some(StripMenu::NewView) {
        content = content.push(new_view_row(has_notes_catalog));
    }
    if focused && inline_menu == Some(StripMenu::Actions) {
        content = content.push(actions_row(state, group));
    }
    content.into()
}

fn tab_button(state: &WorkspaceState, tab: TabId, active: bool) -> Element<'_, Message> {
    let Some(session) = state.tab(&tab) else {
        return column![].into();
    };
    let title = session.title();
    let load_marker = match session.kind() {
        flavite_workspace::ViewKind::Note(note) => match note.load() {
            flavite_workspace::NoteLoad::Reserved => " …",
            flavite_workspace::NoteLoad::Ready => "",
            flavite_workspace::NoteLoad::Failed => " !",
        },
        _ => "",
    };
    row![
        button(text(format!("{title}{load_marker}")).size(style::TEXT_SM))
            .padding(Padding {
                top: 5.0,
                bottom: 5.0,
                left: 8.0,
                right: 8.0,
            })
            .style(style::list_row(active))
            .on_press(Message::Layout(
                flavite_workspace::LayoutAction::ActivateTab {
                    expected_revision: state.revision(),
                    tab,
                }
            )),
        button(text("×").size(style::TEXT_SM))
            .padding(Padding {
                top: 4.0,
                bottom: 4.0,
                left: 6.0,
                right: 6.0,
            })
            .style(style::ghost())
            .on_press(Message::Layout(flavite_workspace::LayoutAction::CloseTab {
                expected_revision: state.revision(),
                tab,
            })),
    ]
    .spacing(0)
    .into()
}

fn new_view_row(has_notes_catalog: bool) -> Element<'static, Message> {
    row![
        button(text("Empty tab").size(style::TEXT_SM))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press(Message::NewViewInFocused(
                flavite_workspace::ViewKind::Empty
            )),
        button(text("Notes list").size(style::TEXT_SM))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press_maybe(has_notes_catalog.then_some(Message::NewViewInFocused(
                flavite_workspace::ViewKind::Notes
            ))),
    ]
    .spacing(style::SPACING_SM)
    .into()
}

fn actions_row(state: &WorkspaceState, group: GroupId) -> Element<'_, Message> {
    let revision = state.revision();
    row![
        button(text("Duplicate tab").size(style::TEXT_SM))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press_maybe(
                state
                    .group(&group)
                    .map(|g| g.active())
                    .map(
                        |active| Message::Layout(flavite_workspace::LayoutAction::DuplicateTab {
                            expected_revision: revision,
                            tab: active,
                        })
                    ),
            ),
        button(text("Split right").size(style::TEXT_SM))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press(Message::Layout(
                flavite_workspace::LayoutAction::SplitGroup {
                    expected_revision: revision,
                    group,
                    arrangement: flavite_workspace::Arrangement::SideBySide,
                },
            )),
        button(text("Split down").size(style::TEXT_SM))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press(Message::Layout(
                flavite_workspace::LayoutAction::SplitGroup {
                    expected_revision: revision,
                    group,
                    arrangement: flavite_workspace::Arrangement::Stacked,
                },
            )),
        button(text("Maximize").size(style::TEXT_SM))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press(Message::Layout(
                flavite_workspace::LayoutAction::ToggleMaximize {
                    expected_revision: revision,
                    group,
                },
            )),
        button(text("Close group").size(style::TEXT_SM))
            .padding(chip_padding())
            .style(style::ghost())
            .on_press(Message::Layout(
                flavite_workspace::LayoutAction::CloseGroup {
                    expected_revision: revision,
                    group,
                },
            )),
    ]
    .spacing(style::SPACING_SM)
    .wrap()
    .into()
}

/// Scrollable overflow list for groups with many tabs.
pub fn overflow_list(state: &WorkspaceState, group: GroupId) -> Option<Element<'_, Message>> {
    let group_state = state.group(&group)?;
    if group_state.tabs().len() <= INLINE_TAB_BUDGET {
        return None;
    }
    let mut rows = column![].spacing(2);
    for tab in group_state.tabs() {
        let Some(session) = state.tab(tab) else {
            continue;
        };
        let active = *tab == group_state.active();
        rows = rows.push(
            button(
                text(session.title())
                    .size(style::TEXT_SM)
                    .width(Length::Fill),
            )
            .width(Length::Fill)
            .padding(chip_padding())
            .style(style::list_row(active))
            .on_press(Message::Layout(
                flavite_workspace::LayoutAction::ActivateTab {
                    expected_revision: state.revision(),
                    tab: *tab,
                },
            )),
        );
    }
    Some(
        container(scrollable(rows).height(Length::Fill))
            .max_height(220)
            .width(Length::Fill)
            .padding(style::SPACING_SM)
            .style(style::card)
            .into(),
    )
}

/// Padding shared by chip-sized strip controls.
pub fn chip_padding() -> Padding {
    Padding {
        top: 4.0,
        bottom: 4.0,
        left: 8.0,
        right: 8.0,
    }
}
