//! Viewport state and Unicode-safe cursor helpers.

/// Canonical shared boundary primitives (RFC-0006 §4.1): the editor's caret
/// movement, deletion, and clamping all use the pinned syntax segmentation.
pub use flavite_syntax::text::{
    clamp_char_boundary, next_grapheme_boundary, next_word_boundary, prev_grapheme_boundary,
    prev_word_boundary,
};

use unicode_segmentation::UnicodeSegmentation;

/// Scroll and visible-line state for the editor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Vertical scroll offset in logical pixels.
    pub scroll_y: f32,
    /// First visible logical line.
    pub first_line: usize,
    /// Number of visible logical lines.
    pub visible_lines: usize,
    /// Line height in logical pixels.
    pub line_height: f32,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            scroll_y: 0.0,
            first_line: 0,
            visible_lines: 1,
            line_height: 22.0,
        }
    }
}

impl Viewport {
    /// Update the visible line range for a viewport height.
    pub fn set_height(&mut self, height: f32) {
        self.visible_lines = (height / self.line_height).ceil().max(1.0) as usize + 1;
    }

    /// Visible line indices, clamped to the document's logical line count.
    #[must_use]
    pub fn visible_range(&self, line_count: usize) -> std::ops::Range<usize> {
        let start = self.first_line.min(line_count);
        start..start.saturating_add(self.visible_lines).min(line_count)
    }
}

/// Selectable characters for native text adapters. AccessKit stores each
/// character's byte length in a u8. Extremely long combining sequences use
/// scalar-sized segments in the projection; input admission still refuses
/// a caret inside the original grapheme.
#[must_use]
pub fn accessible_characters(text: &str) -> Vec<std::ops::Range<usize>> {
    let mut ranges = Vec::new();
    for (start, grapheme) in text.grapheme_indices(true) {
        if grapheme.len() <= usize::from(u8::MAX) {
            ranges.push(start..start + grapheme.len());
        } else {
            ranges.extend(
                grapheme
                    .char_indices()
                    .map(|(i, c)| start + i..start + i + c.len_utf8()),
            );
        }
    }
    ranges
}

/// AccessKit's character byte lengths without allocating an intermediate
/// range for every character. ASCII's only multi-byte grapheme is CRLF.
#[must_use]
pub fn accessible_character_lengths(text: &str) -> Vec<u8> {
    if text.is_ascii() {
        let mut lengths = vec![1; text.len()];
        if text.contains("\r\n") {
            lengths.clear();
            let mut bytes = text.bytes().peekable();
            while let Some(byte) = bytes.next() {
                if byte == b'\r' && bytes.peek() == Some(&b'\n') {
                    bytes.next();
                    lengths.push(2);
                } else {
                    lengths.push(1);
                }
            }
        }
        lengths
    } else {
        accessible_characters(text)
            .into_iter()
            .map(|range| u8::try_from(range.len()).expect("accessible character fits in u8"))
            .collect()
    }
}

/// Clamp shaped glyph offsets to an extended grapheme boundary. Shapers can
/// expose scalar offsets inside combining/emoji sequences.
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
        prev_grapheme_boundary(text, flavite_syntax::TextByte(byte)).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexported_boundaries_move_and_clamp_like_the_syntax_primitives() {
        let text = "a🧑🏽‍💻e\u{301}";
        assert_eq!(
            prev_grapheme_boundary(text, flavite_syntax::TextByte(text.len())).0,
            "a🧑🏽‍💻".len()
        );
        assert_eq!(
            next_grapheme_boundary(text, flavite_syntax::TextByte(1)).0,
            "a🧑🏽‍💻".len()
        );
        assert_eq!(clamp_char_boundary("é", 1), 0);
        assert_eq!(
            prev_word_boundary("one two", flavite_syntax::TextByte(4)).0,
            0
        );
        assert_eq!(
            next_word_boundary("one two", flavite_syntax::TextByte(0)).0,
            3
        );
    }

    #[test]
    fn native_character_lengths_match_unicode_segmentation() {
        let huge_cluster = format!("a{}", "\u{301}".repeat(300));
        for text in [
            "",
            "ordinary ASCII\n",
            "\r\n\r\n",
            "a\rb\nc\r\n",
            "é e\u{301} 🧑🏽‍💻\n",
            &huge_cluster,
        ] {
            let expected: Vec<_> = accessible_characters(text)
                .into_iter()
                .map(|range| u8::try_from(range.len()).unwrap())
                .collect();
            assert_eq!(accessible_character_lengths(text), expected, "{text:?}");
        }
    }
}
