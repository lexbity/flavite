//! S8-T1: incremental structure equivalence (RFC-0008, R6 killer).
//!
//! Property: after **any** edit sequence, the `StructureEngine`'s spans
//! (incremental tree edit + damaged-region rescan) equal a fresh full
//! parse (`ts_ranges::parse_note_ts`) byte-for-byte — links, tags and block
//! IDs, with identical ranges and kinds.
//!
//! Also probes the D10 desync guard: an intentionally wrong `InputEdit`
//! must either be repaired by a full reparse (spans still correct) or
//! trigger the plain-mode degradation safely (empty spans), never produce
//! silently wrong spans.

use flavite_syntax::structure::{EditRange, StructureEngine};
use flavite_syntax::ts_ranges::parse_note_ts;
use proptest::prelude::*;

/// Canonical span triple used for comparison.
type SpanSet = (
    Vec<(usize, usize, String)>,
    Vec<(usize, usize)>,
    Vec<(usize, usize)>,
);

fn fcb(text: &str, byte: usize) -> usize {
    let mut at = byte.min(text.len());
    while !text.is_char_boundary(at) {
        at = at.saturating_sub(1);
    }
    at
}

fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
    starts
}

/// A deterministic pseudo-random edit sequence over a base text.
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
        let kind = rng(3);
        let (start, old_end, replacement) = match kind {
            0 => {
                // Insert a short ASCII / newline-fragment chunk.
                let chunk = match rng(3) {
                    0 => "[[lnk]] ",
                    1 => "#tag ",
                    _ => "x\n",
                };
                (pos, pos, chunk.to_string())
            }
            1 => {
                // Delete up to 8 bytes (char-boundary safe on ASCII corpus).
                let end = (pos + rng(8) + 1).min(len);
                (pos, end, String::new())
            }
            _ => {
                // Replace a small run.
                let end = (pos + rng(6) + 1).min(len);
                let replacement = match rng(2) {
                    0 => "[[rep]]".to_string(),
                    _ => "yy".to_string(),
                };
                (pos, end, replacement)
            }
        };
        // Keep edits on char boundaries (corpus is ASCII except unicode.md).
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

fn full_spans(text: &str) -> SpanSet {
    let parsed = parse_note_ts(text.as_bytes()).expect("full parse");
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

fn engine_spans(engine: &mut StructureEngine, text: &str, hint: Option<EditRange>) -> SpanSet {
    let lines = line_starts(text);
    let spans = engine.sync(text, &lines, hint).expect("sync");
    let links = spans
        .links
        .into_iter()
        .map(|l| (l.span.start.0, l.span.end.0, l.target_raw))
        .collect();
    let tags = spans
        .tags
        .into_iter()
        .map(|s| (s.start.0, s.end.0))
        .collect();
    let block_ids = spans
        .block_ids
        .into_iter()
        .map(|s| (s.start.0, s.end.0))
        .collect();
    (links, tags, block_ids)
}

proptest! {
    /// For a variety of base documents and edit seeds, every incremental
    /// step must equal a fresh full parse.
    #[test]
    fn incremental_equals_full_after_every_edit(
        base in prop::sample::select(&[
            "# H [[link]] #tag ^block\n\nBody with `[[code]]` and ![[embed.png]].\n",
            "- item [[a]]\n- item2 #t\n\n| [[c]] | d |\n|---|---|\n| e | [[f]] |\n",
            "> quote [[q]]\n\n```\n[[fenced]]\n```\n\n[[after]]\n",
            "[[a]] [[b]] [[c]]\n\n#tag ^block\n",
        ]),
        seed in 0u64..10_000,
        steps in 3usize..12,
    ) {
        let (texts, hints) = apply_edit_sequence(base, seed, steps);
        let mut engine = StructureEngine::new();

        // Seed: first sync is full (no hint).
        assert_eq!(
            engine_spans(&mut engine, &texts[0], None),
            full_spans(&texts[0]),
            "seed full parse"
        );

        for (index, (text, hint)) in texts.iter().skip(1).zip(&hints).enumerate() {
            // Degradation (D10) is a designed terminal state: after 3
            // consecutive forced rebuilds the engine goes plain (empty
            // spans) for the session. Once degraded, stop comparing — the
            // contract is "empty spans, editing unaffected", not equality.
            if engine.is_plain() {
                let lines = line_starts(text);
                let spans = engine.sync(text, &lines, Some(*hint)).expect("sync");
                prop_assert!(spans.links.is_empty() && spans.tags.is_empty());
                continue;
            }
            let incremental = engine_spans(&mut engine, text, Some(*hint));
            let full = full_spans(text);
            if incremental != full {
                eprintln!("[debug] base={base:?} seed={seed} steps={steps} failing index={index}");
                eprintln!("[debug] hint={hint:?}");
                eprintln!("[debug] text={text:?}");
                eprintln!("[debug] incremental={incremental:?}");
                eprintln!("[debug] full={full:?}");
            }
            prop_assert_eq!(
                incremental, full,
                "incremental must equal full after edit {:?} on {:?}",
                hint, base
            );
        }
    }

    /// Desync probe: a wrong hint must never produce silently wrong spans —
    /// either the guard repairs via full reparse, or degradation yields
    /// empty spans after 3 consecutive rebuilds.
    #[test]
    fn desync_guard_never_produces_wrong_spans(
        base in prop::sample::select(&[
            "# H [[link]] #tag ^block\n\nBody.\n",
            "[[a]] #tag\n",
        ]),
        _seed in 0u64..2_000,
    ) {
        let mut text = base.to_string();
        let mut engine = StructureEngine::new();
        let lines = line_starts(&text);
        let _ = engine.sync(&text, &lines, None).expect("seed");

        for i in 0..4 {
            // Grow the text so the stale hint is out of bounds and invalid.
            text.push_str(&format!(" extra {i}"));
            let lines = line_starts(&text);
            let bad = EditRange {
                start: text.len().saturating_add(4),
                old_end: text.len().saturating_add(8),
                new_end: text.len().saturating_add(12),
            };
            let spans = engine.sync(&text, &lines, Some(bad)).expect("sync");
            if i < 3 {
                // Repair path: spans still equal a full parse.
                let full = full_spans(&text);
                let got = engine_spans_from(spans);
                prop_assert_eq!(got, full, "guard repair must restore correct spans");
            } else {
                // Degraded after 3 consecutive rebuilds (D10).
                prop_assert!(engine.is_plain(), "must degrade");
                prop_assert!(spans.links.is_empty() && spans.tags.is_empty());
            }
        }
    }
}

/// Convert already-synced `StructureSpans` into the comparison triple.
fn engine_spans_from(spans: flavite_syntax::structure::StructureSpans) -> SpanSet {
    let links = spans
        .links
        .into_iter()
        .map(|l| (l.span.start.0, l.span.end.0, l.target_raw))
        .collect();
    let tags = spans
        .tags
        .into_iter()
        .map(|s| (s.start.0, s.end.0))
        .collect();
    let block_ids = spans
        .block_ids
        .into_iter()
        .map(|s| (s.start.0, s.end.0))
        .collect();
    (links, tags, block_ids)
}
