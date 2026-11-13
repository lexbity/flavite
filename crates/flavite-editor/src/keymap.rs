//! Platform-neutral actions for the custom editor.

use iced::keyboard::{Key, Modifiers, key::Named};

/// Commands recognized by the editor keymap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Insert the event's text at the caret/selection.
    Insert,
    /// Move to previous grapheme.
    Previous,
    /// Move to next grapheme.
    Next,
    /// Move to the previous word boundary.
    PreviousWord,
    /// Move to the next word boundary.
    NextWord,
    /// Move to the end of the current or next word (the `e` motion, X05).
    NextWordEnd,
    /// Move up one visual row.
    Up,
    /// Move down one visual row.
    Down,
    /// Move up one viewport.
    PageUp,
    /// Move down one viewport.
    PageDown,
    /// Delete to the previous word boundary.
    BackspaceWord,
    /// Delete to the next word boundary.
    DeleteWord,
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

/// Decode one platform key event into the editor's keymap actions.
#[must_use]
pub fn key_action(key: &Key, modifiers: Modifiers, text: Option<&str>) -> KeyAction {
    // Alt on macOS and Ctrl elsewhere are word motions. Command remains
    // available for the document/application command bindings.
    let word = if cfg!(target_os = "macos") {
        modifiers.alt() && !modifiers.control() && !modifiers.command()
    } else {
        modifiers.control() && !modifiers.alt() && !modifiers.logo()
    };
    if word {
        match key {
            Key::Named(Named::ArrowLeft) => return KeyAction::PreviousWord,
            Key::Named(Named::ArrowRight) => return KeyAction::NextWord,
            Key::Named(Named::Backspace) => return KeyAction::BackspaceWord,
            Key::Named(Named::Delete) => return KeyAction::DeleteWord,
            _ => {}
        }
    }
    if modifiers.command() || modifiers.control() {
        return match key {
            Key::Named(Named::Home) => KeyAction::Home,
            Key::Named(Named::End) => KeyAction::End,
            Key::Character(c) if c.eq_ignore_ascii_case("a") => KeyAction::SelectAll,
            Key::Character(c) if c.eq_ignore_ascii_case("c") => KeyAction::Copy,
            Key::Character(c) if c.eq_ignore_ascii_case("x") => KeyAction::Cut,
            Key::Character(c) if c.eq_ignore_ascii_case("v") => KeyAction::Paste,
            Key::Character(c) if c.eq_ignore_ascii_case("z") && modifiers.shift() => {
                KeyAction::Redo
            }
            Key::Character(c) if c.eq_ignore_ascii_case("z") => KeyAction::Undo,
            Key::Character(c) if c.eq_ignore_ascii_case("y") => KeyAction::Redo,
            _ => KeyAction::Ignore,
        };
    }
    match key {
        Key::Named(Named::ArrowUp) => KeyAction::Up,
        Key::Named(Named::ArrowDown) => KeyAction::Down,
        Key::Named(Named::PageUp) => KeyAction::PageUp,
        Key::Named(Named::PageDown) => KeyAction::PageDown,
        Key::Named(Named::ArrowLeft) => KeyAction::Previous,
        Key::Named(Named::ArrowRight) => KeyAction::Next,
        Key::Named(Named::Home) => KeyAction::Home,
        Key::Named(Named::End) => KeyAction::End,
        Key::Named(Named::Backspace) => KeyAction::Backspace,
        Key::Named(Named::Delete) => KeyAction::Delete,
        Key::Named(Named::Enter) => KeyAction::Newline,
        _ if text
            .is_some_and(|value| !value.is_empty() && !value.chars().all(char::is_control)) =>
        {
            KeyAction::Insert
        }
        _ => KeyAction::Ignore,
    }
}
