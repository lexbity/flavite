//! RFC-0008 S1/S2 — render-path benchmarks (evidence §3.1, S2-T4).
//!
//! Ignored release benches; run under `just bench-edit` (release,
//! `--ignored`) or without `--release` for the debug diagnostic. `S1`
//! measures the full band re-shape (the reference path; S2 replaces it with
//! the persistent splice). `S2` measures the keystroke splice on a 240-line
//! band — the AC-2/NFR-1 shaping budget.
//!
//! Printed numbers are advisory. Absolute hard-fail gates (5× the §3.1
//! baseline, or the S2 0.5 ms splice budget) engage in release; debug numbers
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

/// S2-T4: the keystroke splice on a 240-line band (AC-2): ≤0.5 ms p50 in
/// release (target 0.15 ms, same hardware); ≤3 ms debug is a diagnostic print
/// (NFR-3), never asserted.
#[test]
#[ignore = "perf budget; runs under just bench-edit (release, --ignored)"]
fn keystroke_splice_cost_stays_sub_half_millisecond() {
    use flavite_buffer::state::BufferState;
    use flavite_editor::widget::EditorState;
    use flavite_editor::{EditorView, viewport::Viewport};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    // A 240-line band: viewport centred on line 80 with 80 visible lines
    // (band_window: first − 80 → 0, first + 160 → 240). Lines match the
    // §3.1 table's ~46 B/line so the splice cost is comparable.
    let text = (0..240u32)
        .map(|index| format!("line {index:03} alpha beta gamma delta epsilon zeta eta theta\n"))
        .collect::<String>();
    let width = 800.0_f32;
    let viewport = Viewport {
        first_line: 80,
        visible_lines: 80,
        ..Viewport::default()
    };
    let base_view = EditorView {
        buffer_id: 1,
        revision: 1,
        text: Arc::from(text.as_str()),
        path: PathBuf::from("note.md"),
        state: BufferState::Clean,
    };
    // One mid-line character edit at line 120: the keystroke splice case.
    let mut edited = text.clone();
    let target = "line 120 alpha";
    let at = edited.find(target).expect("line 120") + target.len() - 5;
    edited.replace_range(at..at + 1, "A");
    let edited_view = EditorView {
        revision: 2,
        text: Arc::from(edited.as_str()),
        ..base_view.clone()
    };

    let mut samples = Vec::with_capacity(100);
    for _ in 0..100 {
        let mut state = EditorState::default();
        // The real frame order: revision change → reconcile (line index +
        // scan) → layout sync. Only the sync (shaping) phase is measured.
        state.reconcile(&base_view, &HashMap::new());
        state.sync_shaped_for_perf(&base_view.text, 1, viewport, width);
        state.reconcile(&edited_view, &HashMap::new());
        let start = Instant::now();
        state.sync_shaped_for_perf(&edited_view.text, 2, viewport, width);
        samples.push(start.elapsed());
    }
    let p50 = percentile(&mut samples, 50);
    let max = *samples.last().expect("100 samples");
    println!(
        "keystroke_splice_240line_ms_p50={:.4} max={:.4}",
        ms(p50),
        ms(max)
    );
    if cfg!(debug_assertions) {
        println!("debug build: splice timing is diagnostic (NFR-3), not asserted");
    } else {
        assert!(
            p50 < Duration::from_millis(1),
            "keystroke splice p50 {:.4} ms ≥ 1 ms (budget ≤0.5 ms)",
            ms(p50)
        );
    }
}
