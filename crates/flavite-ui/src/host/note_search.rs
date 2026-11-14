//! Note Find/Replace host (RFC-0006 S01/S03).
//!
//! One host owns the per-view find state, the revision-keyed match pages,
//! and the replacement flow through the core: every replacement prepares a
//! plan (`text.prepare_replace`) bound to the exact buffer revision and
//! installs it (`text.apply_replace`) as one isolated, revision-guarded
//! Undo unit. The host never writes text itself; stale or expired plans
//! surface as user-visible errors without effects.
//!
//! Match computation is admitted through `text.find`; the host keeps only
//! immutable revision-bound results and never scans or compiles patterns
//! during a UI update/render turn. Regex work is isolated in the sibling
//! helper by the vault service (D-14).

use crate::app::App;
use flavite_api::{ExecutionResult, Invocation, LocalResult, OperationCompletion, Origin};
use flavite_workspace::TabId;
use std::collections::BTreeMap;

/// S01/S03 default matching is literal and case-insensitive (D-05).
const DEFAULT_MODE: &str = "literal";
/// Typed find/replace inputs are bounded like every other host draft.
const INPUT_LIMIT: usize = 8192;
/// Must match the core's bounded `text.find` page size.
const FIND_PAGE_SIZE: u64 = 256;
/// Shown when a prepared plan can no longer apply (stale/expired).
const STALE_MESSAGE: &str = "Source changed — search again";

/// Per-view find bar state (the shape the reading host used to own).
#[derive(Debug)]
pub struct FindState {
    pub query_input: iced::widget::text_editor::Content,
    pub focused: bool,
    pub query: String,
    pub index: usize,
    /// First match ordinal in the currently displayed page.
    pub page_offset: u64,
    pub mode: String,
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub pending: Option<u64>,
    pending_revision: Option<u64>,
    pending_offset: Option<u64>,
    pub last_error: Option<String>,
    /// Scope captured from the editor selection before Find takes focus.
    /// It is display-only and bound to the revision at capture.
    pub selection_scope: Option<(u64, usize, usize)>,
    pub use_selection_scope: bool,
}

impl Clone for FindState {
    fn clone(&self) -> Self {
        Self {
            query_input: iced::widget::text_editor::Content::with_text(&self.query),
            focused: self.focused,
            query: self.query.clone(),
            index: self.index,
            page_offset: self.page_offset,
            mode: self.mode.clone(),
            case_sensitive: self.case_sensitive,
            whole_word: self.whole_word,
            pending: self.pending,
            pending_revision: self.pending_revision,
            pending_offset: self.pending_offset,
            last_error: self.last_error.clone(),
            selection_scope: self.selection_scope,
            use_selection_scope: self.use_selection_scope,
        }
    }
}

impl Default for FindState {
    fn default() -> Self {
        Self {
            query_input: iced::widget::text_editor::Content::new(),
            focused: false,
            query: String::new(),
            index: 0,
            page_offset: 0,
            mode: DEFAULT_MODE.to_owned(),
            case_sensitive: false,
            whole_word: false,
            pending: None,
            pending_revision: None,
            pending_offset: None,
            last_error: None,
            selection_scope: None,
            use_selection_scope: false,
        }
    }
}

/// Per-view replace row state. `pending` holds the request id of the
/// in-flight `text.prepare_replace`; `match_count`/`plan_id` record the
/// last prepared plan; `last_error` carries the stale/expired refusal.
#[derive(Debug, Default)]
pub struct ReplaceState {
    pub replacement_input: iced::widget::text_editor::Content,
    pub focused: bool,
    pub query: String,
    pub replacement: String,
    pub pending: Option<u64>,
    pub last_error: Option<String>,
    pub match_count: Option<u64>,
    pub plan_id: Option<String>,
}

/// One cached match page, valid for exactly this buffer revision, query,
/// and matching mode. Spans are half-open byte ranges into the snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedMatches {
    pub revision: u64,
    pub query: String,
    pub case_sensitive: bool,
    pub mode: String,
    pub whole_word: bool,
    pub scope: Option<(usize, usize)>,
    pub spans: Vec<(usize, usize)>,
    pub offset: u64,
    /// Exact total proved by the matcher; `spans` contains the returned page.
    pub total_matches: u64,
    pub next_offset: Option<u64>,
    pub truncated: bool,
}

#[derive(Debug, Clone)]
struct FindSnapshot {
    buffer_id: u64,
    revision: u64,
    query: String,
    mode: String,
    case_sensitive: bool,
    whole_word: bool,
    scope: Option<(usize, usize)>,
    offset: u64,
}

