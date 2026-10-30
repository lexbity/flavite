//! Saved source-text preview (RFC-0003 §20, W04).
//!
//! This is explicitly a source-text preview, not rendered reading (W09
//! owns the renderer). The content is the saved vault text with its index
//! provenance; unsaved editor content is never shown here.

use crate::app::{App, Message};
use crate::host::context::{ToolContent, ToolState};
use crate::style;
use crate::views::{context_header, empty_state};
use crate::workspace::tab_strip;
use flavite_workspace::TabId;
use iced::widget::{button, column, container, rule, scrollable, text};
use iced::{Element, Length};

/// Render one Preview tab for the active view slot.
pub fn view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let Some(session) = app.workspace.tab(&tab) else {
        return empty_state("Empty", "This tab has no content.");
    };
    if !matches!(session.kind(), flavite_workspace::ViewKind::Preview(_)) {
        return empty_state("Preview", "This tab is not a preview view.");
    }
    let header = context_header::view(app, tab);
    let body: Element<'a, Message> = match app.contexts.state(&tab) {
        Some(ToolState::Ready(ToolContent::Preview { text: saved, .. })) => {
            let open = button(text("Open note").size(style::TEXT_SM))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press_maybe(app.contexts.observed(&tab).map(|source| {
                    Message::OpenNoteFromContext {
                        locator: source.locator.clone(),
                    }
                }));
            container(
                column![
                    container(open).width(Length::Fill),
                    scrollable(
                        text(saved.as_str())
                            .width(Length::Fill)
                            .style(style::dim_text),
                    )
                    .width(Length::Fill)
                    .height(Length::Fill),
                ]
                .spacing(style::SPACING_SM),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(style::SPACING)
            .style(style::content)
            .into()
        }
        Some(ToolState::Unbound { .. }) => empty_state(
            "No source",
            "Focus a note, publish a selection, or pin one to preview it.",
        ),
        Some(ToolState::Missing) => {
            empty_state("Missing note", "The bound note is not in the saved vault.")
        }
        Some(ToolState::Failed { message }) => container(
            column![
                text("Preview unavailable").size(style::HEADING),
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
            "The query service is busy; this preview will update.",
        ),
        Some(
            ToolState::Idle
            | ToolState::Querying
            | ToolState::NoBacklinks
            | ToolState::Ready(ToolContent::Backlinks { .. }),
        )
        | None => empty_state("Loading…", "The preview query is in flight."),
    };
    column![header, rule::horizontal(1), body]
        .width(Length::Fill)
        .height(Length::Fill)
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
