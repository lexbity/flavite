//! Ordered input controller. Only a contiguous chain of this view's own
//! acknowledged edits may consume captures made before the next redraw.
use crate::{
    app::{App, Message},
    host::{note_search, reading},
};
use flavite_editor::{
    input::{CapturedInput, PreparedInput},
    keymap::KeyAction,
};
use flavite_workspace::{CompositionState, Selection, TabId};
#[derive(Debug)]
pub struct InputChain {
    incarnation: u64,
    buffer: u64,
    base: u64,
    revision: u64,
    sequence: u64,
    preferred_x: Option<f32>,
}

/// Native event sequence and host interaction sequence are separate: one
/// command can acknowledge several internal selection/edit operations.
#[derive(Debug)]
pub struct ContextCapture {
    target: super::context_mode::EditorTarget,
    base: u64,
    sequence: u64,
    acknowledged: u64,
}

fn context_chain(
    app: &App,
    tab: TabId,
    incarnation: u64,
    request: &flavite_editor::context::ContextRequest,
) -> bool {
    app.context_mode.capture.as_ref().is_some_and(|chain| {
        let current = super::context_mode::EditorTarget::capture(app, tab);
        chain.target.tab == tab
            && chain.target.incarnation == incarnation
            && current == Some(chain.target)
            && chain.target.buffer == request.buffer
            && request.revision >= chain.base
            && request.revision <= chain.target.revision
            && request.sequence > chain.sequence
            && app
                .workspace
                .tab(&tab)
                .is_some_and(|v| v.interaction().sequence == chain.acknowledged)
    })
}

pub fn remember_context(
    app: &mut App,
    before: super::context_mode::EditorTarget,
    request: &flavite_editor::context::ContextRequest,
) {
    let continued = context_chain(app, before.tab, before.incarnation, request);
    let base = app
        .context_mode
        .capture
        .as_ref()
        .filter(|_| continued)
        .map_or(request.revision, |c| c.base);
    let Some(after) = super::context_mode::EditorTarget::capture(app, before.tab) else {
        return;
    };
    // A core turn may pump a foreign change. Only an acknowledged input
    // receipt may authorize a changed revision; a failed command may not.
    if after.revision != before.revision
        && !app.input_chains.get(&before.tab).is_some_and(|c| {
            c.incarnation == before.incarnation
                && c.buffer == before.buffer
                && c.revision == after.revision
                && c.base <= before.revision
        })
    {
        app.context_mode.capture = None;
        return;
    }
    app.context_mode.capture = Some(ContextCapture {
        target: after,
        base,
        sequence: request.sequence,
        acknowledged: app
            .workspace
            .tab(&before.tab)
            .unwrap()
            .interaction()
            .sequence,
    });
}

/// Command keys follow already-acknowledged motions/edits from this same view,
/// even when the next native event arrives before the new frame is painted.
#[must_use]
pub fn accepts_context(
    app: &App,
    tab: TabId,
    incarnation: u64,
    request: &flavite_editor::context::ContextRequest,
) -> bool {
    let (Some(view), Some(session)) = (app.note_editor_view(tab), app.workspace.tab(&tab)) else {
        return false;
    };
    if app.notes.incarnation(tab) != Some(incarnation) || request.buffer != view.buffer_id {
        return false;
    }
    if app.context_mode.capture.as_ref().is_some_and(|chain| {
        chain.target.tab == tab
            && chain.target.incarnation == incarnation
            && chain.target.buffer == request.buffer
            && request.sequence <= chain.sequence
    }) {
        return false;
    }
    if context_chain(app, tab, incarnation, request) {
        return true;
    }
    if request.sequence <= session.interaction().sequence {
        return false;
    }
    let own_chain = app.input_chains.get(&tab).is_some_and(|chain| {
        chain.incarnation == incarnation
            && chain.buffer == request.buffer
            && chain.revision == view.revision
            && request.revision >= chain.base
            && request.revision <= chain.revision
            && request.sequence > chain.sequence
    });
    own_chain
        || (request.revision == view.revision
            && app
                .editor_interaction_snapshot(tab)
                .is_some_and(|s| s.selection == request.selection))
}
pub fn handle(
    app: &mut App,
    tab: TabId,
    incarnation: u64,
    input: CapturedInput,
) -> iced::Task<Message> {
    handle_inner(app, tab, incarnation, input, false, None)
}

