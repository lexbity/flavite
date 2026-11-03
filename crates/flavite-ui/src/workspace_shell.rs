//! Workspace shell rendering (RFC-0003 §13.1).
//!
//! Chrome, tab groups in the pane grid, source-reading note views, the
//! Notes list, the note picker, and the command palette. Styles come from
//! [`crate::style`]; every surface reads the theme palette.

use crate::app::{App, Message};
use crate::style;
use crate::workspace::{bounds, tab_strip};
use flavite_workspace::{GroupId, LayoutAction, NoteLoad, TabId, ViewKind};
use iced::widget::{
    Space, button, column, container, opaque, pane_grid, row, rule, scrollable, stack, text,
    text_input,
};
use iced::{Element, Length, Padding, alignment};

/// Widget id of the palette query input.
pub const PALETTE_INPUT_ID: &str = "palette-query-input";
/// Widget id of the note picker query input.
pub const PICKER_INPUT_ID: &str = "picker-query-input";

/// Draw the full shell: chrome, workspace region, status bar, overlays.
#[must_use]
pub fn view(app: &App) -> Element<'_, Message> {
    // No vault yet: the start screen owns the window (FR-22, W10). It
    // offers real open/create actions; no demo content ever appears.
    if app.flavite.is_none() {
        return crate::host::dialogs::start_view(app);
    }
    let shell = shell(app);
    if app.strip_menu.is_some() && !app.picker.open && !app.palette_open {
        return menu_overlay(app, shell);
    }
    let overlay = if app.picker.open {
        Some(picker_card(app))
    } else if app.palette_open {
        Some(palette_card(app))
    } else if app.dialog.is_some() {
        Some(crate::host::dialogs::view(app))
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
        } else if app.picker.open {
            Message::PickerClose
        } else {
            Message::DialogCancel
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
    let workspace = column![
        top_bar(app),
        rule::horizontal(1),
        bounds::workspace(workspace_region(app), app),
        rule::horizontal(1),
        status_bar(app),
    ]
    .width(Length::Fill)
    .height(Length::Fill);
    // Notifications are presentation, not content: they float over the
    // workspace and never consume layout rows (RFC W04).
    if let Some(notifications) = crate::views::notifications::overlay(app) {
        stack![
            Element::from(workspace),
            container(notifications)
                .width(Length::Fill)
                .align_y(alignment::Vertical::Top)
                .padding(Padding {
                    top: 36.0,
                    ..Padding::default()
                })
        ]
        .into()
    } else {
        workspace.into()
    }
}

fn top_bar(app: &App) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    let vault_name = app.root().file_name().map_or_else(
        || app.root().display().to_string(),
        |name| name.to_string_lossy().to_string(),
    );
    container(
        row![
            button(text("Flavite").size(metrics.brand))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::AppMenu),
            text(vault_name)
                .size(metrics.text_sm)
                .style(style::dim_text),
            Space::new()
                .width(Length::Fixed(8.0))
                .height(Length::Fixed(0.0)),
            menu_bar(app),
            Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
            button(text("Quick switch").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::PickerOpen),
            button(text("Commands").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::PaletteOpen),
        ]
        .spacing(metrics.spacing)
        .align_y(alignment::Vertical::Center),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 6.0,
        bottom: 6.0,
        left: metrics.spacing_lg,
        right: metrics.spacing_lg,
    })
    .into()
}

/// The standard sorted menu bar (W10): every entry resolves through the
/// shared host-command catalog availability, so menu, palette, keyboard,
/// and accessibility surfaces cannot drift (D-16).
fn menu_bar(app: &App) -> Element<'_, Message> {
    const MENUS: [(&str, crate::workspace::StripMenu); 8] = [
        ("File", crate::workspace::StripMenu::File),
        ("Edit", crate::workspace::StripMenu::Edit),
        ("Selection", crate::workspace::StripMenu::Selection),
        ("View", crate::workspace::StripMenu::View),
        ("Go", crate::workspace::StripMenu::Go),
        ("Graph", crate::workspace::StripMenu::Graph),
        ("Tools", crate::workspace::StripMenu::Tools),
        ("Help", crate::workspace::StripMenu::Help),
    ];
    let metrics = app.appearance.metrics();
    let mut bar = row![].spacing(metrics.spacing_sm);
    for (label, menu) in MENUS {
        let active = app.strip_menu.is_some_and(|(_, open)| open == menu);
        bar = bar.push(
            button(text(label).size(metrics.text_sm))
                .padding(Padding {
                    top: 2.0,
                    bottom: 2.0,
                    left: 6.0,
                    right: 6.0,
                })
                .style(move |theme, status| {
                    if active {
                        style::list_row(true)(theme, status)
                    } else {
                        style::ghost()(theme, status)
                    }
                })
                .on_press(Message::TopMenu(menu)),
        );
    }
    bar.into()
}

