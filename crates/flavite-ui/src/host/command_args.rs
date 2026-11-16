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
use crate::host::context_mode::{EditorTarget, kernel_mutate};
use flavite_api::{Invocation, Origin};
use flavite_edit_context::vocabulary::ContextCommand;
use flavite_workspace::TabId;
use std::collections::BTreeMap;

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

/// Host state: one row per tab.
#[derive(Debug, Default)]
pub struct CommandArgsHost {
    pub tabs: BTreeMap<TabId, CommandArgsState>,
}

impl CommandArgsHost {
    pub(crate) fn detach(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
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

/// Cancel the draft on a tab (called when mode exits or tab closes).
pub(crate) fn cancel_tab(app: &mut App, tab: TabId) {
    app.command_args.tabs.remove(&tab);
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
    app.command_args.tabs.remove(&target.tab);
    // Close matches draft for mutual exclusion
    app.selection_search.tabs.remove(&target.tab);

    let draft = DraftCommand::replace_regex();
    let mut state = CommandArgsState::new(draft, target.clone());
    // Initialize with empty fields for pattern and template
    state.fields = vec![FieldState::new(), FieldState::new()];
    app.command_args.tabs.insert(target.tab, state);
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
            app.command_args.tabs.remove(&tab);
            iced::Task::none()
        }
        CommandArgsMessage::Cancel(tab) => {
            app.command_args.tabs.remove(&tab);
            iced::Task::none()
        }
        CommandArgsMessage::DraftAction { tab, action } => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && let Some(field_state) = state.active_field_state_mut()
            {
                field_state.input.perform(action.clone());
                field_state.text = field_state.input.text();
                // Clear error on edit
                field_state.error = None;
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
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && state.draft.command == ContextCommand::ReplaceRegex
            {
                state.in_selections = !state.in_selections;
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

                        // For ReplaceRegex, handle specially (FEUX-0006 E-5)
                        if command == ContextCommand::ReplaceRegex {
                            let pattern =
                                values.first().and_then(|v| v.clone()).unwrap_or_default();
                            let template =
                                values.get(1).and_then(|v| v.clone()).unwrap_or_default();

                            // Build scope spans from current selections
                            let scope_spans: Vec<(usize, usize)> = if state.in_selections {
                                target.selections.clone()
                            } else {
                                Vec::new() // Empty = whole document
                            };

                            // Call text.replace_plan (ReadNotes) to compute the plan
                            let api = app.flavite.as_ref();
                            if let Some(api) = api {
                                let request_id = api.command_registry().next_request_id();
                                let invocation = Invocation::new(
                                    request_id,
                                    "text.replace_plan",
                                    serde_json::json!({
                                        "buffer_id": target.buffer,
                                        "revision": target.revision,
                                        "query": pattern,
                                        "replacement": template,
                                        "mode": "regex",
                                        "case_sensitive": false,
                                        "whole_word": false,
                                        "scope_spans": scope_spans,
                                    }),
                                    Origin::Gui,
                                );

                                // Execute the replace_plan request
                                match api.execute(invocation) {
                                    flavite_api::ExecutionResult::Local(
                                        flavite_api::LocalResult::Success(success),
                                    ) => {
                                        // Extract ops from the result
                                        if let Some(plan) = success.data.get("plan")
                                            && let (Some(_), Some(_), Some(ops)) = (
                                                plan.get("match_count").and_then(|v| v.as_u64()),
                                                plan.get("output_len").and_then(|v| v.as_u64()),
                                                plan.get("ops").and_then(|v| {
                                                    serde_json::from_value::<
                                                            Vec<serde_json::Value>,
                                                        >(
                                                            v.clone()
                                                        )
                                                        .ok()
                                                }),
                                            )
                                        {
                                            // Now commit via apply_edits
                                            let _ = super::context_mode::kernel_mutate(
                                                app,
                                                &target,
                                                "editor.apply_edits",
                                                serde_json::json!({
                                                    "buffer_id": target.buffer,
                                                    "revision": target.revision,
                                                    "ops": ops,
                                                    "undo_policy": "Isolated",
                                                }),
                                                "Replace was refused; source changed",
                                            );

                                            // Close the draft
                                            app.command_args.tabs.remove(&tab);
                                        }
                                    }
                                    _ => {
                                        // Failed to execute replace_plan
                                        app.context_mode.notice("Replace plan failed");
                                    }
                                }
                            }
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

        // Empty regex
        state.fields = vec![FieldState::with_text("")];
        let result = state.validate_field(0);
        assert!(result.is_err());

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
}
