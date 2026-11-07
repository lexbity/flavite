//! The Settings tab (RFC-0007 §4.4): a generated surface rendering every
//! visible descriptor from [`SettingRegistry::builtins()`] with its
//! current effective value and kind-appropriate controls.
//!
//! Layout: a search bar with a found-count and the copy-path affordance,
//! a category table of contents with per-category counts, and the rows
//! themselves — grouped by category in browse mode, flattened while
//! searching. Rows carry a modified marker (`value != default`), a
//! restore-default action, and controls per kind (`Bool` toggle, bounded
//! `Int` input, `Choice`/`LineWidth` segments, `StringList` chips). The
//! two dynamic kinds render through their dedicated sections
//! (`ThemePackage` list, `ShortcutMap` capture rows).

use crate::app::{App, Message};
use crate::host::settings::SettingsMessage;
use crate::picker::shared_matcher;
use crate::style;
use crate::workspace::tab_strip;
use flavite_settings::descriptor::{Apply, SettingDescriptor, SettingKind};
use flavite_settings::registry::SettingRegistry;
use flavite_settings::resolve::Resolved;
use flavite_settings::value::SettingValue;
use flavite_workspace::TabId;
use iced::widget::{button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};

/// Input widget id prefix for semantic tests.
pub const SETTINGS_INPUT_ID_PREFIX: &str = "settings-input-";

/// TOC column width in logical pixels at 100% scale.
const TOC_WIDTH: f32 = 200.0;

/// The widget id of the tab's search input (the `/` focus target).
#[must_use]
pub fn search_input_id(tab: TabId) -> iced::widget::Id {
    input_id(tab, "search")
}

fn input_id(tab: TabId, id: &str) -> iced::widget::Id {
    iced::widget::Id::from(format!("{SETTINGS_INPUT_ID_PREFIX}{}-{id}", tab.raw()))
}

/// One rendered setting row: the presentation model the tab draws and the
/// tests assert on (RFC-0007 S6: the id sweep runs against this).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingRow {
    /// The descriptor id (e.g. `editor.font_size`).
    pub id: &'static str,
    /// The row title.
    pub title: &'static str,
    /// The row description.
    pub description: &'static str,
    /// The category name.
    pub category: &'static str,
    /// The honest read-only display of the current effective value.
    pub value: String,
    /// Whether the effective value differs from the descriptor default.
    pub modified: bool,
    /// Whether a change applies only after a vault reopen.
    pub on_restart: bool,
}

/// The descriptors the tab renders for one tab: registry order, narrowed
/// by the tab's search query (fuzzy over id/title/description/category)
/// and, when browsing, by the tab's selected TOC category.
#[must_use]
pub fn matched_descriptors(app: &App, tab: TabId) -> Vec<SettingDescriptor> {
    let registry = SettingRegistry::with_builtins();
    let visible = registry.visible();
    let matched = match_rows(app.settings.query(tab), &visible);
    let selected_category = app.settings.category(tab);
    let browsing = app.settings.query(tab).trim().is_empty();
    matched
        .into_iter()
        .filter(|descriptor| {
            !(browsing && selected_category.is_some_and(|sel| sel != descriptor.category))
        })
        .cloned()
        .collect()
}

/// The rows the tab renders for one tab, as the plain test model.
#[must_use]
pub fn rows(app: &App, tab: TabId) -> Vec<SettingRow> {
    let registry = SettingRegistry::with_builtins();
    matched_descriptors(app, tab)
        .into_iter()
        .map(|descriptor| {
            let resolved = app.settings.resolved.as_ref();
            let modified =
                resolved.is_some_and(|resolved| resolved.is_modified(descriptor.id, &registry));
            SettingRow {
                id: descriptor.id,
                title: descriptor.title,
                description: descriptor.description,
                category: descriptor.category,
                value: value_display(&descriptor, resolved),
                modified,
                on_restart: descriptor.apply == Apply::OnRestart,
            }
        })
        .collect()
}

