//! Context-owned editing-history controls over the Session's existing tree.

use super::context_mode::{self, EditorTarget};
use crate::app::{App, Message};
use crate::host::editor_diagnostics::{EventKind, ModeEnum as DiagMode, Outcome};
use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};
use flavite_buffer::history::{
    HistoryCursor, HistoryEntry, HistoryMovement, HistoryPage, HistoryTarget,
};
use flavite_workspace::CommittedSelection;
use iced::{Rectangle, Task};
use std::collections::BTreeMap;

pub const ROW_HEIGHT: f32 = 52.0;
pub const ROW_SPACING: f32 = 2.0;

/// Source-free, stable display metadata. Wall-clock changes never affect node
/// ordering, command grouping, selection, expiry or restore authorization.
pub fn entry_title(entry: &HistoryEntry) -> String {
    if entry.initial {
        return "Initial state".into();
    }
    let time = i64::try_from(entry.changed_at_unix_ms)
        .ok()
        .filter(|&ms| ms != 0)
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S%.3f")
                .to_string()
        })
        .unwrap_or_else(|| "Time unavailable".into());
    format!(
        "{} · {time}{}",
        entry.kind.label(),
        if entry.open { " ···" } else { "" }
    )
}

pub fn entry_status(entry: &HistoryEntry) -> &'static str {
    if entry.current {
        "Current"
    } else if !entry.restorable {
        "Use Undo/Redo to cross the rename transaction"
    } else if entry.on_path {
        "Earlier state"
    } else {
        "Other branch"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryAction {
    Open,
    Close,
    NextPage(HistoryCursor),
    Refresh,
    Restore(HistoryTarget),
}

#[derive(Debug, Clone)]
pub enum HistoryMessage {
    Act(EditorTarget, HistoryAction),
    Bounds {
        source: EditorTarget,
        generation: u64,
        node: u64,
        popup: bool,
        rect: Rectangle,
    },
    ControlBounds(EditorTarget, &'static str, Rectangle),
    Move(bool),
    Pick,
}

#[derive(Debug, Default)]
pub struct HistoryHost {
    pub page: Option<HistoryPage>,
    key: Option<(flavite_workspace::TabId, u64, u64, u64)>,
    pub target: Option<EditorTarget>,
    pub open: bool,
    pub selected: usize,
    pub bounds: BTreeMap<u64, Rectangle>,
    pub rows: BTreeMap<u64, Rectangle>,
    pub controls: BTreeMap<&'static str, Rectangle>,
    pub error: Option<String>,
}

fn key(target: EditorTarget) -> (flavite_workspace::TabId, u64, u64, u64) {
    (
        target.tab,
        target.incarnation,
        target.buffer,
        target.revision,
    )
}

fn trim_bounds(host: &mut HistoryHost) {
    let Some(page) = &host.page else {
        host.bounds.clear();
        host.rows.clear();
        return;
    };
    host.bounds.retain(|node, _| {
        page.trail
            .iter()
            .chain(page.redo.iter())
            .any(|entry| entry.target.node == *node)
    });
    host.rows
        .retain(|node, _| page.entries.iter().any(|entry| entry.target.node == *node));
}

fn read(
    app: &App,
    target: EditorTarget,
    cursor: Option<HistoryCursor>,
) -> Result<HistoryPage, String> {
    let api = app
        .flavite
        .as_ref()
        .ok_or("Open a vault before using editing history")?;
    let result = api.execute(Invocation::new(api.command_registry().next_request_id(), "buffer.history_page",
        serde_json::json!({"buffer_id":target.buffer,"revision":target.revision,"cursor":cursor,"limit":100}), Origin::Gui));
    match result {
        ExecutionResult::Local(LocalResult::Success(success)) => {
            serde_json::from_value(success.data["page"].clone())
                .map_err(|_| "Editing history returned an invalid page".into())
        }
        ExecutionResult::Local(LocalResult::Error(error)) => Err(error.message),
        _ => Err("Editing history changed or is unavailable; refresh the page".into()),
    }
}

pub fn invalidate(app: &mut App) {
    app.context_mode.history.key = None;
}

/// The view and AccessKit use the same bounded strip membership.
pub fn visible_entries(app: &App, source: EditorTarget) -> Vec<&HistoryEntry> {
    if app
        .context_mode
        .session
        .as_ref()
        .is_some_and(|s| s.state.attention())
    {
        return Vec::new();
    }
    if super::context_mode::effect_caption(app, source.tab).is_some() {
        return Vec::new();
    }
    let Some(page) = &app.context_mode.history.page else {
        return Vec::new();
    };
    let width = app
        .editor_bounds
        .get(&source.tab)
        .map_or(640.0, |b| b.width)
        / app.appearance.metrics().scale;
    let count = flavite_editor::context::layout::ContextLayout::new(width).chip_count();
    let redo = page.redo.as_ref().filter(|_| count >= 2);
    let start = page
        .trail
        .len()
        .saturating_sub(count - usize::from(redo.is_some()));
    page.trail[start..].iter().chain(redo).collect()
}

pub fn reconcile(app: &mut App) {
    let Some(session) = &app.context_mode.session else {
        app.context_mode.history = HistoryHost::default();
        return;
    };
    let Some(target) = EditorTarget::capture(app, session.tab) else {
        return;
    };
    if app.context_mode.history.open && app.context_mode.history.target.as_ref() != Some(&target) {
        app.context_mode.history.open = false;
        app.context_mode
            .notice("Source changed; editing history was closed");
    }
    if app.context_mode.history.key == Some(key(target.clone())) {
        return;
    }
    let result = read(app, target.clone(), None);
    let host = &mut app.context_mode.history;
    host.key = Some(key(target));
    host.selected = 0;
    match result {
        Ok(page) => {
            host.page = Some(page);
            host.error = None;
        }
        Err(error) => {
            host.page = None;
            host.error = Some(error);
        }
    }
    trim_bounds(host);
}

pub fn can_restore(app: &App, source: EditorTarget, entry: &HistoryEntry) -> bool {
    // Valid, retained multi-selection states restore like any other state
    // (FR-18/FR-19): the recorded set rides the movement receipt and the
    // primary member is explicit. There is no single-selection admission
    // restriction.
    !entry.current
        && entry.restorable
        && context_mode::valid(app, source.clone(), true)
        && !app.has_composition()
        && !context_mode::too_narrow(app, source.tab)
        && app.flavite.as_ref().is_some_and(|api| !api.is_read_only())
        && app.dialog.is_none()
        && app.shutdown.is_none()
        && !app.palette_open
        && !app.picker.open
        && app.strip_menu.is_none()
        && !super::note_search::blocks_typing(app, &source.tab)
}

pub fn handle(app: &mut App, message: HistoryMessage) -> Task<Message> {
    match message {
        HistoryMessage::Bounds {
            source,
            generation,
            node,
            popup,
            rect,
        } => {
            if generation == app.context_mode.generation()
                && context_mode::valid(app, source, false)
            {
                let map = if popup {
                    &mut app.context_mode.history.rows
                } else {
                    &mut app.context_mode.history.bounds
                };
                map.insert(node, rect);
            }
            Task::none()
        }
        HistoryMessage::ControlBounds(source, id, rect) => {
            if context_mode::valid(app, source, false) {
                app.context_mode.history.controls.insert(id, rect);
            }
            Task::none()
        }
        HistoryMessage::Move(forward) => {
            let host = &mut app.context_mode.history;
            if let Some(page) = &host.page
                && !page.entries.is_empty()
            {
                let n = page.entries.len();
                host.selected = if forward {
                    (host.selected + 1) % n
                } else {
                    (host.selected + n - 1) % n
                };
                return iced::widget::operation::scroll_to(
                    iced::widget::Id::new("editing-history-rows"),
                    iced::widget::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(
                            (host.selected as f32
                                * (ROW_HEIGHT + ROW_SPACING)
                                * app.appearance.metrics().scale
                                - 120.0 * app.appearance.metrics().scale)
                                .max(0.0),
                        ),
                    },
                );
            }
            Task::none()
        }
        HistoryMessage::Pick => {
            let host = &app.context_mode.history;
            if let (Some(source), Some(entry)) = (
                host.target.clone(),
                host.page
                    .as_ref()
                    .and_then(|page| page.entries.get(host.selected)),
            ) {
                return handle(
                    app,
                    HistoryMessage::Act(source, HistoryAction::Restore(entry.target)),
                );
            }
            Task::none()
        }
        HistoryMessage::Act(source, action) => {
            if action == HistoryAction::Close {
                if app
                    .context_mode
                    .history
                    .target
                    .as_ref()
                    .is_some_and(|target| {
                        target.tab == source.tab && target.incarnation == source.incarnation
                    })
                {
                    app.context_mode.history.open = false;
                }
                return context_mode::resume_focus(app);
            }
            if !context_mode::valid(app, source.clone(), true)
                || !app.context_mode.mode(source.tab).active()
                || app.has_composition()
                || app.dialog.is_some()
                || app.shutdown.is_some()
                || app.palette_open
                || app.picker.open
                || app.strip_menu.is_some()
            {
                app.context_mode
                    .notice("Source changed; choose editing history again");
                return Task::none();
            }
            match action {
                HistoryAction::Open | HistoryAction::Refresh | HistoryAction::NextPage(_) => {
                    app.context_mode.clear_preview();
                    let cursor = if let HistoryAction::NextPage(cursor) = action {
                        if app.context_mode.history.page.as_ref().and_then(|p| p.next)
                            != Some(cursor)
                        {
                            return Task::none();
                        }
                        Some(cursor)
                    } else {
                        None
                    };
                    context_mode::seal(app, source.tab);
                    // History owns keyboard input until dismissed. Keep find
                    // drafts, but do not let their previous focus disable all
                    // restores or receive text behind this chooser.
                    if let Some(find) = app.note_search.finds.get_mut(&source.tab) {
                        find.focused = false;
                    }
                    if let Some(replace) = app.note_search.replaces.get_mut(&source.tab) {
                        replace.focused = false;
                    }
                    if let Some(session) = &mut app.context_mode.session {
                        let _ = session.state.close_menu();
                        session.menu_target = None;
                    }
                    let result = read(app, source.clone(), cursor);
                    let mode = DiagMode::from(app.context_mode.mode(source.tab));
                    let source_tab = source.tab;
                    let mut refused = false;
                    {
                        let host = &mut app.context_mode.history;
                        host.key = Some(key(source.clone()));
                        host.target = Some(source);
                        host.open = true;
                        host.selected = 0;
                        match result {
                            Ok(page) => {
                                host.page = Some(page);
                                host.error = None;
                            }
                            Err(error) => {
                                host.error = Some(error);
                                refused = true;
                            }
                        }
                    }
                    let truncated = app
                        .context_mode
                        .history
                        .page
                        .as_ref()
                        .is_some_and(|page| page.history_truncated);
                    if refused {
                        app.diagnostics.record(
                            Some(source_tab),
                            mode,
                            EventKind::HistoryRefusal,
                            Outcome::Refused,
                            None,
                            None,
                        );
                    } else {
                        if truncated {
                            // Visible truncation: eviction reached the
                            // queried range (E-12).
                            app.diagnostics.record(
                                Some(source_tab),
                                mode,
                                EventKind::HistoryEvict,
                                Outcome::Ready,
                                None,
                                None,
                            );
                        }
                        app.diagnostics.record(
                            Some(source_tab),
                            mode,
                            EventKind::HistoryPage,
                            Outcome::Started,
                            None,
                            None,
                        );
                    }
                    trim_bounds(&mut app.context_mode.history);
                    Task::none()
                }
                HistoryAction::Restore(target) => restore(app, source, target),
                HistoryAction::Close => unreachable!("closed before target validation"),
            }
        }
    }
}

fn restore(app: &mut App, source: EditorTarget, target: HistoryTarget) -> Task<Message> {
    let Some(entry) = app
        .context_mode
        .history
        .page
        .as_ref()
        .and_then(|page| {
            page.entries
                .iter()
                .chain(&page.trail)
                .chain(page.redo.iter())
                .find(|e| e.target == target)
        })
        .cloned()
    else {
        app.context_mode
            .notice("Editing-history state expired; refresh history");
        return Task::none();
    };
    if !can_restore(app, source.clone(), &entry) {
        app.context_mode.notice(if entry.restorable {
            "Restore is unavailable for this source"
        } else {
            "Use Undo/Redo to cross the rename transaction"
        });
        return Task::none();
    }
    let Some(api) = &app.flavite else {
        return Task::none();
    };
    let result = api.execute(Invocation::new(
        api.command_registry().next_request_id(),
        "buffer.restore_state",
        serde_json::json!({"buffer_id":source.buffer,"revision":source.revision,"target":target}),
        Origin::Gui,
    ));
    let ExecutionResult::Local(LocalResult::Success(success)) = result else {
        app.context_mode
            .notice("Editing-history state changed or restore was refused; refresh history");
        app.context_mode.history.key = None;
        app.diagnostics.record(
            Some(source.tab),
            DiagMode::from(app.context_mode.mode(source.tab)),
            EventKind::HistoryRestore,
            Outcome::Refused,
            None,
            None,
        );
        return Task::none();
    };
    let Ok(movement) = serde_json::from_value::<HistoryMovement>(success.data["movement"].clone())
    else {
        app.context_mode
            .notice("History restored; refresh the editor selection");
        return Task::none();
    };
    let Some(view) = app.note_editor_view(source.tab) else {
        return Task::none();
    };
    super::input::apply_history_movement(
        app,
        source.tab,
        source.buffer,
        source.revision,
        view.revision,
        &movement,
    );
    if let Some(mut interaction) = app
        .workspace
        .tab(&source.tab)
        .map(|v| v.interaction().clone())
    {
        app.workspace.release_editor_focus(Some(source.tab));
        interaction.focused = true;
        interaction.selections = if let Some(items) = movement.selections.clone() {
            items
                .into_iter()
                .map(|[_, a, h]| CommittedSelection::new(a, h))
                .collect()
        } else if movement.truncated {
            let clamp =
                |at: usize| flavite_editor::viewport::clamp_grapheme_boundary(&view.text, at);
            interaction
                .selections
                .iter()
                .map(|s| CommittedSelection::new(clamp(s.anchor), clamp(s.head)))
                .collect()
        } else {
            super::input::map_selections(&view.text, &interaction.selections, &movement)
        };
        // The restored set's primary member is explicit in the movement
        // receipt (E-4/FR-4); a mapped/clamped set keeps this view's own
        // primary when the cardinality allows.
        interaction.primary = movement
            .primary
            .filter(|&primary| primary < interaction.selections.len())
            .unwrap_or_else(|| interaction.primary.min(interaction.selections.len() - 1));
        interaction.revision = view.revision;
        interaction.sequence += 1;
        interaction.viewport_anchor = super::input::map_history_point(
            &view.text,
            interaction.viewport_anchor,
            &movement,
            flavite_buffer::Affinity::Left,
        );
        let _ = app
            .workspace
            .acknowledge_interaction(interaction, &view.text);
    }
    app.workspace.invalidate_writing_target(source.buffer);
    app.linking.invalidate_if_source_changed(None);
    app.input_chains.remove(&source.tab);
    app.context_mode.capture = None;
    app.context_mode.history.open = false;
    app.context_mode.history.key = None;
    app.context_mode.feedback("Editing state restored");
    app.diagnostics.record(
        Some(source.tab),
        DiagMode::from(app.context_mode.mode(source.tab)),
        EventKind::HistoryRestore,
        Outcome::Applied,
        None,
        None,
    );
    app.reconcile_contexts();
    context_mode::resume_focus(app)
}

pub fn keyboard_message(app: &App, event: &iced::keyboard::Event) -> Option<Message> {
    use iced::keyboard::{Event, Key, key::Named};
    let host = &app.context_mode.history;
    if !host.open || app.dialog.is_some() || app.palette_open || app.picker.open {
        return None;
    }
    let Event::KeyPressed {
        key,
        modifiers,
        repeat,
        ..
    } = event
    else {
        return None;
    };
    if !modifiers.is_empty() {
        return None;
    }
    let message = match key {
        Key::Named(Named::Escape) => {
            HistoryMessage::Act(host.target.clone()?, HistoryAction::Close)
        }
        Key::Named(Named::ArrowDown) => HistoryMessage::Move(true),
        Key::Named(Named::ArrowUp) => HistoryMessage::Move(false),
        Key::Named(Named::Enter) if !repeat => HistoryMessage::Pick,
        _ => return None,
    };
    Some(Message::EditorHistory(message))
}
