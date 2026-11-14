//! One search workbench, shared by the sidebar and workspace tabs.
use crate::{
    app::{App, Message},
    host::vault_search::{SIDEBAR_SEARCH, VaultSearchMessage},
    style,
};
use flavite_workspace::TabId;
use iced::{
    Element, Length,
    widget::{button, column, container, row, text},
};

pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    let indexed = app.vault_search.indexed(app, tab);
    let metrics = app.appearance.metrics();
    let choose = |label, active, mode| {
        button(text(label).size(metrics.text_sm))
            .padding(4)
            .style(style::list_row(active))
            .on_press(Message::VaultSearch(VaultSearchMessage::Mode(tab, mode)))
    };
    let header = row![
        super::replace::observed_control(
            app,
            tab,
            "source",
            choose("Source find", !indexed, false)
        ),
        super::replace::observed_control(app, tab, "indexed", choose("Indexed", indexed, true)),
        super::replace::observed_control(
            app,
            tab,
            "dock",
            button(
                text(if tab == SIDEBAR_SEARCH {
                    "Open as tab"
                } else {
                    "Move to sidebar"
                })
                .size(metrics.text_sm)
            )
            .padding(4)
            .style(style::ghost())
            .on_press(Message::VaultSearch(if tab == SIDEBAR_SEARCH {
                VaultSearchMessage::Undock
            } else {
                VaultSearchMessage::Dock(tab)
            }))
        ),
    ]
    .spacing(metrics.spacing_sm)
    .wrap();
    let content = if indexed {
        super::search::content(app, tab)
    } else {
        super::replace::content(app, tab)
    };
    container(column![header, content].spacing(metrics.spacing_sm))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(4)
        .style(style::content)
        .into()
}
