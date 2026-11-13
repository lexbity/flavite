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
use flavite_workspace::{CommittedSelection, CompositionState, TabId};
#[derive(Debug)]
pub struct InputChain {
    incarnation: u64,
    buffer: u64,
    base: u64,
    revision: u64,
    sequence: u64,
    preferred_x: Option<f32>,
}

impl InputChain {
    /// Construct one chain entry (host/context-mode shared path).
    pub(crate) fn new(
        incarnation: u64,
        buffer: u64,
        base: u64,
        revision: u64,
        sequence: u64,
        preferred_x: Option<f32>,
    ) -> Self {
        Self {
            incarnation,
            buffer,
            base,
            revision,
            sequence,
            preferred_x,
        }
    }

    /// The chain base revision.
    pub(crate) const fn base(&self) -> u64 {
        self.base
    }
}

/// Native event sequence and host interaction sequence are separate: one
/// command can acknowledge several internal selection/edit operations.
#[derive(Debug)]
pub struct ContextCapture {
    target: super::context_mode::EditorTarget,
    base: u64,
    sequence: u64,
    acknowledged: u64,
    /// Identity of the last handled request (E-5 duplicate delivery): the
    /// sequence alone cannot prove a replay — two live widget states for
    /// one view can publish a reused sequence — so a duplicate is the same
    /// request *content* delivered again, never merely a colliding number.
    last: Option<RequestFingerprint>,
}

/// Identity of one handled context request.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RequestFingerprint {
    sequence: u64,
    tag: String,
}

fn request_fingerprint(request: &flavite_editor::context::ContextRequest) -> RequestFingerprint {
    RequestFingerprint {
        sequence: request.sequence,
        tag: format!("{:?}", request.event),
    }
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
            && current.as_ref() == Some(&chain.target)
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
        last: Some(request_fingerprint(request)),
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
    accepts_context_inner(app, tab, incarnation, request)
}

