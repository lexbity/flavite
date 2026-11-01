//! Raw input captured in event order, interpreted against acknowledged host text.
//! No speculative text buffer/history is kept in a widget between redraws.
use crate::{EditorAction, EditorView, keymap::KeyAction};
use unicode_segmentation::UnicodeSegmentation;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedInput {
    pub buffer: u64,
    pub observed_revision: u64,
    pub sequence: u64,
    pub action: KeyAction,
    pub text: String,
    pub extend: bool,
    pub document_boundary: bool,
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
    let previous = |at: usize| {
        text[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(at, _)| at)
    };
    let next = |at: usize| {
        text[at..]
            .graphemes(true)
            .next()
            .map_or(text.len(), |g| at + g.len())
    };
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
        KeyAction::Backspace => replace(
            if start == end { previous(start) } else { start },
            end,
            String::new(),
            None,
        ),
        KeyAction::Delete => replace(
            start,
            if start == end { next(end) } else { end },
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
        KeyAction::Previous | KeyAction::Next | KeyAction::Home | KeyAction::End => {
            let at = match input.action {
                KeyAction::Previous if start != end && !input.extend => start,
                KeyAction::Next if start != end && !input.extend => end,
                KeyAction::Previous => previous(head),
                KeyAction::Next => next(head),
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
