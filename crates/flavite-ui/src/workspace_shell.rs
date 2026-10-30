//! Workspace shell rendering (RFC-0003 §13.1).
//!
//! Chrome, tab groups in the pane grid, source-reading note views, the
//! Notes list, the note picker, and the command palette. Styles come from
//! [`crate::style`]; every surface reads the theme palette.

use crate::app::{App, Message};
use crate::style;
use crate::workspace::tab_strip;
use flavite_api::queries::NoteCatalogEntry;
use flavite_workspace::{GroupId, LayoutAction, NoteLoad, TabId, ViewKind};
use iced::widget::{
    Space, button, column, container, mouse_area, opaque, pane_grid, row, rule, scrollable, stack,
    text, text_input,
};
use iced::{Element, Font, Length, Padding, alignment};

/// Widget id of the palette query input.
pub const PALETTE_INPUT_ID: &str = "palette-query-input";
/// Widget id of the note picker query input.
pub const PICKER_INPUT_ID: &str = "picker-query-input";

/// Draw the full shell: chrome, workspace region, status bar, overlays.
#[must_use]
pub fn view(app: &App) -> Element<'_, Message> {
    let shell = shell(app);
    let overlay = if app.picker.open {
        Some(picker_card(app))
    } else if app.palette_open {
        Some(palette_card(app))
    } else {
        None
    };
    let Some(overlay) = overlay else {
        return shell;
    };
    let backdrop = button(Space::new().width(Length::Fill).height(Length::Fill))
        .style(style::backdrop)
        .on_press(if app.palette_open {
            Message::PaletteClose
        } else {
            Message::PickerClose
        });
    stack![
        shell,
        opaque(backdrop),
        container(overlay)
            .align_x(alignment::Horizontal::Center)
            .align_y(alignment::Vertical::Top)
            .padding(Padding {
                top: 72.0,
                ..Padding::default()
            })
            .width(Length::Fill)
            .height(Length::Fill)
    ]
    .into()
}

fn shell(app: &App) -> Element<'_, Message> {
    column![
        top_bar(app),
        rule::horizontal(1),
        workspace_region(app),
        rule::horizontal(1),
        status_bar(app),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn top_bar(app: &App) -> Element<'_, Message> {
    let vault_name = app.root().file_name().map_or_else(
        || app.root().display().to_string(),
        |name| name.to_string_lossy().to_string(),
    );
    container(
        row![
            text("Flavite").size(style::BRAND).style(style::accent_text),
            text(vault_name).size(style::TEXT_SM).style(style::dim_text),
            Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
            button(text("Quick switch  Ctrl+P").size(style::TEXT_SM))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::PickerOpen),
            button(text("Commands  Ctrl+Shift+P").size(style::TEXT_SM))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::PaletteOpen),
        ]
        .spacing(style::SPACING)
        .align_y(alignment::Vertical::Center),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 8.0,
        bottom: 8.0,
        left: style::SPACING_LG,
        right: style::SPACING_LG,
    })
    .into()
}

fn workspace_region(app: &App) -> Element<'_, Message> {
    let available = flavite_workspace::Size {
        width: app.window_size().width,
        height: (app.window_size().height - 110.0).max(200.0),
    };
    let geometry = flavite_workspace::solve(&app.workspace, available, 1.0, &app.resize_preview);
    let maximized = app.maximized_group().or_else(|| {
        geometry
            .adaptive_maximize
            .then_some(app.focused_group())
            .flatten()
    });
    let Some(maximized) = maximized else {
        return pane_grid_view(app);
    };
    // D-13: manual and adaptive maximize show one group plus a complete
    // switcher; the tree and its saved ratios are untouched.
    column![group_switcher(app), group_panel(app, maximized, true),]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn group_switcher(app: &App) -> Element<'_, Message> {
    let mut switcher = row![].spacing(style::SPACING_SM);
    for group in app.workspace.group_order() {
        let focused = app.focused_group() == Some(group);
        switcher = switcher.push(
            button(text(group_label(app, group)).size(style::TEXT_SM))
                .padding(tab_strip::chip_padding())
                .style(style::list_row(focused))
                .on_press(Message::Layout(LayoutAction::FocusGroup {
                    expected_revision: app.workspace.revision(),
                    group,
                })),
        );
    }
    container(
        row![
            text("Groups").size(style::TEXT_SM).style(style::dim_text),
            switcher
        ]
        .spacing(style::SPACING),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 6.0,
        bottom: 6.0,
        left: style::SPACING_LG,
        right: style::SPACING_LG,
    })
    .style(style::status_bar)
    .into()
}

