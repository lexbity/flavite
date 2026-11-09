//! Compact, horizontal Context Mode controls. Exit never enters overflow.

use crate::{
    app::{App, Message},
    host::context_mode::{self, ContextMessage, EditorTarget},
    style,
    workspace::{
        bounds,
        chrome::{self, Icon},
    },
};
use flavite_editor::context::{
    ContextCommand, ContextMenu,
    layout::{self, ContextLayout},
};
use iced::widget::{
    Space, button, column, container, mouse_area, opaque, row, scrollable, stack, text,
};
use iced::{Element, Length, alignment};

fn observed<'a>(
    tab: flavite_workspace::TabId,
    name: &'static str,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    bounds::observe(content, move |rect| {
        Message::ContextMode(ContextMessage::Bounds(tab, name, rect))
    })
}

fn action(
    app: &App,
    target: EditorTarget,
    command: ContextCommand,
    icon: Icon,
) -> Element<'_, Message> {
    observed(
        target.tab,
        command.id(),
        previewable(
            target,
            command,
            chrome::icon_button(
                *app.appearance.metrics(),
                icon,
                command.title(),
                context_mode::available(app, target, command)
                    .then_some(Message::ContextMode(ContextMessage::Run(target, command))),
            ),
        ),
    )
}

fn previewable<'a>(
    target: EditorTarget,
    command: ContextCommand,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    mouse_area(content)
        .on_enter(Message::ContextMode(ContextMessage::Preview(
            target, command, true,
        )))
        .on_exit(Message::ContextMode(ContextMessage::Preview(
            target, command, false,
        )))
        .into()
}

/// The mode bar replaces the ordinary note header while active.
pub fn view(app: &App, target: EditorTarget) -> Element<'_, Message> {
    let metrics = *app.appearance.metrics();
    let width = app
        .editor_bounds
        .get(&target.tab)
        .map_or(640.0, |b| b.width)
        / metrics.scale;
    let mode = app.context_mode.mode(target.tab);
    let plan = ContextLayout::new(width);
    let mut actions = row![action(app, target, ContextCommand::Exit, Icon::Close)]
        .spacing(layout::GAP * metrics.scale)
        .align_y(alignment::Vertical::Center);
    if plan.shows_state() {
        actions = actions.push(observed(
            target.tab,
            "state",
            container(text(mode.label()).size(metrics.text_sm))
                .width(layout::STATE_WIDTH * metrics.scale),
        ));
    }
    if plan.command_visible(ContextCommand::Copy) {
        for (command, icon) in [
            (ContextCommand::Cut, Icon::Cut),
            (ContextCommand::Copy, Icon::Copy),
            (ContextCommand::Paste, Icon::Paste),
        ] {
            actions = actions.push(action(app, target, command, icon));
        }
    }
    if plan.command_visible(ContextCommand::Undo) {
        actions = actions
            .push(action(app, target, ContextCommand::Undo, Icon::Undo))
            .push(action(app, target, ContextCommand::Redo, Icon::Redo));
    }
    if plan.command_visible(ContextCommand::Commands) {
        actions = actions.push(action(
            app,
            target,
            ContextCommand::Commands,
            Icon::Commands,
        ));
    }
    if plan.menu_visible(ContextMenu::Select) {
        for menu in [ContextMenu::Select, ContextMenu::Go] {
            actions = actions.push(menu_button(app, target, menu));
        }
    }
    if plan.menu_visible(ContextMenu::Type) {
        actions = actions.push(menu_button(app, target, ContextMenu::Type));
    }
    if plan.command_visible(ContextCommand::Find) {
        actions = actions.push(action(app, target, ContextCommand::Find, Icon::Search));
    }
    if plan.command_visible(ContextCommand::History) {
        actions = actions.push(observed(
            target.tab,
            ContextCommand::History.id(),
            button(text("History").size(metrics.text_sm))
                .padding([5.0 * metrics.scale, 7.0 * metrics.scale])
                .width(layout::command_width(ContextCommand::History) * metrics.scale)
                .style(style::ghost())
                .on_press_maybe(
                    context_mode::available(app, target, ContextCommand::History).then_some(
                        Message::EditorHistory(crate::host::editor_history::HistoryMessage::Act(
                            target,
                            crate::host::editor_history::HistoryAction::Open,
                        )),
                    ),
                ),
        ));
    }
    if plan.menu_visible(ContextMenu::More) {
        actions = actions
            .push(Space::new().width(Length::Fill))
            .push(menu_button(app, target, ContextMenu::More));
    }
    let count = app.note_editor_view(target.tab).map_or(0, |view| {
        flavite_editor::context::selected_characters(&view.text, target.selection)
    });
    let message = app
        .context_mode
        .session
        .as_ref()
        .map_or("", |s| s.message.as_str());
    let max_chars = ((width - 130.0).max(40.0) / 6.5) as usize;
    let mut caption: String = message.chars().take(max_chars).collect();
    if message.chars().count() > max_chars {
        caption.push('…');
    }
    let summary = if count == 0 {
        "Caret".into()
    } else {
        format!("{count} char{}", if count == 1 { "" } else { "s" })
    };
    let effect = context_mode::effect_caption(app, target.tab);
    let detail = crate::views::editor_history::strip(app, target, summary, caption, effect);
    observed(
        target.tab,
        "toolbar",
        container(column![
            container(actions)
                .height(36.0 * metrics.scale)
                .align_y(alignment::Vertical::Center),
            detail
        ])
        .padding([0.0, 6.0 * metrics.scale])
        .width(Length::Fill)
        .style(style::toolbar),
    )
}

