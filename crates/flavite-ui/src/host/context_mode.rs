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

/// A control captures its editor identity and selection set, not global
/// focus. The whole committed set rides along (FEUX-0002 X04, E-5): `selection`
/// stays the primary pair (the first canonical member) for captions and
/// single-range effects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorTarget {
    pub tab: TabId,
    pub incarnation: u64,
    pub buffer: u64,
    pub revision: u64,
    /// The primary pair (first canonical member).
    pub selection: (usize, usize),
    /// Every committed member as `(anchor, head)` pairs in canonical order.
    pub selections: Vec<(usize, usize)>,
    /// Monotonic committed-selection version at capture time (FEUX-0002
    /// E-5): mutations fail on mismatch even when the revision matches.
    pub selection_version: u64,
}

impl EditorTarget {
    #[must_use]
    pub fn capture(app: &App, tab: TabId) -> Option<Self> {
        let view = app.note_editor_view(tab)?;
        let interaction = app.workspace.tab(&tab)?.interaction();
        let selections: Vec<(usize, usize)> = interaction
            .selections
            .iter()
            .map(|selection| (selection.anchor, selection.head))
            .collect();
        let selection = *selections.first()?;
        Some(Self {
            tab,
            incarnation: app.notes.incarnation(tab)?,
            buffer: view.buffer_id,
            revision: view.revision,
            selection,
            selections,
            selection_version: interaction.selection_version,
        })
    }

