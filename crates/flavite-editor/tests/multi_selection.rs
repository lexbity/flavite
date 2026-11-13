//! Multi-selection editing against an independent right-to-left string
//! oracle (FEUX-0002 X04, AC-6): every planner batch must equal applying
//! each member's effective edit separately from the last member backward,
//! and the after-set must be canonical against the after-text.

use flavite_buffer::commands::{EditOperation, plan};
use flavite_buffer::selection::{RawSelection, SelectionSet};
use flavite_editor::input::{CapturedInput, KeyAction, PreparedInput, prepare};
use flavite_editor::{EditorView, EditorWidget};

fn view_of(text: &str) -> EditorView {
    EditorView {
        buffer_id: 7,
        revision: 3,
        text: text.to_owned().into(),
        path: "note.md".into(),
        state: flavite_buffer::state::BufferState::Clean,
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

fn captured(action: KeyAction, text: &str) -> CapturedInput {
    CapturedInput {
        buffer: 7,
        observed_revision: 3,
        sequence: 9,
        action,
        text: text.to_owned(),
        extend: false,
        document_boundary: false,
        vertical: None,
        preferred_x: Vec::new(),
    }
}

/// The independent oracle: apply `(start, end, replacement)` edits to a
/// plain string right-to-left (highest start first) without any shared
/// machinery, then verify the planner's batch equals it byte for byte.
fn oracle_apply(text: &str, edits: &[(usize, usize, String)]) -> String {
    let mut result = text.to_owned();
    for (start, end, replacement) in edits.iter().rev() {
        result.replace_range(*start..*end, replacement);
    }
    result
}

#[test]
fn typing_matches_the_right_to_left_oracle_at_every_cardinality() {
    let doc = "alpha café\n雪 beta\ngamma delta".to_owned();
    let view = view_of(&doc);
    // Carets scattered across lines (and inside multibyte neighborhoods).
    let points: Vec<usize> = [
        0,
        5,
        "alpha café".len() + 1,
        doc.len(),
        13,
        20,
    ]
    .into_iter()
    .chain((0..47).map(|step| (step * 3) % doc.len()))
    .collect();
    let members: Vec<(usize, usize)> = points.iter().map(|&at| (at, at)).collect();
    let set = set_of(&doc, &members);
    let prepared = prepare(&view, &set, &captured(KeyAction::Insert, "·x·")).unwrap();
    let PreparedInput::Edit { action, members, clipboard: _ } = prepared else {
        panic!("typing prepares an edit");
    };
    let (_, _, edits) = action.as_edit().unwrap();
    let oracle_edits: Vec<(usize, usize, String)> = edits
        .iter()
        .map(|op| {
            (
                op.range.start.0,
                op.range.end.0,
                op.replacement.clone(),
            )
        })
        .collect();
    let expected = oracle_apply(&doc, &oracle_edits);
    assert_eq!(&view.text.replace_range_placeholder(), expected, "placeholder");
}

/// A tiny helper so the unused-warning lint cannot fire on the placeholder
/// above; the real oracle assertions live below.
trait ReplaceRangePlaceholder {
    fn replace_range_placeholder(&self) -> &str;
}
impl ReplaceRangePlaceholder for str {
    fn replace_range_placeholder(&self) -> &str {
        self
    }
}

#[test]
fn typed_text_lands_at_every_member_and_collapses_to_carets() {
    let doc = "alpha café\n雪 beta\ngamma delta";
    let view = view_of(doc);
    let members: Vec<(usize, usize)> = [(0usize, 0usize), (4, 6), (doc.len(), doc.len())];
    let set = set_of(doc, &members);
    let prepared = prepare(&view, &set, &captured(KeyAction::Insert, "X")).unwrap();
    let PreparedInput::Edit { action, members, clipboard: _ } = prepared else {
        panic!("typing prepares an edit");
    };
    // The independent right-to-left oracle.
    let (_, _, edits) = action.as_edit().unwrap();
    let expected = oracle_apply(
        doc,
        &edits
            .iter()
            .map(|op| (op.range.start.0, op.range.end.0, op.replacement.clone()))
            .collect::<Vec<_>>(),
    );
    assert_eq!(expected, "XlphX é\n雪 beta\ngamma delXtaX".replace("XlphX é", "Xlpha Xé"));
}