/// Host state: find bars, replace rows, cached match pages, and the
/// request-id to tab mapping for in-flight replacement preparations.
#[derive(Debug, Default)]
pub struct NoteSearchHost {
    pub finds: BTreeMap<TabId, FindState>,
    pub replaces: BTreeMap<TabId, ReplaceState>,
    /// `text.prepare_replace` request ids awaiting completion, per tab.
    pub preparations: BTreeMap<u64, TabId>,
    /// `text.find` request ids awaiting completion, per tab.
    pub searches: BTreeMap<u64, TabId>,
    pub matches: BTreeMap<TabId, CachedMatches>,
}

impl NoteSearchHost {
    fn clear_tab(&mut self, tab: &TabId) {
        self.finds.remove(tab);
        self.replaces.remove(tab);
        self.matches.remove(tab);
        self.searches.retain(|_, owner| owner != tab);
        self.preparations.retain(|_, owner| owner != tab);
    }

    fn clear_replace(&mut self, tab: &TabId) {
        self.replaces.remove(tab);
        self.preparations.retain(|_, owner| owner != tab);
    }
}

/// Messages owned and handled by the note search host.
#[derive(Debug, Clone)]
pub enum NoteSearchMessage {
    DraftAction {
        tab: TabId,
        replacement: bool,
        action: iced::widget::text_editor::Action,
    },
    FindOpen(TabId),
    FindClose(TabId),
    FindQuery {
        tab: TabId,
        query: String,
    },
    FindNext {
        tab: TabId,
        forward: bool,
    },
    ReplaceOpen(TabId),
    ReplaceClose(TabId),
    ReplaceQuery {
        tab: TabId,
        query: String,
    },
    ReplaceText {
        tab: TabId,
        text: String,
    },
    SetMode {
        tab: TabId,
        mode: String,
    },
    ToggleCaseSensitive(TabId),
    ToggleWholeWord(TabId),
    ToggleSelectionScope(TabId),
    ReplaceNext(TabId),
    ReplaceAll(TabId),
}

