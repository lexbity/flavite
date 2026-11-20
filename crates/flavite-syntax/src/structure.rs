//! Editor-side structure engine (RFC-0008 D7/D8, S7–S8).
//!
//! The editor widget consumes structure spans through this type — never
//! through tree-sitter directly, keeping the `flavite-editor` → syntax
//! dependency graph unchanged (§10.2 layering; `arch-check` gates it).
//!
//! # Stage B (S7) — full parse behind the flag
//!
//! [`StructureEngine::sync`] performed a full [`crate::ts_ranges::parse_note_ts`]
//! per call.
//!
//! # Stage C (S8) — incremental reparses
//!
//! The engine now owns the block and inline tree-sitter parsers/trees.
//! When a trusted [`EditRange`] hint (vault session, D1) and prior trees
//! exist, it applies the edit to both trees (`tree.edit(InputEdit)`) and
//! reparses incrementally (`parser.parse(text, Some(&old_tree))`), then
//! rescans only the tree-reported changed regions — per-keystroke cost is
//! O(damaged region), not O(document). A coherence guard (D10) verifies the
//! reparsed tree covers the whole text; on any suspicion the engine drops
//! the trees and full-reparses. After 3 consecutive forced rebuilds on
//! distinct revisions it degrades to plain mode (no spans) for the session,
//! warning once — rendering never blocks on structure.
//!
//! # Why the hint type lives here
//!
//! RFC-0008 §5.2 originally placed [`EditRange`] in `flavite-editor`'s
//! surface, but the hint is *produced* by `flavite-vault` and `flavite-api`
//! (both below the editor) and *consumed* by this engine. The type therefore
//! lives in the lowest shared crate — flavite-syntax — and is re-exported by
//! the surface for the editor-facing name.

use crate::mask::TextMask;
use crate::scanner::scan_inline_text;
use crate::text::{Span, TextByte};
use crate::ts_ranges::push_scanned_span;
use std::ops::Range;
use tree_sitter::{InputEdit, Parser, Point, Tree};

/// A single-edit changed-range hint, in pre-edit byte coordinates plus the
/// new end (RFC-0008 D1).
///
/// `start..old_end` is the replaced range in the pre-edit text; `new_end`
/// is the end of the replacement (`start + replacement.len()`) in the
/// post-edit text. `None` means "no trustworthy hint" (external reload,
/// merge, journal patch, first open, or a multi-op batch).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EditRange {
    /// Byte offset where the edit started (pre-edit coordinates).
    pub start: usize,
    /// Byte offset just past the replaced range (pre-edit coordinates).
    pub old_end: usize,
    /// Byte offset just past the replacement (post-edit coordinates).
    pub new_end: usize,
}

/// The spans produced by a structure sync, in the engine's own (syntax)
/// types. The widget converts these into its link/decoration views.
#[derive(Debug, Clone, PartialEq)]
pub struct StructureSpans {
    /// All link spans found (wikilinks and embeds).
    pub links: Vec<crate::pulldown_glue::Link>,
    /// All tag spans found.
    pub tags: Vec<Span<TextByte>>,
    /// All block ID spans found.
    pub block_ids: Vec<Span<TextByte>>,
    /// The text mask excluding code/HTML/math regions.
    pub mask: TextMask,
}

impl StructureSpans {
    /// Empty spans covering a text of `len` bytes (plain mode).
    #[must_use]
    pub fn empty(len: usize) -> Self {
        Self {
            links: Vec::new(),
            tags: Vec::new(),
            block_ids: Vec::new(),
            mask: TextMask::empty(len),
        }
    }
}

/// Consecutive full-rebuild syncs (on distinct revisions) after which the
/// engine degrades to plain mode (RFC-0008 D10).
const MAX_CONSECUTIVE_REBUILDS: u32 = 3;