fn workspace_region(app: &App) -> Element<'_, Message> {
    let available = flavite_workspace::Size {
        width: app.workspace_bounds.width,
        height: app.workspace_bounds.height,
    };
    let geometry = flavite_workspace::solve(
        &app.workspace,
        available,
        app.appearance.metrics().scale,
        &app.resize_preview,
    );
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
    let metrics = app.appearance.metrics();
    let mut switcher = row![].spacing(metrics.spacing_sm);
    for group in app.workspace.group_order() {
        let focused = app.focused_group() == Some(group);
        switcher = switcher.push(
            button(text(group_label(app, group)).size(metrics.text_sm))
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
            text("Groups").size(metrics.text_sm).style(style::dim_text),
            scrollable(switcher).direction(iced::widget::scrollable::Direction::Horizontal(
                iced::widget::scrollable::Scrollbar::default()
            ))
        ]
        .spacing(metrics.spacing),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 6.0,
        bottom: 6.0,
        left: metrics.spacing_lg,
        right: metrics.spacing_lg,
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
        pane_grid::Content::new(group_panel(app, group, focused == Some(group)))
            .style(move |theme| style::pane(theme, focused == Some(group)))
    })
    .spacing(flavite_workspace::SPLITTER_WIDTH * app.appearance.metrics().scale);
    container(grid)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn group_panel(app: &App, group: GroupId, _focused: bool) -> Element<'_, Message> {
    let Some(g) = app.workspace.group(&group) else {
        return column![].into();
    };
    let metrics = app.appearance.metrics();
    let panel: Element<'_, Message> = column![
        tab_strip::view(app, group),
        rule::horizontal(1),
        active_view(app, group, g.active())
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into();
    let panel = if let Some(target) = app.gesture.target().filter(|t| t.group == group) {
        let rect = app.group_bounds.get(&group).copied().unwrap_or_default();
        let active = g.tabs().iter().position(|t| *t == g.active()).unwrap_or(0);
        let slots = tab_strip::plan(rect.width, metrics.controls_width, g.tabs().len(), active);
        // Drop-target geometry tracks the scaled tab strip chrome.
        let tab_height = metrics.tab_height;
        #[allow(clippy::cast_precision_loss)]
        let (x, y, w, h) = match target.edge {
            Some(flavite_workspace::Edge::Left) => {
                (0.0, tab_height, rect.width / 2.0, rect.height - tab_height)
            }
            Some(flavite_workspace::Edge::Right) => (
                rect.width / 2.0,
                tab_height,
                rect.width / 2.0,
                rect.height - tab_height,
            ),
            Some(flavite_workspace::Edge::Top) => (
                0.0,
                tab_height,
                rect.width,
                (rect.height - tab_height) / 2.0,
            ),
            Some(flavite_workspace::Edge::Bottom) => (
                0.0,
                tab_height + (rect.height - tab_height) / 2.0,
                rect.width,
                (rect.height - tab_height) / 2.0,
            ),
            None => (
                (target.index.saturating_sub(slots.start) as f32 * slots.tab_width)
                    .min(rect.width - metrics.controls_width),
                0.0,
                3.0,
                tab_height,
            ),
        };
        stack![
            panel,
            iced::widget::pin(
                container(Space::new().width(w.max(0.0)).height(h.max(0.0))).style(move |theme| {
                    style::drop_target(
                        theme,
                        app.gesture
                            .dragged_tab()
                            .is_some_and(|tab| app.drop_allowed(tab, target)),
                    )
                })
            )
            .x(x)
            .y(y)
        ]
        .into()
    } else {
        panel
    };
    bounds::observe(panel, move |rect| Message::GroupBounds(group, rect))
}

fn menu_overlay<'a>(app: &'a App, shell: Element<'a, Message>) -> Element<'a, Message> {
    let Some((group, menu)) = app.strip_menu else {
        return shell;
    };
    let metrics = app.appearance.metrics();
    let width = 256.0_f32.min(app.window_size().width);
    let (x, y) = if menu == crate::workspace::StripMenu::App {
        (metrics.spacing_lg, 36.0)
    } else {
        let rect = app
            .group_bounds
            .get(&group)
            .copied()
            .unwrap_or(app.workspace_bounds);
        let x =
            (rect.x + rect.width - width).clamp(0.0, (app.window_size().width - width).max(0.0));
        let y = (rect.y + metrics.tab_height).min((app.window_size().height - 80.0).max(0.0));
        (x, y)
    };
    let mut entries = column![].spacing(2);
    for (i, entry) in tab_strip::menu_entries(app, group, menu)
        .into_iter()
        .enumerate()
    {
        entries = entries.push(
            button(text(entry.label).size(metrics.text))
                .width(Length::Fill)
                .padding([7, 10])
                .style(style::list_row(i == app.menu_selected))
                .on_press_maybe(entry.action.map(|_| Message::MenuPick(i))),
        );
    }
    let card = container(scrollable(entries).height(Length::Shrink))
        .width(width)
        .max_height((app.window_size().height - y - 8.0).min(360.0))
        .padding(4)
        .style(style::palette_card);
    stack![
        shell,
        opaque(
            button(Space::new().width(Length::Fill).height(Length::Fill))
                .style(style::menu_backdrop)
                .on_press(Message::MenuClose)
        ),
        iced::widget::pin(opaque(card)).x(x).y(y)
    ]
    .into()
}

