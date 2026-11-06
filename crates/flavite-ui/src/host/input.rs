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
pub fn handle(
    app: &mut App,
    tab: TabId,
    incarnation: u64,
    mut input: CapturedInput,
) -> iced::Task<Message> {
    if !app.is_tab_visible(tab)
        || !reading::is_edit(app, tab)
        || app.picker.open
        || app.palette_open
        || app.strip_menu.is_some()
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
        || interaction.composition == CompositionState::Active
        || (input.observed_revision != view.revision && !own_chain)
    {
        app.input_chains.remove(&tab);
        app.status = "Input source changed; place the caret before continuing".into();
        return iced::Task::none();
    }
    let selection = interaction.selections.first().copied().unwrap_or_default();
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
    let prepared =
        match flavite_editor::input::prepare(&view, (selection.anchor, selection.head), &input) {
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
        let Some(api) = &app.flavite else {
            return iced::Task::none();
        };
        let result = api.execute(
            action
                .into_invocation(api.command_registry().next_request_id())
                .expect("prepared mutation"),
        );
        if !result.is_success() {
            app.input_chains.remove(&tab);
            app.status = "Input rejected; source changed or is unavailable".into();
            return iced::Task::none();
        }
        app.workspace.invalidate_writing_target(view.buffer_id);
        app.linking.invalidate_if_source_changed(None);
    }
    let Some(current) = app.note_editor_view(tab) else {
        return iced::Task::none();
    };
    if history {
        let mut at = interaction
            .selections
            .first()
            .map_or(0, |selection| selection.head)
            .min(current.text.len());
        while !current.text.is_char_boundary(at) {
            at -= 1;
        }
        interaction.selections = vec![Selection::caret(at)];
    }
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
    app.reconcile_contexts();
    app.refresh_semantics();
    clipboard.map_or_else(iced::Task::none, iced::clipboard::write)
}
