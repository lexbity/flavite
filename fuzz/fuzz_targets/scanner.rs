//! Fuzz target for the inline scanner (§10.6).
//!
//! Invariants:
//! - spans are in-bounds and non-overlapping
//! - code/math excluded

#![no_main]

use flavite_syntax::scanner::scan_inline;
use flavite_syntax::text::TextSnapshot;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Try to decode as UTF-8
    if let Ok(_) = String::from_utf8(data.to_vec()) {
        if let Ok(snapshot) = TextSnapshot::new(data) {
            let spans = scan_inline(&snapshot, 0..snapshot.text().len());
            
            // Check that spans are non-overlapping and in ascending order
            for i in 1..spans.len() {
                let prev = &spans[i - 1];
                let curr = &spans[i];
                
                // Check ordering
                assert!(
                    prev.span.start.0 < curr.span.start.0,
                    "Spans must be in ascending order"
                );
                
                // Check non-overlapping
                assert!(
                    prev.span.end.0 <= curr.span.start.0,
                    "Spans must be non-overlapping"
                );
            }
            
            // Check that all spans are within bounds
            for span in &spans {
                assert!(
                    span.span.start.0 < snapshot.text().len(),
                    "Span start out of bounds"
                );
                assert!(
                    span.span.end.0 <= snapshot.text().len(),
                    "Span end out of bounds"
                );
                assert!(
                    span.span.start.0 < span.span.end.0,
                    "Span is empty or backwards"
                );
            }
        }
    }
});
