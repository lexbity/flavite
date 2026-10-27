//! S1 caret geometry and hit-test precision probe — spike S1, slice A6 (§9 Phase 0;
//! D18; ADR-0001; AC-24).
//!
//! The S1 question this answers: can cosmic-text provide caret-rect and
//! hit-test precision at fractional scales that meet the ≤ 0.5 px round-trip
//! error requirement? This probe measures both under controlled conditions
//! with the pinned cosmic-text version (0.15.0, transitive via iced 0.14).
//!
//! The probe shapes text at scale factors 1.0 / 1.25 / 1.5 / 2.0; measures
//! caret-rect precision and hit-test round-trip error (char index → point →
//! char index) at each scale. The headless tests in `xtask/tests/s1_caret_probe.rs`
//! assert the error bound; the binary (`cargo run -p xtask --bin s1_caret_probe`)
//! prints a full table for manual review.
//!
//! Acceptance: round-trip error ≤ 0.5 px at every scale factor (AC-24).

use xtask::s1_caret_probe::{
    FONT_SIZE, MAX_ROUNDTRIP_ERROR, SAMPLE_TEXT, ScaleResults, run_measurements,
};

/// Print a summary table of the results.
fn print_results(results: &[ScaleResults]) {
    println!("Caret geometry and hit-test precision probe");
    println!("==========================================");
    println!();
    println!("Sample text: \"{}\"", SAMPLE_TEXT);
    println!("Font size: {}px", FONT_SIZE);
    println!();

    let mut all_pass = true;

    for result in results {
        all_pass = all_pass && result.passes;
        println!(
            "Scale {:.2}x: max_error={:.4}px, passes={}",
            result.scale, result.max_error, result.passes
        );
    }

    println!();
    if all_pass {
        println!(
            "✓ All scales pass (max_error ≤ {:.1}px)",
            MAX_ROUNDTRIP_ERROR
        );
    } else {
        println!(
            "✗ Some scales fail (max_error > {:.1}px)",
            MAX_ROUNDTRIP_ERROR
        );
    }

    println!();
    println!("Detailed per-scale results:");
    println!();

    for result in results {
        println!("Scale {:.2}x:", result.scale);
        println!(
            "  Metrics: font_size={:.2}, line_height={:.2}",
            result.font_size, result.line_height
        );
        println!("  Max round-trip error: {:.4}px", result.max_error);
        println!("  Result: {}", if result.passes { "PASS" } else { "FAIL" });
        println!();
    }
}

fn main() {
    let results = run_measurements();
    print_results(&results);
}
