//! RFC-0008 S1 — model-path benchmarks (evidence §3.2).
//!
//! Ignored release benches; run under `just bench-edit` or `just bench-scale`
//! (release, `--ignored`). The interactive recorded-batch keystroke path is
//! measured at 2 KB / 100 KB / 1 MB / 5 MB with one caret at EOF, plus undo of
//! a coalesced burst, an isolated boundary install, and a 10 000-node history
//! ceiling.
//!
//! Printed numbers are advisory (compare against the RFC-0008 §3.2 table;
//! ±50% is expected on any machine). Absolute hard-fail gates (5× the §3.2
//! baseline) engage only when `FLAVITE_REFERENCE_PERF` is set — the repo
//! convention (selection_performance.rs) so runner noise cannot gate PRs.
//! Relative gates (ceiling vs fresh) always engage.

use flavite_buffer::{
    EditorModel,
    history::{EditKind, EditorEdit},
    undo::{EditOp, UndoPolicy},
};
use flavite_syntax::text::{Span, TextByte};
use std::time::{Duration, Instant};

const FILLER: &str =
    "# Heading\n\nSome ordinary text with [[target|a link]] and **bold** words.\n\n";
const TYPE_CHARS: usize = 100;
const ROUNDS: usize = 3;
const CEILING_NODES: u128 = 10_000;

/// §3.2 keystroke baselines (ms) per document size.
fn keystroke_baseline_ms(size: usize) -> f64 {
    match size {
        s if s <= 2 * 1024 => 0.010,
        s if s <= 100 * 1024 => 0.34,
        s if s <= 1024 * 1024 => 3.64,
        _ => 15.34,
    }
}

fn model_of(size: usize) -> EditorModel {
    let text = FILLER.repeat(size / FILLER.len() + 1);
    let text = &text[..size.min(text.len())];
    EditorModel::from_text(text)
}

fn op(at: usize, value: &str) -> EditOp {
    EditOp::new(Span::new(TextByte(at), TextByte(at)), value.to_owned())
}

fn record(at: usize, value: &str, origin: u128) -> EditorEdit {
    EditorEdit {
        before_primary: 0,
        origin: format!("{origin:032x}"),
        kind: EditKind::Typing,
        before: vec![[0, at, at]],
        after: vec![[0, at + value.len(), at + value.len()]],
        primary: 0,
    }
}

/// One keystroke at EOF through the real interactive path.
fn type_char(model: &mut EditorModel) {
    let at = model.text().len();
    model
        .try_apply_recorded_batch(&[op(at, "x")], UndoPolicy::Typing, record(at, "x", 1))
        .expect("typing applies");
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percent / 100]
}

#[test]
#[ignore = "perf budget; runs under just bench-edit (release, --ignored)"]
fn model_keystroke_cost_tracks_the_reference_table() {
    println!("doc_bytes,keystroke_ms_p50,keystroke_ms_p90,undo_burst_ms,boundary_ms");
    for size in [2 * 1024, 100 * 1024, 1024 * 1024, 5 * 1024 * 1024] {
        let mut model = model_of(size);
        for _ in 0..10 {
            type_char(&mut model);
        } // warm-up: lazy statics + callsite interest

        let mut keystrokes = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let start = Instant::now();
            for _ in 0..TYPE_CHARS {
                type_char(&mut model);
            }
            keystrokes.push(start.elapsed() / TYPE_CHARS as u32);
        }
        let p50 = percentile(&mut keystrokes, 50);
        let p90 = percentile(&mut keystrokes, 90);

        let start = Instant::now();
        assert!(model.undo().is_some(), "undo of the coalesced burst");
        let undo_burst = start.elapsed();

        let at = model.text().len();
        let start = Instant::now();
        model
            .try_apply_batch(&[op(at, "!")], UndoPolicy::Isolated)
            .expect("isolated boundary install");
        let boundary = start.elapsed();

        println!(
            "{size},{:.4},{:.4},{:.4},{:.4}",
            ms(p50),
            ms(p90),
            ms(undo_burst),
            ms(boundary)
        );

        if std::env::var_os("FLAVITE_REFERENCE_PERF").is_some() {
            let baseline = keystroke_baseline_ms(size);
            assert!(
                ms(p50) <= baseline * 5.0,
                "keystroke p50 {:.4} ms > 5× baseline {baseline} ms at {size} bytes",
                ms(p50)
            );
            // §3.2: undo of a coalesced burst 0.55–3.2 ms @1–5 MB.
            let undo_baseline = if size >= 1024 * 1024 { 3.2 } else { 0.55 };
            assert!(
                ms(undo_burst) <= undo_baseline * 5.0,
                "undo burst {:.4} ms > 5× baseline {undo_baseline} ms at {size} bytes",
                ms(undo_burst)
            );
            // §3.2: boundary install 3.3–6.4 ms @1 MB; smaller docs scale down.
            let boundary_baseline = match size {
                s if s <= 2 * 1024 => 0.05,
                s if s <= 100 * 1024 => 0.7,
                _ => 6.4,
            };
            assert!(
                ms(boundary) <= boundary_baseline * 5.0,
                "boundary install {:.4} ms > 5× baseline {boundary_baseline} ms at {size} bytes",
                ms(boundary)
            );
        }
    }
}

