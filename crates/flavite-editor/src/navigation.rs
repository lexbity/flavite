//! Visual-row movement over a bounded source window. No document authority
//! lives here: the host computes each target against its acknowledged text.

use crate::{
    TypeMetrics,
    viewport::clamp_grapheme_boundary,
    widget::{caret_position, shape_text},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VerticalMotion {
    pub rows: i32,
    pub width: f32,
    pub metrics: TypeMetrics,
}

/// Move by wrapped rows, retaining the requested horizontal position through
/// short and empty lines. Shape only adjacent paragraphs, never the full
/// note. `preferred_x` is the member's sticky column (`None` keeps the
/// caret's current visual x); X04 applies this per member.
#[must_use]
pub fn vertical_target(
    text: &str,
    caret: usize,
    motion: VerticalMotion,
    preferred_x: Option<f32>,
) -> (usize, f32) {
    let caret = clamp_grapheme_boundary(text, caret);
    let mut start = text[..caret].rfind('\n').map_or(0, |at| at + 1);
    let mut end = text[caret..].find('\n').map_or(text.len(), |at| caret + at);
    for _ in 0..motion.rows.unsigned_abs() {
        if motion.rows < 0 && start > 0 {
            start = text[..start - 1].rfind('\n').map_or(0, |at| at + 1);
        } else if motion.rows > 0 && end < text.len() {
            end = text[end + 1..]
                .find('\n')
                .map_or(text.len(), |at| end + 1 + at);
        } else {
            break;
        }
    }
    let visible = &text[start..end];
    let buffer = shape_text(visible, motion.width.max(1.0), motion.metrics);
    let runs: Vec<_> = buffer.layout_runs().collect();
    let (x, y) = caret_position(&runs, visible, caret - start);
    let preferred_x = preferred_x.unwrap_or(x);
    let last_y = runs.last().map_or(0.0, |run| run.line_top);
    let target_y = (y + motion.rows as f32 * motion.metrics.line_height).clamp(0.0, last_y);
    if target_y == y {
        return (caret, preferred_x);
    }
    let target = buffer
        .hit(preferred_x, target_y + motion.metrics.line_height * 0.5)
        .map_or(caret, |cursor| {
            start
                + visible
                    .split_inclusive('\n')
                    .take(cursor.line)
                    .map(str::len)
                    .sum::<usize>()
                + cursor.index
        });
    (clamp_grapheme_boundary(text, target), preferred_x)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn motion(rows: i32, width: f32) -> VerticalMotion {
        VerticalMotion {
            rows,
            width,
            metrics: TypeMetrics::default(),
        }
    }

    #[test]
    fn vertical_column_survives_short_and_empty_lines() {
        let text = "abcdef\nx\n\nabcdef\n";
        let (short, x) = vertical_target(text, 5, motion(1, 500.0), None);
        assert_eq!(short, 8);
        let (empty, _) = vertical_target(text, short, motion(1, 500.0), Some(x));
        assert_eq!(empty, 9);
        let (long, _) = vertical_target(text, empty, motion(1, 500.0), Some(x));
        assert_eq!(long, 15);
        let (back, _) = vertical_target(text, long, motion(-3, 500.0), Some(x));
        assert_eq!(back, 5);
    }

    #[test]
    fn arrows_move_through_wrapped_rows_and_across_paragraphs() {
        let text = "abcdefghijabcdefghijabcdefghij\nsecond";
        let buffer = shape_text(text, 100.0, TypeMetrics::default());
        let runs: Vec<_> = buffer.layout_runs().collect();
        assert!(runs.len() >= 4);
        let (next, x) = vertical_target(text, 2, motion(1, 100.0), None);
        let (_, y) = caret_position(&runs, text, next);
        assert_eq!(y, runs[1].line_top);
        assert_eq!(vertical_target(text, next, motion(-1, 100.0), Some(x)).0, 2);
        let last = text.find('\n').unwrap();
        let (below, _) = vertical_target(text, last, motion(1, 100.0), None);
        assert!(below > last);
    }

    #[test]
    fn document_edges_and_unicode_are_safe() {
        use unicode_segmentation::UnicodeSegmentation;
        let text = "e\u{301} 🧑🏽‍💻\n雪 e\u{301}\n";
        let boundaries: Vec<_> = text
            .grapheme_indices(true)
            .map(|(at, _)| at)
            .chain([text.len()])
            .collect();
        for &at in &boundaries {
            for rows in [-4, -1, 1, 4] {
                let (target, _) = vertical_target(text, at, motion(rows, 70.0), None);
                assert!(boundaries.contains(&target), "{at} → {target}");
            }
        }
        assert_eq!(vertical_target(text, 0, motion(-1, 70.0), None).0, 0);
        assert_eq!(
            vertical_target(text, text.len(), motion(1, 70.0), None).0,
            text.len()
        );
    }
}
