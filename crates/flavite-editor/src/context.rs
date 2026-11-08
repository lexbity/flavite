//! Context Mode's finite command vocabulary and physical-key grammar.
//!
//! This module describes only working source-editor operations. The host
//! owns mode lifetime and dispatch; the widget owns native event capture.

use iced::keyboard::{Modifiers, key::Code};
use unicode_segmentation::UnicodeSegmentation;

/// Native editor context intents carry the exact source and visible selection.
#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub buffer: u64,
    pub revision: u64,
    pub selection: (usize, usize),
    pub sequence: u64,
    pub event: ContextEvent,
}

#[derive(Debug, Clone)]
pub enum ContextEvent {
    Command(ContextCommand),
    Menu(ContextMenu),
    EnterAt((usize, usize)),
    Unbound,
    /// Interpret in acknowledged host mode, rather than the last drawn mode.
    Key(ContextKey),
    /// An IME commit ends composition and uses the same acknowledged edit path.
    Commit(crate::input::CapturedInput),
}

#[derive(Debug, Clone)]
pub struct ContextKey {
    pub code: Option<Code>,
    pub modifiers: Modifiers,
    pub repeat: bool,
    pub input: crate::input::CapturedInput,
}

/// Menus share the same physical grammar for pointer and native routing.
#[must_use]
pub fn menu_key(menu: ContextMenu, code: Code, modifiers: Modifiers) -> Option<ContextCommand> {
    if modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    use ContextCommand::*;
    let command = match (menu, code) {
        (ContextMenu::Select, Code::KeyW) => SelectWord,
        (ContextMenu::Select, Code::KeyX) => SelectLine,
        (ContextMenu::Select, Code::KeyA) => SelectAll,
        (ContextMenu::Go, Code::KeyH) => LineStart,
        (ContextMenu::Go, Code::KeyE) => LineEnd,
        (ContextMenu::Go, Code::KeyT) => DocumentStart,
        (ContextMenu::Go, Code::KeyB) => DocumentEnd,
        _ => command_key(code, modifiers)?,
    };
    menu.commands().contains(&command).then_some(command)
}

/// Unicode word selection shared by right-click and the Select menu.
#[must_use]
pub fn word_bounds(text: &str, at: usize) -> (usize, usize) {
    let at = crate::viewport::clamp_grapheme_boundary(text, at);
    text.split_word_bound_indices()
        .find(|(start, word)| {
            *start <= at
                && at < start + word.len()
                && word.chars().any(|c| c.is_alphanumeric() || c == '_')
        })
        .map_or((at, at), |(start, word)| (start, start + word.len()))
}

/// Human-facing character count, measured in selectable grapheme clusters.
#[must_use]
pub fn selected_characters(text: &str, selection: (usize, usize)) -> usize {
    text.get(selection.0.min(selection.1)..selection.0.max(selection.1))
        .map_or(0, |selected| selected.graphemes(true).count())
}

/// Physical selection motions reuse the ordinary acknowledged input path.
#[must_use]
pub fn motion_key(code: Code, modifiers: Modifiers) -> Option<crate::keymap::KeyAction> {
    if modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    use crate::keymap::KeyAction;
    Some(match code {
        Code::KeyH => KeyAction::Previous,
        Code::KeyJ => KeyAction::Down,
        Code::KeyK => KeyAction::Up,
        Code::KeyL => KeyAction::Next,
        Code::KeyW => KeyAction::NextWord,
        Code::KeyB => KeyAction::PreviousWord,
        _ => return None,
    })
}

/// The input interpretation of one source editor. Never persisted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ContextMode {
    #[default]
    Off,
    Command,
    Typing,
}

impl ContextMode {
    #[must_use]
    pub const fn active(self) -> bool {
        !matches!(self, Self::Off)
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Off => "Edit source",
            Self::Command => "COMMAND",
            Self::Typing => "TYPING",
        }
    }
}

/// Commands shared by the toolbar, palette and physical accelerators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextCommand {
    Toggle,
    Exit,
    Escape,
    Cut,
    Copy,
    Paste,
    Undo,
    Redo,
    Commands,
    History,
    Find,
    Replace,
    InsertStart,
    InsertEnd,
    InsertLineStart,
    InsertLineEnd,
    OpenAbove,
    OpenBelow,
    Change,
    Delete,
    SelectAll,
    SelectWord,
    SelectLine,
    Collapse,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
}

