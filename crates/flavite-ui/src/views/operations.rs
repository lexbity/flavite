//! Durable replacement operations view (RFC-0006 S08): renders the
//! aggregated live job inventory (phase + totals) from the operations host.
//! Rendering only; controls live on the Replace tab.

use crate::app::{App, Message};
use crate::host::operations::OperationRow;
use crate::style;
use flavite_workspace::TabId;
use iced::widget::{button, column, container, scrollable, text};
use iced::{Element, Length};

/// Render the operations panel for one tab.
pub fn view(app: &App, _tab: TabId) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    let rows = app.replace_operations.rows(&app.replace);
    let mut body = column![].spacing(2);
    if rows.is_empty() {
        body = body.push(
            text("No durable replacement operations.")
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    for operation in &rows {
        body = body.push(operation_row(operation));
    }
    let summary = app.replace_operations.summary(&app.replace);
    let summary_text = if summary.is_empty() {
        String::new()
    } else {
        summary
            .into_iter()
            .map(|(phase, count)| format!("{phase}: {count}"))
            .collect::<Vec<_>>()
            .join(" · ")
    };
    container(
        column![
            text("Durable replacement operations")
                .size(metrics.heading)
                .style(style::dim_text),
            text(summary_text)
                .size(metrics.text_sm)
                .style(style::dim_text),
            container(scrollable(body).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(metrics.spacing_sm)
                .style(style::card),
        ]
        .spacing(metrics.spacing_sm),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(metrics.spacing_lg)
    .style(style::content)
    .into()
}

fn operation_row(operation: &OperationRow) -> Element<'static, Message> {
    let job = &operation.job;
    let mut label = format!(
        "{} · {} · selected {} · saved {} · conflicts {} · stale {} · recovery {}",
        operation.tab.raw(),
        job.phase,
        job.selected,
        job.saved,
        job.conflicts,
        job.stale,
        job.needs_recovery,
    );
    if job.index_pending > 0 {
        label.push_str(&format!(" · index pending {}", job.index_pending));
    }
    if job.not_started > 0 {
        label.push_str(&format!(" · not started {}", job.not_started));
    }
    let open = Message::ReplaceFocusTab { tab: operation.tab };
    button(text(label))
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 4.0,
            bottom: 4.0,
            left: 8.0,
            right: 8.0,
        })
        .style(style::list_row(false))
        .on_press(open)
        .into()
}
