//! Match-driven selection host (FEUX-0002 E-10/E-11, X06): `editor.select_
//! matches` / `editor.split_selection` with a preview overlay.
//!
//! One host owns the per-view query row, the draft collections (through the
//! existing `text.find` engine), and the overlay set that paints as an
//! outline channel without touching the committed set. Drafts are correlated
//! by an exact [`PreviewKey`]: a stale or out-of-order completion is
//! rejected by key equality, and a committed change, mode exit or query
//! edit invalidates the overlay. Apply revalidates the key, runs the
//! selection-only apply through the api, and installs the returned canonical
//! set (no ticket, no history node, `selection_version` bump).

use crate::app::{App, Message};
use crate::host::context_mode::EditorTarget;
use crate::host::editor_diagnostics::{EventKind, ModeEnum as DiagMode, Outcome};
use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};
use flavite_workspace::TabId;
use std::collections::BTreeMap;
use std::sync::Arc;

/// The maximum query length (the engine's pattern bound, NFR-8).
const MAX_QUERY_BYTES: usize = 8192;

/// Which machine the row drives: direct match selection or gap selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyKind {
    Select,
    Split,
}

impl ApplyKind {
    /// The canonical api command id (one executor per verb, §6).
    const fn command_id(self) -> &'static str {
        match self {
            Self::Select => "editor.select_matches",
            Self::Split => "editor.split_selection",
        }
    }
}

/// The exact identity a draft is collected under (E-11 §5.7). A completion
/// is accepted only when every component still matches; any committed
/// change, mode generation bump, query edit, or selection-version bump
/// invalidates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewKey {
    pub view_token: u128,
    pub mode_generation: u64,
    pub buffer_id: u64,
    pub revision: u64,
    pub selection_version: u64,
    pub query_version: u64,
}

impl PreviewKey {
    /// The current key for `tab`, if the view is live.
    fn current(app: &App, tab: TabId) -> Option<Self> {
        let view = app.note_editor_view(tab)?;
        let interaction = app.workspace.tab(&tab)?.interaction();
        Some(Self {
            view_token: tab.raw(),
            mode_generation: app.context_mode.generation(),
            buffer_id: view.buffer_id,
            revision: view.revision,
            selection_version: interaction.selection_version,
            query_version: app
                .selection_search
                .tabs
                .get(&tab)
                .map_or(0, |state| state.query_version),
        })
    }
}

/// The accepted overlay candidate: canonical spans plus the exact totals
/// from the collection.
#[derive(Debug, Clone)]
pub struct Overlay {
    pub key: PreviewKey,
    pub spans: Arc<[(usize, usize)]>,
    pub total_matches: u64,
    pub zero_width_count: u64,
}

/// Per-view match-selection row state.
#[derive(Debug)]
pub struct SelectionSearchState {
    pub kind: ApplyKind,
    pub query_input: iced::widget::text_editor::Content,
    pub query: String,
    pub regex: bool,
    pub case_sensitive: bool,
    /// The in-flight draft request, if any.
    pub pending: Option<u64>,
    /// Bumped on every query/option edit; completions carry it.
    pub query_version: u64,
    pub overlay: Option<Overlay>,
    pub error: Option<String>,
    /// The mounted query input's rectangle (the pointer-focus target for
    /// real OS input; the synthetic focus operation does not stick for this
    /// input under the runtime).
    pub input_bounds: Option<iced::Rectangle>,
    /// The row is the keyboard owner while open (mirrors the find bar's
    /// `focused` deferral: Context Mode must not interpret printable keys).
    pub focused: bool,
}

impl SelectionSearchState {
    fn new(kind: ApplyKind) -> Self {
        Self {
            kind,
            query_input: iced::widget::text_editor::Content::new(),
            query: String::new(),
            regex: false,
            case_sensitive: false,
            pending: None,
            query_version: 0,
            overlay: None,
            error: None,
            input_bounds: None,
            focused: true,
        }
    }
}

impl Default for SelectionSearchState {
    fn default() -> Self {
        Self::new(ApplyKind::Select)
    }
}

/// Host state: one row per tab plus draft request correlation.
#[derive(Debug, Default)]
pub struct SelectionSearchHost {
    pub tabs: BTreeMap<TabId, SelectionSearchState>,
    /// A row opened while an overlay is tearing down may miss the focus
    /// operation's first pass; focus again when the row's mounted bounds
    /// arrive (the `Bounds` message is the mount signal).
    pub pending_focus: Option<TabId>,
    requests: BTreeMap<u64, (TabId, PreviewKey)>,
}

