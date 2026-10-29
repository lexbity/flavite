//! Fuzz target for parity between editor and indexer parse paths (AC-5, §10.6).
//!
//! Differential harness: pulldown's exclusion mask vs. editor inline ranges.
//! Both paths invoke the shared scanner, and must produce identical link spans.

#![no_main]

use flavite_syntax::{pulldown_glue::parse_note, ts_ranges::parse_note_ts};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Generate valid Markdown structure from arbitrary bytes. Parser and
    // scanner targets cover malformed input separately; this target compares
    // semantic spans over grammar-shaped documents as required by AC-5.
    let mut document = String::new();
    for (index, byte) in data.iter().copied().take(64).enumerate() {
        document.push_str(&format!(
            "## Heading {index}\n\nText [[note-{byte}|Alias]] ![[asset-{byte}.png]] #tag/{byte} ^block-{byte}\n\n"
        ));
        if byte & 1 != 0 {
            document.push_str("`[[masked]] #masked ^masked` and $[[math]] #math$\n\n");
        }
        if byte & 2 != 0 {
            document.push_str("```md\n[[code]] #code ^code\n```\n\n");
        }
    }
    if let (Ok(indexer), Ok(editor)) = (
        parse_note(document.as_bytes()),
        parse_note_ts(document.as_bytes()),
    ) {
        let indexer_spans: Vec<_> = indexer
            .links
            .iter()
            .map(|link| (link.span, link.is_embed, &link.target_raw))
            .collect();
        let editor_spans: Vec<_> = editor
            .links
            .iter()
            .map(|link| (link.span, link.is_embed, &link.target_raw))
            .collect();
        if editor_spans != indexer_spans {
            panic!("parity divergence: editor={editor_spans:?}, indexer={indexer_spans:?}");
        }
    }
});
