//! Command argument draft host (FEUX-0004 E-11, X03):
//! per-tab draft state for text/number arguments, rendered as a toolbar row.
//!
//! One host owns the per-view input row, validates on Enter, and dispatches
//! through the normal admission path. Drafts are mutually exclusive with the
//! matches draft host (opening one closes the other).

use crate::app::{App, Message};
use crate::host::context_mode::{EditorTarget, kernel_mutate};
use flavite_edit_context::vocabulary::ContextCommand;
use flavite_workspace::TabId;
use std::collections::BTreeMap;

/// Field specification for command arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldSpec {
    /// Text argument with placeholder and literal-only constraint.
    Text {
        placeholder: &'static str,
        literal_only: bool,
    },
    /// Numeric argument (1..=max_value).
    Number { max_value: u32 },
}

impl FieldSpec {
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            FieldSpec::Text { .. } => "text",
            FieldSpec::Number { .. } => "number",
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
}

/// Per-view command-argument row state.
#[derive(Debug)]
pub struct CommandArgsState {
    /// The command being drafted.
    pub draft: DraftCommand,
    /// The input content for all fields concatenated.
    pub input: iced::widget::text_editor::Content,
    /// The current input string.
    pub text: String,
    /// Which field is currently active (index into draft.fields).
    pub active_field: usize,
    /// Validation error, if any.
    pub error: Option<String>,
    /// The row is the keyboard owner while open.
    pub focused: bool,
    /// The editor target for which this draft was opened.
    pub target: EditorTarget,
}

impl CommandArgsState {
    fn new(draft: DraftCommand, target: EditorTarget) -> Self {
        Self {
            draft,
            input: iced::widget::text_editor::Content::new(),
            text: String::new(),
            active_field: 0,
            error: None,
            focused: true,
            target,
        }
    }

    /// Get the active field spec.
    #[must_use]
    fn active_spec(&self) -> Option<&FieldSpec> {
        self.draft.fields.get(self.active_field)
    }

    /// Validate the current input and return the argument value.
    /// Returns Ok(Some(value)) for valid non-empty input, Ok(None) for empty,
    /// or Err(message) for validation failures.
    fn validate(&self) -> Result<Option<String>, String> {
        let text = self.text.trim();
        if text.is_empty() {
            return Ok(None);
        }

        let spec = self
            .active_spec()
            .ok_or_else(|| "No active field".to_string())?;

        match spec {
            FieldSpec::Text {
                literal_only: true, ..
            } => {
                // Text fields just need non-empty content
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
        }
    }
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
    app.command_args
        .tabs
        .insert(target.tab, CommandArgsState::new(draft, target));
}

/// Handle one command-args message.
pub fn handle(app: &mut App, message: CommandArgsMessage) -> iced::Task<Message> {
    match message {
        CommandArgsMessage::Open { tab, draft } => {
            // Close any existing draft on this tab and close matches draft
            // Use a default target - the actual target should be set by open_lines_align/open_goto_line
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
            app.command_args
                .tabs
                .insert(tab, CommandArgsState::new(draft, target));
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
            if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                state.input.perform(action.clone());
                state.text = state.input.text();
                // Clear error on edit
                state.error = None;
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
        CommandArgsMessage::AdjustUp(tab) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && let Some(FieldSpec::Number { max_value }) = state.active_spec()
            {
                if let Ok(n) = state.text.trim().parse::<u32>() {
                    let new_n = n.saturating_add(1).min(*max_value);
                    state.input = iced::widget::text_editor::Content::with_text(&new_n.to_string());
                    state.text = new_n.to_string();
                } else {
                    // Parse failed, try starting from 1
                    state.input = iced::widget::text_editor::Content::with_text("1");
                    state.text = "1".to_string();
                }
            }
            iced::Task::none()
        }
        CommandArgsMessage::AdjustDown(tab) => {
            if let Some(state) = app.command_args.tabs.get_mut(&tab)
                && let Some(FieldSpec::Number { max_value: _ }) = state.active_spec()
            {
                if let Ok(n) = state.text.trim().parse::<u32>() {
                    let new_n = n.saturating_sub(1).max(1);
                    state.input = iced::widget::text_editor::Content::with_text(&new_n.to_string());
                    state.text = new_n.to_string();
                } else {
                    // Parse failed, try starting from 1
                    state.input = iced::widget::text_editor::Content::with_text("1");
                    state.text = "1".to_string();
                }
            }
            iced::Task::none()
        }
        CommandArgsMessage::Apply(tab) => {
            if let Some(state) = app.command_args.tabs.get(&tab) {
                let target = state.target.clone();
                let command = state.draft.command;
                let api_id = command.id();
                let validated = state.validate();

                match validated {
                    Ok(Some(arg)) => {
                        // For X03: dispatch the command with arguments through kernel path

                        // Build the args JSON
                        let mut json_args = serde_json::Map::new();
                        match &state.draft.fields[0] {
                            FieldSpec::Text { .. } => {
                                json_args.insert(
                                    "delimiter".to_string(),
                                    serde_json::Value::String(arg),
                                );
                            }
                            FieldSpec::Number { max_value: _ } => {
                                if let Ok(n) = arg.parse::<u64>() {
                                    json_args.insert(
                                        "line".to_string(),
                                        serde_json::Value::Number(serde_json::Number::from(n)),
                                    );
                                }
                            }
                        }

                        // Dispatch through kernel mutation path
                        let _ = kernel_mutate(
                            app,
                            &target,
                            api_id,
                            serde_json::Value::Object(json_args),
                            "Draft command was refused; source changed",
                        );

                        // Close the draft
                        app.command_args.tabs.remove(&tab);
                    }
                    Ok(None) => {
                        // Empty input - set error
                        if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                            state.error = Some("Argument required".to_string());
                        }
                    }
                    Err(e) => {
                        if let Some(state) = app.command_args.tabs.get_mut(&tab) {
                            state.error = Some(e);
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
        state.text = "  ".to_string();
        assert!(matches!(state.validate(), Ok(None)));

        state.text = "=".to_string();
        let result = state.validate();
        assert!(matches!(result, Ok(Some(ref s)) if s == "="));
    }

    #[test]
    fn number_field_validates_range() {
        let mut state = CommandArgsState::new(DraftCommand::goto_line(10), test_target());

        state.text = "0".to_string();
        assert!(state.validate().is_err());

        state.text = "5".to_string();
        let result = state.validate();
        assert!(matches!(result, Ok(Some(ref s)) if s == "5"));

        state.text = "11".to_string();
        assert!(state.validate().is_err());

        state.text = "abc".to_string();
        assert!(state.validate().is_err());
    }

    #[test]
    fn command_args_host_mutual_exclusion() {
        let mut host = CommandArgsHost::default();
        let tab = TabId::from_raw(1);

        // Initially no draft
        assert!(!host.has_open_draft());

        // Open a draft
        host.tabs.insert(
            tab,
            CommandArgsState::new(DraftCommand::lines_align(), test_target()),
        );
        assert!(host.has_open_draft());
        assert!(host.has_draft(tab));

        // Close the draft
        host.detach(tab);
        assert!(!host.has_open_draft());
    }
}
