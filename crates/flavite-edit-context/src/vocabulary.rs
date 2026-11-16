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
    FindChar {
        forward: bool,
    },
    /// Document-wide match selection (E-10): preview overlay + Apply.
    SelectMatches,
    /// Document-wide gap selection between matches (E-10).
    SplitSelection,
    LineStart,
    LineEnd,
    DocumentStart,
    DocumentEnd,
    // FEUX-0004 X05: Repeat last command
    /// Replay the last recorded command against the current selection set.
    RepeatLast,
    // FEUX-0004 X01: Transform family
    /// Uppercase each character in each selection.
    CaseUpper,
    /// Lowercase each character in each selection.
    CaseLower,
    /// Swap case of each character in each selection.
    CaseSwap,
    /// Title-case each word in each selection (UAX #29 word boundaries).
    CaseTitle,
    /// Increment the first number in each selection (leading-zero width preserved).
    NumberInc,
    /// Decrement the first number in each selection (leading-zero width preserved).
    NumberDec,
    /// Replace each selection's number with its document-order index (1-based).
    NumberSequence,
    // FEUX-0004 X03: Align command
    /// Align lines in each selection's containing line block on a literal delimiter.
    LinesAlign,
    // FEUX-0004 X03: Goto-line command
    /// Go to line N (opens draft if no count pending).
    GotoLine,
    // FEUX-0004 X01: Lines family
    /// Sort lines in each selection's containing line block (byte-lexicographic, stable).
    LinesSortAsc,
    /// Sort lines in each selection's containing line block (inverse byte-lexicographic, stable).
    LinesSortDesc,
    /// Sort lines in each selection's containing line block by numeric value (stable).
    LinesSortNumeric,
    /// Join lines in each selection's containing line block (replace newlines with single space).
    LinesJoin,
    /// Duplicate each selection's containing line block.
    LinesDuplicate,
    /// Remove duplicate lines in each selection's containing line block (byte-exact, first wins).
    LinesDedupe,
    /// Shuffle lines in each selection's containing line block (Fisher-Yates, splitmix64 seeded).
    LinesShuffle,
    /// Move each selection's containing line block up by one position.
    LinesMoveUp,
    /// Move each selection's containing line block down by one position.
    LinesMoveDown,
    // FEUX-0004 X04: Surround family
    /// Add surrounding delimiters to each selection.
    SurroundAdd,
    /// Change surrounding delimiters of each selection.
    SurroundChange,
    /// Delete surrounding delimiters from each selection.
    SurroundDelete,
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
        Self::SelectMatches,
        Self::SplitSelection,
        Self::LineStart,
        Self::LineEnd,
        Self::DocumentStart,
        Self::DocumentEnd,
        // FEUX-0004 X05: Repeat last command
        Self::RepeatLast,
        // FEUX-0004 X01: Transform family
        Self::CaseUpper,
        Self::CaseLower,
        Self::CaseSwap,
        Self::CaseTitle,
        Self::NumberInc,
        Self::NumberDec,
        Self::NumberSequence,
        // FEUX-0004 X03: Align and Goto-line
        Self::LinesAlign,
        Self::GotoLine,
        // FEUX-0004 X01: Lines family
        Self::LinesSortAsc,
        Self::LinesSortDesc,
        Self::LinesSortNumeric,
        Self::LinesJoin,
        Self::LinesDuplicate,
        Self::LinesDedupe,
        Self::LinesShuffle,
        Self::LinesMoveUp,
        Self::LinesMoveDown,
        // FEUX-0004 X04: Surround family
        Self::SurroundAdd,
        Self::SurroundChange,
        Self::SurroundDelete,
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
            Self::SelectMatches => "editor.select_matches",
            Self::SplitSelection => "editor.split_selection",
            Self::FindChar { .. } => "editor.find_char",
            Self::LineStart => "workspace.caret_start_line",
            Self::LineEnd => "workspace.caret_end_line",
            Self::DocumentStart => "workspace.caret_start",
            Self::DocumentEnd => "workspace.caret_end",
            // FEUX-0004 X05: Repeat last command
            Self::RepeatLast => "editor.repeat_last",
            // FEUX-0004 X01: Transform family
            Self::CaseUpper => "editor.case_upper",
            Self::CaseLower => "editor.case_lower",
            Self::CaseSwap => "editor.case_swap",
            Self::CaseTitle => "editor.case_title",
            Self::NumberInc => "editor.number_inc",
            Self::NumberDec => "editor.number_dec",
            Self::NumberSequence => "editor.number_sequence",
            // FEUX-0004 X03: Align and Goto-line
            Self::LinesAlign => "editor.lines_align",
            Self::GotoLine => "editor.goto_line",
            // FEUX-0004 X01: Lines family
            Self::LinesSortAsc => "editor.lines_sort_asc",
            Self::LinesSortDesc => "editor.lines_sort_desc",
            Self::LinesSortNumeric => "editor.lines_sort_numeric",
            Self::LinesJoin => "editor.lines_join",
            Self::LinesDuplicate => "editor.lines_duplicate",
            Self::LinesDedupe => "editor.lines_dedupe",
            Self::LinesShuffle => "editor.lines_shuffle",
            Self::LinesMoveUp => "editor.lines_move_up",
            Self::LinesMoveDown => "editor.lines_move_down",
            // FEUX-0004 X04: Surround family
            Self::SurroundAdd => "editor.surround_add",
            Self::SurroundChange => "editor.surround_change",
            Self::SurroundDelete => "editor.surround_delete",
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
            Self::SelectMatches => "Select matches…",
            Self::SplitSelection => "Split on matches…",
            Self::FindChar { forward: true } => "Find character forward",
            Self::FindChar { forward: false } => "Find character backward",
            Self::LineStart => "Line start",
            Self::LineEnd => "Line end",
            Self::DocumentStart => "Document start",
            Self::DocumentEnd => "Document end",
            // FEUX-0004 X05: Repeat last command
            Self::RepeatLast => "Repeat last",
            // FEUX-0004 X01: Transform family
            Self::CaseUpper => "Uppercase",
            Self::CaseLower => "Lowercase",
            Self::CaseSwap => "Swap case",
            Self::CaseTitle => "Title case",
            Self::NumberInc => "Increment number",
            Self::NumberDec => "Decrement number",
            Self::NumberSequence => "Number sequence",
            // FEUX-0004 X03: Align and Goto-line
            Self::LinesAlign => "Align lines",
            Self::GotoLine => "Go to line…",
            // FEUX-0004 X01: Lines family
            Self::LinesSortAsc => "Sort lines ascending",
            Self::LinesSortDesc => "Sort lines descending",
            Self::LinesSortNumeric => "Sort lines numeric",
            Self::LinesJoin => "Join lines",
            Self::LinesDuplicate => "Duplicate lines",
            Self::LinesDedupe => "Deduplicate lines",
            Self::LinesShuffle => "Shuffle lines",
            Self::LinesMoveUp => "Move lines up",
            Self::LinesMoveDown => "Move lines down",
            // FEUX-0004 X04: Surround family
            Self::SurroundAdd => "Surround add",
            Self::SurroundChange => "Surround change",
            Self::SurroundDelete => "Surround delete",
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
                // FEUX-0004 X01: Transform family - all mutate
                | Self::CaseUpper
                | Self::CaseLower
                | Self::CaseSwap
                | Self::CaseTitle
                | Self::NumberInc
                | Self::NumberDec
                | Self::NumberSequence
                // FEUX-0004 X03: Align mutates
                | Self::LinesAlign
                // FEUX-0004 X03: GotoLine does NOT mutate (selection-only)
                // FEUX-0004 X01: Lines family - all mutate
                | Self::LinesSortAsc
                | Self::LinesSortDesc
                | Self::LinesSortNumeric
                | Self::LinesJoin
                | Self::LinesDuplicate
                | Self::LinesDedupe
                | Self::LinesShuffle
                | Self::LinesMoveUp
                | Self::LinesMoveDown
                // FEUX-0004 X04: Surround family - all mutate
                | Self::SurroundAdd
                | Self::SurroundChange
                | Self::SurroundDelete
                // FEUX-0004 X05: RepeatLast may mutate (replays a command that may mutate)
                | Self::RepeatLast
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
}

