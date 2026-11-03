//! The Notes view (W10): the vault note list with folder grouping and an
//! in-memory filter, moved out of the W02 shell and de-duplicated with the
//! picker matcher service. Catalog rows come from the bounded API snapshot;
//! no filesystem or index access happens here.

use crate::app::{App, Message};
use crate::picker;
use crate::style;
use crate::workspace::tab_strip;
use flavite_workspace::TabId;
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, alignment};

/// Filter input widget id for semantic tests.
pub const NOTES_FILTER_ID_PREFIX: &str = "notes-filter-";

fn filter_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("{NOTES_FILTER_ID_PREFIX}{}", tab.raw()))
}

/// Render the Notes list for one tab. `group` is where picked notes open.
pub fn view(app: &App, tab: TabId, group: flavite_workspace::GroupId) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    if let Some(error) = app.catalog_error() {
        return container(
            column![
                text("Notes unavailable").size(metrics.heading),
                text(error.to_owned()).style(style::dim_text),
                button(text("Retry"))
                    .padding(tab_strip::chip_padding())
                    .style(style::action())
                    .on_press(Message::RefreshCatalog),
            ]
            .spacing(metrics.spacing_sm),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(metrics.spacing_lg)
        .style(style::content)
        .into();
    }
    let filter = app.notes_filter(tab).to_owned();
    let candidates = app.note_candidates();
    let total = candidates.all().len();
    let matched: Vec<crate::picker::NoteCandidate> = candidates.matches(&filter);
    let shown = matched.len();

    // Group by parent directory in canonical path order; the catalog is
    // already sorted, so folders arrive contiguously.
    let mut list = column![].spacing(2);
    let mut current_folder: Option<String> = None;
    const MAX_ROWS: usize = 500;
    let mut rendered = 0usize;
    for candidate in &matched {
        if rendered >= MAX_ROWS {
            break;
        }
        let folder = candidate
            .path()
            .to_string()
            .rsplit_once('/')
            .map_or_else(String::new, |(parent, _)| parent.to_owned());
        if current_folder.as_deref() != Some(folder.as_str()) {
            current_folder = Some(folder.clone());
            list = list.push(
                row![
                    text("📁").size(metrics.text_sm),
                    text(if folder.is_empty() {
                        "Vault root".to_owned()
                    } else {
                        folder
                    })
                    .size(metrics.text_sm)
                    .style(style::dim_text),
                ]
                .spacing(metrics.spacing_sm)
                .padding(iced::Padding {
                    top: 4.0,
                    bottom: 2.0,
                    left: 0.0,
                    right: 0.0,
                }),
            );
        }
        list = list.push(note_row(*metrics, candidate, group));
        rendered += 1;
    }
    if shown > rendered {
        list = list.push(
            text(format!("{shown} notes match; showing the first {rendered}"))
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    if shown == 0 {
        list = list.push(
            text(if total == 0 {
                "This vault has no notes yet. Create one from the File menu."
            } else {
                "No notes match this filter."
            })
            .style(style::dim_text),
        );
    }

    container(
        column![
            row![
                text(format!("{total} notes")).size(metrics.text_lg),
                Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                button(text("Refresh"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::RefreshCatalog),
            ]
            .spacing(metrics.spacing)
            .align_y(alignment::Vertical::Center),
            text_input("Filter notes…", &filter)
                .id(filter_id(tab))
                .on_input(move |value| Message::NotesFilter { tab, value })
                .padding(tab_strip::chip_padding())
                .size(metrics.text_sm),
            text("Pick a note to open it in this group")
                .size(metrics.text_sm)
                .style(style::dim_text),
            container(scrollable(list).height(Length::Fill))
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

/// One selectable note row (title plus native path), shared by grouping.
/// The row owns its content, so it can outlive the candidate snapshot.
fn note_row(
    metrics: crate::style::Metrics,
    candidate: &picker::NoteCandidate,
    group: flavite_workspace::GroupId,
) -> Element<'static, Message> {
    button(
        row![
            text(candidate.title().to_owned()).width(Length::Fill),
            text(candidate.path().to_string())
                .size(metrics.text_sm)
                .style(style::dim_text),
        ]
        .spacing(metrics.spacing),
    )
    .width(Length::Fill)
    .padding(iced::Padding {
        top: 6.0,
        bottom: 6.0,
        left: 10.0,
        right: 10.0,
    })
    .style(style::list_row(false))
    .on_press(Message::OpenNote {
        locator: candidate.locator.clone(),
        group: Some(group),
    })
    .into()
}
