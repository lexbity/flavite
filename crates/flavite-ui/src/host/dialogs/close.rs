//! The close-decision dialog (RFC-0003 W12, D-10): real Keep open /
//! Save close / Retain recovery / Discard choices. Discard is a separate
//! destructive action with a revision-bound confirmation. The quit-timeout
//! variant presents the typed consequential choices truthfully.

use crate::app::{App, Message};
use crate::host::closing::CloseDecision;
use crate::style;
use crate::workspace::tab_strip;
use iced::Element;
use iced::widget::{button, column, row, text};

/// Render the close-decision dialog for one tab.
pub fn view(
    app: &App,
    tab: flavite_workspace::TabId,
    metrics: style::Metrics,
) -> Element<'_, Message> {
    let Some(plan) = app.closing.as_ref().filter(|plan| plan.tab == tab) else {
        return column![].into();
    };
    let state_label = app.close_state_label(tab);
    let mut body = column![
        text("Unsaved changes").size(metrics.heading),
        text(format!("{} — {}", plan.path, state_label))
            .size(metrics.text_sm)
            .style(style::dim_text),
        text(match plan.origin {
            crate::host::closing::CloseOrigin::Single => {
                "This is the last view of the note. Choose what happens to the unsaved work."
            }
            crate::host::closing::CloseOrigin::Group => {
                "Closing this group: decide this note, or keep it open to stop."
            }
            crate::host::closing::CloseOrigin::Quit => {
                "Quitting: decide this note, or keep it open to cancel the quit."
            }
        })
        .size(metrics.text_sm)
        .style(style::dim_text),
    ]
    .spacing(metrics.spacing_sm);

    body = body.push(
        row![
            button(text("Save and close").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press(Message::CloseDecision(CloseDecision::SaveClose)),
            button(text("Close, keep a recoverable copy").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::CloseDecision(CloseDecision::RetainRecovery)),
        ]
        .spacing(metrics.spacing_sm),
    );
    // Discard is a separate destructive action: arm first, confirm second,
    // bound to the revision the user actually saw.
    match plan.discard_armed {
        None => {
            body = body.push(
                button(text("Discard changes…").size(metrics.text_sm))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::CloseDiscardArm(true)),
            );
        }
        Some(_revision) => {
            body = body.push(
                row![
                    button(
                        text("Discard now (cannot be undone by this dialog)").size(metrics.text_sm)
                    )
                    .padding(tab_strip::chip_padding())
                    .style(move |theme, status| style::list_row(true)(theme, status))
                    .on_press(Message::CloseDiscardConfirm),
                    button(text("Keep edit dialog").size(metrics.text_sm))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::CloseDiscardArm(false)),
                ]
                .spacing(metrics.spacing_sm),
            );
        }
    }
    body = body.push(
        button(text("Keep open").size(metrics.text_sm))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press(Message::CloseKeepOpen),
    );
    super::dialog_card(metrics, body)
}

/// The quit-timeout dialog: typed concern, explicit consequential choices.
pub fn shutdown_view<'a>(
    _app: &App,
    concern: &'a str,
    metrics: style::Metrics,
) -> Element<'a, Message> {
    use crate::host::shutdown::ShutdownMessage;
    let body = column![
        text("Quit with unresolved work?").size(metrics.heading),
        text(concern.to_owned())
            .size(metrics.text)
            .style(style::dim_text),
        row![
            button(text("Keep waiting").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press(Message::Shutdown(ShutdownMessage::KeepWaiting)),
            button(text("Quit anyway").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::Shutdown(ShutdownMessage::QuitAnyway)),
            button(text("Cancel quit").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::Shutdown(ShutdownMessage::Cancel)),
        ]
        .spacing(metrics.spacing_sm),
    ]
    .spacing(metrics.spacing_sm);
    super::dialog_card(metrics, body)
}
