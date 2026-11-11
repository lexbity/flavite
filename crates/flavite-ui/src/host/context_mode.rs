//! Target-bound Context Mode controls over the existing editor input and API.

use crate::app::{App, Message};
use flavite_api::{ExecutionResult, Invocation, Origin};
use flavite_edit_context::{
    ContextCommand, ContextMenu, ContextMode, ContextState, Effect, Event, menu_key, reduce,
};
use flavite_editor::{
    context::{native_code, native_modifiers},
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
    /// Monotonic committed-selection version at capture time (FEUX-0002
    /// E-5): mutations fail on mismatch even when the revision matches.
    pub selection_version: u64,
}

impl EditorTarget {
    #[must_use]
    pub fn capture(app: &App, tab: TabId) -> Option<Self> {
        let view = app.note_editor_view(tab)?;
        let interaction = app.workspace.tab(&tab)?.interaction();
        let selection = interaction.selections.first().copied()?;
        Some(Self {
            tab,
            incarnation: app.notes.incarnation(tab)?,
            buffer: view.buffer_id,
            revision: view.revision,
            selection: (selection.anchor, selection.head),
            selection_version: interaction.selection_version,
        })
    }
}

#[derive(Debug, Clone)]
pub enum ContextMessage {
    Preview(EditorTarget, ContextCommand, bool),
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
    /// The pure statechart state (mode, menu, selection, message). Every
    /// mutation goes through `flavite_edit_context` — this host only
    /// classifies admission, forwards events and interprets effects.
    pub state: ContextState,
    pub menu_target: Option<EditorTarget>,
}

#[derive(Debug, Default)]
pub struct ContextHost {
    preview: Option<(EditorTarget, ContextCommand)>,
    pub history: super::editor_history::HistoryHost,
    pub session: Option<ContextSession>,
    pub bounds: BTreeMap<(TabId, &'static str), Rectangle>,
    generation: u64,
    internal_selection: bool,
    pub palette_target: Option<EditorTarget>,
    pub capture: Option<super::input::ContextCapture>,
}

impl ContextHost {
    pub(crate) fn clear_preview(&mut self) {
        self.preview = None;
    }
    pub const fn generation(&self) -> u64 {
        self.generation
    }
    #[must_use]
    pub fn mode(&self, tab: TabId) -> ContextMode {
        self.session
            .as_ref()
            .filter(|s| s.tab == tab)
            .map_or(ContextMode::Off, |s| s.state.mode())
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        if let Some(session) = &mut self.session {
            session.state.set_message(text, true);
        }
    }