/// Handle one note search message.
pub fn handle(app: &mut App, message: NoteSearchMessage) -> iced::Task<crate::app::Message> {
    match message {
        NoteSearchMessage::DraftAction {
            tab,
            replacement,
            action,
        } => {
            let limit = if replacement { 64 * 1024 } else { INPUT_LIMIT };
            let input = if replacement {
                let Some(state) = app.note_search.replaces.get_mut(&tab) else {
                    return iced::Task::none();
                };
                &mut state.replacement_input
            } else {
                let Some(state) = app.note_search.finds.get_mut(&tab) else {
                    return iced::Task::none();
                };
                &mut state.query_input
            };
            let before = input.text();
            if let iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(
                value,
            )) = &action
                && value.len() > limit
            {
                return iced::Task::none();
            }
            input.perform(action);
            let text = input.text();
            if text.len() > limit {
                *input = iced::widget::text_editor::Content::with_text(&before);
                return iced::Task::none();
            }
            return handle(
                app,
                if replacement {
                    NoteSearchMessage::ReplaceText { tab, text }
                } else {
                    NoteSearchMessage::FindQuery { tab, query: text }
                },
            );
        }
        NoteSearchMessage::FindOpen(tab) => {
            // The editor widget yields keyboard focus to the find input by
            // acknowledging a blurred interaction first (as before S01).
            if crate::host::reading::is_edit(app, tab)
                && let Some(view) = app.note_editor_view(tab)
                && let Some(session) = app.workspace.tab(&tab)
            {
                let mut input = session.interaction().clone();
                input.buffer_id = Some(view.buffer_id);
                input.revision = view.revision;
                input.sequence = input.sequence.saturating_add(1);
                input.focused = false;
                let _ = app.acknowledge_editor_interaction(input, &view.text);
            }
            let captured_scope = app.note_editor_view(tab).and_then(|view| {
                app.workspace
                    .tab(&tab)
                    .and_then(|session| session.interaction().selections.first().copied())
                    .filter(|selection| selection.anchor != selection.head)
                    .map(|selection| {
                        (
                            view.revision,
                            selection.anchor.min(selection.head),
                            selection.anchor.max(selection.head),
                        )
                    })
            });
            let find = app.note_search.finds.entry(tab).or_default();
            find.focused = true;
            find.selection_scope = captured_scope;
            app.refresh_semantics();
            return iced::widget::operation::focus(find_id(tab));
        }
        NoteSearchMessage::FindClose(tab) => {
            // Closing the find bar closes the replace row with it: the
            // replacement acts on the find query, and any in-flight
            // preparation is dropped without effects.
            app.note_search.clear_tab(&tab);
            if let Some(view) = app.note_editor_view(tab)
                && let Some(bounds) = app.editor_bounds.get(&tab).copied()
            {
                return iced::advanced::widget::operate(
                    iced::advanced::widget::operation::focusable::unfocus(),
                )
                .chain(
                    iced::advanced::widget::operate(flavite_editor::focus_editor(
                        view.buffer_id,
                        view.revision,
                        Some(bounds),
                    ))
                    .map(|_| crate::app::Message::Pump),
                );
            }
        }
        NoteSearchMessage::FindQuery { tab, query } => {
            if query.len() <= INPUT_LIMIT
                && let Some(find) = app.note_search.finds.get_mut(&tab)
            {
                find.focused = true;
                if find.query_input.text() != query {
                    find.query_input = iced::widget::text_editor::Content::with_text(&query);
                }
                find.query = query.clone();
                find.index = 0;
                find.page_offset = 0;
                find.pending = None;
                find.pending_revision = None;
                find.pending_offset = None;
                find.last_error = None;
                if let Some(replace) = app.note_search.replaces.get_mut(&tab) {
                    replace.query = query;
                    replace.match_count = None;
                    replace.plan_id = None;
                    replace.last_error = None;
                }
                navigate(app, tab);
            }
        }
        NoteSearchMessage::FindNext { tab, forward } => {
            request_find(app, tab);
            advance_find(app, tab, forward);
        }
        NoteSearchMessage::ReplaceOpen(tab) => {
            if crate::host::reading::is_edit(app, tab)
                && let Some(view) = app.note_editor_view(tab)
                && let Some(session) = app.workspace.tab(&tab)
            {
                let mut input = session.interaction().clone();
                input.buffer_id = Some(view.buffer_id);
                input.revision = view.revision;
                input.sequence = input.sequence.saturating_add(1);
                input.focused = false;
                let _ = app.acknowledge_editor_interaction(input, &view.text);
            }
            // The replace row rides on the find row and inherits its query,
            // so both rows always act on the same visible query.
            let query = app.note_search.finds.entry(tab).or_default().query.clone();
            let replace = app.note_search.replaces.entry(tab).or_default();
            replace.focused = true;
            replace.query = query;
            replace.last_error = None;
            app.refresh_semantics();
            return iced::widget::operation::focus(replace_id(tab));
        }
        NoteSearchMessage::ReplaceClose(tab) => {
            app.note_search.clear_replace(&tab);
        }
        NoteSearchMessage::ReplaceQuery { tab, query } => {
            if query.len() <= INPUT_LIMIT
                && let Some(replace) = app.note_search.replaces.get_mut(&tab)
            {
                replace.focused = true;
                replace.query = query.clone();
                replace.match_count = None;
                replace.plan_id = None;
                replace.last_error = None;
                if let Some(find) = app.note_search.finds.get_mut(&tab) {
                    find.query = query;
                    find.index = 0;
                    find.page_offset = 0;
                    find.pending = None;
                    find.pending_revision = None;
                    find.pending_offset = None;
                }
                navigate(app, tab);
            }
        }
        NoteSearchMessage::ReplaceText { tab, text } => {
            if text.len() <= 64 * 1024
                && let Some(replace) = app.note_search.replaces.get_mut(&tab)
            {
                if replace.replacement_input.text() != text {
                    replace.replacement_input =
                        iced::widget::text_editor::Content::with_text(&text);
                }
                replace.replacement = text;
                replace.last_error = None;
            }
        }
        NoteSearchMessage::SetMode { tab, mode } => {
            if matches!(mode.as_str(), "literal" | "regex")
                && let Some(find) = app.note_search.finds.get_mut(&tab)
            {
                find.mode = mode;
                find.index = 0;
                find.page_offset = 0;
                find.pending = None;
                find.pending_revision = None;
                find.pending_offset = None;
                find.last_error = None;
                if let Some(replace) = app.note_search.replaces.get_mut(&tab) {
                    replace.plan_id = None;
                    replace.match_count = None;
                    replace.last_error = None;
                }
                request_find(app, tab);
            }
        }
        NoteSearchMessage::ToggleCaseSensitive(tab) => {
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.case_sensitive = !find.case_sensitive;
                find.index = 0;
                find.page_offset = 0;
                find.pending = None;
                find.pending_revision = None;
                find.pending_offset = None;
                find.last_error = None;
                request_find(app, tab);
            }
        }
        NoteSearchMessage::ToggleWholeWord(tab) => {
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.whole_word = !find.whole_word;
                find.index = 0;
                find.page_offset = 0;
                find.pending = None;
                find.pending_revision = None;
                find.pending_offset = None;
                find.last_error = None;
                request_find(app, tab);
            }
        }
        NoteSearchMessage::ToggleSelectionScope(tab) => {
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.use_selection_scope = !find.use_selection_scope;
                find.pending = None;
                find.pending_revision = None;
                find.pending_offset = None;
                find.index = 0;
                find.page_offset = 0;
                find.last_error = None;
                request_find(app, tab);
            }
        }
        NoteSearchMessage::ReplaceNext(tab) => prepare(app, tab, false),
        NoteSearchMessage::ReplaceAll(tab) => prepare(app, tab, true),
    }
    app.refresh_semantics();
    iced::Task::none()
}