/// A toolbar verb plans an effective range without publishing a speculative
/// selection. Its history records the actual committed before-selection.
pub struct CommandIntent {
    pub kind: flavite_buffer::history::EditKind,
    pub before: [usize; 2],
    pub after: Option<[usize; 2]>,
    pub effective_selection: Option<[usize; 2]>,
}

pub fn handle_command(
    app: &mut App,
    tab: TabId,
    incarnation: u64,
    input: CapturedInput,
    intent: CommandIntent,
) -> iced::Task<Message> {
    handle_inner(app, tab, incarnation, input, false, Some(intent))
}

pub fn commit_composition(
    app: &mut App,
    target: super::context_mode::EditorTarget,
    mut input: CapturedInput,
) -> iced::Task<Message> {
    if input.action != KeyAction::Insert || input.text.is_empty() {
        return iced::Task::none();
    }
    input.buffer = target.buffer;
    input.observed_revision = target.revision;
    input.sequence = app
        .workspace
        .tab(&target.tab)
        .unwrap()
        .interaction()
        .sequence
        .saturating_add(1);
    handle_inner(app, target.tab, target.incarnation, input, true, None)
}

fn handle_inner(
    app: &mut App,
    tab: TabId,
    incarnation: u64,
    mut input: CapturedInput,
    composition_commit: bool,
    command_intent: Option<CommandIntent>,
) -> iced::Task<Message> {
    if super::context_mode::too_narrow(app, tab) {
        app.context_mode
            .notice("Pane too narrow · expand the pane to continue editing");
        return iced::Task::none();
    }
    if app.context_mode.mode(tab) == flavite_editor::context::ContextMode::Command
        && matches!(
            input.action,
            KeyAction::Insert
                | KeyAction::Newline
                | KeyAction::Backspace
                | KeyAction::BackspaceWord
        )
    {
        app.context_mode
            .notice("Commands are active · choose Type to enter text");
        return iced::Task::none();
    }
    if !app.is_tab_visible(tab)
        || app.context_mode.history.open
        || !reading::is_edit(app, tab)
        || app.picker.open
        || app.palette_open
        || app.strip_menu.is_some()
        || app.dialog.is_some()
        || app.shutdown.is_some()
        || note_search::blocks_typing(app, &tab)
        || app.notes.incarnation(tab) != Some(incarnation)
    {
        return iced::Task::none();
    }
    let Some(view) = app.note_editor_view(tab) else {
        return iced::Task::none();
    };
    let Some(session) = app.workspace.tab(&tab) else {
        return iced::Task::none();
    };
    let mut interaction = session.interaction().clone();
    let own_chain = app.input_chains.get(&tab).is_some_and(|chain| {
        chain.incarnation == incarnation
            && chain.buffer == input.buffer
            && chain.revision == view.revision
            && input.observed_revision >= chain.base
            && input.observed_revision <= chain.revision
            && input.sequence > chain.sequence
    });
    if input.buffer != view.buffer_id
        || input.sequence <= interaction.sequence
        || (interaction.composition == CompositionState::Active && !composition_commit)
        || (input.observed_revision != view.revision && !own_chain)
    {
        app.input_chains.remove(&tab);
        app.status = "Input source changed; place the caret before continuing".into();
        return iced::Task::none();
    }
    if composition_commit {
        interaction.composition = CompositionState::Idle;
    }
    let selection = interaction.selections.first().copied().unwrap_or_default();
    if matches!(
        input.action,
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
        super::context_mode::selection_changed(app, tab);
    }
    if let Some(motion) = input.vertical.as_mut() {
        // Geometry is captured by the widget, but the sticky column belongs
        // to the acknowledged input chain. A preceding Home/Left/edit may
        // not have redrawn yet, so the widget's old caret is not authority.
        motion.preferred_x = app
            .input_chains
            .get(&tab)
            .filter(|chain| own_chain && chain.sequence == interaction.sequence)
            .and_then(|chain| chain.preferred_x);
    }
    let effective = command_intent
        .as_ref()
        .and_then(|intent| intent.effective_selection)
        .map_or((selection.anchor, selection.head), |[anchor, head]| {
            (anchor, head)
        });
    let prepared = match flavite_editor::input::prepare(&view, effective, &input) {
        Ok(prepared) => prepared,
        Err(message) => {
            app.status = message.into();
            return iced::Task::none();
        }
    };
    let mut clipboard = None;
    let mut mutation = None;
    let mut history = false;
    let mut preferred_x = None;
    match prepared {
        PreparedInput::Edit {
            action,
            caret,
            clipboard: copied,
        } => {
            mutation = Some(action);
            interaction.selections = vec![Selection::caret(caret)];
            if let Some(after) = command_intent.as_ref().and_then(|intent| intent.after) {
                interaction.selections = vec![Selection::new(after[0], after[1])];
            }
            clipboard = copied;
        }
        PreparedInput::History(action) => {
            mutation = Some(action);
            history = true;
        }
        PreparedInput::Selection { anchor, head } => {
            interaction.selections = vec![Selection::new(anchor, head)]
        }
        PreparedInput::VerticalSelection {
            anchor,
            head,
            preferred_x: x,
        } => {
            interaction.selections = vec![Selection::new(anchor, head)];
            preferred_x = Some(x);
        }
        PreparedInput::Copy(copied) => clipboard = Some(copied),
    }
    if let Some(action) = mutation {
        let edit_change = action.as_edit().and_then(|(_, _, edits)| {
            (edits.len() == 1).then(|| flavite_buffer::history::HistoryChange {
                start: edits[0].range.start.0,
                end: edits[0].range.end.0,
                inserted: edits[0].replacement.len(),
            })
        });
        let Some(api) = &app.flavite else {
            return iced::Task::none();
        };
        let mut invocation = action
            .into_invocation(api.command_registry().next_request_id())
            .expect("prepared mutation");
        if invocation.command_id == "apply_edits" {
            use flavite_buffer::history::{EditKind, EditorEdit};
            let kind = match input.action {
                KeyAction::Cut => EditKind::Cut,
                KeyAction::Paste => EditKind::Paste,
                KeyAction::Delete | KeyAction::DeleteWord => EditKind::Delete,
                KeyAction::Backspace | KeyAction::BackspaceWord => EditKind::Backspace,
                _ => EditKind::Typing,
            };
            invocation.args["editor"] = serde_json::to_value(EditorEdit {
                origin: format!("{:032x}", tab.raw()),
                kind: command_intent.as_ref().map_or(kind, |intent| intent.kind),
                before: vec![
                    command_intent
                        .as_ref()
                        .map_or([selection.anchor, selection.head], |intent| intent.before),
                ],
                after: interaction
                    .selections
                    .iter()
                    .map(|s| [s.anchor, s.head])
                    .collect(),
            })
            .expect("history record is serializable");
            let typing = app.context_mode.mode(tab)
                != flavite_editor::context::ContextMode::Command
                && !matches!(input.action, KeyAction::Paste | KeyAction::Cut);
            invocation.args["undo_policy"] = if typing
                && app.context_mode.mode(tab) == flavite_editor::context::ContextMode::Typing
            {
                serde_json::json!({"context":{"origin":format!("{:032x}",tab.raw())}})
            } else {
                serde_json::json!(if typing { "typing" } else { "isolated" })
            };
        }
        let result = api.execute(invocation);
        if !result.is_success() {
            app.input_chains.remove(&tab);
            app.status = "Input rejected; source changed or is unavailable".into();
            app.context_mode
                .notice("Input rejected; source changed or is unavailable");
            return iced::Task::none();
        }
        if let Some(change) = edit_change
            && let Some(current) = app.note_editor_view(tab)
        {
            apply_history_movement(
                app,
                tab,
                view.buffer_id,
                view.revision,
                current.revision,
                &flavite_buffer::history::HistoryMovement {
                    selections: None,
                    change,
                },
            );
        }
        if history
            && let flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) =
                &result
            && let Some(value) = success
                .data
                .get("movement")
                .filter(|value| !value.is_null())
            && let Ok(movement) =
                serde_json::from_value::<flavite_buffer::history::HistoryMovement>(value.clone())
        {
            let Some(current) = app.note_editor_view(tab) else {
                return iced::Task::none();
            };
            apply_history_movement(
                app,
                tab,
                view.buffer_id,
                view.revision,
                current.revision,
                &movement,
            );
            if let Some(selections) = movement.selections {
                interaction.selections = selections
                    .into_iter()
                    .map(|[a, h]| Selection::new(a, h))
                    .collect();
            } else {
                interaction.selections =
                    map_selections(&current.text, &interaction.selections, &movement.change);
            }
        }
        app.workspace.invalidate_writing_target(view.buffer_id);
        app.linking.invalidate_if_source_changed(None);
    }
    let Some(current) = app.note_editor_view(tab) else {
        return iced::Task::none();
    };
    // The exact source edit's acknowledged caret authorizes typing. History
    // lacks an exact transform and requires a subsequent caret interaction.
    let retain_target = app.workspace.writing_target().is_some();
    interaction.focused = true;
    interaction.buffer_id = Some(current.buffer_id);
    interaction.revision = current.revision;
    interaction.sequence = input.sequence;
    interaction.viewport_anchor = interaction.viewport_anchor.min(current.text.len());
    while !current.text.is_char_boundary(interaction.viewport_anchor) {
        interaction.viewport_anchor -= 1;
    }
    if app
        .workspace
        .acknowledge_interaction(interaction, &current.text)
        .is_err()
    {
        app.input_chains.remove(&tab);
        app.status = "Input applied; refresh the editor caret".into();
        return iced::Task::none();
    }
    if history || (input.action == KeyAction::Copy && !retain_target) {
        app.workspace.invalidate_writing_target(current.buffer_id);
    }
    let base = if own_chain {
        app.input_chains[&tab].base
    } else {
        input.observed_revision
    };
    app.input_chains.insert(
        tab,
        InputChain {
            incarnation,
            buffer: input.buffer,
            base,
            revision: current.revision,
            sequence: input.sequence,
            preferred_x,
        },
    );
    if let Some(find) = app.note_search.finds.get_mut(&tab) {
        find.focused = false;
    }
    if let Some(replace) = app.note_search.replaces.get_mut(&tab) {
        replace.focused = false;
    }
    app.status = if matches!(input.action, KeyAction::Copy) {
        "Copied selection".into()
    } else {
        "Editing current note".into()
    };
    if let Some(session) = &mut app.context_mode.session {
        session.attention = false;
    }
    app.reconcile_contexts();
    app.refresh_semantics();
    clipboard.map_or_else(iced::Task::none, iced::clipboard::write)
}

