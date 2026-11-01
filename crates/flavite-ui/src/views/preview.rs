//! Rendered, source-bound Preview with an explicit Source preview option.
use crate::{
    app::{App, Message},
    host::reading,
    style,
    views::{context_header, properties},
};
use flavite_workspace::TabId;
use iced::widget::{button, column, container, row, rule, scrollable, text};
use iced::{Element, Length};
pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    let body: Element<'_, Message> = if let Some(doc) = reading::document(app, tab) {
        let source = app.reading.source_previews.contains(&tab);
        let open = flavite_workspace::NoteLocator::from_relative(&doc.path).ok();
        let toolbar = row![
            button("Open note").style(style::action()).on_press_maybe(
                open.clone()
                    .map(|locator| Message::OpenNoteFromContext { locator })
            ),
            button("Insert link").style(style::ghost()).on_press_maybe(
                app.valid_writing_target()
                    .and(open)
                    .map(|locator| Message::InsertLink { locator })
            ),
            button(if source {
                "Rendered preview"
            } else {
                "Source preview"
            })
            .style(style::ghost())
            .on_press(Message::ToggleSourcePreview(tab)),
        ]
        .spacing(8);
        let content = if source {
            scrollable(
                text(doc.text.to_string())
                    .font(iced::Font::MONOSPACE)
                    .size(14),
            )
            .height(Length::Fill)
            .into()
        } else {
            crate::read::view(app, tab, doc)
        };
        column![container(toolbar).padding(8), content]
            .height(Length::Fill)
            .into()
    } else {
        properties::loading(app, tab)
    };
    column![context_header::view(app, tab), rule::horizontal(1), body]
        .height(Length::Fill)
        .into()
}