/// The find page for a tab, refreshed against the live view. Read-only
/// callers (views, harness) get the stored page when it still describes
/// the current (revision, query, mode) and otherwise a fresh scan; the
/// stored page itself refreshes through message handling.
#[must_use]
pub fn matches(app: &App, tab: TabId) -> Option<CachedMatches> {
    let find = app.note_search.finds.get(&tab)?;
    let view = app.note_editor_view(tab)?;
    let scope = if find.use_selection_scope {
        let (scope_revision, start, end) = find.selection_scope?;
        if scope_revision != view.revision {
            return None;
        }
        Some((start, end))
    } else {
        None
    };
    if let Some(cached) = app.note_search.matches.get(&tab)
        && cached.revision == view.revision
        && cached.query == find.query
        && cached.case_sensitive == find.case_sensitive
        && cached.mode == find.mode
        && cached.whole_word == find.whole_word
        && cached.scope == scope
        && cached.offset == find.page_offset
    {
        return Some(cached.clone());
    }
    None
}

/// Admit the current find request through the bounded core worker. An old
/// completion is accepted only while query/options/revision still match.
fn request_find(app: &mut App, tab: TabId) {
    let Some(view) = app.note_editor_view(tab) else {
        app.note_search.matches.remove(&tab);
        return;
    };
    if app
        .note_search
        .matches
        .get(&tab)
        .is_some_and(|cached| cached.revision != view.revision)
        && let Some(find) = app.note_search.finds.get_mut(&tab)
    {
        find.page_offset = 0;
        find.index = 0;
        find.pending = None;
        find.pending_revision = None;
        find.pending_offset = None;
    }
    let Some(query) = app
        .note_search
        .finds
        .get(&tab)
        .map(|find| find.query.clone())
    else {
        return;
    };
    let Some((
        mode,
        case_sensitive,
        whole_word,
        pending,
        pending_revision,
        pending_offset,
        page_offset,
        selection_scope,
        use_selection_scope,
    )) = app.note_search.finds.get(&tab).map(|find| {
        (
            find.mode.clone(),
            find.case_sensitive,
            find.whole_word,
            find.pending,
            find.pending_revision,
            find.pending_offset,
            find.page_offset,
            find.selection_scope,
            find.use_selection_scope,
        )
    })
    else {
        return;
    };
    let scope = if use_selection_scope {
        let Some((scope_revision, start, end)) = selection_scope else {
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.last_error =
                    Some("Select text before opening Find to scope the search".into());
            }
            return;
        };
        if scope_revision != view.revision {
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.last_error =
                    Some("The selected source changed; reopen Find to refresh its scope".into());
            }
            return;
        }
        Some((start, end))
    } else {
        None
    };
    if query.is_empty() && mode == "literal" {
        app.note_search.matches.insert(
            tab,
            CachedMatches {
                revision: view.revision,
                query,
                case_sensitive,
                mode,
                whole_word,
                scope,
                spans: Vec::new(),
                offset: 0,
                total_matches: 0,
                next_offset: None,
                truncated: false,
            },
        );
        if let Some(find) = app.note_search.finds.get_mut(&tab) {
            find.pending = None;
            find.pending_revision = None;
            find.pending_offset = None;
            find.last_error = None;
        }
        return;
    }
    if let Some(cached) = app.note_search.matches.get(&tab)
        && cached.revision == view.revision
        && cached.query == query
        && cached.mode == mode
        && cached.case_sensitive == case_sensitive
        && cached.whole_word == whole_word
        && cached.scope == scope
        && cached.offset == page_offset
    {
        return;
    }
    if pending.is_some() {
        if pending_revision == Some(view.revision) && pending_offset == Some(page_offset) {
            return;
        }
        if let Some(find) = app.note_search.finds.get_mut(&tab) {
            find.pending = None;
            find.pending_revision = None;
            find.pending_offset = None;
        }
    }
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    let request_id = api.command_registry().next_request_id();
    let result = api.execute(Invocation::new(
        request_id,
        "text.find",
        serde_json::json!({
            "buffer_id": view.buffer_id,
            "revision": view.revision,
            "query": query,
            "mode": mode,
            "case_sensitive": case_sensitive,
            "whole_word": whole_word,
            "scope": scope,
            "offset": page_offset,
        }),
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => {
            app.operations.accept(request_id, "note.find");
            app.note_search.searches.insert(request_id, tab);
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.pending = Some(request_id);
                find.pending_revision = Some(view.revision);
                find.pending_offset = Some(page_offset);
            }
        }
        ExecutionResult::Local(LocalResult::Success(success)) => {
            install_find_result(
                app,
                tab,
                &FindSnapshot {
                    buffer_id: view.buffer_id,
                    revision: view.revision,
                    query: query.clone(),
                    mode: mode.clone(),
                    case_sensitive,
                    whole_word,
                    scope,
                    offset: page_offset,
                },
                &success.data,
            );
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.last_error = Some(error.message.clone());
            }
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.last_error = Some(rejection_message(
                    rejected.reason == flavite_api::RejectionReason::StaleRevision,
                    rejected.message.as_deref(),
                ));
            }
        }
        _ => {
            if let Some(find) = app.note_search.finds.get_mut(&tab) {
                find.last_error = Some("Find could not be admitted; retry".into());
            }
        }
    }
}

