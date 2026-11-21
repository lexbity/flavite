//! RFC-0008 S8-T5 — end-to-end keystroke chain at the shipped configuration
//! (AC-1 attribution).
//!
//! Ignored release bench; runs under `just bench-edit` (release,
//! `--ignored`). Measures the four phases of one keystroke as the widget
//! actually runs them today, at two document sizes:
//!
//! * **64 KiB** — engine path (the flag-enabled branch, measured directly)
//!   vs bridge path (the shipped default routing).
//! * **1 MiB** — bridge path (the shipped default routing).
//!
//! Phases: model apply (`try_apply_recorded_batch`), structure (the two
//! dispatch branches), shaping (band splice on a 240-line viewport), paint
//! collection (per-line painted set). The sum is the AC-1 end-to-end
//! keystroke cost at each size under each routing.
//!
//! **Routing decision (RFC-0008 Appendix C, 2026-11-19)**: the engine is
//! correct (AC-5 parity) but not budget-meeting — its incremental reparses
//! are not local (≈0.6–0.7× full parse) and trip the D10 guard near grammar
//! boundaries (p99 ≈166 ms at 64 KiB vs ≈1.4 ms for prose edits). The bridge
//! is therefore the shipped default for all documents; the engine stays
//! selectable behind `FLAVITE_TS_STRUCTURE=1`.
//!
//! Printed numbers are advisory. Absolute gates engage only with
//! `FLAVITE_REFERENCE_PERF` set (repo convention): the shipped 1 MiB chain
//! must beat the pre-RFC combined reference (full band reshape 30.42 ms +
//! `scan_note` 6.68 ms + model 3.64 ms). A relative catastrophic-regression
//! guard always engages: the 1 MiB bridge must stay under 5× the pre-RFC
//! `scan_note` reference (perf_structure_incremental.rs holds the engine's
//! own relative guard).

use flavite_buffer::{
    EditorModel,
    history::{EditKind, EditorEdit},
    undo::{EditOp, UndoPolicy},
};
use flavite_editor::viewport::Viewport;
use flavite_editor::widget::{
    EditorState, bridge_structure_spans_for_perf, engine_structure_spans_for_perf,
};
use flavite_syntax::structure::EditRange;
use flavite_syntax::text::{Span, TextByte};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const FILLER: &str =
    "# Heading\n\nSome ordinary text with [[target|a link]] and **bold** words.\n\n";
const TYPE_CHARS: usize = 100;

/// §3.4 pre-RFC `scan_note` reference (ms) at 1 MiB — the bridge's
/// catastrophic-regression ceiling.
const SCAN_NOTE_REFERENCE_1MIB_MS: f64 = 6.68;

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percent / 100]
}

fn model_of(size: usize) -> EditorModel {
    let text = FILLER.repeat(size / FILLER.len() + 1);
    let text = &text[..size.min(text.len())];
    EditorModel::from_text(text)
}

fn op(at: usize, value: &str) -> EditOp {
    EditOp::new(Span::new(TextByte(at), TextByte(at)), value.to_owned())
}

fn record(at: usize, value: &str) -> EditorEdit {
    EditorEdit {
        before_primary: 0,
        origin: format!("{:032x}", 1u128),
        kind: EditKind::Typing,
        before: vec![[0, at, at]],
        after: vec![[0, at + value.len(), at + value.len()]],
        primary: 0,
    }
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
    starts
}

/// A markdown-saturated document of at least `bytes` bytes — links, tags,
/// block ids, fences, tables, quotes (exercises the full scanner).
fn build_doc(bytes: usize) -> String {
    let block = "\
# Heading with [[link]] and #tag and ^block\n\n\
Some prose with *emphasis*, `code` and a [[wikilink|Alias]] plus #tag2.\n\n\
- item one [[a]]\n- item two [[b]]\n\n\
| A | B |\n|---|---|\n| [[cell-a]] | [[cell-b]] |\n\n\
> Quote [[q]] and #qt\n\n";
    let mut out = String::with_capacity(bytes + block.len());
    while out.len() < bytes {
        out.push_str(block);
    }
    out
}