    /// Wire form for the api selection-set argument: canonical triples.
    #[must_use]
    pub fn selections_json(&self) -> serde_json::Value {
        serde_json::json!({
            "primary": 0,
            "items": self
                .selections
                .iter()
                .enumerate()
                .map(|(id, (anchor, head))| serde_json::json!([id, anchor, head]))
                .collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone)]
pub enum ContextMessage {
    Preview(EditorTarget, ContextCommand, bool),
    Run(EditorTarget, ContextCommand),
    /// Right-click entry carrying the resolved selection set (kept or
    /// word-replaced per E-14).
    EnterAt(EditorTarget, Vec<(usize, usize)>),
    OpenMenu(EditorTarget, ContextMenu),
    CloseMenu(TabId),
    MenuMove(bool),
    MenuPick,
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
                .is_some_and(|s| {
                    let committed: Vec<(usize, usize)> = s
                        .selections
                        .iter()
                        .map(|[_, anchor, head]| (*anchor, *head))
                        .collect();
                    committed == target.selections && s.selection == target.selection
                }))
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
    let command = if let Some((source, command)) = app.context_mode.preview.clone()
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
            .as_ref()
            .is_some_and(|(source, _)| *source == target)
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
    valid(app, target.clone(), false)
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

pub(crate) fn focus(app: &App, tab: TabId) -> Task<Message> {
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
                command.map_or(ContextMessage::Unbound(target.clone()), |command| {
                    ContextMessage::Run(s.menu_target.clone().unwrap_or(target), command)
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
        .and_then(|s| s.menu_target.clone())
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
    if !valid(app, target.clone(), true)
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
        // AwaitChar capture (FEUX-0002 E-8, §5.4.1): the first event with
        // produced text resolves the find-char; Esc cancels through the
        // session's escape pop; menu keys cancel then process; Dead and
        // modifier-only events are never published here at all.
        if let Some(request) = app
            .context_mode
            .session
            .as_ref()
            .and_then(|s| s.state.await_char())
        {
            let produced = key
                .input
                .text
                .chars()
                .next()
                .filter(|_| !key.input.text.is_empty())
                .filter(|_| !key.input.text.chars().nth(1).is_some());
            if let Some(produced) = produced {
                let extend = key.modifiers.shift;
                if let Some(s) = &mut app.context_mode.session {
                    s.state.end_await_char();
                }
                let extra = serde_json::json!({
                    "direction": if request.forward { "fwd" } else { "back" },
                    "char": produced.to_string(),
                    "extend": extend,
                });
                let installed = selections_only(app, &target, "editor.find_char", extra);
                return if installed.is_some() {
                    focus(app, target.tab)
                } else {
                    Task::none()
                };
            }
            // A menu key cancels the capture and processes the menu; every
            // other non-character key is ignored (the capture stays).
            if let Some(code) = key.code
                && !key.modifiers.control
                && !key.modifiers.alt
                && !key.modifiers.logo
                && matches!(
                    code,
                    flavite_edit_context::KeyCode::KeyV
                        | flavite_edit_context::KeyCode::KeyG
                        | flavite_edit_context::KeyCode::KeyT
                )
            {
                if let Some(s) = &mut app.context_mode.session {
                    s.state.end_await_char();
                }
                // Fall through to the delivered menu handling below.
            } else {
                return Task::none();
            }
        }
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
                | KeyAction::NextWordEnd
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

/// Install a caller-supplied canonical set on the requesting view (the
/// pointer/right-click entry path); the reducer bumps the committed version
/// when the set changed. Returns whether the set was installed.
fn install_pairs(app: &mut App, target: &EditorTarget, pairs: &[(usize, usize)]) -> bool {
    if pairs.is_empty() {
        return false;
    }
    let Some(mut interaction) = app
        .workspace
        .tab(&target.tab)
        .map(|v| v.interaction().clone())
    else {
        return false;
    };
    interaction.selections = pairs
        .iter()
        .map(|&(anchor, head)| flavite_workspace::CommittedSelection::new(anchor, head))
        .collect();
    interaction.sequence += 1;
    let Some(view) = app.note_editor_view(target.tab) else {
        return false;
    };
    if app
        .workspace
        .acknowledge_interaction(interaction, &view.text)
        .is_err()
    {
        app.input_chains.remove(&target.tab);
        return false;
    }
    app.reconcile_contexts();
    app.refresh_semantics();
    true
}

/// The shared ticketed mutating path (FEUX-0002 X03/X04, E-5): issue an
/// operation ticket, run one planner-backed `editor.*` verb through the
/// Session single-writer path, map passive views once via the movement
/// delta, and install the receipt's exact after-set on the requesting view.
/// The committed version bump itself belongs to the workspace reducer (once
/// per committed set change).
fn kernel_mutate(
    app: &mut App,
    target: &EditorTarget,
    api_id: &'static str,
    mut args: serde_json::Value,
    refused: &'static str,
) -> Option<flavite_api::LocalSuccess> {
    let Some(api) = &app.flavite else {
        return None;
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
        return None;
    };
    let Some(operation) = success.data.get("operation").cloned() else {
        app.context_mode.notice("Operation could not be started");
        return None;
    };
    let object = args.as_object_mut().expect("mutation args are an object");
    object.insert("buffer_id".into(), serde_json::json!(target.buffer));
    object.insert("revision".into(), serde_json::json!(target.revision));
    object.insert("operation".into(), operation);
    object.insert(
        "origin".into(),
        serde_json::json!({
            "view": format!("{:032x}", target.tab.raw()),
            "incarnation": target.incarnation,
        }),
    );
    object.insert("selections".into(), target.selections_json());
    let result = api.execute(Invocation::new(
        api.command_registry().next_request_id(),
        api_id,
        args,
        Origin::Gui,
    ));
    let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result else {
        let message = match &result {
            ExecutionResult::Rejected(rejected) => rejected
                .message
                .clone()
                .unwrap_or_else(|| refused.to_string()),
            ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => {
                if error.code == "read_only_non_applicable" {
                    return None;
                }
                error.message.clone()
            }
            _ => refused.to_string(),
        };
        app.context_mode.notice(message);
        return None;
    };
    Some(success)
}

/// Interpret a mutating receipt: map passive views once, install the exact
/// after-set on the requesting view, and keep the acknowledged-input chain
/// resolvable against the new revision.
fn install_receipt(app: &mut App, target: &EditorTarget, success: &flavite_api::LocalSuccess) {
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
    // Install the exact after-set on the requesting view; the workspace
    // reducer bumps the selection version exactly once per committed change.
    if let Some(mut interaction) = app
        .workspace
        .tab(&target.tab)
        .map(|v| v.interaction().clone())
    {
        let next_selections: Vec<flavite_workspace::CommittedSelection> = after_triples
            .iter()
            .map(|[_, a, h]| flavite_workspace::CommittedSelection::new(*a, *h))
            .collect();
        if !next_selections.is_empty() {
            interaction.selections = next_selections;
        }
        interaction.revision = new_revision;
        interaction.sequence += 1;
        interaction.viewport_anchor = interaction.viewport_anchor.min(
            app.note_editor_view(target.tab)
                .map_or(0, |view| view.text.len()),
        );
        let Some(view) = app.note_editor_view(target.tab) else {
            return;
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
}

/// A ticketed mutating verb through the kernel path, ending in focus.
fn kernel_task(
    app: &mut App,
    target: &EditorTarget,
    api_id: &'static str,
    args: serde_json::Value,
    refused: &'static str,
) -> Task<Message> {
    match kernel_mutate(app, target, api_id, args, refused) {
        Some(success) => {
            install_receipt(app, target, &success);
            focus(app, target.tab)
        }
        None => Task::none(),
    }
}

/// A ticketed paste: the one clipboard string broadcasts to every member
/// through the planner's `Typing` operation (`editor.paste`).
pub(crate) fn paste(app: &mut App, target: EditorTarget, text: String) -> Task<Message> {
    kernel_task(
        app,
        &target,
        "editor.paste",
        serde_json::json!({"text": text}),
        "Paste was refused; source changed",
    )
}

/// A selection-only command over the whole live set (no ticket, no history
/// node): run the api verb and install the returned canonical set. Returns
/// the installed triples so callers can chain typing entry.
fn selections_only(
    app: &mut App,
    target: &EditorTarget,
    api_id: &'static str,
    mut extra: serde_json::Value,
) -> Option<Vec<[usize; 3]>> {
    let Some(api) = &app.flavite else {
        return None;
    };
    let object = extra.as_object_mut().expect("selection args are an object");
    object.insert("buffer_id".into(), serde_json::json!(target.buffer));
    object.insert("revision".into(), serde_json::json!(target.revision));
    object.insert("selections".into(), target.selections_json());
    let result = api.execute(Invocation::new(
        api.command_registry().next_request_id(),
        api_id,
        extra,
        Origin::Gui,
    ));
    let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result else {
        let message = match &result {
            ExecutionResult::Rejected(rejected) => rejected.message.clone(),
            ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => {
                Some(error.message.clone())
            }
            _ => None,
        };
        if let Some(message) = message {
            app.context_mode.notice(message);
        }
        return None;
    };
    let triples: Vec<[usize; 3]> =
        serde_json::from_value(success.data["selections"].clone()).ok()?;
    // Install the canonical set; the reducer bumps the version on change.
    if let Some(mut interaction) = app
        .workspace
        .tab(&target.tab)
        .map(|v| v.interaction().clone())
    {
        interaction.selections = triples
            .iter()
            .map(|[_, a, h]| flavite_workspace::CommittedSelection::new(*a, *h))
            .collect();
        interaction.sequence += 1;
        let Some(view) = app.note_editor_view(target.tab) else {
            return None;
        };
        if app
            .workspace
            .acknowledge_interaction(interaction, &view.text)
            .is_err()
        {
            app.input_chains.remove(&target.tab);
        }
    }
    app.reconcile_contexts();
    app.refresh_semantics();
    Some(triples)
}

/// Per-member head targets on the adjacent visual row (X05 `add_caret_
/// above/below`): computed through the same shaping the widget paints, with
/// the host's own wrap width and type metrics. Refuses when any member's
/// row does not exist (the whole command is refused, E-9).
fn row_targets(app: &App, target: &EditorTarget, rows: i32) -> Result<Vec<usize>, &'static str> {
    use flavite_editor::navigation::{VerticalMotion, vertical_target};
    let Some(view) = app.note_editor_view(target.tab) else {
        return Err("Source changed; command was not applied");
    };
    let bounds = app.editor_bounds.get(&target.tab).map_or(640.0, |b| b.width);
    let metrics = app.appearance.metrics();
    let width =
        flavite_editor::widget::wrap_width_for(bounds, metrics.editor_max_line_width());
    let motion = VerticalMotion {
        rows,
        width,
        metrics: metrics.editor_type(),
    };
    let mut positions = Vec::with_capacity(target.selections.len());
    for &(_, head) in &target.selections {
        let (at, _) = vertical_target(&view.text, head, motion, None);
        if at == head {
            return Err(if rows < 0 {
                "No row above to add a caret on"
            } else {
                "No row below to add a caret on"
            });
        }
        positions.push(at);
    }
    Ok(positions)
}

/// A selection-only verb, ending in focus.
fn selections_only_task(
    app: &mut App,
    target: &EditorTarget,
    api_id: &'static str,
    extra: serde_json::Value,
) -> Task<Message> {
    if selections_only(app, target, api_id, extra).is_some() {
        focus(app, target.tab)
    } else {
        Task::none()
    }
}

fn input(app: &mut App, target: EditorTarget, action: KeyAction, text: String) -> Task<Message> {
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
        preferred_x: Vec::new(),
    };
    super::input::handle(app, target.tab, target.incarnation, captured)
}

/// Execute every toolbar/accelerator/palette action through one target guard.
pub fn handle(app: &mut App, message: ContextMessage) -> Task<Message> {
    match message {
        ContextMessage::Preview(source, command, entered) => {
            if entered
                && valid(app, source.clone(), true)
                && app.context_mode.mode(source.tab).active()
                && !app.context_mode.history.open
            {
                app.context_mode.preview = Some((source, command));
            } else if !entered
                && app
                    .context_mode
                    .preview
                    .as_ref()
                    .is_some_and(|(held, held_command)| *held == source && *held_command == command)
            {
                app.context_mode.preview = None;
            }
            Task::none()
        }
        ContextMessage::ExpandPane(target) => {
            if valid(app, target.clone(), true) && too_narrow(app, target.tab) {
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
                && let (Some(command), Some(target)) = (s.state.menu_command(), s.menu_target.clone())
            {
                return run(app, target, command);
            }
            Task::none()
        }
        ContextMessage::OpenMenu(target, menu) => {
            if !app.context_mode.history.open
                && valid(app, target.clone(), true)
                && app.context_mode.mode(target.tab).active()
            {
                let effects = app
                    .context_mode
                    .session
                    .as_mut()
                    .map(|s| {
                        s.menu_target = Some(target.clone());
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
        ContextMessage::EnterAt(target, selection) => {
            if !valid(app, target.clone(), false) || app.has_composition() || app.dialog.is_some() {
                return Task::none();
            }
            install_pairs(app, &target, &selection);
            enter(app, target.clone());
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
    if !valid(app, target.clone(), false) {
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
    if !available(app, target.clone(), command)
        || !valid(app, target.clone(), true)
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
        enter(app, target.clone());
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
    if !valid(app, target.clone(), true) {
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
            return super::editor_clipboard::read_for_context_paste(target.clone(), generation);
        }
        Undo | Redo => {
            return input(
                app,
                target.clone(),
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
    let text = &view.text;
    // Delivered Go-menu motions and Select all stay host-side single-pair
    // selections (they replace the set whole, which is their set-aware
    // semantics at every N).
    match command {
        SelectAll => {
            select(app, target.clone(), 0, text.len());
            return focus(app, target.tab);
        }
        LineStart => {
            let at = text[..target.selection.1].rfind('\n').map_or(0, |i| i + 1);
            select(app, target.clone(), at, at);
            return focus(app, target.tab);
        }
        LineEnd => {
            let at = text[target.selection.1..]
                .find('\n')
                .map_or(text.len(), |i| target.selection.1 + i);
            select(app, target.clone(), at, at);
            return focus(app, target.tab);
        }
        DocumentStart => {
            select(app, target.clone(), 0, 0);
            return focus(app, target.tab);
        }
        DocumentEnd => {
            select(app, target.clone(), text.len(), text.len());
            return focus(app, target.tab);
        }
        _ => {}
    }
    // Every remaining verb is set-aware through the api translation point
    // (FEUX-0002 X04, E-1/E-6): the whole captured set rides the command.
    let typing_entry = command.enters_typing();
    let task = match command {
        SelectAll | LineStart | LineEnd | DocumentStart | DocumentEnd => {
            unreachable!("handled by the host-side single-pair motions")
        }
        SelectLine => {
            selections_only_task(app, &target, "editor.select_line", serde_json::json!({}))
        }
        SelectWord => {
            selections_only_task(app, &target, "editor.select_word", serde_json::json!({}))
        }
        Collapse => selections_only_task(app, &target, "editor.collapse", serde_json::json!({})),
        ClearSelection => {
            selections_only_task(app, &target, "editor.clear_selection", serde_json::json!({}))
        }
        KeepPrimary => {
            selections_only_task(app, &target, "editor.keep_primary", serde_json::json!({}))
        }
        RotatePrimary => selections_only_task(
            app,
            &target,
            "editor.rotate_primary",
            serde_json::json!({"direction": "next"}),
        ),
        FlipSelections => {
            selections_only_task(app, &target, "editor.flip_selections", serde_json::json!({}))
        }
        TrimSelections => {
            selections_only_task(app, &target, "editor.trim_selections", serde_json::json!({}))
        }
        InvertSelections => {
            selections_only_task(app, &target, "editor.invert_selections", serde_json::json!({}))
        }
        AddCaretAbove | AddCaretBelow => {
            // The rows are editor-side geometry (E-9/X05): the host computes
            // per-member targets through the same layout the widget paints.
            let rows = if command == AddCaretAbove { -1 } else { 1 };
            match row_targets(app, &target, rows) {
                Ok(positions) => selections_only_task(
                    app,
                    &target,
                    if rows < 0 {
                        "editor.add_caret_above"
                    } else {
                        "editor.add_caret_below"
                    },
                    serde_json::json!({ "positions": positions }),
                ),
                Err(message) => {
                    app.context_mode.notice(message);
                    Task::none()
                }
            }
        }
        FindChar { forward } => {
            // E-8: entry opens the one new statechart state; the next
            // produced character resolves it (native_key intercepts).
            if let Some(s) = &mut app.context_mode.session {
                let _ = reduce(
                    &mut s.state,
                    &Event::EnterAwaitChar(flavite_edit_context::AwaitChar { forward }),
                );
            }
            focus(app, target.tab)
        }
        InsertStart => selections_only_task(
            app,
            &target,
            "editor.insert_start",
            serde_json::json!({}),
        ),
        InsertEnd => selections_only_task(app, &target, "editor.insert_end", serde_json::json!({})),
        InsertLineStart => selections_only_task(
            app,
            &target,
            "editor.insert_line_start",
            serde_json::json!({}),
        ),
        InsertLineEnd => selections_only_task(
            app,
            &target,
            "editor.insert_line_end",
            serde_json::json!({}),
        ),
        OpenAbove => kernel_task(
            app,
            &target,
            "editor.open_above",
            serde_json::json!({}),
            "Open above was refused; source changed",
        ),
        OpenBelow => kernel_task(
            app,
            &target,
            "editor.open_below",
            serde_json::json!({}),
            "Open below was refused; source changed",
        ),
        Delete => kernel_task(
            app,
            &target,
            "editor.delete",
            serde_json::json!({"kind": "delete"}),
            "Delete was refused; source changed",
        ),
        Change => kernel_task(
            app,
            &target,
            "editor.change",
            serde_json::json!({}),
            "Change was refused; source changed",
        ),
        Copy => {
            let Some(api) = &app.flavite else {
                return Task::none();
            };
            let result = api.execute(Invocation::new(
                api.command_registry().next_request_id(),
                "editor.copy",
                serde_json::json!({
                    "buffer_id": target.buffer,
                    "revision": target.revision,
                    "selections": target.selections_json(),
                }),
                Origin::Gui,
            ));
            let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result
            else {
                app.context_mode
                    .notice("Copy was refused; source changed");
                return Task::none();
            };
            let copied = success.data["text"].as_str().unwrap_or_default().to_string();
            if copied.is_empty() {
                app.context_mode.notice("Nothing to copy on this last line");
                return Task::none();
            }
            super::editor_clipboard::write(copied)
        }
        Cut => {
            // Clipboard ordering (§5.5): the fragments are resolved before
            // the delete, the write is issued first, and a source change
            // between the two stages refuses the cut with its honest
            // message.
            let Some(api) = &app.flavite else {
                return Task::none();
            };
            let result = api.execute(Invocation::new(
                api.command_registry().next_request_id(),
                "editor.copy",
                serde_json::json!({
                    "buffer_id": target.buffer,
                    "revision": target.revision,
                    "selections": target.selections_json(),
                }),
                Origin::Gui,
            ));
            let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result
            else {
                app.context_mode.notice("Nothing to cut on this last line");
                return Task::none();
            };
            let copied = success.data["text"].as_str().unwrap_or_default().to_string();
            if copied.is_empty() {
                app.context_mode.notice("Nothing to cut on this last line");
                return Task::none();
            }
            let write = super::editor_clipboard::write(copied.clone());
            let cut = kernel_mutate(
                app,
                &target,
                "editor.cut",
                serde_json::json!({}),
                "Cut was refused; source changed",
            );
            if cut.is_none() {
                app.context_mode
                    .notice("Copied; cut was not applied because the source changed");
                return write;
            }
            install_receipt(app, &target, cut.as_ref().expect("cut success"));
            write.chain(focus(app, target.tab))
        }
        Toggle | Exit | Escape | Paste | Undo | Redo | Commands | Find | Replace | History => {
            unreachable!("handled before planning")
        }
    };
    if typing_entry {
        if !app.context_mode.mode(target.tab).active() {
            enter(app, target);
        }
        if let Some(s) = &mut app.context_mode.session {
            let _ = reduce(&mut s.state, &Event::EnterTyping);
        }
    }
    task
}
