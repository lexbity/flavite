//! Backlinks view (RFC-0003 §20, W04).
//!
//! Inbound links of the bound context source, each row offering Open on
//! the linking note. Busy, no-backlinks, missing-note, and failure are
//! distinct honest states, never silent empty lists.

use crate::app::{App, Message};
use crate::host::context::{ToolContent, ToolState};
use crate::style;
use crate::views::{context_header, empty_state};
use crate::workspace::tab_strip;
use flavite_workspace::{NoteLocator, TabId};
use iced::widget::{button, column, container, rule, scrollable, text};
use iced::{Element, Length, Padding};

/// Render one Backlinks tab for the active view slot.
pub fn view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let metrics = *app.appearance.metrics();
    let Some(session) = app.workspace.tab(&tab) else {
        return empty_state(metrics, "Empty", "This tab has no content.");
    };
    if !matches!(session.kind(), flavite_workspace::ViewKind::Backlinks(_)) {
        return empty_state(metrics, "Backlinks", "This tab is not a backlinks view.");
    }
    let header = context_header::view(app, tab);
    let body: Element<'a, Message> = match app.contexts.state(&tab) {
        Some(ToolState::Ready(ToolContent::Backlinks { rows, .. })) => {
            let mut list = column![].spacing(2);
            for row in rows {
                list = list.push(backlink_row(metrics, row));
            }
            container(
                scrollable(list.width(Length::Fill))
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(metrics.spacing_sm)
            .style(style::card)
            .into()
        }
        Some(ToolState::NoBacklinks) => empty_state(
            metrics,
            "No backlinks",
            "No current note links to this one yet.",
        ),
        Some(ToolState::Unbound { .. }) => empty_state(
            metrics,
            "No source",
            "Focus a note, publish a selection, or pin one to inspect its backlinks.",
        ),
        Some(ToolState::Missing) => empty_state(
            metrics,
            "Missing note",
            "The bound note is not in the saved vault.",
        ),
        Some(ToolState::Failed { message }) => container(
            column![
                text("Backlinks unavailable").size(metrics.heading),
                text(message.as_str()).style(style::dim_text),
                retry_button(app, tab),
            ]
            .spacing(metrics.spacing_sm),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(metrics.spacing_lg)
        .style(style::content)
        .into(),
        Some(ToolState::Busy) => empty_state(
            metrics,
            "Queued",
            "The query service is busy; these backlinks will update.",
        ),
        Some(ToolState::Idle) | Some(ToolState::Querying) | None => {
            empty_state(metrics, "Loading…", "The backlink query is in flight.")
        }
        Some(ToolState::Ready(
            ToolContent::Document(_)
            | ToolContent::Preview { .. }
            | ToolContent::BufferPreview { .. },
        )) => empty_state(metrics, "Loading…", "The backlink query is in flight."),
    };
    column![header, rule::horizontal(1), body]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn backlink_row<'a>(
    metrics: crate::style::Metrics,
    row: &'a flavite_api::BacklinkRow,
) -> Element<'a, Message> {
    let action = NoteLocator::from_relative(std::path::Path::new(&row.source_path))
        .ok()
        .map(|locator| Message::OpenNoteFromContext { locator });
    button(
        column![
            text(format!(
                "{} · {} occurrence(s) · {}",
                row.source_title,
                row.occurrences,
                if row.unsaved { "Unsaved" } else { "Saved" }
            ))
            .width(Length::Fill),
            text(
                row.excerpt
                    .clone()
                    .unwrap_or_else(|| row.source_path.clone())
            )
            .size(metrics.text_sm)
            .style(style::dim_text),
        ]
        .spacing(2),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 6.0,
        bottom: 6.0,
        left: 10.0,
        right: 10.0,
    })
    .style(style::list_row(false))
    .on_press_maybe(action)
    .into()
}

fn retry_button(app: &App, tab: TabId) -> iced::widget::Button<'static, Message> {
    let metrics = app.appearance.metrics();
    button(text("Retry").size(metrics.text_sm))
        .padding(tab_strip::chip_padding())
        .style(style::action())
        .on_press_maybe(
            app.contexts
                .observed(&tab)
                .map(|_| Message::RequeryContext { tab }),
        )
}
