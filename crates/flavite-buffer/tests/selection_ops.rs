//! The pure set-op constructors (FEUX-0002 X05, E-9): rotate/flip/trim/
//! invert exactness, cap refusals, toggle-caret parity with Mod-click, and
//! find-char's typed no-op.

use flavite_buffer::selection::{
    FindCharError, RawSelection, SelectionSet, SetError, add_carets, containing_line, find_char,
    flip_selections, invert_selections, rotate_primary, toggle_caret, trim_selections,
};

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

#[test]
fn rotate_primary_advances_and_wraps() {
    let text = "one two three";
    // Explicit ids: id 2 (the middle member) is the primary.
    let set = SelectionSet::normalize(
        text,
        vec![
            RawSelection {
                id: Some(1),
                anchor: 0,
                head: 3,
            },
            RawSelection {
                id: Some(2),
                anchor: 4,
                head: 7,
            },
            RawSelection {
                id: Some(3),
                anchor: 8,
                head: 13,
            },
        ],
        Some(2),
    )
    .unwrap()
    .0;
    let forward = rotate_primary(&set, true);
    assert_eq!(forward.primary().start(), 8, "next member in set order");
    let wrapped = rotate_primary(&forward, true);
    assert_eq!(wrapped.primary().start(), 0, "forward wraps");
    // Rotating backward from the primary lands on the previous member.
    let backward = rotate_primary(&set, false);
    assert_eq!(backward.primary().start(), 0);
    // A single member rotates to itself.
    let one = set_of(text, &[(0, 3)]);
    assert_eq!(rotate_primary(&one, true).primary().start(), 0);
}

#[test]
fn flip_selections_swaps_direction_and_keeps_identity() {
    let text = "one two";
    let set = SelectionSet::normalize(
        text,
        vec![
            RawSelection {
                id: Some(5),
                anchor: 0,
                head: 3,
            },
            RawSelection {
                id: Some(6),
                anchor: 6,
                head: 4,
            },
        ],
        Some(5),
    )
    .unwrap()
    .0;
    let flipped = flip_selections(text, &set).unwrap();
    assert_eq!(flipped.len(), 2);
    assert_eq!(
        flipped.primary_id().as_raw(),
        5,
        "primary identity survives"
    );
    let primary = flipped.primary();
    assert_eq!((primary.anchor(), primary.head()), (3, 0), "anchor↔head");
    // The already-backward member flips forward.
    let owned = flipped.members();
    let other = owned.iter().find(|m| m.id().as_raw() == 6).unwrap();
    assert_eq!((other.anchor(), other.head()), (4, 6));
}

#[test]
fn trim_selections_trims_whitespace_and_empties_become_carets() {
    let text = "  one  \n\ttwo\t  ";
    let set = set_of(text, &[(0, 7), (8, 15)]);
    let trimmed = trim_selections(text, &set).unwrap();
    let spans: Vec<(usize, usize)> = trimmed
        .members()
        .iter()
        .map(|m| (m.start(), m.end()))
        .collect();
    assert_eq!(&text[spans[0].0..spans[0].1], "one");
    assert_eq!(&text[spans[1].0..spans[1].1], "two");
    // A whitespace-only member becomes a caret at the trim position.
    let ws = set_of("   abc", &[(0, 3)]);
    let trimmed = trim_selections("   abc", &ws).unwrap();
    assert!(trimmed.primary().is_caret());
    assert_eq!(trimmed.primary().head(), 3);
}

#[test]
fn invert_selections_takes_the_line_bounded_complement() {
    let text = "aa bb\ncc dd\nee ff\n";
    // "aa" and "dd" selected (spans 0..2 and 6..8).
    let set = set_of(text, &[(0, 2), (6, 8)]);
    let inverted = invert_selections(text, &set).unwrap();
    let spans: Vec<(usize, usize)> = inverted
        .members()
        .iter()
        .map(|m| (m.start(), m.end()))
        .collect();
    assert_eq!(
        spans,
        vec![(2, 5), (8, 11)],
        "line-bounded: unselected spans of intersected lines (whitespace included); fully-selected lines contribute nothing; inter-line newlines never selected"
    );
    // Inverting twice returns the original spans on this simple layout.
    let round = invert_selections(text, &inverted).unwrap();
    let spans: Vec<(usize, usize)> = round
        .members()
        .iter()
        .map(|m| (m.start(), m.end()))
        .collect();
    assert_eq!(spans, vec![(0, 2), (6, 8)]);
}