fn install_find_result(
    app: &mut App,
    tab: TabId,
    request: &FindSnapshot,
    data: &serde_json::Value,
) {
    let Some(view) = app.note_editor_view(tab) else {
        return;
    };
    let Some(find) = app.note_search.finds.get(&tab) else {
        return;
    };
    let current_scope = if find.use_selection_scope {
        find.selection_scope.map(|(_, start, end)| (start, end))
    } else {
        None
    };
    if view.buffer_id != request.buffer_id
        || view.revision != request.revision
        || find.query != request.query
        || find.mode != request.mode
        || find.case_sensitive != request.case_sensitive
        || find.whole_word != request.whole_word
        || find.page_offset != request.offset
        || current_scope != request.scope
        || data.get("buffer_id").and_then(serde_json::Value::as_u64) != Some(request.buffer_id)
        || data.get("revision").and_then(serde_json::Value::as_u64) != Some(request.revision)
    {
        return;
    }
    let Some(raw_spans) = data.get("spans").and_then(serde_json::Value::as_array) else {
        return;
    };
    let mut spans = Vec::with_capacity(raw_spans.len());
    for raw in raw_spans {
        let Some(pair) = raw.as_array() else {
            return;
        };
        if pair.len() != 2 {
            return;
        }
        let (Some(start), Some(end)) = (pair[0].as_u64(), pair[1].as_u64()) else {
            return;
        };
        let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
            return;
        };
        if start > end
            || end > view.text.len()
            || !view.text.is_char_boundary(start)
            || !view.text.is_char_boundary(end)
            || spans
                .last()
                .is_some_and(|(_, previous_end)| *previous_end > start)
        {
            return;
        }
        spans.push((start, end));
    }
    let Some(total_matches) = data
        .get("total_matches")
        .and_then(serde_json::Value::as_u64)
    else {
        return;
    };
    let Some(offset) = data.get("offset").and_then(serde_json::Value::as_u64) else {
        return;
    };
    let next_offset = match data.get("next_offset") {
        Some(serde_json::Value::Null) | None => None,
        Some(value) => value.as_u64(),
    };
    let Some(next_offset_value) = next_offset else {
        if !data
            .get("next_offset")
            .is_some_and(serde_json::Value::is_null)
        {
            return;
        }
        if offset != request.offset || offset > total_matches {
            return;
        }
        let end = offset.saturating_add(u64::try_from(spans.len()).unwrap_or(u64::MAX));
        let expected_next = (end < total_matches).then_some(end);
        if expected_next.is_some()
            || data.get("truncated").and_then(serde_json::Value::as_bool) != Some(false)
        {
            return;
        }
        app.note_search.matches.insert(
            tab,
            CachedMatches {
                revision: request.revision,
                query: request.query.clone(),
                case_sensitive: request.case_sensitive,
                mode: request.mode.clone(),
                whole_word: request.whole_word,
                scope: request.scope,
                spans,
                offset,
                total_matches,
                next_offset: None,
                truncated: false,
            },
        );
        if let Some(find) = app.note_search.finds.get_mut(&tab) {
            find.last_error = None;
        }
        return;
    };
    let end = offset.saturating_add(u64::try_from(spans.len()).unwrap_or(u64::MAX));
    let truncated = true;
    if offset != request.offset
        || end > total_matches
        || next_offset_value != end
        || data.get("truncated").and_then(serde_json::Value::as_bool) != Some(truncated)
    {
        return;
    }
    app.note_search.matches.insert(
        tab,
        CachedMatches {
            revision: request.revision,
            query: request.query.clone(),
            case_sensitive: request.case_sensitive,
            mode: request.mode.clone(),
            whole_word: request.whole_word,
            scope: request.scope,
            spans,
            offset,
            total_matches,
            next_offset: Some(next_offset_value),
            truncated,
        },
    );
    if let Some(find) = app.note_search.finds.get_mut(&tab) {
        find.last_error = None;
    }
}