fn group_label(app: &App, group: GroupId) -> String {
    let Some(group_state) = app.workspace.group(&group) else {
        return "Group".into();
    };
    let Some(active) = app.workspace.tab(&group_state.active()) else {
        return "Group".into();
    };
    active.title()
}

fn pane_grid_view(app: &App) -> Element<'_, Message> {
    let focused = app.focused_group();
    let grid = pane_grid(&app.adapter.panes, |_pane, content, _maximized| {
        let group = content.group;
        let is_focused = focused == Some(group);
        let body = group_panel(app, group, is_focused);
        pane_grid::Content::new(body)
            .title_bar(
                pane_grid::TitleBar::new(
                    Space::new().width(Length::Fill).height(Length::Fixed(6.0)),
                )
                .style(move |theme| style::pane_title_bar(theme, is_focused)),
            )
            .style(move |theme| style::pane(theme, is_focused))
    })
    .on_drag(Message::PaneDrag)
    .on_resize(8, Message::PaneResize)
    .on_click(Message::PaneClicked)
    .spacing(flavite_workspace::SPLITTER_WIDTH);
    container(grid)
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(Padding {
            top: 4.0,
            bottom: 4.0,
            left: style::SPACING_LG,
            right: style::SPACING_LG,
        })
        .into()
}

fn group_panel(app: &App, group: GroupId, focused: bool) -> Element<'_, Message> {
    let Some(group_state) = app.workspace.group(&group) else {
        return column![].into();
    };
    let active = group_state.active();
    let strip = tab_strip::view(
        &app.workspace,
        group,
        focused,
        app.strip_menu
            .filter(|(menu_group, _)| *menu_group == group)
            .map(|(_, menu)| menu),
        !app.catalog().is_empty() || app.catalog_error().is_none(),
    );
    let overflow = tab_strip::overflow_list(&app.workspace, group);
    let body = mouse_area(active_view(app, group, active))
        .on_move(move |_point| Message::BodyHover(group));
    let mut panel = column![strip, body]
        .width(Length::Fill)
        .height(Length::Fill)
        .spacing(2);
    if let Some(overflow) = overflow {
        panel = panel.push(overflow);
    }
    panel.into()
}

fn active_view(app: &App, group: GroupId, tab: TabId) -> Element<'_, Message> {
    let Some(session) = app.workspace.tab(&tab) else {
        return empty_state("Empty", "This tab has no content.");
    };
    match session.kind() {
        ViewKind::Empty => empty_state(
            "Empty",
            "Open a note with Quick switch (Ctrl+P) or add a Notes list.",
        ),
        ViewKind::Notes => notes_view(app, group),
        ViewKind::Note(note) => note_view(app, tab, note.load(), note.locator()),
    }
}

fn empty_state(title: &str, hint: &str) -> Element<'static, Message> {
    container(
        column![
            text(title.to_owned()).size(style::HEADING),
            text(hint.to_owned()).style(style::dim_text),
        ]
        .spacing(style::SPACING_SM),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(style::SPACING_LG)
    .style(style::content)
    .center_x(Length::Fill)
    .align_y(alignment::Vertical::Top)
    .into()
}

