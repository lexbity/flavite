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
use flavite_editor::context::layout;
use iced::widget::{
    Space, button, column, container, mouse_area, opaque, row, scrollable, stack, text, tooltip,
};
use iced::{Element, Length, alignment};

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
                source: source.clone(),
                generation,
                node,
                popup,
                rect,
            })
        },
    )
}

fn control<'a>(
    app: &App,
    source: EditorTarget,
    name: &'static str,
    title: &'static str,
    action: Option<HistoryAction>,
) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    bounds::observe(
        button(text(title).size(metrics.text_sm))
            .padding([4.0 * metrics.scale, 8.0 * metrics.scale])
            .style(style::ghost())
            .on_press_maybe(
                action.map(|action| {
                Message::EditorHistory(HistoryMessage::Act(source.clone(), action))
            }),
            ),
        move |rect| {
            Message::EditorHistory(HistoryMessage::ControlBounds(source.clone(), name, rect))
        },
    )
}

/// The fixed 24px detail row uses only as many chips as actually fit.
pub fn strip<'a>(
    app: &'a App,
    source: EditorTarget,
    summary: String,
    caption: String,
    effect: Option<String>,
) -> Element<'a, Message> {
    let metrics = *app.appearance.metrics();
    let mut content = row![
        container(text(summary).size(metrics.text_sm)).width(layout::SUMMARY_WIDTH * metrics.scale)
    ]
    .spacing(layout::CHIP_GAP * metrics.scale)
    .align_y(alignment::Vertical::Center);
    let visible = editor_history::visible_entries(app, source.clone());
    if !visible.is_empty() {
        for entry in visible {
            let full = format!(
                "{} · {}",
                editor_history::entry_title(entry),
                editor_history::entry_status(entry)
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
            let mut short = if entry.initial {
                "Initial".to_owned()
            } else {
                match entry.kind {
                    flavite_buffer::history::EditKind::OpenAbove => "Open above",
                    flavite_buffer::history::EditKind::OpenBelow => "Open below",
                    flavite_buffer::history::EditKind::Format => "Format",
                    flavite_buffer::history::EditKind::Snapshot => "Snapshot",
                    flavite_buffer::history::EditKind::Edit => "Edit",
                    _ => entry.kind.label(),
                }
                .to_owned()
            };
            if is_redo {
                short = format!("↪ {short}");
            } else if entry.current {
                short.push_str(" ✓");
            }
            let chip = button(text(short).size(metrics.text_sm))
                .width(layout::CHIP_WIDTH * metrics.scale)
                .padding([1.0 * metrics.scale, 4.0 * metrics.scale])
                .style(style::list_row(entry.current))
                .on_press_maybe(
                    (!app.context_mode.history.open
                        && app
                            .context_mode
                            .session
                            .as_ref()
                            .is_some_and(|s| s.state.menu().is_none())
                        && editor_history::can_restore(app, source.clone(), entry))
                    .then_some(Message::EditorHistory(HistoryMessage::Act(
                        source.clone(),
                        HistoryAction::Restore(entry.target),
                    ))),
                );
            content = content.push(observed(
                app,
                source.clone(),
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
        let attention = app
            .context_mode
            .session
            .as_ref()
            .is_some_and(|session| session.state.attention());
        let caption = if attention {
            caption
        } else {
            effect.unwrap_or(caption)
        };
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
        flavite_edit_context::ContextCommand::Exit,
    )));
    detail.into()
}

pub fn overlay<'a>(app: &'a App, shell: Element<'a, Message>) -> Element<'a, Message> {
    let host = &app.context_mode.history;
    if !host.open {
        return shell;
    }
    let Some(source) = host.target.clone() else {
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
        control(
            app,
            source.clone(),
            "history-close",
            "Close",
            Some(HistoryAction::Close)
        )
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
    if let Some(page) = &host.page
        && page.history_truncated
    {
        contents = contents.push(
            text(format!(
                "Earlier states removed to stay within the memory budget (oldest retained #{}).",
                page.retained_watermark
            ))
            .size(metrics.text_sm),
        );
    }
    let mut entries = column![].spacing(editor_history::ROW_SPACING * metrics.scale);
    if let Some(page) = &host.page {
        for (index, entry) in page.entries.iter().enumerate() {
            let detail = editor_history::entry_status(entry);
            let title = format!(
                "{}{}",
                editor_history::entry_title(entry),
                if entry.current { " ✓" } else { "" }
            );
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
            .on_press_maybe(
                editor_history::can_restore(app, source.clone(), entry).then_some({
                    Message::EditorHistory(HistoryMessage::Act(
                        source.clone(),
                        HistoryAction::Restore(entry.target),
                    ))
                }),
            );
            entries = entries.push(observed(app, source.clone(), entry, true, item));
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
            app,
            source.clone(),
            "history-refresh",
            "Newest",
            Some(HistoryAction::Refresh)
        ),
        Space::new().width(Length::Fill),
        control(
            app,
            source.clone(),
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
    let card_source = source.clone();
    let card = bounds::observe(card, move |rect| {
        Message::EditorHistory(HistoryMessage::ControlBounds(
            card_source.clone(),
            "history-card",
            rect,
        ))
    });
    let composed = stack![
        shell,
        opaque(
            button(Space::new().width(Length::Fill).height(Length::Fill))
                .style(style::menu_backdrop)
                .on_press(Message::EditorHistory(HistoryMessage::Act(
                    source.clone(),
                    HistoryAction::Close
                )))
        ),
        iced::widget::pin(opaque(card)).x(x).y(y),
    ]
    .into();
    crate::views::context_toolbar::exit_overlay(app, composed)
}
