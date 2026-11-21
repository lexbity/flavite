//! Segment-scoped structure cache (RFC-0011 D-05 / D-06, slice S5).
//!
//! The document is partitioned into `Frontmatter | Fenced | Prose`
//! segments by a line-level fence/frontmatter scanner; every prose
//! segment's spans come from the shipped pulldown bridge
//! (`parse_prose_segment`) — the exact same
//! parser and converter as the whole-document bridge, so per-segment spans
//! are byte-identical to the whole-document parse by construction.
//!
//! Per keystroke, only the segments intersecting the D-06 tail are
//! re-derived and re-parsed. The tail algorithm:
//!
//! 1. `L` = line of the edit hint (`line_index_of(hint.start)`).
//! 2. `back` = start line of the segment containing `L`.
//! 3. Rescan forward from `back` (seed: the cached fence-parity state at
//!    `back`), recomputing fence parity line by line.
//! 4. The tail ends at the first cached segment boundary where the
//!    recomputed parity equals the cached seed AND the boundary line's
//!    classification matches the cached segment kind — or at document end
//!    (full suffix rebuild, never worse than the whole-document bridge).
//! 5. Invalidate and re-derive segments overlapping `[back, tail_end)`;
//!    re-parse each prose segment; splice.
//!
//! Correctness: parity is recomputed by scanning, never assumed. With an
//! accurate hint (the production contract — the session computes hints from
//! the applied edit), everything before `back` is unchanged, so the cached
//! seed is exact and the tail is typically the single edited segment. A
//! wrong hint degrades to a larger tail: hints whose containing segment
//! starts before the real edit rescan through it (correct); out-of-range
//! hints fail segment lookup and fall back to a full rebuild; after
//! repeated inconsistencies the index degrades to plain mode (empty spans,
//! one warning) exactly like RFC-0008's D10 discipline.

use crate::EditRange;
use crate::pulldown_glue::{Link, parse_prose_segment};
use crate::text::{Span, TextByte};
use crate::text_window::StructureText;

/// A segment's structural role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentKind {
    /// YAML frontmatter at the document start (excluded from scanning).
    Frontmatter,
    /// A fenced code block, closed or unclosed (excluded from scanning).
    Fenced,
    /// Ordinary markdown prose (scanned via the pulldown bridge).
    Prose,
}

/// Fence-open state at a line boundary (the parity seed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FenceState {
    Closed,
    Open(char, usize),
}

/// One document segment.
#[derive(Debug, Clone)]
pub struct Segment {
    pub kind: SegmentKind,
    /// First line (inclusive).
    pub line_start: usize,
    /// One past the last line (exclusive).
    pub line_end: usize,
    /// Byte offset of `line_start`.
    pub byte_start: usize,
    /// Byte offset of `line_end`.
    pub byte_end: usize,
    /// The fence state at `line_start` (parity seed for rescan).
    seed: FenceState,
    /// Hash of the segment's first (up to two) lines — the cut verifier:
    /// a cached suffix is trusted only when the boundary line's content
    /// still matches (catches line shifts from edits before the tail).
    header_hash: u64,
    /// Cached spans (empty for frontmatter/fenced segments).
    spans: SegmentSpans,
}

/// Cached span output for one segment, in document coordinates.
#[derive(Debug, Clone, Default)]
struct SegmentSpans {
    links: Vec<Link>,
    tags: Vec<Span<TextByte>>,
    block_ids: Vec<Span<TextByte>>,
}

/// Report of one [`SegmentIndex::sync`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncReport {
    /// Number of segments re-derived by this sync (0 when degraded).
    pub rebuilt_segments: usize,
}

/// The segment-scoped structure cache (RFC-0011 S5).
#[derive(Debug, Default)]
pub struct SegmentIndex {
    segments: Vec<Segment>,
    /// Consecutive failed syncs (fallback rebuilds or repair attempts).
    consecutive_failures: u32,
    /// Plain-for-session: spans dropped until the index is rebuilt from
    /// scratch (the D10 discipline, carried over).
    degraded: bool,
}

