//! Editing-state chips and a bounded, paged history chooser.

use crate::{
    app::{App, Message},
    host::{
        context_mode::{self, EditorTarget},
        editor_history::{self, HistoryAction, HistoryMessage},
    },
    style,
    workspace::bounds,
};
use flavite_buffer::history::HistoryEntry;
use iced::widget::{
    Space, button, column, container, mouse_area, opaque, row, scrollable, stack, text, tooltip,
};
use iced::{Element, Length, alignment};

fn label(entry: &HistoryEntry) -> String {
    if entry.initial {
        "Initial state".into()
    } else {
        format!(
            "{}{}",
            entry.kind.label(),
            if entry.open { " ···" } else { "" }
        )
    }
}

fn observed<'a>(
    app: &App,
    source: EditorTarget,
    entry: &HistoryEntry,
    popup: bool,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    let node = entry.target.node;
    let generation = app.context_mode.generation();
    bounds::observe_named(
        content,
        format!(
            "history-{generation}-{}-{}-{node}-{popup}",
            source.tab.raw(),
            source.incarnation
        ),
        move |rect| {
            Message::EditorHistory(HistoryMessage::Bounds {
                source,
                generation,
                node,
                popup,
                rect,
            })
        },
    )
}

fn control<'a>(
    source: EditorTarget,
    name: &'static str,
    title: &'static str,
    action: Option<HistoryAction>,
) -> Element<'a, Message> {
    bounds::observe(
        button(text(title).size(12))
            .padding([4, 8])
            .style(style::ghost())
            .on_press_maybe(
                action.map(|action| Message::EditorHistory(HistoryMessage::Act(source, action))),
            ),
        move |rect| Message::EditorHistory(HistoryMessage::ControlBounds(source, name, rect)),
    )
}

/// The fixed 24px detail row uses only as many chips as actually fit.
pub fn strip<'a>(
    app: &'a App,
    source: EditorTarget,
    summary: String,
    caption: String,
) -> Element<'a, Message> {
    let metrics = *app.appearance.metrics();
    let mut content = row![text(summary).size(metrics.text_sm)]
        .spacing(6.0 * metrics.scale)
        .align_y(alignment::Vertical::Center);
    let visible = editor_history::visible_entries(app, source);
    if !visible.is_empty() {
        for entry in visible {
            let full = format!(
                "{}{}",
                label(entry),
                if entry.restorable {
                    ""
                } else {
                    " · Use Undo/Redo to cross the rename transaction"
                }
            );
            let is_redo = app
                .context_mode
                .history
                .page
                .as_ref()
                .unwrap()
                .redo
                .as_ref()
                .is_some_and(|redo| redo.target == entry.target);
            let mut short: String = label(entry)
                .chars()
                .take(if is_redo { 7 } else { 9 })
                .collect();
            if is_redo {
                short = format!("↪ {short}");
            } else if entry.current {
                short.push_str(" ✓");
            }
            if label(entry).chars().count() > 9 {
                short.push('…');
            }
            let chip = button(text(short).size(metrics.text_sm))
                .width(76.0 * metrics.scale)
                .padding([1, 4])
                .style(style::list_row(entry.current))
                .on_press_maybe(
                    (!app.context_mode.history.open
                        && app
                            .context_mode
                            .session
                            .as_ref()
                            .is_some_and(|s| s.menu.is_none())
                        && editor_history::can_restore(app, source, entry))
                    .then_some(Message::EditorHistory(HistoryMessage::Act(
                        source,
                        HistoryAction::Restore(entry.target),
                    ))),
                );
            content = content.push(observed(
                app,
                source,
                entry,
                false,
                tooltip(
                    chip,
                    text(full).size(metrics.text_sm),
                    tooltip::Position::Bottom,
                ),
            ));
        }
    } else {
        content = content.push(text(caption).size(metrics.text_sm).style(style::dim_text));
    }
    let detail = mouse_area(
        container(content)
            .height(24.0 * metrics.scale)
            .width(Length::Fill)
            .clip(true),
    )
    .on_double_click(Message::ContextMode(context_mode::ContextMessage::Run(
        source,
        flavite_editor::context::ContextCommand::Exit,
    )));
    detail.into()
}