/// One source-reading note tab: an explicitly labeled Source view of the
/// current buffer (editing integrates in W03).
fn note_view<'a>(
    app: &'a App,
    tab: TabId,
    load: NoteLoad,
    locator: &'a flavite_workspace::NoteLocator,
) -> Element<'a, Message> {
    let path = locator.components().join("/");
    match load {
        NoteLoad::Reserved => container(
            column![
                text(format!("Opening {path}…")),
                button(text("Close tab"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::Layout(LayoutAction::CloseTab {
                        expected_revision: app.workspace.revision(),
                        tab,
                    })),
            ]
            .spacing(style::SPACING),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(style::SPACING_LG)
        .style(style::content)
        .into(),
        NoteLoad::Failed => container(
            column![
                text("Note unavailable").size(style::HEADING),
                text(path.clone()).style(style::dim_text),
                row![
                    button(text("Retry"))
                        .padding(tab_strip::chip_padding())
                        .style(style::action())
                        .on_press(Message::OpenNote {
                            locator: locator.clone(),
                            group: app.group_of(tab),
                        }),
                    button(text("Close tab"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::Layout(LayoutAction::CloseTab {
                            expected_revision: app.workspace.revision(),
                            tab,
                        })),
                ]
                .spacing(style::SPACING),
            ]
            .spacing(style::SPACING_SM),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(style::SPACING_LG)
        .style(style::content)
        .into(),
        NoteLoad::Ready => {
            let text_body = app
                .note_state_text(tab)
                .unwrap_or_else(|| "The buffer view is unavailable.".into());
            let meta = row![
                container(text(path).size(style::TEXT_SM))
                    .style(|theme| style::chip(theme, style::Tone::Neutral))
                    .padding(tab_strip::chip_padding()),
                container(text("Source").size(style::TEXT_SM))
                    .style(|theme| style::chip(theme, style::Tone::Primary))
                    .padding(tab_strip::chip_padding()),
            ]
            .spacing(style::SPACING_SM);
            container(
                column![
                    meta,
                    container(scrollable(
                        text(text_body).font(Font::MONOSPACE).size(style::TEXT)
                    ))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(style::SPACING)
                    .style(style::card),
                ]
                .spacing(style::SPACING),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(style::SPACING_LG)
            .style(style::content)
            .into()
        }
    }
}

/// The Notes list view: bounded catalog snapshot with an in-memory filter.
fn notes_view(app: &App, group: GroupId) -> Element<'_, Message> {
    if let Some(error) = app.catalog_error() {
        return container(
            column![
                text("Notes unavailable").size(style::HEADING),
                text(error.to_owned()).style(style::dim_text),
                button(text("Retry"))
                    .padding(tab_strip::chip_padding())
                    .style(style::action())
                    .on_press(Message::RefreshCatalog),
            ]
            .spacing(style::SPACING_SM),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(style::SPACING_LG)
        .style(style::content)
        .into();
    }
    let entries = app.catalog();
    let total = entries.len();
    let mut list = column![].spacing(2);
    for entry in entries.iter().take(500) {
        list = list.push(note_row(entry, group));
    }
    container(
        column![
            row![
                text(format!("{total} notes")).size(style::TEXT_LG),
                Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                button(text("Refresh"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::RefreshCatalog),
            ]
            .spacing(style::SPACING)
            .align_y(alignment::Vertical::Center),
            text("Pick a note to open it in this group")
                .size(style::TEXT_SM)
                .style(style::dim_text),
            container(scrollable(list).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(style::SPACING_SM)
                .style(style::card),
        ]
        .spacing(style::SPACING_SM),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(style::SPACING_LG)
    .style(style::content)
    .into()
}

fn note_row(entry: &NoteCatalogEntry, group: GroupId) -> Element<'_, Message> {
    let Ok(locator) = flavite_workspace::NoteLocator::from_relative(&entry.path) else {
        return text(entry.path.display().to_string())
            .size(style::TEXT_SM)
            .style(style::dim_text)
            .into();
    };
    button(
        row![
            text(entry.title.clone()).width(Length::Fill),
            text(entry.path.display().to_string())
                .size(style::TEXT_SM)
                .style(style::dim_text),
        ]
        .spacing(style::SPACING),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 6.0,
        bottom: 6.0,
        left: 10.0,
        right: 10.0,
    })
    .style(style::list_row(false))
    .on_press(Message::OpenNote {
        locator,
        group: Some(group),
    })
    .into()
}

fn status_bar(app: &App) -> Element<'_, Message> {
    let mut right = row![].spacing(style::SPACING_SM);
    if let Some(state) = focused_note_state(app) {
        right = right.push(
            container(text(state).size(style::TEXT_SM))
                .style(move |theme| style::chip(theme, style::Tone::Neutral))
                .padding(tab_strip::chip_padding()),
        );
    }
    let pending = app.pending_operations();
    if pending > 0 {
        right = right.push(
            container(text(format!("{pending} in flight")).size(style::TEXT_SM))
                .style(|theme| style::chip(theme, style::Tone::Warning))
                .padding(tab_strip::chip_padding()),
        );
    }
    if let Some((done, total)) = app.reconcile_progress() {
        right = right.push(
            container(text(format!("Indexing {done}/{total}")).size(style::TEXT_SM))
                .style(|theme| style::chip(theme, style::Tone::Warning))
                .padding(tab_strip::chip_padding()),
        );
    }
    container(
        row![
            text(app.status_text())
                .size(style::TEXT_SM)
                .width(Length::Fill),
            right,
        ]
        .spacing(style::SPACING),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 6.0,
        bottom: 6.0,
        left: style::SPACING_LG,
        right: style::SPACING_LG,
    })
    .style(style::status_bar)
    .into()
}

fn focused_note_state(app: &App) -> Option<&'static str> {
    let tab = app.workspace.focused_tab();
    let session = app.workspace.tab(&tab)?;
    let ViewKind::Note(note) = session.kind() else {
        return None;
    };
    match note.load() {
        NoteLoad::Reserved => Some("Opening"),
        NoteLoad::Ready => Some("Source"),
        NoteLoad::Failed => Some("Unavailable"),
    }
}

/// The floating command palette card (core + host commands).
fn palette_card(app: &App) -> Element<'_, Message> {
    let matches = app.palette_snapshot();
    let selected = app.palette_selection(matches.len());
    let mut rows = column![].spacing(2);
    for (index, spec) in matches.iter().enumerate() {
        let is_selected = index == selected;
        rows = rows.push(
            button(
                row![
                    text(spec.title).width(Length::Fill),
                    text(spec.id).size(style::TEXT_SM).style(style::dim_text),
                    text(crate::palette::keybinding(spec).unwrap_or_default())
                        .size(style::TEXT_SM)
                        .style(style::dim_text),
                ]
                .spacing(style::SPACING),
            )
            .width(Length::Fill)
            .padding(Padding {
                top: 8.0,
                bottom: 8.0,
                left: 10.0,
                right: 10.0,
            })
            .style(style::list_row(is_selected))
            .on_press(Message::PalettePick(index)),
        );
    }
    let body: Element<'_, Message> = if matches.is_empty() {
        text("No matching commands").style(style::dim_text).into()
    } else {
        scrollable(rows)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    };
    container(
        column![
            text_input("Type a command…", app.palette_query_text())
                .id(PALETTE_INPUT_ID)
                .on_input(Message::PaletteQuery)
                .size(style::TEXT_LG)
                .padding(8),
            rule::horizontal(1),
            container(body).max_height(360).width(Length::Fill),
            row![
                Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                text("Up/Down navigate · Enter runs · Esc dismisses")
                    .size(style::TEXT_SM)
                    .style(style::dim_text),
            ],
        ]
        .spacing(style::SPACING_SM)
        .width(Length::Fill),
    )
    .width(Length::Fixed(style::PALETTE_WIDTH))
    .padding(style::SPACING)
    .style(style::palette_card)
    .into()
}

