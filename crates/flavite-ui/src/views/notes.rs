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
pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    content(
        app,
        app.notes_filter(tab).to_owned(),
        filter_id(tab),
        move |value| Message::NotesFilter { tab, value },
        Some(tab),
    )
}

/// The same catalog and note-opening intents in the collapsible navigator.
pub fn sidebar(app: &App) -> Element<'_, Message> {
    content(
        app,
        app.sidebar_filter.clone(),
        iced::widget::Id::new("sidebar-notes-filter"),
        Message::SidebarFilter,
        None,
    )
}

fn content<'a>(
    app: &'a App,
    filter: String,
    id: iced::widget::Id,
    on_input: impl Fn(String) -> Message + 'a,
    tab: Option<TabId>,
) -> Element<'a, Message> {
    let compact = tab.is_none();
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
    if app.catalog().is_empty()
        && matches!(
            app.catalog_status(),
            crate::app::CatalogStatus::Loading | crate::app::CatalogStatus::Empty
        )
    {
        // Ticketed acquisition in flight (RFC-0004 W-D): honest loading
        // text over the existing empty-state pattern, never a blank list.
        return container(
            column![
                text("Notes").size(metrics.heading),
                text("Indexing notes…").style(style::dim_text),
            ]
            .spacing(metrics.spacing_sm),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(metrics.spacing_lg)
        .style(style::content)
        .into();
    }
    let candidates = app.note_candidates();
    let total = candidates.all().len();
    let matched: std::sync::Arc<[crate::picker::NoteCandidate]> = candidates.matches(&filter);
    let shown = matched.len();

    // Group by parent directory in canonical path order; the catalog is
    // already sorted, so folders arrive contiguously.
    let mut list = column![].spacing(2);
    let mut current_folder: Option<String> = None;
    const MAX_ROWS: usize = 500;
    let mut rendered = 0usize;
    for candidate in matched.iter() {
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
                    crate::workspace::chrome::icon(*metrics, crate::workspace::chrome::Icon::Notes),
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
        list = list.push(note_row(*metrics, candidate, compact, &filter));
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

    let mut heading = row![
        text(format!("{total} notes")).size(metrics.text_lg),
        Space::new().width(Length::Fill),
        button(text("Refresh"))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press(Message::RefreshCatalog)
    ]
    .spacing(metrics.spacing)
    .align_y(alignment::Vertical::Center);
    if let Some(tab) = tab {
        heading = heading.push(crate::workspace::chrome::icon_button(
            *metrics,
            crate::workspace::chrome::Icon::Sidebar,
            "Move Notes into the sidebar",
            Some(Message::NotesToSidebar(tab)),
        ));
    }
    let input: Element<'a, Message> = text_input("Filter notes…", &filter)
        .id(id)
        .on_input(on_input)
        .padding(tab_strip::chip_padding())
        .size(metrics.text_sm)
        .into();
    let input = if compact {
        crate::workspace::bounds::observe(input, |rect| {
            Message::SidebarControlBounds("filter".into(), rect)
        })
    } else {
        input
    };
    container(
        column![
            heading,
            input,
            iced::Element::from(if compact {
                Space::new().height(0).into()
            } else {
                Element::from(
                    text("Pick a note to open it")
                        .size(metrics.text_sm)
                        .style(style::dim_text),
                )
            }),
            container(scrollable(list).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(metrics.spacing_sm)
                .style(style::content),
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
    compact: bool,
    filter: &str,
) -> Element<'static, Message> {
    let label: Element<'static, Message> = if compact {
        text(candidate.title().to_owned())
            .size(metrics.text)
            .wrapping(iced::widget::text::Wrapping::None)
            .into()
    } else {
        row![
            text(candidate.title().to_owned()).width(Length::Fill),
            text(candidate.path().to_string())
                .size(metrics.text_sm)
                .style(style::dim_text)
        ]
        .spacing(metrics.spacing)
        .into()
    };
    let element: Element<'static, Message> = button(label)
        .width(Length::Fill)
        .padding(iced::Padding {
            top: 6.0,
            bottom: 6.0,
            left: 10.0,
            right: 10.0,
        })
        .style(style::list_row(false))
        .on_press(Message::OpenIntent(
            crate::host::open_intents::OpenIntent::reuse(candidate.locator.clone(), None),
        ))
        .into();
    if compact {
        let key = format!("note:{}", candidate.path());
        crate::workspace::bounds::observe_named(element, format!("{key}:{filter}"), move |rect| {
            Message::SidebarControlBounds(key.clone(), rect)
        })
    } else {
        element
    }
}
