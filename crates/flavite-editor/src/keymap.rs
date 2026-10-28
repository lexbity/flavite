//! Platform-neutral actions for the custom editor.

/// Commands recognized by the editor keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Insert the event's text at the caret/selection.
    Insert,
    /// Move to previous grapheme.
    Previous,
    /// Move to next grapheme.
    Next,
    /// Move to start of document.
    Home,
    /// Move to end of document.
    End,
    /// Delete the previous grapheme.
    Backspace,
    /// Delete the next grapheme.
    Delete,
    /// Request undo from the Session.
    Undo,
    /// Request redo from the Session.
    Redo,
    /// Copy selected text.
    Copy,
    /// Cut selected text.
    Cut,
    /// Paste plain text from iced's text-only clipboard.
    Paste,
    /// Select the full note.
    SelectAll,
    /// Insert a line break.
    Newline,
    /// No editor action.
    Ignore,
}
