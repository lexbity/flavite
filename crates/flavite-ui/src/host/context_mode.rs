//! Target-bound Context Mode controls over the existing editor input and API.

use crate::app::{App, Message};
use flavite_api::{Invocation, Origin};
use flavite_editor::{
    context::{ContextCommand, ContextMenu, ContextMode},
    input::CapturedInput,
    keymap::KeyAction,
};
use flavite_workspace::{LayoutAction, TabId};
use iced::{Rectangle, Task};
use std::collections::BTreeMap;

/// A control captures its editor identity and selection, not global focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditorTarget {
    pub tab: TabId,
    pub incarnation: u64,
    pub buffer: u64,
    pub revision: u64,
    pub selection: (usize, usize),
}

impl EditorTarget {
    #[must_use]
    pub fn capture(app: &App, tab: TabId) -> Option<Self> {
        let view = app.note_editor_view(tab)?;
        let selection = app.editor_interaction_snapshot(tab)?.selection;
        Some(Self {
            tab,
            incarnation: app.notes.incarnation(tab)?,
            buffer: view.buffer_id,
            revision: view.revision,
            selection,
        })
    }
}

#[derive(Debug, Clone)]
pub enum ContextMessage {
    Run(EditorTarget, ContextCommand),
    EnterAt(EditorTarget, (usize, usize)),
    OpenMenu(EditorTarget, ContextMenu),
    CloseMenu(TabId),
    MenuMove(bool),
    MenuPick,
    PasteReady(EditorTarget, u64, Option<String>),
    Unbound(EditorTarget),
    Bounds(TabId, &'static str, Rectangle),
    ExpandPane(EditorTarget),
}

#[derive(Debug)]
pub struct ContextSession {
    pub tab: TabId,
    incarnation: u64,
    buffer: u64,
    pub mode: ContextMode,
    pub menu: Option<ContextMenu>,
    pub menu_target: Option<EditorTarget>,
    pub selected: usize,
    pub message: String,
    pub attention: bool,
}

#[derive(Debug, Default)]
pub struct ContextHost {
    pub history: super::editor_history::HistoryHost,
    pub session: Option<ContextSession>,
    pub bounds: BTreeMap<(TabId, &'static str), Rectangle>,
    generation: u64,
    internal_selection: bool,
    pub palette_target: Option<EditorTarget>,
    pub capture: Option<super::input::ContextCapture>,
}

impl ContextHost {
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    #[must_use]
    pub fn mode(&self, tab: TabId) -> ContextMode {
        self.session
            .as_ref()
            .filter(|s| s.tab == tab)
            .map_or(ContextMode::Off, |s| s.mode)
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        if let Some(session) = &mut self.session {
            session.message = text.into();
            session.attention = true;
        }
    }

    pub fn feedback(&mut self, text: impl Into<String>) {
        if let Some(session) = &mut self.session {
            session.message = text.into();
            session.attention = false;
        }
    }
}

pub(crate) fn valid(app: &App, target: EditorTarget, selection: bool) -> bool {
    super::reading::is_edit(app, target.tab)
        && app.is_tab_visible(target.tab)
        && app.notes.incarnation(target.tab) == Some(target.incarnation)
        && app
            .note_editor_view(target.tab)
            .is_some_and(|v| v.buffer_id == target.buffer && v.revision == target.revision)
        && (!selection
            || app
                .editor_interaction_snapshot(target.tab)
                .is_some_and(|s| s.selection == target.selection))
}

#[must_use]
pub fn too_narrow(app: &App, tab: TabId) -> bool {
    app.context_mode.mode(tab).active()
        && app
            .editor_bounds
            .get(&tab)
            .is_some_and(|rect| rect.width / app.appearance.metrics().scale < 320.0)
}

/// Available commands derive from the same target checks as execution.
#[must_use]
pub fn available(app: &App, target: EditorTarget, command: ContextCommand) -> bool {
    if command == ContextCommand::History && !app.context_mode.mode(target.tab).active() {
        return false;
    }
    if command == ContextCommand::Exit {
        return app.context_mode.mode(target.tab).active();
    }
    if app.context_mode.history.open
        && !matches!(command, ContextCommand::Escape | ContextCommand::Toggle)
    {
        return false;
    }
    valid(app, target, false)
        && !app.has_composition()
        && app.dialog.is_none()
        && app.shutdown.is_none()
        && (!(command.mutates() || command.enters_typing())
            || (!too_narrow(app, target.tab)
                && app.flavite.as_ref().is_some_and(|api| !api.is_read_only())))
}

pub(crate) fn seal(app: &App, tab: TabId) {
    let (Some(api), Some(view)) = (&app.flavite, app.note_editor_view(tab)) else {
        return;
    };
    if api.is_read_only() {
        return;
    }
    let _ = api.execute(Invocation::new(
        api.command_registry().next_request_id(),
        "buffer.seal_undo_group",
        serde_json::json!({"buffer_id":view.buffer_id,"revision":view.revision}),
        Origin::Gui,
    ));
}

pub fn window_blurred(app: &mut App) {
    if let Some(session) = &app.context_mode.session {
        seal(app, session.tab);
    }
    app.context_mode.capture = None;
}

#[must_use]
pub fn menu_scroll_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("context-menu-{:032x}", tab.raw()))
}