#[test]
#[ignore = "perf budget; runs under just bench-edit (release, --ignored)"]
fn history_ceiling_keystroke_stays_sub_perceptual() {
    // 10 000 distinct-origin edits → 10 000 undo nodes on a 2 KB note
    // (matches §3.3's ceiling scenario; the default 128 MiB budget keeps all
    // nodes live at ~20 MiB accounted).
    let mut model = model_of(2 * 1024);
    for i in 0..CEILING_NODES {
        let at = model.text().len();
        let rec = record(at, "x", i);
        model
            .try_apply_recorded_batch(&[op(at, "x")], UndoPolicy::Context { origin: i }, rec)
            .expect("ceiling node installs");
    }
    // The 128 MiB budget holds ~10k × 2 KB nodes; near the ceiling the
    // conservative accounting may evict a handful (E-12 errs toward
    // eviction), so the gate is "at ceiling", not exact.
    assert!(
        model.history_nodes() >= CEILING_NODES as u64 - 100,
        "expected ~10k nodes at the ceiling, got {}",
        model.history_nodes()
    );

    let mut fresh = model_of(2 * 1024);
    let mut fresh_samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for _ in 0..TYPE_CHARS {
            type_char(&mut fresh);
        }
        fresh_samples.push(start.elapsed() / TYPE_CHARS as u32);
    }
    let mut ceiling_samples = Vec::with_capacity(ROUNDS);
    for _ in 0..ROUNDS {
        let start = Instant::now();
        for _ in 0..TYPE_CHARS {
            type_char(&mut model);
        }
        ceiling_samples.push(start.elapsed() / TYPE_CHARS as u32);
    }
    let fresh_p50 = percentile(&mut fresh_samples, 50);
    let ceiling_p50 = percentile(&mut ceiling_samples, 50);
    let ceiling_p90 = percentile(&mut ceiling_samples, 90);

    println!(
        "ceiling_2kb: fresh_keystroke_ms_p50={:.4} ceiling_keystroke_ms_p50={:.4} \
         ceiling_keystroke_ms_p90={:.4} ratio={:.1}",
        ms(fresh_p50),
        ms(ceiling_p50),
        ms(ceiling_p90),
        ms(ceiling_p50) / ms(fresh_p50).max(1e-9)
    );
    // §3.3: a keystroke at the ceiling costs 0.47 ms — sub-perceptual. The
    // hard-fail is a generous wall (5×): this is an ignored release bench, so
    // runner noise cannot gate PRs.
    assert!(
        ceiling_p50 < Duration::from_millis(5),
        "ceiling keystroke p50 {:.4} ms is not sub-perceptual",
        ms(ceiling_p50)
    );
    let _ = ceiling_p90;
}
