//! Context Mode's finite command vocabulary (moved verbatim from
//! `flavite-editor/src/context.rs` in FEUX-0002 X02; identifiers unchanged).

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
    ClearSelection,
    /// Drop every member but the primary (FEUX-0002 E-9).
    KeepPrimary,
    /// One caret per member on the adjacent visual row (host-computed).
    AddCaretAbove,
    AddCaretBelow,
    /// The primary advances to the next/previous member.
    RotatePrimary,
    /// Swap anchor and head on every member.
    FlipSelections,
    /// Whitespace-trim each member's edges.
    TrimSelections,
    /// Line-bounded complement (E-9).
    InvertSelections,
    /// Capture one produced character (`f`/`Shift+F`, E-8).
    FindChar { forward: bool },
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
        Self::ClearSelection,
        Self::KeepPrimary,
        Self::AddCaretAbove,
        Self::AddCaretBelow,
        Self::RotatePrimary,
        Self::FlipSelections,
        Self::TrimSelections,
        Self::InvertSelections,
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
            Self::Collapse => "editor.collapse",
            // FEUX-0002 E-7: the delivered ID is reassigned to the single-
            // caret meaning; collapse-to-heads keeps its own ID.
            Self::ClearSelection => "editor.clear_selection",
            Self::KeepPrimary => "editor.keep_primary",
            Self::AddCaretAbove => "editor.add_caret_above",
            Self::AddCaretBelow => "editor.add_caret_below",
            Self::RotatePrimary => "editor.rotate_primary",
            Self::FlipSelections => "editor.flip_selections",
            Self::TrimSelections => "editor.trim_selections",
            Self::InvertSelections => "editor.invert_selections",
            Self::FindChar { .. } => "editor.find_char",
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
            Self::Collapse => "Collapse selections",
            Self::ClearSelection => "Clear selection",
            Self::KeepPrimary => "Keep primary selection",
            Self::AddCaretAbove => "Add caret on row above",
            Self::AddCaretBelow => "Add caret on row below",
            Self::RotatePrimary => "Rotate primary selection",
            Self::FlipSelections => "Flip selections",
            Self::TrimSelections => "Trim selections",
            Self::InvertSelections => "Invert selections",
            Self::FindChar { forward: true } => "Find character forward",
            Self::FindChar { forward: false } => "Find character backward",
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
            Self::FindChar { forward: true } => "KeyF",
            Self::FindChar { forward: false } => "Shift+KeyF",
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
        use ContextCommand as C;
        match self {
            Self::Select => &[
                C::SelectWord,
                C::SelectLine,
                C::SelectAll,
                C::Collapse,
                C::ClearSelection,
                C::KeepPrimary,
                C::AddCaretAbove,
                C::AddCaretBelow,
                C::RotatePrimary,
                C::FlipSelections,
                C::TrimSelections,
                C::InvertSelections,
            ],
            Self::Go => &[C::LineStart, C::LineEnd, C::DocumentStart, C::DocumentEnd],
            Self::Type => &[
                C::InsertStart,
                C::InsertEnd,
                C::InsertLineStart,
                C::InsertLineEnd,
                C::OpenAbove,
                C::OpenBelow,
                C::Change,
            ],
            Self::More => &[
                C::Cut,
                C::Copy,
                C::Paste,
                C::Undo,
                C::Redo,
                C::InsertStart,
                C::InsertEnd,
                C::InsertLineStart,
                C::InsertLineEnd,
                C::OpenAbove,
                C::OpenBelow,
                C::Change,
                C::Delete,
                C::SelectWord,
                C::SelectLine,
                C::SelectAll,
                C::Collapse,
                C::ClearSelection,
                C::KeepPrimary,
                C::AddCaretAbove,
                C::AddCaretBelow,
                C::RotatePrimary,
                C::FlipSelections,
                C::TrimSelections,
                C::InvertSelections,
                C::LineStart,
                C::LineEnd,
                C::DocumentStart,
                C::DocumentEnd,
                C::Find,
                C::Replace,
                C::Commands,
                C::History,
            ],
        }
    }
}

impl std::fmt::Display for ContextCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.title())
    }
}
