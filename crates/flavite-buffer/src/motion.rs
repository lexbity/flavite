//! Typed per-member motions (FEUX-0002 X05, §6 `editor.move`).
//!
//! The single motion implementation for the codebase: keyboard motions, the
//! api `editor.move` executor, and tests all apply one of these motions per
//! member through [`apply`]. Visual motions (line up/down, page up/down)
//! need viewport geometry that only the editor owns, so their per-member
//! target bytes are supplied by the caller and only validated here. Word
//! movements share the pinned UAX-29 movement policy of
//! `flavite_syntax::text` — no second matcher or segmenter lives here.

use crate::selection::{RawSelection, SelectionSet, SetError};
use flavite_syntax::text::{TextByte, next_word_boundary, prev_word_boundary};
use unicode_segmentation::UnicodeSegmentation;

/// One motion applied per member (§6 `editor.move`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    CharLeft,
    CharRight,
    LineUp,
    LineDown,
    WordNext,
    WordPrev,
    WordEnd,
    LineStart,
    LineEnd,
    DocStart,
    DocEnd,
    PageUp,
    PageDown,
}

impl Motion {
    /// Whether this motion needs editor-computed per-member target bytes.
    #[must_use]
    pub const fn is_visual(self) -> bool {
        matches!(
            self,
            Self::LineUp | Self::LineDown | Self::PageUp | Self::PageDown
        )
    }
}

/// Typed motion refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MotionError {
    /// The set is invalid against the text.
    Set(SetError),
    /// A visual motion was applied without exactly one target per member.
    MissingTargets,
}

impl From<SetError> for MotionError {
    fn from(error: SetError) -> Self {
        Self::Set(error)
    }
}

impl std::fmt::Display for MotionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Set(error) => write!(f, "{error}"),
            Self::MissingTargets => {
                write!(f, "visual motions need one target position per selection")
            }
        }
    }
}

impl std::error::Error for MotionError {}

/// Apply `motion` to every member (`extend` keeps each anchor, otherwise
/// each member collapses onto its new head). `targets` supplies the per-
/// member head bytes for visual motions (editor-computed from the
/// revision-bound layout; already sticky-column resolved). Members that map
/// onto the same point merge through normalization, and the primary
/// identity survives.
///
/// # Errors
///
/// [`MotionError::MissingTargets`] for a visual motion without exactly one
/// target per member; [`MotionError::Set`] on invalid sets/endpoints.
pub fn apply(
    text: &str,
    set: &SelectionSet,
    motion: Motion,
    extend: bool,
    targets: Option<&[usize]>,
) -> Result<SelectionSet, MotionError> {
    set.validate_against(text)?;
    let targets = if motion.is_visual() {
        match targets {
            Some(targets) if targets.len() == set.len() => targets,
            _ => return Err(MotionError::MissingTargets),
        }
    } else {
        &[]
    };
    let mut candidates = Vec::with_capacity(set.len());
    let mut primary_hint = None;
    for (index, member) in set.iter().enumerate() {
        let head = if motion.is_visual() {
            targets[index]
        } else {
            head_target(text, member, motion, extend)
        };
        if head > text.len() || !crate::selection::is_grapheme_boundary(text, head) {
            return Err(SetError::OutOfBounds { at: head }.into());
        }
        let anchor = if extend { member.anchor() } else { head };
        if member.id() == set.primary_id() {
            primary_hint = Some(member.id().as_raw());
        }
        candidates.push(RawSelection {
            id: Some(member.id().as_raw()),
            anchor,
            head,
        });
    }
    let (moved, _) = SelectionSet::normalize(text, candidates, primary_hint)?;
    Ok(moved)
}

/// The non-visual head target for one member (delivered ordinary-motion
/// semantics: a non-extended range collapses inward first; horizontal moves
/// are grapheme-based, word moves share the UAX-29 movement policy).
fn head_target(
    text: &str,
    member: &crate::selection::Selection,
    motion: Motion,
    extend: bool,
) -> usize {
    let head = member.head();
    let previous = |at: usize| flavite_syntax::text::prev_grapheme_boundary(text, TextByte(at)).0;
    let next = |at: usize| flavite_syntax::text::next_grapheme_boundary(text, TextByte(at)).0;
    let collapses = !member.is_caret() && !extend;
    match motion {
        Motion::CharLeft => {
            if collapses {
                member.start()
            } else {
                previous(head)
            }
        }
        Motion::CharRight => {
            if collapses {
                member.end()
            } else {
                next(head)
            }
        }
        Motion::WordPrev => prev_word_boundary(text, TextByte(head)).0,
        Motion::WordNext => next_word_boundary(text, TextByte(head)).0,
        Motion::WordEnd => word_end(text, head),
        Motion::LineStart => text[..head].rfind('\n').map_or(0, |at| at + 1),
        Motion::LineEnd => text[head..].find('\n').map_or(text.len(), |at| head + at),
        Motion::DocStart => 0,
        Motion::DocEnd => text.len(),
        Motion::LineUp | Motion::LineDown | Motion::PageUp | Motion::PageDown => {
            unreachable!("visual motions take caller-supplied targets and never reach head_target")
        }
    }
}