/// Render the Settings tab for one tab.
#[allow(clippy::too_many_lines)]
pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    let query = app.settings.query(tab).to_owned();
    let selected_category = app.settings.category(tab).map(str::to_owned);

    let matched = matched_descriptors(app, tab);
    let found = matched.len();

    // Table of contents: "All" plus one entry per category in registry
    // order, each with its count within the current search filter.
    let mut categories: Vec<&str> = Vec::new();
    for descriptor in &matched {
        if !categories.contains(&descriptor.category) {
            categories.push(descriptor.category);
        }
    }
    let toc_button = |label: String, count: usize, active: bool, message| {
        button(
            row![
                text(label).size(metrics.text_sm),
                text(format!(" ({count})"))
                    .size(metrics.text_sm)
                    .style(style::dim_text),
            ]
            .spacing(2),
        )
        .width(Length::Fill)
        .padding(tab_strip::chip_padding())
        .style(move |theme, status| {
            if active {
                style::list_row(true)(theme, status)
            } else {
                style::ghost()(theme, status)
            }
        })
        .on_press(message)
    };
    let mut toc = column![toc_button(
        "All".to_owned(),
        matched.len(),
        selected_category.is_none() && query.trim().is_empty(),
        Message::Settings(SettingsMessage::CategorySelect {
            tab,
            category: String::new(),
        }),
    )]
    .spacing(2)
    .width(Length::Fill);
    for category in &categories {
        let count = matched
            .iter()
            .filter(|descriptor| descriptor.category == *category)
            .count();
        toc = toc.push(toc_button(
            (*category).to_owned(),
            count,
            selected_category.as_deref() == Some(*category) && query.trim().is_empty(),
            Message::Settings(SettingsMessage::CategorySelect {
                tab,
                category: (*category).to_owned(),
            }),
        ));
    }

    // Rows: browse mode groups by category; searching flattens the tree.
    let browsing = query.trim().is_empty();
    let mut body = column![];
    if browsing {
        for category in &categories {
            body = body.push(
                text((*category).to_owned())
                    .size(metrics.text_lg)
                    .width(Length::Fill),
            );
            for descriptor in &matched {
                if descriptor.category != *category {
                    continue;
                }
                body = body.push(setting_row(app, metrics, tab, descriptor));
            }
        }
    } else {
        for descriptor in &matched {
            body = body.push(setting_row(app, metrics, tab, descriptor));
        }
    }
    if matched.is_empty() {
        body = body.push(
            text(if browsing {
                "Every visible setting renders here."
            } else {
                "No settings match."
            })
            .style(style::dim_text),
        );
    }

    // FR-10: an unparseable document is an error card with the last-good
    // backup restore affordance — never a silent reset to defaults.
    if let Some(error) = &app.settings.load_error {
        body = body.push(error_card(metrics, error, app.settings.backup_available));
    }
    for warning in &app.settings.warnings {
        body = body.push(
            text(warning.clone())
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    if app.settings.read_only && app.settings.load_error.is_none() {
        body = body.push(
            text("Read-only: settings edits are refused for this vault.")
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }

    container(
        column![
            row![
                text_input("Search settings…", &query)
                    .id(input_id(tab, "search"))
                    .on_input(move |value| Message::Settings(SettingsMessage::Search {
                        tab,
                        query: value,
                    }))
                    .padding(tab_strip::chip_padding())
                    .size(metrics.text_sm)
                    .width(Length::Fill),
                text(format!("{found} settings"))
                    .size(metrics.text_sm)
                    .style(style::dim_text),
                button(text("⧉").size(metrics.text_sm))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::Settings(SettingsMessage::RevealPath)),
            ]
            .spacing(metrics.spacing_sm),
            row![
                container(toc)
                    .width(Length::Fixed(TOC_WIDTH * metrics.scale))
                    .style(style::card),
                container(scrollable(body).width(Length::Fill))
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .padding(metrics.spacing_sm)
                    .style(style::card),
            ]
            .height(Length::Fill)
            .spacing(metrics.spacing_sm),
        ]
        .spacing(metrics.spacing_sm),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(metrics.spacing_lg)
    .style(style::content)
    .into()
}

/// The FR-10 error card: the load failure and, when a last-good backup
/// exists, the one-click restore.
fn error_card<'a>(
    metrics: &crate::style::Metrics,
    error: &str,
    backup_available: bool,
) -> Element<'a, Message> {
    let mut card = column![
        text("Settings could not be read")
            .size(metrics.text)
            .width(Length::Fill),
        text(error.to_owned())
            .size(metrics.text_sm)
            .style(style::dim_text),
    ]
    .spacing(metrics.spacing_sm);
    if backup_available {
        card = card.push(
            button(text("Restore last-good backup").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press(Message::Settings(SettingsMessage::RestoreBackup)),
        );
    } else {
        card = card.push(
            text("No last-good backup exists; edit the document externally or delete it.")
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    container(card)
        .width(Length::Fill)
        .padding(metrics.spacing_sm)
        .style(style::card)
        .into()
}

/// Fuzzy-match descriptors against the query over id, title, description,
/// and category (the `picker::shared_matcher` pattern). An empty query
/// matches everything, in registry order.
fn match_rows<'a>(
    query: &str,
    visible: &'a [&'a SettingDescriptor],
) -> Vec<&'a SettingDescriptor> {
    let needle = query.trim();
    if needle.is_empty() {
        return visible.to_vec();
    }
    let pattern = Pattern::parse(needle, CaseMatching::Smart, Normalization::Smart);
    let haystacks: Vec<String> = visible
        .iter()
        .map(|descriptor| {
            format!(
                "{} {} {} {}",
                descriptor.id, descriptor.title, descriptor.description, descriptor.category
            )
        })
        .collect();
    let mut matcher = shared_matcher();
    let matched: Vec<&str> = pattern
        .match_list(haystacks.iter().map(String::as_str), &mut matcher)
        .into_iter()
        .map(|(matched, _)| matched)
        .collect();
    drop(matcher);
    haystacks
        .iter()
        .enumerate()
        .filter(|(_, haystack)| matched.contains(&haystack.as_str()))
        .map(|(index, _)| visible[index])
        .collect()
}

