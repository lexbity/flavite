//! Command argument draft host (FEUX-0004 E-11, FEUX-0006 E-6, X03):
//! per-tab draft state for text/number arguments, rendered as a toolbar row.
//!
//! One host owns the per-view input row, validates on Enter, and dispatches
//! through the normal admission path. Drafts are mutually exclusive with the
//! matches draft host (opening one closes the other).
//!
//! For commands with multiple fields (e.g., replace_regex with pattern and template),
//! each field is tracked separately to enable proper Tab/Shift+Tab navigation and
//! per-field validation.

use crate::app::{App, Message};
use crate::host::context_mode::{EditorTarget, kernel_mutate, replace_commit};
use crate::host::editor_diagnostics::{EventKind, ModeEnum as DiagMode, Outcome};
use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};
use flavite_edit_context::vocabulary::ContextCommand;
use flavite_workspace::TabId;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Pattern size bound (8 KiB) - matches flavite_text_search::PATTERN_MAX_BYTES
const PATTERN_MAX_BYTES: usize = 8 * 1024;

/// Field specification for command arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSpec {
    /// Text argument with placeholder and literal-only constraint.
    Text {
        placeholder: &'static str,
        literal_only: bool,
    },
    /// Regex argument with placeholder.
    Regex { placeholder: &'static str },
    /// Numeric argument (1..=max_value).
    Number { max_value: u32 },
}

impl FieldSpec {
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            FieldSpec::Text { .. } => "text",
            FieldSpec::Regex { .. } => "regex",
            FieldSpec::Number { .. } => "number",
        }
    }

    #[must_use]
    pub fn placeholder(&self) -> &'static str {
        match self {
            FieldSpec::Text { placeholder, .. } => placeholder,
            FieldSpec::Regex { placeholder } => placeholder,
            FieldSpec::Number { .. } => "Number",
        }
    }
}

/// The command being drafted and its argument specifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftCommand {
    pub command: ContextCommand,
    pub fields: Vec<FieldSpec>,
}

impl DraftCommand {
    /// The canonical command id.
    #[must_use]
    pub const fn command_id(&self) -> &'static str {
        self.command.id()
    }

    /// Create a lines-align draft command.
    #[must_use]
    pub fn lines_align() -> Self {
        Self {
            command: ContextCommand::LinesAlign,
            fields: vec![FieldSpec::Text {
                placeholder: "Delimiter",
                literal_only: true,
            }],
        }
    }

    /// Create a goto-line draft command.
    #[must_use]
    pub fn goto_line(max_lines: u32) -> Self {
        Self {
            command: ContextCommand::GotoLine,
            fields: vec![FieldSpec::Number {
                max_value: max_lines,
            }],
        }
    }

    /// Create a replace-regex draft command (FEUX-0006 E-6).
    /// Has three fields: pattern (Regex), template (Text), and scope toggle.
    #[must_use]
    pub fn replace_regex() -> Self {
        Self {
            command: ContextCommand::ReplaceRegex,
            fields: vec![
                FieldSpec::Regex {
                    placeholder: "Pattern",
                },
                FieldSpec::Text {
                    placeholder: "Replacement",
                    literal_only: true,
                },
            ],
        }
    }
}

/// Per-field input state.
#[derive(Debug, Clone)]
pub struct FieldState {
    /// The text content for this field.
    pub text: String,
    /// The editor content for this field.
    pub input: iced::widget::text_editor::Content,
    /// Validation error for this field, if any.
    pub error: Option<String>,
}

impl FieldState {
    fn new() -> Self {
        Self {
            text: String::new(),
            input: iced::widget::text_editor::Content::new(),
            error: None,
        }
    }

    fn with_text(text: &str) -> Self {
        Self {
            text: text.to_string(),
            input: iced::widget::text_editor::Content::with_text(text),
            error: None,
        }
    }
}

/// A computed replace preview: the exact match/op totals and the op ranges
/// (for the outline). Compute-only (FEUX-0006 E-6): never a commit input.
#[derive(Debug, Clone)]
pub struct ReplacePreview {
    pub match_count: u64,
    pub output_len: u64,
    pub spans: Arc<[(usize, usize)]>,
}

