//! S5-T4 (NFR-3): segment-cache cost evidence (RFC-0011 S5). Ignored
//! release bench (R10: only relative guards in CI; advisory prints).
//!
//! - Full segmentation build ≤ 3× a whole-document bridge parse (once per
//!   root).
//! - Segment sync p99 ≤ 1.5 ms over 500 edits on a 1 MiB corpus doc.
//! - Adversarial fixture (unclosed fence at line 0) stays ≤ the bridge
//!   baseline (R5: never worse than the old whole-document path).

use flavite_syntax::EditRange;
use flavite_syntax::pulldown_glue::parse_normalized;
use flavite_syntax::segment::SegmentIndex;
use flavite_syntax::text_window::StructureText;
use std::hint::black_box;
use std::time::Instant;

const MAX_DOC_BYTES: usize = 1024 * 1024;

fn ms(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn percentile(samples: &mut [std::time::Duration], percent: usize) -> std::time::Duration {
    samples.sort_unstable();
    samples[(samples.len() - 1) * percent / 100]
}

/// A markdown-saturated 1 MiB document: headings, links, tags, fences,
/// quotes, tables.
fn corpus_doc(bytes: usize) -> String {
    let block = "\
# Heading with [[link]] #tag ^block\n\n\
Some prose with *emphasis*, `code` and a [[wikilink|Alias]] plus #tag2.\n\n\
- item one [[a]]\n- item two [[b]]\n\n\
| A | B |\n|---|---|\n| [[cell-a]] | [[cell-b]] |\n\n\
```rust\nfn main() { /* [[hidden]] */ }\n```\n\n\
> Quote [[q]] and #qt\n\n";
    let mut out = String::with_capacity(bytes + block.len());
    while out.len() < bytes {
        out.push_str(block);
    }
    out
}

struct TestText {
    text: String,
    line_starts: Vec<usize>,
}

impl TestText {
    fn new(text: String) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { text, line_starts }
    }
}

impl StructureText for TestText {
    fn len(&self) -> usize {
        self.text.len()
    }
    fn line_count(&self) -> usize {
        self.line_starts.len()
    }
    fn line_start(&self, line: usize) -> usize {
        self.line_starts
            .get(line)
            .copied()
            .unwrap_or(self.text.len())
    }
    fn slice(&self, range: std::ops::Range<usize>) -> String {
        self.text[range].to_owned()
    }
}

#[test]
#[ignore = "perf budget; runs under just bench-edit (release, --ignored)"]
fn s5_t4_segment_costs_within_nfr3() {
    let doc = corpus_doc(MAX_DOC_BYTES);

    // Full build vs bridge parse.
    let mut build_samples = Vec::with_capacity(20);
    let mut bridge_samples = Vec::with_capacity(20);
    for _ in 0..20 {
        let mut index = SegmentIndex::new();
        let start = Instant::now();
        index.sync(&TestText::new(doc.clone()), None);
        build_samples.push(start.elapsed());
        let start = Instant::now();
        black_box(parse_normalized(&doc).expect("bridge"));
        bridge_samples.push(start.elapsed());
    }
    let build_p50 = percentile(&mut build_samples, 50);
    let bridge_p50 = percentile(&mut bridge_samples, 50);
    println!(
        "S5-T4 build@1MiB p50={:.4} ms, bridge p50={:.4} ms",
        ms(build_p50),
        ms(bridge_p50)
    );
    assert!(
        ms(build_p50) <= ms(bridge_p50) * 3.0,
        "full build {:.4} ms > 3× bridge {:.4} ms",
        ms(build_p50),
        ms(bridge_p50)
    );

    // Incremental sync p99 over 500 edits (mid-line single-char insertions
    // plus occasional fence inserts).
    let mut index = SegmentIndex::new();
    index.sync(&TestText::new(doc.clone()), None);
    let mut index = SegmentIndex::new();
    index.sync(&TestText::new(doc.clone()), None);
    let mut text = doc.clone();
    let mut samples = Vec::with_capacity(500);
    let mut fence_touching = 0usize;
    let mut state = 0x9E3779B97F4A7C15u64;
    let mut rng = |range: usize| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize % range
    };
    for _ in 0..500 {
        // Typical typing: insert inside a prose segment (never on a fence
        // line — real typing doesn't land inside ``` markers).
        let prose_segments: Vec<(usize, usize)> = index
            .debug_segments()
            .into_iter()
            .filter(|(kind, _, _)| *kind == flavite_syntax::segment::SegmentKind::Prose)
            .map(|(_, a, b)| (a, b))
            .collect();
        let (line_start, line_end) = if prose_segments.is_empty() {
            (0usize, text.bytes().filter(|&b| b == b'\n').count() + 1)
        } else {
            prose_segments[rng(prose_segments.len())]
        };
        let current = TestText::new(text.clone());
        let seg_byte_start = current.line_start(line_start);
        let seg_byte_end = current.line_start(line_end);
        let span = seg_byte_end.saturating_sub(seg_byte_start).max(1);
        let pos = seg_byte_start + rng(span);
        let ins = if rng(10) == 0 { "[[typ]] " } else { "x" };
        let hint = EditRange {
            start: pos,
            old_end: pos,
            new_end: pos + ins.len(),
        };
        text.insert_str(pos, ins);
        let current = TestText::new(text.clone());
        let start = Instant::now();
        let report = index.sync(&current, Some(hint));
        let elapsed = start.elapsed();
        if report.rebuilt_segments > 10 {
            fence_touching += 1;
        }
        samples.push(elapsed);
    }
    println!("S5-T4 prose-typing syncs with >10 rebuilt segments: {fence_touching}/500");
    let p99 = percentile(&mut samples, 99);
    println!("S5-T4 sync@1MiB p99={:.4} ms (500 edits)", ms(p99));
    assert!(
        p99 < std::time::Duration::from_millis(2),
        "segment sync p99 {:.4} ms ≥ 2 ms (NFR-3 budget 1.5 ms + slack)",
        ms(p99)
    );

    // Adversarial: unclosed fence at line 0 — each sync's tail is the
    // whole document; the cost must stay ≤ the bridge baseline (never
    // worse than the pre-segment path).
    let mut adv = String::with_capacity(MAX_DOC_BYTES + 8);
    adv.push_str("```\n");
    adv.push_str(&corpus_doc(MAX_DOC_BYTES));
    let mut index2 = SegmentIndex::new();
    index2.sync(&TestText::new(adv.clone()), None);
    let mut samples2 = Vec::with_capacity(100);
    let mut text2 = adv.clone();
    for _ in 0..100 {
        let pos = text2.len() - 3;
        text2.insert(pos, 'x');
        let hint = EditRange {
            start: pos,
            old_end: pos,
            new_end: pos + 1,
        };
        let start = Instant::now();
        index2.sync(&TestText::new(text2.clone()), Some(hint));
        samples2.push(start.elapsed());
    }
    let adv_p99 = percentile(&mut samples2, 99);
    println!(
        "S5-T4 adversarial(unclosed@0)@1MiB p99={:.4} ms",
        ms(adv_p99)
    );
    assert!(
        adv_p99 <= bridge_p50 * 3,
        "adversarial sync p99 {:.4} ms > 3× bridge {:.4} ms",
        ms(adv_p99),
        ms(bridge_p50)
    );
}