impl ContextCommand {
    /// One authoritative description list for every exposed command.
    pub const ALL: &'static [Self] = &[
        Self::Toggle,
        Self::Exit,
        Self::Cut,
        Self::Copy,
        Self::Paste,
        Self::Undo,
        Self::Redo,
        Self::Commands,
        Self::History,
        Self::Find,
        Self::Replace,
        Self::InsertStart,
        Self::InsertEnd,
        Self::InsertLineStart,
        Self::InsertLineEnd,
        Self::OpenAbove,
        Self::OpenBelow,
        Self::Change,
        Self::Delete,
        Self::SelectAll,
        Self::SelectWord,
        Self::SelectLine,
        Self::Collapse,
        Self::LineStart,
        Self::LineEnd,
        Self::DocumentStart,
        Self::DocumentEnd,
    ];

    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Toggle => "editor.context_toggle",
            Self::Exit => "editor.context_exit",
            Self::Escape => "editor.context_back",
            Self::Cut => "editor.cut",
            Self::Copy => "editor.copy",
            Self::Paste => "editor.paste",
            Self::Undo => "buffer.undo",
            Self::Redo => "buffer.redo",
            Self::Commands => "workspace.command_palette",
            Self::History => "editor.history",
            Self::Find => "workspace.find",
            Self::Replace => "workspace.replace",
            Self::InsertStart => "editor.insert_start",
            Self::InsertEnd => "editor.insert_end",
            Self::InsertLineStart => "editor.insert_line_start",
            Self::InsertLineEnd => "editor.insert_line_end",
            Self::OpenAbove => "editor.open_above",
            Self::OpenBelow => "editor.open_below",
            Self::Change => "editor.change",
            Self::Delete => "editor.delete",
            Self::SelectAll => "workspace.select_all",
            Self::SelectWord => "editor.select_word",
            Self::SelectLine => "editor.select_line",
            Self::Collapse => "editor.clear_selection",
            Self::LineStart => "workspace.caret_start_line",
            Self::LineEnd => "workspace.caret_end_line",
            Self::DocumentStart => "workspace.caret_start",
            Self::DocumentEnd => "workspace.caret_end",
        }
    }

    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Toggle => "Toggle Context Mode",
            Self::Exit => "Exit Context Mode",
            Self::Escape => "Return to commands",
            Self::Cut => "Cut",
            Self::Copy => "Copy",
            Self::Paste => "Paste",
            Self::Undo => "Undo",
            Self::Redo => "Redo",
            Self::Commands => "Commands",
            Self::History => "Editing history",
            Self::Find => "Find in note",
            Self::Replace => "Replace in note",
            Self::InsertStart => "Type before selection",
            Self::InsertEnd => "Type after selection",
            Self::InsertLineStart => "Type at line start",
            Self::InsertLineEnd => "Type at line end",
            Self::OpenAbove => "Open line above",
            Self::OpenBelow => "Open line below",
            Self::Change => "Change selection",
            Self::Delete => "Delete selection",
            Self::SelectAll => "Select all",
            Self::SelectWord => "Select word",
            Self::SelectLine => "Select line",
            Self::Collapse => "Clear selection",
            Self::LineStart => "Line start",
            Self::LineEnd => "Line end",
            Self::DocumentStart => "Document start",
            Self::DocumentEnd => "Document end",
        }
    }

    #[must_use]
    pub const fn mutates(self) -> bool {
        matches!(
            self,
            Self::Cut
                | Self::Paste
                | Self::Undo
                | Self::Redo
                | Self::OpenAbove
                | Self::OpenBelow
                | Self::Change
                | Self::Delete
        )
    }

    #[must_use]
    pub const fn enters_typing(self) -> bool {
        matches!(
            self,
            Self::InsertStart
                | Self::InsertEnd
                | Self::InsertLineStart
                | Self::InsertLineEnd
                | Self::OpenAbove
                | Self::OpenBelow
                | Self::Change
        )
    }

    /// Physical-position hints; no guessed active-layout glyph is shown.
    #[must_use]
    pub const fn hint(self) -> &'static str {
        match self {
            Self::Cut => "Shift+KeyD",
            Self::Copy => "KeyY",
            Self::Paste => "KeyP",
            Self::Undo => "KeyU",
            Self::Redo => "Shift+KeyU",
            Self::Commands => "Shift+Semicolon",
            Self::Find => "Slash",
            Self::Replace => "Shift+Slash",
            Self::InsertStart => "KeyI",
            Self::InsertEnd => "KeyA",
            Self::InsertLineStart => "Shift+KeyI",
            Self::InsertLineEnd => "Shift+KeyA",
            Self::OpenAbove => "Shift+KeyO",
            Self::OpenBelow => "KeyO",
            Self::Change => "KeyC",
            Self::Delete => "KeyD",
            Self::SelectLine => "KeyX",
            Self::Collapse => "Semicolon",
            _ => "",
        }
    }
}

/// A toolbar menu is a flat list; commands are not duplicated as aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextMenu {
    Select,
    Go,
    Type,
    More,
}

