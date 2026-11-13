//! Context Mode's physical-key grammar and Unicode selection helpers
//! (moved from `flavite-editor/src/context.rs` in FEUX-0002 X02, with the
//! key identity abstracted to [`crate::key`]; conversion to native codes is
//! the editor's capture-boundary job).

use crate::key::{KeyCode, Modifiers, Motion};
use crate::vocabulary::{ContextCommand as C, ContextMenu};
use flavite_syntax::text::{TextByte, clamp_char_boundary, prev_grapheme_boundary};
use unicode_segmentation::UnicodeSegmentation;

/// Resolve a physical command key. Global chords never enter this grammar.
#[must_use]
pub fn command_key(code: KeyCode, modifiers: Modifiers) -> Option<C> {
    if modifiers.control || modifiers.alt || modifiers.logo {
        return None;
    }
    Some(match (code, modifiers.shift) {
        (KeyCode::KeyI, false) => C::InsertStart,
        (KeyCode::KeyI, true) => C::InsertLineStart,
        (KeyCode::KeyA, false) => C::InsertEnd,
        (KeyCode::KeyA, true) => C::InsertLineEnd,
        (KeyCode::KeyO, false) => C::OpenBelow,
        (KeyCode::KeyO, true) => C::OpenAbove,
        (KeyCode::KeyC, false) => C::Change,
        (KeyCode::KeyD, false) => C::Delete,
        (KeyCode::KeyD, true) => C::Cut,
        (KeyCode::KeyY, false) => C::Copy,
        (KeyCode::KeyP, false) => C::Paste,
        (KeyCode::KeyU, false) => C::Undo,
        (KeyCode::KeyU, true) => C::Redo,
        (KeyCode::KeyX, false) => C::SelectLine,
        (KeyCode::Semicolon, false) => C::Collapse,
        (KeyCode::Semicolon, true) => C::Commands,
        (KeyCode::Slash, false) => C::Find,
        (KeyCode::Slash, true) => C::Replace,
        (KeyCode::KeyF, false) => C::FindChar { forward: true },
        (KeyCode::KeyF, true) => C::FindChar { forward: false },
        _ => return None,
    })
}

/// Physical selection motions reuse the ordinary acknowledged input path.
#[must_use]
pub fn motion_key(code: KeyCode, modifiers: Modifiers) -> Option<Motion> {
    if modifiers.control || modifiers.alt || modifiers.logo {
        return None;
    }
    Some(match code {
        KeyCode::KeyH => Motion::Previous,
        KeyCode::KeyJ => Motion::Down,
        KeyCode::KeyK => Motion::Up,
        KeyCode::KeyL => Motion::Next,
        KeyCode::KeyW => Motion::NextWord,
        KeyCode::KeyE => Motion::NextWordEnd,
        KeyCode::KeyB => Motion::PreviousWord,
        _ => return None,
    })
}

/// Menus share the same physical grammar for pointer and native routing.
#[must_use]
pub fn menu_key(menu: ContextMenu, code: KeyCode, modifiers: Modifiers) -> Option<C> {
    if modifiers.control || modifiers.alt || modifiers.logo {
        return None;
    }
    let command = match (menu, code) {
        (ContextMenu::Select, KeyCode::KeyW) => C::SelectWord,
        (ContextMenu::Select, KeyCode::KeyX) => C::SelectLine,
        (ContextMenu::Select, KeyCode::KeyA) => C::SelectAll,
        (ContextMenu::Go, KeyCode::KeyH) => C::LineStart,
        (ContextMenu::Go, KeyCode::KeyE) => C::LineEnd,
        (ContextMenu::Go, KeyCode::KeyT) => C::DocumentStart,
        (ContextMenu::Go, KeyCode::KeyB) => C::DocumentEnd,
        _ => command_key(code, modifiers)?,
    };
    menu.commands().contains(&command).then_some(command)
}

/// Unicode word selection shared by right-click and the Select menu.
#[must_use]
pub fn word_bounds(text: &str, at: usize) -> (usize, usize) {
    let at = clamp_grapheme_boundary(text, at);
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

/// Clamp to the nearest grapheme boundary at or before `byte` (moved from
/// `flavite-editor/src/viewport.rs`, which now delegates here).
///
/// # Panics
/// Only if the pinned grapheme cursor reports incomplete context on a fully
/// owned `&str` — treated as a defect, never as input (same contract as the
/// moved implementation).
#[must_use]
pub fn clamp_grapheme_boundary(text: &str, byte: usize) -> usize {
    let byte = clamp_char_boundary(text, byte);
    let mut cursor = unicode_segmentation::GraphemeCursor::new(byte, text.len(), true);
    if cursor
        .is_boundary(text, 0)
        .expect("complete text supplies grapheme context")
    {
        byte
    } else {
        prev_grapheme_boundary(text, TextByte(byte)).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn physical_grammar_never_claims_global_chords_or_unknown_keys() {
        assert_eq!(
            command_key(KeyCode::KeyI, Modifiers::EMPTY),
            Some(C::InsertStart)
        );
        assert_eq!(
            command_key(KeyCode::KeyI, Modifiers::SHIFT),
            Some(C::InsertLineStart)
        );
        assert_eq!(
            command_key(KeyCode::Slash, Modifiers::SHIFT),
            Some(C::Replace)
        );
        for modifier in [Modifiers::CTRL, Modifiers::ALT, Modifiers::LOGO] {
            for code in [KeyCode::KeyI, KeyCode::KeyD, KeyCode::KeyU, KeyCode::Slash] {
                assert_eq!(command_key(code, modifier), None);
            }
            assert_eq!(motion_key(KeyCode::KeyH, modifier), None);
        }
        assert_eq!(command_key(KeyCode::KeyW, Modifiers::EMPTY), None);
        assert_eq!(
            motion_key(KeyCode::KeyH, Modifiers::SHIFT),
            Some(Motion::Previous)
        );
    }

    #[test]
    fn menu_keys_resolve_inside_their_menu_and_fall_through_to_the_root_grammar() {
        assert_eq!(
            menu_key(ContextMenu::Select, KeyCode::KeyW, Modifiers::EMPTY),
            Some(C::SelectWord)
        );
        assert_eq!(
            menu_key(ContextMenu::Go, KeyCode::KeyB, Modifiers::EMPTY),
            Some(C::DocumentEnd)
        );
        // Root keys stay usable inside a menu (delivered fall-through).
        assert_eq!(
            menu_key(ContextMenu::More, KeyCode::KeyD, Modifiers::EMPTY),
            Some(C::Delete)
        );
        assert_eq!(
            menu_key(ContextMenu::Go, KeyCode::KeyW, Modifiers::EMPTY),
            None
        );
        assert_eq!(
            menu_key(ContextMenu::Select, KeyCode::KeyW, Modifiers::CTRL),
            None
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

    #[test]
    fn clamping_lands_on_grapheme_boundaries() {
        let text = "a🧑🏽‍💻e\u{301}";
        assert_eq!(clamp_grapheme_boundary(text, 1), 1);
        assert_eq!(clamp_grapheme_boundary(text, 2), 1); // inside the ZWJ cluster
        assert_eq!(clamp_grapheme_boundary(text, usize::MAX), text.len());
        assert_eq!(clamp_grapheme_boundary("é", 1), 0); // inside a char, clamp to char boundary first
    }
}
