//! The N-selection edit planner (FEUX-0002 E-6, §5.3).
//!
//! One pure planner pass per operation over `(text, set, op)` produces the
//! non-overlapping edit batch, the after-text, and the after-set. The
//! planner is the single mutation semantic: every destructive edit path
//! (toolbar verbs, chords, and the captured typing path) derives its edits
//! here. Catalog verbs carry the FEUX-001 D-11 empty policies (Delete/Change
//! on an empty member acts on the following grapheme, or the preceding
//! non-newline grapheme at EOL/EOF; an empty line is a no-op); Copy/Cut of
//! an empty member takes its containing line including its line break, with
//! duplicate lines merged. The ordinary keyboard delete family
//! (`Backspace`/`ForwardDelete` and their word variants) keeps the delivered
//! ordinary-editing rules (a backspace at start-of-line joins the previous
//! line) per §6 "existing ordinary semantics preserved". Typing inserts at
//! every caret; paste broadcasts through `Typing`.

use crate::positions::Affinity;
use crate::selection::{SelectionSet, SpanEdit, is_grapheme_boundary};
use flavite_syntax::text::{
    TextByte, next_grapheme_boundary, next_word_boundary, prev_grapheme_boundary,
    prev_word_boundary,
};

/// The operation to plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditOperation {
    /// Delete each member's effective range (D-11 empty policies: following
    /// grapheme; at EOL/EOF the preceding non-newline grapheme; empty line
    /// no-op). The `editor.delete`/`editor.change` catalog verbs.
    Delete,
    /// Ordinary backspace (§6: existing ordinary semantics): each caret
    /// deletes the preceding grapheme (crossing the line break joins
    /// lines); a caret at the document start is a no-op; ranges delete
    /// whole.
    Backspace,
    /// Ordinary forward delete: each caret deletes the following grapheme
    /// (at end-of-line that is the line break); a caret at the document end
    /// is a no-op; ranges delete whole.
    ForwardDelete,
    /// Ordinary word backspace: each caret deletes back to the previous
    /// word boundary; ranges delete whole.
    BackspaceWord,
    /// Ordinary word delete: each caret deletes to the next word boundary;
    /// ranges delete whole.
    ForwardDeleteWord,
    /// Cut each member's effective range (D-11: an empty member takes its
    /// containing line including its line break; duplicate containing lines
    /// merge). `PreparedEdit::fragments` carries the cut text in document
    /// order for the clipboard effect.
    Cut,
    /// Type `text` at every member: a range member is replaced whole, a
    /// caret member receives an insertion at its head (one atomic batch).
    /// Paste broadcasts the same string through this op.
    Typing(String),
    /// Insert `text` at every member's collapsed start
    /// (`min(anchor, head)`); identical points dedup to one insertion
    /// (D-11: duplicates merge before editing).
    InsertAtStarts(String),
    /// Insert `text` at every member's collapsed end (`max(anchor, head)`),
    /// identical points dedup.
    InsertAtEnds(String),
    /// Insert `text` at the start of every member's head line; members on
    /// the same line share one insertion (D-11: unique target lines).
    InsertAtLineStarts(String),
    /// Insert `text` at the end of every member's head line; unique lines.
    InsertAtLineEnds(String),
    /// Open one new line above (`true`) or below (`false`) every member's
    /// head line; unique lines, one batch. The after caret lands on the
    /// pushed-down (above) or new (below) line via the insertion
    /// stick-right rule.
    OpenLine { above: bool },
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
    /// Cut text in document order (pre-edit coordinates); empty unless the
    /// operation was [`EditOperation::Cut`]. Copy uses
    /// [`copy_fragments`], which shares the same effective ranges.
    pub fragments: Vec<String>,
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
    // Collapsing ops (typing, insert-at-point, line opening) replace every
    // member with a caret at the requested boundary (D-11: "collapse each
    // range to the requested boundary; TYPING") instead of mapping ranges
    // through: carets stick right past their insertion, except open-above
    // whose caret stays before the inserted break (delivered O lands on the
    // new empty line). `None` means: map the members through the edits.
    let mut collapse: Option<(Vec<CollapsePoint>, Affinity)> = None;
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
        EditOperation::Cut => {
            // D-11: carets take their containing line (duplicate lines
            // merge); ranges are taken whole. No-op members (an empty last
            // line's caret) contribute nothing.
            for (start, end) in cut_ranges(text, set) {
                planned.push(PlannedEdit {
                    start,
                    end,
                    replacement: String::new(),
                });
            }
        }
        EditOperation::Backspace | EditOperation::BackspaceWord => {
            for member in set.iter() {
                let range = if member.is_caret() {
                    let head = member.head();
                    let previous = if op == &EditOperation::BackspaceWord {
                        prev_word_boundary(text, TextByte(head)).0
                    } else {
                        prev_grapheme_boundary(text, TextByte(head)).0
                    };
                    (previous != head).then_some((previous, head))
                } else {
                    Some((member.start(), member.end()))
                };
                if let Some((start, end)) = range {
                    planned.push(PlannedEdit {
                        start,
                        end,
                        replacement: String::new(),
                    });
                }
            }
        }
        EditOperation::ForwardDelete | EditOperation::ForwardDeleteWord => {
            for member in set.iter() {
                let range = if member.is_caret() {
                    let head = member.head();
                    let next = if op == &EditOperation::ForwardDeleteWord {
                        next_word_boundary(text, TextByte(head)).0
                    } else {
                        next_grapheme_boundary(text, TextByte(head)).0
                    };
                    (next != head).then_some((head, next))
                } else {
                    Some((member.start(), member.end()))
                };
                if let Some((start, end)) = range {
                    planned.push(PlannedEdit {
                        start,
                        end,
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
                    fragments: Vec::new(),
                });
            }
            for member in set.iter() {
                // A range member is replaced whole (typing over a
                // selection); a caret receives an insertion at its head.
                let (start, end) = if member.is_caret() {
                    (member.head(), member.head())
                } else {
                    (member.start(), member.end())
                };
                planned.push(PlannedEdit {
                    start,
                    end,
                    replacement: insertion.clone(),
                });
            }
            collapse = Some((
                set.iter()
                    .map(|member| {
                        let at = if member.is_caret() {
                            member.head()
                        } else {
                            member.start()
                        };
                        (at, Some(member.id().as_raw()))
                    })
                    .collect(),
                Affinity::Right,
            ));
        }
        EditOperation::InsertAtStarts(insertion) => {
            collapse = Some((
                plan_point_insertions(&mut planned, set, insertion, |member| member.start()),
                Affinity::Right,
            ));
        }
        EditOperation::InsertAtEnds(insertion) => {
            collapse = Some((
                plan_point_insertions(&mut planned, set, insertion, |member| member.end()),
                Affinity::Right,
            ));
        }
        EditOperation::InsertAtLineStarts(insertion) => {
            collapse = Some((
                plan_point_insertions(&mut planned, set, insertion, |member| {
                    line_boundary(text, member.head(), true)
                }),
                Affinity::Right,
            ));
        }
        EditOperation::InsertAtLineEnds(insertion) => {
            collapse = Some((
                plan_point_insertions(&mut planned, set, insertion, |member| {
                    line_boundary(text, member.head(), false)
                }),
                Affinity::Right,
            ));
        }
        EditOperation::OpenLine { above } => {
            // Above: the caret lands on the new empty line (before the
            // inserted break); below: past the break on the new line.
            let affinity = if *above {
                Affinity::Left
            } else {
                Affinity::Right
            };
            collapse = Some((
                plan_point_insertions(&mut planned, set, "\n", |member| {
                    line_boundary(text, member.head(), *above)
                }),
                affinity,
            ));
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
    // Collapsing ops put a caret past each insertion; Delete maps the
    // members through the edits. The resulting set is validated against the
    // after-text (never adjusted by total length).
    let after_set = match &collapse {
        Some((points, affinity)) => {
            // Carets at the pre-edit collapse points, mapped through the
            // batch with the op's boundary affinity: each caret lands on its
            // own insertion, earlier insertions shift later carets.
            let spans: Vec<crate::history::MovementSpan> = edits
                .iter()
                .map(|edit| crate::history::MovementSpan {
                    start: edit.start,
                    end: edit.end,
                    replacement_len: edit.replacement_len,
                })
                .collect();
            let (carets, _) = SelectionSet::normalize(
                text,
                points
                    .iter()
                    .map(|&(at, id)| crate::selection::RawSelection {
                        id,
                        anchor: at,
                        head: at,
                    })
                    .collect(),
                Some(set.primary_id().as_raw()),
            )?;
            // Bulk two-pointer mapping (NFR-1): the carets are ascending, so
            // one O(points + spans) pass replaces per-point span scans.
            let points: Vec<usize> = carets
                .members()
                .iter()
                .map(|member| member.anchor())
                .collect();
            let mapped_points = crate::selection::map_points_through_spans(
                &points,
                &spans,
                positions_affinity(*affinity),
            );
            let mapped: Vec<crate::selection::RawSelection> = carets
                .members()
                .iter()
                .zip(mapped_points)
                .map(|(member, at)| crate::selection::RawSelection {
                    id: Some(member.id().as_raw()),
                    anchor: at,
                    head: at,
                })
                .collect();
            // The primary identity travels with its member through both
            // normalizations: the default (last survivor) would silently
            // move the primary to the set's end on every collapse batch
            // (FEUX-0002 E-4/FR-4).
            let (set, _) =
                SelectionSet::normalize(&after_text, mapped, Some(carets.primary_id().as_raw()))?;
            set
        }
        None => {
            let (set, _) = set.map_through(&edits);
            set
        }
    };
    after_set.validate_against(&after_text)?;
    let fragments = match op {
        EditOperation::Cut => planned
            .iter()
            .map(|edit| text[edit.start..edit.end].to_string())
            .collect(),
        _ => Vec::new(),
    };
    Ok(PreparedEdit {
        ops,
        edits,
        after_text,
        after_set,
        fragments,
    })
}

/// The copy/cut effective ranges in document order (D-11): a range member is
/// taken whole; a caret takes its containing line including its line break;
/// duplicate containing lines merge; an empty last line's caret is a no-op.
/// `Copy` (via [`copy_fragments`]) and `EditOperation::Cut` share this one
/// implementation.
fn cut_ranges(text: &str, set: &SelectionSet) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(set.len());
    for member in set.iter() {
        if !member.is_caret() {
            ranges.push((member.start(), member.end()));
            continue;
        }
        let at = member.head();
        let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
        let line_end = text[at..].find('\n').map_or(text.len(), |i| at + i);
        let end = (line_end + usize::from(line_end < text.len())).min(text.len());
        if end > line_start {
            ranges.push((line_start, end));
        }
    }
    ranges.sort_unstable();
    ranges.dedup();
    ranges
}

