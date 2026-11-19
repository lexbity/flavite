//! RFC-0008 S1 — render-path baseline (evidence §3.1).
//!
//! Ignored release benches; run under `just bench-edit` (release,
//! `--ignored`) or without `--release` for the debug diagnostic. Measures the
//! full band re-shape (the current `ShapingKey`-keyed path; S2 replaces it
//! with the persistent splice). The persistent-buffer splice column lands in
//! S2 on the same harness shape.
//!
//! Printed numbers are advisory. Absolute hard-fail gates (5× the §3.1
//! baseline) engage only when `FLAVITE_REFERENCE_PERF` is set. Debug numbers
//! are diagnostics (NFR-3), never gates.

use flavite_editor::widget::shape_for_test;
use std::{
    hint::black_box,
    time::{Duration, Instant},
};

/// One line repeated to build the band text (≈52 bytes/line; the §3.1 table
/// uses 45–46 B/line — close enough for a shaping-time baseline).
const LINE: &str = "The quick brown fox jumps over the lazy dog 0123456789\n";

const BANDS: &[(usize, f64)] = &[(60, 7.96), (120, 15.52), (240, 30.42)];

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percent / 100]
}

#[test]
#[ignore = "perf budget; runs under just bench-edit (release, --ignored)"]
fn full_reshape_scales_with_band_size_as_documented() {
    let debug_build = cfg!(debug_assertions);
    println!("band_lines,text_kb,reshape_ms_p50,reshape_ms_max");
    for &(lines, baseline_ms) in BANDS {
        let text = LINE.repeat(lines);
        let mut samples = Vec::with_capacity(5);
        for _ in 0..5 {
            let start = Instant::now();
            black_box(shape_for_test(&text, 800.0));
            samples.push(start.elapsed());
        }
        let p50 = percentile(&mut samples, 50);
        let max = *samples.last().expect("five samples");
        println!(
            "{lines},{:.1},{:.3},{:.3}",
            text.len() as f64 / 1024.0,
            ms(p50),
            ms(max)
        );
        if debug_build {
            println!("debug build: shaping at {lines} lines is diagnostic (NFR-3), not a gate");
        } else if std::env::var_os("FLAVITE_REFERENCE_PERF").is_some() {
            assert!(
                ms(p50) <= baseline_ms * 5.0,
                "full reshape p50 {:.3} ms > 5× baseline {baseline_ms} ms at {lines} lines",
                ms(p50)
            );
        }
    }
}