pub fn overlay<'a>(app: &'a App, shell: Element<'a, Message>) -> Element<'a, Message> {
    let host = &app.context_mode.history;
    if !host.open {
        return shell;
    }
    let Some(source) = host.target else {
        return shell;
    };
    let metrics = *app.appearance.metrics();
    let width = (500.0 * metrics.scale).min(app.window_size().width - 16.0);
    let x = ((app.window_size().width - width) / 2.0).max(0.0);
    let y = app
        .context_mode
        .bounds
        .get(&(source.tab, "toolbar"))
        .map_or(100.0, |r| r.y + r.height + 4.0);
    let title = row![
        text("Editing history").size(metrics.text),
        Space::new().width(Length::Fill),
        control(source, "history-close", "Close", Some(HistoryAction::Close))
    ]
    .align_y(alignment::Vertical::Center);
    let mut contents = column![
        title,
        text("Choose a state to restore immediately")
            .size(metrics.text_sm)
            .style(style::dim_text)
    ]
    .spacing(6.0 * metrics.scale);
    if let Some(error) = &host.error {
        contents = contents.push(text(error).size(metrics.text_sm));
    }
    let mut entries = column![].spacing(editor_history::ROW_SPACING * metrics.scale);
    if let Some(page) = &host.page {
        for (index, entry) in page.entries.iter().enumerate() {
            let detail: &str = if entry.current {
                "Current"
            } else if !entry.restorable {
                "Use Undo/Redo to cross the rename transaction"
            } else {
                if entry.on_path {
                    "Earlier state"
                } else {
                    "Other branch"
                }
            };
            let title = format!("{}{}", label(entry), if entry.current { " ✓" } else { "" });
            let item = button(
                column![
                    text(title).size(metrics.text),
                    text(detail).size(metrics.text_sm).style(style::dim_text)
                ]
                .spacing(1),
            )
            .width(Length::Fill)
            .height(editor_history::ROW_HEIGHT * metrics.scale)
            .padding([5.0 * metrics.scale, 8.0 * metrics.scale])
            .style(style::list_row(index == host.selected))
            .on_press_maybe(editor_history::can_restore(app, source, entry).then_some(
                Message::EditorHistory(HistoryMessage::Act(
                    source,
                    HistoryAction::Restore(entry.target),
                )),
            ));
            entries = entries.push(observed(app, source, entry, true, item));
        }
    }
    let height = (app.window_size().height - y - 125.0 * metrics.scale)
        .max(80.0)
        .min(400.0 * metrics.scale);
    contents = contents.push(
        container(
            scrollable(entries)
                .id(iced::widget::Id::new("editing-history-rows"))
                .height(Length::Shrink),
        )
        .max_height(height),
    );
    contents = contents.push(row![
        control(
            source,
            "history-refresh",
            "Newest",
            Some(HistoryAction::Refresh)
        ),
        Space::new().width(Length::Fill),
        control(
            source,
            "history-next",
            "Older states",
            host.page
                .as_ref()
                .and_then(|p| p.next)
                .map(HistoryAction::NextPage)
        ),
    ]);
    let card = container(contents)
        .width(width)
        .padding(metrics.spacing)
        .style(style::palette_card);
    let card = bounds::observe(card, move |rect| {
        Message::EditorHistory(HistoryMessage::ControlBounds(source, "history-card", rect))
    });
    let composed = stack![
        shell,
        opaque(
            button(Space::new().width(Length::Fill).height(Length::Fill))
                .style(style::menu_backdrop)
                .on_press(Message::EditorHistory(HistoryMessage::Act(
                    source,
                    HistoryAction::Close
                )))
        ),
        iced::widget::pin(opaque(card)).x(x).y(y),
    ]
    .into();
    crate::views::context_toolbar::exit_overlay(app, composed)
}
