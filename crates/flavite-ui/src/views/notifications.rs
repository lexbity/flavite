//! Notification presentation (RFC-0003 W04: presentation is not content).
//!
//! Extracted from the workspace region: notices float over the workspace
//! as an overlay and never consume layout rows. Acknowledgement happens
//! only after the entry is visibly presented (FR-28 / D-16).

use crate::app::{App, Message};
use crate::style;
use crate::workspace::tab_strip;
use iced::widget::{button, column, container, row, text};
use iced::{Element, Length, Padding, alignment};

/// The floating notification overlay, `None` while nothing is presented.
pub fn overlay(app: &App) -> Option<Element<'_, Message>> {
    if app.notices.is_empty() {
        return None;
    }
    let hidden = app.notices.len().saturating_sub(3);
    let mut entries = column![
        text(format!("Notifications ({})", app.notices.len()))
            .size(style::TEXT_SM)
            .style(style::dim_text)
    ]
    .spacing(2);
    for notice in app.notices.iter().take(3) {
        entries = entries.push(
            row![
                text(format!("{}: {:?}", notice.event.event_type(), notice.event))
                    .size(style::TEXT_SM)
                    .width(Length::Fill),
                button(text("Dismiss").size(style::TEXT_SM))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::AcknowledgeNotice(notice.id)),
            ]
            .spacing(style::SPACING_SM)
            .align_y(alignment::Vertical::Center),
        );
    }
    if hidden > 0 {
        entries = entries.push(
            text(format!(
                "{hidden} additional notices are waiting for presentation"
            ))
            .size(style::TEXT_SM)
            .style(style::dim_text),
        );
    }
    Some(
        container(entries)
            .width(Length::Fill)
            .padding(Padding {
                top: style::SPACING_SM,
                bottom: style::SPACING_SM,
                left: style::SPACING_LG,
                right: style::SPACING_LG,
            })
            .style(style::card)
            .into(),
    )
}