/// One setting row: title with modified marker, description, the
/// kind-appropriate control, the apply tag when a change needs a vault
/// reopen, and the restore-default action on modified rows.
#[allow(clippy::too_many_lines)]
fn setting_row<'a>(
    app: &'a App,
    metrics: &crate::style::Metrics,
    tab: TabId,
    descriptor: &SettingDescriptor,
) -> Element<'a, Message> {
    let registry = SettingRegistry::with_builtins();
    let resolved = app.settings.resolved.as_ref();
    let modified = resolved.is_some_and(|resolved| resolved.is_modified(descriptor.id, &registry));
    let mut head = row![text(descriptor.title.to_owned()).size(metrics.text),]
        .spacing(metrics.spacing_sm);
    if modified {
        head = head.push(text("●").size(metrics.text_sm).style(style::dim_text));
        head = head.push(
            button(text("Reset").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::Settings(SettingsMessage::ResetOne {
                    tab,
                    id: descriptor.id.to_owned(),
                })),
        );
    }
    if descriptor.apply == Apply::OnRestart {
        head = head.push(
            text("· applies on restart")
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    let mut body = column![
        head,
        text(descriptor.description.to_owned())
            .size(metrics.text_sm)
            .style(style::dim_text),
    ]
    .spacing(2);
    body = body.push(control(app, metrics, tab, descriptor, resolved));
    if let Some((id, message)) = &app.settings.row_error
        && id == descriptor.id
    {
        body = body.push(
            text(message.clone())
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    container(body)
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 6.0,
            bottom: 6.0,
            left: 10.0,
            right: 10.0,
        })
        .style(style::card)
        .into()
}

/// The kind-appropriate control for one descriptor row. On a read-only
/// vault the controls render; attempts are refused with the
/// `settings_read_only` status (D-11), never hidden.
#[allow(clippy::too_many_lines)]
fn control<'a>(
    app: &'a App,
    metrics: &crate::style::Metrics,
    tab: TabId,
    descriptor: &SettingDescriptor,
    resolved: Option<&Resolved>,
) -> Element<'a, Message> {
    match &descriptor.kind {
        SettingKind::Bool => {
            let current = resolved
                .and_then(|resolved| resolved.get(descriptor.id))
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let label = if current { "On" } else { "Off" };
            button(text(label.to_owned()).size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press(Message::Settings(SettingsMessage::Toggle {
                    tab,
                    id: descriptor.id.to_owned(),
                    value: !current,
                }))
                .into()
        }
        SettingKind::Int { .. } => {
            let draft = app.settings.int_draft(tab, descriptor.id);
            let int_id = descriptor.id.to_owned();
            column![
                text_input("Number", &draft)
                    .id(input_id(tab, descriptor.id))
                    .on_input(move |value| Message::Settings(SettingsMessage::IntDraft {
                        tab,
                        id: int_id.clone(),
                        text: value,
                    }))
                    .on_submit(Message::Settings(SettingsMessage::IntSubmit {
                        tab,
                        id: descriptor.id.to_owned(),
                    }))
                    .padding(tab_strip::chip_padding())
                    .size(metrics.text_sm)
                    .width(Length::Fixed(120.0 * metrics.scale)),
                text(format!(
                    "{}–{}",
                    int_bounds(descriptor).0, int_bounds(descriptor).1
                ))
                .size(metrics.text_sm)
                .style(style::dim_text),
            ]
            .spacing(2)
            .into()
        }
        SettingKind::Choice(options) => {
            let current = resolved
                .and_then(|resolved| resolved.get(descriptor.id))
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default();
            let mut segments = row![].spacing(metrics.spacing_sm);
            for (value, label) in options.iter() {
                let active = *value == current;
                segments = segments.push(
                    button(text((*label).to_owned()).size(metrics.text_sm))
                        .padding(tab_strip::chip_padding())
                        .style(move |theme, status| {
                            if active {
                                style::list_row(true)(theme, status)
                            } else {
                                style::ghost()(theme, status)
                            }
                        })
                        .on_press_maybe((!active).then(|| {
                            Message::Settings(SettingsMessage::Choose {
                                tab,
                                id: descriptor.id.to_owned(),
                                choice: (*value).to_owned(),
                            })
                        })),
                );
            }
            segments.into()
        }
        SettingKind::LineWidth => {
            let current = resolved
                .and_then(|resolved| resolved.get(descriptor.id))
                .map(|value| match value {
                    SettingValue::Str(text) if text == "full" => None,
                    SettingValue::Int(columns) => Some(u16::try_from(*columns).unwrap_or(80)),
                    _ => Some(80),
                });
            let segments: [(Option<u16>, &str); 4] =
                [(Some(80), "80"), (Some(100), "100"), (Some(120), "120"), (None, "Full")];
            let mut row_widgets = row![].spacing(metrics.spacing_sm);
            for (columns, label) in segments {
                let active = current == Some(columns);
                row_widgets = row_widgets.push(
                    button(text(label).size(metrics.text_sm))
                        .padding(tab_strip::chip_padding())
                        .style(move |theme, status| {
                            if active {
                                style::list_row(true)(theme, status)
                            } else {
                                style::ghost()(theme, status)
                            }
                        })
                        .on_press_maybe((!active).then(|| {
                            Message::Settings(SettingsMessage::SetLineWidth { tab, columns })
                        })),
                );
            }
            row_widgets.into()
        }
        SettingKind::StringList => {
            let list = resolved
                .and_then(|resolved| resolved.get(descriptor.id))
                .and_then(|value| value.as_list().cloned())
                .unwrap_or_default();
            let draft = app.settings.list_draft(tab, descriptor.id);
            let mut chips = column![].spacing(2);
            for (index, entry) in list.iter().enumerate() {
                chips = chips.push(
                    row![
                        text(entry.clone()).size(metrics.text_sm),
                        button(text("×").size(metrics.text_sm))
                            .padding(tab_strip::chip_padding())
                            .style(style::ghost())
                            .on_press(Message::Settings(SettingsMessage::SetListRemove {
                                tab,
                                id: descriptor.id.to_owned(),
                                index,
                            })),
                    ]
                    .spacing(metrics.spacing_sm),
                );
            }
            if list.is_empty() {
                chips = chips.push(text("(none)").size(metrics.text_sm).style(style::dim_text));
            }
            let list_id = descriptor.id.to_owned();
            chips = chips.push(
                text_input("Add entry…", &draft)
                    .id(input_id(tab, descriptor.id))
                    .on_input(move |value| Message::Settings(SettingsMessage::ListDraft {
                        tab,
                        id: list_id.clone(),
                        text: value,
                    }))
                    .on_submit(Message::Settings(SettingsMessage::ListSubmit {
                        tab,
                        id: descriptor.id.to_owned(),
                    }))
                    .padding(tab_strip::chip_padding())
                    .size(metrics.text_sm)
                    .width(Length::Fixed(240.0 * metrics.scale)),
            );
            chips.into()
        }
        SettingKind::Text => text(value_display(descriptor, resolved))
            .size(metrics.text_sm)
            .into(),
        // The dynamic kinds get dedicated sections: options resolved from
        // the theme store and the host chord catalog respectively (D-04).
        SettingKind::ThemePackage => theme_package_section(app, metrics, tab),
        SettingKind::ShortcutMap => shortcut_section(app, metrics),
    }
}

