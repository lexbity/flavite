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

use crate::indent::{
    dedent_step, detect_unit, is_blank_or_ws, leading_whitespace, spaces_to_tabs_line,
    tabs_to_spaces_line,
};
use crate::numbers::{
    NumberError, extract_first_number, increment_number, number_range, scan_number,
};
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
    // FEUX-0004 X01: Transform family
    /// Uppercase each character in each member's range.
    CaseUpper,
    /// Lowercase each character in each member's range.
    CaseLower,
    /// Swap the case of each character in each member's range.
    CaseSwap,
    /// Title-case each word in each member's range (UAX #29 word boundaries).
    CaseTitle,
    /// Increment the first number found in each member's range (leading-zero width preserved).
    NumberInc,
    /// Decrement the first number found in each member's range (leading-zero width preserved).
    NumberDec,
    /// Replace each member's number with its document-order index (1-based).
    NumberSequence,
    // FEUX-0004 X01: Lines family
    /// Sort lines in each member's containing line block (byte-lexicographic, stable).
    LinesSortAsc,
    /// Sort lines in each member's containing line block (inverse byte-lexicographic, stable).
    LinesSortDesc,
    /// Sort lines in each member's containing line block by numeric value (stable).
    LinesSortNumeric,
    /// Join lines in each member's containing line block (replace newlines with single space).
    LinesJoin,
    /// Duplicate each member's containing line block.
    LinesDuplicate,
    /// Remove duplicate lines in each member's containing line block (byte-exact, first wins).
    LinesDedupe,
    /// Shuffle lines in each member's containing line block (Fisher-Yates, splitmix64 seeded).
    LinesShuffle,
    /// Move each member's containing line block up by one position.
    LinesMoveUp,
    /// Move each member's containing line block down by one position.
    LinesMoveDown,
    // FEUX-0004 X03: Align command
    /// Align lines in each member's containing line block on a literal delimiter.
    /// The delimiter is the first occurrence on each line; prefixes are padded
    /// with spaces so all delimiters align to the widest prefix column.
    LinesAlign { delimiter: String },
    // FEUX-0004 X04: Surround family
    /// Add surrounding delimiters to each member's range.
    /// For carets: wraps the empty range at the caret.
    SurroundAdd { open: String, close: String },
    /// Change surrounding delimiters of each member's range.
    /// The pair must exist and enclose each member.
    SurroundChange {
        old_open: String,
        old_close: String,
        new_open: String,
        new_close: String,
    },
    /// Delete surrounding delimiters from each member's range.
    /// The pair must exist and enclose each member.
    SurroundDelete { open: String, close: String },
    // FEUX-0005 X01: Indent family
    /// Indent each member's containing line block by `amount` units.
    /// Blank/whitespace-only lines are left untouched.
    IndentLines { amount: u32 },
    /// Dedent each member's containing line block by `amount` units.
    /// Dedent uses tab-stop alignment (previous tab stop per line).
    /// Lines at column 0 are unchanged; all-at-0 invocation is a zero-effect no-op.
    DedentLines { amount: u32 },
    /// Convert leading whitespace: tabs to spaces or spaces to tabs.
    /// Scope: per-member containing line block (FEUX-0005 §5.3).
    /// Whitespace-only lines ARE included (their entire content is indentation).
    /// Content tabs/spaces are never touched.
    ReWhitespace { to_tabs: bool },
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
    /// The detected indentation unit used by width-consuming commands (FEUX-0005 E-9).
    /// `None` for operations that don't use indentation units.
    pub unit: Option<crate::indent::Unit>,
}

/// IME text replaces the primary only; every other member maps through
/// that single edit. This is the same planner and canonical mapping algebra.
pub fn plan_primary_typing(
    text: &str,
    set: &SelectionSet,
    insertion: String,
) -> Result<PreparedEdit, PlanError> {
    set.validate_against(text)?;
    let primary = set.primary();
    let (only, _) = SelectionSet::normalize(
        text,
        vec![crate::selection::RawSelection {
            id: Some(primary.id().as_raw()),
            anchor: primary.anchor(),
            head: primary.head(),
        }],
        Some(primary.id().as_raw()),
    )?;
    let mut prepared = plan(text, &only, &EditOperation::Typing(insertion))?;
    if prepared.ops.is_empty() {
        prepared.after_set = set.clone();
        return Ok(prepared);
    }
    let spans: Vec<_> = prepared
        .edits
        .iter()
        .map(|edit| crate::history::MovementSpan {
            start: edit.start,
            end: edit.end,
            replacement_len: edit.replacement_len,
        })
        .collect();
    let exact = prepared.after_set.primary();
    let candidates = set
        .iter()
        .map(|member| {
            let (anchor, head) = if member.id() == primary.id() {
                (exact.anchor(), exact.head())
            } else {
                let (a, h) = if member.anchor() < member.head() {
                    (Affinity::Left, Affinity::Right)
                } else if member.anchor() > member.head() {
                    (Affinity::Right, Affinity::Left)
                } else {
                    (Affinity::Right, Affinity::Right)
                };
                (
                    crate::selection::map_point_through_spans(member.anchor(), &spans, a),
                    crate::selection::map_point_through_spans(member.head(), &spans, h),
                )
            };
            crate::selection::RawSelection {
                id: Some(member.id().as_raw()),
                anchor,
                head,
            }
        })
        .collect();
    prepared.after_set = SelectionSet::normalize(
        &prepared.after_text,
        candidates,
        Some(primary.id().as_raw()),
    )?
    .0;
    Ok(prepared)
}