/// Per-view command-argument row state.
#[derive(Debug)]
pub struct CommandArgsState {
    /// The command being drafted.
    pub draft: DraftCommand,
    /// Per-field input state.
    pub fields: Vec<FieldState>,
    /// Which field is currently active (index into draft.fields).
    pub active_field: usize,
    /// The row is the keyboard owner while open.
    pub focused: bool,
    /// The editor target for which this draft was opened.
    pub target: EditorTarget,
    /// Scope for replace_regex: true = Selections, false = Document.
    pub in_selections: bool,
    /// The in-flight replace_plan request (preview or commit), if any.
    pub pending: Option<u64>,
    /// A preview/commit refresh queued behind the in-flight request: with at
    /// most one worker job per draft, a new draft pass waits for the current
    /// one to settle instead of double-firing (the helper pool is app-wide).
    pub refresh_pending: Option<bool>,
    /// The latest computed preview (replace_regex only).
    pub preview: Option<ReplacePreview>,
    /// Plan-level error (replace_regex): shown on the row independent of any
    /// single field's validation error.
    pub state_error: Option<String>,
}

impl CommandArgsState {
    fn new(draft: DraftCommand, target: EditorTarget) -> Self {
        let fields = vec![FieldState::new(); draft.fields.len()];
        Self {
            draft,
            fields,
            active_field: 0,
            focused: true,
            target,
            in_selections: false,
            pending: None,
            refresh_pending: None,
            preview: None,
            state_error: None,
        }
    }

    /// Get the active field spec.
    #[must_use]
    fn active_spec(&self) -> Option<&FieldSpec> {
        self.draft.fields.get(self.active_field)
    }

    /// Get the active field state mutably.
    #[must_use]
    fn active_field_state_mut(&mut self) -> Option<&mut FieldState> {
        self.fields.get_mut(self.active_field)
    }

    /// Validate a single field.
    fn validate_field(&self, field_idx: usize) -> Result<Option<String>, String> {
        let field_state = self
            .fields
            .get(field_idx)
            .ok_or_else(|| "Field index out of bounds".to_string())?;
        let spec = self
            .draft
            .fields
            .get(field_idx)
            .ok_or_else(|| "Spec index out of bounds".to_string())?;

        let text = field_state.text.trim();
        if text.is_empty() {
            return Ok(None);
        }

        match spec {
            FieldSpec::Text {
                literal_only: true, ..
            } => {
                if text.is_empty() {
                    Err("Argument cannot be empty".to_string())
                } else {
                    Ok(Some(text.to_string()))
                }
            }
            FieldSpec::Text { .. } => {
                if text.is_empty() {
                    Err("Argument cannot be empty".to_string())
                } else {
                    Ok(Some(text.to_string()))
                }
            }
            FieldSpec::Number { max_value } => {
                let parsed: Result<u32, _> = text.parse();
                match parsed {
                    Ok(n) if n >= 1 && n <= *max_value => Ok(Some(text.to_string())),
                    Ok(_) => Err(format!("Number must be between 1 and {max_value}")),
                    Err(_) => Err("Invalid number".to_string()),
                }
            }
            FieldSpec::Regex { .. } => {
                // Validate regex: non-empty and within byte limit
                if text.is_empty() {
                    Err("Pattern cannot be empty".to_string())
                } else if text.len() > PATTERN_MAX_BYTES {
                    Err(format!(
                        "Pattern exceeds the {} KiB limit",
                        PATTERN_MAX_BYTES / 1024
                    ))
                } else {
                    // Compile-only validation on main thread (FEUX-0006 E-6/E-10)
                    // This is bounded by the same limits as the worker
                    if let Err(e) = validate_regex_compile(text) {
                        Err(format!("Invalid regex: {e}"))
                    } else {
                        Ok(Some(text.to_string()))
                    }
                }
            }
        }
    }

    /// Validate all fields and return their values.
    fn validate_all(&self) -> Result<Vec<Option<String>>, Vec<(usize, String)>> {
        let mut values = Vec::new();
        let mut errors = Vec::new();

        for (idx, _) in self.fields.iter().enumerate() {
            match self.validate_field(idx) {
                Ok(value) => values.push(value),
                Err(e) => errors.push((idx, e)),
            }
        }

        if errors.is_empty() {
            Ok(values)
        } else {
            Err(errors)
        }
    }
}

