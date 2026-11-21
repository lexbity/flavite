//! S5-T1 / S5-T2: segment-cache parity and incremental equivalence
//! (RFC-0011 AC-5 / FR-6).
//!
//! S5-T1 — whole-vs-segment parity: `SegmentIndex::build` spans equal the
//! whole-document pulldown bridge spans byte-for-byte (ranges and kinds) on
//! the acceptance corpus, including the fence-corner fixtures added by S5.
//!
//! S5-T2 — incremental equivalence: after EVERY edit of randomized
//! sequences on corpus-style documents, `SegmentIndex::sync(hint)` spans
//! equal a fresh full bridge reparse. Hint-perturb probes also run: hints
//! shifted within/around the edited region (which overlap the change) must
//! still yield correct spans, and out-of-range hints must repair via full
//! rebuild — never wrong spans.

use flavite_syntax::EditRange;
use flavite_syntax::pulldown_glue::parse_normalized;
use flavite_syntax::segment::SegmentIndex;
use flavite_syntax::text::TextSnapshot;
use flavite_syntax::text_window::StructureText;
use std::fs;
use std::path::PathBuf;

const CORPUS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");
const KNOWN_DIVERGENCES: &str = "KNOWN-DIVERGENCES.md";

/// Canonical span triple (start, end, kind-relevant payload).
type SpanSet = (
    Vec<(usize, usize, String)>,
    Vec<(usize, usize)>,
    Vec<(usize, usize)>,
);

/// The test text: a `String`-backed `StructureText` with an O(1)
/// precomputed line-start index (mirrors `DocumentText` query cost).
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

fn bridge_spans(text: &str) -> SpanSet {
    let parsed = parse_normalized(text).expect("bridge parse");
    let links = parsed
        .links
        .into_iter()
        .map(|l| (l.span.start.0, l.span.end.0, l.target_raw))
        .collect();
    let tags = parsed
        .tags
        .into_iter()
        .map(|s| (s.start.0, s.end.0))
        .collect();
    let block_ids = parsed
        .block_ids
        .into_iter()
        .map(|s| (s.start.0, s.end.0))
        .collect();
    (links, tags, block_ids)
}

fn segment_spans(index: &SegmentIndex) -> SpanSet {
    let (links, tags, block_ids) = index.spans();
    let links = links
        .into_iter()
        .map(|l| (l.span.start.0, l.span.end.0, l.target_raw))
        .collect();
    let tags = tags.into_iter().map(|s| (s.start.0, s.end.0)).collect();
    let block_ids = block_ids
        .into_iter()
        .map(|s| (s.start.0, s.end.0))
        .collect();
    (links, tags, block_ids)
}

/// Normalize fixture bytes the same way the bridge does (BOM/CRLF folding),
/// so both paths compare on identical text.
fn normalized_fixture(bytes: &[u8]) -> String {
    let snapshot = TextSnapshot::new(bytes).expect("decodable fixture");
    snapshot.text().to_string()
}

fn corpus_fixtures() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(CORPUS_DIR)
        .expect("corpus dir must exist")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|x| x == "md")
                && p.file_name().is_some_and(|n| n != KNOWN_DIVERGENCES)
        })
        .map(|p| {
            let bytes = fs::read(&p).expect("read fixture");
            (normalized_fixture(&bytes), p)
        })
        .collect();
    out.sort_by(|a, b| a.1.cmp(&b.1));
    out
}

/// S5-T1: whole-vs-segment parity on the full corpus.
#[test]
fn segment_build_matches_the_whole_document_bridge_on_the_corpus() {
    let fixtures = corpus_fixtures();
    assert!(!fixtures.is_empty(), "corpus must contain fixtures");
    let mut checked = 0usize;
    for (text, path) in &fixtures {
        let mut index = SegmentIndex::new();
        index.sync(&TestText::new(text.clone()), None);
        let segments = segment_spans(&index);
        let bridge = bridge_spans(text);
        assert_eq!(
            segments,
            bridge,
            "segment spans diverge from the whole-document bridge on {}",
            path.display()
        );
        assert!(index.segment_count() >= 1);
        // All spans must be in bounds and char-aligned.
        for (start, end, _) in &segments.0 {
            assert!(start <= end && *end <= text.len());
            assert!(text.is_char_boundary(*start) && text.is_char_boundary(*end));
        }
        checked += 1;
    }
    eprintln!("[S5-T1] {checked} corpus fixtures agree segment-vs-bridge");
}

