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
use flavite_edit_context::{ContextCommand, ContextMenu};
use flavite_editor::context::layout::{self, ContextLayout};
use iced::widget::{
    Space, button, column, container, mouse_area, opaque, row, scrollable, stack, text, tooltip,
};
use iced::{Element, Length, alignment};

fn observed<'a>(
    app: &App,
    tab: flavite_workspace::TabId,
    name: &'static str,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    bounds::observe_named(
        content,
        format!(
            "context-{tab}-{name}-{:?}-{:?}-{:?}",
            app.editor_bounds.get(&tab),
            app.window_size(),
            app.context_mode.mode(tab)
        ),
        move |rect| Message::ContextMode(ContextMessage::Bounds(tab, name, rect)),
    )
}

fn action(
    app: &App,
    target: EditorTarget,
    command: ContextCommand,
    icon: Icon,
) -> Element<'_, Message> {
    observed(
        app,
        target.tab,
        command.id(),
        previewable(
            target.clone(),
            command,
            tooltip(
                button(chrome::icon(*app.appearance.metrics(), icon))
                    .width(app.appearance.metrics().icon_target)
                    .height(app.appearance.metrics().icon_target)
                    .padding(4)
                    .style(style::toolbar_button)
                    .on_press_maybe(
                        context_mode::available(app, target.clone(), command)
                            .then_some(Message::ContextMode(ContextMessage::Run(target, command))),
                    ),
                container(text(command.title()).size(app.appearance.metrics().text_sm))
                    .padding(6)
                    .style(style::card),
                tooltip::Position::Top,
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
            target.clone(),
            command,
            true,
        )))
        .on_exit(Message::ContextMode(ContextMessage::Preview(
            target, command, false,
        )))
        .into()
}