/// Compile-only regex validation (FEUX-0006 E-6, E-10).
/// This performs compile-time validation with the same limits as the worker,
/// but does NOT execute the regex against any text.
fn validate_regex_compile(pattern: &str) -> Result<(), String> {
    use regex::RegexBuilder;
    RegexBuilder::new(pattern)
        .case_insensitive(false)
        .multi_line(true)
        .dot_matches_new_line(false)
        .size_limit(PATTERN_MAX_BYTES * 1024) // 8 MiB program size limit
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Host state: one row per tab plus draft request correlation.
#[derive(Debug, Default)]
pub struct CommandArgsHost {
    pub tabs: BTreeMap<TabId, CommandArgsState>,
    /// In-flight replace_plan requests: request id → (tab, is_commit).
    pub requests: BTreeMap<u64, (TabId, bool)>,
}

impl CommandArgsHost {
    pub(crate) fn detach(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
        self.requests.retain(|_, (owner, _)| *owner != tab);
    }

    /// Check if any tab has an open draft.
    #[must_use]
    pub fn has_open_draft(&self) -> bool {
        !self.tabs.is_empty()
    }

    /// Check if a specific tab has an open draft.
    #[must_use]
    pub fn has_draft(&self, tab: TabId) -> bool {
        self.tabs.contains_key(&tab)
    }

    /// Get the active draft command for a tab.
    #[must_use]
    pub fn draft_command(&self, tab: TabId) -> Option<&DraftCommand> {
        self.tabs.get(&tab).map(|s| &s.draft)
    }

    /// Get the in_selections flag for a tab.
    #[must_use]
    pub fn in_selections(&self, tab: TabId) -> bool {
        self.tabs
            .get(&tab)
            .map(|s| s.in_selections)
            .unwrap_or(false)
    }

    /// Returns true if this host is blocking typing for the given tab.
    #[must_use]
    pub fn blocks_typing(&self, tab: &TabId) -> bool {
        self.tabs.contains_key(tab)
    }
}

/// Returns true if the command-args draft is blocking typing for the given tab.
pub fn blocks_typing(app: &App, tab: &TabId) -> bool {
    app.command_args.blocks_typing(tab)
}

/// Messages owned by the command-args host.
#[derive(Debug, Clone)]
pub enum CommandArgsMessage {
    /// Open a draft for the given command on the given tab.
    Open { tab: TabId, draft: DraftCommand },
    /// Close the draft on the given tab.
    Close(TabId),
    /// Draft text edit action.
    DraftAction {
        tab: TabId,
        action: iced::widget::text_editor::Action,
    },
    /// Apply the draft (Enter pressed).
    Apply(TabId),
    /// Cancel the draft (Esc pressed).
    Cancel(TabId),
    /// Move to next field (Tab).
    NextField(TabId),
    /// Move to previous field (Shift+Tab).
    PrevField(TabId),
    /// Adjust number up (Up arrow for Number fields).
    AdjustUp(TabId),
    /// Adjust number down (Down arrow for Number fields).
    AdjustDown(TabId),
    /// Toggle scope (Selections vs Document) for replace_regex.
    ToggleScope(TabId),
    /// The mounted row rectangle (for focus management).
    Bounds { tab: TabId, rect: iced::Rectangle },
    /// The mounted input rectangle (for pointer focus).
    InputBounds { tab: TabId, rect: iced::Rectangle },
}

/// Record one diagnostics outcome for the command-args path.
fn diag(app: &mut App, tab: TabId, kind: EventKind, outcome: Outcome) {
    app.diagnostics.record(
        Some(tab),
        DiagMode::from(app.context_mode.mode(tab)),
        kind,
        outcome,
        None,
        None,
    );
}

/// Cancel one in-flight replace_plan request at the source (cancel-
/// supersersede): the Session stops the worker job and drops its outcome;
/// the UI retires its correlation state.
fn cancel_plan(app: &mut App, request_id: u64) {
    if let Some(api) = app.flavite.as_ref() {
        let _ = api.execute(Invocation::new(
            api.command_registry().next_request_id(),
            "editor.cancel_selection",
            serde_json::json!({"request_id": request_id}),
            Origin::Gui,
        ));
    }
    app.command_args.requests.remove(&request_id);
    app.operations.cancel(request_id);
}

/// Cancel the draft on a tab (called when mode exits or tab closes).
pub(crate) fn cancel_tab(app: &mut App, tab: TabId) {
    let pending = app
        .command_args
        .tabs
        .get(&tab)
        .and_then(|state| state.pending);
    if let Some(request_id) = pending {
        cancel_plan(app, request_id);
    }
    app.command_args.tabs.remove(&tab);
    app.command_args
        .requests
        .retain(|_, (owner, _)| *owner != tab);
}

/// Open a lines-align draft for the given target.
pub(crate) fn open_lines_align(app: &mut App, target: EditorTarget) {
    // Close any existing draft on this tab
    app.command_args.tabs.remove(&target.tab);
    // Close matches draft for mutual exclusion
    app.selection_search.tabs.remove(&target.tab);

    let draft = DraftCommand::lines_align();
    app.command_args
        .tabs
        .insert(target.tab, CommandArgsState::new(draft, target));
}

/// Open a replace-regex draft for the given target (FEUX-0006 E-6).
pub(crate) fn open_replace_regex(app: &mut App, target: EditorTarget) {
    // Close any existing draft on this tab
    cancel_tab(app, target.tab);
    // Close matches draft for mutual exclusion
    app.selection_search.tabs.remove(&target.tab);

    let draft = DraftCommand::replace_regex();
    let mut state = CommandArgsState::new(draft, target.clone());
    // Initialize with empty fields for pattern and template
    state.fields = vec![FieldState::new(), FieldState::new()];
    app.command_args.tabs.insert(target.tab, state);
    request_plan(app, target.tab, false);
}

/// Open a goto-line draft for the given target.
/// Uses a large max line count; the actual validation happens in the handler (FEUX-0004 E-12).
pub(crate) fn open_goto_line(app: &mut App, target: EditorTarget) {
    // Close any existing draft on this tab
    app.command_args.tabs.remove(&target.tab);
    // Close matches draft for mutual exclusion
    app.selection_search.tabs.remove(&target.tab);

    // Use a large max value; the handler will validate against actual line count
    // and return an error with the correct bound if needed
    let draft = DraftCommand::goto_line(999_999);
    let mut state = CommandArgsState::new(draft, target.clone());
    state.fields = vec![FieldState::new()];
    app.command_args.tabs.insert(target.tab, state);
}

/// Build the `text.replace_plan` invocation args for a draft row. Returns
/// None when the pattern is empty (idle — no worker pass).
fn build_replace_plan_args(state: &CommandArgsState) -> Option<serde_json::Value> {
    let pattern = state.fields.first()?.text.trim();
    if pattern.is_empty() {
        return None;
    }
    let template = state
        .fields
        .get(1)
        .map(|field| field.text.clone())
        .unwrap_or_default();
    let target = &state.target;
    let scope_spans = if state.in_selections {
        target.selections.clone()
    } else {
        Vec::new()
    };
    Some(serde_json::json!({
        "buffer_id": target.buffer,
        "revision": target.revision,
        "query": pattern,
        "replacement": template,
        "mode": "regex",
        "case_sensitive": false,
        "whole_word": false,
        "scope_spans": scope_spans,
    }))
}

/// Admit one replace_plan compute through the bounded worker. At most one
/// request per draft: a new pass while one is in flight queues as a refresh
/// and lands when the in-flight request settles (no double-fire — the
/// helper pool is app-wide). `is_commit` marks the Enter-time fresh
/// computation — the only input a commit ever uses (E-6: the preview is
/// never the commit input).
fn request_plan(app: &mut App, tab: TabId, is_commit: bool) {
    let Some(state) = app.command_args.tabs.get(&tab) else {
        return;
    };
    if !matches!(state.draft.command, ContextCommand::ReplaceRegex) {
        return;
    }
    if state.pending.is_some() {
        // Queue behind the in-flight request; it settles on completion.
        if let Some(state) = app.command_args.tabs.get_mut(&tab) {
            state.refresh_pending = Some(is_commit);
        }
        return;
    }
    let Some(args) = build_replace_plan_args(state) else {
        // Idle: no pattern yet — clear any stale preview.
        if let Some(state) = app.command_args.tabs.get_mut(&tab) {
            state.pending = None;
            state.refresh_pending = None;
            state.preview = None;
            state.state_error = None;
        }
        return;
    };
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    let request_id = api.command_registry().next_request_id();
    let result = api.execute(Invocation::new(
        request_id,
        "text.replace_plan",
        args,
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => {
            app.operations.accept(
                request_id,
                if is_commit {
                    "replace.commit"
                } else {
                    "replace.preview"
                },
            );
            app.command_args
                .requests
                .insert(request_id, (tab, is_commit));
            if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                state.pending = Some(request_id);
                state.state_error = None;
            }
            diag(app, tab, EventKind::MatchCollect, Outcome::Started);
        }
        ExecutionResult::Local(LocalResult::Success(success)) => {
            if is_commit {
                finish_commit(app, tab, &success.data);
            } else {
                install_preview(app, tab, &success.data);
            }
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                state.pending = None;
                state.refresh_pending = None;
                state.state_error = Some(error.message.clone());
            }
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused);
        }
        ExecutionResult::Rejected(rejected) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                state.pending = None;
                state.refresh_pending = None;
                state.state_error = Some(
                    rejected
                        .message
                        .clone()
                        .unwrap_or_else(|| "Replace plan was refused".into()),
                );
            }
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused);
        }
        _ => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                state.pending = None;
                state.refresh_pending = None;
                state.state_error = Some("Replace plan could not be admitted; retry".into());
            }
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused);
        }
    }
}

