//! Raw input captured in event order, interpreted against acknowledged host
//! text. No speculative text buffer/history is kept in a widget between
//! redraws. Interpretation is set-aware (FEUX-0002 X04): every key action is
//! planned per member through the canonical `flavite-buffer` planner, which
//! is the only mutation semantic; this module only translates key actions to
//! planner operations and visual-movement targets.
use flavite_buffer::commands::{EditOperation, copy_fragments, plan};
use flavite_buffer::motion::{self, Motion};
use flavite_buffer::selection::SelectionSet;

use crate::{EditorAction, EditorView, keymap::KeyAction};

#[derive(Debug, Clone, PartialEq)]
pub struct CapturedInput {
    pub buffer: u64,
    pub observed_revision: u64,
    pub sequence: u64,
    pub action: KeyAction,
    pub text: String,
    pub extend: bool,
    pub document_boundary: bool,
    pub vertical: Option<crate::navigation::VerticalMotion>,
    /// Sticky visual column per member, parallel to the selection set at
    /// capture time (X04: vertical motion keeps per-member columns; missing
    /// entries mean "no sticky column").
    pub preferred_x: Vec<Option<f32>>,
}

#[derive(Debug)]
pub enum PreparedInput {
    Edit {
        action: EditorAction,
        /// After-set members for the optimistic pre-receipt caret install
        /// (already validated against the after-text by the planner).
        members: Vec<(usize, usize)>,
        /// Cut text (fragments joined with LF in document order, §5.3).
        clipboard: Option<String>,
    },
    History(EditorAction),
    Selection {
        members: Vec<(usize, usize)>,
    },
    VerticalSelection {
        members: Vec<(usize, usize)>,
        preferred_x: Vec<Option<f32>>,
    },
    Copy {
        text: String,
    },
}

/// The member pairs of a canonical set (host translation shape).
#[must_use]
pub fn member_pairs(set: &SelectionSet) -> Vec<(usize, usize)> {
    set.members()
        .iter()
        .map(|member| (member.anchor(), member.head()))
        .collect()
}

/// The key action's planner operation over the whole set. Copy, history,
/// SelectAll, and motions are handled directly in [`prepare`].
fn operation_of(action: KeyAction, text: &str) -> Option<EditOperation> {
    match action {
        KeyAction::Insert | KeyAction::Newline | KeyAction::Paste => {
            let mut normalized = text.replace("\r\n", "\n");
            if action == KeyAction::Newline {
                normalized = "\n".into();
            }
            Some(EditOperation::Typing(normalized))
        }
        KeyAction::Backspace => Some(EditOperation::Backspace),
        KeyAction::BackspaceWord => Some(EditOperation::BackspaceWord),
        KeyAction::Delete => Some(EditOperation::ForwardDelete),
        KeyAction::DeleteWord => Some(EditOperation::ForwardDeleteWord),
        KeyAction::Cut => Some(EditOperation::Cut),
        _ => None,
    }
}

/// The buffer motion for a horizontal (non-visual) key action.
fn motion_of(action: KeyAction, document_boundary: bool) -> Option<Motion> {
    match action {
        KeyAction::Previous => Some(Motion::CharLeft),
        KeyAction::Next => Some(Motion::CharRight),
        KeyAction::PreviousWord => Some(Motion::WordPrev),
        KeyAction::NextWord => Some(Motion::WordNext),
        KeyAction::NextWordEnd => Some(Motion::WordEnd),
        KeyAction::Home if document_boundary => Some(Motion::DocStart),
        KeyAction::Home => Some(Motion::LineStart),
        KeyAction::End if document_boundary => Some(Motion::DocEnd),
        KeyAction::End => Some(Motion::LineEnd),
        _ => None,
    }
}

