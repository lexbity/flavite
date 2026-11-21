//! Structure-path parity through the editor layer (RFC-0008 AC-5 at the
//! widget's routing; RFC-0011 S4/S5/S6).
//!
//! RFC-0011 S6 deleted the tree-sitter engine and its flag: the segment
//! cache ([`SegmentIndex`], RFC-0011 S5) is the only structure producer.
//! This test proves that producer's output is byte-identical to the
//! whole-document pulldown bridge (`links::spans_from_pulldown`) over the
//! acceptance corpus — through the exact conversion the widget uses — the
//! semantics record that survives the engine. The corpus lives in
//! flavite-syntax; the documentation file is excluded as in the S5 parity
//! harness.

use flavite_editor::links::spans_from_pulldown;
use flavite_syntax::segment::SegmentIndex;
use std::fs;
use std::path::PathBuf;

const CORPUS_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../flavite-syntax/tests/corpus"
);
const KNOWN_DIVERGENCES: &str = "KNOWN-DIVERGENCES.md";

#[test]
fn widget_structure_path_matches_pulldown_on_the_corpus() {
    let mut fixtures: Vec<PathBuf> = fs::read_dir(CORPUS_DIR)
        .expect("corpus dir must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "md")
                && p.file_name().is_some_and(|n| n != KNOWN_DIVERGENCES)
        })
        .collect();
    fixtures.sort();

    assert!(!fixtures.is_empty(), "corpus must contain fixtures");
    let mut checked = 0usize;
    for fixture in &fixtures {
        // The widget consumes the *normalized* revision text (CRLF folded,
        // BOM stripped); normalize fixture bytes the same way so both paths
        // see identical input.
        let bytes = fs::read(fixture).expect("read fixture");
        let text = flavite_syntax::TextSnapshot::new(&bytes)
            .expect("fixture must be decodable")
            .text()
            .to_string();
        let parsed =
            flavite_syntax::pulldown_glue::parse_normalized(&text).expect("pulldown parse");
        let pulldown = spans_from_pulldown(parsed);

        // The widget's producer: the segment cache (first sync = full
        // build), converted through the same canonical span conversion.
        let mut segment = SegmentIndex::new();
        segment.sync(&text, None);
        let spans = segment.spans();
        let segment_path = flavite_editor::links::spans_from_canonical(
            spans.0,
            spans.1,
            spans.2,
        );
        assert_eq!(
            segment_path,
            pulldown,
            "segment structure spans diverge from pulldown on {}",
            fixture.display()
        );
        checked += 1;
    }
    eprintln!("[S6] {checked} corpus fixtures agree through the widget layer (segment producer)");
}