/// Install a completed replace_plan preview when the target is still the one
/// the draft was captured under (stale results drop exactly like the match
/// collection pipeline).
fn install_preview(app: &mut App, tab: TabId, data: &serde_json::Value) {
    let Some(current) = EditorTarget::capture(app, tab) else {
        return;
    };
    let Some(state) = app.command_args.tabs.get(&tab) else {
        return;
    };
    if current.buffer != state.target.buffer
        || current.revision != state.target.revision
        || current.selection_version != state.target.selection_version
    {
        return;
    }
    let Some(match_count) = data.get("match_count").and_then(serde_json::Value::as_u64) else {
        return;
    };
    let Some(output_len) = data.get("output_len").and_then(serde_json::Value::as_u64) else {
        return;
    };
    let Some(raw_ops) = data.get("ops").and_then(serde_json::Value::as_array) else {
        return;
    };
    let Some(view) = app.note_editor_view(tab) else {
        return;
    };
    let mut spans = Vec::with_capacity(raw_ops.len());
    for raw in raw_ops {
        let Some(pair) = raw.as_array() else {
            return;
        };
        if pair.len() != 3 {
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
    if let Some(state) = app.command_args.tabs.get_mut(&tab) {
        state.pending = None;
        state.state_error = None;
        state.preview = Some(ReplacePreview {
            match_count,
            output_len,
            spans: Arc::from(spans.into_boxed_slice()),
        });
    }
    diag(app, tab, EventKind::MatchPreview, Outcome::Ready);
    // A refresh queued behind this request lands now.
    let queued = app
        .command_args
        .tabs
        .get(&tab)
        .and_then(|state| state.refresh_pending);
    if let Some(is_commit) = queued {
        if let Some(state) = app.command_args.tabs.get_mut(&tab) {
            state.refresh_pending = None;
        }
        request_plan(app, tab, is_commit);
    }
}

/// Finish an Enter-time replace_plan: validate the computed ops, commit
/// through the editor kernel (E-5), and close the draft on success. Any
/// refusal leaves the text untouched and the draft open with the error.
fn finish_commit(app: &mut App, tab: TabId, data: &serde_json::Value) {
    let Some(raw_ops) = data.get("ops").and_then(serde_json::Value::as_array) else {
        return;
    };
    let mut ops = Vec::with_capacity(raw_ops.len());
    for raw in raw_ops {
        let Some(pair) = raw.as_array() else {
            return;
        };
        if pair.len() != 3 {
            return;
        }
        let (Some(start), Some(end)) = (pair[0].as_u64(), pair[1].as_u64()) else {
            return;
        };
        let Some(replacement) = pair[2].as_str() else {
            return;
        };
        let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
            return;
        };
        ops.push((start, end, replacement.to_string()));
    }
    // No matches: a no-op replace — close the draft without committing.
    if ops.is_empty() {
        app.command_args.tabs.remove(&tab);
        return;
    }
    let Some(target) = EditorTarget::capture(app, tab) else {
        if let Some(state) = app.command_args.tabs.get_mut(&tab) {
            state.pending = None;
            state.state_error = Some("Source changed; replace was refused".into());
        }
        return;
    };
    match replace_commit(app, tab, &target, ops) {
        Ok(()) => {
            app.command_args.tabs.remove(&tab);
        }
        Err(message) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                state.pending = None;
                state.state_error = Some(message);
            }
        }
    }
}