impl SelectionSearchHost {
    pub(crate) fn detach(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
        self.requests.retain(|_, (owner, _)| *owner != tab);
    }
}

/// Messages owned by the match-selection host.
#[derive(Debug, Clone)]
pub enum SelectionSearchMessage {
    Open {
        tab: TabId,
        kind: ApplyKind,
    },
    Close(TabId),
    DraftAction {
        tab: TabId,
        action: iced::widget::text_editor::Action,
    },
    ToggleRegex(TabId),
    ToggleCase(TabId),
    Apply(TabId),
    /// The mounted row rectangle; also the mount signal for the deferred
    /// focus of the query input.
    Bounds {
        tab: TabId,
        rect: iced::Rectangle,
    },
    /// The mounted query input's rectangle (pointer-focus target).
    InputBounds {
        tab: TabId,
        rect: iced::Rectangle,
    },
}

/// Record one diagnostics outcome for the match-collection path (X07).
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

/// Handle one match-selection message.
pub fn handle(app: &mut App, message: SelectionSearchMessage) -> iced::Task<Message> {
    match message {
        SelectionSearchMessage::Open { tab, kind } => {
            // A fresh row always starts from the live target; the focus
            // moves to the query input.
            {
                let state = app.selection_search.tabs.entry(tab).or_default();
                if state.kind != kind {
                    *state = SelectionSearchState::new(kind);
                }
                state.error = None;
            }
            request_collection(app, tab);
            app.refresh_semantics();
            // Focus the input immediately AND when its mounted bounds land
            // (opening through a closing overlay can miss the first pass).
            app.selection_search.pending_focus = Some(tab);
            focus_query_editor(app, tab)
        }
        SelectionSearchMessage::Close(tab) => {
            diag(app, tab, EventKind::MatchCollect, Outcome::Cancelled, None);
            cancel_tab(app, tab);
            app.refresh_semantics();
            // The row's query input owned the runtime focus; once the row is
            // gone that focus is stale and every following keystroke would
            // vanish with it. Restore the editor exactly like FindClose.
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
            iced::Task::none()
        }
        SelectionSearchMessage::DraftAction { tab, action } => {
            let mut changed = false;
            {
                let Some(state) = app.selection_search.tabs.get_mut(&tab) else {
                    return iced::Task::none();
                };
                let before = state.query_input.text();
                if let iced::widget::text_editor::Action::Edit(
                    iced::widget::text_editor::Edit::Paste(value),
                ) = &action
                    && value.len() > MAX_QUERY_BYTES
                {
                    return iced::Task::none();
                }
                state.query_input.perform(action);
                let text = state.query_input.text();
                if text.len() > MAX_QUERY_BYTES {
                    state.query_input = iced::widget::text_editor::Content::with_text(&before);
                    return iced::Task::none();
                }
                if state.query != text {
                    state.query = text;
                    state.query_version = state.query_version.wrapping_add(1);
                    state.overlay = None;
                    state.error = None;
                    changed = true;
                }
            }
            if changed {
                request_collection(app, tab);
            }
            app.refresh_semantics();
            iced::Task::none()
        }
        SelectionSearchMessage::ToggleRegex(tab) => {
            let mut changed = false;
            if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
                state.regex = !state.regex;
                state.query_version = state.query_version.wrapping_add(1);
                state.overlay = None;
                state.error = None;
                changed = true;
            }
            if changed {
                request_collection(app, tab);
            }
            app.refresh_semantics();
            iced::Task::none()
        }
        SelectionSearchMessage::ToggleCase(tab) => {
            let mut changed = false;
            if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
                state.case_sensitive = !state.case_sensitive;
                state.query_version = state.query_version.wrapping_add(1);
                state.overlay = None;
                state.error = None;
                changed = true;
            }
            if changed {
                request_collection(app, tab);
            }
            app.refresh_semantics();
            iced::Task::none()
        }
        SelectionSearchMessage::Apply(tab) => apply(app, tab),
        SelectionSearchMessage::InputBounds { tab, rect } => {
            if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
                state.input_bounds = Some(rect);
            }
            iced::Task::none()
        }
        SelectionSearchMessage::Bounds { tab, rect: _ } => {
            if app.selection_search.pending_focus == Some(tab) {
                app.selection_search.pending_focus = None;
                app.selection_search
                    .tabs
                    .get(&tab)
                    .map_or(iced::Task::none(), |_| focus_query_editor(app, tab))
            } else {
                iced::Task::none()
            }
        }
    }
}