fn active_view(app: &App, group: GroupId, tab: TabId) -> Element<'_, Message> {
    let metrics = *app.appearance.metrics();
    let Some(session) = app.workspace.tab(&tab) else {
        return crate::views::empty_state(metrics, "Empty", "This tab has no content.");
    };
    match session.kind() {
        ViewKind::Empty => crate::views::empty_state(
            metrics,
            "Empty",
            "Open a note with Quick switch (Ctrl+P) or add a Notes list.",
        ),
        ViewKind::Notes => crate::views::notes::view(app, tab, group),
        ViewKind::Search => crate::views::search::view(app, tab),
        ViewKind::History(locator) => crate::views::history::view(app, tab, locator),
        ViewKind::Note(note) => crate::views::note::view(app, tab, note.load(), note.locator()),
        ViewKind::Graph(_) => crate::views::graph::view(app, tab),
        ViewKind::Preview(_) => crate::views::preview::view(app, tab),
        ViewKind::Backlinks(_) => crate::views::backlinks::view(app, tab),
        ViewKind::Outline(_) => crate::views::outline::view(app, tab),
        ViewKind::Properties(_) => crate::views::properties::view(app, tab),
    }
}

fn status_bar(app: &App) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    let mut right = row![].spacing(metrics.spacing_sm);
    right = right.push(
        container(text(app.writing_target_label()).size(metrics.text_sm))
            .style(|theme| style::chip(theme, style::Tone::Primary))
            .padding(tab_strip::chip_padding()),
    );
    if let Some(state) = focused_note_state(app) {
        right = right.push(
            container(text(state).size(metrics.text_sm))
                .style(move |theme| style::chip(theme, style::Tone::Neutral))
                .padding(tab_strip::chip_padding()),
        );
    }
    let pending = app.pending_operations();
    if pending > 0 {
        right = right.push(
            container(text(format!("{pending} in flight")).size(metrics.text_sm))
                .style(|theme| style::chip(theme, style::Tone::Warning))
                .padding(tab_strip::chip_padding()),
        );
    }
    let displaced = app.operation_history_overflowed();
    if displaced > 0 {
        right = right.push(
            container(
                text(format!("{displaced} older outcomes outside history")).size(metrics.text_sm),
            )
            .style(|theme| style::chip(theme, style::Tone::Warning))
            .padding(tab_strip::chip_padding()),
        );
    }
    if let Some((done, total)) = app.reconcile_progress() {
        right = right.push(
            container(text(format!("Indexing {done}/{total}")).size(metrics.text_sm))
                .style(|theme| style::chip(theme, style::Tone::Warning))
                .padding(tab_strip::chip_padding()),
        );
    }
    container(
        row![
            text(app.status_text())
                .size(metrics.text_sm)
                .width(Length::Fill),
            right,
        ]
        .spacing(metrics.spacing),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 6.0,
        bottom: 6.0,
        left: metrics.spacing_lg,
        right: metrics.spacing_lg,
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
    let metrics = app.appearance.metrics();
    let matches = app.palette_snapshot();
    let selected = app.palette_selection(matches.len());
    let mut rows = column![].spacing(2);
    for (index, spec) in matches.iter().enumerate() {
        let is_selected = index == selected;
        rows = rows.push(
            button(
                row![
                    text(spec.title).width(Length::Fill),
                    text(spec.id).size(metrics.text_sm).style(style::dim_text),
                    text(crate::palette::keybinding(spec).unwrap_or_default())
                        .size(metrics.text_sm)
                        .style(style::dim_text),
                ]
                .spacing(metrics.spacing),
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
                .on_submit(Message::PaletteSubmit)
                .size(metrics.text_lg)
                .padding(8),
            rule::horizontal(1),
            container(body).max_height(360).width(Length::Fill),
            row![
                Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                text("Up/Down navigate · Enter runs · Esc dismisses")
                    .size(metrics.text_sm)
                    .style(style::dim_text),
            ],
        ]
        .spacing(metrics.spacing_sm)
        .width(Length::Fill),
    )
    .width(Length::Fixed(metrics.palette_width))
    .padding(metrics.spacing)
    .style(style::palette_card)
    .into()
}

/// The floating note picker card (quick switch).
fn picker_card(app: &App) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
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
                        .size(metrics.text_sm)
                        .style(style::dim_text),
                ]
                .spacing(metrics.spacing),
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
                .size(metrics.text_lg)
                .padding(8),
            rule::horizontal(1),
            container(body).max_height(360).width(Length::Fill),
            row![
                Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                text("Enter opens · Ctrl+Enter inserts a link at the caret · Esc dismisses")
                    .size(metrics.text_sm)
                    .style(style::dim_text),
            ],
        ]
        .spacing(metrics.spacing_sm)
        .width(Length::Fill),
    )
    .width(Length::Fixed(metrics.palette_width))
    .padding(metrics.spacing)
    .style(style::palette_card)
    .into()
}