/// Extra context around each tree-reported changed range that is rescanned,
/// so spans straddling an edit boundary (e.g. the surviving prefix of a
/// wikilink/tag that was edited mid-span) are re-derived rather than lost.
/// 256 bytes covers typical note lines and link targets; pathological longer
/// boundary-crossing spans are covered by the coherence guard / plain-mode
/// degradation instead.
const RESCAN_CONTEXT: usize = 256;

/// Cached spans from the previous revision, in that revision's coordinates.
#[derive(Debug, Clone, PartialEq)]
struct CachedSpans {
    links: Vec<crate::pulldown_glue::Link>,
    tags: Vec<Span<TextByte>>,
    block_ids: Vec<Span<TextByte>>,
}

/// The widget's structure source: owns the incremental tree-sitter state and
/// produces spans.
#[derive(Default)]
pub struct StructureEngine {
    /// Block-grammar parser (created on first use).
    block_parser: Option<Parser>,
    /// Inline-grammar parser (created on first use).
    inline_parser: Option<Parser>,
    /// Cached block tree of the last synced revision.
    block_tree: Option<Tree>,
    /// Cached inline tree of the last synced revision.
    inline_tree: Option<Tree>,
    /// Cached spans of the last synced revision.
    spans: Option<CachedSpans>,
    /// Line-start offsets of the last synced text (for `InputEdit` row/col
    /// geometry of the *pre-edit* text). O(lines) — the small-file path
    /// (NFR-7); the paged-buffer RFC replaces this with per-chunk summaries.
    last_lines: Vec<usize>,
    /// Byte length of the last synced text.
    last_len: usize,
    /// Consecutive forced full-rebuild syncs since the last clean
    /// incremental sync.
    rebuilds: u32,
    /// Degraded to plain mode (no spans) for the session.
    plain: bool,
    /// Accumulated edit magnitude since the last full parse (re-anchor
    /// threshold: `last_len / 8`). Bounds cumulative tree drift from long
    /// incremental runs.
    pending_churn: usize,
    /// Number of prose windows of the last synced revision (drift guard).
    last_windows: Option<usize>,
    /// While the document is in grammar error-recovery territory, parse
    /// canonically (full) until it heals — incremental edits on a divergent
    /// tree can produce spans that differ from a canonical parse. Clean
    /// documents stay on the sub-ms incremental path.
    require_full_until_clean: bool,
}

impl std::fmt::Debug for StructureEngine {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StructureEngine")
            .field("block_tree", &self.block_tree.is_some())
            .field("inline_tree", &self.inline_tree.is_some())
            .field("cached_spans", &self.spans.is_some())
            .field("last_len", &self.last_len)
            .field("rebuilds", &self.rebuilds)
            .field("plain", &self.plain)
            .finish_non_exhaustive()
    }
}

/// How spans were produced for a sync: incrementally (rescanned damaged
/// regions) or as a full scan.
enum Scan {
    /// Incremental: rescan exactly these byte ranges.
    Incremental(Vec<Range<usize>>),
    /// Full scan over every prose window.
    Full,
}

/// Outcome of an incremental attempt.
#[derive(Debug)]
enum IncrementalOutcome {
    /// Synced incrementally; rescan these byte ranges.
    Synced(Tree, Tree, Vec<Range<usize>>),
    /// No prior state (first sync) — a normal full parse, not a failure.
    NoState,
    /// No hint (reload/merge/journal/first open) — a normal full parse.
    NoHint,
    /// The incremental attempt failed a coherence guard or had an invalid
    /// hint — a forced rebuild (counts toward plain-mode degradation).
    GuardTrip,
    /// The incremental result is in grammar error-recovery territory (or the
    /// damage is huge): a canonical full reparse is required for correct
    /// spans. Normal maintenance — not a failure, not counted.
    Repair,
}

impl StructureEngine {
    /// Create an empty engine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the engine has degraded to plain mode (D10). The widget logs
    /// the warning once when this first becomes true.
    #[must_use]
    pub fn is_plain(&self) -> bool {
        self.plain
    }