fn focus(app: &App, tab: TabId) -> Task<Message> {
    let (Some(view), Some(bounds)) = (
        app.note_editor_view(tab),
        app.editor_bounds.get(&tab).copied(),
    ) else {
        return Task::none();
    };
    iced::advanced::widget::operate(flavite_editor::focus_editor(
        view.buffer_id,
        view.revision,
        Some(bounds),
    ))
    .map(|_| Message::Pump)
}

/// Restore native editor focus after dismissing one of its owned overlays.
pub fn resume_focus(app: &App) -> Task<Message> {
    let Some(s) = &app.context_mode.session else {
        return Task::none();
    };
    if app.workspace.focused_tab() != s.tab
        || app.context_mode.history.open
        || app.palette_open
        || app.picker.open
        || app.dialog.is_some()
        || app.strip_menu.is_some()
        || super::note_search::blocks_typing(app, &s.tab)
    {
        return Task::none();
    }
    focus(app, s.tab)
}

/// Only ignored native keys reach this path; the editor captures its own keys.
#[must_use]
pub fn keyboard_message(app: &App, event: &iced::keyboard::Event) -> Option<Message> {
    use iced::keyboard::{
        Key,
        key::{Named, Physical},
    };
    let s = app.context_mode.session.as_ref()?;
    if app.palette_open
        || app.context_mode.history.open
        || app.picker.open
        || app.dialog.is_some()
        || app.strip_menu.is_some()
        || app.has_composition()
        || super::note_search::blocks_typing(app, &s.tab)
    {
        return None;
    }
    let iced::keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat,
        ..
    } = event
    else {
        return None;
    };
    let target = EditorTarget::capture(app, s.tab)?;
    if matches!(key, Key::Named(Named::Escape)) {
        return Some(Message::ContextMode(ContextMessage::Run(
            target,
            ContextCommand::Escape,
        )));
    }
    if modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    if let Some(menu) = s.menu {
        let message = match key {
            Key::Named(Named::ArrowDown) => ContextMessage::MenuMove(true),
            Key::Named(Named::ArrowUp) => ContextMessage::MenuMove(false),
            Key::Named(Named::Enter) => ContextMessage::MenuPick,
            _ => {
                let Physical::Code(code) = physical_key else {
                    return None;
                };
                let command = flavite_editor::context::menu_key(menu, *code, *modifiers);
                if *repeat {
                    return None;
                }
                command.map_or(ContextMessage::Unbound(target), |command| {
                    ContextMessage::Run(s.menu_target.unwrap_or(target), command)
                })
            }
        };
        return Some(Message::ContextMode(message));
    }
    None
}

fn exit(app: &mut App) {
    app.context_mode.history = Default::default();
    app.context_mode.capture = None;
    if let Some(session) = app.context_mode.session.take() {
        seal(app, session.tab);
        if app.note_search.finds.contains_key(&session.tab) {
            let _ = super::note_search::handle(
                app,
                super::note_search::NoteSearchMessage::FindClose(session.tab),
            );
        }
        app.palette_open = false;
        app.picker = Default::default();
    }
    app.context_mode.palette_target = None;
    app.context_mode.generation = app.context_mode.generation.wrapping_add(1);
    app.context_mode.bounds.clear();
}

