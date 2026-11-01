//! Source-bound current heading navigation, independent of keyboard focus.
use crate::{
    app::{App, Message},
    host::reading,
    style,
    views::{context_header, empty_state},
};
use flavite_workspace::TabId;
use iced::widget::{button, column, container, rule, scrollable, text};
use iced::{Element, Length};
pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    let body = if let Some(doc) = reading::document(app, tab) {
        if doc.document.outline.is_empty() {
            empty_state("No headings", "Add a Markdown heading in the source note.")
        } else {
            let items = doc
                .document
                .outline
                .iter()
                .fold(column![].spacing(4), |items, heading| {
                    items.push(reading::observe_control(
                        tab,
                        doc,
                        "outline",
                        heading.byte,
                        button(text(format!(
                            "{}{}",
                            "  ".repeat(usize::from(heading.level.saturating_sub(1))),
                            heading.title
                        )))
                        .style(style::ghost())
                        .on_press_maybe(reading::current(app, doc).then(|| {
                            Message::OutlineNavigate {
                                tab,
                                document: doc.clone(),
                                byte: heading.byte,
                            }
                        })),
                    ))
                });
            scrollable(container(items).padding(12))
                .height(Length::Fill)
                .into()
        }
    } else {
        super::properties::loading(app, tab)
    };
    column![context_header::view(app, tab), rule::horizontal(1), body]
        .height(Length::Fill)
        .into()
}
