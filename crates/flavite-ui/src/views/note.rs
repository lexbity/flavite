//! Source-reading note view (RFC-0003 W03: shared editing, per-view input).
//!
//! One editable note tab. The widget runtime is keyed by TabId and restores
//! the host-owned logical interaction state after reparenting. Reserved and
//! failed loads render honest retry/close states.

use crate::app::{App, Message};
use crate::style;
use crate::views::empty_state;
use crate::workspace::{bounds, tab_strip};
use flavite_workspace::{LayoutAction, NoteLoad, TabId};
use iced::widget::{button, column, container, row, rule, text, text_input};
use iced::{Element, Length, alignment};

/// Render one note tab for the active view slot.
pub fn view<'a>(
    app: &'a App,
    tab: TabId,
    load: NoteLoad,
    locator: &'a flavite_workspace::NoteLocator,
) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
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
            .spacing(metrics.spacing),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(metrics.spacing_lg)
        .style(style::content)
        .into(),
        NoteLoad::Failed => container(
            column![
                text("Note unavailable").size(metrics.heading),
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
                .spacing(metrics.spacing),
            ]
            .spacing(metrics.spacing_sm),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(metrics.spacing_lg)
        .style(style::content)
        .into(),
        NoteLoad::Ready => ready_view(app, tab),
    }
}

fn ready_view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let Some(view) = app.note_editor_view(tab) else {
        return empty_state(
            *metrics,
            "Buffer unavailable",
            "The note view needs to be reopened.",
        );
    };
    let Some(incarnation) = app.notes.incarnation(tab) else {
        return empty_state(
            *metrics,
            "View unavailable",
            "This note view is no longer active.",
        );
    };
    let mode = app
        .workspace
        .tab(&tab)
        .and_then(|s| {
            if let flavite_workspace::ViewKind::Note(n) = s.kind() {
                Some(n.mode())
            } else {
                None
            }
        })
        .unwrap_or_default();
    let initial_interaction = app.editor_interaction_snapshot(tab);
    let mut editor =
        flavite_editor::EditorWidget::from_view(view, move |action| Message::EditorAction {
            tab,
            incarnation,
            action,
        })
        .with_view_key(tab.raw(), incarnation)
        .with_type_metrics(app.appearance.metrics().editor_type())
        .with_max_line_width(app.appearance.metrics().editor_max_line_width())
        .with_colors(crate::style::tokens::editor_colors(app.appearance.scheme()))
        .with_keyboard_enabled(!app.reading.find.get(&tab).is_some_and(|find| find.focused))
        .with_input_intents(move |input| Message::EditorInput {
            tab,
            incarnation,
            input,
        })
        .with_input_enabled(!app.picker.open && !app.palette_open && app.strip_menu.is_none())
        .with_interaction(move |snapshot| Message::EditorAction {
            tab,
            incarnation,
            action: flavite_editor::EditorAction::Interaction(snapshot),
        });
    if let Some(interaction) = initial_interaction {
        editor = editor.with_initial_interaction(interaction);
    }
    let meta = container(
        row![
            button(
                text(if mode == flavite_workspace::NoteMode::Edit {
                    "Edit"
                } else {
                    "Read"
                })
                .size(metrics.text_sm)
            )
            .style(style::action())
            .on_press_maybe((!app.has_composition()).then_some(Message::ToggleNoteMode(tab)))
        ]
        .spacing(8)
        .align_y(alignment::Vertical::Center),
    )
    .height(metrics.tab_height)
    .padding([0, 12])
    .align_y(alignment::Vertical::Center)
    .style(style::toolbar);
    let state = match app.note_buffer_state(tab) {
        Some(flavite_api::BufferStatus::Clean) => "Saved",
        Some(flavite_api::BufferStatus::Dirty) => "Unsaved changes",
        Some(flavite_api::BufferStatus::Conflicted) => {
            "Conflict — resolve before editing structure"
        }
        Some(flavite_api::BufferStatus::Orphaned) => "Missing on disk — recovery retained",
        None => "Unavailable",
    };
    // RFC-0006 §6.10: the source-format indicator rides with the save-state
    // label — one compact, user-facing freshness/format surface.
    let state = match app.note_source_format(tab) {
        Some(format) => format!(
            "{state} · {}{}",
            format.encoding_code(),
            if format.bom() { "+bom" } else { "" }
        ),
        None => state.to_owned(),
    };
    let footer = container(
        text(state)
            .size(metrics.text_sm)
            .style(style::dim_text),
    )
    .height(24)
    .padding([4, 12]);
    let editor: Element<'a, Message> = if mode == flavite_workspace::NoteMode::Edit {
        bounds::observe(
            editor.width(Length::Fill).height(Length::Fill),
            move |rect| Message::EditorBounds(tab, rect),
        )
    } else if let Some(doc) = crate::host::reading::document(app, tab) {
        crate::read::view(app, tab, doc)
    } else if let Some(error) = app
        .reading
        .notes
        .get(&tab)
        .and_then(|entry| entry.error.as_ref())
    {
        container(
            column![
                text(error),
                button("Retry reading").on_press(Message::RetryReading(tab)),
                button("Edit source").on_press(Message::ToggleNoteMode(tab))
            ]
            .spacing(12),
        )
        .padding(16)
        .height(Length::Fill)
        .into()
    } else {
        empty_state(
            *metrics,
            "Loading reading view…",
            "Preparing the current revision.",
        )
    };
    let mut content = column![meta, rule::horizontal(1)];
    if let Some(banner) = encoding_banner(app, tab, metrics) {
        content = content.push(banner);
    }
    if let Some(find) = app.reading.find.get(&tab) {
        let matches = crate::host::reading::find_matches(app, tab);
        content = content.push(
            container(
                row![
                    text_input("Find in current note", &find.query)
                        .id(crate::host::reading::find_id(tab))
                        .on_input(move |query| Message::FindQuery { tab, query })
                        .on_submit(Message::FindNext { tab, forward: true }),
                    text(format!(
                        "{} / {}",
                        if matches.is_empty() {
                            0
                        } else {
                            find.index % matches.len() + 1
                        },
                        matches.len()
                    ))
                    .size(12),
                    button("Previous")
                        .style(style::ghost())
                        .on_press(Message::FindNext {
                            tab,
                            forward: false
                        }),
                    button("Next")
                        .style(style::ghost())
                        .on_press(Message::FindNext { tab, forward: true }),
                    button("Close find")
                        .style(style::ghost())
                        .on_press(Message::FindClose(tab))
                ]
                .spacing(8),
            )
            .padding(8),
        );
    }
    content
        .push(editor)
        .push(rule::horizontal(1))
        .push(footer)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The pending encoding banner for a tab, when the core requires a typed
