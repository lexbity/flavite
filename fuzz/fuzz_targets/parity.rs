//! Fuzz target for parity between editor and indexer parse paths (AC-5, §10.6).
//!
//! Differential harness: grammar-generated documents through both parsers.
//! Assert: `editor_link_spans == indexer_link_spans`
//!
//! For now, this just exercises the indexer parse path. The editor path
//! (tree-sitter) will be integrated later when PLAN-B lands.

#![no_main]

use flavite_syntax::pulldown_glue::parse_note;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Try to decode as UTF-8
    let _ = data;
    
    // Try the indexer parse path
    let indexer_result = parse_note(data);
    
    // Just ensure it doesn't panic
    let _ = indexer_result;
});
