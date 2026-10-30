//! Source-reading note view (RFC-0003 W03: shared editing, per-view input).
//!
//! One editable note tab. The widget runtime is keyed by TabId and restores
//! the host-owned logical interaction state after reparenting. Reserved and
//! failed loads render honest retry/close states.

use crate::app::{App, Message};
use crate::style;
use crate::views::empty_state;
use crate::workspace::{
    bounds,
    chrome::{self, Icon},
    tab_strip,
};
use flavite_workspace::{LayoutAction, NoteLoad, TabId};
use iced::widget::{button, column, container, row, rule, text};
use iced::{Element, Length, alignment};

/// Render one note tab for the active view slot.
pub fn view<'a>(
    app: &'a App,
    tab: TabId,
    load: NoteLoad,
    locator: &'a flavite_workspace::NoteLocator,
) -> Element<'a, Message> {
    let path = locator.components().join("/");
    match load {
        NoteLoad::Reserved => container(
            column![
                text(format!("Opening {path}…")),
                button(text("Close tab"))
                    .padding(tab_strip::chip_padding())
                    .style(style::ghost())
                    .on_press(Message::Layout(LayoutAction::CloseTab {
                        expected_revision: app.workspace.revision(),
                        tab,
                    })),
            ]
            .spacing(style::SPACING),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(style::SPACING_LG)
        .style(style::content)
        .into(),
        NoteLoad::Failed => container(
            column![
                text("Note unavailable").size(style::HEADING),
                text(path.clone()).style(style::dim_text),
                row![
                    button(text("Retry"))
                        .padding(tab_strip::chip_padding())
                        .style(style::action())
                        .on_press(Message::OpenNote {
                            locator: locator.clone(),
                            group: app.group_of(tab),
                        }),
                    button(text("Close tab"))
                        .padding(tab_strip::chip_padding())
                        .style(style::ghost())
                        .on_press(Message::Layout(LayoutAction::CloseTab {
                            expected_revision: app.workspace.revision(),
                            tab,
                        })),
                ]
                .spacing(style::SPACING),
            ]
            .spacing(style::SPACING_SM),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(style::SPACING_LG)
        .style(style::content)
        .into(),
        NoteLoad::Ready => ready_view(app, tab, path),
    }
}

fn ready_view<'a>(app: &'a App, tab: TabId, path: String) -> Element<'a, Message> {
    let Some(view) = app.note_editor_view(tab) else {
        return empty_state("Buffer unavailable", "The note view needs to be reopened.");
    };
    let Some(incarnation) = app.notes.incarnation(tab) else {
        return empty_state("View unavailable", "This note view is no longer active.");
    };
    let initial_interaction = app.editor_interaction_snapshot(tab);
    let mut editor =
        flavite_editor::EditorWidget::from_view(view, move |action| Message::EditorAction {
            tab,
            incarnation,
            action,
        })
        .with_view_key(tab.raw(), incarnation)
        .with_input_enabled(!app.picker.open && !app.palette_open && app.strip_menu.is_none())
        .with_interaction(move |snapshot| Message::EditorAction {
            tab,
            incarnation,
            action: flavite_editor::EditorAction::Interaction(snapshot),
        });
    if let Some(interaction) = initial_interaction {
        editor = editor.with_initial_interaction(interaction);
    }
    let meta = container(
        row![
            chrome::icon(Icon::Note),
            text(path).size(style::TEXT_SM).width(Length::Fill),
            text("Source").size(style::TEXT_SM).style(style::dim_text)
        ]
        .spacing(8)
        .align_y(alignment::Vertical::Center),
    )
    .height(32)
    .padding([0, 12])
    .align_y(alignment::Vertical::Center)
    .style(style::toolbar);
    let state = app
        .note_buffer_state(tab)
        .unwrap_or_else(|| "Unavailable".into());
    let state = match state.as_str() {
        "Clean" => "Saved",
        "Dirty" => "Unsaved changes",
        other => other,
    };
    let footer = container(
        text(state.to_owned())
            .size(style::TEXT_SM)
            .style(style::dim_text),
    )
    .height(24)
    .padding([4, 12]);
    let editor = bounds::observe(
        editor.width(Length::Fill).height(Length::Fill),
        move |rect| Message::EditorBounds(tab, rect),
    );
    column![
        meta,
        rule::horizontal(1),
        editor,
        rule::horizontal(1),
        footer
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}