/// Leaving the editor view exits; owned controls and OS blur retain the mode.
pub fn reconcile(app: &mut App) {
    let workspace = &app.workspace;
    app.context_mode
        .bounds
        .retain(|(tab, _), _| workspace.tab(tab).is_some());
    let stale = app.context_mode.session.as_ref().is_some_and(|s| {
        app.workspace.focused_tab() != s.tab
            || !super::reading::is_edit(app, s.tab)
            || app.notes.incarnation(s.tab) != Some(s.incarnation)
            || app
                .note_editor_view(s.tab)
                .is_none_or(|v| v.buffer_id != s.buffer)
    });
    if stale {
        exit(app);
    }
    let stale_menu = app
        .context_mode
        .session
        .as_ref()
        .and_then(|s| s.menu_target)
        .is_some_and(|target| !valid(app, target, true));
    if stale_menu && let Some(s) = &mut app.context_mode.session {
        s.menu = None;
        s.menu_target = None;
        s.message = "Source changed; choose the command again".into();
    }
    super::editor_history::reconcile(app);
}

/// A pointer/motion boundary ends the old typing undo group, not typing mode.
pub fn selection_changed(app: &mut App, tab: TabId) {
    if app.context_mode.internal_selection {
        return;
    }
    app.context_mode.capture = None;
    if app
        .context_mode
        .history
        .page
        .as_ref()
        .is_some_and(|page| page.trail.iter().any(|entry| entry.current && entry.open))
    {
        super::editor_history::invalidate(app);
    }
    if app.context_mode.mode(tab).active() {
        seal(app, tab);
        if let Some(s) = &mut app.context_mode.session {
            s.menu = None;
        }
    }
}

/// Keys are interpreted after preceding events have installed their effects.
/// A rendered COMMAND/TYPING label is never input authority.
pub fn native_key(
    app: &mut App,
    target: EditorTarget,
    mut key: flavite_editor::context::ContextKey,
) -> Task<Message> {
    use flavite_editor::context::{command_key, menu_key, motion_key};
    use iced::keyboard::key::Code;
    if !valid(app, target, true)
        || app.context_mode.history.open
        || too_narrow(app, target.tab)
        || app.palette_open
        || app.picker.open
        || app.strip_menu.is_some()
        || app.dialog.is_some()
        || super::note_search::blocks_typing(app, &target.tab)
        || app.has_composition()
    {
        return Task::none();
    }
    let mode = app.context_mode.mode(target.tab);
    if mode.active() {
        let clipboard = match key.input.action {
            KeyAction::Copy => Some(ContextCommand::Copy),
            KeyAction::Cut => Some(ContextCommand::Cut),
            KeyAction::Paste => Some(ContextCommand::Paste),
            KeyAction::Undo => Some(ContextCommand::Undo),
            KeyAction::Redo => Some(ContextCommand::Redo),
            _ => None,
        };
        if let Some(command) = clipboard {
            return if key.repeat {
                Task::none()
            } else {
                run(app, target, command)
            };
        }
        if let Some(menu) = app.context_mode.session.as_ref().and_then(|s| s.menu) {
            match key.input.action {
                KeyAction::Down => return handle(app, ContextMessage::MenuMove(true)),
                KeyAction::Up => return handle(app, ContextMessage::MenuMove(false)),
                KeyAction::Newline => return handle(app, ContextMessage::MenuPick),
                _ => {}
            }
            if key.repeat {
                return Task::none();
            }
            let command = key
                .code
                .and_then(|code| menu_key(menu, code, key.modifiers));
            return command.map_or_else(Task::none, |command| run(app, target, command));
        }
    }
    if mode == ContextMode::Command {
        if let Some(code) = key.code {
            if let Some(command) = command_key(code, key.modifiers) {
                return if key.repeat {
                    Task::none()
                } else {
                    run(app, target, command)
                };
            }
            let menu = if key.modifiers.control() || key.modifiers.alt() || key.modifiers.logo() {
                None
            } else {
                match code {
                    Code::KeyV => Some(ContextMenu::Select),
                    Code::KeyG => Some(ContextMenu::Go),
                    Code::KeyT => Some(ContextMenu::Type),
                    _ => None,
                }
            };
            if let Some(menu) = menu {
                return if key.repeat {
                    Task::none()
                } else {
                    handle(app, ContextMessage::OpenMenu(target, menu))
                };
            }
            if let Some(action) = motion_key(code, key.modifiers) {
                key.input.action = action;
            }
        }
        if !matches!(
            key.input.action,
            KeyAction::Previous
                | KeyAction::Next
                | KeyAction::PreviousWord
                | KeyAction::NextWord
                | KeyAction::Up
                | KeyAction::Down
                | KeyAction::PageUp
                | KeyAction::PageDown
                | KeyAction::Home
                | KeyAction::End
                | KeyAction::SelectAll
        ) {
            return handle(app, ContextMessage::Unbound(target));
        }
    }
    key.input.buffer = target.buffer;
    key.input.observed_revision = target.revision;
    key.input.sequence = app
        .workspace
        .tab(&target.tab)
        .unwrap()
        .interaction()
        .sequence
        .saturating_add(1);
    super::input::handle(app, target.tab, target.incarnation, key.input)
}

