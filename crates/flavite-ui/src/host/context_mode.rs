//! Target-bound Context Mode controls over the existing editor input and API.

use crate::app::{App, Message};
use crate::host::editor_diagnostics::{EventKind, ModeEnum as DiagMode, Outcome};
use flavite_api::{ExecutionResult, Invocation, Origin};
use flavite_edit_context::{
    BindingId, ContextCommand, ContextMenu, ContextMode, ContextState, Effect, Event,
    PendingCommand, reduce,
};
use flavite_editor::{
    context::{native_code, native_modifiers},
    input::CapturedInput,
    keymap::KeyAction,
};
use flavite_workspace::{LayoutAction, TabId};
use iced::{Rectangle, Task};
use std::collections::BTreeMap;

/// FEUX-0004 X06: Convert a digit KeyCode to its numeric value (E-4).
/// Returns None for non-digit positions.
#[must_use]
fn digit_value(code: flavite_edit_context::KeyCode) -> Option<u32> {
    match code {
        flavite_edit_context::KeyCode::Digit0 => Some(0),
        flavite_edit_context::KeyCode::Digit1 => Some(1),
        flavite_edit_context::KeyCode::Digit2 => Some(2),
        flavite_edit_context::KeyCode::Digit3 => Some(3),
        flavite_edit_context::KeyCode::Digit4 => Some(4),
        flavite_edit_context::KeyCode::Digit5 => Some(5),
        flavite_edit_context::KeyCode::Digit6 => Some(6),
        flavite_edit_context::KeyCode::Digit7 => Some(7),
        flavite_edit_context::KeyCode::Digit8 => Some(8),
        flavite_edit_context::KeyCode::Digit9 => Some(9),
        _ => None,
    }
}

/// A control captures its editor identity and selection set, not global
/// focus. The whole committed set rides along (FEUX-0002 X04, E-5): `selection`
/// stays the primary pair for captions and single-range effects, and
/// `primary` names that member by its index in `selections` (E-4/FR-4:
/// canonical order and "primary first" cannot both be implicit, so the
/// primary is explicit in every capture).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditorTarget {
    pub tab: TabId,
    pub incarnation: u64,
    pub buffer: u64,
    pub revision: u64,
    /// The primary pair.
    pub selection: (usize, usize),
    /// Every committed member as `(anchor, head)` pairs in canonical order.
    pub selections: Vec<(usize, usize)>,
    /// Index into `selections` of the primary member.
    pub primary: usize,
    /// Monotonic committed-selection version at capture time (FEUX-0002
    /// E-5): mutations fail on mismatch even when the revision matches.
    pub selection_version: u64,
}

impl EditorTarget {
    pub(crate) fn issue_operation(
        &self,
        api: &flavite_api::Flavite,
    ) -> Result<flavite_api::editor_commands::OperationArg, String> {
        api.issue_editor_operation(
            self.buffer,
            self.revision,
            Some(flavite_api::editor_commands::EditorViewArg {
                view: format!("{:032x}", self.tab.raw()),
                incarnation: self.incarnation,
                selection_version: self.selection_version,
            }),
        )
    }
    #[must_use]
    pub fn capture(app: &App, tab: TabId) -> Option<Self> {
        let view = app.note_editor_view(tab)?;
        let interaction = app.workspace.tab(&tab)?.interaction();
        let selections: Vec<(usize, usize)> = interaction
            .selections
            .iter()
            .map(|selection| (selection.anchor, selection.head))
            .collect();
        let primary = interaction.primary.min(selections.len().saturating_sub(1));
        let selection = *selections.get(primary)?;
        Some(Self {
            tab,
            incarnation: app.notes.incarnation(tab)?,
            buffer: view.buffer_id,
            revision: view.revision,
            selection,
            selections,
            primary,
            selection_version: interaction.selection_version,
        })
    }

