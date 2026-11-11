//! Selection algebra property tests (FEUX-0002 X03, AC-3/AC-6): an
//! independent interval-union oracle, idempotence, map_through against a
//! sequential text simulator, and grapheme-boundary validation over Unicode
//! corpora.

use flavite_buffer::selection::{
    Affinity, MAX_SELECTIONS, RawSelection, SelectionSet, SetError, SpanEdit,
};
use proptest::prelude::*;

/// Independent oracle: merge candidates into canonical members.
///
/// Rules (same semantics, independent algorithm): sort by span; merge any
/// two members whose spans overlap (nonempty intersection), OR where one is
/// a caret inside/on the closed span of the other; the survivor keeps the
/// earliest id. Adjacent nonempty spans stay separate.
fn oracle(candidates: &[(u32, usize, usize)]) -> Vec<(u32, usize, usize)> {
    let mut members: Vec<(u32, usize, usize)> = candidates.to_vec();
    loop {
        let mut changed = false;
        'outer: for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let a = members[i];
                let b = members[j];
                let (a_from, a_to) = (a.1.min(a.2), a.1.max(a.2));
                let (b_from, b_to) = (b.1.min(b.2), b.1.max(b.2));
                let a_caret = a_from == a_to;
                let b_caret = b_from == b_to;
                let overlaps = a_from < b_to && b_from < a_to;
                let caret_inside = (a_caret && b_from <= a_from && a_from <= b_to)
                    || (b_caret && a_from <= b_from && b_from <= a_to);
                // Equal carets merge; a caret exactly on a nonempty span's
                // boundary merges into it; adjacent nonempty spans do not.
                let caret_at_edge = (a_caret
                    && (a_from == b_from || a_from == b_to || b_from < a_from && a_from < b_to))
                    || (b_caret
                        && (b_from == a_from
                            || b_from == a_to
                            || a_from < b_from && b_from < a_to));
                let merge = overlaps || caret_inside || caret_at_edge;
                if merge {
                    // The survivor keeps the earliest id among members that
                    // contributed a real span; a merged-in caret contributes
                    // no span and does not compete (FEUX-0002 §5.2).
                    let mut span_ids = Vec::new();
                    let mut all_ids = Vec::new();
                    for m in [a, b] {
                        all_ids.push(m.0);
                        if m.1 != m.2 {
                            span_ids.push(m.0);
                        }
                    }
                    let id = if span_ids.is_empty() {
                        *all_ids.iter().min().unwrap()
                    } else {
                        *span_ids.iter().min().unwrap()
                    };
                    let from = a_from.min(b_from);
                    let to = a_to.max(b_to);
                    let (anchor, head) = if a.1 <= a.2 { (from, to) } else { (to, from) };
                    members.remove(j);
                    members[i] = (id, anchor, head);
                    changed = true;
                    break 'outer;
                }
            }
        }
        if !changed {
            break;
        }
    }
    members.sort_by_key(|&(id, anchor, head)| (anchor.min(head), anchor.max(head), id));
    members
}

fn arb_text() -> impl Strategy<Value = String> {
    proptest::collection::vec(
        prop_oneof![
            Just("a".to_string()),
            Just("é".to_string()),
            Just("漢".to_string()),
            Just("🇦🇧".to_string()),
            Just("\n".to_string()),
            Just("b".to_string()),
            Just("e\u{301}".to_string()),
        ],
        0..24,
    )
    .prop_map(|units| units.concat())
}

fn grapheme_boundaries(text: &str) -> Vec<usize> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut v: Vec<usize> = text.grapheme_indices(true).map(|(at, _)| at).collect();
    v.push(text.len());
    v
}