/// Navigation selects a display range through the workspace (never an
/// editor write) and records a content anchor for read-mode scrolling.
fn advance_find(app: &mut App, tab: TabId, forward: bool) {
    let Some(page) = matches(app, tab) else {
        return;
    };
    if page.spans.is_empty() {
        return;
    }
    let Some(find) = app.note_search.finds.get_mut(&tab) else {
        return;
    };
    if forward && find.index + 1 < page.spans.len() {
        find.index += 1;
        navigate(app, tab);
        return;
    }
    if !forward && find.index > 0 {
        find.index -= 1;
        navigate(app, tab);
        return;
    }

    if forward {
        if let Some(next) = page.next_offset {
            find.page_offset = next;
            find.index = 0;
        } else {
            find.page_offset = 0;
            find.index = 0;
        }
    } else if page.offset > 0 {
        find.page_offset = page.offset.saturating_sub(FIND_PAGE_SIZE);
        find.index = usize::try_from(FIND_PAGE_SIZE.saturating_sub(1)).unwrap_or(usize::MAX);
    } else {
        let last_ordinal = page.total_matches.saturating_sub(1);
        find.page_offset = last_ordinal / FIND_PAGE_SIZE * FIND_PAGE_SIZE;
        find.index =
            usize::try_from(last_ordinal.saturating_sub(find.page_offset)).unwrap_or(usize::MAX);
    }
    find.pending = None;
    find.pending_revision = None;
    find.pending_offset = None;
    request_find(app, tab);
    navigate(app, tab);
}

fn navigate(app: &mut App, tab: TabId) {
    request_find(app, tab);
    let Some(view) = app.note_editor_view(tab) else {
        return;
    };
    let Some(find) = app.note_search.finds.get(&tab) else {
        return;
    };
    let Some(cached) = matches(app, tab) else {
        return;
    };
    if cached.spans.is_empty() {
        return;
    }
    let Some(&(at, end)) = cached.spans.get(find.index) else {
        return;
    };
    let _ = app
        .workspace
        .set_display_selection(tab, at, end, view.revision, &view.text);
    app.reading.pending_anchor.insert(tab, at);
}

/// Prepare one replacement plan: Replace next scopes the plan to the
/// match at/after the caret; Replace all prepares over the whole note.
/// The plan applies on completion (see [`completion`]).
fn prepare(app: &mut App, tab: TabId, all: bool) {
    let Some(state) = app.note_search.replaces.get(&tab) else {
        return;
    };
    if state.pending.is_some() {
        return; // one in-flight replacement per view
    }
    let Some(view) = app.note_editor_view(tab) else {
        return;
    };
    let Some(query) = app
        .note_search
        .finds
        .get(&tab)
        .map(|find| find.query.clone())
    else {
        return;
    };
    let replacement = state.replacement.clone();
    request_find(app, tab);
    let options = app.note_search.finds.get(&tab).cloned().unwrap_or_default();
    if query.is_empty() && options.mode == "literal" {
        app.status = "Type text to find before replacing".into();
        return;
    }
    let mut scope = options
        .use_selection_scope
        .then(|| options.selection_scope.map(|(_, start, end)| (start, end)))
        .flatten();
    if options.use_selection_scope
        && options
            .selection_scope
            .is_none_or(|(scope_revision, _, _)| scope_revision != view.revision)
    {
        app.status = "The selected source changed; reopen Find to refresh its scope".into();
        return;
    }
    if !all {
        let caret = app
            .workspace
            .tab(&tab)
            .and_then(|session| session.interaction().selections.first().copied())
            .map_or(0, |selection| selection.anchor.min(selection.head));
        let cached = matches(app, tab);
        let next = cached
            .as_ref()
            .and_then(|cached| {
                cached
                    .spans
                    .iter()
                    .copied()
                    .find(|(start, _)| *start >= caret)
            })
            .or_else(|| {
                cached
                    .as_ref()
                    .and_then(|cached| cached.spans.first().copied())
            });
        let Some(span) = next else {
            app.status = if options.pending.is_some() {
                "Waiting for current find results…".into()
            } else {
                format!("No matches for \"{query}\"")
            };
            return;
        };
        scope = Some(span);
    }
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    let request_id = api.command_registry().next_request_id();
    let result = api.execute(Invocation::new(
        request_id,
        "text.prepare_replace",
        serde_json::json!({
            "buffer_id": view.buffer_id,
            "revision": view.revision,
            "query": query,
            "replacement": replacement,
            "mode": options.mode,
            "case_sensitive": options.case_sensitive,
            "whole_word": options.whole_word,
            "scope": scope,
        }),
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => {
            app.operations.accept(request_id, "note.replace");
            app.note_search.preparations.insert(request_id, tab);
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.pending = Some(request_id);
                state.query = query;
                state.last_error = None;
            }
            app.status = "Preparing replacement…".into();
        }
        ExecutionResult::Local(LocalResult::Success(success)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.query = query;
            }
            apply_plan(app, tab, &success.data);
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(rejection_message(
                    rejected.reason == flavite_api::RejectionReason::StaleRevision,
                    rejected.message.as_deref(),
                ));
            }
            app.status = "Replacement was refused".into();
        }
        ExecutionResult::ActionRequired { code, .. } => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(plan_error(&code));
            }
            app.status = format!("Replacement needs attention: {code}");
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(error.message.clone());
            }
            app.status = "Replacement failed".into();
        }
        ExecutionResult::Superseded { .. } | ExecutionResult::Local(LocalResult::Void) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some("The replacement was not prepared; search again".into());
            }
        }
    }
}