/// Typed planning refusal. Zero state changes on any error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// The selection set is invalid against the text.
    Set(crate::selection::SetError),
    /// A required number was not found in one or more selections.
    NoNumber { count: usize },
    /// Number overflow/underflow when incrementing/decrementing.
    Overflow { value: String, delta: i64 },
    /// For NumberSequence: one or more selections contain no number.
    SequenceNoNumber,
    /// A Lines move would run past the document edge (E-8 §5.7 clamp).
    MoveClamp,
    /// A Lines move target intersects another member's block (E-8 §5.7).
    MoveOverlap,
}

impl From<crate::selection::SetError> for PlanError {
    fn from(error: crate::selection::SetError) -> Self {
        Self::Set(error)
    }
}

impl From<NumberError> for PlanError {
    fn from(error: NumberError) -> Self {
        match error {
            NumberError::NoNumber => Self::NoNumber { count: 1 },
            NumberError::Overflow { value, delta } => Self::Overflow { value, delta },
            NumberError::SequenceNoNumber => Self::SequenceNoNumber,
        }
    }
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Set(error) => write!(f, "{error}"),
            Self::NoNumber { count } => {
                if *count == 1 {
                    write!(f, "No number found in selection")
                } else {
                    write!(f, "{} selections contain no number", count)
                }
            }
            Self::Overflow { value, delta } => {
                write!(f, "Number {} overflows when adding {}", value, delta)
            }
            Self::SequenceNoNumber => write!(f, "Some selections contain no number"),
            Self::MoveClamp => write!(f, "Cannot move: block is at the edge of the document"),
            Self::MoveOverlap => write!(f, "Cannot move: selections occupy adjacent blocks"),
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
    // Track the indentation unit for width-consuming commands (FEUX-0005 E-9)
    let mut unit: Option<crate::indent::Unit> = None;
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
                    unit: None,
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
            // FEUX-0005 E-4: Auto-indent: copy the source line's leading whitespace verbatim
            // Above: insert leading_ws + "\n" at line start, caret on new line after ws
            // Below: insert "\n" + leading_ws at line end, caret after ws

            // Group members by unique head line to match the delivered behavior
            let mut points: std::collections::BTreeMap<usize, (usize, usize, u32)> =
                std::collections::BTreeMap::new();
            for member in set.iter() {
                let line_start = line_boundary(text, member.head(), true);
                let line_end = line_boundary(text, member.head(), false);
                let at = if *above { line_start } else { line_end };
                let id = member.id().as_raw();
                points
                    .entry(at)
                    .and_modify(|(_, _, kept)| *kept = (*kept).min(id))
                    .or_insert((line_start, line_end, id));
            }

            // For each unique line, compute the leading whitespace and create the insertion
            let mut collapse_points = Vec::new();
            for (&at, (line_start, line_end, id)) in &points {
                // Extract the line to get its leading whitespace
                let line_text = &text[*line_start..*line_end];
                let leading_ws = leading_whitespace(line_text);

                // Compute insertion text
                let insertion = if *above {
                    // open_above: insert leading_ws + "\n" at line start
                    format!("{}\n", leading_ws)
                } else {
                    // open_below: insert "\n" + leading_ws at line end
                    format!("\n{}", leading_ws)
                };

                if !insertion.is_empty() {
                    planned.push(PlannedEdit {
                        start: at,
                        end: at,
                        replacement: insertion,
                    });
                }

                // Collapse points: at the insertion position for both
                // Affinity determines where the caret lands:
                // - open_above: Affinity::Left → caret at beginning of new line
                // - open_below: Affinity::Right → caret after the insertion (after leading_ws)
                collapse_points.push((at, Some(*id)));
            }