proptest! {
    #[test]
    fn normalize_matches_the_independent_oracle(
        text in arb_text(),
        count in 0usize..24,
        seed in any::<u64>(),
    ) {
        let boundaries = grapheme_boundaries(&text);
        if boundaries.is_empty() {
            return Ok(());
        }
        let mut lcg = seed;
        let mut next = move || {
            lcg ^= lcg << 13;
            lcg ^= lcg >> 7;
            lcg ^= lcg << 17;
            lcg
        };
        let mut candidates = Vec::new();
        let mut raw = Vec::new();
        for i in 0..count {
            let a = boundaries[(next() as usize) % boundaries.len()];
            let b = boundaries[(next() as usize) % boundaries.len()];
            let id = i as u32;
            candidates.push((id, a, b));
            raw.push(RawSelection { id: Some(id), anchor: a, head: b });
        }
        if raw.is_empty() {
            prop_assert_eq!(
                SelectionSet::normalize(&text, raw, None),
                Err(SetError::Empty)
            );
            return Ok(());
        }
        let expected = oracle(&candidates);
        let (set, remap) = SelectionSet::normalize(&text, raw, None).unwrap();
        let primary_ok = expected.last().is_none_or(|&(expected_id, ..)| {
            set.primary().id().as_raw() == expected_id
        });
        if !primary_ok || set.len() != expected.len() || set.members().iter().zip(expected.iter()).any(
            |(member, &(expected_id, anchor, head))| {
                (member.start(), member.end()) != (anchor.min(head), anchor.max(head))
                    || member.id().as_raw() != expected_id
            },
        ) {
            eprintln!("FAIL seed={seed} text={text:?} count={count}");
            eprintln!("CANDIDATES={candidates:?}");
            eprintln!("EXPECTED={expected:?}");
            eprintln!("MEMBERS={:?}", set.members());
            eprintln!("REMAP={remap:?}");
        }
        // Same member count and same spans.
        prop_assert_eq!(set.len(), expected.len());
        for (member, &(expected_id, anchor, head)) in set.members().iter().zip(expected.iter()) {
            prop_assert_eq!((member.start(), member.end()), (anchor.min(head), anchor.max(head)));
            // Survivor id is the earliest id in the merged group; the remap
            // maps every member id into a surviving id.
            prop_assert_eq!(member.id().as_raw(), expected_id);
            prop_assert_eq!(remap.map(member.id().as_raw()), expected_id);
        }
        // Primary: last survivor.
        if let Some(&(expected_id, ..)) = expected.last() {
            prop_assert_eq!(set.primary().id().as_raw(), expected_id);
        }
        // Idempotence: normalizing the canonical members' raw form yields
        // the same set.
        let again = set
            .members()
            .iter()
            .map(|m| RawSelection { id: Some(m.id().as_raw()), anchor: m.anchor(), head: m.head() })
            .collect::<Vec<_>>();
        let (set2, _) = SelectionSet::normalize(&text, again, None).unwrap();
        prop_assert_eq!(set2, set);
    }

    #[test]
    fn non_boundary_endpoints_are_refused(
        text in arb_text(),
    ) {
        let boundaries = grapheme_boundaries(&text);
        for at in 0..=text.len() {
            if boundaries.contains(&at) {
                continue;
            }
            let result = SelectionSet::normalize(
                &text,
                vec![RawSelection { id: None, anchor: at, head: at }],
                None,
            );
            match result {
                Err(SetError::NotGraphemeBoundary { at: got }) => {
                    prop_assert_eq!(got, at);
                }
                other => prop_assert!(false, "expected NotGraphemeBoundary, got {other:?}"),
            }
        }
    }

    #[test]
    fn too_many_is_refused_with_the_observed_count(
        text in arb_text(),
    ) {
        let boundaries = grapheme_boundaries(&text);
        if boundaries.is_empty() {
            return Ok(());
        }
        let many = (0..=MAX_SELECTIONS)
            .map(|_| RawSelection { id: None, anchor: 0, head: 0 })
            .collect::<Vec<_>>();
        match SelectionSet::normalize(&text, many, None) {
            Err(SetError::TooMany { observed }) => {
                prop_assert_eq!(observed, MAX_SELECTIONS + 1);
            }
            other => prop_assert!(false, "expected TooMany, got {other:?}"),
        }
    }
}

/// Sequential map_through oracle: apply edits with real replacement texts in
/// order, tracking each member's endpoints with the same affinity rules.
#[derive(Debug, Clone)]
struct SimEdit {
    start: usize,
    end: usize,
    replacement: String,
}