fn accepts_context_inner(
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
    // Duplicate delivery / stale replay (E-5): a request strictly older
    // than the last handled one is a replay and is refused. A request at
    // exactly the last handled sequence is refused only when it is the
    // SAME request delivered again; two live widget states for one view
    // publish independent counters and can reuse the last sequence for a
    // genuinely new key, and its different content is what admits it.
    if app.context_mode.capture.as_ref().is_some_and(|chain| {
        chain.target.tab == tab
            && chain.target.incarnation == incarnation
            && chain.target.buffer == request.buffer
            && (request.sequence < chain.sequence
                || (request.sequence == chain.sequence
                    && chain
                        .last
                        .as_ref()
                        .is_some_and(|fingerprint| fingerprint == &request_fingerprint(request))))
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
    handle_inner(app, tab, incarnation, input, false)
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
    handle_inner(app, target.tab, target.incarnation, input, true)
}

fn handle_inner(
    app: &mut App,
    tab: TabId,
    incarnation: u64,
    mut input: CapturedInput,
    composition_commit: bool,
) -> iced::Task<Message> {
    if super::context_mode::too_narrow(app, tab) {
        app.context_mode
            .notice("Pane too narrow · expand the pane to continue editing");
        return iced::Task::none();
    }
    if app.context_mode.mode(tab) == flavite_edit_context::ContextMode::Command
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
    let interaction = {
        let Some(session) = app.workspace.tab(&tab) else {
            return iced::Task::none();
        };
        session.interaction().clone()
    };
    let mut interaction = interaction;
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
    if let Some(sticky) = input.preferred_x.get_mut(0) {
        // Geometry is captured by the widget, but the primary member's
        // sticky column belongs to the acknowledged input chain. A
        // preceding Home/Left/edit may not have redrawn yet, so the
        // widget's old caret is not authority. Other members keep their
        // captured columns (X04 per-member geometry).
        let chained = app
            .input_chains
            .get(&tab)
            .filter(|_| own_chain && app.input_chains[&tab].sequence == interaction.sequence);
        if let Some(chain) = chained {
            *sticky = chain.preferred_x;
        }
    }
    // A paste reads at the clipboard seam: the intent parks here and
    // resumes through ClipboardMessage::TypingPaste with the text (X04).
    // A resumed intent carries its acquired text and proceeds directly
    // (FR-8/FR-9): acquisition and completed paste are distinct stages.
    if input.action == KeyAction::Paste && !input.clipboard_acquired {
        return super::editor_clipboard::read_for_typing(tab, incarnation, input);
    }
    // The whole committed set plans the action (FEUX-0002 X04, E-6).
    let committed: Vec<flavite_buffer::selection::RawSelection> = interaction
        .selections
        .iter()
        .enumerate()
        .map(|(id, selection)| flavite_buffer::selection::RawSelection {
            id: Some(id as u32),
            anchor: selection.anchor,
            head: selection.head,
        })
        .collect();
    let Ok((set, _)) = flavite_buffer::selection::SelectionSet::normalize(
        &view.text,
        committed,
        Some(
            interaction
                .primary
                .min(interaction.selections.len().saturating_sub(1)) as u32,
        ),
    ) else {
        app.input_chains.remove(&tab);
        app.status = "Selection changed; place the caret before continuing".into();
        return iced::Task::none();
    };
    // FR-20: an IME commit joins Typing at the primary caret only; the
    // other members are retained and mapped through the same batch.
    let planning_set = if composition_commit {
        let primary = set.primary();
        flavite_buffer::selection::SelectionSet::normalize(
            &view.text,
            vec![flavite_buffer::selection::RawSelection {
                id: Some(primary.id().as_raw()),
                anchor: primary.anchor(),
                head: primary.head(),
            }],
            Some(primary.id().as_raw()),
        )
        .map(|(planning, _)| planning)
        .unwrap_or_else(|_| set.clone())
    } else {
        set.clone()
    };
    let prepared = match flavite_editor::input::prepare(&view, &planning_set, &input) {
        Ok(prepared) => prepared,
        Err(message) => {
            app.status = message.into();
            return iced::Task::none();
        }
    };
    let mut clipboard = None;
    let mut mutation = None;
    let mut history = false;
    let mut preferred_x: Option<f32> = None;
    let mut primary = 0usize;
    // The recorded before-set is the committed set at capture time (before
    // any optimistic after-set replaces it) — FEUX-0002 FR-19/E-4.
    let before_members = interaction.selections.clone();
    let mut after_members: Option<Vec<(usize, usize)>> = None;
    match prepared {
        PreparedInput::Edit {
            action,
            members,
            primary: prepared_primary,
            clipboard: copied,
        } => {
            let members = if composition_commit {
                let edits = action
                    .as_edit()
                    .map(|(_, _, edits)| edits)
                    .unwrap_or_default();
                match composition_after_set(&view.text, &set, &members, edits) {
                    Some((after, primary_index)) => {
                        primary = primary_index;
                        after
                    }
                    None => {
                        primary = prepared_primary;
                        members
                    }
                }
            } else {
                primary = prepared_primary;
                members
            };
            mutation = Some(action);
            after_members = Some(members.clone());
            interaction.selections = members
                .iter()
                .map(|&(anchor, head)| CommittedSelection::new(anchor, head))
                .collect();
            interaction.primary = primary;
            clipboard = copied;
        }
        PreparedInput::History(action) => {
            mutation = Some(action);
            history = true;
        }
        PreparedInput::Selection {
            members,
            primary: prepared_primary,
        } => {
            primary = prepared_primary;
            interaction.selections = members
                .iter()
                .map(|&(anchor, head)| CommittedSelection::new(anchor, head))
                .collect();
            interaction.primary = primary;
        }
        PreparedInput::VerticalSelection {
            members,
            primary: prepared_primary,
            preferred_x: columns,
        } => {
            primary = prepared_primary;
            interaction.selections = members
                .iter()
                .map(|&(anchor, head)| CommittedSelection::new(anchor, head))
                .collect();
            interaction.primary = primary;
            preferred_x = columns.first().copied().flatten();
        }
        PreparedInput::Copy { text } => clipboard = Some(text),
    }
    if let Some(action) = mutation {
        // §5.5 clipboard ordering for the ordinary cut: the clipboard write
        // completes before the guarded deletion, which resumes through
        // ClipboardMessage::CutCompleted with the same captured intent.
        if input.action == KeyAction::Cut
            && !input.clipboard_acquired
            && clipboard.as_deref().is_some_and(|text| !text.is_empty())
        {
            let cut_text = clipboard.expect("cut prepared with clipboard text");
            return super::editor_clipboard::write_for_cut_resume(
                tab,
                incarnation,
                input,
                cut_text,
            );
        }
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
                kind,
                before: before_members
                    .iter()
                    .enumerate()
                    .map(|(id, s)| [id, s.anchor, s.head])
                    .collect(),
                after: after_members
                    .as_ref()
                    .expect("edit prepared with an after-set")
                    .iter()
                    .enumerate()
                    .map(|(id, &(anchor, head))| [id, anchor, head])
                    .collect(),
                primary,
            })
            .expect("history record is serializable");
            let typing = app.context_mode.mode(tab) != flavite_edit_context::ContextMode::Command
                && !matches!(input.action, KeyAction::Paste | KeyAction::Cut);
            invocation.args["undo_policy"] = if typing
                && app.context_mode.mode(tab) == flavite_edit_context::ContextMode::Typing
            {
                serde_json::json!({"context":{"origin":format!("{:032x}",tab.raw())}})
            } else {
                serde_json::json!(if typing { "typing" } else { "isolated" })
            };
        }
        let result = api.execute(invocation);
        if !result.is_success() {
            app.input_chains.remove(&tab);
            if input.action == KeyAction::Cut {
                // Clipboard ordering (§5.5): the copy landed, the cut did
                // not.
                app.status = "Copied; cut was not applied because the source changed".into();
                app.context_mode
                    .notice("Copied; cut was not applied because the source changed");
            } else {
                app.status = "Input rejected; source changed or is unavailable".into();
                app.context_mode
                    .notice("Input rejected; source changed or is unavailable");
            }
            return iced::Task::none();
        }
        if let flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) =
            &result
        {
            // One movement receipt covers the whole batch (E-13): map
            // passive views once, then install the exact after-set (or map
            // when the receipt carries none).
            let movement = success
                .data
                .get("movement")
                .filter(|value| !value.is_null())
                .and_then(|value| {
                    serde_json::from_value::<flavite_buffer::history::HistoryMovement>(
                        value.clone(),
                    )
                    .ok()
                });
            let after = success
                .data
                .get("after_selections")
                .filter(|value| !value.is_null())
                .and_then(|value| serde_json::from_value::<Vec<[usize; 3]>>(value.clone()).ok());
            if let Some(current) = app.note_editor_view(tab) {
                if let Some(movement) = &movement {
                    apply_history_movement(
                        app,
                        tab,
                        view.buffer_id,
                        view.revision,
                        current.revision,
                        movement,
                    );
                }
                if let Some(after) = after {
                    interaction.selections = after
                        .iter()
                        .map(|[_, a, h]| CommittedSelection::new(*a, *h))
                        .collect();
                } else if let Some(movement) = &movement {
                    if let Some(selections) = &movement.selections {
                        // The exact recorded set rides the receipt (X03/X04).
                        interaction.selections = selections
                            .iter()
                            .map(|[_, a, h]| CommittedSelection::new(*a, *h))
                            .collect();
                    } else if movement.truncated {
                        // MovementTooComplex: clamp to the document and let
                        // the user re-place the caret; never guess
                        // (FEUX-0002 E-13).
                        interaction.selections = interaction
                            .selections
                            .iter()
                            .map(|s| {
                                let clamp = |at: usize| {
                                    flavite_editor::viewport::clamp_grapheme_boundary(
                                        &current.text,
                                        at,
                                    )
                                };
                                CommittedSelection::new(clamp(s.anchor), clamp(s.head))
                            })
                            .collect();
                        app.context_mode
                            .notice("Source changed; selections approximated");
                    } else {
                        interaction.selections =
                            map_selections(&current.text, &interaction.selections, movement);
                    }
                }
                // The primary member rides the receipt (E-4/FR-4); when the
                // receipt does not name one, keep the current member when it
                // survives the new set's cardinality.
                let receipt_primary = movement
                    .as_ref()
                    .and_then(|movement| movement.primary)
                    .filter(|&primary| primary < interaction.selections.len());
                interaction.primary = receipt_primary.unwrap_or_else(|| {
                    interaction
                        .primary
                        .min(interaction.selections.len().saturating_sub(1))
                });
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
        session.state.clear_attention();
    }
    app.reconcile_contexts();
    app.refresh_semantics();
    // The clipboard write for a resumed cut already completed at the seam.
    if input.clipboard_acquired {
        clipboard = None;
    }
    clipboard.map_or_else(iced::Task::none, super::editor_clipboard::write)
}

