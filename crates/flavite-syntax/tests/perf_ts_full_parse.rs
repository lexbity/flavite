//! S6-T3: full-parse cost diagnostic (`#[ignore]` perf).
//!
//! RFC-0008 S6-T3 asks whether a full `parse_note_ts` parse @1MB stays within
//! 2× the pulldown `scan_note` cost. `scan_note` (flavite-editor) wraps the
//! pulldown glue; the syntax crate cannot depend on the editor crate, so this
//! compares against `pulldown_glue::parse_note` — the identical underlying
//! parse + scanner machinery `scan_note` drives.
//!
//! ## Measured reality (release, this machine, 2026-11-19)
//!
//! tree-sitter-md's block and inline grammars parse at ≈250–600 ms/MB even on
//! plain prose (measured). `parse_note_ts` performs two such parses, so a
//! full-document parse @1MB is ≈0.5–1.0 s — ~30–100× slower than the
//! pulldown path (~6.7 ms @1MB, RFC §3.4). The RFC's 2× sanity bound is
//! therefore **not achievable** for a full-document parse with these
//! grammars; this is recorded here as the evidence that the per-keystroke
//! budget must come from **incremental** structure parsing (S8, `InputEdit`)
//! — and that the D8 worker contingency is the named fallback if incremental
//! reparses ever regress.
//!
//! The assertion is therefore self-calibrating: `parse_note_ts` must stay
//! within a small constant factor of the **sum of its two raw grammar
//! parses** (re-measured in-process). That catches algorithmic regressions —
//! e.g. the quadratic exclusion-list scan that made `parse_note_ts` @1MB
//! ~3× slower than the raw parses during S6 development — without asserting
//! machine-dependent absolute ratios.

use flavite_syntax::pulldown_glue::parse_note;
use flavite_syntax::ts_ranges::parse_note_ts;
use std::time::{Duration, Instant};
use tree_sitter::Parser;

/// Build a ~1 MiB markdown corpus exercising headings, prose, emphasis, code,
/// tags, block ids, lists, tables, fences and quotes.
fn build_1mb_corpus() -> String {
    let block = "\
# Heading with [[link]] and #tag and ^block\n\n\
Some prose with *emphasis*, **strong**, `inline code` and a [[wikilink|Alias]].\n\n\
- item one with [[item-one]]\n- item two with #item-two\n\n\
| A | B |\n|---|---|\n| [[cell-a]] | [[cell-b]] |\n\n\
```rust\nfn main() { let s = \"[[fenced]]\"; }\n```\n\n\
> Quote with [[quote]] and #quote-tag\n\n";
    let target = 1024 * 1024;
    let mut out = String::with_capacity(target + block.len());
    while out.len() < target {
        out.push_str(block);
    }
    out
}

fn best_of(n: usize, mut f: impl FnMut() -> Duration) -> Duration {
    (0..n).map(|_| f()).min().unwrap_or_default()
}

fn timed<F: FnOnce() -> R, R>(f: F) -> (R, Duration) {
    let t = Instant::now();
    let r = f();
    (r, t.elapsed())
}

/// Time the raw block-grammar and inline-grammar parses of `doc`.
fn raw_grammar_parses(doc: &str) -> Duration {
    let mut block = Parser::new();
    block
        .set_language(&tree_sitter_md::LANGUAGE.into())
        .expect("block language");
    let mut inline = Parser::new();
    inline
        .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
        .expect("inline language");
    let block_dt = best_of(3, || timed(|| block.parse(doc, None).expect("parse")).1);
    let inline_dt = best_of(3, || timed(|| inline.parse(doc, None).expect("parse")).1);
    block_dt + inline_dt
}

#[test]
#[ignore = "perf: full ts parse vs pulldown full parse @1MB (S6-T3)"]
fn full_parse_vs_pulldown_at_1mb() {
    let doc = build_1mb_corpus();
    assert!(doc.len() >= 1024 * 1024, "corpus must be ≥1 MiB");

    // Warm up both paths once.
    let ts = parse_note_ts(doc.as_bytes()).expect("ts parse");
    let pl = parse_note(doc.as_bytes()).expect("pulldown parse");
    assert!(!ts.links.is_empty() && !pl.links.is_empty());

    let ts_dt = best_of(5, || {
        timed(|| parse_note_ts(doc.as_bytes()).expect("ts parse")).1
    });
    let pl_dt = best_of(5, || {
        timed(|| parse_note(doc.as_bytes()).expect("pulldown parse")).1
    });
    let raw_dt = raw_grammar_parses(&doc);

    println!(
        "S6-T3 @{} bytes: parse_note_ts = {ts_dt:?}; raw block+inline parses = {raw_dt:?}; \
         pulldown parse_note = {pl_dt:?}; ts/pulldown = {:.1}x",
        doc.len(),
        ts_dt.as_secs_f64() / pl_dt.as_secs_f64()
    );

    // Self-calibrating algorithmic-regression ceiling: the full path (two
    // parses + two walks + mask + scanner) must stay within 3× of the raw
    // grammar parses it performs. The quadratic exclusion-scan regression
    // during S6 made this ratio ≈3.6×; a healthy build is ≈1.1–1.3×.
    assert!(
        ts_dt <= raw_dt * 3,
        "parse_note_ts {ts_dt:?} exceeds 3× raw parses {raw_dt:?} — algorithmic regression?"
    );
}
