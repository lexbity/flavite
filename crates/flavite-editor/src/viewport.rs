//! Viewport state and Unicode-safe cursor helpers.

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

/// Clamp a byte offset to a UTF-8 character boundary.
#[must_use]
pub fn clamp_char_boundary(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while !text.is_char_boundary(at) {
        at = at.saturating_sub(1);
    }
    at
}

/// Previous extended grapheme boundary, including combining and emoji sequences.
#[must_use]
pub fn previous_grapheme_boundary(text: &str, byte: usize) -> usize {
    let byte = clamp_char_boundary(text, byte);
    text.grapheme_indices(true)
        .map(|(index, _)| index)
        .take_while(|index| *index < byte)
        .last()
        .unwrap_or(0)
}

/// Next extended grapheme boundary.
#[must_use]
pub fn next_grapheme_boundary(text: &str, byte: usize) -> usize {
    let byte = clamp_char_boundary(text, byte);
    text.grapheme_indices(true)
        .map(|(index, grapheme)| index + grapheme.len())
        .find(|end| *end > byte)
        .unwrap_or(text.len())
}
