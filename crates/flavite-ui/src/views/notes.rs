//! A shared, lazy vault file tree; directory reads run through the API worker.
use crate::{
    app::{App, Message},
    host::file_browser::{BrowserMessage, Owner},
    style,
    workspace::{bounds, tab_strip},
};
use flavite_workspace::TabId;
use iced::{
    Element, Length, alignment,
    widget::{Space, button, column, container, row, scrollable, text, text_input},
};

pub const NOTES_FILTER_ID_PREFIX: &str = "notes-filter-";
pub fn scroll_id(owner: Owner) -> iced::widget::Id {
    iced::widget::Id::from(format!("file-tree-{owner:?}"))
}

pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    content(
        app,
        Some(tab),
        app.notes_filter(tab).to_owned(),
        iced::widget::Id::from(format!("{NOTES_FILTER_ID_PREFIX}{}", tab.raw())),
    )
}
pub fn sidebar(app: &App) -> Element<'_, Message> {
    content(
        app,
        None,
        app.sidebar_filter.clone(),
        iced::widget::Id::new("sidebar-notes-filter"),
    )
}
fn message(action: BrowserMessage) -> Message {
    Message::FileBrowser(action)
}

fn content<'a>(
    app: &'a App,
    owner: Owner,
    filter: String,
    input_id: iced::widget::Id,
) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let rows = app.file_browser.rows(owner, &filter);
    let state = app.file_browser.views.get(&owner);
    let row_height = 30.0 * metrics.scale;
    let start = ((state.map_or(0.0, |s| s.scroll) / row_height) as usize)
        .saturating_sub(4)
        .min(rows.len());
    let end =
        (start + (app.window_size().height / row_height).ceil() as usize + 12).min(rows.len());
    let mut list = column![Space::new().height(start as f32 * row_height)].spacing(0);
    for entry in &rows[start..end] {
        let path = entry.entry.path.clone();
        let basename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let selected = state.is_some_and(|state| state.selected.as_ref() == Some(&path));
        let title = app
            .catalog()
            .iter()
            .find(|note| note.path == path)
            .map(|note| note.title.as_str())
            .filter(|title| *title != basename && !title.is_empty());
        let active = app
            .workspace
            .document_anchor()
            .and_then(|tab| app.notes.locator_of(tab))
            .is_some_and(|locator| {
                locator.components().iter().collect::<std::path::PathBuf>() == path
            });
        let glyph = if entry.entry.directory {
            if entry.expanded { "▾" } else { "▸" }
        } else if active {
            "●"
        } else {
            "·"
        };
        let label = row![
            text(glyph).size(metrics.text),
            text(basename)
                .size(metrics.text)
                .wrapping(iced::widget::text::Wrapping::None),
            Space::new().width(Length::Fill),
            text(title.unwrap_or("").to_owned())
                .size(metrics.text_sm)
                .style(style::dim_text)
        ]
        .spacing(metrics.spacing_sm)
        .align_y(alignment::Vertical::Center);
        let action = if entry.entry.directory {
            BrowserMessage::Toggle(owner, path.clone())
        } else {
            BrowserMessage::Open(owner, path.clone())
        };
        let control = button(label)
            .height(row_height)
            .width(Length::Fill)
            .padding(iced::Padding {
                top: 2.0,
                bottom: 2.0,
                left: 8.0 + entry.depth as f32 * 16.0 * metrics.scale,
                right: 8.0,
            })
            .style(style::list_row(selected || active))
            .on_press(message(action));
        let key = if entry.entry.directory {
            format!("folder:{}", path.display())
        } else {
            format!("note:{}", path.display())
        };
        list = list.push(bounds::observe_named(
            control,
            format!("tree-{owner:?}-{key}-{filter}"),
            move |rect| Message::SidebarControlBounds(key.clone(), rect),
        ));
    }
    list = list.push(Space::new().height((rows.len() - end) as f32 * row_height));
    // Directory paging is explicit; no first-500 cutoff makes files unreachable.
    let mut paging = column![].spacing(metrics.spacing_sm);
    for (path, directory) in &app.file_browser.directories {
        let visible =
            path.as_os_str().is_empty() || state.is_some_and(|s| s.expanded.contains(path));
        if visible && let Some(error) = &directory.error {
            paging = paging.push(text(error.clone()).style(style::dim_text));
        }
        if visible && directory.next.is_some() {
            paging = paging.push(
                button(text(format!(
                    "Load more in {}",
                    if path.as_os_str().is_empty() {
                        "vault root".into()
                    } else {
                        path.display().to_string()
                    }
                )))
                .on_press(message(BrowserMessage::More(path.clone())))
                .style(style::ghost()),
            );
        }
    }
    if rows.is_empty() {
        list = list.push(
            text(if app.file_browser.directories.is_empty() {
                "Loading vault folders…"
            } else if filter.is_empty() {
                "This folder has no visible files."
            } else {
                "No loaded files or indexed notes match."
            })
            .style(style::dim_text),
        );
    }
    if let Some(error) = app
        .file_browser
        .directories
        .get(std::path::Path::new(""))
        .and_then(|dir| dir.error.as_ref())
    {
        list = list.push(text(error.clone()).style(style::dim_text));
    }
    let mut heading = row![
        text(if owner.is_none() {
            "Vault root"
        } else {
            "Files"
        })
        .size(metrics.text_lg),
        Space::new().width(Length::Fill),
        button(text("Reveal"))
            .on_press(message(BrowserMessage::Reveal(owner)))
            .style(style::ghost()),
        button(text("Refresh"))
            .on_press(message(BrowserMessage::Refresh))
            .style(style::ghost())
    ]
    .spacing(metrics.spacing_sm)
    .align_y(alignment::Vertical::Center);
    if let Some(tab) = owner {
        heading = heading.push(crate::workspace::chrome::icon_button(
            *metrics,
            crate::workspace::chrome::Icon::Sidebar,
            "Move file browser into the sidebar",
            Some(Message::NotesToSidebar(tab)),
        ));
    }
    let input = text_input("Filter notes and loaded files…", &filter)
        .id(input_id)
        .on_input(move |value| {
            owner.map_or(Message::SidebarFilter(value.clone()), |tab| {
                Message::NotesFilter { tab, value }
            })
        })
        .padding(tab_strip::chip_padding())
        .size(metrics.text_sm);
    let input: Element<'a, Message> = if owner.is_none() {
        bounds::observe(input, |rect| {
            Message::SidebarControlBounds("filter".into(), rect)
        })
    } else {
        input.into()
    };
    container(
        column![
            heading,
            input,
            scrollable(list)
                .id(scroll_id(owner))
                .on_scroll(move |viewport| message(BrowserMessage::Scroll(
                    owner,
                    viewport.absolute_offset().y
                )))
                .height(Length::Fill),
            paging
        ]
        .spacing(metrics.spacing_sm),
    )
    .padding(metrics.spacing_lg)
    .width(Length::Fill)
    .height(Length::Fill)
    .style(style::content)
    .into()
}