/// The text a `Copy` command would place on the clipboard: one fragment per
/// member in document order under the D-11 cut/cut effective-range policy
/// (carets take their containing line; duplicates merge). The host joins
/// fragments with LF (FEUX-0002 §5.3).
///
/// # Errors
///
/// [`PlanError::Set`] when the set is invalid against the text.
pub fn copy_fragments(text: &str, set: &SelectionSet) -> Result<Vec<String>, PlanError> {
    set.validate_against(text)?;
    Ok(cut_ranges(text, set)
        .into_iter()
        .map(|(start, end)| text[start..end].to_string())
        .collect())
}

fn positions_affinity(affinity: Affinity) -> crate::positions::Affinity {
    match affinity {
        Affinity::Left => crate::positions::Affinity::Left,
        Affinity::Right => crate::positions::Affinity::Right,
    }
}

/// One collapse-point insertion: the byte offset plus the contributing
/// member's id (identity travels with the member through both
/// normalizations, FEUX-0002 E-4/FR-4).
type CollapsePoint = (usize, Option<u32>);

/// Plan point insertions: one `PlannedEdit` per unique insertion point
/// (dedup identical points — duplicate members merge into one insertion and
/// their collapse carets re-normalize into one member). Returns the unique
/// pre-edit insertion points for the collapse after-set; an empty insertion
/// is a zero-effect command (no edits, members collapse onto themselves).
fn plan_point_insertions(
    planned: &mut Vec<PlannedEdit>,
    set: &SelectionSet,
    insertion: &str,
    point_of: impl Fn(&crate::selection::Selection) -> usize,
) -> Vec<(usize, Option<u32>)> {
    // Duplicate points merge into one insertion; the merged member keeps
    // the earliest (smallest) id, matching the normalization survivor rule.
    let mut points: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();
    for member in set.iter() {
        let at = point_of(member);
        let id = member.id().as_raw();
        points
            .entry(at)
            .and_modify(|kept| *kept = (*kept).min(id))
            .or_insert(id);
    }
    if !insertion.is_empty() {
        for &at in points.keys() {
            planned.push(PlannedEdit {
                start: at,
                end: at,
                replacement: insertion.to_string(),
            });
        }
    }
    points.into_iter().map(|(at, id)| (at, Some(id))).collect()
}