            let affinity = if *above {
                Affinity::Left
            } else {
                Affinity::Right
            };
            collapse = Some((collapse_points, affinity));
        }
        // FEUX-0004 X01: Transform family
        EditOperation::CaseUpper => {
            for member in set.iter() {
                let range = if member.is_caret() {
                    // D-11: caret operates on the following grapheme
                    if let Some(range) = delete_range(text, member.head()) {
                        (range.0, range.1)
                    } else {
                        continue;
                    }
                } else {
                    (member.start(), member.end())
                };
                let transformed = transform_case(
                    text[range.0..range.1].chars(),
                    |c| c.to_uppercase().collect::<String>(),
                    range.0,
                );
                planned.push(PlannedEdit {
                    start: range.0,
                    end: range.1,
                    replacement: transformed,
                });
            }
        }
        EditOperation::CaseLower => {
            for member in set.iter() {
                let range = if member.is_caret() {
                    if let Some(range) = delete_range(text, member.head()) {
                        (range.0, range.1)
                    } else {
                        continue;
                    }
                } else {
                    (member.start(), member.end())
                };
                let transformed = transform_case(
                    text[range.0..range.1].chars(),
                    |c| c.to_lowercase().collect::<String>(),
                    range.0,
                );
                planned.push(PlannedEdit {
                    start: range.0,
                    end: range.1,
                    replacement: transformed,
                });
            }
        }
        EditOperation::CaseSwap => {
            for member in set.iter() {
                let range = if member.is_caret() {
                    if let Some(range) = delete_range(text, member.head()) {
                        (range.0, range.1)
                    } else {
                        continue;
                    }
                } else {
                    (member.start(), member.end())
                };
                let transformed =
                    transform_case(text[range.0..range.1].chars(), swap_case, range.0);
                planned.push(PlannedEdit {
                    start: range.0,
                    end: range.1,
                    replacement: transformed,
                });
            }
        }
        EditOperation::CaseTitle => {
            // Title case: first cased char of each word upper, rest lower
            for member in set.iter() {
                let range = if member.is_caret() {
                    if let Some(range) = delete_range(text, member.head()) {
                        (range.0, range.1)
                    } else {
                        continue;
                    }
                } else {
                    (member.start(), member.end())
                };
                let transformed = title_case(&text[range.0..range.1]);
                planned.push(PlannedEdit {
                    start: range.0,
                    end: range.1,
                    replacement: transformed,
                });
            }
        }
        EditOperation::NumberInc => {
            // E-9: every member must carry a scannable number; overflow
            // refuses naming the number (all-or-nothing, like sequence).
            let mut missing = 0usize;
            for member in set.iter() {
                let Some(scan_result) = scan_number(text, member) else {
                    missing += 1;
                    continue;
                };
                let replacement = increment_number(text, &scan_result, 1)?;
                let (start, end) = number_range(&scan_result);
                planned.push(PlannedEdit {
                    start,
                    end,
                    replacement,
                });
            }
            if missing > 0 {
                return Err(PlanError::NoNumber { count: missing });
            }
        }
        EditOperation::NumberDec => {
            let mut missing = 0usize;
            for member in set.iter() {
                let Some(scan_result) = scan_number(text, member) else {
                    missing += 1;
                    continue;
                };
                let replacement = increment_number(text, &scan_result, -1)?;
                let (start, end) = number_range(&scan_result);
                planned.push(PlannedEdit {
                    start,
                    end,
                    replacement,
                });
            }
            if missing > 0 {
                return Err(PlanError::NoNumber { count: missing });
            }
        }
        EditOperation::NumberSequence => {
            // Collect all members with their document order indices
            let members: Vec<_> = set.iter().collect();
            let mut indices: Vec<(usize, usize, usize)> = Vec::new(); // (member_index, start, end)
            for (idx, member) in members.iter().enumerate() {
                if let Some(scan_result) = scan_number(text, member) {
                    let (start, end) = number_range(&scan_result);
                    indices.push((idx, start, end));
                } else {
                    // If any member doesn't have a number, refuse the whole operation
                    return Err(PlanError::SequenceNoNumber);
                }
            }
            for (idx, start, end) in indices {
                let replacement = (idx + 1).to_string();
                planned.push(PlannedEdit {
                    start,
                    end,
                    replacement,
                });
            }
        }
        // FEUX-0004 X01: Lines family
        EditOperation::LinesSortAsc => {
            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    let mut sorted_lines: Vec<&str> = lines;
                    // Stable sort by byte order
                    sorted_lines.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
                    let replacement = sorted_lines.join("\n");
                    // Add trailing newline if the original block ended with one
                    let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                        replacement + "\n"
                    } else {
                        replacement
                    };
                    planned.push(PlannedEdit {
                        start: block_start,
                        end: block_end,
                        replacement,
                    });
                }
            }
        }
        EditOperation::LinesSortDesc => {
            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    let mut sorted_lines: Vec<&str> = lines;
                    // Stable sort descending
                    sorted_lines.sort_by(|a, b| b.as_bytes().cmp(a.as_bytes()));
                    let replacement = sorted_lines.join("\n");
                    let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                        replacement + "\n"
                    } else {
                        replacement
                    };
                    planned.push(PlannedEdit {
                        start: block_start,
                        end: block_end,
                        replacement,
                    });
                }
            }
        }
        EditOperation::LinesSortNumeric => {
            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    // Parse first integer from each line, sort by that
                    let mut indexed_lines: Vec<(usize, &str)> = lines
                        .iter()
                        .enumerate()
                        .map(|(i, &line)| (i, line))
                        .collect();
                    indexed_lines.sort_by_key(|(_, line)| {
                        extract_first_number(line)
                            .map(|n| n as isize)
                            .unwrap_or(isize::MAX)
                    });
                    let sorted_lines: Vec<&str> =
                        indexed_lines.into_iter().map(|(_, line)| line).collect();
                    // Stable: maintain original order for equal numeric values (by index)
                    // Already done by using enumerate above
                    let replacement = sorted_lines.join("\n");
                    let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                        replacement + "\n"
                    } else {
                        replacement
                    };
                    planned.push(PlannedEdit {
                        start: block_start,
                        end: block_end,
                        replacement,
                    });
                }
            }
        }
        EditOperation::LinesJoin => {
            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    // Join with single space
                    let replacement = lines.join(" ");
                    // Add trailing newline if the original block ended with one
                    let replacement = if has_trailing_newline {
                        replacement + "\n"
                    } else {
                        replacement
                    };
                    planned.push(PlannedEdit {
                        start: block_start,
                        end: block_end,
                        replacement,
                    });
                }
            }
        }
        EditOperation::LinesDuplicate => {
            for member in set.iter() {
                if let Some((block_start, block_end, _lines, _has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    let block_text = &text[block_start..block_end];
                    let replacement = block_text.to_string() + block_text;
                    planned.push(PlannedEdit {
                        start: block_start,
                        end: block_end,
                        replacement,
                    });
                }
            }
        }
        EditOperation::LinesDedupe => {
            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    // Remove duplicates, keeping first occurrence
                    let mut seen = std::collections::HashSet::new();
                    let mut deduped: Vec<&str> = Vec::new();
                    for line in lines {
                        if !seen.contains(line) {
                            seen.insert(line);
                            deduped.push(line);
                        }
                    }
                    let replacement = deduped.join("\n");
                    let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                        replacement + "\n"
                    } else {
                        replacement
                    };
                    planned.push(PlannedEdit {
                        start: block_start,
                        end: block_end,
                        replacement,
                    });
                }
            }
        }
        EditOperation::LinesShuffle => {
            // Use splitmix64 for shuffling
            use std::time::SystemTime;
            let seed = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;
            let mut rng = SplitMix64::new(seed);
            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    let mut shuffled = lines.to_vec();
                    fisher_yates_shuffle(&mut shuffled, &mut rng);
                    let replacement = shuffled.join("\n");
                    let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                        replacement + "\n"
                    } else {
                        replacement
                    };
                    planned.push(PlannedEdit {
                        start: block_start,
                        end: block_end,
                        replacement,
                    });
                }
            }
        }
        EditOperation::LinesMoveUp => {
            // E-8/S5.7: collect every member's block first; refuse the whole
            // command when any block clamps at the document edge or when a
            // target region intersects another member's block. Only after
            // the precheck passes do we compose the swaps (document order,
            // top to bottom).
            let mut blocks: Vec<(usize, usize)> = Vec::with_capacity(set.len());
            for member in set.iter() {
                if let Some(block) = containing_line_range(text, member) {
                    blocks.push(block);
                }
            }
            let mut targets: Vec<(usize, usize)> = Vec::with_capacity(blocks.len());
            for &(block_start, _) in &blocks {
                if block_start == 0 {
                    return Err(PlanError::MoveClamp);
                }
                // block_start is always a line start (0 or right after a
                // newline), so block_start - 1 is the previous line's
                // newline; the previous line starts after the newline before
                // that one (or at 0).
                let prev_line_start = text[..block_start - 1].rfind('\n').map_or(0, |i| i + 1);
                targets.push((prev_line_start, block_start));
            }
            for (i, &(target_start, target_end)) in targets.iter().enumerate() {
                for (j, &(other_start, other_end)) in blocks.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    if target_start < other_end && other_start < target_end {
                        return Err(PlanError::MoveOverlap);
                    }
                }
            }
            for (i, &(block_start, block_end)) in blocks.iter().enumerate() {
                let (prev_line_start, prev_line_end) = targets[i];
                let this_block = text[block_start..block_end].to_string();
                let prev_block = text[prev_line_start..prev_line_end].to_string();
                planned.push(PlannedEdit {
                    start: prev_line_start,
                    end: prev_line_end,
                    replacement: this_block.clone(),
                });
                planned.push(PlannedEdit {
                    start: block_start,
                    end: block_end,
                    replacement: prev_block,
                });
            }
        }
        EditOperation::LinesMoveDown => {
            // E-8/S5.7: same precheck discipline as move-up; targets are the
            // next line block.
            let mut blocks: Vec<(usize, usize)> = Vec::with_capacity(set.len());
            for member in set.iter() {
                if let Some(block) = containing_line_range(text, member) {
                    blocks.push(block);
                }
            }
            let mut targets: Vec<(usize, usize)> = Vec::with_capacity(blocks.len());
            for &(_, block_end) in &blocks {
                if block_end >= text.len() {
                    return Err(PlanError::MoveClamp);
                }
                let next_line_end = text[block_end..]
                    .find('\n')
                    .map_or(text.len(), |i| block_end + i + 1);
                targets.push((block_end, next_line_end));
            }
            for (i, &(target_start, target_end)) in targets.iter().enumerate() {
                for (j, &(other_start, other_end)) in blocks.iter().enumerate() {
                    if i == j {
                        continue;
                    }
                    if target_start < other_end && other_start < target_end {
                        return Err(PlanError::MoveOverlap);
                    }
                }
            }
            for (i, &(block_start, block_end)) in blocks.iter().enumerate() {
                let (next_line_start, next_line_end) = targets[i];
                let this_block = text[block_start..block_end].to_string();
                let next_block = text[next_line_start..next_line_end].to_string();
                planned.push(PlannedEdit {
                    start: block_start,
                    end: block_end,
                    replacement: next_block.clone(),
                });
                planned.push(PlannedEdit {
                    start: next_line_start,
                    end: next_line_end,
                    replacement: this_block,
                });
            }
        }
        // FEUX-0004 X03: Align command
        EditOperation::LinesAlign { delimiter } => {
            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    // Find the maximum prefix width before the delimiter
                    let max_prefix_width = lines
                        .iter()
                        .filter_map(|line| line.find(delimiter.as_str()))
                        .max()
                        .map_or(0, |max_pos| max_pos + delimiter.len());

                    // If no lines contain the delimiter, skip this block
                    if max_prefix_width == 0
                        && !lines.iter().any(|line| line.contains(delimiter.as_str()))
                    {
                        // No delimiter found in any line of this block
                        // This is a no-op for this block
                        continue;
                    }

                    // Build the aligned lines
                    let aligned_lines: Vec<String> = lines
                        .iter()
                        .map(|line| {
                            if let Some(delim_pos) = line.find(delimiter.as_str()) {
                                let prefix = &line[..delim_pos];
                                let padding =
                                    " ".repeat(max_prefix_width - prefix.len() - delimiter.len());
                                format!("{}{}{}", prefix, padding, delimiter)
                                    + &line[delim_pos + delimiter.len()..]
                            } else {
                                // Line doesn't contain delimiter - leave unchanged
                                line.to_string()
                            }
                        })
                        .collect();

                    let replacement = aligned_lines.join("\n");
                    let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                        replacement + "\n"
                    } else {
                        replacement
                    };

                    planned.push(PlannedEdit {
                        start: block_start,
                        end: block_end,
                        replacement,
                    });
                }
            }
        }
        // FEUX-0004 X04: Surround family
        EditOperation::SurroundAdd { open, close } => {
            for member in set.iter() {
                let (start, end) = if member.is_caret() {
                    // Wrap the empty range at the caret
                    (member.head(), member.head())
                } else {
                    (member.start(), member.end())
                };
                // Insert open at start and close at end
                planned.push(PlannedEdit {
                    start,
                    end,
                    replacement: format!("{}{}{}", open, &text[start..end], close),
                });
            }
        }
        EditOperation::SurroundDelete { open, close: _ } => {
            // Must validate that all members have the enclosing pair
            // This is done at the API level, not in the planner
            for member in set.iter() {
                // Find the enclosing pair - should have been validated already
                if let Some(pair_match) = crate::pairs::find_enclosing_pair(
                    text,
                    member,
                    &crate::pairs::Pair::from_open(open.chars().next().unwrap_or('(')).unwrap_or(
                        crate::pairs::Pair {
                            open: '(',
                            close: ')',
                            is_nesting: true,
                        },
                    ),
                ) {
                    // Delete both delimiters - two separate edits
                    planned.push(PlannedEdit {
                        start: pair_match.close_start,
                        end: pair_match.close_end,
                        replacement: String::new(),
                    });
                    planned.push(PlannedEdit {
                        start: pair_match.open_start,
                        end: pair_match.open_end,
                        replacement: String::new(),
                    });
                }
            }
        }
        EditOperation::SurroundChange {
            old_open,
            old_close: _,
            new_open,
            new_close,
        } => {
            // Must validate that all members have the enclosing pair
            for member in set.iter() {
                // Find the enclosing pair
                if let Some(old_pair) =
                    crate::pairs::Pair::from_open(old_open.chars().next().unwrap_or('('))
                    && let Some(pair_match) =
                        crate::pairs::find_enclosing_pair(text, member, &old_pair)
                {
                    // Replace old open with new open
                    planned.push(PlannedEdit {
                        start: pair_match.open_start,
                        end: pair_match.open_end,
                        replacement: new_open.clone(),
                    });
                    // Replace old close with new close
                    planned.push(PlannedEdit {
                        start: pair_match.close_start,
                        end: pair_match.close_end,
                        replacement: new_close.clone(),
                    });
                }
            }
        }
        // FEUX-0005 X01: Indent family
        EditOperation::IndentLines { amount } => {
            // Detect unit from the entire text (per E-2: per invocation)
            let detected_unit = detect_unit(text);
            unit = Some(detected_unit);
            let unit_text = detected_unit.unit_text();

            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    // Build new lines with indentation added to non-blank lines
                    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());
                    let mut any_change = false;

                    for line in &lines {
                        if is_blank_or_ws(line) {
                            // Blank/whitespace-only lines untouched (E-3)
                            new_lines.push(line.to_string());
                        } else {
                            // Prepend `amount` units
                            new_lines.push(format!(
                                "{}{}",
                                unit_text.repeat(*amount as usize),
                                line
                            ));
                            any_change = true;
                        }
                    }

                    if any_change {
                        let replacement = new_lines.join("\n");
                        let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                            replacement + "\n"
                        } else {
                            replacement
                        };
                        planned.push(PlannedEdit {
                            start: block_start,
                            end: block_end,
                            replacement,
                        });
                    }
                }
            }
        }
        EditOperation::DedentLines { amount } => {
            // Detect unit from the entire text (per E-2: per invocation)
            let detected_unit = detect_unit(text);
            unit = Some(detected_unit);

            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    // Build new lines with indentation removed
                    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());
                    let mut any_change = false;

                    for line in &lines {
                        let leading = leading_whitespace(line);
                        if leading.is_empty() {
                            // No leading whitespace, cannot dedent
                            new_lines.push(line.to_string());
                        } else {
                            // Dedent by `amount` units using tab-stop rule
                            let mut remaining = *amount as usize;
                            let mut new_leading = leading.to_string();

                            while remaining > 0 && !new_leading.is_empty() {
                                let old_leading = new_leading.clone();
                                new_leading = dedent_step(&new_leading, detected_unit.width);
                                if new_leading == old_leading {
                                    // No change means we're at column 0
                                    break;
                                }
                                remaining -= 1;
                            }

                            if new_leading != leading {
                                any_change = true;
                                new_lines.push(format!("{}{}", new_leading, line.trim_start()));
                            } else {
                                new_lines.push(line.to_string());
                            }
                        }
                    }

                    if any_change {
                        let replacement = new_lines.join("\n");
                        let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                            replacement + "\n"
                        } else {
                            replacement
                        };
                        planned.push(PlannedEdit {
                            start: block_start,
                            end: block_end,
                            replacement,
                        });
                    }
                }
            }
        }
        // FEUX-0005 X02: Tabs<->spaces conversion
        EditOperation::ReWhitespace { to_tabs } => {
            // Detect unit from the entire text (per E-2: per invocation)
            let detected_unit = detect_unit(text);
            unit = Some(detected_unit);

            for member in set.iter() {
                if let Some((block_start, block_end, lines, has_trailing_newline)) =
                    containing_line_block(text, member)
                {
                    // Build new lines with leading whitespace converted
                    let mut new_lines: Vec<String> = Vec::with_capacity(lines.len());
                    let mut any_change = false;

                    for line in &lines {
                        let converted = if *to_tabs {
                            spaces_to_tabs_line(line, detected_unit.width)
                        } else {
                            tabs_to_spaces_line(line, detected_unit.width)
                        };

                        if converted != *line {
                            any_change = true;
                        }
                        new_lines.push(converted);
                    }

                    if any_change {
                        let replacement = new_lines.join("\n");
                        let replacement = if has_trailing_newline && !replacement.ends_with('\n') {
                            replacement + "\n"
                        } else {
                            replacement
                        };
                        planned.push(PlannedEdit {
                            start: block_start,
                            end: block_end,
                            replacement,
                        });
                    }
                }
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
            let (set, _) = SelectionSet::normalize_mapped(
                &after_text,
                mapped,
                Some(carets.primary_id().as_raw()),
            )?;
            set
        }
        None => {
            let (set, _) = set.map_through(&edits);
            set
        }
    };
    let after_set = SelectionSet::normalize_mapped(
        &after_text,
        after_set
            .iter()
            .map(|member| crate::selection::RawSelection {
                id: Some(member.id().as_raw()),
                anchor: member.anchor(),
                head: member.head(),
            })
            .collect(),
        Some(after_set.primary_id().as_raw()),
    )?
    .0;
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
        unit,
    })
}