/// The current overlay spans for the widget preview, only while the key
/// still matches the live view (the outline never paints stale drafts).
#[must_use]
pub fn overlay_spans(app: &App, tab: TabId) -> Option<Arc<[(usize, usize)]>> {
    let state = app.selection_search.tabs.get(&tab)?;
    let overlay = state.overlay.as_ref()?;
    let current = PreviewKey::current(app, tab)?;
    if current != overlay.key {
        return None;
    }
    Some(overlay.spans.clone())
}

/// Whether Apply is enabled: an overlay exists and is still key-fresh.
#[must_use]
pub fn apply_enabled(app: &App, tab: TabId) -> bool {
    let Some(state) = app.selection_search.tabs.get(&tab) else {
        return false;
    };
    let Some(overlay) = state.overlay.as_ref() else {
        return false;
    };
    PreviewKey::current(app, tab).as_ref() == Some(&overlay.key)
}

/// The caption for the toolbar detail line while a row is open.
#[must_use]
pub fn caption(app: &App, tab: TabId) -> Option<String> {
    let state = app.selection_search.tabs.get(&tab)?;
    if let Some(error) = &state.error {
        return Some(error.clone());
    }
    if state.pending.is_some() {
        return Some("Collecting matches…".to_owned());
    }
    let overlay = state.overlay.as_ref()?;
    let total = overlay.total_matches;
    Some(match state.kind {
        ApplyKind::Select => format!("{total} match{}", if total == 1 { "" } else { "es" }),
        ApplyKind::Split => {
            let gaps = overlay.spans.len();
            format!("{gaps} gap{}", if gaps == 1 { "" } else { "s" })
        }
    })
}

/// Cancel one in-flight collection at the source (E-11): the Session stops
/// the worker job and drops its outcome; the UI retires its correlation
/// state. A cancelled request never delivers a completion.
fn cancel_collection(app: &mut App, request_id: u64) {
    if let Some(api) = app.flavite.as_ref() {
        let _ = api.execute(Invocation::new(
            api.command_registry().next_request_id(),
            "editor.cancel_selection",
            serde_json::json!({"request_id": request_id}),
            Origin::Gui,
        ));
    }
    app.selection_search.requests.remove(&request_id);
    app.operations.cancel(request_id);
}

/// Cancel every pending collection owned by `tab` and drop its row state
/// (mode exit / view teardown).
pub(crate) fn cancel_tab(app: &mut App, tab: TabId) {
    let pending = app
        .selection_search
        .tabs
        .get(&tab)
        .and_then(|state| state.pending);
    if let Some(request_id) = pending {
        cancel_collection(app, request_id);
    }
    app.selection_search.tabs.remove(&tab);
    app.selection_search
        .requests
        .retain(|_, (owner, _)| *owner != tab);
}