/// Phase 1: model apply — one typing keystroke at EOF through the real
/// interactive recorded-batch path. Returns the per-keystroke p99.
fn model_phase_ms(size: usize) -> f64 {
    let mut model = model_of(size);
    let mut samples = Vec::with_capacity(TYPE_CHARS);
    for _ in 0..TYPE_CHARS {
        let at = model.text().len();
        let start = Instant::now();
        model
            .try_apply_recorded_batch(&[op(at, "x")], UndoPolicy::Typing, record(at, "x"))
            .expect("typing applies");
        samples.push(start.elapsed());
    }
    ms(percentile(&mut samples, 99))
}

/// Phase 2, engine path: the widget's engine branch at a mid-document edit
/// (seeded full sync, then incremental, measured directly — independent of
/// the env flag).
fn structure_engine_phase_ms(doc: &str, edits: usize) -> f64 {
    let mut engine = flavite_syntax::StructureEngine::new();
    let lines = line_starts(doc);
    // Seed: first sync is a full parse (no hint) — not part of the
    // per-keystroke measurement.
    let spans = engine_structure_spans_for_perf(doc, None, &lines, &mut engine);
    assert!(!spans.0.is_empty(), "seed must yield links");

    let mut text = doc.to_string();
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut rng = |range: usize| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as usize % range
    };
    let mut samples = Vec::with_capacity(edits);
    for _ in 0..edits {
        let len = text.len();
        let mut pos = (len / 3 + rng(len / 3)).min(len.saturating_sub(1));
        while !text.is_char_boundary(pos) {
            pos = pos.saturating_sub(1);
        }
        let ch = (b'a' + (rng(26) as u8)) as char;
        let hint = EditRange {
            start: pos,
            old_end: pos,
            new_end: pos + 1,
        };
        let l = line_starts(&text);
        let start = Instant::now();
        let spans = engine_structure_spans_for_perf(&text, Some(hint), &l, &mut engine);
        samples.push(start.elapsed());
        assert!(!spans.0.is_empty(), "clean doc must keep links");
        text.insert(pos, ch);
    }
    ms(percentile(&mut samples, 99))
}

/// Phase 2, bridge path: the widget's bridge branch (the shipped default
/// routing) at a mid-document edit.
fn structure_bridge_phase_ms(doc: &str, edits: usize) -> f64 {
    let mut samples = Vec::with_capacity(edits);
    for _ in 0..edits {
        let start = Instant::now();
        let spans = bridge_structure_spans_for_perf(doc);
        samples.push(start.elapsed());
        assert!(!spans.0.is_empty(), "clean doc must keep links");
    }
    ms(percentile(&mut samples, 99))
}

/// A 240-line band viewport (band = first − 80 → 0, first + 160 → 240),
/// matching the §3.1 table's band geometry.
fn band_fixture() -> (String, flavite_editor::EditorView, Viewport) {
    let text = (0..240u32)
        .map(|index| format!("line {index:03} alpha beta gamma delta epsilon zeta eta theta\n"))
        .collect::<String>();
    let viewport = Viewport {
        first_line: 80,
        visible_lines: 80,
        ..Viewport::default()
    };
    let view = flavite_editor::EditorView {
        buffer_id: 1,
        revision: 1,
        text: flavite_buffer::chunks::DocumentText::from(text.as_str()),
        path: PathBuf::from("note.md"),
        state: flavite_buffer::state::BufferState::Clean,
        last_edit: None,
    };
    (text, view, viewport)
}