/// The composition commit's after-set (FR-20): the composed text landed at
/// the primary member only, so every other member is retained and mapped
/// through the primary's edit batch, then re-normalized against the
/// after-text. Returns the member pairs and the primary index.
fn composition_after_set(
    text: &str,
    set: &flavite_buffer::selection::SelectionSet,
    primary_after: &[(usize, usize)],
    edits: &[flavite_buffer::undo::EditOp],
) -> Option<(Vec<(usize, usize)>, usize)> {
    use flavite_buffer::selection::{RawSelection, map_point_through_spans};
    let &(anchor, head) = primary_after.first()?;
    let edit = edits.first()?;
    let after_text = edit.apply(text);
    let span = [flavite_buffer::history::MovementSpan {
        start: edit.range.start.0,
        end: edit.range.end.0,
        replacement_len: edit.replacement.len(),
    }];
    // The primary keeps id 0; every other member gets a fresh distinct id
    // so the primary hint resolves exactly and merge survivors stay
    // deterministic.
    let mut candidates = Vec::with_capacity(set.len());
    for (index, member) in set.iter().enumerate() {
        if member.id() == set.primary_id() {
            candidates.push(RawSelection {
                id: Some(0),
                anchor,
                head,
            });
            continue;
        }
        let (anchor_affinity, head_affinity) = if member.anchor() < member.head() {
            (
                flavite_buffer::Affinity::Left,
                flavite_buffer::Affinity::Right,
            )
        } else if member.anchor() > member.head() {
            (
                flavite_buffer::Affinity::Right,
                flavite_buffer::Affinity::Left,
            )
        } else {
            (
                flavite_buffer::Affinity::Right,
                flavite_buffer::Affinity::Right,
            )
        };
        candidates.push(RawSelection {
            id: Some(index as u32 + 1),
            anchor: map_point_through_spans(member.anchor(), &span, anchor_affinity),
            head: map_point_through_spans(member.head(), &span, head_affinity),
        });
    }
    let (after_set, _) =
        flavite_buffer::selection::SelectionSet::normalize(&after_text, candidates, Some(0))
            .ok()?;
    let primary_index = after_set
        .members()
        .iter()
        .position(|member| member.id() == after_set.primary_id())
        .unwrap_or(0);
    let members = after_set
        .members()
        .iter()
        .map(|member| (member.anchor(), member.head()))
        .collect();
    Some((members, primary_index))
}