impl SegmentIndex {
    /// A fresh, empty index (the first sync is a full build).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sync the cache to `text` for the edit described by `hint` (None for
    /// first sync, external reloads, journal patches, and multi-op batches
    /// — all of which rebuild from scratch). Returns the sync report.
    pub fn sync(&mut self, text: &impl StructureText, hint: Option<EditRange>) -> SyncReport {
        if self.degraded {
            return SyncReport {
                rebuilt_segments: 0,
            };
        }
        if self.segments.is_empty() || hint.is_none() {
            self.build(text);
            self.consecutive_failures = 0;
            return SyncReport {
                rebuilt_segments: self.segments.len(),
            };
        }
        match self.sync_incremental(text, hint.expect("hint present")) {
            Ok(report) => {
                self.consecutive_failures = 0;
                report
            }
            Err(()) => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= 3 {
                    // D10: >3 consecutive failures → plain-for-session.
                    self.degraded = true;
                    self.segments.clear();
                    SyncReport {
                        rebuilt_segments: 0,
                    }
                } else {
                    self.build(text);
                    SyncReport {
                        rebuilt_segments: self.segments.len(),
                    }
                }
            }
        }
    }

    /// All spans in document order (links, tags, block IDs). Empty when
    /// degraded.
    #[must_use]
    pub fn spans(&self) -> (Vec<Link>, Vec<Span<TextByte>>, Vec<Span<TextByte>>) {
        let mut links = Vec::new();
        let mut tags = Vec::new();
        let mut block_ids = Vec::new();
        for segment in &self.segments {
            links.extend(segment.spans.links.iter().cloned());
            tags.extend(segment.spans.tags.iter().copied());
            block_ids.extend(segment.spans.block_ids.iter().copied());
        }
        (links, tags, block_ids)
    }

    /// Whether the index has degraded to plain mode (empty spans) for this
    /// session (D10).
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        self.degraded
    }

    /// Number of segments currently cached (0 = empty or degraded).
    #[must_use]
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    /// Debug introspection (hidden): segment kinds and line spans.
    #[doc(hidden)]
    pub fn debug_segments(&self) -> Vec<(SegmentKind, usize, usize)> {
        self.segments
            .iter()
            .map(|s| (s.kind, s.line_start, s.line_end))
            .collect()
    }

    /// Consecutive failed syncs (hidden, observability/debug).
    #[doc(hidden)]
    pub fn debug_failures(&self) -> u32 {
        self.consecutive_failures
    }

    /// Whether no segmentation exists yet (plain gate or never synced).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Diagnostic hook (hidden, test-only): corrupts the cache so the next
    /// *incremental* sync fails validation and exercises the rebuild →
    /// degrade path. Not used by production code.
    #[doc(hidden)]
    pub fn inject_inconsistent_cache(&mut self) {
        if !self.segments.is_empty() {
            self.segments.push(Segment {
                kind: SegmentKind::Prose,
                line_start: usize::MAX,
                line_end: usize::MAX,
                byte_start: usize::MAX,
                byte_end: usize::MAX,
                seed: FenceState::Closed,
                header_hash: 0,
                spans: SegmentSpans::default(),
            });
        }
    }

    fn build(&mut self, text: &impl StructureText) {
        self.segments = build_segments(text);
    }

    fn segment_at_line(&self, line: usize) -> Option<usize> {
        self.segments
            .binary_search_by(|s| {
                if line < s.line_start {
                    std::cmp::Ordering::Greater
                } else if line >= s.line_end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .ok()
    }

    fn sync_incremental(
        &mut self,
        text: &impl StructureText,
        hint: EditRange,
    ) -> Result<SyncReport, ()> {
        let line_count = text.line_count();
        let mut l = text.line_index_of(hint.start.min(text.len()));
        // Frontmatter staleness guard: the cached prefix's first segment
        // must still agree with the current frontmatter region (cheap —
        // scans only up to the closing delimiter). If it drifted, anchor the
        // rescan at the document start.
        let cur_fm_end = frontmatter_end_line(text).unwrap_or(0);
        let cached_fm_end = self
            .segments
            .first()
            .filter(|s| s.line_start == 0 && s.kind == SegmentKind::Frontmatter)
            .map(|s| s.line_end);
        let fm_stale = match (cached_fm_end, cur_fm_end) {
            (Some(k), m) => k != m,
            (None, m) => m != 0,
        };
        if fm_stale {
            l = 0;
        }
        let idx = self.segment_at_line(l).ok_or(())?;
        let back = self.segments[idx].line_start;
        let seed = self.segments[idx].seed;

        // The tail must cover the entire hinted range: a cut at or before
        // the hint's end line would keep the edited segment cached (stale).
        // `new_end`/`old_end` are the hint's post/pre-edit boundaries; the
        // max is the conservative end of the edited region.
        let hint_end_line = text.line_index_of(hint.new_end.max(hint.old_end).min(text.len()));

        let (window, cut) =
            resegment_from(&self.segments, text, back, seed, line_count, hint_end_line)?;

        let mut old = std::mem::take(&mut self.segments);
        // Build the stitched list by MOVING segments (no per-span cloning —
        // cloning every retained span per keystroke was O(total spans)).
        let window_len = window.len();
        let mut stitched: Vec<Segment> =
            Vec::with_capacity(idx + window_len + old.len().saturating_sub(idx));
        stitched.extend(old.drain(..idx));
        stitched.extend(window);
        let rebuilt = match cut {
            Some((keep, old_from)) => {
                stitched.truncate(idx + keep);
                // The cut's header-hash check guarantees the retained suffix
                // still starts on the same physical line, so its line ranges
                // stay valid; byte offsets (and the spans inside them) may
                // have shifted by edits before the tail, so translate them
                // by the running-cursor delta (drain moves the retained
                // segments — no cloning).
                let mut cursor = stitched.last().map_or(0, |s| s.byte_end);
                for mut segment in old.drain(old_from - idx..) {
                    let len = segment.byte_end - segment.byte_start;
                    let old_start = segment.byte_start;
                    segment.byte_start = cursor;
                    segment.byte_end = cursor + len;
                    let delta = cursor as isize - old_start as isize;
                    if delta != 0 {
                        shift_segment_spans(&mut segment, delta);
                    }
                    cursor = segment.byte_end;
                    stitched.push(segment);
                }
                keep
            }
            _ => window_len,
        };
        self.segments = stitched;
        if !validate_tiling(&self.segments, text) {
            return Err(());
        }
        Ok(SyncReport {
            rebuilt_segments: rebuilt,
        })
    }
}

/// The full segmentation of `text` (frontmatter + fences + prose), used on
/// first sync, hint-less syncs, and fallback rebuilds. O(n), once per root.
///
/// Classification is unified: fence state is computed over ALL lines (a
/// fence opened inside the frontmatter region can extend beyond it — the
/// bridge's exclusion is the union of the frontmatter mask and pulldown's
/// code blocks). A line is `Fenced` when fence state is open, else
/// `Frontmatter` when inside the frontmatter region, else `Prose`; runs of
/// equal class merge into segments.
fn build_segments(text: &impl StructureText) -> Vec<Segment> {
    let line_count = text.line_count();
    let fm_end = frontmatter_end_line(text).unwrap_or(0);
    let mut segments = Vec::new();
    let mut line = 0usize;
    let mut seg_start = 0usize;
    let mut state = FenceState::Closed;
    let mut para_open = false;
    let mut html = None;
    let mut seg_class = step_class(text, 0, fm_end, state, para_open, html).0;
    while line < line_count {
        let (class, next_state, next_para, next_html) =
            step_class(text, line, fm_end, state, para_open, html);
        if class != seg_class {
            flush_segment(&mut segments, text, seg_start, line, seg_class);
            seg_start = line;
            seg_class = class;
        }
        state = next_state;
        para_open = next_para;
        html = next_html;
        line += 1;
    }
    flush_segment(&mut segments, text, seg_start, line_count, seg_class);
    segments
}

/// The class of line `line`, the fence state after it, and whether a
/// paragraph is open after it (see [`build_segments`]). Opener and closer
/// lines belong to the fenced run.
///
/// Fence openers are honored unconditionally outside the frontmatter
/// region (pulldown opens fenced code blocks after paragraph lines too;
/// the corpus and goldens encode this behavior). HTML blocks are the one
/// exclusion: raw-HTML regions are classified `Fenced` and suppress fence
/// openers while active, so no prose segment is ever re-parsed across an
/// HTML boundary — per-segment bridge parses cannot carry an open
/// comment/tag across a segment cut.
fn step_class(
    text: &impl StructureText,
    line: usize,
    fm_end: usize,
    state: FenceState,
    para_open: bool,
    html: Option<HtmlBlockKind>,
) -> (SegmentKind, FenceState, bool, Option<HtmlBlockKind>) {
    let line_text = line_text(text, line);
    match state {
        FenceState::Closed => {
            // An active HTML raw region suppresses all scanning: its lines
            // are classified `Fenced` (excluded) so no prose segment is
            // ever re-parsed across an HTML boundary — per-segment bridge
            // parses cannot carry an open comment/tag across a segment cut.
            if let Some(kind) = html {
                // The terminator line belongs to the raw region too (the
                // bridge masks it in full); the state clears after it.
                return (
                    SegmentKind::Fenced,
                    FenceState::Closed,
                    false,
                    if html_block_ends(&line_text, kind) {
                        None
                    } else {
                        html
                    },
                );
            }
            // HTML block start (any region, including frontmatter): types
            // 1–5 may interrupt a paragraph; generic tags (type 6/7) may
            // not — inside a paragraph they stay paragraph text.
            if let Some(kind) = html_block_kind(&line_text) {
                let can_start = if line >= fm_end {
                    kind != HtmlBlockKind::Generic || !para_open
                } else {
                    true
                };
                if can_start {
                    // If the opener line itself terminates the block (e.g.
                    // `<? ... ?>`), the raw region ends with this line.
                    let next_html = if html_block_ends(&line_text, kind) {
                        None
                    } else {
                        Some(kind)
                    };
                    return (SegmentKind::Fenced, FenceState::Closed, false, next_html);
                }
            }
            if line >= fm_end {
                // Fence openers inside the frontmatter region are honored
                // unconditionally (existing behavior): the region is a
                // block, not a paragraph, so no gating applies.
                if let Some((ch, n)) = fence_opener(&line_text) {
                    return (SegmentKind::Fenced, FenceState::Open(ch, n), false, None);
                }
                if is_blank_line(&line_text) {
                    return (SegmentKind::Prose, FenceState::Closed, false, None);
                }
                if is_block_interrupt(&line_text) {
                    return (SegmentKind::Prose, FenceState::Closed, false, None);
                }
                return (SegmentKind::Prose, FenceState::Closed, true, None);
            }
            if let Some((ch, n)) = fence_opener(&line_text) {
                (SegmentKind::Fenced, FenceState::Open(ch, n), false, None)
            } else {
                (SegmentKind::Frontmatter, FenceState::Closed, false, None)
            }
        }
        FenceState::Open(ch, n) => {
            if fence_closer(&line_text, ch, n) {
                (SegmentKind::Fenced, FenceState::Closed, false, None)
            } else {
                (SegmentKind::Fenced, FenceState::Open(ch, n), false, None)
            }
        }
    }
}

fn flush_segment(
    segments: &mut Vec<Segment>,
    text: &impl StructureText,
    start_line: usize,
    end_line: usize,
    kind: SegmentKind,
) {
    if start_line >= end_line {
        return;
    }
    match kind {
        SegmentKind::Prose => flush_prose(segments, text, start_line, end_line),
        SegmentKind::Frontmatter | SegmentKind::Fenced => {
            let byte_start = text.line_start(start_line);
            let byte_end = if end_line < text.line_count() {
                text.line_start(end_line)
            } else {
                text.len()
            };
            segments.push(Segment {
                kind,
                line_start: start_line,
                line_end: end_line,
                byte_start,
                byte_end,
                seed: FenceState::Closed,
                header_hash: header_hash(text, start_line, end_line),
                spans: SegmentSpans::default(),
            });
        }
    }
}

/// A re-derived window plus the cut `(window_segments_to_keep,
/// cached_index_to_keep_from)` when the tail ends early.
type ResegmentOutcome = (Vec<Segment>, Option<(usize, usize)>);

/// Rescan `[back, line_count)` with the cached parity `seed`, returning the
/// re-derived window segments and — when the tail ends early — the cut
/// `(window_segments_to_keep, cached_index_to_keep_from)`.
fn resegment_from(
    old: &[Segment],
    text: &impl StructureText,
    back: usize,
    seed: FenceState,
    line_count: usize,
    hint_end_line: usize,
) -> Result<ResegmentOutcome, ()> {
    let mut window = Vec::new();
    let mut line = back;
    let mut seg_start = back;
    let mut state = seed;
    let mut cut: Option<(usize, usize)> = None;
    // Frontmatter exists only at the document start; it matters for the
    // classification only when the rescan includes line 0 (lines before
    // `back` are untouched and cached).
    let fm_end = if back == 0 {
        frontmatter_end_line(text).unwrap_or(0)
    } else {
        0
    };
    // Paragraph state at `back`: when the rescan starts mid-document, derive
    // it from the immediately preceding line (blank or a block-interrupt
    // closes the paragraph; fence-shaped lines are treated as closed, which
    // is exact for closers and conservative for gated openers — the cut
    // verifier re-checks boundary trustworthiness).
    let mut para_open = matches!(seed, FenceState::Closed) && back > 0 && {
        let prev = line_text(text, back - 1);
        !is_blank_line(&prev) && !is_block_interrupt(&prev) && !is_fence_shaped(&prev)
    };
    let mut html = None;
    let mut seg_class = step_class(text, back, fm_end, state, para_open, html).0;
    let mut broke_at_cut = false;
    while line < line_count {
        let (class, next_state, next_para, next_html) =
            step_class(text, line, fm_end, state, para_open, html);
        if class != seg_class {
            // Complete the current run first so a cut at this boundary
            // includes it in the kept window.
            flush_segment(&mut window, text, seg_start, line, seg_class);
            try_cut(
                &mut cut,
                &CutCtx {
                    old,
                    text,
                    hint_end_line,
                },
                window.len(),
                line,
                FenceState::Closed,
                window.last().map(|s| s.kind),
            )?;
            if cut.is_some() {
                // The tail ends here: the remaining suffix is cached and
                // trusted. Stop walking — the stitch reuses it. The current
                // run is already flushed; skip the trailing flush.
                broke_at_cut = true;
                break;
            }
            seg_start = line;
            seg_class = class;
        }
        state = next_state;
        para_open = next_para;
        html = next_html;
        line += 1;
    }
    if !broke_at_cut {
        flush_segment(&mut window, text, seg_start, line_count, seg_class);
    }
    Ok((window, cut))
}
/// The shared context for cut evaluation.
struct CutCtx<'a, T: StructureText> {
    old: &'a [Segment],
    text: &'a T,
    hint_end_line: usize,
}