/// The floating note picker card (quick switch).
fn picker_card(app: &App) -> Element<'_, Message> {
    let matches = app.picker_matches();
    let selected = app.picker.selected.min(matches.len().saturating_sub(1));
    let mut rows = column![].spacing(2);
    for (index, entry) in matches.iter().take(50).enumerate() {
        let is_selected = index == selected;
        rows = rows.push(
            button(
                row![
                    text(entry.title.clone()).width(Length::Fill),
                    text(entry.path.display().to_string())
                        .size(style::TEXT_SM)
                        .style(style::dim_text),
                ]
                .spacing(style::SPACING),
            )
            .width(Length::Fill)
            .padding(Padding {
                top: 8.0,
                bottom: 8.0,
                left: 10.0,
                right: 10.0,
            })
            .style(style::list_row(is_selected))
            .on_press(Message::PickerPick(index)),
        );
    }
    let body: Element<'_, Message> = if app.catalog_error().is_some() {
        text("The note catalog is unavailable; retry from the Notes list.")
            .style(style::dim_text)
            .into()
    } else if matches.is_empty() {
        text(if app.catalog().is_empty() {
            "No notes in this vault yet"
        } else {
            "No matching notes"
        })
        .style(style::dim_text)
        .into()
    } else {
        scrollable(rows).width(Length::Fill).into()
    };
    container(
        column![
            text_input("Open a note…", &app.picker.query)
                .id(PICKER_INPUT_ID)
                .on_input(Message::PickerQuery)
                .size(style::TEXT_LG)
                .padding(8),
            rule::horizontal(1),
            container(body).max_height(360).width(Length::Fill),
            row![
                Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                text("Enter opens · Esc dismisses")
                    .size(style::TEXT_SM)
                    .style(style::dim_text),
            ],
        ]
        .spacing(style::SPACING_SM)
        .width(Length::Fill),
    )
    .width(Length::Fixed(style::PALETTE_WIDTH))
    .padding(style::SPACING)
    .style(style::palette_card)
    .into()
}
