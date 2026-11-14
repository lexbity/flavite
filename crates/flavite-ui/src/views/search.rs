//! The saved-content Search view (W10, RFC-0006 S06).
//!
//! Rendering only: the service state lives in [`crate::host::search`]. The
//! view shows the frozen snapshot/freshness/total with an explicit Refresh,
//! the mode selector (full-text default, indexed expressions explicit), and
//! continuation by ordinal ("Next page" appends once; replay never
//! duplicates rows).

use crate::app::{App, Message};
use crate::host::search::{SearchPanelMode, SearchRowView};
use crate::style;
use crate::workspace::tab_strip;
use flavite_workspace::TabId;
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length};

/// Input widget id prefix for semantic tests.
pub const SEARCH_INPUT_ID_PREFIX: &str = "search-input-";

pub(crate) fn input_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("{SEARCH_INPUT_ID_PREFIX}{}", tab.raw()))
}

/// Render the Search view for one tab. The input renders even before the
/// first edit so the view is immediately usable after opening.
#[allow(clippy::too_many_lines)]
pub(crate) fn content(app: &App, tab: TabId) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    let state = app.search.view_state(tab);
    let query = state.query.clone();
    let mut results = column![].spacing(2);
    for result in &state.rows {
        results = results.push(result_row(*metrics, result));
    }
    if state.rows.is_empty() && state.error.is_none() && !state.updating {
        results = results.push(
            text(if state.query.trim().is_empty() {
                "Search matches saved note content: words, tags, links, properties."
            } else {
                "No saved notes match."
            })
            .style(style::dim_text),
        );
    }
    let status = if state.updating {
        String::from("Searching…")
    } else if let Some(error) = &state.error {
        error.clone()
    } else if let (Some((epoch, generation)), Some(total)) = (state.snapshot, state.total) {
        format!(
            "Frozen snapshot {epoch}/{generation} · {total} results · saved content only — unsaved edits are not searched"
        )
    } else if state.snapshot.is_some() {
        String::from("Frozen snapshot · saved content only — unsaved edits are not searched")
    } else {
        String::from("Saved content only — unsaved edits are not searched")
    };
    let next_page = state.has_next().then(|| {
        button(text("Next page"))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press(Message::SearchNextPage { tab })
    });
    let refresh = state.snapshot.is_some().then(|| {
        button(text("Refresh"))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press(Message::SearchRefresh { tab })
    });
    let mode_button = |label: &'static str, mode: SearchPanelMode| {
        let active = state.mode == mode;
        button(text(label).style(if active {
            |theme: &iced::Theme| iced::widget::text::Style {
                color: Some(theme.palette().text),
            }
        } else {
            style::dim_text
        }))
        .padding(tab_strip::chip_padding())
        .style(move |theme, status| {
            if active {
                style::list_row(true)(theme, status)
            } else {
                style::ghost()(theme, status)
            }
        })
        .on_press_maybe((!active).then_some(Message::SearchSetMode { tab, mode }))
    };

    container(
        column![
            row![
                text_input("Search saved notes…", &query)
                    .id(input_id(tab))
                    .on_input(move |value| Message::SearchQuery { tab, value })
                    .on_submit(Message::SearchSubmitNow { tab })
                    .padding(tab_strip::chip_padding())
                    .size(metrics.text_sm)
                    .width(Length::Fill),
                mode_button("Text", SearchPanelMode::FullText),
                mode_button("Expression", SearchPanelMode::Expression),
            ]
            .spacing(metrics.spacing_sm),
            text(status).size(metrics.text_sm).style(style::dim_text),
            container(scrollable(results).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(metrics.spacing_sm)
                .style(style::card),
            row![
                Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                next_page,
                refresh,
            ]
            .spacing(metrics.spacing_sm),
        ]
        .spacing(metrics.spacing_sm),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(metrics.spacing_lg)
    .style(style::content)
    .into()
}

fn result_row(metrics: crate::style::Metrics, result: &SearchRowView) -> Element<'static, Message> {
    let locator = flavite_workspace::NoteLocator::from_relative(std::path::Path::new(&result.path));
    let open = locator.ok().map(|locator| {
        Message::OpenIntent(crate::host::open_intents::OpenIntent::reuse(locator, None))
    });
    let mut body = column![
        text(result.title.clone()),
        text(result.path.clone())
            .size(metrics.text_sm)
            .style(style::dim_text),
    ]
    .spacing(2);
    if !result.excerpt.is_empty() {
        body = body.push(
            text(result.excerpt.clone())
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    match open {
        Some(message) => button(body)
            .width(Length::Fill)
            .padding(iced::Padding {
                top: 6.0,
                bottom: 6.0,
                left: 10.0,
                right: 10.0,
            })
            .style(style::list_row(false))
            .on_press(message)
            .into(),
        None => container(body)
            .width(Length::Fill)
            .padding(iced::Padding {
                top: 6.0,
                bottom: 6.0,
                left: 10.0,
                right: 10.0,
            })
            .into(),
    }
}