pub fn map_selections(
    text: &str,
    selections: &[CommittedSelection],
    movement: &flavite_buffer::history::HistoryMovement,
) -> Vec<CommittedSelection> {
    let point = |at, affinity| map_history_point(text, at, movement, affinity);
    selections
        .iter()
        .map(|s| {
            use flavite_buffer::Affinity::{Left, Right};
            if s.anchor < s.head {
                CommittedSelection::new(point(s.anchor, Left), point(s.head, Right))
            } else if s.anchor > s.head {
                CommittedSelection::new(point(s.anchor, Right), point(s.head, Left))
            } else {
                CommittedSelection::caret(point(s.head, Right))
            }
        })
        .collect()
}

pub fn map_history_point(
    text: &str,
    at: usize,
    movement: &flavite_buffer::history::HistoryMovement,
    affinity: flavite_buffer::Affinity,
) -> usize {
    let mapped = flavite_buffer::selection::map_point_through_spans(at, &movement.spans, affinity);
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
            if movement.truncated {
                // MovementTooComplex: clamp, never guess. The notice is
                // published by the initiating view's handler; the passive
                // view only avoids stale coordinates.
                let clamp =
                    |at: usize| flavite_editor::viewport::clamp_grapheme_boundary(&view.text, at);
                interaction.selections = interaction
                    .selections
                    .iter()
                    .map(|s| CommittedSelection::new(clamp(s.anchor), clamp(s.head)))
                    .collect();
            } else {
                interaction.selections =
                    map_selections(&view.text, &interaction.selections, movement);
            }
            // The mapped set may have merged members: keep this view's own
            // primary member when its cardinality allows (E-4/FR-4). The
            // receipt's primary index belongs to the initiating view's
            // after-set, not to this passive mapping.
            interaction.primary = interaction.primary.min(interaction.selections.len() - 1);
            interaction.revision = after;
            interaction.sequence += 1;
            interaction.viewport_anchor = map_history_point(
                &view.text,
                interaction.viewport_anchor,
                movement,
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