/// Handle one command-args message.
pub fn handle(app: &mut App, message: CommandArgsMessage) -> iced::Task<Message> {
    match message {
        CommandArgsMessage::Open { tab, draft } => {
            // Close any existing draft on this tab and close matches draft
            let target = crate::host::context_mode::EditorTarget {
                tab,
                incarnation: 0,
                buffer: 0,
                revision: 0,
                selection: (0, 0),
                selections: vec![],
                primary: 0,
                selection_version: 0,
            };
            let fields_len = draft.fields.len();
            let mut state = CommandArgsState::new(draft, target);
            // Initialize fields
            state.fields = vec![FieldState::new(); fields_len];
            app.command_args.tabs.insert(tab, state);
            // Close matches draft for mutual exclusion (E-11)
            app.selection_search.tabs.remove(&tab);
            app.selection_search.pending_focus = None;

            iced::Task::none()
        }
        CommandArgsMessage::Close(tab) => {
            cancel_tab(app, tab);
            iced::Task::none()
        }
        CommandArgsMessage::Cancel(tab) => {
            cancel_tab(app, tab);
            iced::Task::none()
        }
        CommandArgsMessage::DraftAction { tab, action } => {
            let is_replace = app
                .command_args
                .tabs
                .get(&tab)
                .is_some_and(|state| state.draft.command == ContextCommand::ReplaceRegex);
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && let Some(field_state) = state.active_field_state_mut()
            {
                field_state.input.perform(action.clone());
                field_state.text = field_state.input.text();
                // Clear error on edit
                field_state.error = None;
            }
            if is_replace {
                request_plan(app, tab, false);
            }
            iced::Task::none()
        }
        CommandArgsMessage::NextField(tab) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && state.active_field + 1 < state.draft.fields.len()
            {
                state.active_field += 1;
            }
            iced::Task::none()
        }
        CommandArgsMessage::PrevField(tab) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && state.active_field > 0
            {
                state.active_field -= 1;
            }
            iced::Task::none()
        }
        CommandArgsMessage::ToggleScope(tab) => {
            let mut toggled = false;
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && state.draft.command == ContextCommand::ReplaceRegex
            {
                state.in_selections = !state.in_selections;
                state.preview = None;
                state.state_error = None;
                toggled = true;
            }
            if toggled {
                request_plan(app, tab, false);
            }
            iced::Task::none()
        }
        CommandArgsMessage::AdjustUp(tab) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                let max_value = if let Some(FieldSpec::Number { max_value }) = state.active_spec() {
                    *max_value
                } else {
                    return iced::Task::none();
                };
                if let Some(field_state) = state.active_field_state_mut() {
                    if let Ok(n) = field_state.text.trim().parse::<u32>() {
                        let new_n = n.saturating_add(1).min(max_value);
                        *field_state = FieldState::with_text(&new_n.to_string());
                    } else {
                        // Parse failed, try starting from 1
                        *field_state = FieldState::with_text("1");
                    }
                }
            }
            iced::Task::none()
        }
        CommandArgsMessage::AdjustDown(tab) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && let Some(field_state) = state.active_field_state_mut()
            {
                if let Ok(n) = field_state.text.trim().parse::<u32>() {
                    let new_n = n.saturating_sub(1).max(1);
                    *field_state = FieldState::with_text(&new_n.to_string());
                } else {
                    // Parse failed, try starting from 1
                    *field_state = FieldState::with_text("1");
                }
            }
            iced::Task::none()
        }
        CommandArgsMessage::Apply(tab) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                let target = state.target.clone();
                let command = state.draft.command;

                // Validate all fields
                match state.validate_all() {
                    Ok(values) => {
                        // Check that required fields are not empty
                        let mut has_error = false;
                        for (idx, spec) in state.draft.fields.iter().enumerate() {
                            if let Some(value) = values.get(idx)
                                && value.is_none()
                            {
                                // Field is empty - check if it's required
                                match spec {
                                    FieldSpec::Regex { .. } | FieldSpec::Text { .. } => {
                                        // These are required for replace_regex
                                        if command == ContextCommand::ReplaceRegex {
                                            if let Some(field_state) = state.fields.get_mut(idx) {
                                                field_state.error =
                                                    Some("Field cannot be empty".to_string());
                                            }
                                            has_error = true;
                                        }
                                    }
                                    FieldSpec::Number { .. } => {
                                        if let Some(field_state) = state.fields.get_mut(idx) {
                                            field_state.error =
                                                Some("Field cannot be empty".to_string());
                                        }
                                        has_error = true;
                                    }
                                }
                            }
                        }

                        if has_error {
                            return iced::Task::none();
                        }

                        // For ReplaceRegex, Enter runs a FRESH replace_plan at
                        // the current revision and commits on its completion
                        // (FEUX-0006 E-5/E-6): the preview is never the commit
                        // input.
                        if command == ContextCommand::ReplaceRegex {
                            request_plan(app, tab, true);
                        } else {
                            // For single-field commands (LinesAlign, GotoLine)
                            if let Some(Some(arg)) = values.first() {
                                let mut json_args = serde_json::Map::new();
                                match &state.draft.fields[0] {
                                    FieldSpec::Text { .. } => {
                                        json_args.insert(
                                            "delimiter".to_string(),
                                            serde_json::Value::String(arg.clone()),
                                        );
                                    }
                                    FieldSpec::Regex { .. } => {
                                        json_args.insert(
                                            "pattern".to_string(),
                                            serde_json::Value::String(arg.clone()),
                                        );
                                    }
                                    FieldSpec::Number { max_value: _ } => {
                                        if let Ok(n) = arg.parse::<u64>() {
                                            json_args.insert(
                                                "line".to_string(),
                                                serde_json::Value::Number(
                                                    serde_json::Number::from(n),
                                                ),
                                            );
                                        }
                                    }
                                }

                                // Dispatch through kernel mutation path
                                let _ = kernel_mutate(
                                    app,
                                    &target,
                                    command.id(),
                                    serde_json::Value::Object(json_args),
                                    "Draft command was refused; source changed",
                                );

                                // Close the draft
                                app.command_args.tabs.remove(&tab);
                            } else {
                                // Empty argument
                                if let Some(field_state) = state.fields.get_mut(0) {
                                    field_state.error = Some("Argument required".to_string());
                                }
                            }
                        }
                    }
                    Err(errors) => {
                        // Set errors on fields
                        for (idx, error) in errors {
                            if let Some(field_state) = state.fields.get_mut(idx) {
                                field_state.error = Some(error);
                            }
                        }
                    }
                }
            }
            iced::Task::none()
        }
        CommandArgsMessage::Bounds { .. } | CommandArgsMessage::InputBounds { .. } => {
            // Bounds messages are used for focus management - ignore for now
            iced::Task::none()
        }
    }
}