/// decision (§6.10). Keyboard-accessible buttons share one catalog of
/// actions; Cancel dismisses without effects.
fn encoding_banner<'a>(
    app: &'a App,
    tab: TabId,
    metrics: &crate::style::type_scale::Metrics,
) -> Option<Element<'a, Message>> {
    let pending = app.encoding.pending(tab)?;
    let reason = crate::host::encoding::payload_reason(pending.payload.as_ref());
    let mut row_items = Vec::new();
    match pending.code.as_str() {
        "encoding-choice-required" => {
            row_items.push(text(format!(
                "Encoding action required ({reason}) — pick an explicit format:"
            ))
            .size(metrics.text_sm)
            .into());
            for (label, encoding) in [
                ("UTF-8", "utf8"),
                ("UTF-16 LE", "utf16le"),
                ("UTF-16 BE", "utf16be"),
            ] {
                row_items.push(
                    button(text(label).size(metrics.text_sm))
                        .style(style::action())
                        .on_press(Message::Encoding(
                            crate::host::encoding::EncodingMessage::Reopen {
                                tab,
                                encoding: encoding.to_owned(),
                                bom: "auto".to_owned(),
                            },
                        ))
                        .into(),
                );
            }
        }
        "encoding-unencodable" => {
            row_items.push(
                text("This draft cannot be saved under its strict ASCII format — convert to UTF-8?")
                    .size(metrics.text_sm)
                    .into(),
            );
            row_items.push(
                button(text("Convert to UTF-8").size(metrics.text_sm))
                    .style(style::action())
                    .on_press(Message::Encoding(
                        crate::host::encoding::EncodingMessage::Convert {
                            tab,
                            encoding: "utf8".to_owned(),
                            bom: "absent".to_owned(),
                        },
                    ))
                    .into(),
            );
        }
        other => {
            row_items.push(
                text(format!("Encoding needs attention ({other})"))
                    .size(metrics.text_sm)
                    .into(),
            );
        }
    }
    row_items.push(
        button(text("Cancel").size(metrics.text_sm))
            .style(style::action())
            .on_press(Message::Encoding(
                crate::host::encoding::EncodingMessage::Dismiss { tab },
            ))
            .into(),
    );
    Some(
        container(row(row_items).spacing(8))
            .padding([6, 12])
            .width(Length::Fill)
            .style(style::toolbar)
            .into(),
    )
}