/// The end of the UAX-29 word segment containing `at`, or of the next one.
/// A caret strictly inside a segment lands on that segment's end (the `e`
/// motion); one on a segment boundary — or between words — lands on the
/// next word segment's end (the pinned D-04 movement policy).
fn word_end(text: &str, at: usize) -> usize {
    let mut cursor = 0usize;
    for segment in text.split_word_bounds() {
        let start = cursor;
        cursor += segment.len();
        if start <= at && at < cursor && (at > start || has_word_char(segment)) {
            return cursor;
        }
    }
    next_word_boundary(text, TextByte(at)).0
}

fn has_word_char(segment: &str) -> bool {
    segment.chars().any(char::is_alphanumeric)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::RawSelection;

    fn set_of(text: &str, at: usize) -> SelectionSet {
        SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: at,
                head: at,
            }],
            None,
        )
        .unwrap()
        .0
    }

    #[test]
    fn char_motions_collapse_ranges_first_then_step() {
        let text = "abc";
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 2,
            }],
            None,
        )
        .unwrap()
        .0;
        let moved = apply(text, &set, Motion::CharRight, false, None).unwrap();
        assert_eq!(moved.primary().head(), 2, "collapse onto the end first");
        let moved = apply(text, &moved, Motion::CharRight, false, None).unwrap();
        assert_eq!(moved.primary().head(), 3);
        let moved = apply(text, &moved, Motion::CharRight, false, None).unwrap();
        assert_eq!(moved.primary().head(), 3, "clamped at the bound");
        let moved = apply(text, &moved, Motion::CharLeft, false, None).unwrap();
        assert_eq!(moved.primary().head(), 2);
    }

    #[test]
    fn extend_keeps_the_anchor() {
        let text = "abcdef";
        let set = set_of(text, 2);
        let moved = apply(text, &set, Motion::CharRight, true, None).unwrap();
        assert_eq!(moved.primary().anchor(), 2);
        assert_eq!(moved.primary().head(), 3);
    }

    #[test]
    fn word_end_lands_on_the_containing_or_next_word() {
        let text = "one two";
        // Inside "one" (1): lands on its end.
        let moved = apply(text, &set_of(text, 1), Motion::WordEnd, false, None).unwrap();
        assert_eq!(moved.primary().head(), 3);
        // On the word's end already: next word's end.
        let moved = apply(text, &set_of(text, 3), Motion::WordEnd, false, None).unwrap();
        assert_eq!(moved.primary().head(), 7);
        // Between words (4, on the space): next word's end.
        let moved = apply(text, &set_of(text, 4), Motion::WordEnd, false, None).unwrap();
        assert_eq!(moved.primary().head(), 7);
    }

    #[test]
    fn line_and_doc_motions_collapse_to_boundaries() {
        let text = "ab\ncd";
        let moved = apply(text, &set_of(text, 4), Motion::LineStart, false, None).unwrap();
        assert_eq!(moved.primary().head(), 3);
        let moved = apply(text, &set_of(text, 3), Motion::LineEnd, false, None).unwrap();
        assert_eq!(moved.primary().head(), 5);
        let moved = apply(text, &set_of(text, 3), Motion::DocStart, false, None).unwrap();
        assert_eq!(moved.primary().head(), 0);
        let moved = apply(text, &set_of(text, 3), Motion::DocEnd, false, None).unwrap();
        assert_eq!(moved.primary().head(), 5);
    }

    #[test]
    fn visual_motions_require_one_target_per_member() {
        let text = "ab\ncd";
        let set = set_of(text, 0);
        assert_eq!(
            apply(text, &set, Motion::LineDown, false, None),
            Err(MotionError::MissingTargets)
        );
        let moved = apply(text, &set, Motion::LineDown, false, Some(&[3])).unwrap();
        assert_eq!(moved.primary().head(), 3);
    }

    #[test]
    fn members_merging_onto_one_point_keep_the_primary() {
        let text = "ab\ncd";
        let set = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: Some(1),
                    anchor: 0,
                    head: 0,
                },
                RawSelection {
                    id: Some(2),
                    anchor: 1,
                    head: 1,
                },
            ],
            Some(2),
        )
        .unwrap()
        .0;
        // Both members move onto the same line-end point and merge
        // deterministically — the survivor keeps the earliest id, so the
        // lost primary hint resolves to the last survivor.
        let moved = apply(text, &set, Motion::LineEnd, false, None).unwrap();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved.primary_id().as_raw(), 1);
        // A merge that keeps the hinted member preserves it as primary.
        let set = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: Some(1),
                    anchor: 0,
                    head: 0,
                },
                RawSelection {
                    id: Some(2),
                    anchor: 1,
                    head: 1,
                },
            ],
            Some(1),
        )
        .unwrap()
        .0;
        let moved = apply(text, &set, Motion::DocEnd, false, None).unwrap();
        assert_eq!(moved.primary_id().as_raw(), 1);
    }
}