#[test]
fn invert_selections_refuses_oversize_and_empty_results() {
    // Every intersected line fully selected: no unselected spans remain.
    let text = "one\ntwo\n";
    let set = set_of(text, &[(0, 4), (4, 8)]);
    assert_eq!(invert_selections(text, &set), Err(SetError::Empty));
    // A caret-only set still inverts to the whole lines (unselected spans).
    let caret = set_of(text, &[(0, 0)]);
    let inverted = invert_selections(text, &caret).unwrap();
    assert_eq!(inverted.primary().end(), 3, "the line excluding its break");
}

#[test]
fn toggle_caret_adds_removes_and_never_empties() {
    let text = "one two three";
    let set = set_of(text, &[(0, 3)]);
    // Add a caret inside the gap.
    let two = toggle_caret(text, &set, 5).unwrap();
    assert_eq!(two.len(), 2);
    // A caret at a range edge is inside for removal (boundary absorption).
    let back = toggle_caret(text, &two, 3).unwrap();
    assert_eq!(back.len(), 1, "boundary caret removes the range");
    // The last member is never removed (toggling inside it is a no-op;
    // toggling outside it adds).
    let lone = toggle_caret(text, &back, 5).unwrap();
    assert_eq!(lone.len(), 1);
    let added = toggle_caret(text, &back, 0).unwrap();
    assert_eq!(added.len(), 2);
    assert_eq!(
        toggle_caret(text, &set, 99),
        Err(SetError::OutOfBounds { at: 99 })
    );
}

#[test]
fn add_carets_merges_and_refuses_the_cap() {
    let text = "one two";
    let set = set_of(text, &[(0, 3)]);
    let merged = add_carets(text, &set, &[4, 4, 7]).unwrap();
    assert_eq!(merged.len(), 3, "duplicate points merge");
    // A 10,000-member set accepts one more candidate only up to the cap:
    // the merged result refuses with the observed count.
    let text = "x".repeat(10_001);
    let candidates: Vec<RawSelection> = (0..10_000)
        .map(|at| RawSelection {
            id: None,
            anchor: at,
            head: at,
        })
        .collect();
    let (set, _) = SelectionSet::normalize(&text, candidates, None).unwrap();
    let error = add_carets(&text, &set, &[10_000]).unwrap_err();
    assert!(matches!(error, SetError::TooMany { observed: 10_001 }));
}

#[test]
fn find_char_scans_from_each_head_and_refuses_whole() {
    let text = "one two one";
    let set = set_of(text, &[(0, 0)]);
    let found = find_char(text, &set, true, 'o', false).unwrap();
    assert_eq!(
        found.primary().head(),
        6,
        "forward scan skips the head's grapheme"
    );
    // Backward from byte 0 finds nothing: the whole command refuses.
    let error = find_char(text, &set, false, 'e', false).unwrap_err();
    assert!(
        matches!(error, FindCharError::NotFound),
        "backward finds nothing"
    );
    // Extend keeps the anchor.
    let ranged = set_of(text, &[(4, 4)]);
    let extended = find_char(text, &ranged, true, 'o', true).unwrap();
    assert_eq!(extended.primary().anchor(), 4);
    assert_eq!(extended.primary().head(), 6);
    // No match on ANY member refuses the whole command.
    let pair = set_of(text, &[(0, 0), (11, 11)]);
    assert!(matches!(
        find_char(text, &pair, true, 'z', false),
        Err(FindCharError::NotFound)
    ));
}

#[test]
fn containing_line_includes_the_break() {
    let text = "ab\ncd\n";
    assert_eq!(containing_line(text, 0), (0, 3));
    assert_eq!(containing_line(text, 1), (0, 3));
    assert_eq!(
        containing_line(text, 4),
        (3, 6),
        "last line keeps its break"
    );
}