/// The pressed (action) or quiet (ghost) style, chosen at call time so
/// option rows can highlight the active choice with one helper.
fn action_or_ghost(
    active: bool,
) -> impl Fn(&iced::Theme, iced::widget::button::Status) -> iced::widget::button::Style + Copy {
    move |theme, status| {
        if active {
            style::action()(theme, status)
        } else {
            style::ghost()(theme, status)
        }
    }
}

/// The current display chord for a command (override or default).
fn chord_label(app: &App, id: &str) -> String {
    crate::host::commands::resolved_chord(id, &app.appearance.shortcut_overrides())
        .map(|chord| flavite_settings::chord::chord_string(&flavite_settings::chord::Chord {
            modifiers: chord.modifiers,
            key: chord.key,
        }))
        .unwrap_or_else(|| "unbound".into())
}

/// One shortcut-editor row: the command id, its effective chord, a
/// capture button, and a reset button when overridden (the dialog-era
/// capture state machine, now tab-scoped state).
#[allow(clippy::too_many_lines)]
fn shortcut_section<'a>(
    app: &'a App,
    metrics: &crate::style::Metrics,
) -> Element<'a, Message> {
    let mut section = column![].spacing(2);
    for (id, _) in crate::host::commands::DEFAULT_CHORDS {
        let capturing = app.settings.capture.as_deref() == Some(*id);
        let overridden = app.appearance.shortcut_overrides().contains_key(*id);
        let mut row = row![
            text((*id).to_owned()).size(metrics.text_sm),
            text(chord_label(app, id)).size(metrics.text),
        ]
        .spacing(metrics.spacing_sm)
        .align_y(iced::alignment::Vertical::Center);
        row = row.push(
            button(
                text(if capturing { "Press keys…" } else { "Change" }).size(metrics.text_sm),
            )
            .padding(tab_strip::chip_padding())
            .style(action_or_ghost(capturing))
            .on_press(Message::Settings(SettingsMessage::BeginShortcutCapture {
                command: (*id).to_owned(),
            })),
        );
        if overridden {
            row = row.push(
                button(text("Reset").size(metrics.text_sm))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(app.reset_shortcut_message((*id).to_owned())),
            );
        }
        section = section.push(row);
    }
    section = section.push(
        button(text("Restore default shortcuts").size(metrics.text_sm))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press(app.restore_shortcuts_message()),
    );
    if let Some(error) = &app.appearance.shortcut_error {
        section = section.push(
            text(error.clone())
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    section.into()
}

/// The theme-package section (xpi themes): the installed list with
/// per-theme select/remove (removal asks first), the install button
/// (native dialog), and the conversion warnings of the active package.
#[allow(clippy::too_many_lines)]
fn theme_package_section<'a>(
    app: &'a App,
    metrics: &crate::style::Metrics,
    tab: TabId,
) -> Element<'a, Message> {
    let appearance = &app.appearance;
    let mut section = column![].spacing(metrics.spacing_sm);
    section = section.push(
        row![
            button(text("Install theme (.xpi)…").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::Settings(SettingsMessage::ThemeInstallPick)),
            button(text("Built-in palettes").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(action_or_ghost(appearance.theme_package_id().is_none()))
                .on_press(Message::Settings(SettingsMessage::ChooseThemePackage {
                    tab,
                    package: None,
                })),
        ]
        .spacing(metrics.spacing_sm),
    );
    if appearance.installed_themes.is_empty() {
        section = section.push(
            text("No theme packages installed. Convert a browser theme with `flavite theme convert`, or install a standard .xpi directly.")
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    for theme in &appearance.installed_themes {
        let active = appearance.theme_package_id().as_deref() == Some(theme.id.as_str());
        let pending = app.settings.pending_uninstall.as_deref() == Some(theme.id.as_str());
        let mut theme_row = row![
            text(if theme.name.is_empty() {
                theme.id.clone()
            } else {
                theme.name.clone()
            })
            .size(metrics.text),
            text(if theme.animated_frame { "animated" } else { "" })
                .size(metrics.text_sm)
                .style(style::dim_text),
        ]
        .spacing(metrics.spacing_sm)
        .align_y(iced::alignment::Vertical::Center);
        theme_row = theme_row.push(
            button(text(if active { "Active" } else { "Use" }).size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(action_or_ghost(active))
                .on_press_maybe((!active).then(|| {
                    Message::Settings(SettingsMessage::ChooseThemePackage {
                        tab,
                        package: Some(theme.id.clone()),
                    })
                })),
        );
        if pending {
            theme_row = theme_row.push(
                text("Remove this package?").size(metrics.text_sm).style(style::dim_text),
            );
            theme_row = theme_row.push(
                button(text("Confirm removal").size(metrics.text_sm))
                    .padding(tab_strip::chip_padding())
                    .style(style::action())
                    .on_press(Message::Settings(SettingsMessage::ThemeUninstallConfirm { tab })),
            );
            theme_row = theme_row.push(
                button(text("Keep").size(metrics.text_sm))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::Settings(SettingsMessage::ThemeUninstallCancel { tab })),
            );
        } else {
            theme_row = theme_row.push(
                button(text("Remove").size(metrics.text_sm))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::Settings(SettingsMessage::ThemeUninstallAsk {
                        tab,
                        id: theme.id.clone(),
                    })),
            );
        }
        section = section.push(theme_row);
    }
    if let Some(package) = appearance.theme_package.as_ref()
        && !package.warnings.is_empty()
    {
        section = section.push(
            text(package.warnings.join("; "))
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    if let Some(error) = &appearance.theme_error {
        section = section.push(
            text(error.clone())
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    section.into()
}

/// The inclusive bounds of an `Int` descriptor.
fn int_bounds(descriptor: &SettingDescriptor) -> (i64, i64) {
    match descriptor.kind {
        SettingKind::Int { min, max } => (min, max),
        _ => (0, 0),
    }
}

/// The honest read-only display of a descriptor's current effective value.
#[allow(clippy::too_many_lines)]
fn value_display(descriptor: &SettingDescriptor, resolved: Option<&Resolved>) -> String {
    let Some(value) = resolved.and_then(|resolved| resolved.get(descriptor.id)) else {
        return "(unavailable)".to_owned();
    };
    match &descriptor.kind {
        SettingKind::Bool => match value.as_bool() {
            Some(true) => "On".to_owned(),
            Some(false) => "Off".to_owned(),
            None => "(unavailable)".to_owned(),
        },
        SettingKind::Int { .. } => value
            .as_int()
            .map_or_else(|| "(unavailable)".to_owned(), |int| int.to_string()),
        SettingKind::Text => value
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| "(unavailable)".to_owned()),
        SettingKind::Choice(options) => {
            let current = value.as_str().unwrap_or_default();
            options
                .iter()
                .find(|(name, _)| *name == current)
                .map_or_else(|| current.to_owned(), |(_, label)| (*label).to_owned())
        }
        SettingKind::LineWidth => match value {
            SettingValue::Int(columns) => format!("{columns} columns"),
            SettingValue::Str(text) if text == "full" => "full width".to_owned(),
            _ => "(unavailable)".to_owned(),
        },
        SettingKind::ThemePackage => {
            let id = value.as_str().unwrap_or_default();
            if id.is_empty() {
                "Built-in palettes".to_owned()
            } else {
                id.to_owned()
            }
        }
        SettingKind::StringList => value.as_list().map_or_else(
            || "(unavailable)".to_owned(),
            |list| {
                if list.is_empty() {
                    "(none)".to_owned()
                } else {
                    list.join(", ")
                }
            },
        ),
        SettingKind::ShortcutMap => {
            let Some(map) = value.as_shortcut_map() else {
                return "(unavailable)".to_owned();
            };
            if map.is_empty() {
                "(no overrides)".to_owned()
            } else {
                map.iter()
                    .map(|(command, chord)| format!("{command} = {chord}"))
                    .collect::<Vec<_>>()
                    .join("; ")
            }
        }
    }
}