fn sim_map(mut p: usize, edits: &[SimEdit], affinity: Affinity) -> usize {
    let mut shift = 0isize;
    for edit in edits {
        let start = shift_pos(edit.start, shift);
        let end = shift_pos(edit.end, shift);
        if p < start {
            continue;
        }
        if p == start {
            if affinity == Affinity::Right {
                return start + edit.replacement.len();
            }
        } else if p < end {
            return if affinity == Affinity::Right {
                start + edit.replacement.len()
            } else {
                start
            };
        } else {
            p = shift_pos(
                p,
                edit.replacement.len() as isize - (edit.end - edit.start) as isize,
            );
        }
        shift += edit.replacement.len() as isize - (edit.end - edit.start) as isize;
    }
    p
}

fn shift_pos(position: usize, delta: isize) -> usize {
    if delta >= 0 {
        position.saturating_add(delta as usize)
    } else {
        position.saturating_sub((-delta) as usize)
    }
}

fn sim_text(text: &str, edits: &[SimEdit]) -> String {
    let mut out = String::new();
    let mut cursor = 0;
    for edit in edits {
        out.push_str(&text[cursor..edit.start]);
        out.push_str(&edit.replacement);
        cursor = edit.end;
    }
    out.push_str(&text[cursor..]);
    out
}

proptest! {
    #[test]
    fn map_through_agrees_with_the_sequential_simulator(
        text in arb_text(),
        count in 0usize..8,
        seed in any::<u64>(),
    ) {
        let boundaries = grapheme_boundaries(&text);
        if boundaries.is_empty() {
            return Ok(());
        }
        let mut lcg = seed;
        let mut next = move || {
            lcg ^= lcg << 13;
            lcg ^= lcg >> 7;
            lcg ^= lcg << 17;
            lcg
        };
        let mut candidates = Vec::new();
        for _ in 0..count {
            let a = boundaries[(next() as usize) % boundaries.len()];
            let b = boundaries[(next() as usize) % boundaries.len()];
            candidates.push(RawSelection { id: None, anchor: a, head: b });
        }
        if candidates.is_empty() {
            return Ok(());
        }
        let (set, _) = SelectionSet::normalize(&text, candidates, None).unwrap();

        // Generate an ascending non-overlapping edit batch with replacement
        // texts drawn from the same units.
        let units = ["x", "yy", "", "é"];
        let mut edits = Vec::new();
        let mut cursor = 0;
        for _ in 0..(next() % 4) {
            let start = boundaries[(next() as usize) % boundaries.len()].max(cursor);
            let end = boundaries[(next() as usize) % boundaries.len()].max(start);
            let replacement = units[(next() as usize) % units.len()].to_string();
            edits.push(SimEdit { start, end, replacement });
            cursor = end;
        }
        if edits.is_empty() {
            return Ok(());
        }
        let spans: Vec<SpanEdit> = edits
            .iter()
            .map(|e| SpanEdit::new(e.start, e.end, e.replacement.len()))
            .collect();
        let after = sim_text(&text, &edits);
        let (mapped, _) = set.map_through(&spans);

        // The mapped set must validate against the after-text.
        prop_assert!(mapped.validate_against(&after).is_ok());

        // Simulate each original member's endpoints with the same affinities,
        // then apply the independent oracle to the simulated endpoints.
        let mut simulated = Vec::new();
        for member in set.members() {
            let (anchor_aff, head_aff) = if member.anchor() < member.head() {
                (Affinity::Left, Affinity::Right)
            } else if member.anchor() > member.head() {
                (Affinity::Right, Affinity::Left)
            } else {
                (Affinity::Right, Affinity::Right)
            };
            let exp_anchor = sim_map(member.anchor(), &edits, anchor_aff);
            let exp_head = sim_map(member.head(), &edits, head_aff);
            prop_assert!(exp_anchor <= after.len() && exp_head <= after.len());
            simulated.push((member.id().as_raw(), exp_anchor, exp_head));
        }
        let expected = oracle(&simulated);
        prop_assert_eq!(mapped.len(), expected.len());
        for (member, &(_id, anchor, head)) in mapped.members().iter().zip(expected.iter()) {
            prop_assert_eq!(
                (member.start(), member.end()),
                (anchor.min(head), anchor.max(head))
            );
        }
    }
}