/// The line start (`true`) or line end (`false`) byte of the line containing
/// byte `at`.
fn line_boundary(text: &str, at: usize, start: bool) -> usize {
    if start {
        text[..at].rfind('\n').map_or(0, |at| at + 1)
    } else {
        // `find` returns a line-relative offset; the closure parameter must
        // not shadow the absolute byte position.
        text[at..]
            .find('\n')
            .map_or(text.len(), |offset| at + offset)
    }
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
    fn typing_over_a_range_replaces_it_whole() {
        let text = "one café two";
        // Select "café" (bytes 4..9) and type "X".
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 4,
                head: 9,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::Typing("X".into())).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(4, 9, 1)]);
        assert_eq!(prepared.after_text, "one X two");
        assert_eq!(
            prepared.after_set.primary().head(),
            5,
            "collapsed onto the typed text"
        );
    }

    #[test]
    fn insert_at_starts_and_ends_dedup_identical_points() {
        let text = "abcdef";
        // Two members whose collapsed starts coincide at 3 (a caret and the
        // start of a range): one insertion, and the mapped members merge.
        let set = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: Some(1),
                    anchor: 3,
                    head: 3,
                },
                RawSelection {
                    id: Some(2),
                    anchor: 3,
                    head: 5,
                },
            ],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::InsertAtStarts("_".into())).unwrap();
        assert_eq!(
            prepared.edits,
            vec![SpanEdit::new(3, 3, 1)],
            "one insertion for one point"
        );
        assert_eq!(prepared.after_text, "abc_def");
        // Both members collapsed onto the same insertion point: one caret.
        assert_eq!(prepared.after_set.len(), 1);
        assert_eq!(prepared.after_set.primary().head(), 4);
    }

    #[test]
    fn line_scoped_insertions_share_one_point_per_line() {
        let text = "ab\ncd\n";
        // Two carets on the first line and one on the second.
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
                    anchor: 2,
                    head: 2,
                },
                RawSelection {
                    id: Some(3),
                    anchor: 4,
                    head: 4,
                },
            ],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::InsertAtLineStarts(">".into())).unwrap();
        assert_eq!(
            prepared.edits,
            vec![SpanEdit::new(0, 0, 1), SpanEdit::new(3, 3, 1)],
            "one insertion per unique line"
        );
        assert_eq!(prepared.after_text, ">ab\n>cd\n");
        // After carets stick right past their insertion.
        let heads: Vec<usize> = prepared
            .after_set
            .members()
            .iter()
            .map(|m| m.head())
            .collect();
        assert_eq!(
            heads,
            vec![1, 5],
            "the second caret shifts by the first insertion"
        );
        // The three original members merge to two (the first-line pair
        // mapped onto one point).
        assert_eq!(prepared.after_set.len(), 2);
    }

    #[test]
    fn open_line_above_and_below_land_on_the_pushed_or_new_line() {
        let text = "ab\ncd";
        let set = set_of(text, 1);
        let prepared = plan(text, &set, &EditOperation::OpenLine { above: true }).unwrap();
        assert_eq!(prepared.after_text, "\nab\ncd");
        assert_eq!(
            prepared.after_set.primary().head(),
            0,
            "the caret lands on the new empty line above (delivered O)"
        );

        let prepared = plan(text, &set, &EditOperation::OpenLine { above: false }).unwrap();
        assert_eq!(prepared.after_text, "ab\n\ncd");
        assert_eq!(
            prepared.after_set.primary().head(),
            3,
            "caret on the new line (past the inserted break)"
        );

        // Unique lines: two carets on one line open one line.
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
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::OpenLine { above: false }).unwrap();
        assert_eq!(prepared.after_text, "ab\n\ncd");
        assert_eq!(prepared.after_set.len(), 1, "merged members after dedup");
    }

    #[test]
    fn insert_at_line_end_handles_the_last_line_without_a_newline() {
        let text = "ab\ncd";
        let set = set_of(text, 4);
        let prepared = plan(text, &set, &EditOperation::InsertAtLineEnds("!".into())).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(5, 5, 1)]);
        assert_eq!(prepared.after_text, "ab\ncd!");
        assert_eq!(prepared.after_set.primary().head(), 6);
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

    #[test]
    fn ordinary_backspace_joins_lines_and_stops_at_the_document_start() {
        let text = "ab\ncd";
        // Caret before 'c': backspace eats the line break (joins).
        let prepared = plan(text, &set_of(text, 3), &EditOperation::Backspace).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(2, 3, 0)]);
        assert_eq!(prepared.after_text, "abcd");
        // Caret at 0 is a no-op.
        let prepared = plan(text, &set_of(text, 0), &EditOperation::Backspace).unwrap();
        assert!(prepared.edits.is_empty());
        assert_eq!(prepared.after_text, text);
    }

    #[test]
    fn ordinary_forward_delete_eats_the_line_break_at_eol() {
        let text = "ab\ncd";
        // Caret at EOL (2): the following grapheme is the newline.
        let prepared = plan(text, &set_of(text, 2), &EditOperation::ForwardDelete).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(2, 3, 0)]);
        assert_eq!(prepared.after_text, "abcd");
        // Caret at EOF is a no-op.
        let prepared = plan(text, &set_of(text, 5), &EditOperation::ForwardDelete).unwrap();
        assert!(prepared.edits.is_empty());
    }

    #[test]
    fn word_deletes_take_delivered_word_ranges_per_member() {
        let text = "one two three";
        let prepared = plan(text, &set_of(text, 3), &EditOperation::BackspaceWord).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(0, 3, 0)]);
        assert_eq!(prepared.after_text, " two three");
        let prepared = plan(text, &set_of(text, 3), &EditOperation::ForwardDeleteWord).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(3, 7, 0)]);
        assert_eq!(prepared.after_text, "one three");
        // A range member is deleted whole regardless of the word policy.
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 3,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::BackspaceWord).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(0, 3, 0)]);
    }

    #[test]
    fn cut_takes_whole_lines_for_carets_and_merges_duplicates() {
        let text = "one\ntwo\nthree";
        // Two carets on "two" (bytes 4 and 6): one line, one fragment.
        let set = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: Some(1),
                    anchor: 4,
                    head: 4,
                },
                RawSelection {
                    id: Some(2),
                    anchor: 6,
                    head: 6,
                },
            ],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::Cut).unwrap();
        assert_eq!(prepared.edits, vec![SpanEdit::new(4, 8, 0)]);
        assert_eq!(prepared.fragments, vec!["two\n"]);
        assert_eq!(prepared.after_text, "one\nthree");
        assert_eq!(
            prepared.after_set.primary().head(),
            4,
            "the caret lands at the cut line's start"
        );
        // A range member is cut whole, not line-expanded.
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 3,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::Cut).unwrap();
        assert_eq!(prepared.fragments, vec!["one"]);
    }

    #[test]
    fn copy_fragments_share_the_cut_ranges() {
        let text = "one\ntwo\nthree";
        let set = set_of(text, 8);
        let fragments = copy_fragments(text, &set).unwrap();
        assert_eq!(fragments, vec!["three"]);
        // An empty last line's caret contributes nothing.
        let trailing = "one\n";
        let fragments = copy_fragments(trailing, &set_of(trailing, 4)).unwrap();
        assert!(fragments.is_empty());
    }
}

#[cfg(test)]
mod collapse_primary_tests {
    use super::*;
    use crate::selection::RawSelection;

    #[test]
    fn typing_preserves_the_primary_member_identity() {
        // FEUX-0002 R14/E-4: a collapse-op batch keeps the input set's
        // primary member — the after-set's primary never silently moves to
        // the last survivor.
        let text = "# Alpha\n\nalpha body\none\ntwo\nthree\nfour";
        let raw: Vec<RawSelection> = [(0usize, 0usize), (8, 8), (9, 9), (20, 20)]
            .iter()
            .enumerate()
            .map(|(id, &(anchor, head))| RawSelection {
                id: Some(id as u32),
                anchor,
                head,
            })
            .collect();
        let (set, _) = SelectionSet::normalize(text, raw, Some(0)).unwrap();
        assert_eq!(set.primary().head(), 0, "the first member is primary");
        let prepared = plan(text, &set, &EditOperation::Typing("!".into())).unwrap();
        assert_eq!(prepared.after_set.len(), 4);
        assert_eq!(
            prepared.after_set.primary().head(),
            1,
            "the primary is the first member's mapped caret, not the last"
        );
    }
}