impl ContextMenu {
    #[must_use]
    pub const fn hint(self, command: ContextCommand) -> &'static str {
        match (self, command) {
            (Self::Select, ContextCommand::SelectWord) => "KeyW",
            (Self::Select, ContextCommand::SelectAll) => "KeyA",
            (Self::Go, ContextCommand::LineStart) => "KeyH",
            (Self::Go, ContextCommand::LineEnd) => "KeyE",
            (Self::Go, ContextCommand::DocumentStart) => "KeyT",
            (Self::Go, ContextCommand::DocumentEnd) => "KeyB",
            _ => command.hint(),
        }
    }
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Select => "Select",
            Self::Go => "Go",
            Self::Type => "Type",
            Self::More => "More",
        }
    }

    #[must_use]
    pub const fn commands(self) -> &'static [ContextCommand] {
        use ContextCommand::*;
        match self {
            Self::Select => &[SelectWord, SelectLine, SelectAll, Collapse],
            Self::Go => &[LineStart, LineEnd, DocumentStart, DocumentEnd],
            Self::Type => &[
                InsertStart,
                InsertEnd,
                InsertLineStart,
                InsertLineEnd,
                OpenAbove,
                OpenBelow,
                Change,
            ],
            Self::More => &[
                Cut,
                Copy,
                Paste,
                Undo,
                Redo,
                InsertStart,
                InsertEnd,
                InsertLineStart,
                InsertLineEnd,
                OpenAbove,
                OpenBelow,
                Change,
                Delete,
                SelectWord,
                SelectLine,
                SelectAll,
                Collapse,
                LineStart,
                LineEnd,
                DocumentStart,
                DocumentEnd,
                Find,
                Replace,
                Commands,
                History,
            ],
        }
    }
}

/// Resolve a physical command key. Global chords never enter this grammar.
#[must_use]
pub fn command_key(code: Code, modifiers: Modifiers) -> Option<ContextCommand> {
    if modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    use ContextCommand::*;
    Some(match (code, modifiers.shift()) {
        (Code::KeyI, false) => InsertStart,
        (Code::KeyI, true) => InsertLineStart,
        (Code::KeyA, false) => InsertEnd,
        (Code::KeyA, true) => InsertLineEnd,
        (Code::KeyO, false) => OpenBelow,
        (Code::KeyO, true) => OpenAbove,
        (Code::KeyC, false) => Change,
        (Code::KeyD, false) => Delete,
        (Code::KeyD, true) => Cut,
        (Code::KeyY, false) => Copy,
        (Code::KeyP, false) => Paste,
        (Code::KeyU, false) => Undo,
        (Code::KeyU, true) => Redo,
        (Code::KeyX, false) => SelectLine,
        (Code::Semicolon, false) => Collapse,
        (Code::Semicolon, true) => Commands,
        (Code::Slash, false) => Find,
        (Code::Slash, true) => Replace,
        _ => return None,
    })
}

impl std::fmt::Display for ContextCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.title())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_grammar_never_claims_global_chords_or_unknown_keys() {
        assert_eq!(
            command_key(Code::KeyI, Modifiers::empty()),
            Some(ContextCommand::InsertStart)
        );
        assert_eq!(
            command_key(Code::KeyI, Modifiers::SHIFT),
            Some(ContextCommand::InsertLineStart)
        );
        assert_eq!(
            command_key(Code::Slash, Modifiers::SHIFT),
            Some(ContextCommand::Replace)
        );
        for modifier in [Modifiers::CTRL, Modifiers::ALT, Modifiers::LOGO] {
            for code in [Code::KeyI, Code::KeyD, Code::KeyU, Code::Slash] {
                assert_eq!(command_key(code, modifier), None);
            }
            assert_eq!(motion_key(Code::KeyH, modifier), None);
        }
        assert_eq!(command_key(Code::KeyZ, Modifiers::empty()), None);
        assert_eq!(command_key(Code::KeyH, Modifiers::empty()), None);
        assert_eq!(
            motion_key(Code::KeyH, Modifiers::SHIFT),
            Some(crate::keymap::KeyAction::Previous)
        );
    }

    #[test]
    fn word_selection_and_counts_preserve_unicode_graphemes() {
        let text = "café e\u{301}lan 👩‍💻 雪";
        assert_eq!(word_bounds(text, 4), (0, 5)); // inside é
        assert_eq!(word_bounds(text, 7), (6, 12)); // inside combining sequence
        assert_eq!(word_bounds(text, 13), (13, 13)); // emoji is not a word
        assert_eq!(word_bounds(text, usize::MAX), (text.len(), text.len()));
        assert_eq!(selected_characters(text, (0, 5)), 4);
        assert_eq!(selected_characters(text, (12, 6)), 4);
        assert_eq!(selected_characters("👩‍💻", (0, "👩‍💻".len())), 1);
        assert_eq!(selected_characters(text, (1, usize::MAX)), 0);
    }
}
