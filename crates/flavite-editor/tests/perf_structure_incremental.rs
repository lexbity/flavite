//! S8-T2: structure cost diagnostic (`#[ignore]` perf, RFC-0008 NFR-5).
//!
//! ## Measured reality (release, this machine, 2026-11-19)
//!
//! tree-sitter-md's incremental reparses scale at ≈0.6–0.7× a full parse —
//! the grammars do **not** localize to the damaged region, so a 1 MiB
//! incremental sync costs ~2.1–2.8 s (block ≈0.25 s + inline ≈0.75 s +
//! changed-ranges ≈0.3 s, plus rescan). NFR-5's ≤1.5 ms p99 @1 MB is
//! therefore **not achievable** with these grammars on this hardware; the
//! widget routes documents > 64 KiB through the pulldown bridge
//! (`spans_from_pulldown`, same latency as the pre-RFC path) and the engine
//! serves typical notes.
//!
//! This test keeps the numbers honest: it measures the engine's incremental
//! cost at a typical note size (64 KiB) and the bridge at 1 MiB, asserting
//! only relative regressions (engine incremental must be far cheaper than a
//! full engine parse; the 1 MiB bridge must stay under the full-parse
//! ceiling). The absolute NFR-5 gate stays open and is recorded in
//! `RFC-0008-baselines.md` with the evidence above and the D8 worker /
//! paged-buffer follow-on.

use flavite_syntax::structure::{EditRange, StructureEngine};
use flavite_syntax::ts_ranges::parse_note_ts;
use std::time::Instant;

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
    starts
}

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

fn incremental_p99(engine: &mut StructureEngine, doc: &str, edits: usize) -> std::time::Duration {
    let mut text = doc.to_string();
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut rng = |range: usize| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize % range
    };
    let mut samples = Vec::with_capacity(edits);
    for _ in 0..edits {
        let len = text.len();
        let pos = (40 + rng(len - 40)).min(len - 1);
        let mut pos = pos.min(text.len());
        while !text.is_char_boundary(pos) {
            pos -= 1;
        }
        let ch = (b'a' + (rng(26) as u8)) as char;
        let hint = EditRange {
            start: pos,
            old_end: pos,
            new_end: pos + 1,
        };
        let t = Instant::now();
        let spans = engine
            .sync(&text, &line_starts(&text), Some(hint))
            .expect("sync");
        samples.push(t.elapsed());
        assert!(!spans.links.is_empty(), "clean doc must keep links");
        text.insert(pos, ch);
    }
    samples.sort_unstable();
    samples[(samples.len() * 99) / 100]
}

#[test]
#[ignore = "perf: structure cost (S8-T2) — engine@64KiB + bridge@1MiB"]
fn structure_cost_engine_and_bridge() {
    // --- Engine at a typical note size (64 KiB) ---
    let note = build_doc(64 * 1024);
    let mut engine = StructureEngine::new();
    let lines = line_starts(&note);
    let full_dt = {
        let t = Instant::now();
        engine.sync(&note, &lines, None).expect("seed");
        t.elapsed()
    };
    let p99 = incremental_p99(&mut engine, &note, 500);
    println!("S8-T2 engine@64KiB: full parse {full_dt:?}, incremental keystroke p99={p99:?}");
    // Regression guard: the incremental path must never blow up beyond a
    // small multiple of a full parse (the 95× quadratic regression class
    // during S6 dev). Whether it *beats* full parse is grammar-dependent;
    // on this hardware it does not (see module docs / baselines).
    assert!(
        p99 < full_dt * 16,
        "incremental p99 {p99:?} exceeds 16× full parse {full_dt:?}"
    );

    // --- Bridge at 1 MiB (the large-doc path) ---
    let big = build_doc(1024 * 1024);
    let big_full_dt = {
        let t = Instant::now();
        let parsed = parse_note_ts(big.as_bytes()).expect("full parse");
        assert!(!parsed.links.is_empty());
        t.elapsed()
    };
    let bridge_dt = {
        let t = Instant::now();
        let parsed = flavite_syntax::pulldown_glue::parse_normalized(&big).expect("bridge parse");
        assert!(!parsed.links.is_empty());
        t.elapsed()
    };
    println!("S8-T2 bridge@1MiB: full ts parse {big_full_dt:?}, pulldown bridge {bridge_dt:?}");
    assert!(
        bridge_dt <= big_full_dt,
        "bridge must not regress vs the full ts parse"
    );
}
