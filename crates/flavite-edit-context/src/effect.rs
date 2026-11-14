//! The source range affected by a selection-first command. Rendering and
//! execution ask the same planner; previewing never changes live selection.
//! (Moved from `flavite-editor/src/context/effect.rs` in FEUX-0002 X02.)

use crate::vocabulary::ContextCommand;
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
            || crate::grammar::clamp_grapheme_boundary(text, endpoint) != endpoint
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
    let (range, unit) = if matches!(command, Copy | Cut) {
        (
            flavite_buffer::commands::line_range(text, start)?,
            EffectUnit::Line,
        )
    } else {
        (
            flavite_buffer::commands::delete_range(text, start)
                .map_or(start..start, |(from, to)| from..to),
            EffectUnit::Character,
        )
    };
    Some(CommandEffect {
        range,
        implicit: true,
        unit,
    })
}

/// Implicit previews use the same ranges as editing and draw duplicate
/// containing lines once. Callers pass only their visible canonical window.
#[must_use]
pub fn implicit_ranges(
    text: &str,
    members: &[flavite_buffer::selection::Selection],
    command: ContextCommand,
) -> Vec<Range<usize>> {
    let mut result = Vec::new();
    let mut line_cache: Option<Range<usize>> = None;
    for member in members.iter().filter(|member| member.is_caret()) {
        let range = if matches!(command, ContextCommand::Copy | ContextCommand::Cut)
            && let Some(range) = line_cache
                .as_ref()
                .filter(|range| range.contains(&member.head()))
        {
            range.clone()
        } else {
            let Some(effect) = plan(text, (member.anchor(), member.head()), command)
                .filter(|effect| effect.implicit && !effect.range.is_empty())
            else {
                continue;
            };
            if effect.unit == EffectUnit::Line {
                line_cache = Some(effect.range.clone());
            }
            effect.range
        };
        if result.last() != Some(&range) {
            result.push(range);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_segmentation::UnicodeSegmentation;

    #[test]
    fn duplicate_line_previews_use_one_effect_for_many_carets() {
        let text = "x".repeat(10_000);
        let raw = (0..10_000)
            .map(|index| flavite_buffer::selection::RawSelection {
                id: Some(u32::try_from(index).unwrap()),
                anchor: index,
                head: index,
            })
            .collect();
        let (set, _) =
            flavite_buffer::selection::SelectionSet::normalize(&text, raw, Some(0)).unwrap();
        assert_eq!(
            implicit_ranges(&text, set.members(), ContextCommand::Cut),
            vec![0..10_000]
        );
        assert_eq!(
            implicit_ranges(&text, set.members(), ContextCommand::Copy),
            vec![0..10_000]
        );
    }

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