/// Admit a draft through the bounded core worker. At most one collection is
/// in flight per view (E-11): a new draft cancels the previous request at
/// the source instead of leaving it running behind a stale overlay.
fn request_collection(app: &mut App, tab: TabId) {
    let superseded = app
        .selection_search
        .tabs
        .get(&tab)
        .and_then(|state| state.pending);
    let Some(state) = app.selection_search.tabs.get(&tab) else {
        return;
    };
    if state.query.is_empty() && !state.regex {
        // Idle, not a match-everywhere search (D-05): clear any stale
        // overlay.
        if let Some(request_id) = superseded {
            cancel_collection(app, request_id);
        }
        if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
            state.pending = None;
            state.overlay = None;
            state.error = None;
        }
        return;
    }
    let Some(target) = EditorTarget::capture(app, tab) else {
        if let Some(request_id) = superseded {
            cancel_collection(app, request_id);
        }
        if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
            state.pending = None;
        }
        return;
    };
    let Some(view) = app.note_editor_view(tab) else {
        if let Some(request_id) = superseded {
            cancel_collection(app, request_id);
        }
        if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
            state.pending = None;
        }
        return;
    };
    let Some(key) = PreviewKey::current(app, tab) else {
        if let Some(request_id) = superseded {
            cancel_collection(app, request_id);
        }
        if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
            state.pending = None;
        }
        return;
    };
    // E-11: superseding a draft cancels the old request; the fresh overlay
    // check runs after the cancel so a completed previous collection's
    // overlay (key-fresh) still short-circuits without re-collecting.
    if let Some(request_id) = superseded {
        cancel_collection(app, request_id);
        if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
            state.pending = None;
        }
    }
    if app
        .selection_search
        .tabs
        .get(&tab)
        .is_some_and(|s| s.overlay.as_ref().is_some_and(|overlay| overlay.key == key))
    {
        return;
    }
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    let query = app
        .selection_search
        .tabs
        .get(&tab)
        .map(|s| s.query.clone())
        .unwrap_or_default();
    let (regex, case_sensitive) = app
        .selection_search
        .tabs
        .get(&tab)
        .map(|s| (s.regex, s.case_sensitive))
        .unwrap_or((false, false));
    let kind = app
        .selection_search
        .tabs
        .get(&tab)
        .map(|s| s.kind)
        .unwrap_or(ApplyKind::Select);
    let request_id = api.command_registry().next_request_id();
    let result = api.execute(Invocation::new(
        request_id,
        kind.command_id(),
        serde_json::json!({
            "buffer_id": view.buffer_id,
            "revision": view.revision,
            "selections": target.selections_json(),
            "query": {
                "text": query,
                "regex": regex,
                "case_sensitive": case_sensitive,
            },
        }),
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => {
            app.operations.accept(request_id, "selection.find");
            app.selection_search.requests.insert(request_id, (tab, key));
            if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
                state.pending = Some(request_id);
                state.error = None;
            }
            diag(app, tab, EventKind::MatchCollect, Outcome::Started, None);
        }
        ExecutionResult::Local(LocalResult::Success(success)) => {
            install_collection(app, tab, &key, &success.data);
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
                state.pending = None;
                state.overlay = None;
                state.error = Some(error.message.clone());
            }
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused, None);
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
                state.pending = None;
                state.overlay = None;
                state.error = Some(
                    rejected
                        .message
                        .clone()
                        .unwrap_or_else(|| "Match collection was refused".into()),
                );
            }
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused, None);
        }
        _ => {
            if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
                state.pending = None;
                state.overlay = None;
                state.error = Some("Match collection could not be admitted; retry".into());
            }
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused, None);
        }
    }
}

/// Install a completed collection when the key still matches exactly.
fn install_collection(app: &mut App, tab: TabId, key: &PreviewKey, data: &serde_json::Value) {
    let current = PreviewKey::current(app, tab);
    let Some(view) = app.note_editor_view(tab) else {
        return;
    };
    let Some(raw_spans) = data.get("spans").and_then(serde_json::Value::as_array) else {
        return;
    };
    if current.as_ref() != Some(key) {
        // Stale or out-of-order: reject by exact key match (E-11).
        if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
            state.pending = None;
            state.overlay = None;
        }
        return;
    }
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
        {
            return;
        }
        spans.push((start, end));
    }
    let total_matches = data
        .get("total_matches")
        .and_then(serde_json::Value::as_u64);
    let zero_width_count = data
        .get("zero_width_count")
        .and_then(serde_json::Value::as_u64);
    let (Some(total_matches), Some(zero_width_count)) = (total_matches, zero_width_count) else {
        return;
    };
    if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
        state.pending = None;
        state.error = None;
        state.overlay = Some(Overlay {
            key: key.clone(),
            spans: Arc::from(spans.into_boxed_slice()),
            total_matches,
            zero_width_count,
        });
    }
    diag(
        app,
        tab,
        EventKind::MatchPreview,
        Outcome::Ready,
        u32::try_from(total_matches).ok(),
    );
}

