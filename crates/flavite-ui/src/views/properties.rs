//! Current frontmatter inspection and conservative typed scalar edits.
use crate::{
    app::{App, Message},
    host::{context::ToolState, reading},
    style,
    views::{context_header, empty_state},
};
use flavite_api::{ScalarValue, inspect_properties};
use flavite_workspace::TabId;
use iced::widget::{button, column, container, row, rule, scrollable, text, text_input};
use iced::{Element, Length};
pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    let body = if let Some(doc) = reading::document(app, tab) {
        let mut fields = column![].spacing(12);
        let open = || {
            button("Edit source").style(style::action()).on_press_maybe(
                flavite_workspace::NoteLocator::from_relative(&doc.path)
                    .ok()
                    .map(|locator| Message::EditSourceFromContext { locator }),
            )
        };
        match inspect_properties(&doc.text) {
            Ok(properties) => {
                for property in properties {
                    let value = match &property.value {
                        ScalarValue::Text(t) | ScalarValue::Number(t) => t.clone(),
                        ScalarValue::Bool(b) => b.to_string(),
                        ScalarValue::Null => "null".into(),
                    };
                    let key = property.key;
                    let draft = app
                        .reading
                        .property_drafts
                        .get(&(tab, key.clone()))
                        .cloned()
                        .unwrap_or(value);
                    let input_key = key.clone();
                    let document = doc.clone();
                    let input = text_input("Property value", &draft).on_input(move |value| {
                        Message::PropertyDraft {
                            tab,
                            document: document.clone(),
                            key: input_key.clone(),
                            value,
                        }
                    });
                    let can_edit = reading::can_edit(app, doc);
                    fields = fields.push(
                        column![
                            text(format!(
                                "{key} · {}",
                                match property.value {
                                    ScalarValue::Text(_) => "text",
                                    ScalarValue::Number(_) => "number",
                                    ScalarValue::Bool(_) => "boolean",
                                    ScalarValue::Null => "null",
                                }
                            ))
                            .size(12),
                            row![
                                reading::observe_control(
                                    tab,
                                    doc,
                                    "property-input",
                                    property.range.start,
                                    input
                                ),
                                reading::observe_control(
                                    tab,
                                    doc,
                                    "property-apply",
                                    property.range.start,
                                    button("Apply").style(style::action()).on_press_maybe(
                                        can_edit.then(|| Message::PropertyApply {
                                            tab,
                                            document: doc.clone(),
                                            key: key.clone()
                                        })
                                    )
                                )
                            ]
                            .spacing(8)
                        ]
                        .spacing(4),
                    );
                }
                if doc.document.properties.is_none() {
                    fields = fields.push(text("No frontmatter"));
                }
                if doc.buffer.is_none() {
                    fields =
                        fields.push(text("Open the source note to edit its properties.").size(12));
                }
                fields = fields.push(open());
            }
            Err(reason) => {
                if let Some(frontmatter) = &doc.document.properties {
                    fields = fields.push(
                        text(frontmatter.yaml_text.clone())
                            .font(iced::Font::MONOSPACE)
                            .size(14),
                    );
                }
                fields = fields.push(text(reason).size(12)).push(open());
            }
        }
        scrollable(container(fields).padding(16))
            .height(Length::Fill)
            .into()
    } else {
        loading(app, tab)
    };
    column![context_header::view(app, tab), rule::horizontal(1), body]
        .height(Length::Fill)
        .into()
}
pub(crate) fn loading(app: &App, tab: TabId) -> Element<'_, Message> {
    match app.contexts.state(&tab) {
        Some(ToolState::Failed { message }) => container(
            column![
                text(message),
                button("Retry").on_press(Message::RequeryContext { tab })
            ]
            .spacing(12),
        )
        .padding(16)
        .into(),
        Some(ToolState::Missing) => empty_state(
            "Missing note",
            "Locate the bound source or choose another binding.",
        ),
        Some(ToolState::Unbound { .. }) => empty_state(
            "No selection",
            "Choose a source using the binding controls.",
        ),
        Some(ToolState::Busy) => empty_state("Queued", "The query service is busy."),
        _ => empty_state("Loading…", "Reading the current source structure."),
    }
}