/// Deterministic edit sequence returning (texts, hints).
fn apply_edit_sequence(base: &str, seed: u64, steps: usize) -> (Vec<String>, Vec<EditRange>) {
    let mut texts = vec![base.to_string()];
    let mut hints = Vec::new();
    let mut state = seed;
    let mut rng = |range: usize| {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (state >> 33) as usize % range
    };
    for _ in 0..steps {
        let cur = texts.last().unwrap().clone();
        if cur.is_empty() {
            break;
        }
        let len = cur.len();
        let pos = rng(len + 1);
        let kind = rng(4);
        let (start, old_end, replacement) = match kind {
            0 => {
                let chunk = match rng(4) {
                    0 => "[[lnk]] ",
                    1 => "#tag ",
                    2 => "```\n[[fenced]]\n```\n",
                    _ => "x\n",
                };
                (pos, pos, chunk.to_string())
            }
            1 => {
                let end = (pos + rng(8) + 1).min(len);
                (pos, end, String::new())
            }
            2 => {
                let end = (pos + rng(6) + 1).min(len);
                let replacement = match rng(2) {
                    0 => "[[rep]]".to_string(),
                    _ => "yy".to_string(),
                };
                (pos, end, replacement)
            }
            _ => {
                // Fence toggle: insert a fence opener/closer fragment.
                let end = (pos + rng(4) + 1).min(len);
                let replacement = match rng(2) {
                    0 => "```\n".to_string(),
                    _ => "\n```\n".to_string(),
                };
                (pos, end, replacement)
            }
        };
        let start = fcb(&cur, start);
        let old_end = fcb(&cur, old_end);
        let mut next = cur;
        next.replace_range(start..old_end, &replacement);
        let new_end = start + replacement.len();
        texts.push(next);
        hints.push(EditRange {
            start,
            old_end,
            new_end,
        });
    }
    (texts, hints)
}

fn fcb(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while !text.is_char_boundary(at) {
        at = at.saturating_sub(1);
    }
    at
}

#[test]
fn segment_incremental_matches_full_after_every_edit() {
    let bases = [
        "# H [[link]] #tag ^block\n\nBody with `[[code]]` and ![[embed.png]].\n",
        "- item [[a]]\n- item2 #t\n\n| [[c]] | d |\n|---|---|\n| e | [[f]] |\n",
        "> quote [[q]]\n\n```rust\n[[fenced]] #f\n```\n\n[[after]]\n",
        "```\n[[unclosed]]\n",
        "---\ntitle: T\n---\n\nBody [[b]]\n",
        "[[a]] [[b]] [[c]]\n\n#tag ^block\n",
    ];
    let mut passed = 0usize;
    for &base in &bases {
        for seed in 0..40u64 {
            let (texts, hints) = apply_edit_sequence(base, seed, 8);
            let mut index = SegmentIndex::new();
            index.sync(&TestText::new(texts[0].clone()), None);
            assert_eq!(segment_spans(&index), bridge_spans(&texts[0]), "seed");
            for (text, hint) in texts.iter().skip(1).zip(&hints) {
                if index.is_degraded() {
                    break;
                }
                index.sync(&TestText::new(text.clone()), Some(*hint));
                let full = bridge_spans(text);
                let got = segment_spans(&index);
                if got != full {
                    eprintln!("[S5-T2] base={base:?} seed={seed} hint={hint:?}");
                    eprintln!("[S5-T2] text={text:?}");
                    eprintln!("[S5-T2] segs={:?}", index.debug_segments());
                    eprintln!("[S5-T2] got={got:?}");
                    eprintln!("[S5-T2] full={full:?}");
                    panic!("incremental spans diverge from full reparse");
                }
            }
            passed += 1;
        }
    }
    eprintln!("[S5-T2] {passed} sequences (6 bases × 40 seeds × 8 edits) all equal full reparse");
}

#[test]
fn hint_perturbations_never_produce_wrong_spans() {
    let base = "# H [[link]] #tag\n\n```rust\n[[fenced]]\n```\n\nAfter [[a]].\n";
    // Perturbation 1: hint shifted within/around the edited region (overlaps
    // the change) — must still be correct.
    let mut text = base.to_string();
    let mut index = SegmentIndex::new();
    index.sync(&TestText::new(text.clone()), None);
    let edits = [
        ("X", 10usize),
        ("\n```\n[[new]]\n```\n", text.len()),
        ("", 40),
        ("[[extra]] ", 3),
    ];
    for (i, (replacement, at)) in edits.iter().enumerate() {
        let at = fcb(&text, *at);
        let before = text.clone();
        text.replace_range(at..at, replacement);
        // Hint perturbed by a few bytes around the edit (still overlapping).
        let perturb = at.saturating_sub(2 + ((i * 3) % 7));
        let hint = EditRange {
            start: perturb,
            old_end: at,
            new_end: at + replacement.len(),
        };
        index.sync(&TestText::new(text.clone()), Some(hint));
        assert!(!index.is_degraded());
        assert_eq!(
            segment_spans(&index),
            bridge_spans(&text),
            "perturbed hint {i}"
        );
        drop(before);
    }

    // Perturbation 2: stale hint from the previous edit (overlaps the
    // previous region; the current text is the old one plus nothing new).
    let stale_hint = EditRange {
        start: 8,
        old_end: 8,
        new_end: 9,
    };
    index.sync(&TestText::new(text.clone()), Some(stale_hint));
    assert_eq!(segment_spans(&index), bridge_spans(&text), "stale hint");

    // Perturbation 3: out-of-range hint → repair via full rebuild.
    let mut index2 = SegmentIndex::new();
    index2.sync(&TestText::new(text.clone()), None);
    let absurd = EditRange {
        start: text.len() + 100,
        old_end: text.len() + 110,
        new_end: text.len() + 120,
    };
    let report = index2.sync(&TestText::new(text.clone()), Some(absurd));
    assert!(!index2.is_degraded());
    assert!(report.rebuilt_segments >= 1);
    assert_eq!(
        segment_spans(&index2),
        bridge_spans(&text),
        "out-of-range hint"
    );
}