/// Record the first tail cut at boundary `b`: the boundary is trustworthy
/// when a cached segment starts exactly at `b`, its parity seed equals the
/// recomputed state, the cached kind matches the boundary line's
/// classification, the retained suffix starts on the same content (header
/// hash), and the boundary is a real kind transition — adjacent prose
/// segments are never valid (a fresh build merges them), so keeping one
/// would split a pulldown paragraph window and diverge from the bridge.
fn try_cut<T: StructureText>(
    cut: &mut Option<(usize, usize)>,
    ctx: &CutCtx<'_, T>,
    window_len: usize,
    b: usize,
    state: FenceState,
    window_last_kind: Option<SegmentKind>,
) -> Result<(), ()> {
    if cut.is_some() {
        return Ok(());
    }
    let old = ctx.old;
    let text = ctx.text;
    let Some(o) = old
        .binary_search_by(|s| {
            if b < s.line_start {
                std::cmp::Ordering::Greater
            } else if b >= s.line_end {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .ok()
    else {
        return Ok(());
    };
    let cached = &old[o];
    if cached.line_start != b || cached.seed != state {
        return Ok(());
    }
    // The retained suffix must start after the hinted edit range: a cut at
    // or before the hint's end line would keep the edited segment cached
    // (stale).
    if cached.line_start <= ctx.hint_end_line {
        return Ok(());
    }
    // The boundary line's classification must match the cached segment kind.
    let kind_here = if b >= text.line_count() {
        None
    } else if fence_opener(&line_text(text, b)).is_some() {
        Some(SegmentKind::Fenced)
    } else {
        Some(SegmentKind::Prose)
    };
    if kind_here != Some(cached.kind) {
        return Ok(());
    }
    // Adjacent prose is a stale boundary (a fresh build merges prose runs).
    if window_last_kind == Some(SegmentKind::Prose) && cached.kind == SegmentKind::Prose {
        return Ok(());
    }
    // The retained suffix must start on the same content: edits before the
    // tail can shift line numbering so a numerically-equal boundary points
    // at different bytes. The header hash catches that.
    if cached.header_hash != header_hash(text, b, cached.line_end) {
        return Ok(());
    }
    *cut = Some((window_len, o));
    Ok(())
}

/// Hash of the segment's first (up to two) lines, in document bytes. Used
/// by the cut verifier to confirm a cached suffix still starts on the same
/// content after edits that shift line numbering.
fn header_hash(text: &impl StructureText, line_start: usize, line_end: usize) -> u64 {
    let span_end = text
        .line_start(line_start + 2)
        .min(text.line_start(line_end));
    let bytes = text.slice(text.line_start(line_start)..span_end);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    use std::hash::{Hash, Hasher};
    bytes.hash(&mut hasher);
    hasher.finish()
}

/// Push a prose segment `[start_line, end_line)`, parsed through the
/// pulldown bridge (spans offset into document coordinates).
fn flush_prose(
    segments: &mut Vec<Segment>,
    text: &impl StructureText,
    start_line: usize,
    end_line: usize,
) {
    if start_line >= end_line {
        return;
    }
    let byte_start = text.line_start(start_line);
    let byte_end = if end_line < text.line_count() {
        text.line_start(end_line)
    } else {
        text.len()
    };
    let mut spans = SegmentSpans::default();
    if byte_end > byte_start {
        let seg_text = text.slice(byte_start..byte_end);
        if let Ok((links, tags, block_ids)) = parse_prose_segment(&seg_text) {
            spans.links = links
                .into_iter()
                .map(|mut link| {
                    link.span = Span::new(
                        TextByte(link.span.start.0 + byte_start),
                        TextByte(link.span.end.0 + byte_start),
                    );
                    link
                })
                .collect();
            spans.tags = tags
                .into_iter()
                .map(|mut span| {
                    span.start.0 += byte_start;
                    span.end.0 += byte_start;
                    span
                })
                .collect();
            spans.block_ids = block_ids
                .into_iter()
                .map(|mut span| {
                    span.start.0 += byte_start;
                    span.end.0 += byte_start;
                    span
                })
                .collect();
        }
        // parse error (embedded NUL): leave the segment's spans empty,
        // mirroring the whole-document bridge's error behavior.
    }
    segments.push(Segment {
        kind: SegmentKind::Prose,
        line_start: start_line,
        line_end: end_line,
        byte_start,
        byte_end,
        seed: FenceState::Closed,
        header_hash: header_hash(text, start_line, end_line),
        spans,
    });
}

/// Mirror of [`crate::mask::find_frontmatter_close`] over [`StructureText`]:
/// line index just past the closing `---`/`...` of frontmatter opened at the
/// first line, if any. The opener must be exactly `---\n` (the bridge's
/// condition).
fn frontmatter_end_line(text: &impl StructureText) -> Option<usize> {
    let line_count = text.line_count();
    if line_count == 0 || text.line_start(1) != 4 || text.slice(0..4) != "---\n" {
        return None;
    }
    for line in 1..line_count {
        let start = text.line_start(line);
        let end = if line + 1 < line_count {
            text.line_start(line + 1)
        } else {
            text.len()
        };
        let content = text.slice(start..end);
        let trimmed = content.strip_suffix('\n').unwrap_or(&content).trim();
        if trimmed == "---" || trimmed == "..." {
            return Some(line + 1);
        }
    }
    None
}

fn line_text(text: &impl StructureText, line: usize) -> String {
    let start = text.line_start(line);
    let end = if line + 1 < text.line_count() {
        text.line_start(line + 1)
    } else {
        text.len()
    };
    text.slice(start..end)
}

/// Shift a segment's cached spans by `delta` bytes (used when retained
/// segments' byte offsets move due to edits before the tail).
fn shift_segment_spans(segment: &mut Segment, delta: isize) {
    for link in &mut segment.spans.links {
        link.span.start.0 = (link.span.start.0 as isize + delta) as usize;
        link.span.end.0 = (link.span.end.0 as isize + delta) as usize;
    }
    for span in segment
        .spans
        .tags
        .iter_mut()
        .chain(&mut segment.spans.block_ids)
    {
        span.start.0 = (span.start.0 as isize + delta) as usize;
        span.end.0 = (span.end.0 as isize + delta) as usize;
    }
}

/// Segments must tile the document exactly: contiguous line ranges, byte
/// ranges, first segment at 0, last at the document end — and never
/// contain adjacent prose segments (a fresh build merges prose runs, so
/// adjacency is by definition a stale boundary).
fn validate_tiling(segments: &[Segment], text: &impl StructureText) -> bool {
    if segments.is_empty() {
        return text.len() == 0;
    }
    let mut expected_line = 0usize;
    let mut expected_byte = 0usize;
    for segment in segments {
        if segment.line_start != expected_line || segment.byte_start != expected_byte {
            return false;
        }
        if segment.line_end < segment.line_start || segment.byte_end < segment.byte_start {
            return false;
        }
        expected_line = segment.line_end;
        expected_byte = segment.byte_end;
    }
    if expected_line != text.line_count() || expected_byte != text.len() {
        return false;
    }
    !segments
        .windows(2)
        .any(|pair| pair[0].kind == SegmentKind::Prose && pair[1].kind == SegmentKind::Prose)
}

/// Strip blockquote container prefixes (`>` with optional following space,
/// up to 3 levels) so fences inside blockquotes classify like CommonMark
/// containers. No other container forms are stripped — list-continuation
/// fences carry only indentation (≤3 spaces), which the fence rules accept.
fn leading_spaces(s: &str) -> usize {
    s.bytes().take_while(|&b| b == b' ').count()
}

/// The line content after at most 3 leading spaces (container indentation
/// is not stripped — see [`fence_opener`]).
fn strip_container_indent(line: &str) -> &str {
    &line[leading_spaces(line).min(3)..]
}

fn is_blank_line(line: &str) -> bool {
    line.trim().is_empty()
}

/// ATX heading: 1–6 `#` followed by space/tab or end of line.
fn is_atx_heading(line: &str) -> bool {
    let rest = strip_container_indent(line);
    let mut chars = rest.chars();
    let mut n = 0usize;
    for c in chars.by_ref() {
        if c == '#' && n < 6 {
            n += 1;
        } else {
            break;
        }
    }
    n >= 1 && matches!(chars.next(), None | Some(' ' | '\t'))
}

/// Setext heading underline (`=`/`-` run) or thematic break (`-` run ≥ 3).
fn is_setext_or_thematic_dash(line: &str) -> bool {
    let trimmed = strip_container_indent(line)
        .trim_end_matches(['\n', '\r'])
        .trim_end();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.chars().all(|c| c == '=') {
        return true;
    }
    trimmed.chars().all(|c| c == '-') && trimmed.len() >= 3
}

/// Thematic break of `*` or `_` (≥ 3 markers, whitespace between allowed).
fn is_thematic_star_underscore(line: &str) -> bool {
    let trimmed = strip_container_indent(line)
        .trim_end_matches(['\n', '\r'])
        .trim_end();
    let ch = match trimmed.chars().next() {
        Some(c @ ('*' | '_')) => c,
        _ => return false,
    };
    let mut count = 0usize;
    for c in trimmed.chars() {
        if c == ch {
            count += 1;
        } else if c == ' ' || c == '\t' {
            continue;
        } else {
            return false;
        }
    }
    count >= 3
}

/// List item marker: bullet (`-`/`+`/`*`) or ordered (`1.`/`1)`) followed
/// by space/tab or end of line.
fn is_list_marker_line(line: &str) -> bool {
    let mut chars = strip_container_indent(line).chars();
    match chars.next() {
        Some('-' | '+' | '*') => matches!(chars.next(), Some(' ' | '\t') | None),
        Some(d) if d.is_ascii_digit() => {
            let mut digits = 1usize;
            for c in chars.by_ref() {
                if c.is_ascii_digit() && digits < 9 {
                    digits += 1;
                } else {
                    break;
                }
            }
            matches!(chars.next(), Some('.') | Some(')'))
                && matches!(chars.next(), Some(' ' | '\t') | None)
        }
        _ => false,
    }
}

fn is_blockquote_line(line: &str) -> bool {
    matches!(strip_container_indent(line).chars().next(), Some('>'))
}

/// HTML block kind (CommonMark block types, approximated for the segment
/// scanner's purpose: knowing when a raw-HTML region ends so fence openers
/// inside it never fire).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HtmlBlockKind {
    /// Type 1: `<script`/`<pre`/`<style`/`<textarea` — ends at its closing
    /// tag, ignoring blank lines.
    ScriptClose(&'static str),
    /// Type 2: `<!--` — ends at `-->`.
    Comment,
    /// Type 3: `<?` — ends at `?>`.
    Pi,
    /// Type 4: `<!` + uppercase letter — ends at `>`.
    Declaration,
    /// Type 5: `<!CDATA[[` — ends at `]]>`.
    Cdata,
    /// Types 6/7: any other `<`-tag — ends at the first blank line.
    Generic,
}

/// The HTML block kind started at this line, if any.
fn html_block_kind(line: &str) -> Option<HtmlBlockKind> {
    let rest = strip_container_indent(line);
    let bytes = rest.as_bytes();
    if !bytes.starts_with(b"<") {
        return None;
    }
    if rest.starts_with("<!--") {
        return Some(HtmlBlockKind::Comment);
    }
    if rest.starts_with("<?") {
        return Some(HtmlBlockKind::Pi);
    }
    if rest.starts_with("<![CDATA[") {
        return Some(HtmlBlockKind::Cdata);
    }
    if rest.starts_with("<!") && bytes.get(2).is_some_and(|b| b.is_ascii_uppercase()) {
        return Some(HtmlBlockKind::Declaration);
    }
    for tag in ["script", "pre", "style", "textarea"] {
        if rest[1..].to_ascii_lowercase().starts_with(tag)
            && html_name_boundary(rest.as_bytes(), 1 + tag.len())
        {
            return Some(HtmlBlockKind::ScriptClose(tag));
        }
    }
    // Type 6: `<` followed by a known block-level tag name (commonmark.js
    // whitelist; pulldown does not open an unclosed block for inline tags
    // like `<em>`/`<span>` or unknown names). No closing `>` is required.
    for tag in HTML_BLOCK_TAGS {
        if rest[1..].starts_with(tag) && html_name_boundary(rest.as_bytes(), 1 + tag.len()) {
            return Some(HtmlBlockKind::Generic);
        }
    }
    // Type 7: a complete open or close tag — `<name …>` with a `>` on the
    // line and only whitespace after it (a line with trailing content is
    // an inline tag, masked by the bridge inside its prose segment, and is
    // not a block).
    let after_lt = if bytes.get(1) == Some(&b'/') {
        &rest[2..]
    } else {
        &rest[1..]
    };
    if is_complete_tag(after_lt) {
        // A complete open/close tag that ends at a `>` with only
        // whitespace after it is a type-7 block; trailing content makes it
        // an inline tag (masked by the bridge inside its prose segment).
        if let Some(idx) = after_lt.find('>') {
            let after = after_lt[idx + 1..].trim_end_matches(['\n', '\r']).trim();
            if after.is_empty() {
                return Some(HtmlBlockKind::Generic);
            }
        }
    }
    None
}

/// Whether `s` (the text after `<` or `</`) is a syntactically complete
/// HTML tag: a name, optional whitespace-separated attributes (possibly
/// with quoted/unquoted values), optional self-closing `/`, then `>`.
fn is_complete_tag(s: &str) -> bool {
    let b = s.as_bytes();
    if b.first().is_none_or(|c| !c.is_ascii_alphabetic()) {
        return false;
    }
    let mut i = 1usize;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'-') {
        i += 1;
    }
    loop {
        let mut saw_ws = false;
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
            i += 1;
            saw_ws = true;
        }
        if i >= b.len() {
            return false;
        }
        match b[i] {
            b'>' => return true,
            b'/' => return b.get(i + 1) == Some(&b'>'),
            _ => {
                // Attribute name: CommonMark requires whitespace before
                // each attribute (`<SSj1:>` is not a tag).
                if !saw_ws {
                    return false;
                }
                if !(b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b':') {
                    return false;
                }
                while i < b.len()
                    && (b[i].is_ascii_alphanumeric()
                        || b[i] == b'_'
                        || b[i] == b':'
                        || b[i] == b'-'
                        || b[i] == b'.')
                {
                    i += 1;
                }
                if i < b.len() && b[i] == b'=' {
                    i += 1;
                    if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                        let quote = b[i];
                        i += 1;
                        while i < b.len() && b[i] != quote {
                            i += 1;
                        }
                        if i >= b.len() {
                            return false;
                        }
                        i += 1;
                    } else {
                        let start = i;
                        while i < b.len() && !matches!(b[i], b' ' | b'\t' | b'\r' | b'\n' | b'>') {
                            i += 1;
                        }
                        if i == start {
                            return false;
                        }
                    }
                }
            }
        }
    }
}

