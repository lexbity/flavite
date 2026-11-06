//! Source-reading note view (RFC-0003 W03: shared editing, per-view input).
//!
//! One editable note tab. The widget runtime is keyed by TabId and restores
//! the host-owned logical interaction state after reparenting. Reserved and
//! failed loads render honest retry/close states.

use crate::app::{App, Message};
use crate::style;
use crate::views::empty_state;
use crate::workspace::{
    bounds,
    chrome::{self, Icon},
    tab_strip,
};
use flavite_workspace::{LayoutAction, NoteLoad, TabId};
use iced::widget::{button, column, container, row, rule, text, text_editor};
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
                        .on_press(Message::OpenIntent(
                            crate::host::open_intents::OpenIntent::reuse(
                                locator.clone(),
                                Some(tab),
                            ),
                        )),
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
    let path_label = view
        .path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let source_buffer = view.buffer_id;
    let source_revision = view.revision;
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
        .with_candidates(app.editor_candidates().clone())
        .with_stable_links(app.appearance.prefs.prefer_stable_links)
        .with_link_resolutions(app.editor_link_resolutions(
            tab,
            incarnation,
            source_buffer,
            source_revision,
        ))
        .with_keyboard_enabled(!crate::host::note_search::blocks_typing(app, &tab))
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
            chrome::icon_button(
                *metrics,
                Icon::Back,
                "Back",
                app.nav.can_go_back(tab).then_some(Message::NavigateBack)
            ),
            chrome::icon_button(
                *metrics,
                Icon::Forward,
                "Forward",
                app.nav
                    .can_go_forward(tab)
                    .then_some(Message::NavigateForward)
            ),
            text(path_label).size(metrics.text).style(style::dim_text),
            iced::widget::Space::new().width(Length::Fill),
            chrome::icon_button(
                *metrics,
                Icon::Search,
                "Find in note · Ctrl+F",
                Some(Message::RunHostCommand("workspace.find"))
            ),
            chrome::icon_button(
                *metrics,
                if mode == flavite_workspace::NoteMode::Edit {
                    Icon::Read
                } else {
                    Icon::Edit
                },
                if mode == flavite_workspace::NoteMode::Edit {
                    "Reading view · Ctrl+E"
                } else {
                    "Edit source · Ctrl+E"
                },
                (!app.has_composition()).then_some(Message::ToggleNoteMode(tab))
            ),
        ]
        .spacing(4)
        .align_y(alignment::Vertical::Center),
    )
    .height(38.0 * metrics.scale)
    .padding([0, 8])
    .width(Length::Fill);
    let state = match app.note_buffer_state(tab) {
        Some(flavite_buffer::state::BufferState::Clean) => "Saved",
        Some(flavite_buffer::state::BufferState::Dirty) => "Unsaved changes",
        Some(flavite_buffer::state::BufferState::Conflicted) => {
            "Conflict — resolve before editing structure"
        }
        Some(flavite_buffer::state::BufferState::Orphaned) => "Missing on disk — recovery retained",
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
    let footer = container(text(state).size(metrics.text_sm).style(style::dim_text))
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
    if let Some(find) = app.note_search.finds.get(&tab) {
        let page = crate::host::note_search::matches(app, tab);
        let total = page.as_ref().map_or(0, |page| page.spans.len());
        let total_matches = page.as_ref().map_or(0, |page| page.total_matches);
        // Empty query is Idle (D-05): no counts are presented at all.
        let counts = if find.query.is_empty() && find.mode == "literal" {
            String::new()
        } else if find.pending.is_some() {
            "Searching…".to_owned()
        } else if let Some(page) = &page {
            let current = if total == 0 {
                0
            } else {
                page.offset
                    .saturating_add(find.index as u64)
                    .saturating_add(1)
            };
            let first = page.offset.saturating_add(1);
            let last = page.offset.saturating_add(total as u64);
            format!(
                "{current} / {total_matches} · page {first}–{last}{}",
                if page.next_offset.is_some() {
                    " · Next continues"
                } else {
                    ""
                }
            )
        } else {
            format!("0 / {total_matches}")
        };
        let find_status = find.last_error.as_deref().unwrap_or("");
        let replace_open = app.note_search.replaces.contains_key(&tab);
        content = content.push(
            container(
                row![
                    text_editor(&find.query_input)
                        .id(crate::host::note_search::find_id(tab))
                        .placeholder("Find source · Alt+Enter inserts a newline")
                        .height(64)
                        .on_action(move |action| Message::NoteSearch(
                            crate::host::note_search::NoteSearchMessage::DraftAction {
                                tab,
                                replacement: false,
                                action
                            }
                        ))
                        .key_binding(move |key| crate::host::note_search::draft_binding(
                            tab, false, key
                        )),
                    text(counts).size(12),
                    button(if find.mode == "regex" {
                        "Regex"
                    } else {
                        "Literal"
                    })
                    .style(style::ghost())
                    .on_press(Message::NoteSearch(
                        crate::host::note_search::NoteSearchMessage::SetMode {
                            tab,
                            mode: if find.mode == "regex" {
                                "literal"
                            } else {
                                "regex"
                            }
                            .into(),
                        },
                    )),
                    button(if find.case_sensitive { "Aa" } else { "aA" })
                        .style(style::ghost())
                        .on_press(Message::NoteSearch(
                            crate::host::note_search::NoteSearchMessage::ToggleCaseSensitive(tab),
                        )),
                    button(if find.whole_word {
                        "Whole word ✓"
                    } else {
                        "Whole word"
                    })
                    .style(style::ghost())
                    .on_press(Message::NoteSearch(
                        crate::host::note_search::NoteSearchMessage::ToggleWholeWord(tab),
                    )),
                    button(if find.use_selection_scope {
                        "Selection only ✓"
                    } else {
                        "Selection only"
                    })
                    .style(style::ghost())
                    .on_press(Message::NoteSearch(
                        crate::host::note_search::NoteSearchMessage::ToggleSelectionScope(tab),
                    )),
                    button("Previous")
                        .style(style::ghost())
                        .on_press(Message::NoteSearch(
                            crate::host::note_search::NoteSearchMessage::FindNext {
                                tab,
                                forward: false,
                            },
                        )),
                    button("Next")
                        .style(style::ghost())
                        .on_press(Message::NoteSearch(
                            crate::host::note_search::NoteSearchMessage::FindNext {
                                tab,
                                forward: true,
                            },
                        )),
                    button(if replace_open {
                        "Hide replace"
                    } else {
                        "Replace…"
                    })
                    .style(style::ghost())
                    .on_press(Message::NoteSearch(if replace_open {
                        crate::host::note_search::NoteSearchMessage::ReplaceClose(tab)
                    } else {
                        crate::host::note_search::NoteSearchMessage::ReplaceOpen(tab)
                    })),
                    button("Close find")
                        .style(style::ghost())
                        .on_press(Message::NoteSearch(
                            crate::host::note_search::NoteSearchMessage::FindClose(tab)
                        )),
                ]
                .spacing(8),
            )
            .padding(8),
        );
        if !find_status.is_empty() {
            content = content.push(text(find_status).size(12));
        }
        if let Some(replace) = app.note_search.replaces.get(&tab) {
            let status = if let Some(error) = &replace.last_error {
                error.clone()
            } else if replace.pending.is_some() {
                "Preparing replacement…".to_owned()
            } else if let Some(count) = replace.match_count {
                format!(
                    "{count} match{} in the prepared plan",
                    if count == 1 { "" } else { "es" }
                )
            } else {
                String::new()
            };
            content = content.push(
                container(
                    row![
                        text_editor(&replace.replacement_input)
                            .id(crate::host::note_search::replace_id(tab))
                            .placeholder("Replace source · Alt+Enter inserts a newline")
                            .height(64)
                            .on_action(move |action| Message::NoteSearch(
                                crate::host::note_search::NoteSearchMessage::DraftAction {
                                    tab,
                                    replacement: true,
                                    action
                                }
                            ))
                            .key_binding(move |key| crate::host::note_search::draft_binding(
                                tab, true, key
                            )),
                        button("Replace next").style(style::ghost()).on_press_maybe(
                            replace.pending.is_none().then_some(Message::NoteSearch(
                                crate::host::note_search::NoteSearchMessage::ReplaceNext(tab),
                            ),)
                        ),
                        button("Replace all").style(style::ghost()).on_press_maybe(
                            replace.pending.is_none().then_some(Message::NoteSearch(
                                crate::host::note_search::NoteSearchMessage::ReplaceAll(tab),
                            ),)
                        ),
                        text(status).size(12),
                    ]
                    .spacing(8),
                )
                .padding(8),
            );
        }
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
            row_items.push(
                text(format!(
                    "Encoding action required ({reason}) — pick an explicit format:"
                ))
                .size(metrics.text_sm)
                .into(),
            );
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
                text(
                    "This draft cannot be saved under its strict ASCII format — convert to UTF-8?",
                )
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