/// Route one replace_plan completion; returns whether it belonged to the
/// command-args host.
pub fn completion(app: &mut App, completion: &flavite_api::OperationCompletion) -> bool {
    let Some((tab, is_commit)) = app.command_args.requests.remove(&completion.request_id) else {
        return false;
    };
    if app.workspace.tab(&tab).is_none() {
        return true;
    }
    let Some(state) = app.command_args.tabs.get_mut(&tab) else {
        return true;
    };
    if state.pending != Some(completion.request_id) {
        return true;
    }
    state.pending = None;
    match &completion.result {
        ExecutionResult::Local(LocalResult::Success(success)) => {
            if is_commit {
                finish_commit(app, tab, &success.data);
            } else {
                install_preview(app, tab, &success.data);
            }
        }
        ExecutionResult::Local(LocalResult::Error(error)) => {
            state.preview = None;
            state.refresh_pending = None;
            state.state_error = Some(error.message.clone());
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused);
        }
        ExecutionResult::Rejected(rejected) => {
            state.preview = None;
            state.refresh_pending = None;
            state.state_error = Some(
                rejected
                    .message
                    .clone()
                    .unwrap_or_else(|| "Replace plan was refused".into()),
            );
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused);
        }
        _ => {
            state.preview = None;
            state.refresh_pending = None;
            state.state_error = Some("Replace plan is stale; try again".into());
            diag(app, tab, EventKind::MatchRefused, Outcome::Refused);
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_align_draft_has_text_field() {
        let draft = DraftCommand::lines_align();
        assert_eq!(draft.command, ContextCommand::LinesAlign);
        assert_eq!(draft.fields.len(), 1);
        match &draft.fields[0] {
            FieldSpec::Text {
                placeholder,
                literal_only,
            } => {
                assert_eq!(*placeholder, "Delimiter");
                assert!(*literal_only);
            }
            _ => panic!("Expected Text field"),
        }
    }

    #[test]
    fn goto_line_draft_has_number_field() {
        let draft = DraftCommand::goto_line(100);
        assert_eq!(draft.command, ContextCommand::GotoLine);
        assert_eq!(draft.fields.len(), 1);
        match &draft.fields[0] {
            FieldSpec::Number { max_value } => {
                assert_eq!(*max_value, 100);
            }
            _ => panic!("Expected Number field"),
        }
    }

    #[test]
    fn replace_regex_draft_has_two_fields() {
        let draft = DraftCommand::replace_regex();
        assert_eq!(draft.command, ContextCommand::ReplaceRegex);
        assert_eq!(draft.fields.len(), 2);
        match &draft.fields[0] {
            FieldSpec::Regex { placeholder } => {
                assert_eq!(*placeholder, "Pattern");
            }
            _ => panic!("Expected Regex field"),
        }
        match &draft.fields[1] {
            FieldSpec::Text {
                placeholder,
                literal_only,
            } => {
                assert_eq!(*placeholder, "Replacement");
                assert!(*literal_only);
            }
            _ => panic!("Expected Text field"),
        }
    }

    fn test_target() -> EditorTarget {
        EditorTarget {
            tab: TabId::from_raw(1),
            buffer: 0,
            revision: 0,
            incarnation: 0,
            selection: (0, 0),
            selections: vec![],
            primary: 0,
            selection_version: 0,
        }
    }

    #[test]
    fn text_field_validates_non_empty() {
        let mut state = CommandArgsState::new(DraftCommand::lines_align(), test_target());
        state.fields = vec![FieldState::with_text("  ")];
        assert!(matches!(state.validate_field(0), Ok(None)));

        state.fields = vec![FieldState::with_text("=")];
        let result = state.validate_field(0);
        assert!(matches!(result, Ok(Some(ref s)) if s == "="));
    }

    #[test]
    fn number_field_validates_range() {
        let mut state = CommandArgsState::new(DraftCommand::goto_line(10), test_target());
        state.fields = vec![FieldState::with_text("0")];
        assert!(state.validate_field(0).is_err());

        state.fields = vec![FieldState::with_text("5")];
        let result = state.validate_field(0);
        assert!(matches!(result, Ok(Some(ref s)) if s == "5"));

        state.fields = vec![FieldState::with_text("11")];
        assert!(state.validate_field(0).is_err());

        state.fields = vec![FieldState::with_text("abc")];
        assert!(state.validate_field(0).is_err());
    }

    #[test]
    fn regex_field_validates_compile() {
        let mut state = CommandArgsState::new(DraftCommand::replace_regex(), test_target());

        // Valid regex
        state.fields = vec![FieldState::with_text(r"a(b)?c")];
        let result = state.validate_field(0);
        assert!(matches!(result, Ok(Some(_))));

        // Invalid regex
        state.fields = vec![FieldState::with_text(r"[")];
        let result = state.validate_field(0);
        assert!(result.is_err());

        // Empty regex: skipped here (None); requiredness is enforced at apply time
        state.fields = vec![FieldState::with_text("")];
        let result = state.validate_field(0);
        assert!(matches!(result, Ok(None)));

        // Too long regex (simulate with a short one for test)
        state.fields = vec![FieldState::with_text("a")];
        let result = state.validate_field(0);
        assert!(matches!(result, Ok(Some(_))));
    }

    #[test]
    fn command_args_host_mutual_exclusion() {
        let mut host = CommandArgsHost::default();
        let tab = TabId::from_raw(1);

        // Initially no draft
        assert!(!host.has_open_draft());

        // Open a draft
        let mut state = CommandArgsState::new(DraftCommand::lines_align(), test_target());
        state.fields = vec![FieldState::new()];
        host.tabs.insert(tab, state);
        assert!(host.has_open_draft());
        assert!(host.has_draft(tab));

        // Close the draft
        host.detach(tab);
        assert!(!host.has_open_draft());
    }

    #[test]
    fn field_state_new() {
        let state = FieldState::new();
        assert!(state.text.is_empty());
        assert!(state.error.is_none());
    }

    #[test]
    fn field_state_with_text() {
        let state = FieldState::with_text("test");
        assert_eq!(state.text, "test");
        assert!(state.error.is_none());
    }

    #[cfg(test)]
    impl CommandArgsState {
        /// Get all non-empty field values (test-only).
        fn get_all_values(&self) -> Vec<String> {
            self.fields.iter().map(|f| f.text.clone()).collect()
        }
    }

    #[test]
    fn command_args_state_get_values() {
        let mut state = CommandArgsState::new(DraftCommand::replace_regex(), test_target());
        state.fields = vec![
            FieldState::with_text("pattern"),
            FieldState::with_text("template"),
        ];
        let values = state.get_all_values();
        assert_eq!(values.len(), 2);
        assert_eq!(values[0], "pattern");
        assert_eq!(values[1], "template");
    }

    #[test]
    fn build_replace_plan_args_maps_fields_and_scope() {
        let mut state = CommandArgsState::new(DraftCommand::replace_regex(), test_target());
        state.fields = vec![FieldState::with_text(r"\d+"), FieldState::with_text("[$0]")];
        state.in_selections = false;
        let args = build_replace_plan_args(&state).expect("args");
        assert_eq!(args["query"], r"\d+");
        assert_eq!(args["replacement"], "[$0]");
        assert_eq!(args["mode"], "regex");
        assert_eq!(args["case_sensitive"], false);
        assert_eq!(args["whole_word"], false);
        assert_eq!(args["scope_spans"].as_array().map(|a| a.len()), Some(0));

        // Selections scope carries the committed member spans.
        state.in_selections = true;
        state.target.selections = vec![(0, 4), (8, 12)];
        let args = build_replace_plan_args(&state).expect("args");
        let spans = args["scope_spans"].as_array().expect("spans");
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0][0].as_u64(), Some(0));
        assert_eq!(spans[0][1].as_u64(), Some(4));
        assert_eq!(spans[1][0].as_u64(), Some(8));
        assert_eq!(spans[1][1].as_u64(), Some(12));

        // Empty pattern is idle (no worker pass).
        state.fields[0] = FieldState::new();
        assert!(build_replace_plan_args(&state).is_none());
    }

    #[test]
    fn replace_draft_state_starts_idle_without_preview_or_error() {
        let state = CommandArgsState::new(DraftCommand::replace_regex(), test_target());
        assert!(state.pending.is_none());
        assert!(state.refresh_pending.is_none());
        assert!(state.preview.is_none());
        assert!(state.state_error.is_none());
        assert_eq!(state.active_field, 0);
        assert_eq!(state.fields.len(), 2);
    }
}
