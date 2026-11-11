//! The N-selection edit planner (FEUX-0002 E-6, §5.3).
//!
//! One pure planner pass per operation over `(text, set, op)` produces the
//! non-overlapping edit batch, the after-text, and the after-set. The
//! planner is the single mutation semantic: every destructive edit path
//! (toolbar, menu, chords, and later the captured typing path) derives its
//! edits here. Empty-selection policies are FEUX-001 D-11's table (Delete
//! on an empty member acts on the following grapheme, or the preceding
//! non-newline grapheme at EOL/EOF; an empty line is a no-op). Typing
//! inserts at every caret.
//!
//! X03 ships `Delete` and `Typing` (the proof verbs); additional verbs land
//! with their phases (X04+) — no unimplemented variants are registered.

use crate::selection::{SelectionSet, SpanEdit, is_grapheme_boundary};
use flavite_syntax::text::{TextByte, next_grapheme_boundary, prev_grapheme_boundary};

/// The operation to plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOperation {
    /// Delete each member's effective range (D-11 empty policies).
    Delete,
    /// Insert `text` at every caret (one atomic batch, ascending order).
    Typing(String),
}

/// A planned edit batch.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedEdit {
    /// Ascending, non-overlapping edit ops with their replacement text.
    /// Empty when every member was a no-op (e.g. Delete on an empty line).
    pub ops: Vec<crate::undo::EditOp>,
    /// Span view of `ops` (pre-edit coordinates) for mapping.
    pub edits: Vec<SpanEdit>,
    /// The after-text (the batch applied to the given text).
    pub after_text: String,
    /// The after-set (mapped through the edits and re-normalized).
    pub after_set: SelectionSet,
}

/// Typed planning refusal. Zero state changes on any error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The selection set is invalid against the text.
    Set(crate::selection::SetError),
}

