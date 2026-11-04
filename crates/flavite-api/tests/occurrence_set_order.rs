//! RFC-0005 S4 property gate (AC-7 / §4.7 invariant): `OccurrenceSet`
//! iteration MUST be byte-identical to the retired `saved`-then-
//! `retain`-then-`extend` construction for every dirty-source set — the
//! `(src, ord)` run adjacency the backlinks excerpt grouping relies on
//! (`query_service.rs` `execute_backlinks`) is pinned here, not assumed.
//!
//! Three constructions are compared per case:
//! 1. `OccurrenceSet::iter()` (the production code path);
//! 2. the old algorithm (saved → `retain` per dirty source → append
//!    overlay) — reproduced verbatim;
//! 3. the filter-then-chain reference (saved ∖ dirty, then overlay).
//!
//! `Vec<LinkOccurrence>` equality is elementwise `PartialEq` — a byte
//! comparison for these records.

use flavite_api::graph_projection::{LinkOccurrence, OccurrenceSet, OccurrenceSource};
use flavite_graph::{EdgeKind, GraphEdgeRecord, NodeId};
use proptest::prelude::*;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// The row-id side of a [`NodeId`] — the mirror's documented conversion
/// policy (`node_id_entity`, RFC-0005 §7), reproduced in the test so the
/// order gate does not depend on the implementation under test.
fn source_key(id: NodeId) -> i64 {
    i64::try_from(id.id).unwrap_or(-1)
}

fn occurrence(source: u64, ord: u64, target: u64, unresolved: bool) -> LinkOccurrence {
    let to = if unresolved {
        NodeId::unresolved(target)
    } else {
        NodeId::note(target)
    };
    LinkOccurrence {
        edge: GraphEdgeRecord {
            from: NodeId::note(source),
            to,
            kind: EdgeKind::Link,
        },
        source_path: PathBuf::from(format!("s{source}.md")),
        source: OccurrenceSource::Saved {
            content_hash: vec![source as u8, ord as u8],
        },
        start: usize::try_from(ord * 10).unwrap(),
        end: usize::try_from(ord * 10 + 9).unwrap(),
        target_raw: format!("t{target}"),
    }
}

/// The canonical saved slice: 8 sources, resolved runs of 2 (sources
/// 0..8), plus unresolved runs for source 1 (the two-section layout).
fn canonical_saved() -> Vec<LinkOccurrence> {
    let mut saved = Vec::new();
    for source in 0..8 {
        for ord in 0..2 {
            saved.push(occurrence(source, ord, (source * 7 + ord) % 64, false));
        }
    }
    for ord in 0..2 {
        saved.push(occurrence(1, ord, ord + 100, true));
    }
    saved
}

/// The old algorithm, verbatim: remove every occurrence of each dirty
/// source (retain), then append overlay runs.
fn old_construction(
    saved: &[LinkOccurrence],
    dirty: &BTreeSet<i64>,
    overlay: &[LinkOccurrence],
) -> Vec<LinkOccurrence> {
    let mut result = saved.to_vec();
    for source in dirty {
        result.retain(|occurrence| source_key(occurrence.edge.from) != *source);
    }
    result.extend(overlay.iter().cloned());
    result
}

fn overlay_occurrence(source: u64, ord: u64, target: u64) -> LinkOccurrence {
    LinkOccurrence {
        edge: GraphEdgeRecord {
            from: NodeId::note(source),
            to: NodeId::note(target),
            kind: EdgeKind::Link,
        },
        source_path: PathBuf::from(format!("s{source}.md")),
        source: OccurrenceSource::Buffer {
            buffer_id: source,
            revision: ord,
        },
        start: 0,
        end: 9,
        target_raw: format!("t{target}"),
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1_000))]

    #[test]
    fn occurrence_set_iteration_is_byte_identical_to_the_retired_construction(
        dirty_sources in prop::collection::vec(0u64..8, 0..8),
        overlay_runs in prop::collection::vec((0u64..8, 0u64..64), 0..16),
    ) {
        let saved = canonical_saved();
        let dirty: BTreeSet<i64> = dirty_sources.iter().map(|s| source_key(NodeId::note(*s))).collect();
        let overlay: Vec<LinkOccurrence> = overlay_runs
            .iter()
            .enumerate()
            .map(|(ord, (source, target))| overlay_occurrence(*source, ord as u64, *target))
            .collect();
        let set = OccurrenceSet::from_parts(Arc::from(saved.clone()), dirty.clone(), overlay.clone());

        let actual: Vec<LinkOccurrence> = set.iter().cloned().collect();
        let old = old_construction(&saved, &dirty, &overlay);
        let reference: Vec<LinkOccurrence> = saved
            .iter()
            .filter(|occurrence| !dirty.contains(&source_key(occurrence.edge.from)))
            .cloned()
            .chain(overlay.iter().cloned())
            .collect();

        prop_assert_eq!(&actual, &old, "iter() diverges from the retired retain+extend construction");
        prop_assert_eq!(&actual, &reference, "iter() diverges from the filter-then-chain reference");
        prop_assert_eq!(set.len(), actual.len(), "len() must agree with iter()");
        for (index, expected) in actual.iter().enumerate() {
            prop_assert_eq!(set.get(index), Some(expected), "get(index) must agree with iter()");
        }
        prop_assert_eq!(set.get(actual.len()), None, "get out of range is None");
        for occurrence in &actual {
            prop_assert!(set.contains(occurrence), "contains() must agree with iter()");
        }
    }
}

#[test]
fn no_dirty_sources_iterates_the_saved_runs_verbatim() {
    let saved = canonical_saved();
    let set = OccurrenceSet::from_parts(Arc::from(saved.clone()), BTreeSet::new(), Vec::new());
    let actual: Vec<LinkOccurrence> = set.iter().cloned().collect();
    assert_eq!(actual, saved);
    assert_eq!(set.len(), saved.len());
}

#[test]
fn every_source_dirty_yields_exactly_the_overlay_runs() {
    let saved = canonical_saved();
    let dirty: BTreeSet<i64> = (0..8).map(|s| source_key(NodeId::note(s))).collect();
    let overlay = vec![overlay_occurrence(0, 0, 5), overlay_occurrence(3, 1, 9)];
    let set = OccurrenceSet::from_parts(Arc::from(saved), dirty, overlay.clone());
    let actual: Vec<LinkOccurrence> = set.iter().cloned().collect();
    assert_eq!(actual, overlay);
}

#[test]
fn default_is_empty() {
    let set = OccurrenceSet::default();
    assert!(set.is_empty());
    assert_eq!(set.len(), 0);
    assert_eq!(set.iter().next(), None);
}