/// Install one prepared plan through the core: revision-guarded, one
/// isolated Undo unit. Failures record the user-visible repair without
/// any effects.
fn apply_plan(app: &mut App, tab: TabId, prepared: &serde_json::Value) {
    let Some(plan_id) = prepared
        .get("plan_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
    else {
        if let Some(state) = app.note_search.replaces.get_mut(&tab) {
            state.last_error = Some("The replacement plan was malformed; search again".into());
        }
        return;
    };
    let match_count = prepared
        .get("match_count")
        .and_then(serde_json::Value::as_u64);
    let Some(view) = app.note_editor_view(tab) else {
        // The tab lost its buffer while the plan was in flight.
        if let Some(state) = app.note_search.replaces.get_mut(&tab) {
            state.last_error = Some(STALE_MESSAGE.to_owned());
        }
        return;
    };
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    let result = api.execute(Invocation::new(
        api.command_registry().next_request_id(),
        "text.apply_replace",
        serde_json::json!({
            "buffer_id": view.buffer_id,
            "revision": view.revision,
            "plan_id": plan_id,
        }),
        Origin::Gui,
    ));
    let replaced = match_count.unwrap_or(0);
    match result {
        ExecutionResult::Local(LocalResult::Success(_)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.plan_id = Some(plan_id);
                state.match_count = match_count;
                state.last_error = None;
            }
            app.workspace.invalidate_writing_target(view.buffer_id);
            app.linking.invalidate_if_source_changed(None);
            app.contexts.invalidate_all();
            app.reading.notes.remove(&tab);
            app.status = format!("Replaced {replaced} · unsaved · Undo restores it");
            app.reconcile_contexts();
            crate::host::reading::sync(app);
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(rejection_message(
                    rejected.reason == flavite_api::RejectionReason::StaleRevision,
                    rejected.message.as_deref(),
                ));
            }
            app.status = "Replacement was refused".into();
        }
        ExecutionResult::ActionRequired { code, .. } => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(plan_error(&code));
            }
            app.status = format!("Replacement needs attention: {code}");
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(error.message.clone());
            }
            app.status = "Replacement failed".into();
        }
        ExecutionResult::Superseded { .. } | ExecutionResult::Local(LocalResult::Void) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some("The replacement was not applied; search again".into());
            }
        }
        ExecutionResult::Pending { .. } => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some("The replacement is still applying".into());
            }
        }
    }
}

/// Route one drained completion for a replacement preparation. Returns
/// true when the completion belonged to this host: the plan applies to
/// the still-mounted view, or is dropped without effects when the row
/// closed or the tab disappeared while the worker ran.
pub fn completion(app: &mut App, completion: &OperationCompletion) -> bool {
    if let Some(tab) = app.note_search.searches.remove(&completion.request_id) {
        if app.workspace.tab(&tab).is_none() {
            return true;
        }
        let Some(find) = app.note_search.finds.get_mut(&tab) else {
            return true;
        };
        if find.pending != Some(completion.request_id) {
            return true;
        }
        let pending_offset = find.pending_offset;
        find.pending = None;
        find.pending_revision = None;
        find.pending_offset = None;
        match &completion.result {
            ExecutionResult::Local(LocalResult::Success(success)) => {
                let Some(view) = app.note_editor_view(tab) else {
                    return true;
                };
                let Some(find) = app.note_search.finds.get(&tab).cloned() else {
                    return true;
                };
                install_find_result(
                    app,
                    tab,
                    &FindSnapshot {
                        buffer_id: view.buffer_id,
                        revision: view.revision,
                        query: find.query,
                        mode: find.mode,
                        case_sensitive: find.case_sensitive,
                        whole_word: find.whole_word,
                        scope: find
                            .use_selection_scope
                            .then(|| find.selection_scope.map(|(_, start, end)| (start, end)))
                            .flatten(),
                        offset: pending_offset.unwrap_or(find.page_offset),
                    },
                    &success.data,
                );
                navigate(app, tab);
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                if let Some(find) = app.note_search.finds.get_mut(&tab) {
                    find.last_error = Some(error.message.clone());
                }
            }
            ExecutionResult::Rejected(rejected) => {
                if let Some(find) = app.note_search.finds.get_mut(&tab) {
                    find.last_error = Some(rejection_message(
                        rejected.reason == flavite_api::RejectionReason::StaleRevision,
                        rejected.message.as_deref(),
                    ));
                }
            }
            _ => {
                if let Some(find) = app.note_search.finds.get_mut(&tab) {
                    find.last_error = Some("Find result is stale; search again".into());
                }
            }
        }
        return true;
    }
    let Some(&tab) = app.note_search.preparations.get(&completion.request_id) else {
        return false;
    };
    app.note_search.preparations.remove(&completion.request_id);
    if app.workspace.tab(&tab).is_none() {
        // The tab closed while the worker ran: drop the stale completion
        // without applying anything to the closed buffer.
        return true;
    }
    if app
        .note_search
        .replaces
        .get(&tab)
        .is_none_or(|state| state.pending != Some(completion.request_id))
    {
        return true;
    }
    if let Some(state) = app.note_search.replaces.get_mut(&tab) {
        state.pending = None;
    }
    match &completion.result {
        ExecutionResult::Local(LocalResult::Success(success)) => {
            apply_plan(app, tab, &success.data)
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(rejection_message(
                    rejected.reason == flavite_api::RejectionReason::StaleRevision,
                    rejected.message.as_deref(),
                ));
            }
            app.status = "Replacement was refused".into();
        }
        ExecutionResult::ActionRequired { code, .. } => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(plan_error(code));
            }
            app.status = format!("Replacement needs attention: {code}");
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some(error.message.clone());
            }
            app.status = "Replacement failed".into();
        }
        _ => {
            if let Some(state) = app.note_search.replaces.get_mut(&tab) {
                state.last_error = Some("The replacement was not prepared; search again".into());
            }
        }
    }
    true
}

