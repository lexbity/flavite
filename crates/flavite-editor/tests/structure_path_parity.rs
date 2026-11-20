//! S7-T3: structure-path parity through the editor layer (RFC-0008 AC-5 at
//! the widget's flag-on seam).
//!
//! The widget's flag-on path runs `flavite_syntax::StructureEngine::sync`
//! (full parse in stage B) and converts the result with
//! `flavite_editor::links::spans_from_ts`. This test drives the exact same
//! functions the widget uses and proves their output is byte-identical to
//! the shipped pulldown path (`links::scan_note`) over the S6 acceptance
//! corpus. The corpus live in flavite-syntax; the waivered fixture
//! (`inline_link_anchor.md`) and the documentation file are excluded exactly
//! as in the S6 parity harness.

use flavite_editor::links::{spans_from_pulldown, ts_structure_spans};
use std::fs;
use std::path::PathBuf;

const CORPUS_DIR: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../flavite-syntax/tests/corpus"
);
const KNOWN_DIVERGENCES: &str = "KNOWN-DIVERGENCES.md";
const WAIVED: &[&str] = &["inline_link_anchor.md"];

#[test]
fn widget_structure_path_matches_pulldown_on_the_corpus() {
    let mut fixtures: Vec<PathBuf> = fs::read_dir(CORPUS_DIR)
        .expect("corpus dir must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "md")
                && p.file_name().is_some_and(|n| {
                    n != KNOWN_DIVERGENCES && !WAIVED.contains(&n.to_str().unwrap())
                })
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
        let ts = ts_structure_spans(&text);
        assert_eq!(
            ts,
            pulldown,
            "tree-sitter structure spans diverge from pulldown on {}",
            fixture.display()
        );
        checked += 1;
    }
    eprintln!("[S7-T3] {checked} corpus fixtures agree through the widget layer");
}