/// Block-level tag names recognized by pulldown's type-6 HTML blocks
/// (the commonmark.js `htmlBlockTags` whitelist).
const HTML_BLOCK_TAGS: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

/// An HTML tag name ends at byte `i`: end of line or whitespace/`>` (a
/// `/` is only a valid boundary inside a complete tag, handled by
/// [`is_complete_tag`]).
fn html_name_boundary(bytes: &[u8], i: usize) -> bool {
    bytes
        .get(i)
        .is_none_or(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'>'))
}

/// Whether line `line` ends an active HTML block of `kind`.
fn html_block_ends(line: &str, kind: HtmlBlockKind) -> bool {
    match kind {
        HtmlBlockKind::Generic => is_blank_line(line),
        HtmlBlockKind::Comment => line.contains("-->"),
        HtmlBlockKind::Pi => line.contains("?>"),
        HtmlBlockKind::Declaration => line.contains('>'),
        HtmlBlockKind::Cdata => line.contains("]]>"),
        HtmlBlockKind::ScriptClose(tag) => line.contains(&format!("</{tag}")),
    }
}

/// A line that closes a paragraph: blank, or a block-level construct that
/// CommonMark allows to interrupt a paragraph (heading, thematic break,
/// list item, blockquote, HTML block). Fence openers are handled
/// separately by [`step_class`].
fn is_block_interrupt(line: &str) -> bool {
    is_atx_heading(line)
        || is_setext_or_thematic_dash(line)
        || is_thematic_star_underscore(line)
        || is_list_marker_line(line)
        || is_blockquote_line(line)
}