fn menu_button(app: &App, target: EditorTarget, menu: ContextMenu) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    let open = app
        .context_mode
        .session
        .as_ref()
        .is_some_and(|s| s.menu == Some(menu));
    observed(
        target.tab,
        menu.title(),
        button(text(format!("{} ▾", menu.title())).size(metrics.text_sm))
            .padding([5.0 * metrics.scale, 7.0 * metrics.scale])
            .width(layout::menu_width(menu) * metrics.scale)
            .style(style::list_row(open))
            .on_press(Message::ContextMode(if open {
                ContextMessage::CloseMenu(target.tab)
            } else {
                ContextMessage::OpenMenu(target, menu)
            })),
    )
}

/// Dropdowns float over the editor; opening one never moves the text.
pub fn overlay<'a>(app: &'a App, shell: Element<'a, Message>) -> Element<'a, Message> {
    let Some(session) = &app.context_mode.session else {
        return shell;
    };
    let Some(menu) = session.menu else {
        return shell;
    };
    let Some(target) = session.menu_target else {
        return shell;
    };
    let Some(anchor) = app
        .context_mode
        .bounds
        .get(&(session.tab, menu.title()))
        .or_else(|| app.context_mode.bounds.get(&(session.tab, "toolbar")))
    else {
        return shell;
    };
    let metrics = app.appearance.metrics();
    let width = (270.0 * metrics.scale).min(app.window_size().width - 8.0);
    let x = anchor
        .x
        .min((app.window_size().width - width - 4.0).max(0.0));
    let y = anchor.y + anchor.height;
    let mut entries = column![
        text(format!("{} · Context Mode", menu.title()))
            .size(metrics.text_sm)
            .style(style::dim_text)
    ]
    .spacing(2.0 * metrics.scale);
    for (index, &command) in menu.commands().iter().enumerate() {
        entries = entries.push(observed(
            target.tab,
            command.title(),
            previewable(
                target,
                command,
                button(
                    row![
                        text(command.title()).size(metrics.text),
                        Space::new().width(Length::Fill),
                        text(menu.hint(command))
                            .size(metrics.text_sm)
                            .style(style::dim_text)
                    ]
                    .spacing(12.0 * metrics.scale),
                )
                .width(Length::Fill)
                .padding([7.0 * metrics.scale, 10.0 * metrics.scale])
                .style(style::list_row(index == session.selected))
                .on_press_maybe(
                    context_mode::available(app, target, command)
                        .then_some(Message::ContextMode(ContextMessage::Run(target, command))),
                ),
            ),
        ));
    }
    let card = container(
        scrollable(entries)
            .id(context_mode::menu_scroll_id(target.tab))
            .height(Length::Shrink),
    )
    .width(width)
    .max_height(
        (app.window_size().height - y - 8.0)
            .max(80.0)
            .min(430.0 * metrics.scale),
    )
    .padding(5.0 * metrics.scale)
    .style(style::palette_card);
    let content = stack![
        shell,
        opaque(
            button(Space::new().width(Length::Fill).height(Length::Fill))
                .style(style::menu_backdrop)
                .on_press(Message::ContextMode(ContextMessage::CloseMenu(target.tab)))
        ),
        iced::widget::pin(opaque(card)).x(x).y(y)
    ]
    .into();
    exit_overlay(app, content)
}

/// An owned menu/palette backdrop must never swallow the Exit button.
pub fn exit_overlay<'a>(app: &'a App, content: Element<'a, Message>) -> Element<'a, Message> {
    let Some(session) = &app.context_mode.session else {
        return content;
    };
    let Some(target) = EditorTarget::capture(app, session.tab) else {
        return content;
    };
    let Some(rect) = app
        .context_mode
        .bounds
        .get(&(session.tab, ContextCommand::Exit.id()))
    else {
        return content;
    };
    stack![
        content,
        iced::widget::pin(opaque(action(
            app,
            target,
            ContextCommand::Exit,
            Icon::Close
        )))
        .x(rect.x)
        .y(rect.y)
    ]
    .into()
}