/// A toolbar menu is a flat list; commands are not duplicated as aliases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ContextMenu {
    Select,
    Go,
    More,
    // FEUX-0004 X01: new menus
    Transform,
    Lines,
    // FEUX-0004 X04: Surround menu
    Surround,
}

impl ContextMenu {
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Select => "Select",
            Self::Go => "Go",
            Self::More => "More",
            Self::Transform => "Transform",
            Self::Lines => "Lines",
            Self::Surround => "Surround",
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
                C::SelectMatches,
                C::SplitSelection,
            ],
            Self::Go => &[
                C::LineStart,
                C::LineEnd,
                C::DocumentStart,
                C::DocumentEnd,
                C::GotoLine,
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
                C::SelectMatches,
                C::SplitSelection,
                C::LineStart,
                C::LineEnd,
                C::DocumentStart,
                C::DocumentEnd,
                C::Find,
                C::Replace,
                C::Commands,
                C::History,
            ],
            // FEUX-0004 X01: Transform menu
            Self::Transform => &[
                C::CaseUpper,
                C::CaseLower,
                C::CaseSwap,
                C::CaseTitle,
                C::NumberInc,
                C::NumberDec,
                C::NumberSequence,
            ],
            // FEUX-0004 X01/X03: Lines menu
            Self::Lines => &[
                C::LinesSortAsc,
                C::LinesSortDesc,
                C::LinesSortNumeric,
                C::LinesJoin,
                C::LinesDuplicate,
                C::LinesDedupe,
                C::LinesShuffle,
                C::LinesMoveUp,
                C::LinesMoveDown,
                C::LinesAlign,
            ],
            // FEUX-0004 X04: Surround menu (E-2)
            Self::Surround => &[
                C::SurroundAdd,
                C::SurroundChange,
                C::SurroundDelete,
            ],
        }
    }
}

impl std::fmt::Display for ContextCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.title())
    }
}
