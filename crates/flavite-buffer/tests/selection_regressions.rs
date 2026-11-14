//! Selection and history regressions from the FEUX follow-up review.
use flavite_buffer::{
    Affinity,
    commands::{EditOperation, plan},
    history::{EditKind, EditorEdit, MovementSpan},
    model::EditorModel,
    selection::{
        RawSelection, SelectionSet, map_movement, map_point_through_spans, map_points_through_spans,
    },
    undo::{EditOp, UndoPolicy},
};
use flavite_syntax::{Span, TextByte};

fn insert(model: &mut EditorModel, at: usize, value: &str) {
    model
        .try_apply_batch(
            &[EditOp::new(
                Span::new(TextByte(at), TextByte(at)),
                value.into(),
            )],
            UndoPolicy::Isolated,
        )
        .unwrap();
}

#[test]
fn restore_across_steps_maps_like_individual_undos() {
    let mut model = EditorModel::from_text("abcdefghij");
    let root = model.history_page(None, 100).unwrap().entries[0].target;
    insert(&mut model, 8, "X");
    insert(&mut model, 0, "Y");
    assert_eq!(model.text(), "YabcdefghXij");
    model.restore_state(root).unwrap();
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(
        map_movement(9, movement, Affinity::Right),
        8,
        "native coordinates from successive steps must not be shifted twice"
    );
}

#[test]
fn coalesced_typing_keeps_complete_batch_movement() {
    let mut model = EditorModel::from_text("abcd");
    let (mut set, _) = SelectionSet::normalize(
        "abcd",
        vec![
            RawSelection {
                id: Some(0),
                anchor: 0,
                head: 0,
            },
            RawSelection {
                id: Some(1),
                anchor: 4,
                head: 4,
            },
        ],
        Some(0),
    )
    .unwrap();
    for value in ["X", "Y"] {
        let prepared = plan(model.text(), &set, &EditOperation::Typing(value.into())).unwrap();
        let record = EditorEdit {
            before_primary: 0,
            origin: format!("{:032x}", 1),
            kind: EditKind::Typing,
            before: set.to_triples(),
            after: prepared.after_set.to_triples(),
            primary: 0,
        };
        model
            .try_apply_recorded_batch(&prepared.ops, UndoPolicy::Context { origin: 1 }, record)
            .unwrap();
        set = prepared.after_set;
    }
    assert_eq!(model.text(), "XYabcdXY");
    assert_eq!(model.history_nodes(), 2, "one coalesced typing group");
    model.undo();
    assert_eq!(model.text(), "abcd");
    let movement = model.history_movement().unwrap();
    assert!(!movement.truncated);
    assert_eq!(
        map_movement(3, movement, Affinity::Right),
        1,
        "both typed characters must be accounted for"
    );
}

#[test]
fn replacement_end_has_the_same_bulk_and_scalar_mapping() {
    let spans = [MovementSpan {
        start: 1,
        end: 2,
        replacement_len: 3,
    }];
    let expected = map_point_through_spans(2, &spans, Affinity::Right);
    assert_eq!(expected, 4);
    assert_eq!(
        map_points_through_spans(&[2], &spans, Affinity::Right),
        vec![expected]
    );
}

#[test]
fn typing_adjacent_ranges_keeps_one_caret_after_each_replacement() {
    let (set, _) = SelectionSet::normalize(
        "ab",
        vec![
            RawSelection {
                id: Some(0),
                anchor: 0,
                head: 1,
            },
            RawSelection {
                id: Some(1),
                anchor: 1,
                head: 2,
            },
        ],
        Some(0),
    )
    .unwrap();
    let prepared = plan("ab", &set, &EditOperation::Typing("X".into())).unwrap();
    assert_eq!(prepared.after_text, "XX");
    assert_eq!(
        prepared.after_set.len(),
        2,
        "adjacent replacements must leave separate carets at 1 and 2"
    );
}

#[test]
fn normalization_preserves_the_surviving_sources_direction_and_all_aliases() {
    use flavite_buffer::selection::{RawSelection, SelectionSet};
    let text = "x".repeat(10_001);
    let candidates = (0..10_000)
        .map(|index| RawSelection {
            id: Some(10_000 - index as u32),
            anchor: index + 2,
            head: index,
        })
        .collect();
    let (set, remap) = SelectionSet::normalize(&text, candidates, Some(10_000)).unwrap();
    assert_eq!(set.len(), 1);
    assert_eq!((set.primary().anchor(), set.primary().head()), (10_001, 0));
    for id in 1..=10_000 {
        assert_eq!(remap.map(id), 1);
    }
}

#[test]
fn maximum_supplied_id_cannot_overflow_or_duplicate_synthetic_ids() {
    use flavite_buffer::selection::{RawSelection, SelectionSet};
    let (set, _) = SelectionSet::normalize(
        "abcd",
        vec![
            RawSelection {
                id: Some(u32::MAX),
                anchor: 0,
                head: 0,
            },
            RawSelection {
                id: None,
                anchor: 1,
                head: 1,
            },
            RawSelection {
                id: Some(u32::MAX),
                anchor: 2,
                head: 2,
            },
        ],
        Some(u32::MAX),
    )
    .unwrap();
    let ids: std::collections::HashSet<_> =
        set.members().iter().map(|member| member.id()).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(set.primary_id().as_raw(), u32::MAX);
}

#[test]
fn edits_that_join_graphemes_round_the_result_instead_of_refusing_text() {
    use flavite_buffer::{
        commands::{EditOperation, plan},
        selection::{RawSelection, SelectionSet},
    };
    let text = "👩👩";
    let set = SelectionSet::normalize(
        text,
        vec![RawSelection {
            id: Some(0),
            anchor: 4,
            head: 4,
        }],
        Some(0),
    )
    .unwrap()
    .0;
    let typed = plan(text, &set, &EditOperation::Typing("\u{200d}".into())).unwrap();
    assert_eq!(typed.after_text, "👩‍👩");
    assert_eq!(typed.after_set.primary().head(), typed.after_text.len());
    let text = "🇦x🇧";
    let set = SelectionSet::normalize(
        text,
        vec![RawSelection {
            id: Some(0),
            anchor: 4,
            head: 5,
        }],
        Some(0),
    )
    .unwrap()
    .0;
    let deleted = plan(text, &set, &EditOperation::Delete).unwrap();
    assert_eq!(deleted.after_text, "🇦🇧");
    assert_eq!(deleted.after_set.primary().head(), deleted.after_text.len());
}