    /// Synchronize structure with the current `text` (normalized) and return
    /// spans.
    ///
    /// * `lines` — line-start byte offsets of `text` (ascending, starts with
    ///   `0`); used for `InputEdit` row/column geometry.
    /// * `hint` — the session-computed single-edit changed-range hint; when
    ///   `Some` and prior trees exist, the block and inline grammars are
    ///   reparsed incrementally and only the damaged regions are rescanned.
    ///   When `None` (external reload, merge, journal patch, first open,
    ///   multi-op batch) or no prior state exists, the document is fully
    ///   reparsed and rescanned.
    ///
    /// # Errors
    ///
    /// Returns an error when the text carries an embedded NUL or both
    /// tree-sitter grammars fail to parse it.
    pub fn sync(
        &mut self,
        text: &str,
        lines: &[usize],
        hint: Option<EditRange>,
    ) -> Result<StructureSpans, String> {
        if self.plain {
            return Ok(StructureSpans::empty(text.len()));
        }
        // tree-sitter's C parser treats embedded NUL as an input terminator.
        if text.contains('\0') {
            return Err("markdown contains an embedded NUL byte".into());
        }

        let (block, inline, scan) = match self.try_incremental(text, lines, hint) {
            IncrementalOutcome::Synced(block, inline, changed) => {
                // A clean incremental sync resets the rebuild streak.
                self.rebuilds = 0;
                (block, inline, Scan::Incremental(changed))
            }
            IncrementalOutcome::GuardTrip => {
                // Forced rebuild (D10): count toward degradation.
                self.rebuilds += 1;
                if self.rebuilds > MAX_CONSECUTIVE_REBUILDS {
                    self.plain = true;
                    return Ok(StructureSpans::empty(text.len()));
                }
                let (b, i) = self.run_full_reparse(text)?;
                self.pending_churn = 0;
                (b, i, Scan::Full)
            }
            // First sync or a legitimate no-hint full parse: no penalty.
            IncrementalOutcome::NoState | IncrementalOutcome::NoHint => {
                let (b, i) = self.run_full_reparse(text)?;
                self.pending_churn = 0;
                (b, i, Scan::Full)
            }
            // Error-recovery, huge damage, or accumulated-churn re-anchor:
            // full reparse for canonical spans; not a failure, not counted.
            IncrementalOutcome::Repair => {
                let (b, i) = self.run_full_reparse(text)?;
                self.pending_churn = 0;
                (b, i, Scan::Full)
            }
        };

        // Structure-level divergence guards: if the incremental tree lost all
        // prose windows on non-empty text, or the window count changed
        // drastically versus the previous revision, the incremental tree has
        // diverged from a canonical parse — repair with a full reparse.
        let mut block = block;
        let mut inline = inline;
        let mut scan = scan;
        if matches!(scan, Scan::Incremental(_)) {
            let (_mask, windows) =
                crate::ts_ranges::mask_and_windows_from_trees(text, &block, Some(&inline));
            let non_empty = !text.trim().is_empty();
            let windows_collapsed = non_empty && windows.is_empty();
            let count_drift = self.last_windows.is_some_and(|prev| {
                prev > 0 && (windows.len() * 2 < prev || windows.len() > prev * 2)
            });
            if windows_collapsed || count_drift {
                let (b, i) = self.run_full_reparse(text)?;
                self.pending_churn = 0;
                block = b;
                inline = i;
                scan = Scan::Full;
            }
        }
        let (mask, windows) =
            crate::ts_ranges::mask_and_windows_from_trees(text, &block, Some(&inline));

        let spans = match (self.spans.take(), &scan) {
            (Some(prev), Scan::Incremental(changed)) => {
                let hint = hint.expect("incremental mode implies a hint");
                self.merge_spans(text, prev, hint, changed, &mask, &windows)
            }
            _ => full_scan(text, &mask, &windows),
        };

        self.block_tree = Some(block);
        self.inline_tree = Some(inline);
        self.last_lines = lines.to_vec();
        self.last_len = text.len();
        self.last_windows = Some(windows.len());
        self.spans = Some(spans.clone());

        Ok(StructureSpans {
            links: spans.links,
            tags: spans.tags,
            block_ids: spans.block_ids,
            mask,
        })
    }

