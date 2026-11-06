//! Raw input captured in event order, interpreted against acknowledged host text.
//! No speculative text buffer/history is kept in a widget between redraws.
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
}
#[derive(Debug)]
pub enum PreparedInput {
    Edit {
        action: EditorAction,
        caret: usize,
        clipboard: Option<String>,
    },
    History(EditorAction),
    Selection {
        anchor: usize,
        head: usize,
    },
    VerticalSelection {
        anchor: usize,
        head: usize,
        preferred_x: f32,
    },
    Copy(String),
}
/// Build positions only from the current authoritative snapshot and per-view
/// selection. The host must first validate this capture's origin/revision chain.
pub fn prepare(
    view: &EditorView,
    selection: (usize, usize),
    input: &CapturedInput,
) -> Result<PreparedInput, &'static str> {
    let text = &view.text;
    let (anchor, head) = selection;
    if !text.is_char_boundary(anchor) || !text.is_char_boundary(head) {
        return Err("Selection changed; place the caret again");
    }
    let (start, end) = (anchor.min(head), anchor.max(head));
    let previous = |at: usize| crate::viewport::previous_grapheme_boundary(text, at);
    let next = |at: usize| crate::viewport::next_grapheme_boundary(text, at);
    let replace = |from, to, replacement: String, clipboard: Option<String>| {
        if from == to && replacement.is_empty() {
            clipboard.map_or(
                PreparedInput::Selection { anchor, head },
                PreparedInput::Copy,
            )
        } else {
            PreparedInput::Edit {
                caret: from + replacement.len(),
                action: EditorAction::replace(view, from, to, replacement),
                clipboard,
            }
        }
    };
    if input.action == KeyAction::Paste && input.text.is_empty() {
        return Ok(PreparedInput::Selection { anchor, head });
    }
    Ok(match input.action {
        KeyAction::Insert | KeyAction::Newline | KeyAction::Paste => replace(
            start,
            end,
            if input.action == KeyAction::Newline {
                "\n".into()
            } else {
                input.text.clone()
            },
            None,
        ),
        KeyAction::Backspace | KeyAction::BackspaceWord => replace(
            if start != end {
                start
            } else if input.action == KeyAction::BackspaceWord {
                crate::viewport::previous_word_boundary(text, start)
            } else {
                previous(start)
            },
            end,
            String::new(),
            None,
        ),
        KeyAction::Delete | KeyAction::DeleteWord => replace(
            start,
            if start != end {
                end
            } else if input.action == KeyAction::DeleteWord {
                crate::viewport::next_word_boundary(text, end)
            } else {
                next(end)
            },
            String::new(),
            None,
        ),
        KeyAction::Cut => replace(start, end, String::new(), Some(text[start..end].into())),
        KeyAction::Copy => PreparedInput::Copy(text[start..end].into()),
        KeyAction::Undo | KeyAction::Redo => {
            PreparedInput::History(EditorAction::history(view, input.action == KeyAction::Redo))
        }
        KeyAction::SelectAll => PreparedInput::Selection {
            anchor: 0,
            head: text.len(),
        },
        KeyAction::Up | KeyAction::Down | KeyAction::PageUp | KeyAction::PageDown => {
            let motion = input
                .vertical
                .ok_or("Vertical navigation needs editor geometry")?;
            let (at, preferred_x) = crate::navigation::vertical_target(text, head, motion);
            PreparedInput::VerticalSelection {
                anchor: if input.extend { anchor } else { at },
                head: at,
                preferred_x,
            }
        }
        KeyAction::Previous
        | KeyAction::Next
        | KeyAction::PreviousWord
        | KeyAction::NextWord
        | KeyAction::Home
        | KeyAction::End => {
            let at = match input.action {
                KeyAction::Previous if start != end && !input.extend => start,
                KeyAction::Next if start != end && !input.extend => end,
                KeyAction::Previous => previous(head),
                KeyAction::Next => next(head),
                KeyAction::PreviousWord => crate::viewport::previous_word_boundary(text, head),
                KeyAction::NextWord => crate::viewport::next_word_boundary(text, head),
                KeyAction::Home if input.document_boundary => 0,
                KeyAction::End if input.document_boundary => text.len(),
                KeyAction::Home => text[..head].rfind('\n').map_or(0, |at| at + 1),
                KeyAction::End => text[head..].find('\n').map_or(text.len(), |at| head + at),
                _ => head,
            };
            PreparedInput::Selection {
                anchor: if input.extend { anchor } else { at },
                head: at,
            }
        }
        KeyAction::Ignore => return Err("Unsupported input"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn word_selection_and_deletion_use_authoritative_utf8_offsets() {
        let view = view();
        let mut key = input(KeyAction::NextWord);
        key.extend = true;
        assert!(matches!(
            prepare(&view, (0, 4), &key).unwrap(),
            PreparedInput::Selection { anchor: 0, head: 9 }
        ));
        key.action = KeyAction::BackspaceWord;
        let PreparedInput::Edit { action, caret, .. } = prepare(&view, (9, 9), &key).unwrap()
        else {
            panic!("edit")
        };
        assert_eq!(caret, 4);
        let (_, _, edits) = action.as_edit().unwrap();
        assert_eq!((edits[0].range.start.0, edits[0].range.end.0), (4, 9));
        // An existing selection takes precedence over the word modifier.
        let PreparedInput::Edit { action, .. } = prepare(&view, (0, 3), &key).unwrap() else {
            panic!("edit")
        };
        assert_eq!(action.as_edit().unwrap().2[0].range.end.0, 3);
    }

    #[test]
    fn vertical_selection_is_prepared_from_the_current_caret() {
        let view = view();
        let mut key = input(KeyAction::Down);
        key.extend = true;
        key.vertical = Some(crate::navigation::VerticalMotion {
            rows: 1,
            width: 500.0,
            metrics: crate::TypeMetrics::default(),
            preferred_x: Some(0.0),
        });
        assert!(matches!(
            prepare(&view, (0, 0), &key).unwrap(),
            PreparedInput::VerticalSelection {
                anchor: 0,
                head: 10,
                ..
            }
        ));
        assert!(matches!(
            prepare(&view, (0, 10), &key).unwrap(),
            PreparedInput::VerticalSelection {
                anchor: 0,
                head: 10,
                ..
            }
        ));
        assert!(prepare(&view, (usize::MAX, 0), &key).is_err());
    }
}
