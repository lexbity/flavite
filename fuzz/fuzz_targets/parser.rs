#![no_main]

//! Fuzz target: `parser` — pulldown-cmark event stream over arbitrary bytes (§10.6).
//!
//! Invariants: no panic, no hang, event stream completes. Budget enforcement:
//! inputs > `MAX_NOTE_BYTES` (32 MB, §4.10) are rejected as non-findings.

use libfuzzer_sys::fuzz_target;
use pulldown_cmark::{Options, Parser};

/// Maximum note size (§4.10): inputs larger than this are rejected as
/// non-findings — exceeding a budget is a finding about the *producer*, not
/// this target.
const MAX_NOTE_BYTES: usize = 32 * 1024 * 1024;

fuzz_target!(|data: &[u8]| {
    // Budget enforcement (§10.6): reject oversized inputs as non-findings.
    if data.len() > MAX_NOTE_BYTES {
        return;
    }

    // pulldown-cmark operates on &str; use lossy conversion for arbitrary bytes.
    // Invalid UTF-8 is replaced with U+FFFD — this is acceptable because we are
    // fuzzing the parser's robustness, not testing exact text handling.
    let text = String::from_utf8_lossy(data);

    // Enable all extensions to maximize parser surface coverage.
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_SMART_PUNCTUATION);
    options.insert(Options::ENABLE_HEADING_ATTRIBUTES);
    options.insert(Options::ENABLE_YAML_STYLE_METADATA_BLOCKS);
    options.insert(Options::ENABLE_SUPERSCRIPT);
    options.insert(Options::ENABLE_SUBSCRIPT);
    options.insert(Options::ENABLE_DEFINITION_LIST);
    options.insert(Options::ENABLE_MATH);
    #[allow(deprecated)]
    options.insert(Options::ENABLE_OLD_FOOTNOTES);

    // Drain the event stream. Invariants: no panic, no hang (libfuzzer
    // timeout catches hangs), the stream completes.
    let parser = Parser::new_ext(&text, options);
    for _event in parser {
        // Drain — we are testing for panics and hangs, not event content.
    }
});