/// The mode bar replaces the ordinary note header while active.
pub fn view<'a>(app: &'a App, target: &EditorTarget) -> Element<'a, Message> {
    let metrics = *app.appearance.metrics();
    let width = app
        .editor_bounds
        .get(&target.tab)
        .map_or(640.0, |b| b.width)
        / metrics.scale;
    let mode = app.context_mode.mode(target.tab);
    let plan = ContextLayout::new(width);
    let mut actions = row![action(
        app,
        target.clone(),
        ContextCommand::Exit,
        Icon::Close
    )]
    .spacing(layout::GAP * metrics.scale)
    .align_y(alignment::Vertical::Center);
    if plan.shows_state() {
        actions = actions.push(observed(
            app,
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
            actions = actions.push(action(app, target.clone(), command, icon));
        }
    }
    if plan.command_visible(ContextCommand::Undo) {
        actions = actions
            .push(action(
                app,
                target.clone(),
                ContextCommand::Undo,
                Icon::Undo,
            ))
            .push(action(
                app,
                target.clone(),
                ContextCommand::Redo,
                Icon::Redo,
            ));
    }
    if plan.command_visible(ContextCommand::Commands) {
        actions = actions.push(action(
            app,
            target.clone(),
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
        actions = actions.push(action(
            app,
            target.clone(),
            ContextCommand::Find,
            Icon::Search,
        ));
    }
    if plan.command_visible(ContextCommand::History) {
        actions = actions.push(observed(
            app,
            target.tab,
            ContextCommand::History.id(),
            button(text("History").size(metrics.text_sm))
                .padding([5.0 * metrics.scale, 7.0 * metrics.scale])
                .width(layout::command_width(ContextCommand::History) * metrics.scale)
                .style(style::ghost())
                .on_press_maybe(
                    context_mode::available(app, target.clone(), ContextCommand::History)
                        .then_some({
                            Message::EditorHistory(
                                crate::host::editor_history::HistoryMessage::Act(
                                    target.clone(),
                                    crate::host::editor_history::HistoryAction::Open,
                                ),
                            )
                        }),
                ),
        ));
    }
    if plan.menu_visible(ContextMenu::More) {
        actions = actions
            .push(Space::new().width(Length::Fill))
            .push(menu_button(app, target, ContextMenu::More));
    }
    // The count caption: primary characters at N=1, "N selections" once the
    // set grows (FEUX-0002 X04/D-18; non-color redundancy).
    let member_count = target.selections.len();
    let count = app.note_editor_view(target.tab).map_or(0, |view| {
        flavite_edit_context::selected_characters(&view.text, target.selection)
    });
    let message = app
        .context_mode
        .session
        .as_ref()
        .map_or("", |s| s.state.message().0);
    let max_chars = ((width - 130.0).max(40.0) / 6.5) as usize;
    let mut caption: String = message.chars().take(max_chars).collect();
    if message.chars().count() > max_chars {
        caption.push('…');
    }
    let summary = if member_count > 1 {
        format!("{member_count} selections")
    } else if count == 0 {
        "Caret".into()
    } else {
        format!("{count} char{}", if count == 1 { "" } else { "s" })
    };
    let effect = context_mode::effect_caption(app, target.tab);
    let detail = crate::views::editor_history::strip(app, target.clone(), summary, caption, effect);
    let mut rows: Vec<Element<'_, Message>> = vec![
        container(actions)
            .height(36.0 * metrics.scale)
            .align_y(alignment::Vertical::Center)
            .into(),
    ];
    // The burst hint (FEUX-0002 E-15, X07): non-blocking, dismissible,
    // offers only Exit Context Mode and Open history — never an undo
    // action (shared-buffer edits make that promise false).
    if app.diagnostics.burst_hint {
        rows.push(
            container(
                row![
                    text(
                        "Several edits in quick succession — exit Context Mode or review editing history",
                    )
                    .size(metrics.text_sm),
                    Space::new().width(Length::Fill),
                    observed(
                        app,
                        target.tab,
                        "burst-exit",
                        button(text("Exit Context Mode").size(metrics.text_sm))
                            .padding([3.0 * metrics.scale, 6.0 * metrics.scale])
                            .style(style::ghost())
                            .on_press(Message::EditorDiagnostics(
                                crate::host::editor_diagnostics::DiagnosticsMessage::ExitContextMode(
                                    target.tab,
                                ),
                            )),
                    ),
                    observed(
                        app,
                        target.tab,
                        "burst-history",
                        button(text("Open history").size(metrics.text_sm))
                            .padding([3.0 * metrics.scale, 6.0 * metrics.scale])
                            .style(style::ghost())
                            .on_press(Message::EditorDiagnostics(
                                crate::host::editor_diagnostics::DiagnosticsMessage::OpenHistory(
                                    target.tab,
                                ),
                            )),
                    ),
                    observed(
                        app,
                        target.tab,
                        "burst-dismiss",
                        button(text("✕").size(metrics.text_sm))
                            .padding([3.0 * metrics.scale, 5.0 * metrics.scale])
                            .style(style::ghost())
                            .on_press(Message::EditorDiagnostics(
                                crate::host::editor_diagnostics::DiagnosticsMessage::DismissBurstHint,
                            )),
                    ),
                ]
                .spacing(6.0 * metrics.scale)
                .align_y(alignment::Vertical::Center),
            )
            .padding([3.0 * metrics.scale, 0.0])
            .width(Length::Fill)
            .into(),
        );
    }
    rows.push(detail);
    observed(
        app,
        target.tab,
        "toolbar",
        container(column(rows))
            .padding([0.0, 6.0 * metrics.scale])
            .width(Length::Fill)
            .style(style::toolbar),
    )
}

fn menu_button<'a>(app: &App, target: &EditorTarget, menu: ContextMenu) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let open = app
        .context_mode
        .session
        .as_ref()
        .is_some_and(|s| s.state.menu() == Some(menu));
    observed(
        app,
        target.tab,
        menu.title(),
        button(text(format!("{} ▾", menu.title())).size(metrics.text_sm))
            .padding([5.0 * metrics.scale, 7.0 * metrics.scale])
            .width(layout::menu_width(menu) * metrics.scale)
            .style(style::list_row(open))
            .on_press(Message::ContextMode(if open {
                ContextMessage::CloseMenu(target.tab)
            } else {
                ContextMessage::OpenMenu(target.clone(), menu)
            })),
    )
}

/// Dropdowns float over the editor; opening one never moves the text.
pub fn overlay<'a>(app: &'a App, shell: Element<'a, Message>) -> Element<'a, Message> {
    let Some(session) = &app.context_mode.session else {
        return shell;
    };
    let Some(menu) = session.state.menu() else {
        return shell;
    };
    let Some(target) = session.menu_target.clone() else {
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
            app,
            target.tab,
            command.title(),
            previewable(
                target.clone(),
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
                .style(style::list_row(index == session.state.selected()))
                .on_press_maybe(
                    context_mode::available(app, target.clone(), command).then_some({
                        Message::ContextMode(ContextMessage::Run(target.clone(), command))
                    }),
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
