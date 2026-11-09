//! The source range affected by a selection-first command. Rendering and
//! execution ask the same planner; previewing never changes live selection.

use super::ContextCommand;
use std::ops::Range;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEffect {
    pub range: Range<usize>,
    pub implicit: bool,
    pub unit: EffectUnit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectUnit {
    Selection,
    Character,
    Line,
}

impl CommandEffect {
    #[must_use]
    pub fn accessible_description(&self) -> &'static str {
        match self.unit {
            EffectUnit::Selection => "Acts on the selected text.",
            EffectUnit::Character if self.range.is_empty() => {
                "This empty line has no character to remove."
            }
            EffectUnit::Character => "Acts on one whole character on this line.",
            EffectUnit::Line if self.range.is_empty() => {
                "This empty last line has no text or line break to copy or cut."
            }
            EffectUnit::Line => "Acts on the current line, including its line break when present.",
        }
    }

    #[must_use]
    pub fn description(&self, command: ContextCommand) -> String {
        if self.range.is_empty() {
            return match command {
                ContextCommand::Change => "Type on this empty line",
                ContextCommand::Copy => "No text to copy on this last line",
                ContextCommand::Cut => "No text to cut on this last line",
                _ => "No character to delete on this line",
            }
            .into();
        }
        let verb = match command {
            ContextCommand::Copy => "Copy",
            ContextCommand::Cut => "Cut",
            ContextCommand::Change => "Change",
            _ => "Delete",
        };
        let target = match self.unit {
            EffectUnit::Selection => "selection",
            EffectUnit::Character => "outlined character",
            EffectUnit::Line => "outlined line",
        };
        format!("{verb} {target}")
    }
}

/// Callers supply source coordinates, never display columns. Invalid endpoints
/// fail rather than silently retargeting a destructive command.
#[must_use]
pub fn plan(
    text: &str,
    selection: (usize, usize),
    command: ContextCommand,
) -> Option<CommandEffect> {
    use ContextCommand::{Change, Copy, Cut, Delete};
    if !matches!(command, Copy | Cut | Delete | Change) {
        return None;
    }
    for endpoint in [selection.0, selection.1] {
        if endpoint > text.len()
            || crate::viewport::clamp_grapheme_boundary(text, endpoint) != endpoint
        {
            return None;
        }
    }
    let start = selection.0.min(selection.1);
    let end = selection.0.max(selection.1);
    if start != end {
        return Some(CommandEffect {
            range: start..end,
            implicit: false,
            unit: EffectUnit::Selection,
        });
    }
    let line_start = text[..start].rfind('\n').map_or(0, |at| at + 1);
    let line_end = text[start..].find('\n').map_or(text.len(), |at| start + at);
    let (range, unit) = if matches!(command, Copy | Cut) {
        (
            line_start..(line_end + usize::from(line_end < text.len())),
            EffectUnit::Line,
        )
    } else if start < line_end {
        (
            start..crate::viewport::next_grapheme_boundary(text, flavite_syntax::TextByte(start)).0,
            EffectUnit::Character,
        )
    } else if start > line_start {
        (
            crate::viewport::prev_grapheme_boundary(text, flavite_syntax::TextByte(start)).0..start,
            EffectUnit::Character,
        )
    } else {
        (start..start, EffectUnit::Character)
    };
    Some(CommandEffect {
        range,
        implicit: true,
        unit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;

    #[test]
    fn implicit_ranges_preserve_graphemes_and_never_delete_a_line_break() {
        for text in ["", "\n", "a\n\n雪", "👩‍💻e\u{301}\n", "a\rb\n"] {
            for at in text
                .grapheme_indices(true)
                .map(|(at, _)| at)
                .chain([text.len()])
            {
                let effect = plan(text, (at, at), ContextCommand::Delete).unwrap();
                let selected = &text[effect.range.clone()];
                assert!(!selected.contains('\n'));
                assert!(selected.graphemes(true).count() <= 1);
                assert_eq!(
                    effect,
                    plan(text, (at, at), ContextCommand::Change).unwrap()
                );
                assert_eq!(
                    plan(text, (at, at), ContextCommand::Copy),
                    plan(text, (at, at), ContextCommand::Cut)
                );
            }
        }
        assert_eq!(
            plan("雪\n", (3, 3), ContextCommand::Delete).unwrap().range,
            0..3
        );
        assert_eq!(
            plan("a\n\nb", (2, 2), ContextCommand::Delete)
                .unwrap()
                .range,
            2..2
        );
        assert_eq!(
            plan("a\n\nb", (2, 2), ContextCommand::Cut).unwrap().range,
            2..3
        );
    }

    #[test]
    fn explicit_ranges_win_and_invalid_offsets_do_not_round_into_other_text() {
        let text = "e\u{301}\n雪";
        let effect = plan(text, (text.len(), 0), ContextCommand::Cut).unwrap();
        assert_eq!(effect.range, 0..text.len());
        assert!(!effect.implicit);
        for endpoint in [1, 2, text.len() + 1, usize::MAX] {
            assert!(plan(text, (endpoint, endpoint), ContextCommand::Delete).is_none());
        }
        assert!(plan(text, (0, 0), ContextCommand::Undo).is_none());
    }
}