/// A fence-shaped line (opener or closer candidate): ≥ 3 backticks/tildes
/// at line start after ≤ 3 spaces, with only whitespace after.
fn is_fence_shaped(line: &str) -> bool {
    if fence_opener(line).is_some() {
        return true;
    }
    let indent = leading_spaces(line);
    if indent > 3 {
        return false;
    }
    let mut chars = line[indent..].chars();
    let ch = match chars.next() {
        Some(c @ ('`' | '~')) => c,
        _ => return false,
    };
    let mut n = 1usize;
    for c in chars.by_ref() {
        if c == ch {
            n += 1;
        } else {
            break;
        }
    }
    n >= 3 && chars.all(|c| c == ' ' || c == '\t' || c == '\n' || c == '\r')
}

/// A fence opener at this line: `(fence_char, length)`. Up to 3 leading
/// spaces; at least 3 backticks or tildes; anything after the run is an
/// info string (ignored) — except that a backtick fence's info string may
/// not contain backticks (CommonMark), so such a line is not an opener.
/// Container prefixes (`>` blockquotes) are deliberately NOT stripped: a
/// fence inside a blockquote is scoped to its container, and the prose
/// segment's pulldown bridge handles container-local code blocks correctly.
/// Splitting on container-level fences would require grammar-grade parsing
/// (RFC-0011 D-06/R4); the bridge already excludes those ranges, so parity
/// is preserved with coarser segments.
fn fence_opener(line: &str) -> Option<(char, usize)> {
    let indent = leading_spaces(line);
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let mut chars = rest.chars();
    let ch = chars.next()?;
    if ch != '`' && ch != '~' {
        return None;
    }
    let mut n = 1usize;
    for c in chars {
        if c == ch {
            n += 1;
        } else {
            break;
        }
    }
    if n < 3 {
        return None;
    }
    if ch == '`' && rest[n..].contains('`') {
        return None;
    }
    Some((ch, n))
}