impl From<crate::selection::SetError> for PlanError {
    fn from(error: crate::selection::SetError) -> Self {
        Self::Set(error)
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Set(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// One concrete edit with its replacement text (internal).
#[derive(Debug, Clone, PartialEq, Eq)]
struct PlannedEdit {
    start: usize,
    end: usize,
    replacement: String,
}

impl PlannedEdit {
    fn as_spans(&self) -> SpanEdit {
        SpanEdit::new(self.start, self.end, self.replacement.len())
    }
}

/// Plan one operation over `(text, set)`.
///
/// # Errors
///
/// [`PlanError::Set`] when the set is invalid against the text. An empty
/// result (`edits` empty) is not an error: it is a zero-effect command.
pub fn plan(text: &str, set: &SelectionSet, op: &EditOperation) -> Result<PreparedEdit, PlanError> {
    set.validate_against(text)?;
    let mut planned = Vec::with_capacity(set.len());
    match op {
        EditOperation::Delete => {
            for member in set.iter() {
                if member.is_caret() {
                    if let Some(range) = delete_range(text, member.anchor()) {
                        planned.push(PlannedEdit {
                            start: range.0,
                            end: range.1,
                            replacement: String::new(),
                        });
                    }
                } else {
                    // A range member is deleted whole.
                    planned.push(PlannedEdit {
                        start: member.start(),
                        end: member.end(),
                        replacement: String::new(),
                    });
                }
            }
        }
        EditOperation::Typing(insertion) => {
            if insertion.is_empty() {
                // No-op typing is a zero-effect command.
                return Ok(PreparedEdit {
                    ops: Vec::new(),
                    edits: Vec::new(),
                    after_text: text.to_string(),
                    after_set: set.clone(),
                });
            }
            for member in set.iter() {
                let at = member.head();
                planned.push(PlannedEdit {
                    start: at,
                    end: at,
                    replacement: insertion.clone(),
                });
            }
        }
    }
    planned.sort_by_key(|edit| (edit.start, edit.end));
    planned.dedup_by_key(|edit| (edit.start, edit.end));
    let edits: Vec<SpanEdit> = planned.iter().map(PlannedEdit::as_spans).collect();
    let ops: Vec<crate::undo::EditOp> = planned
        .iter()
        .map(|edit| {
            crate::undo::EditOp::new(
                flavite_syntax::text::Span::new(
                    flavite_syntax::text::TextByte(edit.start),
                    flavite_syntax::text::TextByte(edit.end),
                ),
                edit.replacement.clone(),
            )
        })
        .collect();
    let after_text = apply_edits(text, &planned);
    let (after_set, _) = set.map_through(&edits);
    // The mapped set is validated against the after-text (never adjusted by
    // total length).
    after_set.validate_against(&after_text)?;
    Ok(PreparedEdit {
        ops,
        edits,
        after_text,
        after_set,
    })
}

/// The effective delete range for one caret, or None for a no-op.
///
/// D-11 empty policy: following grapheme; at EOL/EOF the preceding
/// non-newline grapheme when the caret is not at the start of its line; a
/// caret at the start of its line (including an empty line) is a no-op.
fn delete_range(text: &str, at: usize) -> Option<(usize, usize)> {
    debug_assert!(at <= text.len());
    debug_assert!(is_grapheme_boundary(text, at));
    if text.is_empty() {
        return None;
    }
    if at == text.len() {
        return preceding_non_newline(text, at);
    }
    let next = next_grapheme_boundary(text, TextByte(at)).0;
    if text.get(at..next) == Some("\n") {
        // At EOL: prefer the preceding non-newline grapheme; a caret at the
        // start of its line (empty line) is a no-op.
        let line_start = text[..at].rfind('\n').map_or(0, |at| at + 1);
        if at == line_start {
            return None;
        }
        return preceding_non_newline(text, at);
    }
    Some((at, next))
}

/// The single preceding non-newline grapheme before `at`, if any. Newline
/// runs are skipped so a trailing newline is never deleted (D-11: at
/// EOL/EOF the preceding non-newline grapheme).
fn preceding_non_newline(text: &str, at: usize) -> Option<(usize, usize)> {
    let mut cursor = at;
    while cursor > 0 {
        let prev = prev_grapheme_boundary(text, TextByte(cursor)).0;
        if prev == cursor {
            break;
        }
        if !text[prev..cursor].contains('\n') {
            return Some((prev, cursor));
        }
        cursor = prev;
    }
    None
}

/// Apply an ascending, non-overlapping edit batch to `text`.
fn apply_edits(text: &str, edits: &[PlannedEdit]) -> String {
    if edits.is_empty() {
        return text.to_string();
    }
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for edit in edits {
        result.push_str(&text[cursor..edit.start]);
        result.push_str(&edit.replacement);
        cursor = edit.end;
    }
    result.push_str(&text[cursor..]);
    result
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
    fn delete_at_caret_removes_the_following_grapheme() {
        let text = "aé漢";
        let prepared = plan(text, &set_of(text, 0), &EditOperation::Delete).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(0, 1, 0)]);
        assert_eq!(prepared.after_text, "é漢");
        assert_eq!(prepared.after_set.primary().head(), 0);
    }

    #[test]
    fn delete_at_eol_uses_the_preceding_non_newline_grapheme() {
        let text = "ab\ncd";
        // Caret before 'c' (index 3): deletes 'c'.
        let prepared = plan(text, &set_of(text, 3), &EditOperation::Delete).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(3, 4, 0)]);
        assert_eq!(prepared.after_text, "ab\nd");
        // Caret at EOL (index 2, right before the newline): deletes 'b'.
        let prepared = plan(text, &set_of(text, 2), &EditOperation::Delete).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(1, 2, 0)]);
        assert_eq!(prepared.after_text, "a\ncd");
    }

    #[test]
    fn delete_on_an_empty_line_is_a_no_op() {
        let text = "a\n\nb";
        // The empty line between the newlines: caret at 2.
        let prepared = plan(text, &set_of(text, 2), &EditOperation::Delete).unwrap();
        assert!(prepared.edits.is_empty());
        assert_eq!(prepared.after_text, text);
        // The set is unchanged.
        assert_eq!(prepared.after_set.primary().head(), 2);
    }

    #[test]
    fn delete_at_eof_after_a_trailing_newline_takes_the_preceding_text() {
        let text = "ab\n";
        let prepared = plan(text, &set_of(text, 3), &EditOperation::Delete).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(1, 2, 0)]);
        assert_eq!(prepared.after_text, "a\n");
    }

    #[test]
    fn typing_inserts_at_every_caret_in_one_batch() {
        let text = "ab cd";
        let set = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: None,
                    anchor: 0,
                    head: 0,
                },
                RawSelection {
                    id: None,
                    anchor: 3,
                    head: 3,
                },
            ],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::Typing("_".into())).unwrap();
        assert_eq!(
            prepared.edits,
            vec![SpanEdit::new(0, 0, 1), SpanEdit::new(3, 3, 1)]
        );
        assert_eq!(prepared.after_text, "_ab _cd");
        let members = prepared.after_set.members();
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].head(), 1, "insertion sticks right");
        assert_eq!(members[1].head(), 5);
    }

    #[test]
    fn typing_empty_is_a_zero_effect_command() {
        let text = "abc";
        let prepared = plan(
            text,
            &set_of(text, 1),
            &EditOperation::Typing(String::new()),
        )
        .unwrap();
        assert!(prepared.edits.is_empty());
        assert_eq!(prepared.after_text, text);
    }

    #[test]
    fn invalid_set_is_refused_before_any_planning() {
        let text = "ab";
        let set = SelectionSet::normalize(
            "abcdef",
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 4,
            }],
            None,
        )
        .unwrap()
        .0;
        assert!(plan(text, &set, &EditOperation::Delete).is_err());
    }
}