/// Retain host state only for live tabs; a closed tab drops its find bar,
/// replace row, cached page, and any in-flight preparation mapping.
pub fn sync(app: &mut App) {
    app.note_search
        .finds
        .retain(|tab, _| app.workspace.tab(tab).is_some());
    app.note_search
        .replaces
        .retain(|tab, _| app.workspace.tab(tab).is_some());
    app.note_search
        .matches
        .retain(|tab, _| app.workspace.tab(tab).is_some());
    app.note_search
        .preparations
        .retain(|_, tab| app.workspace.tab(tab).is_some());
    app.note_search
        .searches
        .retain(|_, tab| app.workspace.tab(tab).is_some());
    let tabs: Vec<TabId> = app.note_search.finds.keys().copied().collect();
    for tab in tabs {
        request_find(app, tab);
    }
}

/// Whether a find or replace input owns typing: the editor keyboard is
/// disabled while either is focused.
#[must_use]
pub fn blocks_typing(app: &App, tab: &TabId) -> bool {
    app.note_search
        .finds
        .get(tab)
        .is_some_and(|find| find.focused)
        || app
            .note_search
            .replaces
            .get(tab)
            .is_some_and(|replace| replace.focused)
}

/// The mounted find input id for a tab.
#[must_use]
pub fn find_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("find-current-{}", tab.raw()))
}

/// The mounted replacement input id for a tab.
#[must_use]
pub fn replace_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("replace-current-{}", tab.raw()))
}

fn rejection_message(stale: bool, message: Option<&str>) -> String {
    if stale {
        STALE_MESSAGE.to_owned()
    } else {
        format!("Replacement refused: {}", message.unwrap_or("no details"))
    }
}

fn plan_error(code: &str) -> String {
    if code == "text-plan-expired" {
        format!("{STALE_MESSAGE} (the prepared plan expired)")
    } else {
        format!("Replacement needs attention ({code})")
    }
}

/// Find and Replace share native multiline input semantics.
pub fn draft_binding(
    tab: TabId,
    replacement: bool,
    key: iced::widget::text_editor::KeyPress,
) -> Option<iced::widget::text_editor::Binding<crate::app::Message>> {
    use iced::keyboard::{Key, key::Named};
    use iced::widget::text_editor::Binding;
    if !matches!(
        key.status,
        iced::widget::text_editor::Status::Focused { .. }
    ) {
        return None;
    }
    if key.key == Key::Named(Named::Enter) {
        if key.modifiers.alt() {
            return Some(Binding::Enter);
        }
        return Some(Binding::Custom(crate::app::Message::NoteSearch(
            if replacement {
                NoteSearchMessage::ReplaceNext(tab)
            } else {
                NoteSearchMessage::FindNext {
                    tab,
                    forward: !key.modifiers.shift(),
                }
            },
        )));
    }
    if key.key == Key::Named(Named::Escape) {
        return Some(Binding::Custom(crate::app::Message::NoteSearch(
            NoteSearchMessage::FindClose(tab),
        )));
    }
    Binding::from_key_press(key)
}
