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
    let Some(session) = app.workspace.tab(&tab) else {
        return empty_state("Empty", "This tab has no content.");
    };
    if !matches!(session.kind(), flavite_workspace::ViewKind::Backlinks(_)) {
        return empty_state("Backlinks", "This tab is not a backlinks view.");
    }
    let header = context_header::view(app, tab);
    let body: Element<'a, Message> = match app.contexts.state(&tab) {
        Some(ToolState::Ready(ToolContent::Backlinks { rows, .. })) => {
            let mut list = column![].spacing(2);
            for row in rows {
                list = list.push(backlink_row(row));
            }
            container(
                scrollable(list.width(Length::Fill))
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(style::SPACING_SM)
            .style(style::card)
            .into()
        }
        Some(ToolState::NoBacklinks) => {
            empty_state("No backlinks", "No saved note links to this one yet.")
        }
        Some(ToolState::Unbound { .. }) => empty_state(
            "No source",
            "Focus a note, publish a selection, or pin one to inspect its backlinks.",
        ),
        Some(ToolState::Missing) => {
            empty_state("Missing note", "The bound note is not in the saved vault.")
        }
        Some(ToolState::Failed { message }) => container(
            column![
                text("Backlinks unavailable").size(style::HEADING),
                text(message.as_str()).style(style::dim_text),
                retry_button(app, tab),
            ]
            .spacing(style::SPACING_SM),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(style::SPACING_LG)
        .style(style::content)
        .into(),
        Some(ToolState::Busy) => empty_state(
            "Queued",
            "The query service is busy; these backlinks will update.",
        ),
        Some(ToolState::Idle) | Some(ToolState::Querying) | None => {
            empty_state("Loading…", "The backlink query is in flight.")
        }
        Some(ToolState::Ready(ToolContent::Preview { .. })) => {
            empty_state("Loading…", "The backlink query is in flight.")
        }
    };
    column![header, rule::horizontal(1), body]
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

fn backlink_row<'a>(row: &'a flavite_api::BacklinkRow) -> Element<'a, Message> {
    let action = NoteLocator::from_relative(std::path::Path::new(&row.source_path))
        .ok()
        .map(|locator| Message::OpenNoteFromContext { locator });
    button(
        column![
            text(row.source_title.clone()).width(Length::Fill),
            text(
                row.excerpt
                    .clone()
                    .unwrap_or_else(|| row.source_path.clone())
            )
            .size(style::TEXT_SM)
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
    button(text("Retry").size(style::TEXT_SM))
        .padding(tab_strip::chip_padding())
        .style(style::action())
        .on_press_maybe(
            app.contexts
                .observed(&tab)
                .map(|_| Message::RequeryContext { tab }),
        )
}