    /// Attempt an incremental resync from the cached trees and a hint.
    fn try_incremental(
        &mut self,
        text: &str,
        lines: &[usize],
        hint: Option<EditRange>,
    ) -> IncrementalOutcome {
        let Some(hint) = hint else {
            return IncrementalOutcome::NoHint;
        };
        // No prior state (or last state is not the pre-edit text): full.
        if self.block_tree.is_none() || self.inline_tree.is_none() {
            return IncrementalOutcome::NoState;
        }
        // A document still in grammar error-recovery territory must stay on
        // the canonical full-parse path until it heals.
        if self.require_full_until_clean {
            return IncrementalOutcome::Repair;
        }
        // Re-anchor once accumulated churn exceeds an eighth of the document:
        // bounds cumulative tree drift from long incremental runs while
        // ordinary typing (small edits) stays incremental.
        if self.pending_churn > self.last_len / 8 {
            return IncrementalOutcome::Repair;
        }
        let Some(old_block) = self.block_tree.take() else {
            return IncrementalOutcome::NoState;
        };
        let Some(old_inline) = self.inline_tree.take() else {
            return IncrementalOutcome::NoState;
        };
        let Some(edit) = Self::input_edit(&self.last_lines, self.last_len, lines, text.len(), hint)
        else {
            return IncrementalOutcome::GuardTrip;
        };

        let mut block = old_block;
        let mut inline = old_inline;
        block.edit(&edit);
        inline.edit(&edit);

        let Some(new_block) = self.parse_block_with(text, Some(&block)) else {
            return IncrementalOutcome::GuardTrip;
        };
        let Some(new_inline) = self.parse_inline_with(text, Some(&inline)) else {
            return IncrementalOutcome::GuardTrip;
        };
        // D10 coherence guard: the reparsed trees must cover the whole text.
        if new_block.root_node().end_byte() != text.len()
            || new_inline.root_node().end_byte() != text.len()
        {
            return IncrementalOutcome::GuardTrip;
        }

        // Grammar error-recovery territory: tree-sitter's incremental edits
        // preserve the previous tree's recovery layout, which can diverge
        // from a canonical parse (e.g. after deleting a fence opener). Spans
        // must come from a canonical parse in that state (the node's stored
        // error bit is an O(1) read).
        if new_block.root_node().has_error() || new_inline.root_node().has_error() {
            return IncrementalOutcome::Repair;
        }

        // The edit span itself is always damaged even when the grammars
        // report no structural change; seed the rescan set with it.
        let edit_start = hint.start.saturating_sub(RESCAN_CONTEXT);
        let edit_end = (hint.new_end + RESCAN_CONTEXT).min(text.len());
        let mut changed: Vec<Range<usize>> = Vec::new();
        changed.push(edit_start..edit_end);
        changed.extend(
            block
                .changed_ranges(&new_block)
                .map(|r| r.start_byte..r.end_byte),
        );
        changed.extend(
            inline
                .changed_ranges(&new_inline)
                .map(|r| r.start_byte..r.end_byte),
        );
        // Pad with context so boundary-crossing spans are fully rescanned.
        for r in &mut changed {
            r.start = r.start.saturating_sub(RESCAN_CONTEXT);
            r.end = (r.end + RESCAN_CONTEXT).min(text.len());
        }
        changed.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(changed.len());
        for r in changed {
            match merged.last_mut() {
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        // Huge damage: repairing with a full scan is cheaper and canonical.
        let damaged_bytes: usize = merged.iter().map(|r| r.end - r.start).sum();
        if damaged_bytes > text.len() / 2 {
            return IncrementalOutcome::Repair;
        }
        self.pending_churn += (hint.old_end - hint.start) + hint.new_end.abs_diff(hint.old_end);
        IncrementalOutcome::Synced(new_block, new_inline, merged)
    }

    /// Full reparse of `text` with both grammars, guarded.
    fn full_reparse(&mut self, text: &str) -> Result<(Tree, Tree), String> {
        let block = self
            .parse_block_with(text, None)
            .ok_or("tree-sitter block parse failed")?;
        let inline = self
            .parse_inline_with(text, None)
            .ok_or("tree-sitter inline parse failed")?;
        if block.root_node().end_byte() != text.len() || inline.root_node().end_byte() != text.len()
        {
            return Err("tree-sitter parse length mismatch".into());
        }
        Ok((block, inline))
    }

    /// Full reparse that also updates the canonical-parse cooldown: while
    /// the document stays in grammar error-recovery territory, keep parsing
    /// fully until it heals.
    fn run_full_reparse(&mut self, text: &str) -> Result<(Tree, Tree), String> {
        let (block, inline) = self.full_reparse(text)?;
        self.require_full_until_clean =
            block.root_node().has_error() || inline.root_node().has_error();
        Ok((block, inline))
    }

    fn parse_block_with(&mut self, text: &str, old: Option<&Tree>) -> Option<Tree> {
        let parser = self.block_parser.get_or_insert_with(|| {
            let mut parser = Parser::new();
            parser
                .set_language(&crate::ts_ranges::markdown_language())
                .expect("markdown block grammar is compiled in");
            parser
        });
        parser.parse(text, old)
    }

    fn parse_inline_with(&mut self, text: &str, old: Option<&Tree>) -> Option<Tree> {
        let parser = self.inline_parser.get_or_insert_with(|| {
            let mut parser = Parser::new();
            parser
                .set_language(&tree_sitter_md::INLINE_LANGUAGE.into())
                .expect("markdown inline grammar is compiled in");
            parser
        });
        parser.parse(text, old)
    }

    /// Build a `InputEdit` from the hint. Position geometry is computed from
    /// line-start indices only (O(log lines) per point); out-of-bounds or
    /// inverted hints return `None` (caller falls back to a full parse).
    fn input_edit(
        lines_old: &[usize],
        len_old: usize,
        lines_new: &[usize],
        len_new: usize,
        hint: EditRange,
    ) -> Option<InputEdit> {
        if hint.start > hint.old_end || hint.old_end > len_old || hint.new_end > len_new {
            return None;
        }
        Some(InputEdit {
            start_byte: hint.start,
            old_end_byte: hint.old_end,
            new_end_byte: hint.new_end,
            start_position: Self::point_at(lines_old, hint.start, len_old),
            old_end_position: Self::point_at(lines_old, hint.old_end, len_old),
            new_end_position: Self::point_at(lines_new, hint.new_end, len_new),
        })
    }

    /// Row/column of `byte` from `line_starts` (zero-based; columns in
    /// bytes). Defensive on empty/odd line tables.
    fn point_at(line_starts: &[usize], byte: usize, len: usize) -> Point {
        let clamped = byte.min(len);
        if line_starts.is_empty() {
            return Point {
                row: 0,
                column: clamped,
            };
        }
        let row = line_starts
            .partition_point(|start| *start <= clamped)
            .saturating_sub(1);
        let line_start = line_starts.get(row).copied().unwrap_or(0);
        Point {
            row,
            column: clamped.saturating_sub(line_start),
        }
    }

    /// Merge the cached spans with a rescan of the damaged regions.
    ///
    /// Any prose window intersecting a tree-reported changed range is
    /// **fully** rescanned and its previous spans are dropped — its span set
    /// then equals exactly what a full scan would compute for that window.
    /// Spans in unaffected windows are translated by the edit's length delta
    /// (they sit in text that did not change, so translation is exact).
    /// This is the per-keystroke O(damaged region) path, and it keeps the
    /// incremental result equal to the full scan by construction for every
    /// window.
    fn merge_spans(
        &self,
        text: &str,
        prev: CachedSpans,
        hint: EditRange,
        changed: &[Range<usize>],
        mask: &TextMask,
        windows: &[Range<usize>],
    ) -> CachedSpans {
        let delta = hint.new_end as isize - hint.old_end as isize;

        // Which windows are affected (intersect any changed range)?
        let mut affected = vec![false; windows.len()];
        for r in changed {
            let mut wi = windows.partition_point(|w| w.end <= r.start);
            while wi < windows.len() && windows[wi].start < r.end {
                affected[wi] = true;
                wi += 1;
            }
        }
        let in_affected = |s: usize, e: usize| {
            affected
                .iter()
                .zip(windows)
                .any(|(on, w)| *on && s < w.end && e > w.start)
        };

        let mut links: Vec<crate::pulldown_glue::Link> = prev
            .links
            .into_iter()
            .filter_map(|mut link| {
                let s = link.span.start.0;
                let e = link.span.end.0;
                let (s, e) = if e <= hint.start {
                    (s, e)
                } else if s >= hint.old_end {
                    ((s as isize + delta) as usize, (e as isize + delta) as usize)
                } else {
                    return None;
                };
                link.span = Span::new(TextByte(s), TextByte(e));
                Some(link)
            })
            .filter(|link| !in_affected(link.span.start.0, link.span.end.0))
            .collect();
        let mut tags: Vec<Span<TextByte>> = prev
            .tags
            .into_iter()
            .filter_map(|span| {
                let s = span.start.0;
                let e = span.end.0;
                let (s, e) = if e <= hint.start {
                    (s, e)
                } else if s >= hint.old_end {
                    ((s as isize + delta) as usize, (e as isize + delta) as usize)
                } else {
                    return None;
                };
                Some(Span::new(TextByte(s), TextByte(e)))
            })
            .filter(|span| !in_affected(span.start.0, span.end.0))
            .collect();
        let mut block_ids: Vec<Span<TextByte>> = prev
            .block_ids
            .into_iter()
            .filter_map(|span| {
                let s = span.start.0;
                let e = span.end.0;
                let (s, e) = if e <= hint.start {
                    (s, e)
                } else if s >= hint.old_end {
                    ((s as isize + delta) as usize, (e as isize + delta) as usize)
                } else {
                    return None;
                };
                Some(Span::new(TextByte(s), TextByte(e)))
            })
            .filter(|span| !in_affected(span.start.0, span.end.0))
            .collect();

        // Full rescan of every affected window.
        for (wi, window) in windows.iter().enumerate() {
            if !affected[wi] {
                continue;
            }
            for range in mask.unmasked(window) {
                for span in scan_inline_text(text, range) {
                    push_scanned_span(span, &mut links, &mut tags, &mut block_ids);
                }
            }
        }

        links.sort_by_key(|link| (link.span.start.0, link.span.end.0));
        tags.sort_unstable_by_key(|span| (span.start.0, span.end.0));
        block_ids.sort_unstable_by_key(|span| (span.start.0, span.end.0));

        CachedSpans {
            links,
            tags,
            block_ids,
        }
    }
}

/// Full scan over every prose window (first sync, reload, or huge damage).
fn full_scan(text: &str, mask: &TextMask, windows: &[Range<usize>]) -> CachedSpans {
    let mut spans = CachedSpans {
        links: Vec::new(),
        tags: Vec::new(),
        block_ids: Vec::new(),
    };
    for window in windows {
        for range in mask.unmasked(window) {
            for span in scan_inline_text(text, range) {
                push_scanned_span(
                    span,
                    &mut spans.links,
                    &mut spans.tags,
                    &mut spans.block_ids,
                );
            }
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::TextSnapshot;
    use crate::ts_ranges::parse_note_ts;

    fn line_starts(text: &str) -> Vec<usize> {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
        starts
    }

    fn full_spans(text: &str) -> StructureSpans {
        let parsed = parse_note_ts(text.as_bytes()).expect("parse");
        StructureSpans {
            links: parsed.links,
            tags: parsed.tags,
            block_ids: parsed.block_ids,
            mask: parsed.mask,
        }
    }

    fn assert_eq_spans(a: &StructureSpans, b: &StructureSpans, ctx: &str) {
        assert_eq!(a.links, b.links, "links: {ctx}");
        assert_eq!(a.tags, b.tags, "tags: {ctx}");
        assert_eq!(a.block_ids, b.block_ids, "block_ids: {ctx}");
    }

    #[test]
    fn first_sync_is_full_and_second_is_incremental_equivalent() {
        let text = "# H [[link]] #tag ^block\n\nBody with `[[code]]` and ![[embed.png]].\n";
        let lines = line_starts(text);
        let mut engine = StructureEngine::new();

        let first = engine.sync(text, &lines, None).expect("first sync");
        assert_eq_spans(&first, &full_spans(text), "first sync full");

        // One insert inside the body paragraph.
        let at = text.find("Body with").expect("anchor");
        let mut edited = text.to_string();
        edited.insert_str(at, "[[inserted]] ");
        let e_lines = line_starts(&edited);
        let hint = EditRange {
            start: at,
            old_end: at,
            new_end: at + "[[inserted]] ".len(),
        };
        let second = engine
            .sync(&edited, &e_lines, Some(hint))
            .expect("incremental");
        assert_eq_spans(&second, &full_spans(&edited), "incremental eq full");

        // A delete spanning a link.
        let at2 = edited.find("[[inserted]]").expect("anchor2");
        let mut edited2 = edited.clone();
        edited2.replace_range(at2..at2 + "[[inserted]] ".len(), "");
        let e2_lines = line_starts(&edited2);
        let hint2 = EditRange {
            start: at2,
            old_end: at2 + "[[inserted]] ".len(),
            new_end: at2,
        };
        let third = engine
            .sync(&edited2, &e2_lines, Some(hint2))
            .expect("incremental2");
        assert_eq_spans(&third, &full_spans(&edited2), "delete eq full");
    }

    #[test]
    fn invalid_hints_fall_back_to_full_and_degrade_after_three_rebuilds() {
        let text = "# H [[link]]\n\nParagraph.\n";
        let lines = line_starts(text);
        let mut engine = StructureEngine::new();
        let _ = engine.sync(text, &lines, None).expect("seed");

        // Bogus hint: out of bounds → full rebuild each time.
        for i in 0..4 {
            let mut t = text.to_string();
            t.push_str(&format!(" edit {i}"));
            let l = line_starts(&t);
            let bad = EditRange {
                start: t.len() + 10,
                old_end: t.len() + 20,
                new_end: t.len() + 30,
            };
            let spans = engine.sync(&t, &l, Some(bad)).expect("sync");
            if i < 3 {
                // Before degradation, the full-rebuild fallback still returns
                // correct spans.
                assert_eq_spans(&spans, &full_spans(&t), "rebuild correct");
            } else {
                // After 3 consecutive rebuilds the engine degrades to plain.
                assert!(engine.is_plain());
                assert!(spans.links.is_empty() && spans.tags.is_empty());
            }
        }

        // Degradation is sticky for the session.
        let spans = engine
            .sync(
                text,
                &lines,
                Some(EditRange {
                    start: 0,
                    old_end: 0,
                    new_end: 0,
                }),
            )
            .expect("sync after degrade");
        assert!(spans.links.is_empty());
    }

    #[test]
    fn empty_and_huge_texts_produce_empty_spans() {
        let mut engine = StructureEngine::new();
        let spans = engine.sync("", &[0], None).expect("empty sync");
        assert!(spans.links.is_empty());
        assert!(spans.tags.is_empty());
        assert_eq!(spans.mask.len(), 0);
        // NUL is rejected before FFI.
        assert!(engine.sync("a\0b", &[0], None).is_err());
    }

    #[test]
    fn snapshot_coordinates_are_unaffected_by_engine_sync() {
        // The engine never builds snapshots; parse_note_ts still does.
        let _snap = TextSnapshot::new(b"# H\n").unwrap();
        let _ = full_spans("# H [[l]]\n");
    }
}