pub fn map_selections(
    text: &str,
    selections: &[Selection],
    change: &flavite_buffer::history::HistoryChange,
) -> Vec<Selection> {
    let point = |at, affinity| map_history_point(text, at, change, affinity);
    selections
        .iter()
        .map(|s| {
            use flavite_buffer::Affinity::{Left, Right};
            if s.anchor < s.head {
                Selection::new(point(s.anchor, Left), point(s.head, Right))
            } else if s.anchor > s.head {
                Selection::new(point(s.anchor, Right), point(s.head, Left))
            } else {
                Selection::caret(point(s.head, Right))
            }
        })
        .collect()
}

pub fn map_history_point(
    text: &str,
    at: usize,
    change: &flavite_buffer::history::HistoryChange,
    affinity: flavite_buffer::Affinity,
) -> usize {
    let mapped = flavite_buffer::positions::map_through_replacement(
        at,
        flavite_buffer::TextEdit::new(change.start, change.end, change.inserted),
        affinity,
    );
    let floor = flavite_editor::viewport::clamp_grapheme_boundary(text, mapped);
    if affinity == flavite_buffer::Affinity::Right && floor < mapped.min(text.len()) {
        flavite_editor::viewport::next_grapheme_boundary(text, flavite_syntax::TextByte(floor)).0
    } else {
        floor
    }
}

/// Map passive views once, without adopting the initiating view's selection
/// or stealing focus. Their old revision is an exact mapping guard.
pub fn apply_history_movement(
    app: &mut App,
    source: TabId,
    buffer: u64,
    before: u64,
    after: u64,
    movement: &flavite_buffer::history::HistoryMovement,
) {
    let Some(view) = app.note_editor_view(source) else {
        return;
    };
    let mapped: Vec<_> = app
        .workspace
        .tabs()
        .filter(|(id, _)| **id != source)
        .filter_map(|(_, tab)| {
            let mut interaction = tab.interaction().clone();
            if interaction.buffer_id != Some(buffer) || interaction.revision != before {
                return None;
            }
            interaction.selections =
                map_selections(&view.text, &interaction.selections, &movement.change);
            interaction.revision = after;
            interaction.sequence += 1;
            interaction.viewport_anchor = map_history_point(
                &view.text,
                interaction.viewport_anchor,
                &movement.change,
                flavite_buffer::Affinity::Left,
            );
            Some(interaction)
        })
        .collect();
    for interaction in mapped {
        let _ = app
            .workspace
            .acknowledge_interaction(interaction, &view.text);
    }
}