    /// Wire form for the api selection-set argument: canonical triples with
    /// an explicit primary hint. Members carry their index as id, so the
    /// hint resolves to exactly the captured primary member.
    #[must_use]
    pub fn selections_json(&self) -> serde_json::Value {
        serde_json::json!({
            "primary": self.primary,
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
    /// Pending typing-run text for `.` repeat (FEUX-0004 E-5): set when a
    /// typing-entry command begins its run, accumulated by the input host,
    /// finalized with `InvocationRecord` at the run's end (group seal).
    pub typing_record: Option<(&'static str, String)>,
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

/// Record one diagnostics outcome for the view (FEUX-0002 X07). The ring is
/// content-free by schema; only enums and counts reach it.
fn diag(app: &mut App, tab: TabId, kind: EventKind, outcome: Outcome, count: Option<u32>) {
    app.diagnostics.record(
        Some(tab),
        DiagMode::from(app.context_mode.mode(tab)),
        kind,
        outcome,
        count,
        None,
    );
}

/// The clock and burst setting the pure statechart needs (X07).
fn chart_inputs(app: &App) -> (u64, bool) {
    (app.now_ms(), app.appearance.burst_hint_enabled())
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
            Effect::ShowBurstHint => {
                app.diagnostics.show_burst_hint();
                diag(app, tab, EventKind::BurstHint, Outcome::Shown, None);
            }
            Effect::KeepPrimary => {
                // §5.4 row 10: Command Escape with more than one member
                // keeps the primary (the multi-selection recovery).
                if let Some(target) = EditorTarget::capture(app, tab) {
                    selections_only(app, &target, "editor.keep_primary", serde_json::json!({}));
                }
            }
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
    // §5.4.1: while `AwaitChar` captures, the state row shows the capture
    // hint — the Command-mode default preview (Delete) must not paint over
    // it, and the next key resolves find-char, not a delete.
    if session.state.await_args().is_some() {
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
    let requires_write = command.mutates() || command.enters_typing();
    // ReplaceRegex opens its draft even in read-only vaults: the live
    // preview is compute-only ReadNotes; commit is refused at Enter when
    // the vault state demands it (E-12). Every other mutating command keeps
    // the delivered write gate.
    let write_ok = if command == ContextCommand::ReplaceRegex {
        !too_narrow(app, target.tab)
    } else {
        !requires_write
            || (!too_narrow(app, target.tab)
                && app.flavite.as_ref().is_some_and(|api| !api.is_read_only()))
    };
    valid(app, target.clone(), false)
        && !app.has_composition()
        && app.dialog.is_none()
        && app.shutdown.is_none()
        && write_ok
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
        if let Some(api) = &app.flavite {
            api.cancel_editor_view_operations(session.tab.raw());
        }
        seal(app, session.tab);
    }
    app.context_mode.capture = None;
}

#[must_use]
pub fn menu_scroll_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("context-menu-{:032x}", tab.raw()))
}

pub(crate) fn focus(app: &mut App, tab: TabId) -> Task<Message> {
    let (Some(view), Some(bounds)) = (
        app.note_editor_view(tab),
        app.editor_bounds.get(&tab).copied(),
    ) else {
        return Task::none();
    };
    if let Some(session) = app.workspace.tab(&tab)
        && !session.interaction().focused
    {
        let mut interaction = session.interaction().clone();
        interaction.focused = true;
        interaction.sequence = interaction.sequence.saturating_add(1);
        let _ = app.acknowledge_editor_interaction(interaction, &view.text);
    }
    iced::advanced::widget::operate(iced::advanced::widget::operation::focusable::unfocus())
        .chain(iced::advanced::widget::operate(
            flavite_editor::focus_editor(view.buffer_id, view.revision, Some(bounds)),
        ))
        .map(|_| Message::Pump)
}

/// Restore native editor focus after dismissing one of its owned overlays.
pub fn resume_focus(app: &mut App) -> Task<Message> {
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
        || super::selection_search::blocks_typing(app, &s.tab)
        || super::command_args::blocks_typing(app, &s.tab)
    {
        return Task::none();
    }
    let tab = s.tab;
    focus(app, tab)
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
        || super::selection_search::blocks_typing(app, &s.tab)
        || super::command_args::blocks_typing(app, &s.tab)
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
                let native_mods = native_modifiers(*modifiers);

                // Ctrl/Alt/Logo never enter the Context grammar
                if native_mods.control || native_mods.alt || native_mods.logo {
                    return None;
                }

                // Look up in keymap: try menu accelerator first, then root
                let command = native_code(*code).and_then(|code| {
                    let slot = flavite_edit_context::Slot {
                        position: code,
                        shift: native_mods.shift,
                    };
                    // Try menu accelerator lookup
                    app.appearance
                        .context_keys
                        .lookup_menu(menu, slot.position)
                        // Fall through to root if it's a command in this menu
                        .or_else(|| {
                            app.appearance.context_keys.lookup_root(slot).and_then(
                                |bid| match bid {
                                    BindingId::Command(cmd) => {
                                        if menu.commands().contains(cmd) {
                                            Some(*cmd)
                                        } else {
                                            None
                                        }
                                    }
                                    _ => None,
                                },
                            )
                        })
                });

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
    // FEUX-0004 X05: any unfinished typing run records at session exit.
    finalize_typing_record(app);
    app.context_mode.clear_preview();
    app.diagnostics.clear_burst_hint();
    app.context_mode.history = Default::default();
    app.context_mode.capture = None;
    if let Some(session) = app.context_mode.session.take() {
        if let Some(api) = &app.flavite {
            api.cancel_editor_view_operations(session.tab.raw());
        }
        seal(app, session.tab);
        if app.note_search.finds.contains_key(&session.tab) {
            let _ = super::note_search::handle(
                app,
                super::note_search::NoteSearchMessage::FindClose(session.tab),
            );
        }
        // Match-selection drafts are context-mode owned (E-11): mode exit
        // cancels the in-flight collection at the source and discards the
        // overlay.
        super::selection_search::cancel_tab(app, session.tab);
        app.selection_search.detach(session.tab);
        super::command_args::cancel_tab(app, session.tab);
        app.command_args.detach(session.tab);
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
        let (now_ms, burst_enabled) = chart_inputs(app);
        let effects = app
            .context_mode
            .session
            .as_mut()
            .map(|s| {
                s.menu_target = None;
                reduce(
                    &mut s.state,
                    &Event::MenuTargetInvalid,
                    now_ms,
                    burst_enabled,
                )
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
    if !valid(app, target.clone(), true)
        || app.context_mode.history.open
        || too_narrow(app, target.tab)
        || app.palette_open
        || app.picker.open
        || app.strip_menu.is_some()
        || app.dialog.is_some()
        || super::note_search::blocks_typing(app, &target.tab)
        || super::selection_search::blocks_typing(app, &target.tab)
        || app.has_composition()
    {
        return Task::none();
    }
    let mode = app.context_mode.mode(target.tab);
    if mode.active() {
        // AwaitArgs capture (FEUX-0004 E-6, supersedes FEUX-0002 E-8, §5.4.1):
        // the first event with produced text resolves one argument; Esc cancels
        // through the session's escape pop; menu keys cancel then process;
        // Dead and modifier-only events are never published here at all.
        if let Some(request) = app
            .context_mode
            .session
            .as_ref()
            .and_then(|s| s.state.await_args())
        {
            let produced = key
                .input
                .text
                .chars()
                .next()
                .filter(|_| !key.input.text.is_empty())
                .filter(|_| !key.input.text.chars().nth(1).is_some());
            if let Some(produced) = produced {
                // Collect this argument and potentially dispatch
                if let Some(s) = &mut app.context_mode.session {
                    // Use the new collect_char method to add the char and potentially complete
                    if let Some(collected) = s.state.collect_char(produced) {
                        // All arguments collected - dispatch based on the command
                        // FEUX-0004 X06: For FindChar, dispatch directly with count if present
                        match request.command {
                            PendingCommand::FindChar { forward } => {
                                // Get the count from AwaitArgs
                                let count_arg = s.state.await_args().and_then(|args| args.count);
                                let direction = if forward { "fwd" } else { "back" };
                                let mut json_args = serde_json::json!({
                                    "direction": direction,
                                    "char": collected[0].to_string(),
                                    "extend": false
                                });
                                if let Some(n) = count_arg {
                                    json_args["count"] = serde_json::Value::from(n);
                                }
                                return kernel_task(
                                    app,
                                    &target,
                                    "editor.find_char",
                                    json_args,
                                    "Find char was refused; source changed",
                                );
                            }
                            PendingCommand::SurroundAdd if collected.len() == 1 => {
                                return kernel_task(
                                    app,
                                    &target,
                                    "editor.surround_add",
                                    serde_json::json!({ "char": collected[0].to_string() }),
                                    "Surround add was refused; source changed",
                                );
                            }
                            PendingCommand::SurroundDelete if collected.len() == 1 => {
                                return kernel_task(
                                    app,
                                    &target,
                                    "editor.surround_delete",
                                    serde_json::json!({ "char": collected[0].to_string() }),
                                    "Surround delete was refused; source changed",
                                );
                            }
                            PendingCommand::SurroundChange if collected.len() == 2 => {
                                return kernel_task(
                                    app,
                                    &target,
                                    "editor.surround_change",
                                    serde_json::json!({
                                        "old_char": collected[0].to_string(),
                                        "new_char": collected[1].to_string()
                                    }),
                                    "Surround change was refused; source changed",
                                );
                            }
                            _ => {
                                // Should not happen - argument count mismatch
                                s.state.cancel_await_args();
                            }
                        }
                    }
                    // If collect_char returned None, we advanced to next arg and don't need to do anything
                }
                return Task::none();
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
                    s.state.cancel_await_args();
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
            let command = key.code.and_then(|code| {
                // Look up in keymap: try menu accelerator first, then root
                let slot = flavite_edit_context::Slot {
                    position: code,
                    shift: key.modifiers.shift,
                };
                app.appearance
                    .context_keys
                    .lookup_menu(menu, slot.position)
                    // Fall through to root if it's a command in this menu
                    .or_else(|| {
                        app.appearance
                            .context_keys
                            .lookup_root(slot)
                            .and_then(|bid| match bid {
                                BindingId::Command(cmd) => {
                                    if menu.commands().contains(cmd) {
                                        Some(*cmd)
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            })
                    })
            });
            return command.map_or_else(Task::none, |command| run(app, target, command));
        }
    }
    if mode == ContextMode::Command {
        if let Some(code) = key.code {
            // Ctrl/Alt/Logo never enter the Context grammar
            if key.modifiers.control || key.modifiers.alt || key.modifiers.logo {
                // They are handled by chord matching, not Context Mode
            } else {
                // FEUX-0004 X06: Digit interception (E-4)
                // Digits are grammar-owned: intercept before keymap lookup
                // Conditions: Command mode, no menu open, no AwaitArgs pending
                let no_menu = app
                    .context_mode
                    .session
                    .as_ref()
                    .is_none_or(|s| s.state.menu().is_none());
                let no_await_args = app
                    .context_mode
                    .session
                    .as_ref()
                    .is_none_or(|s| s.state.await_args().is_none());

                if no_menu && no_await_args {
                    // Check if this is a digit
                    if let Some(digit_value) = digit_value(code) {
                        // Send CountDigit event to statechart
                        // Get the inputs before the mutable borrow
                        let now_ms = app.now_ms();
                        let burst_enabled = app.appearance.burst_hint_enabled();
                        if let Some(s) = &mut app.context_mode.session {
                            use flavite_edit_context::session::Event;
                            let effects = flavite_edit_context::session::reduce(
                                &mut s.state,
                                &Event::CountDigit(digit_value),
                                now_ms,
                                burst_enabled,
                            );
                            run_effects(app, target.tab, effects);
                        }
                        return Task::none();
                    }
                }

                let slot = flavite_edit_context::Slot {
                    position: code,
                    shift: key.modifiers.shift,
                };

                // Look up in keymap. The resolved binding is cloned out so
                // the keymap borrow does not span the dispatch (which may
                // mutate the app).
                if let Some(binding_id) = app.appearance.context_keys.lookup_root(slot).cloned() {
                    match binding_id {
                        BindingId::Command(command) => {
                            return if key.repeat {
                                Task::none()
                            } else {
                                run(app, target, command)
                            };
                        }
                        BindingId::Motion(motion) => {
                            // FEUX-0004 X06: a pending count multiplies the
                            // motion (E-4 §5.3). The count is consumed by the
                            // dispatch (cleared even on refusal); motions are
                            // acceptors, so a count must never survive the
                            // key to silently multiply a later command.
                            let count = app
                                .context_mode
                                .session
                                .as_ref()
                                .and_then(|s| s.state.count());
                            if let Some(n) = count {
                                let (now_ms, burst_enabled) = chart_inputs(app);
                                if let Some(s) = &mut app.context_mode.session {
                                    let effects = reduce(
                                        &mut s.state,
                                        &Event::ClearCount,
                                        now_ms,
                                        burst_enabled,
                                    );
                                    let _ = run_effects(app, target.tab, effects);
                                }
                                return counted_motion(app, &target, motion, n);
                            }
                            key.input.action = flavite_editor::context::motion_action(motion);
                        }
                        BindingId::FindChar { forward } => {
                            let command = ContextCommand::FindChar { forward };
                            return if key.repeat {
                                Task::none()
                            } else {
                                run(app, target, command)
                            };
                        }
                        BindingId::MenuOpener(menu) => {
                            return if key.repeat {
                                Task::none()
                            } else {
                                handle(app, ContextMessage::OpenMenu(target, menu))
                            };
                        }
                    }
                }
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
    key.input.sequence = key.input.sequence.max(
        app.workspace
            .tab(&target.tab)
            .unwrap()
            .interaction()
            .sequence
            .saturating_add(1),
    );
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
        .acknowledge_editor_interaction(interaction, &view.text)
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
pub(crate) fn kernel_mutate(
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
        serde_json::json!({"buffer_id": target.buffer,"revision":target.revision,"view":{"view":format!("{:032x}",target.tab.raw()),"incarnation":target.incarnation,"selection_version":target.selection_version}}),
        Origin::Gui,
    );
    let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = api.execute(issue)
    else {
        app.context_mode.notice("Operation could not be started");
        diag(
            app,
            target.tab,
            EventKind::OperationRefused,
            Outcome::Refused,
            Some(target.selections.len() as u32),
        );
        return None;
    };
    let Some(operation) = success.data.get("operation").cloned() else {
        app.context_mode.notice("Operation could not be started");
        diag(
            app,
            target.tab,
            EventKind::OperationRefused,
            Outcome::Refused,
            Some(target.selections.len() as u32),
        );
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
        app.context_mode.notice(&message);
        // Classify the terminal outcome: duplicate delivery and retired
        // replays are distinct outcomes, not generic refusals (E-5).
        let (kind, outcome) = crate::host::editor_diagnostics::classify_error(&message);
        diag(
            app,
            target.tab,
            kind,
            outcome,
            Some(target.selections.len() as u32),
        );
        return None;
    };
    diag(
        app,
        target.tab,
        EventKind::OperationApplied,
        Outcome::Applied,
        Some(target.selections.len() as u32),
    );
    Some(success)
}

/// Commit a computed regex-replace plan through the delivered editor kernel
/// (FEUX-0006 E-5): the worker computed the ops; this path issues the
/// operation ticket, validates the ops at admission (ascending,
/// non-overlapping, in-bounds scalar boundaries), maps the live selection
/// set through the ops to the exact recorded after-set (§5.5), and runs
/// `apply_edits` with one origin-sealed undo node. The initiating view gets
/// its exact after-set (never the passive clamp); passive views map through
/// the delivered movement-delta path.
pub(crate) fn replace_commit(
    app: &mut App,
    tab: TabId,
    target: &EditorTarget,
    ops: Vec<(usize, usize, String)>,
) -> Result<(), String> {
    let Some(view) = app.note_editor_view(tab) else {
        return Err("Source changed; replace was refused".into());
    };
    let text = &view.text;
    // Admission validation of the worker-computed ops — primary here, the
    // worker guarantee is defense-in-depth (mirrors §5.4 layering).
    let mut cursor = 0usize;
    for (start, end, _) in &ops {
        if *start > *end
            || *end > text.len()
            || *start < cursor
            || !text.is_char_boundary(*start)
            || !text.is_char_boundary(*end)
        {
            return Err("Replace plan ops are invalid; refusing".into());
        }
        cursor = *end;
    }
    let Some(api) = &app.flavite else {
        return Err("Vault unavailable".into());
    };
    let issue = Invocation::new(
        api.command_registry().next_request_id(),
        "editor.issue_operation",
        serde_json::json!({
            "buffer_id": target.buffer,
            "revision": target.revision,
            "view": {
                "view": format!("{:032x}", tab.raw()),
                "incarnation": target.incarnation,
                "selection_version": target.selection_version,
            },
        }),
        Origin::Gui,
    );
    let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = api.execute(issue)
    else {
        return Err("Operation could not be started".into());
    };
    let Some(operation) = success.data.get("operation").cloned() else {
        return Err("Operation could not be started".into());
    };
    // The recorded before-set from the live selection set (canonical
    // triples), then the exact after-set mapped through the ops (§5.5).
    let candidates: Vec<flavite_buffer::selection::RawSelection> = target
        .selections
        .iter()
        .enumerate()
        .map(
            |(index, &(anchor, head))| flavite_buffer::selection::RawSelection {
                id: Some(u32::try_from(index).unwrap_or(u32::MAX)),
                anchor,
                head,
            },
        )
        .collect();
    let hint = u32::try_from(target.primary).ok();
    let (before, _) = flavite_buffer::selection::SelectionSet::normalize(text, candidates, hint)
        .map_err(|error| error.to_string())?;
    let edits: Vec<flavite_buffer::selection::SpanEdit> = ops
        .iter()
        .map(|&(start, end, ref replacement)| {
            flavite_buffer::selection::SpanEdit::new(start, end, replacement.len())
        })
        .collect();
    let (after, _) = before.map_through(&edits);
    let before_triples = before.to_triples();
    let after_triples = after.to_triples();
    let before_primary = before_triples
        .iter()
        .position(|triple| triple[0] == before.primary_id().as_raw() as usize)
        .unwrap_or(0);
    let after_primary = after_triples
        .iter()
        .position(|triple| triple[0] == after.primary_id().as_raw() as usize)
        .unwrap_or(0);
    let record = flavite_buffer::history::EditorEdit {
        before_primary,
        origin: format!("{:032x}", tab.raw()),
        kind: flavite_buffer::history::EditKind::Replace,
        before: before_triples,
        after: after_triples,
        primary: after_primary,
    };
    let ops_wire: Vec<serde_json::Value> = ops
        .into_iter()
        .map(|(start, end, replacement)| {
            serde_json::json!({"range": [start, end], "replacement": replacement})
        })
        .collect();
    let result = api.execute(Invocation::new(
        api.command_registry().next_request_id(),
        "apply_edits",
        serde_json::json!({
            "buffer_id": target.buffer,
            "revision": target.revision,
            "operation": operation,
            "ops": ops_wire,
            "undo_policy": "isolated",
            "editor": record,
        }),
        Origin::Gui,
    ));
    let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result else {
        let message = match &result {
            ExecutionResult::Rejected(rejected) => rejected
                .message
                .clone()
                .unwrap_or_else(|| "Replace was refused".into()),
            ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => error.message.clone(),
            _ => "Replace was refused".into(),
        };
        diag(
            app,
            tab,
            EventKind::OperationRefused,
            Outcome::Refused,
            Some(target.selections.len() as u32),
        );
        return Err(message);
    };
    install_receipt(app, target, &success);
    diag(
        app,
        tab,
        EventKind::OperationApplied,
        Outcome::Applied,
        Some(target.selections.len() as u32),
    );
    Ok(())
}

/// Interpret a mutating receipt: map passive views once, install the exact
/// after-set on the requesting view, and keep the acknowledged-input chain
/// resolvable against the new revision.
pub(crate) fn install_receipt(
    app: &mut App,
    target: &EditorTarget,
    success: &flavite_api::LocalSuccess,
) {
    let new_revision = success.data["revision"].as_u64().unwrap_or(target.revision);
    let movement = serde_json::from_value::<flavite_buffer::history::HistoryMovement>(
        success.data["movement"].clone(),
    )
    .unwrap_or(flavite_buffer::history::HistoryMovement {
        selections: None,
        spans: Vec::new(),
        step_ends: Vec::new(),
        truncated: false,
        primary: None,
    });
    let after_triples: Vec<[usize; 3]> =
        serde_json::from_value(success.data["after_selections"].clone())
            .unwrap_or_else(|_| movement.selections.clone().unwrap_or_default());
    // Map passive views once (movement spans; clamp on truncation).
    super::input::apply_history_movement(
        app,
        target.tab,
        target.buffer,
        target.revision,
        new_revision,
        &movement,
    );
    // Install the exact after-set on the requesting view; the primary
    // member rides the receipt (E-4/FR-4) and the workspace reducer bumps
    // the selection version exactly once per committed change.
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
        } else if let Some(view) = app.note_editor_view(target.tab) {
            interaction.selections = if movement.truncated || !movement.valid_frames() {
                interaction
                    .selections
                    .iter()
                    .map(|selection| {
                        flavite_workspace::CommittedSelection::new(
                            flavite_editor::viewport::clamp_grapheme_boundary(
                                &view.text,
                                selection.anchor,
                            ),
                            flavite_editor::viewport::clamp_grapheme_boundary(
                                &view.text,
                                selection.head,
                            ),
                        )
                    })
                    .collect()
            } else {
                super::input::map_selections(&view.text, &interaction.selections, &movement)
            };
        }
        if let Some(primary) = movement.primary
            && primary < interaction.selections.len()
        {
            interaction.primary = primary;
        } else {
            interaction.primary = interaction.primary.min(interaction.selections.len() - 1);
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
            .acknowledge_editor_interaction(interaction, &view.text)
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
    // Clone args for recording after mutation
    let args_for_record = args.clone();
    match kernel_mutate(app, target, api_id, args, refused) {
        Some(success) => {
            install_receipt(app, target, &success);
            // FEUX-0005 E-9: the completion message names the unit the planner
            // used (rides the receipt — never re-detected from the edited
            // text). Zero-effect no-ops stay silent (delivered no-op rule):
            // the revision is unchanged when the batch was empty.
            if matches!(
                api_id,
                "editor.indent"
                    | "editor.dedent"
                    | "editor.tabs_to_spaces"
                    | "editor.spaces_to_tabs"
            ) {
                let zero_effect = success.data["revision"].as_u64() == Some(target.revision);
                if !zero_effect && let Some(unit) = success.data["unit"].as_str() {
                    let title = match api_id {
                        "editor.indent" => "Indent",
                        "editor.dedent" => "Dedent",
                        "editor.tabs_to_spaces" => "Tabs → Spaces",
                        "editor.spaces_to_tabs" => "Spaces → Tabs",
                        _ => "",
                    };
                    app.context_mode.notice(&format!("{title} · unit: {unit}"));
                }
            }

            // FEUX-0004 X05: Record successful mutations for `.` repeat
            // Exclude non-mutating commands and commands that shouldn't be repeated
            let should_record = !matches!(
                api_id,
                // Selection-only commands (class-S)
                "editor.select_word" | "editor.select_line" | "editor.select_all" |
                "editor.collapse" | "editor.clear_selection" | "editor.keep_primary" |
                "editor.add_caret_above" | "editor.add_caret_below" | "editor.rotate_primary" |
                "editor.flip_selections" | "editor.trim_selections" | "editor.invert_selections" |
                "editor.select_matches" | "editor.split_selection" |
                // Non-mutating operations
                "editor.copy" | "editor.goto_line" | "buffer.undo" | "buffer.redo" |
                // Menu openers (not actual commands)
                "editor.menu_select" | "editor.menu_go" | "editor.menu_more" |
                "editor.menu_transform" | "editor.menu_lines" | "editor.menu_surround" |
                // Typing-entry commands record their run text at group seal
                // (FEUX-0004 E-5): the admission-time args are empty and would
                // otherwise overwrite the register with an untyped record.
                "editor.change" | "editor.insert_start" | "editor.insert_end" |
                "editor.insert_line_start" | "editor.insert_line_end" |
                "editor.open_above" | "editor.open_below" |
                // Repeat itself (would be recursive)
                "editor.repeat_last"
            );

            if should_record {
                // Extract args as BTreeMap for recording
                let mut args_map = std::collections::BTreeMap::new();
                if let serde_json::Value::Object(obj) = args_for_record {
                    for (k, v) in obj {
                        if let serde_json::Value::String(s) = v {
                            args_map.insert(k, s);
                        }
                    }
                }
                record_invocation(app, api_id, args_map);
            }
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
pub(crate) fn selections_only(
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
        diag(
            app,
            target.tab,
            EventKind::SelectionOp,
            Outcome::Refused,
            Some(target.selections.len() as u32),
        );
        return None;
    };
    diag(
        app,
        target.tab,
        EventKind::SelectionOp,
        Outcome::Applied,
        Some(target.selections.len() as u32),
    );
    let triples: Vec<[usize; 3]> =
        serde_json::from_value(success.data["selections"].clone()).ok()?;
    // The canonical primary identity rides the response (E-4/FR-4): the
    // member whose id matches keeps the primary, whatever its position.
    let primary_id = success.data["primary"].as_u64().unwrap_or(0) as usize;
    let primary = triples
        .iter()
        .position(|triple| triple[0] == primary_id)
        .unwrap_or(0);
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
        interaction.primary = primary.min(interaction.selections.len().saturating_sub(1));
        interaction.sequence += 1;
        let view = app.note_editor_view(target.tab)?;
        if app
            .acknowledge_editor_interaction(interaction, &view.text)
            .is_err()
        {
            app.input_chains.remove(&target.tab);
        } else {
            // The selection-only command advanced the committed interaction
            // sequence; the acknowledged-input chain must track it or the
            // next captured key is refused by `accepts_context` (the widget
            // resynchronizes its sequence to the committed interaction every
            // frame, so it can never outrun it on its own).
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
                    view.revision,
                    sequence,
                    None,
                ),
            );
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
    let bounds = app
        .editor_bounds
        .get(&target.tab)
        .map_or(640.0, |b| b.width);
    let metrics = app.appearance.metrics();
    let width = flavite_editor::widget::wrap_width_for(bounds, metrics.editor_max_line_width());
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

/// A counted motion dispatch (FEUX-0004 X06, E-4 §5.3): the count is applied
/// per member in one plan through the delivered `editor.move` path.
/// Horizontal (text-pure) motions pass the count to `apply_with_count`;
/// visual up/down recompute the N-row target heads through the same vertical
/// geometry the widget paints (clamped per member, the delivered rule).
fn counted_motion(
    app: &mut App,
    target: &EditorTarget,
    motion: flavite_edit_context::Motion,
    n: u32,
) -> Task<Message> {
    let api_name = match motion {
        flavite_edit_context::Motion::Previous => "char_left",
        flavite_edit_context::Motion::Next => "char_right",
        flavite_edit_context::Motion::Up => "line_up",
        flavite_edit_context::Motion::Down => "line_down",
        flavite_edit_context::Motion::NextWord => "word_next",
        flavite_edit_context::Motion::PreviousWord => "word_prev",
        flavite_edit_context::Motion::NextWordEnd => "word_end",
    };
    if matches!(
        motion,
        flavite_edit_context::Motion::Up | flavite_edit_context::Motion::Down
    ) {
        let rows = if matches!(motion, flavite_edit_context::Motion::Up) {
            -(n as i32)
        } else {
            n as i32
        };
        match counted_vertical_targets(app, target, rows) {
            Some(positions) => selections_only_task(
                app,
                target,
                "editor.move",
                serde_json::json!({"motion": api_name, "positions": positions}),
            ),
            None => Task::none(),
        }
    } else {
        selections_only_task(
            app,
            target,
            "editor.move",
            serde_json::json!({"motion": api_name, "count": n}),
        )
    }
}

/// Vertical target heads for a counted up/down motion: N visual rows per
/// member, clamped by the delivered vertical geometry (no refusal — the
/// motion simply stops at the document edge).
fn counted_vertical_targets(app: &App, target: &EditorTarget, rows: i32) -> Option<Vec<usize>> {
    use flavite_editor::navigation::{VerticalMotion, vertical_target};
    let view = app.note_editor_view(target.tab)?;
    let bounds = app
        .editor_bounds
        .get(&target.tab)
        .map_or(640.0, |b| b.width);
    let metrics = app.appearance.metrics();
    let width = flavite_editor::widget::wrap_width_for(bounds, metrics.editor_max_line_width());
    let motion = VerticalMotion {
        rows,
        width,
        metrics: metrics.editor_type(),
    };
    let mut positions = Vec::with_capacity(target.selections.len());
    for &(_, head) in &target.selections {
        let (at, _) = vertical_target(&view.text, head, motion, None);
        positions.push(at);
    }
    Some(positions)
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
        clipboard_acquired: false,
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
                if name == "toolbar" && app.context_mode.bounds.get(&(tab, name)) != Some(&rect) {
                    app.context_mode
                        .bounds
                        .retain(|(held_tab, _), _| *held_tab != tab);
                }
                app.context_mode.bounds.insert((tab, name), rect);
            }
            Task::none()
        }
        ContextMessage::CloseMenu(tab) => {
            app.context_mode.preview = None;
            let (now_ms, burst_enabled) = chart_inputs(app);
            let task = app
                .context_mode
                .session
                .as_mut()
                .filter(|s| s.tab == tab)
                .map(|s| {
                    s.menu_target = None;
                    reduce(&mut s.state, &Event::CloseMenu, now_ms, burst_enabled)
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
                && let (Some(command), Some(target)) =
                    (s.state.menu_command(), s.menu_target.clone())
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
                let (now_ms, burst_enabled) = chart_inputs(app);
                let effects = app
                    .context_mode
                    .session
                    .as_mut()
                    .map(|s| {
                        s.menu_target = Some(target.clone());
                        reduce(&mut s.state, &Event::OpenMenu(menu), now_ms, burst_enabled)
                    })
                    .unwrap_or_default();
                let _ = run_effects(app, target.tab, effects);
            }
            Task::none()
        }
        ContextMessage::Unbound(target) => {
            if valid(app, target.clone(), false) {
                app.context_mode
                    .notice("No command on this key · choose a menu");
                // FEUX-0004 E-4: an unbound key clears a pending count.
                let (now_ms, burst_enabled) = chart_inputs(app);
                if let Some(s) = &mut app.context_mode.session {
                    let effects = reduce(&mut s.state, &Event::ClearCount, now_ms, burst_enabled);
                    let _ = run_effects(app, target.tab, effects);
                }
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
        typing_record: None,
    });
}

/// Enter AwaitArgs state for a command requiring character arguments (FEUX-0004 E-6).
fn enter_await_args(app: &mut App, target: &EditorTarget, pending: PendingCommand) {
    use flavite_edit_context::session::AwaitArgs;
    // Ensure we have an active session
    if app.context_mode.session.is_none() {
        enter(app, target.clone());
    }
    let (now_ms, burst_enabled) = chart_inputs(app);
    let session = app.context_mode.session.as_mut().unwrap();
    // FEUX-0004 X06: Capture the current count for find_char (E-4)
    // Count is retained through AwaitArgs entry
    let count = session.state.count();
    // Close any open menu
    session.state.close_menu();
    // Start awaiting the first argument based on the pending command
    let args = match pending {
        PendingCommand::FindChar { forward } => {
            let mut args = AwaitArgs::find_char(forward);
            args.count = count;
            args
        }
        PendingCommand::SurroundAdd => {
            let mut args = AwaitArgs::surround_add();
            args.count = count;
            args
        }
        PendingCommand::SurroundChange => {
            let mut args = AwaitArgs::surround_change();
            args.count = count;
            args
        }
        PendingCommand::SurroundDelete => {
            let mut args = AwaitArgs::surround_delete();
            args.count = count;
            args
        }
    };
    let _ = reduce(
        &mut session.state,
        &Event::EnterAwaitArgs(args),
        now_ms,
        burst_enabled,
    );
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
        diag(
            app,
            target.tab,
            EventKind::OperationRefused,
            Outcome::Refused,
            Some(target.selections.len() as u32),
        );
        return Task::none();
    }
    if command == Toggle {
        if app.workspace.focused_tab() != target.tab {
            app.dispatch_layout(|expected_revision| LayoutAction::ActivateTab {
                expected_revision,
                tab: target.tab,
            });
        }
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
        // FEUX-0004 X05: escaping a typing run seals it for `.` replay.
        finalize_typing_record(app);
        let (now_ms, burst_enabled) = chart_inputs(app);
        let effects = app
            .context_mode
            .session
            .as_mut()
            .map(|s| {
                reduce(
                    &mut s.state,
                    &Event::Escape {
                        selections: target.selections.len(),
                    },
                    now_ms,
                    burst_enabled,
                )
            })
            .unwrap_or_default();
        let task = run_effects(app, target.tab, effects);
        return task.unwrap_or_else(|| focus(app, target.tab));
    }
    // FEUX-0004 X05: a command dispatch ends any prior typing run; record it
    // before the new invocation replaces the register.
    finalize_typing_record(app);
    seal(app, target.tab);
    super::editor_history::invalidate(app);
    if command != History {
        app.context_mode.history.open = false;
    }
    if !valid(app, target.clone(), true) {
        app.context_mode
            .notice("Source changed; command was not applied");
        // FEUX-0004 E-4: the dispatch attempt consumes the count even when
        // refused; a stale refusal must not leave it pending.
        let (now_ms, burst_enabled) = chart_inputs(app);
        if let Some(s) = &mut app.context_mode.session {
            let effects = reduce(&mut s.state, &Event::ClearCount, now_ms, burst_enabled);
            let _ = run_effects(app, target.tab, effects);
        }
        return Task::none();
    }
    // FEUX-0004 X06: Capture count before RunAdmitted clears it (E-4)
    let count = app
        .context_mode
        .session
        .as_ref()
        .and_then(|s| s.state.count());

    let (now_ms, burst_enabled) = chart_inputs(app);
    let effects = if let Some(s) = &mut app.context_mode.session {
        s.menu_target = None;
        reduce(
            &mut s.state,
            &Event::RunAdmitted(command),
            now_ms,
            burst_enabled,
        )
    } else {
        Vec::new()
    };
    let _ = run_effects(app, target.tab, effects);
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
    // Select all replaces the set whole at every N (set-aware by shape).
    if command == SelectAll {
        select(app, target.clone(), 0, text.len());
        return focus(app, target.tab);
    }
    // Match-driven selection (FEUX-0002 X06): opens the owned query row;
    // drafts overlay an outline set and Apply installs it (E-10/E-11).
    if let Some(kind) = match command {
        SelectMatches => Some(super::selection_search::ApplyKind::Select),
        SplitSelection => Some(super::selection_search::ApplyKind::Split),
        KeepMatching { keep } => Some(super::selection_search::ApplyKind::Keep { keep }),
        _ => None,
    } {
        return super::selection_search::handle(
            app,
            super::selection_search::SelectionSearchMessage::Open {
                tab: target.tab,
                kind,
            },
        );
    }
    // Every remaining verb is set-aware through the api translation point
    // (FEUX-0002 X04/X05, E-1/E-6): the whole captured set rides the
    // command. The Go-menu motions became `editor.move` per-member verbs.
    let typing_entry = command.enters_typing();
    // Capture before the match consumes `command` (FEUX-0004 X05: typing
    // runs record their accumulated text at group seal).
    let typing_entry_id = typing_entry.then(|| command.id());
    let task = match command {
        SelectAll => unreachable!("handled by the host-side whole-set select"),
        LineStart => selections_only_task(
            app,
            &target,
            "editor.move",
            serde_json::json!({"motion": "line_start"}),
        ),
        LineEnd => selections_only_task(
            app,
            &target,
            "editor.move",
            serde_json::json!({"motion": "line_end"}),
        ),
        DocumentStart => selections_only_task(
            app,
            &target,
            "editor.move",
            serde_json::json!({"motion": "doc_start"}),
        ),
        DocumentEnd => selections_only_task(
            app,
            &target,
            "editor.move",
            serde_json::json!({"motion": "doc_end"}),
        ),
        SelectLine => {
            // FEUX-0004 X06: select_line accepts count (count = N lines)
            let args = if let Some(n) = count {
                serde_json::json!({"count": n})
            } else {
                serde_json::json!({})
            };
            selections_only_task(app, &target, "editor.select_line", args)
        }
        SelectWord => {
            selections_only_task(app, &target, "editor.select_word", serde_json::json!({}))
        }
        Collapse => selections_only_task(app, &target, "editor.collapse", serde_json::json!({})),
        ClearSelection => selections_only_task(
            app,
            &target,
            "editor.clear_selection",
            serde_json::json!({}),
        ),
        KeepPrimary => {
            selections_only_task(app, &target, "editor.keep_primary", serde_json::json!({}))
        }
        RotatePrimary => selections_only_task(
            app,
            &target,
            "editor.rotate_primary",
            serde_json::json!({"direction": "next"}),
        ),
        FlipSelections => selections_only_task(
            app,
            &target,
            "editor.flip_selections",
            serde_json::json!({}),
        ),
        TrimSelections => selections_only_task(
            app,
            &target,
            "editor.trim_selections",
            serde_json::json!({}),
        ),
        InvertSelections => selections_only_task(
            app,
            &target,
            "editor.invert_selections",
            serde_json::json!({}),
        ),
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
            // E-8/X06: entry opens the one new statechart state; the next
            // produced character resolves it (native_key intercepts).
            // Count is captured at entry time (E-4).
            enter_await_args(app, &target, PendingCommand::FindChar { forward });
            focus(app, target.tab)
        }
        InsertStart => {
            selections_only_task(app, &target, "editor.insert_start", serde_json::json!({}))
        }
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
            let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result else {
                app.context_mode.notice("Copy was refused; source changed");
                return Task::none();
            };
            let copied = success.data["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if copied.is_empty() {
                app.context_mode.notice("Nothing to copy on this last line");
                return Task::none();
            }
            super::editor_clipboard::write_for_context_copy(
                target.clone(),
                app.context_mode.generation(),
                copied,
            )
        }
        Cut => {
            // Clipboard ordering (§5.5): the fragments are resolved before
            // anything else; the clipboard write runs to completion, and
            // only the seam's revalidated completion performs the one
            // guarded deletion. A refused or failed write never deletes
            // the source (E-16, AC-8).
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
            let ExecutionResult::Local(flavite_api::LocalResult::Success(success)) = result else {
                app.context_mode.notice("Nothing to cut on this last line");
                return Task::none();
            };
            let copied = success.data["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if copied.is_empty() {
                app.context_mode.notice("Nothing to cut on this last line");
                return Task::none();
            }
            let generation = app.context_mode.generation;
            return super::editor_clipboard::write_for_cut(target, generation, copied);
        }
        // FEUX-0004 X01: Transform family
        CaseUpper => kernel_task(
            app,
            &target,
            "editor.case_upper",
            serde_json::json!({}),
            "Uppercase was refused; source changed",
        ),
        CaseLower => kernel_task(
            app,
            &target,
            "editor.case_lower",
            serde_json::json!({}),
            "Lowercase was refused; source changed",
        ),
        CaseSwap => kernel_task(
            app,
            &target,
            "editor.case_swap",
            serde_json::json!({}),
            "Swap case was refused; source changed",
        ),
        CaseTitle => kernel_task(
            app,
            &target,
            "editor.case_title",
            serde_json::json!({}),
            "Title case was refused; source changed",
        ),
        NumberInc => kernel_task(
            app,
            &target,
            "editor.number_inc",
            serde_json::json!({}),
            "Increment was refused; source changed",
        ),
        NumberDec => kernel_task(
            app,
            &target,
            "editor.number_dec",
            serde_json::json!({}),
            "Decrement was refused; source changed",
        ),
        NumberSequence => kernel_task(
            app,
            &target,
            "editor.number_sequence",
            serde_json::json!({}),
            "Number sequence was refused; source changed",
        ),
        // FEUX-0004 X01: Lines family
        LinesSortAsc => kernel_task(
            app,
            &target,
            "editor.lines_sort_asc",
            serde_json::json!({}),
            "Sort lines ascending was refused; source changed",
        ),
        LinesSortDesc => kernel_task(
            app,
            &target,
            "editor.lines_sort_desc",
            serde_json::json!({}),
            "Sort lines descending was refused; source changed",
        ),
        LinesSortNumeric => kernel_task(
            app,
            &target,
            "editor.lines_sort_numeric",
            serde_json::json!({}),
            "Sort lines numeric was refused; source changed",
        ),
        LinesJoin => kernel_task(
            app,
            &target,
            "editor.lines_join",
            serde_json::json!({}),
            "Join lines was refused; source changed",
        ),
        LinesDuplicate => kernel_task(
            app,
            &target,
            "editor.lines_duplicate",
            serde_json::json!({}),
            "Duplicate lines was refused; source changed",
        ),
        LinesDedupe => kernel_task(
            app,
            &target,
            "editor.lines_dedupe",
            serde_json::json!({}),
            "Deduplicate lines was refused; source changed",
        ),
        LinesShuffle => kernel_task(
            app,
            &target,
            "editor.lines_shuffle",
            serde_json::json!({}),
            "Shuffle lines was refused; source changed",
        ),
        LinesMoveUp => kernel_task(
            app,
            &target,
            "editor.lines_move_up",
            serde_json::json!({}),
            "Move lines up was refused; source changed",
        ),
        LinesMoveDown => kernel_task(
            app,
            &target,
            "editor.lines_move_down",
            serde_json::json!({}),
            "Move lines down was refused; source changed",
        ),
        // FEUX-0004 X03: LinesAlign and GotoLine
        LinesAlign => {
            // LinesAlign requires a delimiter argument - open draft host
            super::command_args::open_lines_align(app, target.clone());
            iced::Task::none()
        }
        ReplaceRegex => {
            // ReplaceRegex requires pattern and template arguments - open draft host
            super::command_args::open_replace_regex(app, target.clone());
            iced::Task::none()
        }
        GotoLine => {
            // FEUX-0004 X06: GotoLine with count jumps directly (E-12)
            if let Some(line_num) = count {
                // Jump to line line_num directly
                if line_num == 0 {
                    app.context_mode.notice("Document line numbers start at 1");
                    iced::Task::none()
                } else {
                    // Use editor.move with Line motion
                    selections_only_task(
                        app,
                        &target,
                        "editor.move",
                        serde_json::json!({"motion": {"Line": line_num}}),
                    )
                }
            } else {
                // No count - open draft host
                super::command_args::open_goto_line(app, target.clone());
                iced::Task::none()
            }
        }
        // FEUX-0004 X04: Surround family (E-6: AwaitArgs)
        SurroundAdd => {
            // SurroundAdd requires a single character argument - enter AwaitArgs
            enter_await_args(app, &target, PendingCommand::SurroundAdd);
            iced::Task::none()
        }
        SurroundDelete => {
            // SurroundDelete requires a single character argument - enter AwaitArgs
            enter_await_args(app, &target, PendingCommand::SurroundDelete);
            iced::Task::none()
        }
        SurroundChange => {
            // SurroundChange requires two character arguments - enter AwaitArgs
            enter_await_args(app, &target, PendingCommand::SurroundChange);
            iced::Task::none()
        }
        // FEUX-0005 X01: Indent family (E-3: count = units)
        Indent => {
            let args = if let Some(n) = count {
                serde_json::json!({"amount": n})
            } else {
                serde_json::json!({})
            };
            kernel_task(
                app,
                &target,
                "editor.indent",
                args,
                "Indent was refused; source changed",
            )
        }
        Dedent => {
            let args = if let Some(n) = count {
                serde_json::json!({"amount": n})
            } else {
                serde_json::json!({})
            };
            kernel_task(
                app,
                &target,
                "editor.dedent",
                args,
                "Dedent was refused; source changed",
            )
        }
        // FEUX-0005 X02: Whitespace conversion (E-9: unit display in message)
        TabsToSpaces => kernel_task(
            app,
            &target,
            "editor.tabs_to_spaces",
            serde_json::json!({}),
            "Tabs → Spaces was refused; source changed",
        ),
        SpacesToTabs => kernel_task(
            app,
            &target,
            "editor.spaces_to_tabs",
            serde_json::json!({}),
            "Spaces → Tabs was refused; source changed",
        ),
        // FEUX-0004 X05/X06: Repeat last command (count· for sequential replays)
        RepeatLast => {
            // Replay the last recorded invocation against the current selection set
            // FEUX-0004 X06: if count is present, replay count times
            replay_last_with_count(app, target.clone(), count)
        }
        Toggle
        | Exit
        | Escape
        | Paste
        | Undo
        | Redo
        | Commands
        | Find
        | Replace
        | History
        | SelectMatches
        | SplitSelection
        | KeepMatching { .. } => {
            unreachable!("handled before planning")
        }
    };
    if typing_entry {
        if !app.context_mode.mode(target.tab).active() {
            enter(app, target);
        }
        let (now_ms, burst_enabled) = chart_inputs(app);
        if let Some(s) = &mut app.context_mode.session {
            if let Some(id) = typing_entry_id {
                s.typing_record = Some((id, String::new()));
            }
            let _ = reduce(&mut s.state, &Event::EnterTyping, now_ms, burst_enabled);
        }
    }
    task
}

/// Record an invocation for `.` repeat (FEUX-0004 E-5).
/// Called by the host after a class-E command completes successfully.
pub(crate) fn record_invocation(
    app: &mut App,
    command_id: &'static str,
    args: std::collections::BTreeMap<String, String>,
) {
    use flavite_edit_context::session::InvocationRecord;

    let (now_ms, burst_enabled) = chart_inputs(app);
    if let Some(s) = &mut app.context_mode.session {
        let record = InvocationRecord { command_id, args };
        let _ = reduce(&mut s.state, &Event::Record(record), now_ms, burst_enabled);
    }
}

/// Finalize a pending typing-run record at its group seal (FEUX-0004 E-5):
/// the accumulated run text is recorded for `.` replay. Called wherever a
/// typing run ends (Escape, next dispatch, session exit); empty runs record
/// nothing.
fn finalize_typing_record(app: &mut App) {
    let (command_id, text) = {
        let Some(session) = &mut app.context_mode.session else {
            return;
        };
        let Some(record) = session.typing_record.take() else {
            return;
        };
        record
    };
    if text.is_empty() {
        return;
    }
    let mut args = std::collections::BTreeMap::new();
    args.insert("typed_text".to_string(), text);
    record_invocation(app, command_id, args);
}

/// Replay the last recorded invocation with optional count (FEUX-0004 E-5/X06).
/// If count is Some(n), replay n times sequentially (count·).
/// If count is None, replay once.
/// Returns Task::none() after synchronous execution; all replay work happens
/// in the current frame against the live state, so the UI will refresh with
/// the final result in one pass.
fn replay_last_with_count(
    app: &mut App,
    target: EditorTarget,
    count: Option<u32>,
) -> Task<Message> {
    // Get the last recorded invocation - clone it to avoid borrow issues
    let record = app
        .context_mode
        .session
        .as_ref()
        .and_then(|s| s.state.repeat_last().cloned());

    let Some(record) = record else {
        app.context_mode.notice("No command to repeat");
        diag(
            app,
            target.tab,
            EventKind::RepeatOutcome,
            Outcome::NoRegister,
            None,
        );
        return focus(app, target.tab);
    };

    // FEUX-0004 X06: if count is present, replay count times sequentially
    let count = count.unwrap_or(1);

    // Resolve replay steps from the record. Typing-entry records carry their
    // run text and replay as one batch (E-5): change composes to a typed
    // replacement; the insert_* family inserts the text directly; open-
    // above/below compose open-then-type.
    enum ReplayStep {
        Kernel(&'static str, serde_json::Value),
        SelectionOnly(&'static str, serde_json::Value),
    }
    let typed_text = record.args.get("typed_text").cloned();
    let steps: Vec<ReplayStep> = match (record.command_id, typed_text) {
        ("editor.change", Some(text)) => vec![ReplayStep::Kernel(
            "editor.type",
            serde_json::json!({"text": text, "undo_policy": "isolated"}),
        )],
        (
            "editor.insert_start"
            | "editor.insert_end"
            | "editor.insert_line_start"
            | "editor.insert_line_end",
            Some(text),
        ) => vec![ReplayStep::SelectionOnly(
            record.command_id,
            serde_json::json!({"text": text}),
        )],
        ("editor.open_above" | "editor.open_below", Some(text)) => vec![
            ReplayStep::Kernel(record.command_id, serde_json::json!({})),
            ReplayStep::Kernel(
                "editor.type",
                serde_json::json!({"text": text, "undo_policy": "isolated"}),
            ),
        ],
        (id, _) => {
            let args = serde_json::Value::Object(
                record
                    .args
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect(),
            );
            vec![ReplayStep::Kernel(id, args)]
        }
    };

    // Synchronous replay loop: each dispatch is synchronous, so we apply the
    // steps in sequence against the live app state. After each successful
    // step, we re-capture the target to get the updated revision/selection.
    let mut current_target = target;

    for _ in 0..count {
        for step in &steps {
            let succeeded = match step {
                ReplayStep::Kernel(id, args) => {
                    match kernel_mutate(
                        app,
                        &current_target,
                        id,
                        args.clone(),
                        "Repeat was refused; source changed",
                    ) {
                        Some(success) => {
                            install_receipt(app, &current_target, &success);
                            true
                        }
                        None => false,
                    }
                }
                ReplayStep::SelectionOnly(id, args) => {
                    selections_only(app, &current_target, id, args.clone()).is_some()
                }
            };
            if !succeeded {
                // A refusing replay stops the run with its reason; the
                // register is unchanged (E-5).
                app.context_mode
                    .notice("Repeat was refused; source changed");
                diag(
                    app,
                    current_target.tab,
                    EventKind::RepeatOutcome,
                    Outcome::Refused,
                    Some(current_target.selections.len() as u32),
                );
                return Task::none();
            }
            // Update target for the next step/replay - re-capture from app
            // state.
            if let Some(new_target) = EditorTarget::capture(app, current_target.tab) {
                current_target = new_target;
            } else {
                app.context_mode.notice("Tab closed during repeat");
                diag(
                    app,
                    current_target.tab,
                    EventKind::RepeatOutcome,
                    Outcome::Refused,
                    Some(current_target.selections.len() as u32),
                );
                return Task::none();
            }
        }
    }

    diag(
        app,
        current_target.tab,
        EventKind::RepeatOutcome,
        Outcome::Replayed,
        Some(current_target.selections.len() as u32),
    );
    // Return focus to ensure UI updates
    focus(app, current_target.tab)
}