/// Phase 3 + 4: shaping (band splice) and paint collection on a 240-line
/// band with one changed line — the render half. Independent of document
/// size; measured once.
fn render_phases_ms() -> (f64, f64) {
    let (text, view, viewport) = band_fixture();
    let width = 800.0_f32;
    let paint_height = 80.0 * 22.0;

    let mut edited = text.clone();
    let target = "line 120 alpha";
    let at = edited.find(target).expect("line 120") + target.len() - 5;
    edited.replace_range(at..at + 1, "A");
    let edited_view = flavite_editor::EditorView {
        revision: 2,
        text: flavite_buffer::chunks::DocumentText::from(edited.as_str()),
        last_edit: Some(EditRange {
            start: at,
            old_end: at + 1,
            new_end: at + 1,
        }),
        ..view.clone()
    };

    let mut shaping = Vec::with_capacity(200);
    let mut paint = Vec::with_capacity(200);
    for _ in 0..200 {
        let mut state = EditorState::default();
        state.reconcile(&view, &std::collections::HashMap::new());
        let view_text = view.text.materialize();
        let edited_text = edited_view.text.materialize();
        state.sync_shaped_for_perf(&view_text, 1, viewport, width);
        state.reconcile(&edited_view, &std::collections::HashMap::new());
        let start = Instant::now();
        state.sync_shaped_for_perf(&edited_text, 2, viewport, width);
        shaping.push(start.elapsed());
        let start = Instant::now();
        let painted =
            state.painted_line_count_for_perf(&edited_text, 2, viewport, width, paint_height);
        assert!(painted > 0, "paint collection paints lines");
        paint.push(start.elapsed());
    }
    (
        ms(percentile(&mut shaping, 99)),
        ms(percentile(&mut paint, 99)),
    )
}

#[test]
#[ignore = "perf budget; runs under just bench-edit (release, --ignored)"]
fn end_to_end_keystroke_chain_at_shipped_config() {
    let model_1mib = model_phase_ms(1024 * 1024);
    let engine_64kib = structure_engine_phase_ms(&build_doc(64 * 1024), 100);
    let bridge_64kib = structure_bridge_phase_ms(&build_doc(64 * 1024), 100);
    let bridge_1mib = structure_bridge_phase_ms(&build_doc(1024 * 1024), 30);
    let (shaping, paint) = render_phases_ms();

    println!("RFC-0008 S8-T5 end-to-end keystroke chain (shipped config, p99)");
    println!("phase,doc_bytes,ms_p99");
    println!("model,1048576,{model_1mib:.3}");
    println!("structure_engine,65536,{engine_64kib:.3}");
    println!("structure_bridge,65536,{bridge_64kib:.3}");
    println!("structure_bridge,1048576,{bridge_1mib:.3}");
    println!("shaping,band240,{shaping:.3}");
    println!("paint,band240,{paint:.3}");

    // The engine path is the designed route for typical notes; the bridge is
    // the fallback. If the engine is not faster than the bridge at its own
    // ceiling, the shipped default should route that size class to the
    // bridge (recorded in RFC-0008 Appendix C / baselines).
    println!("engine_vs_bridge_at_64kib: engine={engine_64kib:.3} ms bridge={bridge_64kib:.3} ms");

    let sum_64kib_engine = model_1mib + engine_64kib + shaping + paint;
    let sum_64kib_bridge = model_1mib + bridge_64kib + shaping + paint;
    let sum_1mib = model_1mib + bridge_1mib + shaping + paint;
    println!("sum_engine_routing,65536,{sum_64kib_engine:.3}");
    println!("sum_bridge_routing,65536,{sum_64kib_bridge:.3}");
    println!("sum_bridge_routing,1048576,{sum_1mib:.3}");

    // Relative catastrophic-regression guard (always engages): the 1 MiB
    // bridge must not blow past 5× the pre-RFC `scan_note` reference — the
    // R8 discipline (relative gates over absolute where possible).
    assert!(
        bridge_1mib <= SCAN_NOTE_REFERENCE_1MIB_MS * 5.0,
        "1 MiB bridge {bridge_1mib:.3} ms > 5× pre-RFC scan_note {SCAN_NOTE_REFERENCE_1MIB_MS} ms"
    );

    // Absolute AC-1 gate, only with the reference-perf env (runner noise
    // must not gate PRs): the shipped 1 MiB chain must beat the pre-RFC
    // combined reference (full reshape 30.42 ms + scan_note 6.68 ms + model
    // 3.64 ms = 40.74 ms).
    if std::env::var_os("FLAVITE_REFERENCE_PERF").is_some() {
        const PRE_RFC_COMBINED_1MIB_MS: f64 = 30.42 + 6.68 + 3.64;
        assert!(
            sum_1mib < PRE_RFC_COMBINED_1MIB_MS,
            "shipped 1 MiB chain {sum_1mib:.3} ms ≥ pre-RFC combined {PRE_RFC_COMBINED_1MIB_MS} ms"
        );
    }
}