    pub fn feedback(&mut self, text: impl Into<String>) {
        if let Some(session) = &mut self.session {
            session.state.set_message(text, false);
        }
    }
}

/// Execute pure statechart effects against the app, in returned order.
/// Returns the focus task if one was requested.
fn run_effects(app: &mut App, tab: TabId, effects: Vec<Effect>) -> Option<Task<Message>> {
    let mut task = None;
    for effect in effects {
        match effect {
            Effect::SealTypingGroup => seal(app, tab),
            Effect::ClearPreview => app.context_mode.clear_preview(),
            Effect::FocusEditor => task = Some(focus(app, tab)),
            Effect::InvalidateHistory => super::editor_history::invalidate(app),
        }
    }
    task
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
        && (!selection
            || app
                .workspace
                .tab(&target.tab)
                .is_some_and(|tab| tab.interaction().selection_version == target.selection_version))
}

#[must_use]
pub fn too_narrow(app: &App, tab: TabId) -> bool {
    app.context_mode.mode(tab).active()
        && app
            .editor_bounds
            .get(&tab)
            .is_some_and(|rect| rect.width / app.appearance.metrics().scale < 320.0)
}

/// This describes a prospective command, never a committed selection or edit.
/// Both the source editor and toolbar ask the same bound preview owner.
pub fn preview_command(app: &App, tab: TabId) -> Option<ContextCommand> {
    let session = app.context_mode.session.as_ref().filter(|s| s.tab == tab)?;
    if app.context_mode.history.open
        || app.palette_open
        || app.picker.open
        || app.dialog.is_some()
        || app.strip_menu.is_some()
        || app.has_composition()
        || too_narrow(app, tab)
    {
        return None;
    }
    let target = EditorTarget::capture(app, tab)?;
    let command = if let Some((source, command)) = app.context_mode.preview
        && source == target
    {
        command
    } else if let Some(command) = session.state.menu_command() {
        command
    } else if session.state.mode() == ContextMode::Command {
        ContextCommand::Delete
    } else {
        return None;
    };
    available(app, target, command).then_some(command)
}

pub fn effect_caption(app: &App, tab: TabId) -> Option<String> {
    let session = app.context_mode.session.as_ref().filter(|s| s.tab == tab)?;
    let target = EditorTarget::capture(app, tab)?;
    if session.state.menu().is_none()
        && !app
            .context_mode
            .preview
            .is_some_and(|(source, _)| source == target)
    {
        return None;
    }
    let command = preview_command(app, tab)?;
    let view = app.note_editor_view(tab)?;
    flavite_edit_context::effect::plan(&view.text, target.selection, command)
        .map(|effect| effect.description(command))
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
    if let Some(menu) = s.state.menu() {
        let message = match key {
            Key::Named(Named::ArrowDown) => ContextMessage::MenuMove(true),
            Key::Named(Named::ArrowUp) => ContextMessage::MenuMove(false),
            Key::Named(Named::Enter) => ContextMessage::MenuPick,
            _ => {
                let Physical::Code(code) = physical_key else {
                    return None;
                };
                let command = native_code(*code)
                    .and_then(|code| menu_key(menu, code, native_modifiers(*modifiers)));
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
    app.context_mode.clear_preview();
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
    if stale_menu {
        let effects = app
            .context_mode
            .session
            .as_mut()
            .map(|s| {
                s.menu_target = None;
                reduce(&mut s.state, &Event::MenuTargetInvalid)
            })
            .unwrap_or_default();
        debug_assert!(
            effects.is_empty(),
            "MenuTargetInvalid carries no effects by contract"
        );
    }
    super::editor_history::reconcile(app);
}

/// A pointer/motion boundary ends the old typing undo group, not typing mode.
pub fn selection_changed(app: &mut App, tab: TabId) {
    if app.context_mode.internal_selection {
        return;
    }
    app.context_mode.clear_preview();
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
            s.state.external_selection_changed();
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
    use flavite_edit_context::{command_key, menu_key, motion_key};
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
        if let Some(menu) = app
            .context_mode
            .session
            .as_ref()
            .and_then(|s| s.state.menu())
        {
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
            let menu = if key.modifiers.control || key.modifiers.alt || key.modifiers.logo {
                None
            } else {
                match code {
                    flavite_edit_context::KeyCode::KeyV => Some(ContextMenu::Select),
                    flavite_edit_context::KeyCode::KeyG => Some(ContextMenu::Go),
                    flavite_edit_context::KeyCode::KeyT => Some(ContextMenu::Type),
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
            if let Some(motion) = motion_key(code, key.modifiers) {
                key.input.action = flavite_editor::context::motion_action(motion);
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

/// The ticketed, planner-backed delete (FEUX-0002 X03 proof verb): issue an
/// operation ticket, run `editor.delete` through the Session single-writer
/// path, install the receipt's exact after-set on the requesting view, bump
/// its selection version, and map passive views once via the movement delta.
fn kernel_delete(app: &mut App, target: EditorTarget) -> Task<Message> {
    let Some(api) = &app.flavite else {
        return Task::none();
    };
    let issue = Invocation::new(
        api.command_registry().next_request_id(),
        "editor.issue_operation",
        serde_json::json!({"buffer_id": target.buffer}),
        Origin::Gui,
    );
    let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = api.execute(issue)
    else {
        app.context_mode.notice("Operation could not be started");
        return Task::none();
    };
    let Some(operation) = success.data.get("operation").cloned() else {
        app.context_mode.notice("Operation could not be started");
        return Task::none();
    };
    let (anchor, head) = target.selection;
    let args = serde_json::json!({
        "buffer_id": target.buffer,
        "revision": target.revision,
        "operation": operation,
        "origin": {
            "view": format!("{:032x}", target.tab.raw()),
            "incarnation": target.incarnation,
        },
        "selections": {
            "primary": 0,
            "items": [[0, anchor, head]],
        },
        "kind": "delete",
    });
    let result = api.execute(Invocation::new(
        api.command_registry().next_request_id(),
        "editor.delete",
        args,
        Origin::Gui,
    ));
    let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result else {
        let message = match &result {
            ExecutionResult::Rejected(rejected) => rejected
                .message
                .clone()
                .unwrap_or_else(|| "Delete was refused; source changed".into()),
            ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => {
                if error.code == "read_only_non_applicable" {
                    return Task::none();
                }
                error.message.clone()
            }
            _ => "Delete was refused; source changed".into(),
        };
        app.context_mode.notice(message);
        return Task::none();
    };
    let new_revision = success.data["revision"].as_u64().unwrap_or(target.revision);
    let movement = serde_json::from_value::<flavite_buffer::history::HistoryMovement>(
        success.data["movement"].clone(),
    )
    .unwrap_or(flavite_buffer::history::HistoryMovement {
        selections: None,
        spans: Vec::new(),
        truncated: false,
    });
    let after_triples: Vec<[usize; 3]> =
        serde_json::from_value(success.data["after_selections"].clone()).unwrap_or_default();
    // Map passive views once (movement spans; clamp on truncation).
    super::input::apply_history_movement(
        app,
        target.tab,
        target.buffer,
        target.revision,
        new_revision,
        &movement,
    );
    // Install the exact after-set on the requesting view; bump the selection
    // version only when the set actually changed.
    if let Some(mut interaction) = app
        .workspace
        .tab(&target.tab)
        .map(|v| v.interaction().clone())
    {
        let next_selections: Vec<flavite_workspace::CommittedSelection> = after_triples
            .iter()
            .map(|[_, a, h]| flavite_workspace::CommittedSelection::new(*a, *h))
            .collect();
        let changed = !next_selections.is_empty() && next_selections != interaction.selections;
        if changed {
            interaction.selection_version = interaction.selection_version.wrapping_add(1);
        }
        interaction.selections = if next_selections.is_empty() {
            interaction.selections.clone()
        } else {
            next_selections
        };
        interaction.revision = new_revision;
        interaction.sequence += 1;
        interaction.viewport_anchor = interaction.viewport_anchor.min(
            app.note_editor_view(target.tab)
                .map_or(0, |view| view.text.len()),
        );
        let Some(view) = app.note_editor_view(target.tab) else {
            return Task::none();
        };
        if app
            .workspace
            .acknowledge_interaction(interaction, &view.text)
            .is_err()
        {
            app.input_chains.remove(&target.tab);
        }
    }
    app.workspace.invalidate_writing_target(target.buffer);
    app.linking.invalidate_if_source_changed(None);
    // Maintain the acknowledged-input chain so queued native keys still
    // resolve against the new revision (same contract as handle_inner).
    let sequence = app
        .workspace
        .tab(&target.tab)
        .map_or(1, |v| v.interaction().sequence);
    let base = app
        .input_chains
        .get(&target.tab)
        .map_or(target.revision, |chain| chain.base());
    app.input_chains.insert(
        target.tab,
        super::input::InputChain::new(
            target.incarnation,
            target.buffer,
            base,
            new_revision,
            sequence,
            None,
        ),
    );
    if let Some(session) = &mut app.context_mode.session {
        session.state.clear_attention();
    }
    app.reconcile_contexts();
    app.refresh_semantics();
    focus(app, target.tab)
}

fn input(app: &mut App, target: EditorTarget, action: KeyAction, text: String) -> Task<Message> {
    input_command(app, target, action, text, None)
}

fn input_command(
    app: &mut App,
    target: EditorTarget,
    action: KeyAction,
    text: String,
    semantic: Option<super::input::CommandIntent>,
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
        ContextMessage::Preview(source, command, entered) => {
            if entered
                && valid(app, source, true)
                && app.context_mode.mode(source.tab).active()
                && !app.context_mode.history.open
            {
                app.context_mode.preview = Some((source, command));
            } else if !entered && app.context_mode.preview == Some((source, command)) {
                app.context_mode.preview = None;
            }
            Task::none()
        }
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
            app.context_mode.preview = None;
            let task = app
                .context_mode
                .session
                .as_mut()
                .filter(|s| s.tab == tab)
                .map(|s| {
                    s.menu_target = None;
                    reduce(&mut s.state, &Event::CloseMenu)
                })
                .map(|effects| run_effects(app, tab, effects))
                .unwrap_or_default();
            task.unwrap_or_else(|| focus(app, tab))
        }
        ContextMessage::MenuMove(forward) => {
            app.context_mode.preview = None;
            let scrolled = app.context_mode.session.as_mut().map(|s| {
                let selected = s.state.menu_move(forward);
                let tab = s.tab;
                // Keep the selected row visible in the bounded overflow card.
                // Aligning it near the middle also works after wraparound.
                (
                    tab,
                    iced::widget::operation::scroll_to(
                        menu_scroll_id(tab),
                        iced::widget::scrollable::AbsoluteOffset {
                            x: None,
                            y: Some(
                                (selected as f32 * 34.2 * app.appearance.metrics().scale
                                    - 150.0 * app.appearance.metrics().scale)
                                    .max(0.0),
                            ),
                        },
                    ),
                )
            });
            if let Some((tab, scroll)) = scrolled {
                let _ = run_effects(app, tab, vec![Effect::ClearPreview]);
                return scroll;
            }
            Task::none()
        }
        ContextMessage::MenuPick => {
            if let Some(s) = &app.context_mode.session
                && let (Some(command), Some(target)) = (s.state.menu_command(), s.menu_target)
            {
                return run(app, target, command);
            }
            Task::none()
        }
        ContextMessage::OpenMenu(target, menu) => {
            if !app.context_mode.history.open
                && valid(app, target, true)
                && app.context_mode.mode(target.tab).active()
            {
                let effects = app
                    .context_mode
                    .session
                    .as_mut()
                    .map(|s| {
                        s.menu_target = Some(target);
                        reduce(&mut s.state, &Event::OpenMenu(menu))
                    })
                    .unwrap_or_default();
                let _ = run_effects(app, target.tab, effects);
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
        state: ContextState::activated(),
        menu_target: None,
    });
}

fn run(app: &mut App, target: EditorTarget, command: ContextCommand) -> Task<Message> {
    use ContextCommand::*;
    app.context_mode.preview = None;
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
        let effects = app
            .context_mode
            .session
            .as_mut()
            .map(|s| reduce(&mut s.state, &Event::Escape))
            .unwrap_or_default();
        let task = run_effects(app, target.tab, effects);
        return task.unwrap_or_else(|| focus(app, target.tab));
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
        s.menu_target = None;
        let _ = reduce(&mut s.state, &Event::RunAdmitted(command));
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
    let mut effective_selection = None;
    match command {
        SelectAll => select(app, target, 0, text.len()),
        SelectLine => select(app, target, line_start, line_with_break),
        SelectWord => {
            let (a, b) = flavite_edit_context::word_bounds(text, head);
            select(app, target, a, b);
        }
        Collapse => select(app, target, head, head),
        ClearSelection => select(app, target, head, head),
        LineStart | InsertLineStart => select(app, target, line_start, line_start),
        LineEnd | InsertLineEnd => select(app, target, line_end, line_end),
        DocumentStart => select(app, target, 0, 0),
        DocumentEnd => select(app, target, text.len(), text.len()),
        InsertStart => select(app, target, start, start),
        InsertEnd => select(app, target, end, end),
        OpenAbove => {
            effective_selection = Some([line_start, line_start]);
            edit = Some((KeyAction::Newline, String::new()));
            caret_after = Some(line_start);
        }
        OpenBelow => {
            effective_selection = Some([line_end, line_end]);
            edit = Some((KeyAction::Newline, String::new()));
        }
        Delete => {
            // The ticketed kernel path (FEUX-0002 X03): the planner derives
            // the effective range, the Session installs atomically, and the
            // receipt carries the exact after-set + movement delta.
            return kernel_delete(app, target);
        }
        Cut | Copy | Change => {
            let Some(effect) = flavite_edit_context::effect::plan(text, target.selection, command)
            else {
                app.context_mode
                    .notice("Selection changed; place the caret again");
                return Task::none();
            };
            if !effect.range.is_empty() {
                effective_selection = Some([effect.range.start, effect.range.end]);
                edit = Some((
                    match command {
                        Cut => KeyAction::Cut,
                        Copy => KeyAction::Copy,
                        _ => KeyAction::Delete,
                    },
                    String::new(),
                ));
            } else if command != Change {
                app.context_mode.notice(effect.description(command));
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
            let _ = reduce(&mut s.state, &Event::EnterTyping);
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
            Some(super::input::CommandIntent {
                kind,
                before: [anchor, head],
                after: caret_after.map(|at| [at, at]),
                effective_selection,
            }),
        )
    });
    task.chain(focus(app, target.tab))
}