/// Apply the overlay: revalidate the key, then run the selection-only apply
/// through the api (the same install path as every selection-only verb).
fn apply(app: &mut App, tab: TabId) -> iced::Task<Message> {
    let Some(target) = EditorTarget::capture(app, tab) else {
        return iced::Task::none();
    };
    let Some(state) = app.selection_search.tabs.get(&tab) else {
        return iced::Task::none();
    };
    let Some(overlay) = state.overlay.clone() else {
        app.context_mode.notice("Collect matches before applying");
        return iced::Task::none();
    };
    let current = PreviewKey::current(app, tab);
    if current.as_ref() != Some(&overlay.key) {
        // The preview is stale: re-collect instead of applying a stale set.
        request_collection(app, tab);
        app.context_mode
            .notice("Source changed — collecting matches again");
        return iced::Task::none();
    }
    let query = state.query.clone();
    let regex = state.regex;
    let case_sensitive = state.case_sensitive;
    let kind = state.kind;
    let spans: Vec<(usize, usize)> = overlay.spans.to_vec();
    // The exact observed totals ride the apply (E-10/R13): the span list is
    // capped, so the totals are the authority for oversize/zero-width
    // refusals.
    let total_matches = overlay.total_matches;
    let zero_width_count = overlay.zero_width_count;
    let installed = crate::host::context_mode::selections_only(
        app,
        &target,
        kind.command_id(),
        serde_json::json!({
            "query": {
                "text": query,
                "regex": regex,
                "case_sensitive": case_sensitive,
            },
            "apply": true,
            "spans": spans,
            "total_matches": total_matches,
            "zero_width_count": zero_width_count,
        }),
    );
    if installed.is_some() {
        // The committed set replaced the overlay; the row stays open for a
        // refined query.
        if let Some(state) = app.selection_search.tabs.get_mut(&tab) {
            state.overlay = None;
            state.error = None;
        }
        app.refresh_semantics();
        diag(
            app,
            tab,
            EventKind::MatchCommit,
            Outcome::Applied,
            u32::try_from(spans.len()).ok(),
        );
    } else {
        diag(
            app,
            tab,
            EventKind::MatchRefused,
            Outcome::Refused,
            u32::try_from(spans.len()).ok(),
        );
    }
    iced::Task::none()
}

/// Route one completion; returns whether it belonged to selection search.
pub fn completion(app: &mut App, completion: &flavite_api::OperationCompletion) -> bool {
    let Some((tab, key)) = app.selection_search.requests.remove(&completion.request_id) else {
        return false;
    };
    if app.workspace.tab(&tab).is_none() {
        return true;
    }
    let Some(state) = app.selection_search.tabs.get_mut(&tab) else {
        return true;
    };
    if state.pending != Some(completion.request_id) {
        return true;
    }
    state.pending = None;
    match &completion.result {
        ExecutionResult::Local(LocalResult::Success(success)) => {
            install_collection(app, tab, &key, &success.data);
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            state.overlay = None;
            state.error = Some(error.message.clone());
            diag_completion_error(app, tab, &error.message);
        }
        ExecutionResult::Rejected(rejected) => {
            state.overlay = None;
            state.error = Some(
                rejected
                    .message
                    .clone()
                    .unwrap_or_else(|| "Match collection was refused".into()),
            );
            let message = state.error.clone().unwrap_or_default();
            diag_completion_error(app, tab, &message);
        }
        _ => {
            state.overlay = None;
            state.error = Some("Match collection is stale; search again".into());
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused, None);
        }
    }
    true
}

/// Classify a collection failure: regex-worker unavailability is its own
/// typed outcome (actionable, literal paths unaffected), everything else is
/// a refusal.
fn diag_completion_error(app: &mut App, tab: TabId, message: &str) {
    let kind = if message.contains("regex helper") || message.contains("regex is unavailable") {
        EventKind::WorkerUnavailable
    } else {
        EventKind::MatchRefused
    };
    diag(app, tab, kind, Outcome::Refused, None);
}

/// Whether the row owns keyboard input (Context Mode defers printable keys
/// while it is open, exactly like the find bar).
#[must_use]
pub fn blocks_typing(app: &App, tab: &TabId) -> bool {
    app.selection_search
        .tabs
        .get(tab)
        .is_some_and(|state| state.focused)
}

/// Focus the query input via the low-level focusable operation (the same
/// path the find bar uses to restore/clear focus).
fn focus_query_editor(_app: &App, tab: TabId) -> iced::Task<Message> {
    // The same runtime-level focus the find bar uses: the low-level
    // focusable operation does not stick for the query input under the
    // real runtime (the row then never receives a keystroke).
    iced::widget::operation::focus(query_id(tab))
}

/// The query input widget id (focus target).
#[must_use]
pub fn query_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("selection-search-{:032x}", tab.raw()))
}

/// Key-binding parity with the find bar: Enter applies, Escape closes,
/// and every other key falls through to the text editor's own bindings —
/// a custom binding that returns `None` makes iced DROP the key entirely,
/// which silently disabled all query entry into a focused input (R14).
pub fn draft_binding(
    tab: TabId,
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
        return Some(Binding::Custom(crate::app::Message::SelectionSearch(
            SelectionSearchMessage::Apply(tab),
        )));
    }
    if key.key == Key::Named(Named::Escape) {
        return Some(Binding::Custom(crate::app::Message::SelectionSearch(
            SelectionSearchMessage::Close(tab),
        )));
    }
    Binding::from_key_press(key)
}