fn select(app: &mut App, target: EditorTarget, anchor: usize, head: usize) {
    app.context_mode.internal_selection = true;
    app.accessible_selection(
        target.tab,
        target.incarnation,
        target.revision,
        anchor,
        head,
    );
    app.context_mode.internal_selection = false;
}

fn input(app: &mut App, target: EditorTarget, action: KeyAction, text: String) -> Task<Message> {
    input_command(app, target, action, text, None)
}

fn input_command(
    app: &mut App,
    target: EditorTarget,
    action: KeyAction,
    text: String,
    semantic: Option<super::input::HistoryIntent>,
) -> Task<Message> {
    let Some(view) = app.note_editor_view(target.tab) else {
        return Task::none();
    };
    if view.buffer_id != target.buffer || view.revision != target.revision {
        app.context_mode
            .notice("Source changed; command was not applied");
        return Task::none();
    }
    let sequence = app
        .workspace
        .tab(&target.tab)
        .map_or(1, |v| v.interaction().sequence.saturating_add(1));
    let captured = CapturedInput {
        buffer: view.buffer_id,
        observed_revision: view.revision,
        sequence,
        action,
        text,
        extend: false,
        document_boundary: false,
        vertical: None,
    };
    if let Some(intent) = semantic {
        super::input::handle_command(app, target.tab, target.incarnation, captured, intent)
    } else {
        super::input::handle(app, target.tab, target.incarnation, captured)
    }
}

/// Execute every toolbar/accelerator/palette action through one target guard.
pub fn handle(app: &mut App, message: ContextMessage) -> Task<Message> {
    match message {
        ContextMessage::ExpandPane(target) => {
            if valid(app, target, true) && too_narrow(app, target.tab) {
                super::commands::dispatch(app, "workspace.maximize_group")
            } else {
                Task::none()
            }
        }
        ContextMessage::Bounds(tab, name, rect) => {
            if app.context_mode.mode(tab).active()
                || (name == ContextCommand::Toggle.id() && super::reading::is_edit(app, tab))
            {
                app.context_mode.bounds.insert((tab, name), rect);
            }
            Task::none()
        }
        ContextMessage::CloseMenu(tab) => {
            if let Some(s) = app.context_mode.session.as_mut().filter(|s| s.tab == tab) {
                s.menu = None;
                s.menu_target = None;
            }
            focus(app, tab)
        }
        ContextMessage::MenuMove(forward) => {
            if let Some(s) = &mut app.context_mode.session
                && let Some(menu) = s.menu
            {
                let n = menu.commands().len();
                s.selected = if forward {
                    (s.selected + 1) % n
                } else {
                    (s.selected + n - 1) % n
                };
                // Keep the selected row visible in the bounded overflow card.
                // Aligning it near the middle also works after wraparound.
                return iced::widget::operation::scroll_to(
                    menu_scroll_id(s.tab),
                    iced::widget::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(
                            (s.selected as f32 * 34.2 * app.appearance.metrics().scale
                                - 150.0 * app.appearance.metrics().scale)
                                .max(0.0),
                        ),
                    },
                );
            }
            Task::none()
        }
        ContextMessage::MenuPick => {
            if let Some(s) = &app.context_mode.session
                && let (Some(menu), Some(target)) = (s.menu, s.menu_target)
            {
                return run(app, target, menu.commands()[s.selected]);
            }
            Task::none()
        }
        ContextMessage::OpenMenu(target, menu) => {
            if !app.context_mode.history.open
                && valid(app, target, true)
                && app.context_mode.mode(target.tab).active()
            {
                seal(app, target.tab);
                if let Some(s) = &mut app.context_mode.session {
                    s.menu = Some(menu);
                    s.menu_target = Some(target);
                    s.selected = 0;
                }
            }
            Task::none()
        }
        ContextMessage::Unbound(target) => {
            if valid(app, target, false) {
                app.context_mode
                    .notice("No command on this key · choose a menu or Type");
            }
            Task::none()
        }
        ContextMessage::PasteReady(target, generation, content) => {
            if generation != app.context_mode.generation
                || !valid(app, target, true)
                || app.context_mode.history.open
            {
                app.context_mode
                    .notice("Source changed; paste was not applied");
                return Task::none();
            }
            match content {
                Some(text) if !text.is_empty() => {
                    input(app, target, KeyAction::Paste, text).chain(focus(app, target.tab))
                }
                _ => {
                    app.context_mode.notice("Clipboard is empty");
                    focus(app, target.tab)
                }
            }
        }
        ContextMessage::EnterAt(target, selection) => {
            if !valid(app, target, false) || app.has_composition() || app.dialog.is_some() {
                return Task::none();
            }
            select(app, target, selection.0, selection.1);
            enter(app, target);
            focus(app, target.tab)
        }
        ContextMessage::Run(target, command) => run(app, target, command),
    }
}