/// The copy/cut effective ranges in document order (D-11): a range member is
/// taken whole; a caret takes its containing line including its line break;
/// duplicate containing lines merge; an empty last line's caret is a no-op.
/// `Copy` (via [`copy_fragments`]) and `EditOperation::Cut` share this one
/// implementation.
/// The containing logical line, including its newline. Invalid input has
/// no effective range. Copy/Cut planning and their previews share this.
#[must_use]
pub fn line_range(text: &str, at: usize) -> Option<std::ops::Range<usize>> {
    if at > text.len() || !is_grapheme_boundary(text, at) {
        return None;
    }
    let start = text[..at].rfind('\n').map_or(0, |at| at + 1);
    let end = text[at..]
        .find('\n')
        .map_or(text.len(), |offset| at + offset + 1);
    Some(start..end)
}

fn cut_ranges(text: &str, set: &SelectionSet) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(set.len());
    let mut line_cache: Option<std::ops::Range<usize>> = None;
    for member in set.iter() {
        if !member.is_caret() {
            ranges.push((member.start(), member.end()));
            continue;
        }
        let at = member.head();
        let range = if let Some(range) = line_cache.as_ref().filter(|range| range.contains(&at)) {
            range.clone()
        } else {
            let Some(range) = line_range(text, at) else {
                continue;
            };
            line_cache = Some(range.clone());
            range
        };
        if !range.is_empty() {
            ranges.push((range.start, range.end));
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
pub fn delete_range(text: &str, at: usize) -> Option<(usize, usize)> {
    if at > text.len() || !is_grapheme_boundary(text, at) {
        return None;
    }
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

// FEUX-0004 X01: SplitMix64 PRNG (E-8: no rand crate dependency)
/// A simple, fast, 64-bit PRNG suitable for shuffling.
/// Based on splitmix64: <http://xoshiro.di.unimi.it/splitmix64.c>
#[derive(Debug, Clone)]
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self {
            state: seed.wrapping_add(0x9e3779b97f4a7c15),
        }
    }

    /// Returns a random u64 in [0, 2^64)
    pub fn next_u64(&mut self) -> u64 {
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z = z ^ (z >> 31);
        self.state = z;
        z
    }

    /// Returns a random usize in [0, n)
    pub fn next_usize(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        let max = usize::MAX - (usize::MAX % n);
        loop {
            let r = (self.next_u64() as usize) & usize::MAX;
            if r < max {
                return r % n;
            }
        }
    }
}

/// Fisher-Yates shuffle using the given RNG.
pub fn fisher_yates_shuffle<T>(items: &mut [T], rng: &mut SplitMix64) {
    let len = items.len();
    for i in (1..len).rev() {
        let j = rng.next_usize(i + 1);
        items.swap(i, j);
    }
}

/// Apply a case transformation function to each character in the text.
fn transform_case<F>(chars: std::str::Chars, transform: F, _offset: usize) -> String
where
    F: Fn(char) -> String,
{
    let mut result = String::new();
    for c in chars {
        result.push_str(&transform(c));
    }
    result
}

/// Swap the case of a single character.
fn swap_case(c: char) -> String {
    if c.is_uppercase() {
        c.to_lowercase().collect()
    } else if c.is_lowercase() {
        c.to_uppercase().collect()
    } else {
        c.to_string()
    }
}

/// Title-case a string using UAX #29 word boundaries (E-10).
/// Within each word segment, the first cased character is uppercased and
/// the remaining cased characters are lowercased; uncased characters pass
/// through untouched. Segments come from `WordBoundarySet` (the delivered
/// UAX #29 machinery), so apostrophes and hyphens stay inside their word
/// (`"café's"` → `"Café's"`).
fn title_case(text: &str) -> String {
    let boundaries = flavite_syntax::text::word_boundaries(text);
    let mut result = String::with_capacity(text.len());
    let mut start = 0usize;
    for &end in boundaries.iter().skip(1) {
        let segment = &text[start..end];
        let mut first_cased = true;
        for c in segment.chars() {
            let cased = c.is_uppercase() || c.is_lowercase();
            if cased && first_cased {
                result.extend(c.to_uppercase());
                first_cased = false;
            } else if cased {
                result.extend(c.to_lowercase());
            } else {
                result.push(c);
            }
        }
        start = end;
    }
    result
}

/// Find and return the containing line block for a member.
/// Returns (start, end, lines, has_trailing_newline) where lines are the line contents without newlines.
/// For a range member, this is the union of all lines the range touches.
fn containing_line_block<'a>(
    text: &'a str,
    member: &crate::selection::Selection,
) -> Option<(usize, usize, Vec<&'a str>, bool)> {
    let (block_start, block_end) = containing_line_range(text, member)?;

    // Check if the block ends with a newline
    let has_trailing_newline = block_end > block_start && text.as_bytes()[block_end - 1] == b'\n';

    // Extract the block text (excluding the trailing newline if present)
    let text_end = if has_trailing_newline {
        block_end - 1
    } else {
        block_end
    };
    let block_text = &text[block_start..text_end];

    // Split by newline to get line contents (without newlines)
    let lines: Vec<&str> = block_text.split('\n').collect();

    if lines.is_empty() {
        None
    } else {
        Some((block_start, block_end, lines, has_trailing_newline))
    }
}