/// Build positions only from the current authoritative snapshot and per-view
/// selection set. The host must first validate this capture's origin/revision
/// chain. Every mutation derives its edits from the canonical planner.
///
/// # Errors
///
/// A static message when the action is unsupported.
pub fn prepare(
    view: &EditorView,
    set: &SelectionSet,
    input: &CapturedInput,
) -> Result<PreparedInput, &'static str> {
    let text = &view.text;
    let unchanged = || PreparedInput::Selection {
        members: member_pairs(set),
    };
    if let Some(edit_operation) = operation_of(input.action, input.text.as_str()) {
        let edit_operation = match edit_operation {
            // Paste with an empty clipboard is a no-op (§5.5).
            EditOperation::Typing(ref insertion)
                if insertion.is_empty() && input.action == KeyAction::Paste =>
            {
                return Ok(unchanged());
            }
            op => op,
        };
        let prepared =
            plan(text, set, &edit_operation).map_err(|_| "Selection changed; place the caret again")?;
        if prepared.ops.is_empty() {
            // Zero-effect (e.g. an empty line's Delete): no history node.
            return Ok(unchanged());
        }
        let clipboard = (!prepared.fragments.is_empty())
            .then(|| prepared.fragments.join("\n"));
        return Ok(PreparedInput::Edit {
            action: EditorAction::Edit {
                buffer_id: view.buffer_id,
                expected_revision: view.revision,
                edits: prepared.ops,
            },
            members: member_pairs(&prepared.after_set),
            clipboard,
        });
    }
    if input.action == KeyAction::Copy {
        let fragments = copy_fragments(text, set).map_err(|_| "Selection changed; place the caret again")?;
        return Ok(PreparedInput::Copy {
            text: fragments.join("\n"),
        });
    }
    if matches!(
        input.action,
        KeyAction::Up | KeyAction::Down | KeyAction::PageUp | KeyAction::PageDown
    ) {
        let vertical = input
            .vertical
            .as_ref()
            .ok_or("Vertical navigation needs editor geometry")?;
        let mut members = Vec::with_capacity(set.len());
        let mut preferred_x = Vec::with_capacity(set.len());
        for (index, member) in set.iter().enumerate() {
            let sticky = input
                .preferred_x
                .get(index)
                .copied()
                .flatten();
            let (at, column) =
                crate::navigation::vertical_target(text, member.head(), *vertical, sticky);
            let anchor = if input.extend { member.anchor() } else { at };
            members.push((anchor, at));
            preferred_x.push(Some(column));
        }
        return Ok(PreparedInput::VerticalSelection {
            members,
            preferred_x,
        });
    }
    if let Some(motion) = motion_of(input.action, input.document_boundary) {
        let moved = motion::apply(text, set, motion, input.extend, None)
            .map_err(|_| "Selection changed; place the caret again")?;
        return Ok(PreparedInput::Selection {
            members: member_pairs(&moved),
        });
    }
    match input.action {
        KeyAction::Undo | KeyAction::Redo => Ok(PreparedInput::History(
            EditorAction::history(view, input.action == KeyAction::Redo),
        )),
        KeyAction::SelectAll => Ok(PreparedInput::Selection {
            members: vec![(0, text.len())],
        }),
        _ => Err("Unsupported input"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_buffer::selection::RawSelection;

    fn view() -> EditorView {
        EditorView {
            buffer_id: 1,
            revision: 1,
            text: "one café\n雪 three".into(),
            path: "note.md".into(),
            state: flavite_buffer::state::BufferState::Clean,
        }
    }

    fn input(action: KeyAction) -> CapturedInput {
        CapturedInput {
            buffer: 1,
            observed_revision: 1,
            sequence: 1,
            action,
            text: String::new(),
            extend: false,
            document_boundary: false,
            vertical: None,
            preferred_x: Vec::new(),
        }
    }

    fn set_of(text: &str, members: &[(usize, usize)]) -> SelectionSet {
        let raw: Vec<RawSelection> = members
            .iter()
            .map(|&(anchor, head)| RawSelection {
                id: None,
                anchor,
                head,
            })
            .collect();
        SelectionSet::normalize(text, raw, None).unwrap().0
    }

    fn edit_of(prepared: PreparedInput) -> (EditorAction, Vec<(usize, usize)>, Option<String>) {
        match prepared {
            PreparedInput::Edit {
                action,
                members,
                clipboard,
            } => (action, members, clipboard),
            other => panic!("expected an edit, got {other:?}"),
        }
    }

    #[test]
    fn clipboard_newlines_are_normalized_before_the_planner_runs() {
        let view = view();
        let mut key = input(KeyAction::Paste);
        key.text = "a\r\nb\rc".into();
        let set = set_of(&view.text, &[(0, 0)]);
        let (action, members, clipboard) = edit_of(prepare(&view, &set, &key).unwrap());
        assert_eq!(members, vec![(5, 5)]);
        assert!(clipboard.is_none());
        let (_, _, edits) = action.as_edit().unwrap();
        assert_eq!(edits[0].replacement, "a\nb\rc");
    }

    #[test]
    fn word_selection_and_deletion_use_authoritative_utf8_offsets() {
        let view = view();
        let mut key = input(KeyAction::NextWord);
        key.extend = true;
        let set = set_of(&view.text, &[(0, 4)]);
        let PreparedInput::Selection { members } = prepare(&view, &set, &key).unwrap() else {
            panic!("selection");
        };
        assert_eq!(members, vec![(0, 9)]);
        key.action = KeyAction::BackspaceWord;
        let set = set_of(&view.text, &[(9, 9)]);
        let (action, members, _) = edit_of(prepare(&view, &set, &key).unwrap());
        assert_eq!(members, vec![(4, 4)]);
        let (_, _, edits) = action.as_edit().unwrap();
        assert_eq!((edits[0].range.start.0, edits[0].range.end.0), (4, 9));
        // An existing selection takes precedence over the word modifier.
        let set = set_of(&view.text, &[(0, 3)]);
        let (action, _, _) = edit_of(prepare(&view, &set, &key).unwrap());
        assert_eq!(action.as_edit().unwrap().2[0].range.end.0, 3);
    }

    #[test]
    fn typing_lands_on_every_member_of_the_set_in_one_batch() {
        let view = view();
        let mut key = input(KeyAction::Insert);
        key.text = "_".into();
        let set = set_of(&view.text, &[(0, 0), (4, 6)]);
        let (action, members, _) = edit_of(prepare(&view, &set, &key).unwrap());
        let (_, _, edits) = action.as_edit().unwrap();
        assert_eq!(edits.len(), 2, "one atomic batch");
        assert_eq!((edits[0].range.start.0, edits[0].range.end.0), (0, 0));
        assert_eq!((edits[1].range.start.0, edits[1].range.end.0), (4, 6));
        // Each member collapses past its own insertion (stick right).
        assert_eq!(members, vec![(1, 1), (6, 6)]);
    }

    #[test]
    fn backspace_across_members_merges_post_delete_collisions() {
        let view = view();
        let key = input(KeyAction::Backspace);
        // Two carets on one line, both backspacing onto the same point.
        let set = set_of(&view.text, &[(1, 1), (2, 2)]);
        let (_, members, _) = edit_of(prepare(&view, &set, &key).unwrap());
        assert_eq!(members, vec![(0, 0)], "collided carets merge");
    }

    #[test]
    fn cut_joins_fragments_with_lf_in_document_order() {
        let view = view();
        let key = input(KeyAction::Cut);
        let set = set_of(&view.text, &[(0, 3), (14, 19)]);
        let (_, members, clipboard) = edit_of(prepare(&view, &set, &key).unwrap());
        assert_eq!(clipboard.as_deref(), Some("one\nthree"));
        assert_eq!(members.len(), 2);
        assert_eq!(members[0], (0, 0), "the first caret lands at its cut start");
    }

    #[test]
    fn copy_joins_fragments_with_lf_in_document_order() {
        let view = view();
        let key = input(KeyAction::Copy);
        let set = set_of(&view.text, &[(0, 3), (14, 19)]);
        let PreparedInput::Copy { text } = prepare(&view, &set, &key).unwrap() else {
            panic!("copy");
        };
        assert_eq!(text, "one\nthree");
    }

    #[test]
    fn vertical_selection_is_prepared_per_member() {
        let view = view();
        let mut key = input(KeyAction::Down);
        key.extend = true;
        key.vertical = Some(crate::navigation::VerticalMotion {
            rows: 1,
            width: 500.0,
            metrics: crate::TypeMetrics::default(),
        });
        key.preferred_x = vec![Some(0.0), Some(0.0)];
        // The second caret sits on the second line (after the 雪 grapheme).
        let set = set_of(&view.text, &[(0, 0), (13, 13)]);
        let PreparedInput::VerticalSelection {
            members,
            preferred_x,
        } = prepare(&view, &set, &key).unwrap()
        else {
            panic!("vertical");
        };
        assert_eq!(members.len(), 2);
        assert_eq!(members[0], (0, 10));
        assert_eq!(preferred_x.len(), 2);
    }

    #[test]
    fn zero_effect_deletes_change_nothing() {
        let view = view();
        let key = input(KeyAction::Backspace);
        let set = set_of(&view.text, &[(0, 0)]);
        let PreparedInput::Selection { members } = prepare(&view, &set, &key).unwrap() else {
            panic!("selection");
        };
        assert_eq!(members, vec![(0, 0)]);
    }
}