fn enter(app: &mut App, target: EditorTarget) {
    if app.context_mode.session.as_ref().is_some_and(|s| {
        s.tab != target.tab || s.incarnation != target.incarnation || s.buffer != target.buffer
    }) {
        exit(app);
    }
    seal(app, target.tab);
    super::editor_history::invalidate(app);
    if !valid(app, target, false) {
        app.context_mode
            .notice("Source changed; command was not applied");
        return;
    }
    app.context_mode.generation = app.context_mode.generation.wrapping_add(1);
    app.context_mode.session = Some(ContextSession {
        tab: target.tab,
        incarnation: target.incarnation,
        buffer: target.buffer,
        mode: ContextMode::Command,
        menu: None,
        menu_target: None,
        selected: 0,
        message: "Select text, then act · i types · Esc returns to commands".into(),
        attention: false,
    });
}

fn run(app: &mut App, target: EditorTarget, command: ContextCommand) -> Task<Message> {
    use ContextCommand::*;
    if command == Exit || (command == Toggle && app.context_mode.mode(target.tab).active()) {
        if app.dialog.is_some() || app.shutdown.is_some() {
            return Task::none();
        }
        if !app.context_mode.session.as_ref().is_some_and(|s| {
            s.tab == target.tab && s.incarnation == target.incarnation && s.buffer == target.buffer
        }) {
            return Task::none();
        }
        exit(app);
        return focus(app, target.tab);
    }
    if !available(app, target, command)
        || !valid(app, target, true)
        || app.palette_open
        || app.picker.open
        || app.strip_menu.is_some()
    {
        app.context_mode
            .notice("Source changed or command unavailable; choose it again");
        return Task::none();
    }
    if command == Toggle {
        app.dispatch_layout(|expected_revision| LayoutAction::ActivateTab {
            expected_revision,
            tab: target.tab,
        });
        enter(app, target);
        return focus(app, target.tab);
    }
    if command == Escape {
        if app.context_mode.history.open {
            return super::editor_history::handle(
                app,
                super::editor_history::HistoryMessage::Act(
                    target,
                    super::editor_history::HistoryAction::Close,
                ),
            );
        }
        super::editor_history::invalidate(app);
        if let Some(s) = &mut app.context_mode.session {
            if s.menu.take().is_none() {
                s.mode = ContextMode::Command;
            }
            s.message = "Commands · exit with ✕ or the toggle shortcut".into();
            s.attention = false;
        }
        seal(app, target.tab);
        return focus(app, target.tab);
    }
    seal(app, target.tab);
    super::editor_history::invalidate(app);
    if command != History {
        app.context_mode.history.open = false;
    }
    if !valid(app, target, true) {
        app.context_mode
            .notice("Source changed; command was not applied");
        return Task::none();
    }
    if let Some(s) = &mut app.context_mode.session {
        s.menu = None;
        s.menu_target = None;
        s.message = command.title().into();
        s.attention = false;
    }
    match command {
        History => {
            return super::editor_history::handle(
                app,
                super::editor_history::HistoryMessage::Act(
                    target,
                    super::editor_history::HistoryAction::Open,
                ),
            );
        }
        Commands => return app.update(Message::PaletteOpen),
        Find | Replace => {
            let message = if command == Find {
                super::note_search::NoteSearchMessage::FindOpen(target.tab)
            } else {
                super::note_search::NoteSearchMessage::ReplaceOpen(target.tab)
            };
            return super::note_search::handle(app, message);
        }
        Paste => {
            let generation = app.context_mode.generation;
            return iced::clipboard::read().map(move |text| {
                Message::ContextMode(ContextMessage::PasteReady(target, generation, text))
            });
        }
        Undo | Redo => {
            return input(
                app,
                target,
                if command == Undo {
                    KeyAction::Undo
                } else {
                    KeyAction::Redo
                },
                String::new(),
            )
            .chain(focus(app, target.tab));
        }
        _ => {}
    }
    let Some(view) = app.note_editor_view(target.tab) else {
        return Task::none();
    };
    let (anchor, head) = target.selection;
    let (start, end) = (anchor.min(head), anchor.max(head));
    let text = &view.text;
    let line_start = text[..head].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[head..].find('\n').map_or(text.len(), |i| head + i);
    let line_with_break = (line_end + usize::from(line_end < text.len())).min(text.len());
    let mut edit = None;
    let mut caret_after = None;
    match command {
        SelectAll => select(app, target, 0, text.len()),
        SelectLine => select(app, target, line_start, line_with_break),
        SelectWord => {
            let (a, b) = flavite_editor::context::word_bounds(text, head);
            select(app, target, a, b);
        }
        Collapse => select(app, target, head, head),
        LineStart | InsertLineStart => select(app, target, line_start, line_start),
        LineEnd | InsertLineEnd => select(app, target, line_end, line_end),
        DocumentStart => select(app, target, 0, 0),
        DocumentEnd => select(app, target, text.len(), text.len()),
        InsertStart => select(app, target, start, start),
        InsertEnd => select(app, target, end, end),
        OpenAbove => {
            select(app, target, line_start, line_start);
            edit = Some((KeyAction::Newline, String::new()));
            caret_after = Some(line_start);
        }
        OpenBelow => {
            select(app, target, line_end, line_end);
            edit = Some((KeyAction::Newline, String::new()));
        }
        Cut | Copy => {
            if start == end {
                select(app, target, line_start, line_with_break);
            }
            edit = Some((
                if command == Cut {
                    KeyAction::Cut
                } else {
                    KeyAction::Copy
                },
                String::new(),
            ));
        }
        Delete | Change => {
            let mut effective = (start, end);
            if start == end {
                let next = flavite_editor::viewport::next_grapheme_boundary(
                    text,
                    flavite_syntax::TextByte(head),
                )
                .0;
                let previous = flavite_editor::viewport::prev_grapheme_boundary(
                    text,
                    flavite_syntax::TextByte(head),
                )
                .0;
                let (a, b) = if head < line_end {
                    (head, next)
                } else if head > line_start {
                    (previous, head)
                } else {
                    (head, head)
                };
                select(app, target, a, b);
                effective = (a, b);
            }
            if effective.0 != effective.1 {
                edit = Some((KeyAction::Delete, String::new()));
            }
        }
        Toggle | Exit | Escape | Paste | Undo | Redo | Commands | Find | Replace | History => {
            unreachable!("handled before planning")
        }
    }
    if command.enters_typing() {
        if !app.context_mode.mode(target.tab).active() {
            enter(app, target);
        }
        if let Some(s) = &mut app.context_mode.session {
            s.mode = ContextMode::Typing;
            s.message = "Typing · click or move to reposition · Esc returns to commands".into();
        }
    }
    let task = edit.map_or_else(Task::none, |(action, text)| {
        use flavite_buffer::history::EditKind;
        let kind = match command {
            Change => EditKind::Change,
            OpenAbove => EditKind::OpenAbove,
            OpenBelow => EditKind::OpenBelow,
            Cut => EditKind::Cut,
            _ => EditKind::Delete,
        };
        input_command(
            app,
            target,
            action,
            text,
            Some(super::input::HistoryIntent {
                kind,
                before: [anchor, head],
                after: caret_after.map(|at| [at, at]),
            }),
        )
    });
    task.chain(focus(app, target.tab))
}
