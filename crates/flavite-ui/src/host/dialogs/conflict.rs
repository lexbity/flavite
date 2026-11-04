//! The conflict dialog (RFC-0003 W12): reload / keep-mine / merge, as the
//! core permits. `save-as` is not implemented in core and surfaces as its
//! typed `save-as-confirm` action-required code in the status line — the
//! dialog never fakes it.

use crate::app::{App, Message};
use crate::style;
use crate::workspace::tab_strip;
use flavite_api::BufferStatus as BufferState;
use iced::Element;
use iced::widget::{button, column, row, text};

/// Render the conflict dialog for one conflicted tab.
pub fn view(
    app: &App,
    tab: flavite_workspace::TabId,
    metrics: style::Metrics,
) -> Element<'_, Message> {
    let Some(state) = app.notes.state(&tab) else {
        return column![].into();
    };
    let path = state.locator.to_path(app.root()).display().to_string();
    let label = match app.note_buffer_state(tab) {
        Some(BufferState::Conflicted) => {
            "The file changed on disk while this note had unsaved edits. \
             Resolve before editing structure; saving is refused until resolved."
        }
        Some(BufferState::Orphaned) => {
            "The file was deleted on disk. Restore it externally, discard the \
             edits, or keep the note open with its retained recovery copy."
        }
        _ => "This note needs a decision.",
    };
    let resolution_button = |label: &'static str, resolution: &'static str| {
        button(text(label).size(metrics.text_sm))
            .padding(tab_strip::chip_padding())
            .style(style::action())
            .on_press(Message::ConflictResolve {
                tab,
                resolution: resolution.into(),
            })
    };
    let mut body = column![
        text("Conflict").size(metrics.heading),
        text(format!("{path} — {label}"))
            .size(metrics.text_sm)
            .style(style::dim_text),
        row![
            resolution_button("Reload from disk (discard my edits)", "reload"),
            resolution_button("Keep my edits (then save)", "keep_mine"),
        ]
        .spacing(metrics.spacing_sm),
        row![
            resolution_button("Prepare three-way merge", "merge"),
            button(text("Compare in merge view after preparing").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost()),
        ]
        .spacing(metrics.spacing_sm),
        text(
            "Save-as is not offered by the core yet (save-as-confirm); \
              use keep-mine plus a manual copy if you need it."
        )
        .size(metrics.text_sm)
        .style(style::dim_text),
    ]
    .spacing(metrics.spacing_sm);
    body = body.push(
        button(text("Keep open").size(metrics.text_sm))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press(Message::DialogCancel),
    );
    super::dialog_card(metrics, body)
}
