//! The merge-confirmation dialog (RFC-0003 W12): the core prepared a
//! three-way merge (`MergeReady` receipt with structured hunks); the user
//! reviews the hunks and explicitly applies the merged text through
//! `conflict.apply_merge`, verified by core against the receipt.

use super::PreparedMerge;
use crate::app::Message;
use crate::style;
use crate::workspace::tab_strip;
use iced::widget::{button, column, row, scrollable, text};
use iced::{Element, Length};

/// Render the merge confirmation for a prepared receipt.
pub fn view(prepared: &PreparedMerge, metrics: style::Metrics) -> Element<'_, Message> {
    let mut body = column![
        text("Confirm merge").size(metrics.heading),
        text(format!(
            "{} — {} conflict hunk(s) against the on-disk revision",
            prepared.path,
            prepared.conflicts.len()
        ))
        .size(metrics.text_sm)
        .style(style::dim_text),
    ]
    .spacing(metrics.spacing_sm);
    if prepared.conflicts.is_empty() {
        body = body.push(text("The merge applied cleanly.").size(metrics.text));
    } else {
        let mut hunks = column![].spacing(metrics.spacing_sm);
        for (index, hunk) in prepared.conflicts.iter().enumerate() {
            hunks = hunks.push(text(format!("Hunk {}: {hunk}", index + 1)).size(metrics.text_sm));
        }
        body = body.push(scrollable(hunks).height(Length::Fill));
    }
    body = body.push(
        row![
            button(text("Apply merged text").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press(Message::MergeApply),
            button(text("Cancel (keep the conflict)").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::DialogCancel),
        ]
        .spacing(metrics.spacing_sm),
    );
    super::dialog_card(metrics, body)
}