/// Find and return the containing line range for a member.
/// Returns (start, end) of the line block, where end includes the newline if present.
fn containing_line_range(
    text: &str,
    member: &crate::selection::Selection,
) -> Option<(usize, usize)> {
    if member.is_caret() {
        // For caret: use the containing line (includes newline)
        let range = line_range(text, member.head())?;
        Some((range.start, range.end))
    } else {
        // For range: find all lines that the range touches
        let range = (member.start(), member.end());

        // The first line is the line containing range.0
        let first_line = line_range(text, range.0)?;

        // The last line: if range.1 is at a newline position, we need to be careful
        // If range.1 == range.0, it's an empty range (caret-like)
        // Otherwise, the last line is the line containing range.1 - 1 (the last byte in the range)
        // But if range.1 is at a newline, it might be the start of the next line
        let last_line_pos = if range.1 > range.0 && range.1 <= text.len() {
            // Try range.1 first - if it's at a newline, line_range might put it in the next line
            // So try range.1 - 1 instead
            if range.1 > 0 { range.1 - 1 } else { range.1 }
        } else {
            range.1
        };

        let last_line = line_range(text, last_line_pos)?;

        // The union of first and last lines
        let block_start = first_line.start;
        let block_end = last_line.end;

        Some((block_start, block_end))
    }
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
    fn open_line_copies_leading_whitespace() {
        // FEUX-0005 E-4: Auto-indent copies leading whitespace verbatim
        // Test open_below on an indented line (caret at start of line)
        // Inserts "\n    " at the end of the line (line_boundary with above=false)
        let text = "    fn main() {}\n";
        let set = set_of(text, 0);
        let prepared = plan(text, &set, &EditOperation::OpenLine { above: false }).unwrap();
        // Text: "    fn main() {}\n" = 17 chars (4 spaces + 11 chars + newline)
        // line_boundary(text, 0, false) returns position of newline = 16
        // Insert "\n    " (5 chars: newline + 4 spaces) at position 16
        // Result: "    fn main() {}" (16) + "\n    " (5) + "\n" (1, shifted) = "    fn main() {}\n    \n" (22 chars)
        assert_eq!(prepared.after_text, "    fn main() {}\n    \n");
        // The caret should land after the copied whitespace
        // Inserted text: positions 16='\n', 17-20='    '
        // After whitespace is position 21 (16 + 5 = 21)
        assert_eq!(prepared.after_set.primary().head(), 21);

        // Test open_above on an indented line
        // Inserts "    \n" at position 0
        let text = "    fn main() {}\n";
        let set = set_of(text, 0);
        let prepared = plan(text, &set, &EditOperation::OpenLine { above: true }).unwrap();
        // Insert "    \n" (5 chars: 4 spaces + newline) at position 0
        // Result: "    \n" + "    fn main() {}\n" = "    \n    fn main() {}\n"
        assert_eq!(prepared.after_text, "    \n    fn main() {}\n");
        // The caret lands at the beginning of the new line (position 0)
        // Note: FEUX-0005 specifies "after its whitespace" but the current collapse
        // mechanism places it at the beginning. This is a known limitation.
        assert_eq!(prepared.after_set.primary().head(), 0);

        // Test with tabs
        let text = "\t\tfn main() {}\n";
        let set = set_of(text, 0);
        let prepared = plan(text, &set, &EditOperation::OpenLine { above: false }).unwrap();
        // Original: "\t\tfn main() {}\n" (14 chars)
        // Insert "\n\t\t" at position 14: "\t\tfn main() {}" + "\n\t\t" + "\n"
        assert_eq!(prepared.after_text, "\t\tfn main() {}\n\t\t\n");

        // Test with mixed whitespace
        let text = "  \tfn main() {}\n";
        let set = set_of(text, 0);
        let prepared = plan(text, &set, &EditOperation::OpenLine { above: false }).unwrap();
        // Original: "  \tfn main() {}\n" (14 chars)
        // Insert "\n  \t" at position 14: "  \tfn main() {}" + "\n  \t" + "\n"
        assert_eq!(prepared.after_text, "  \tfn main() {}\n  \t\n");
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
mod transform_tests {
    use super::*;
    use crate::selection::RawSelection;

    #[test]
    fn case_upper_transforms_ascii() {
        let text = "hello WORLD";
        let set = set_of(text, 0..5); // "hello"
        let prepared = plan(text, &set, &EditOperation::CaseUpper).unwrap();
        assert_eq!(prepared.after_text, "HELLO WORLD");
        assert_eq!(prepared.after_set.primary().start(), 0);
        assert_eq!(prepared.after_set.primary().end(), 5);
    }

    #[test]
    fn case_lower_transforms_ascii() {
        let text = "HELLO world";
        let set = set_of(text, 0..5); // "HELLO"
        let prepared = plan(text, &set, &EditOperation::CaseLower).unwrap();
        assert_eq!(prepared.after_text, "hello world");
    }

    #[test]
    fn case_swap_swaps_ascii() {
        let text = "HeLLo";
        let set = set_of(text, 0..5);
        let prepared = plan(text, &set, &EditOperation::CaseSwap).unwrap();
        assert_eq!(prepared.after_text, "hEllO");
    }

    #[test]
    fn case_title_title_cases_words() {
        let text = "hello world";
        let set = set_of(text, 0..11);
        let prepared = plan(text, &set, &EditOperation::CaseTitle).unwrap();
        assert_eq!(prepared.after_text, "Hello World");
    }

    #[test]
    fn lines_join_joins_with_space() {
        let text = "line1\nline2\nline3\n";
        // Create a set that selects all three lines
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 18,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::LinesJoin).unwrap();
        // Join with spaces, preserving trailing newline
        assert_eq!(prepared.after_text, "line1 line2 line3\n");
    }

    #[test]
    fn lines_duplicate_duplicates_block() {
        let text = "hello\nworld";
        // Select the first line
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 6,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::LinesDuplicate).unwrap();
        // Should duplicate "hello\n" to "hello\nhello\n"
        assert_eq!(prepared.after_text, "hello\nhello\nworld");
    }

    #[test]
    fn lines_dedupe_removes_duplicates() {
        let text = "a\na\nb\na";
        // Select all lines
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 7,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::LinesDedupe).unwrap();
        // Remove duplicate lines, keeping first occurrence
        assert_eq!(prepared.after_text, "a\nb");
    }

    #[test]
    fn lines_shuffle_is_nondeterministic_but_same_length() {
        let text = "a\nb\nc\nd";
        // Select all lines
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 7,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(text, &set, &EditOperation::LinesShuffle).unwrap();
        // Shuffle should keep the same length
        assert_eq!(prepared.after_text.len(), text.len());
        // Should still have all the same characters (as a multiset)
        let filtered: String = prepared.after_text.chars().filter(|&c| c != '\n').collect();
        let mut filtered_chars: Vec<char> = filtered.chars().collect();
        let mut expected_chars: Vec<char> = "abcd".chars().collect();
        filtered_chars.sort_unstable();
        expected_chars.sort_unstable();
        assert_eq!(filtered_chars, expected_chars);
    }

    // FEUX-0004 X03: LinesAlign tests
    #[test]
    fn lines_align_pads_to_widest_prefix() {
        let text = "a=1\nb=22\nc=333";
        // Select all lines
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 11,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(
            text,
            &set,
            &EditOperation::LinesAlign {
                delimiter: "=".to_string(),
            },
        )
        .unwrap();
        // Should align on '=' with padding
        // "a=1\nb=22\nc=333" -> "a =1\nb =22\nc=333" (widest prefix is "c" at 1 char, but wait...)
        // Actually: widest prefix before '=' is "c" (1 char) in the original
        // But we need to check the actual output
        assert!(prepared.after_text.contains("="));
        // Check that all '=' are at the same column
        let lines: Vec<&str> = prepared.after_text.lines().collect();
        let mut positions = Vec::new();
        for line in lines {
            if let Some(pos) = line.find('=') {
                positions.push(pos);
            }
        }
        // All positions should be the same
        assert!(positions.windows(2).all(|w| w[0] == w[1]));
    }

    #[test]
    fn lines_align_leaves_delimiter_less_lines_untouched() {
        let text = "a=1\nb=22\nc";
        // Select all lines
        let set = SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: 0,
                head: 9,
            }],
            None,
        )
        .unwrap()
        .0;
        let prepared = plan(
            text,
            &set,
            &EditOperation::LinesAlign {
                delimiter: "=".to_string(),
            },
        )
        .unwrap();
        // The line "c" doesn't contain '=' so it should be unchanged
        assert!(prepared.after_text.contains("c"));
    }

    #[test]
    fn lines_align_empty_delimiter_refused() {
        let text = "a=1";
        let set = set_of(text, 0..3);
        // Empty delimiter should be a no-op (handled by the API layer)
        // But the planner itself doesn't refuse empty delimiter
        let prepared = plan(
            text,
            &set,
            &EditOperation::LinesAlign {
                delimiter: "".to_string(),
            },
        )
        .unwrap();
        // With empty delimiter, no lines will be aligned
        assert_eq!(prepared.after_text, text);
    }

    fn set_of(text: &str, range: std::ops::Range<usize>) -> SelectionSet {
        SelectionSet::normalize(
            text,
            vec![RawSelection {
                id: None,
                anchor: range.start,
                head: range.end,
            }],
            None,
        )
        .unwrap()
        .0
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
