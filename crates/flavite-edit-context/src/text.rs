//! Unicode text helpers for Context Mode (FEUX-0003 X01).
//!
//! These functions were previously in `grammar.rs` and are moved here to
//! separate the text manipulation concerns from the key binding grammar.
//!
//! The grammar module (`grammar.rs`) is being deprecated in favor of the
//! data-driven keymap approach; these text helpers have no dependency on
//! the grammar and belong in their own module.

use flavite_syntax::text::{TextByte, clamp_char_boundary, prev_grapheme_boundary};
use unicode_segmentation::UnicodeSegmentation;

/// Unicode word selection shared by right-click and the Select menu.
///
/// Returns the (start, end) byte indices of the word containing the cursor
/// at position `at`. If `at` is not inside a word, returns `(at, at)`.
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
    fn word_selection_preserves_unicode_graphemes() {
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