/// A fence closer for `(ch, n)` at this line: up to 3 leading spaces, at
/// least `n` fence chars, then only whitespace (including the line
/// terminator, which is part of the line slice). Container prefixes are not
/// stripped (see [`fence_opener`]).
fn fence_closer(line: &str, ch: char, n: usize) -> bool {
    let indent = leading_spaces(line);
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let mut count = 0usize;
    for c in rest.chars() {
        if c == ch {
            count += 1;
        } else {
            break;
        }
    }
    count >= n
        && rest
            .chars()
            .skip(count)
            .all(|c| c == ' ' || c == '\n' || c == '\r')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ops::Range;

    struct TestText(String);

    impl StructureText for TestText {
        fn len(&self) -> usize {
            self.0.len()
        }
        fn line_count(&self) -> usize {
            1 + self.0.bytes().filter(|&b| b == b'\n').count()
        }
        fn line_start(&self, line: usize) -> usize {
            if line == 0 {
                return 0;
            }
            let mut current = 0usize;
            for (i, b) in self.0.bytes().enumerate() {
                if b == b'\n' {
                    current += 1;
                    if current == line {
                        return i + 1;
                    }
                }
            }
            self.0.len()
        }
        fn slice(&self, range: Range<usize>) -> String {
            self.0[range].to_owned()
        }
    }

    fn t(text: &str) -> TestText {
        TestText(text.to_owned())
    }

    type SpanSet = (
        Vec<(usize, usize, String)>,
        Vec<(usize, usize)>,
        Vec<(usize, usize)>,
    );

    fn full_spans(text: &str) -> SpanSet {
        let parsed = crate::pulldown_glue::parse_normalized(text).expect("bridge parse");
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

    fn seg_spans(index: &SegmentIndex) -> SpanSet {
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

    #[test]
    fn build_matches_the_whole_document_bridge() {
        for text in [
            "",
            "plain prose with [[a]] #tag\n",
            "---\ntitle: X\n---\n\nBody [[b]]\n",
            "before [[l1]]\n\n```rust\n[[hidden]] #h\n```\n\nafter [[l2]]\n",
            "```\nunclosed [[hidden]]\n",
            "> quote [[q]]\n\n> ```\n> [[in-quote-fence]]\n> ```\n\n> after [[q2]]\n",
            "- item [[l1]]\n  ```\n  [[hidden]]\n  ```\n- item2 [[l2]]\n",
            "~~~\n[[tilde-hidden]]\n~~~\n",
            "`code [[hidden]]` and [[visible]]\n",
        ] {
            let mut index = SegmentIndex::new();
            index.sync(&t(text), None);
            assert_eq!(seg_spans(&index), full_spans(text), "text: {text:?}");
        }
    }

    #[test]
    fn incremental_sync_matches_full_after_edits() {
        // A compact deterministic edit sequence over a fence-rich doc.
        let base = "# H [[link]]\n\n```rust\n[[fenced]] #f\n```\n\nAfter [[after]].\n";
        let mut text = base.to_string();
        let mut index = SegmentIndex::new();
        index.sync(&t(&text), None);
        assert_eq!(seg_spans(&index), full_spans(&text));
        let edits: Vec<(&str, EditRange)> = vec![
            (
                "x",
                EditRange {
                    start: 10,
                    old_end: 10,
                    new_end: 11,
                },
            ),
            (
                "\n```\n[[new-fence]]\n```\n",
                EditRange {
                    start: text.len(),
                    old_end: text.len(),
                    new_end: 0,
                },
            ),
            (
                "",
                EditRange {
                    start: 40,
                    old_end: 46,
                    new_end: 40,
                },
            ),
            (
                "[[extra]] ",
                EditRange {
                    start: 3,
                    old_end: 3,
                    new_end: 11,
                },
            ),
            (
                "\n",
                EditRange {
                    start: 0,
                    old_end: 0,
                    new_end: 1,
                },
            ),
        ];
        for (replacement, hint) in edits {
            let start = hint.start.min(text.len());
            let old_end = hint.old_end.min(text.len());
            text.replace_range(start..old_end, replacement);
            let report = index.sync(&t(&text), Some(hint));
            assert!(
                !index.is_degraded(),
                "sync must not degrade on accurate hints"
            );
            assert!(report.rebuilt_segments >= 1);
            assert_eq!(seg_spans(&index), full_spans(&text), "text: {text:?}");
        }
    }

    #[test]
    fn out_of_range_hint_repairs_via_rebuild() {
        let mut text = "prose [[a]]\n".to_string();
        let mut index = SegmentIndex::new();
        index.sync(&t(&text), None);
        text.push_str(" more [[b]]\n");
        let bad = EditRange {
            start: text.len() + 8,
            old_end: text.len() + 9,
            new_end: text.len() + 10,
        };
        let report = index.sync(&t(&text), Some(bad));
        assert!(!index.is_degraded());
        assert!(report.rebuilt_segments >= 1);
        assert_eq!(seg_spans(&index), full_spans(&text));
    }

    #[test]
    fn consecutive_failures_degrade_to_plain() {
        // Multi-segment document so an injected corrupt suffix survives the
        // incremental cut and fails tiling validation.
        let text = "prose [[a]]\n\n```\n[[hidden]]\n```\n";
        let mut index = SegmentIndex::new();
        index.sync(&t(text), None);
        assert_eq!(index.segment_count(), 3);
        let hint = EditRange {
            start: 1,
            old_end: 1,
            new_end: 2,
        };
        for _ in 0..3 {
            index.inject_inconsistent_cache();
            index.sync(&t(text), Some(hint));
        }
        assert!(index.is_degraded());
        let (links, tags, block_ids) = index.spans();
        assert!(links.is_empty() && tags.is_empty() && block_ids.is_empty());
    }

    #[test]
    fn fence_helpers() {
        assert_eq!(fence_opener("```"), Some(('`', 3)));
        assert_eq!(fence_opener("   ```rust"), Some(('`', 3)));
        assert_eq!(fence_opener("    ```"), None, "4 spaces is indented code");
        assert_eq!(fence_opener("~~~"), Some(('~', 3)));
        assert_eq!(fence_opener("``"), None, "two ticks are not a fence");
        assert_eq!(
            fence_opener("> ```"),
            None,
            "blockquote fences are container-scoped"
        );
        assert_eq!(
            fence_opener("`````[[lnk]] `"),
            None,
            "backtick in the info string disables the opener"
        );
        assert_eq!(fence_opener("`````plain"), Some(('`', 5)));
        assert!(fence_closer("```", '`', 3));
        assert!(fence_closer("   ```  ", '`', 3));
        assert!(!fence_closer("~~~~", '`', 3));
        assert!(fence_closer("~~~~", '~', 3));
        assert!(
            !fence_closer("```x", '`', 3),
            "trailing text is not a closer"
        );
    }
}
