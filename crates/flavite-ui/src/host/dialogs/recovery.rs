//! The recovery-offer dialog (RFC-0003 W12, D-12): a durable recovery
//! snapshot was retained for a note (crash, close-with-recovery). The
//! notice never mutates anything — restoring or discarding is explicit.

use crate::app::Message;
use crate::style;
use crate::workspace::tab_strip;
use iced::Element;
use iced::widget::{button, column, row, text};

/// Render the recovery offer for one note path.
pub fn view(path: &str, metrics: style::Metrics) -> Element<'_, Message> {
    let body = column![
        text("Recover unsaved changes").size(metrics.heading),
        text(format!(
            "A durable recovery copy of unsaved changes was retained for {path}. \
             Restoring installs it into the note (opening it if needed); the \
             on-disk file is not touched until you save."
        ))
        .size(metrics.text_sm)
        .style(style::dim_text),
        row![
            button(text("Restore recovered changes").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press(Message::RecoveryResolve {
                    path: path.to_owned(),
                    restore: true,
                }),
            button(text("Discard recovered copy").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::RecoveryResolve {
                    path: path.to_owned(),
                    restore: false,
                }),
            button(text("Later").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::DialogCancel),
        ]
        .spacing(metrics.spacing_sm),
    ]
    .spacing(metrics.spacing_sm);
    super::dialog_card(metrics, body)
}
