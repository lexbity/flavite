//! Fuzz target for parity between editor and indexer parse paths (AC-5, §10.6).
//!
//! Differential harness: pulldown's exclusion mask vs. editor inline ranges.
//! Both paths invoke the shared scanner, and must produce identical link spans.

#![no_main]

use flavite_syntax::{pulldown_glue::parse_note, scanner::scan_inline, SpanKind};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(indexed) = parse_note(data) else { return };
    let text = indexed.snapshot.text();
    let mut editor_links = Vec::new();
    let mut cursor = 0;
    for excluded in indexed.mask.ranges() {
        editor_links.extend(scan_inline(&indexed.snapshot, cursor..excluded.start)
            .into_iter().filter(|span| matches!(span.kind, SpanKind::Link | SpanKind::Embed)));
        cursor = excluded.end;
    }
    editor_links.extend(scan_inline(&indexed.snapshot, cursor..text.len())
        .into_iter().filter(|span| matches!(span.kind, SpanKind::Link | SpanKind::Embed)));
    assert_eq!(
        indexed.links.iter().map(|link| (link.span.start.0, link.span.end.0, link.target_raw.as_str(), link.is_embed)).collect::<Vec<_>>(),
        editor_links.iter().map(|span| (span.span.start.0, span.span.end.0, span.payload.as_ref().unwrap().target_raw.as_str(), span.kind == SpanKind::Embed)).collect::<Vec<_>>(),
        "editor and indexer link spans diverged"
    );
});
