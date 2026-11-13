//! Canonical selection values and algebra (FEUX-0002 E-1/E-4, §5.1–5.2).
//!
//! This is the single implementation of selection merging, boundary
//! validation, and ordered mapping in the codebase. No other crate
//! re-implements it; `flavite-workspace` stores the canonical *output* as
//! plain data and `flavite-api` is the sole translation point. No separate
//! types crate exists (E-1: the FEUX-0002 amendment dropped it).
//!
//! Semantics (FEUX-001 D-4 carried verbatim, FEUX-0002 §5.1):
//! - Directed half-open ranges: each selection keeps its `anchor` and `head`;
//!   the geometric span is `[min(anchor, head), max(anchor, head))`.
//! - Sorted, non-overlapping; adjacent nonempty ranges are kept separate.
//! - A caret inside or on the boundary of a range is absorbed into it.
//! - Stable primary identity (a `SelectionId` that survives normalization).
//! - Never empty; at most [`MAX_SELECTIONS`] members; endpoints validated
//!   against the current text (bounds + extended-grapheme boundaries).

use flavite_syntax::text::{TextByte, next_grapheme_boundary, prev_grapheme_boundary};
use std::collections::HashMap;

/// Maximum number of members in one canonical selection set (D-4).
pub const MAX_SELECTIONS: usize = 10_000;

/// A stable identity within one view's set. Deterministic under
/// normalization: a merged survivor keeps the earliest id of its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SelectionId(u32);

impl SelectionId {
    /// Create an id from a raw `u32`.
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// The raw value.
    #[must_use]
    pub const fn as_raw(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for SelectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// One directed, half-open selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    id: SelectionId,
    anchor: usize,
    head: usize,
}

impl Selection {
    /// Construct a member from its parts (crate-internal; validation happens
    /// through [`SelectionSet::normalize`]).
    #[must_use]
    pub(crate) const fn from_parts(id: SelectionId, anchor: usize, head: usize) -> Self {
        Self { id, anchor, head }
    }

    /// The stable identity.
    #[must_use]
    pub const fn id(self) -> SelectionId {
        self.id
    }

    /// The anchor endpoint (where the selection started).
    #[must_use]
    pub const fn anchor(self) -> usize {
        self.anchor
    }

    /// The head endpoint (where the selection ends).
    #[must_use]
    pub const fn head(self) -> usize {
        self.head
    }

    /// Geometric start: `min(anchor, head)`.
    #[must_use]
    pub const fn start(self) -> usize {
        if self.anchor <= self.head {
            self.anchor
        } else {
            self.head
        }
    }

    /// Geometric end: `max(anchor, head)`.
    #[must_use]
    pub const fn end(self) -> usize {
        if self.anchor >= self.head {
            self.anchor
        } else {
            self.head
        }
    }

    /// Whether this is a collapsed caret.
    #[must_use]
    pub const fn is_caret(self) -> bool {
        self.anchor == self.head
    }
}

/// Untrusted input shape (API boundary). Bounds and grapheme alignment are
/// validated by [`SelectionSet::normalize`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawSelection {
    /// Optional hint id from the caller (e.g. primary identity on the wire).
    /// It is never trusted: survivors keep deterministic ids.
    pub id: Option<u32>,
    /// Anchor endpoint (byte offset).
    pub anchor: usize,
    /// Head endpoint (byte offset).
    pub head: usize,
}

/// One replacement span in pre-edit coordinates (movement deltas and
/// after-set mapping).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanEdit {
    /// Start of the replaced range (byte offset).
    pub start: usize,
    /// End of the replaced range (byte offset, exclusive).
    pub end: usize,
    /// Byte length of the replacement text.
    pub replacement_len: usize,
}

impl SpanEdit {
    /// Build from a replaced range and replacement length.
    #[must_use]
    pub const fn new(start: usize, end: usize, replacement_len: usize) -> Self {
        Self {
            start,
            end: if end < start { start } else { end },
            replacement_len,
        }
    }

    /// The signed length delta this edit applies to later offsets.
    #[must_use]
    pub const fn delta(self) -> isize {
        self.replacement_len as isize - (self.end - self.start) as isize
    }
}

/// Typed refusal of a set construction or validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetError {
    /// More candidates than [`MAX_SELECTIONS`]; the count is reported.
    TooMany { observed: usize },
    /// An endpoint is outside the current text.
    OutOfBounds { at: usize },
    /// An endpoint is not an extended-grapheme boundary.
    NotGraphemeBoundary { at: usize },
    /// No candidates were supplied; a SelectionSet is never empty.
    Empty,
    /// The primary hint named no member.
    InvalidPrimary,
}

impl std::fmt::Display for SetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany { observed } => write!(
                f,
                "Would select {observed} selections; the limit is {MAX_SELECTIONS}"
            ),
            Self::OutOfBounds { at } => write!(f, "Selection endpoint {at} is outside the text"),
            Self::NotGraphemeBoundary { at } => {
                write!(f, "Selection endpoint {at} is not a grapheme boundary")
            }
            Self::Empty => f.write_str("A selection set cannot be empty"),
            Self::InvalidPrimary => f.write_str("The primary selection hint is invalid"),
        }
    }
}

impl std::error::Error for SetError {}

/// Old-id -> survivor-id mapping produced by normalization or mapping. A
/// candidate id that merged into another member maps to that member's id;
/// surviving ids map to themselves.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdRemap {
    map: HashMap<u32, u32>,
}

impl IdRemap {
    /// Map an old id to its survivor id.
    #[must_use]
    pub fn map(&self, old: u32) -> u32 {
        self.map.get(&old).copied().unwrap_or(old)
    }

    /// Map an old id to its survivor id.
    #[must_use]
    pub fn map_id(&self, old: SelectionId) -> SelectionId {
        SelectionId::from_raw(self.map(old.as_raw()))
    }

    fn insert(&mut self, old: u32, survivor: u32) {
        self.map.insert(old, survivor);
    }
}

/// A canonical selection set: ordered (by start, then end, then id),
/// non-overlapping, non-empty, with a stable primary member. Each member
/// keeps its direction (`anchor` vs `head`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectionSet {
    ordered: Vec<Selection>,
    primary: SelectionId,
}

/// Boundary affinity for endpoint mapping (§5.2; mirrors the delivered
/// `positions::Affinity` contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// Stay before an insertion at the same offset (selection starts).
    Left,
    /// Follow an insertion at the same offset (selection ends, carets).
    Right,
}

/// A validated candidate in the sweep: id + directed endpoints.
type Candidate = (u32, usize, usize);

impl SelectionSet {
    /// Normalize untrusted candidates against `text` into one canonical set.
    ///
    /// # Errors
    ///
    /// [`SetError::TooMany`] above the cap, [`SetError::OutOfBounds`] /
    /// [`SetError::NotGraphemeBoundary`] on any endpoint,
    /// [`SetError::Empty`] with no candidates.
    pub fn normalize(
        text: &str,
        candidates: Vec<RawSelection>,
        primary_hint: Option<u32>,
    ) -> Result<(Self, IdRemap), SetError> {
        if candidates.is_empty() {
            return Err(SetError::Empty);
        }
        if candidates.len() > MAX_SELECTIONS {
            return Err(SetError::TooMany {
                observed: candidates.len(),
            });
        }
        let mut seen = Vec::with_capacity(candidates.len());
        let mut assigned_ids = std::collections::HashMap::with_capacity(candidates.len());
        let max_existing = candidates.iter().filter_map(|c| c.id).max().unwrap_or(0);
        let mut synthetic_next = max_existing.saturating_add(1);
        for candidate in candidates {
            for endpoint in [candidate.anchor, candidate.head] {
                if endpoint > text.len() {
                    return Err(SetError::OutOfBounds { at: endpoint });
                }
                if !is_grapheme_boundary(text, endpoint) {
                    return Err(SetError::NotGraphemeBoundary { at: endpoint });
                }
            }
            // Untrusted ids are hints, not authority. Duplicate ids are
            // ambiguous (identity cannot be preserved for two distinct
            // members), so later duplicates are reassigned synthetic ids;
            // the first occurrence keeps its id so a primary hint still
            // resolves.
            let id = match candidate.id {
                Some(id) if !assigned_ids.contains_key(&id) => {
                    assigned_ids.insert(id, id);
                    id
                }
                Some(_) => {
                    let id = synthetic_next;
                    synthetic_next += 1;
                    id
                }
                None => {
                    let id = synthetic_next;
                    synthetic_next += 1;
                    id
                }
            };
            seen.push((id, candidate.anchor, candidate.head));
        }
        Self::canonicalize(seen, primary_hint)
    }

    /// The pure span algebra: sort, merge, absorb, assign the primary.
    /// Inputs are already validated (bounds/grapheme) by the caller.
    fn canonicalize(
        mut candidates: Vec<Candidate>,
        primary_hint: Option<u32>,
    ) -> Result<(Self, IdRemap), SetError> {
        // Sort by geometric span, then by id (deterministic tie-break).
        candidates.sort_by_key(|&(id, anchor, head)| (anchor.min(head), anchor.max(head), id));
        let mut remap = IdRemap::default();
        let mut merged: Vec<Candidate> = Vec::with_capacity(candidates.len());
        for (id, anchor, head) in candidates {
            let from = anchor.min(head);
            let to = anchor.max(head);
            let caret = from == to;
            let mut placed = false;
            if let Some(last) = merged.last_mut() {
                let (last_id, last_anchor, last_head) = *last;
                let last_from = last_anchor.min(last_head);
                let last_to = last_anchor.max(last_head);
                let last_caret = last_from == last_to;
                let overlaps = from < last_to || (caret && from == last_to);
                if overlaps {
                    let new_from = last_from.min(from);
                    let new_to = last_to.max(to);
                    // Survivor identity: a span always outranks a caret (an
                    // absorbed caret contributes no span); two spans keep the
                    // smaller id; two equal carets keep the smaller id. The
                    // union span takes the direction of the survivor source.
                    let (survivor, src_anchor, src_head) = if !last_caret && !caret {
                        (last_id.min(id), last_anchor, last_head)
                    } else if !last_caret {
                        (last_id, last_anchor, last_head)
                    } else if !caret {
                        (id, anchor, head)
                    } else {
                        (last_id.min(id), last_anchor, last_head)
                    };
                    let (new_anchor, new_head) = if src_anchor <= src_head {
                        (new_from, new_to)
                    } else {
                        (new_to, new_from)
                    };
                    if survivor != last_id {
                        for value in remap.map.values_mut() {
                            if *value == last_id {
                                *value = survivor;
                            }
                        }
                        remap.insert(last_id, survivor);
                    }
                    if id != survivor {
                        remap.insert(id, survivor);
                    }
                    *last = (survivor, new_anchor, new_head);
                    placed = true;
                }
            }
            if !placed {
                merged.push((id, anchor, head));
            }
        }
        // Second pass: absorb carets that sit inside or on the boundary of a
        // surviving range (regardless of sweep order). Adjacent nonempty
        // ranges are untouched.
        //
        // The survivor ranges are sorted by `from` and disjoint, so their
        // `to` endpoints are non-decreasing. Each caret finds its earliest
        // containing range by two binary searches instead of a linear scan
        // (NFR-1: no O(n²) member operations).
        let ranges: Vec<Candidate> = merged
            .iter()
            .copied()
            .filter(|&(_, a, h)| a.min(h) < a.max(h))
            .collect();
        let mut absorbed = Vec::with_capacity(merged.len());
        for (id, anchor, head) in &merged {
            let caret = anchor.min(head) == anchor.max(head);
            if !caret {
                absorbed.push((*id, *anchor, *head));
                continue;
            }
            let at = *anchor;
            let host = {
                // Ranges whose `from` is at or before `at`.
                let k = ranges.partition_point(|&(_, a, h)| a.min(h) <= at);
                // The earliest of those whose `to` reaches `at`.
                let j = ranges[..k].partition_point(|&(_, a, h)| a.max(h) < at);
                (j < k).then(|| ranges[j])
            };
            if let Some((host_id, _, _)) = host {
                remap.insert(*id, host_id);
            } else {
                absorbed.push((*id, *anchor, *head));
            }
        }
        // Primary resolution: hint's survivor; else the last surviving member.
        let primary = if let Some(hint) = primary_hint {
            let survivor = remap.map(hint);
            if absorbed.iter().any(|&(id, ..)| id == survivor) {
                survivor
            } else {
                // Fall back to the last survivor (the hint named an absorbed
                // caret whose host is not the primary).
                absorbed.last().map_or(0, |&(id, ..)| id)
            }
        } else {
            absorbed.last().map_or(0, |&(id, ..)| id)
        };
        if !absorbed.iter().any(|&(id, ..)| id == primary) {
            return Err(SetError::InvalidPrimary);
        }
        let ordered = absorbed
            .into_iter()
            .map(|(id, anchor, head)| Selection {
                id: SelectionId::from_raw(id),
                anchor,
                head,
            })
            .collect();
        Ok((
            Self {
                ordered,
                primary: SelectionId::from_raw(primary),
            },
            remap,
        ))
    }

    /// Validate an existing set against `text` (bounds + grapheme + cap +
    /// canonical order).
    ///
    /// # Errors
    ///
    /// [`SetError`] on the first violation.
    pub fn validate_against(&self, text: &str) -> Result<(), SetError> {
        if self.ordered.is_empty() {
            return Err(SetError::Empty);
        }
        if self.ordered.len() > MAX_SELECTIONS {
            return Err(SetError::TooMany {
                observed: self.ordered.len(),
            });
        }
        if !self.ordered.iter().any(|s| s.id == self.primary) {
            return Err(SetError::InvalidPrimary);
        }
        let mut last_to: Option<usize> = None;
        for selection in &self.ordered {
            for endpoint in [selection.anchor, selection.head] {
                if endpoint > text.len() {
                    return Err(SetError::OutOfBounds { at: endpoint });
                }
                if !is_grapheme_boundary(text, endpoint) {
                    return Err(SetError::NotGraphemeBoundary { at: endpoint });
                }
            }
            if let Some(last_to) = last_to
                && selection.start() < last_to
                && !(selection.is_caret() && selection.start() == last_to)
            {
                return Err(SetError::InvalidPrimary);
            }
            last_to = Some(selection.end());
        }
        Ok(())
    }

    /// All members in canonical order.
    pub fn iter(&self) -> impl Iterator<Item = &Selection> {
        self.ordered.iter()
    }

    /// All members in canonical order (owned).
    #[must_use]
    pub fn members(&self) -> Vec<Selection> {
        self.ordered.clone()
    }

    /// Member count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ordered.len()
    }

    /// Whether the set has no members (always false for a canonical set —
    /// normalization refuses emptiness — present for clippy::len_without_is_empty
    /// and for callers that hold an unbuilt set).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ordered.is_empty()
    }

    /// The primary member.
    #[must_use]
    pub fn primary(&self) -> &Selection {
        self.ordered
            .iter()
            .find(|s| s.id == self.primary)
            .expect("primary is always a member")
    }

    /// The primary member's id.
    #[must_use]
    pub const fn primary_id(&self) -> SelectionId {
        self.primary
    }

    /// Map every member through an ascending, non-overlapping edit batch
    /// (pre-edit coordinates), then re-normalize. Direction is preserved per
    /// member: a forward member maps its anchor with left affinity and its
    /// head with right affinity; a reversed member swaps them; a caret uses
    /// right affinity (insertions stick right).
    ///
    /// The caller validates the result against the after-text (the planner
    /// does this); mapping itself is pure coordinate algebra.
    #[must_use]
    pub fn map_through(&self, edits: &[SpanEdit]) -> (Self, IdRemap) {
        let mut candidates = Vec::with_capacity(self.ordered.len());
        let mut hint = None;
        for selection in &self.ordered {
            let (anchor_affinity, head_affinity) = if selection.anchor < selection.head {
                (Affinity::Left, Affinity::Right)
            } else if selection.anchor > selection.head {
                (Affinity::Right, Affinity::Left)
            } else {
                (Affinity::Right, Affinity::Right)
            };
            // O(log m) per endpoint (prefix-sum binary search in
            // `map_endpoint`), so mapping n members through m edits is
            // O(n log m + m) — no per-endpoint span scan (NFR-1).
            let anchor = map_endpoint(selection.anchor, edits, anchor_affinity);
            let head = map_endpoint(selection.head, edits, head_affinity);
            if selection.id == self.primary {
                hint = Some(selection.id.as_raw());
            }
            candidates.push((selection.id.as_raw(), anchor, head));
        }
        let (set, remap) = Self::canonicalize(candidates, hint)
            .expect("mapping a valid set keeps at least one member");
        (set, remap)
    }

    /// Convert to wire/recorded triples `[id, anchor, head]`.
    #[must_use]
    pub fn to_triples(&self) -> Vec<[usize; 3]> {
        self.ordered
            .iter()
            .map(|s| [s.id.as_raw() as usize, s.anchor, s.head])
            .collect()
    }
}

/// Map one endpoint through an ordered list of movement spans (the received
/// [`crate::history::HistoryMovement`] delta), with the same sequential
/// semantics as `map_endpoint` — each span is interpreted relative to the
/// running position after earlier spans, so per-step native coordinates are
/// exactly correct without flattening (FEUX-0002 E-13).
#[must_use]
pub fn map_point_through_spans(
    mapped: usize,
    spans: &[crate::history::MovementSpan],
    affinity: crate::positions::Affinity,
) -> usize {
    let affinity = match affinity {
        crate::positions::Affinity::Left => Affinity::Left,
        crate::positions::Affinity::Right => Affinity::Right,
    };
    let edits: Vec<SpanEdit> = spans
        .iter()
        .map(|span| SpanEdit::new(span.start, span.end, span.replacement_len))
        .collect();
    map_endpoint(mapped, &edits, affinity)
}

/// Map many ascending endpoints through one ascending, non-overlapping span
/// batch in a single two-pointer pass (NFR-1: O(points + spans), no
/// per-point span scan). Each point maps with the same rule as
/// [`map_point_through_spans`]; `points` MUST be non-decreasing.
#[must_use]
pub fn map_points_through_spans(
    points: &[usize],
    spans: &[crate::history::MovementSpan],
    affinity: crate::positions::Affinity,
) -> Vec<usize> {
    let affinity = match affinity {
        crate::positions::Affinity::Left => Affinity::Left,
        crate::positions::Affinity::Right => Affinity::Right,
    };
    let mut out = Vec::with_capacity(points.len());
    let mut shift: isize = 0;
    let mut i = 0;
    for &p in points {
        // Advance every span lying entirely before `p`; accumulate deltas.
        // A point exactly at an EMPTY span's boundary (start == end == p)
        // is not consumed — it is the `== start` case (stick right / stay
        // before), matching `map_endpoint`'s branch order.
        while i < spans.len() {
            let start = spans[i].start;
            let end = shift_pos(spans[i].end, shift);
            if p < end || (p == end && start == spans[i].end) {
                break;
            }
            shift += spans[i].replacement_len as isize - (spans[i].end - spans[i].start) as isize;
            i += 1;
        }
        let mapped = if i == spans.len() {
            shift_pos(p, shift)
        } else {
            let start = shift_pos(spans[i].start, shift);
            let end = shift_pos(spans[i].end, shift);
            if p == start {
                if affinity == Affinity::Right {
                    start + spans[i].replacement_len
                } else {
                    shift_pos(p, shift)
                }
            } else if p < end {
                if affinity == Affinity::Right {
                    start + spans[i].replacement_len
                } else {
                    start
                }
            } else {
                shift_pos(p, shift)
            }
        };
        out.push(mapped);
    }
    out
}

/// Map one endpoint through an ascending, non-overlapping edit batch
/// (§5.2). Positions strictly inside a replacement collapse to its start
/// (left affinity) or its end (right affinity); a position exactly at an
/// edit's start sticks right under right affinity; under left affinity it
/// stays before the insertion. Positions at or after an edit's end shift by
/// its delta.
///
/// Sequential per endpoint (the batch semantics with affinity continuation
/// are not reducible to a simple prefix-sum binary search in the presence
/// of deletions); batch callers that need O(n + m) whole-set mapping use
/// [`map_points_through_spans`] or the planner's bulk collapse pass.
#[must_use]
fn map_endpoint(mut mapped: usize, edits: &[SpanEdit], affinity: Affinity) -> usize {
    let mut shift: isize = 0;
    for edit in edits {
        let start = shift_pos(edit.start, shift);
        let end = shift_pos(edit.end, shift);
        if mapped < start {
            continue;
        }
        if mapped == start {
            if affinity == Affinity::Right {
                return start + edit.replacement_len;
            }
            // Left affinity stays before the insertion; later edits still
            // apply relative to this offset.
        } else if mapped < end {
            return if affinity == Affinity::Right {
                start + edit.replacement_len
            } else {
                start
            };
        } else {
            mapped = shift_pos(mapped, edit.delta());
        }
        shift += edit.delta();
    }
    mapped
}

fn shift_pos(position: usize, delta: isize) -> usize {
    if delta >= 0 {
        position.saturating_add(delta as usize)
    } else {
        position.saturating_sub((-delta) as usize)
    }
}

/// Whether `at` is an extended-grapheme boundary in `text`.
///
/// Uses the pinned grapheme cursor's local boundary check (constant per
/// call) rather than a whole-string scan: normalization validates every
/// member endpoint, so an O(document) scan per endpoint would make a
/// 10,000-member normalize quadratic in document length (NFR-1).
#[must_use]
pub fn is_grapheme_boundary(text: &str, at: usize) -> bool {
    if at > text.len() {
        return false;
    }
    if at == 0 || at == text.len() {
        return true;
    }
    if !text.is_char_boundary(at) {
        return false;
    }
    let mut cursor = unicode_segmentation::GraphemeCursor::new(at, text.len(), true);
    cursor
        .is_boundary(text, 0)
        .expect("complete text supplies grapheme context")
}

/// The `editor.toggle_caret` transform (FEUX-0002 §6, E-14 a11y/programmatic
/// parity with Mod-click): a caret at `at` is added, or the member that
/// contains `at` is removed. A caret at a range edge counts as inside for
/// removal (boundary carets are absorbed), the last member is never
/// removed, and normalization absorbs duplicate points.
///
/// # Errors
///
/// [`SetError`] when `at` is out of bounds, off a grapheme boundary, or the
/// result would be empty.
pub fn toggle_caret(text: &str, set: &SelectionSet, at: usize) -> Result<SelectionSet, SetError> {
    if at > text.len() {
        return Err(SetError::OutOfBounds { at });
    }
    if !is_grapheme_boundary(text, at) {
        return Err(SetError::NotGraphemeBoundary { at });
    }
    let containing = set
        .iter()
        .find(|member| member.start() <= at && at <= member.end())
        .map(|member| member.id());
    if let Some(id) = containing {
        // Removing never empties the set: a lone member is kept.
        if set.len() == 1 {
            return Ok(set.clone());
        }
        let candidates: Vec<RawSelection> = set
            .iter()
            .filter(|member| member.id() != id)
            .map(raw_of)
            .collect();
        let hint = set.primary_id();
        let (next, _) = normalize_with_hint_fallback(text, candidates, hint)?;
        return Ok(next);
    }
    add_carets(text, set, &[at])
}

/// Add one caret per point, merged into `set` through normalization (the
/// `editor.add_caret_above`/`below` pure half: the host supplies
/// layout-computed positions, this validates and merges).
///
/// # Errors
///
/// [`SetError`] on invalid points or exceeding [`MAX_SELECTIONS`] (with the
/// observed count).
pub fn add_carets(
    text: &str,
    set: &SelectionSet,
    points: &[usize],
) -> Result<SelectionSet, SetError> {
    let mut candidates: Vec<RawSelection> = set.iter().map(raw_of).collect();
    let hint = set.primary_id();
    for &at in points {
        if at > text.len() {
            return Err(SetError::OutOfBounds { at });
        }
        if !is_grapheme_boundary(text, at) {
            return Err(SetError::NotGraphemeBoundary { at });
        }
        candidates.push(RawSelection {
            id: None,
            anchor: at,
            head: at,
        });
    }
    let (next, _) = normalize_with_hint_fallback(text, candidates, hint)?;
    Ok(next)
}

/// `editor.rotate_primary` (E-9): the primary advances to the next (`true`)
/// or previous member in canonical set order, wrapping. Selection-only.
#[must_use]
pub fn rotate_primary(set: &SelectionSet, forward: bool) -> SelectionSet {
    let position = set
        .iter()
        .position(|member| member.id() == set.primary_id())
        .unwrap_or(0);
    let count = set.len();
    let next_position = if forward {
        (position + 1) % count
    } else {
        (position + count - 1) % count
    };
    let hint = set.ordered[next_position].id.as_raw();
    let candidates: Vec<RawSelection> = set.iter().map(raw_of).collect();
    let (rotated, _) = SelectionSet::canonicalize(
        candidates
            .into_iter()
            .map(|raw| (raw.id.unwrap(), raw.anchor, raw.head))
            .collect(),
        Some(hint),
    )
    .expect("rotating a valid set keeps every member");
    rotated
}

/// `editor.flip_selections` (E-9): swap anchor and head on every member,
/// direction-preserving for the set's primary identity (the id travels with
/// its member through normalization).
///
/// # Errors
///
/// [`SetError`] when the set is invalid against the text.
pub fn flip_selections(text: &str, set: &SelectionSet) -> Result<SelectionSet, SetError> {
    let candidates: Vec<RawSelection> = set
        .iter()
        .map(|member| RawSelection {
            id: Some(member.id().as_raw()),
            anchor: member.head(),
            head: member.anchor(),
        })
        .collect();
    let (flipped, _) = normalize_with_hint_fallback(text, candidates, set.primary_id())?;
    Ok(flipped)
}

/// `editor.trim_selections` (E-9): trim Unicode whitespace from each
/// range's edges; a fully-whitespace (or empty) member becomes a caret at
/// the trim position.
///
/// # Errors
///
/// [`SetError`] when the set is invalid against the text.
pub fn trim_selections(text: &str, set: &SelectionSet) -> Result<SelectionSet, SetError> {
    let mut candidates = Vec::with_capacity(set.len());
    for member in set.iter() {
        let mut start = member.start();
        let mut end = member.end();
        while start < end
            && let Some(c) = text[start..].chars().next()
            && c.is_whitespace()
        {
            start += c.len_utf8();
        }
        while end > start
            && let Some(c) = text[..end].chars().next_back()
            && c.is_whitespace()
        {
            end -= c.len_utf8();
        }
        candidates.push(RawSelection {
            id: Some(member.id().as_raw()),
            anchor: start,
            head: end,
        });
    }
    let (trimmed, _) = normalize_with_hint_fallback(text, candidates, set.primary_id())?;
    Ok(trimmed)
}

/// `editor.invert_selections` (E-9, line-bounded complement): for every
/// line intersecting the current set, the unselected spans of that line
/// (excluding its line break) become the new set. Fully-selected lines
/// contribute nothing; inter-line newlines are never selected. Refused —
/// before materializing — when the result would exceed the cap, or when no
/// unselected text remains (a set is never empty).
///
/// # Errors
///
/// [`SetError::TooMany`] with the observed span count, or
/// [`SetError::Empty`] when every intersected line is fully selected.
pub fn invert_selections(text: &str, set: &SelectionSet) -> Result<SelectionSet, SetError> {
    set.validate_against(text)?;
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut line_start = 0usize;
    while line_start <= text.len() {
        let line_end = text[line_start..]
            .find('\n')
            .map_or(text.len(), |at| line_start + at);
        // Only lines intersecting the set participate.
        let intersects = set
            .iter()
            .any(|member| member.start() <= line_end && line_start <= member.end());
        if intersects {
            let mut cursor = line_start;
            for member in set.iter() {
                // Clamp to the line; members entirely past it contribute no
                // gap beyond the line end.
                let member_start = member.start().max(line_start).min(line_end);
                let member_end = member.end().max(line_start).min(line_end);
                if member_end <= cursor {
                    continue;
                }
                if member_start > cursor {
                    spans.push((cursor, member_start));
                }
                cursor = cursor.max(member_end);
            }
            if cursor < line_end {
                spans.push((cursor, line_end));
            }
        }
        if line_end == text.len() {
            break;
        }
        line_start = line_end + 1;
    }
    if spans.len() > MAX_SELECTIONS {
        return Err(SetError::TooMany {
            observed: spans.len(),
        });
    }
    if spans.is_empty() {
        return Err(SetError::Empty);
    }
    let candidates: Vec<RawSelection> = spans
        .into_iter()
        .map(|(start, end)| RawSelection {
            id: None,
            anchor: start,
            head: end,
        })
        .collect();
    let hint = set.primary_id();
    let (inverted, _) = normalize_with_hint_fallback(text, candidates, hint)?;
    Ok(inverted)
}

/// Typed find-char refusal (FEUX-0002 X05, E-8): the whole command is a
/// no-op when any member has no match in its direction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindCharError {
    /// No match from at least one member's head.
    NotFound,
    /// The set or a produced endpoint is invalid.
    Set(SetError),
}

impl From<SetError> for FindCharError {
    fn from(error: SetError) -> Self {
        Self::Set(error)
    }
}

impl std::fmt::Display for FindCharError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "character not found"),
            Self::Set(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for FindCharError {}

/// `editor.find_char` (E-8): scan from each member's head in `direction`
/// for one occurrence of `target` (a scan for a literal `char`, never a
/// regex); the head lands on the found character's first byte. `extend`
/// keeps each anchor. The whole command refuses (`NotFound`, set
/// unchanged) when any member finds nothing.
///
/// # Errors
///
/// [`FindCharError::NotFound`] or [`FindCharError::Set`].
pub fn find_char(
    text: &str,
    set: &SelectionSet,
    forward: bool,
    target: char,
    extend: bool,
) -> Result<SelectionSet, FindCharError> {
    set.validate_against(text)?;
    let mut candidates = Vec::with_capacity(set.len());
    for member in set.iter() {
        let head = member.head();
        let found = if forward {
            // Search strictly after the grapheme at the head.
            let from =
                flavite_syntax::text::next_grapheme_boundary(text, TextByte(head.min(text.len())))
                    .0;
            text[from..].find(target).map(|at| from + at)
        } else {
            let until = head.min(text.len());
            text[..until].rfind(target)
        };
        let Some(at) = found else {
            return Err(FindCharError::NotFound);
        };
        let anchor = if extend { member.anchor() } else { at };
        candidates.push(RawSelection {
            id: Some(member.id().as_raw()),
            anchor,
            head: at,
        });
    }
    let hint = set.primary_id();
    let (found, _) = normalize_with_hint_fallback(text, candidates, hint)?;
    Ok(found)
}

/// Expand `(start, end)` outward to the containing extended-grapheme
/// clusters (FEUX-0002 X06). Match spans are scalar-aligned; selections must
/// be grapheme-aligned, so a span whose endpoint splits a cluster widens to
/// cover it whole. Endpoints already on boundaries are unchanged.
#[must_use]
pub fn grapheme_expand(text: &str, start: usize, end: usize) -> (usize, usize) {
    let start = if is_grapheme_boundary(text, start) {
        start
    } else {
        prev_grapheme_boundary(text, TextByte(start)).0
    };
    let end = if is_grapheme_boundary(text, end) {
        end
    } else {
        next_grapheme_boundary(text, TextByte(end)).0
    };
    (start, end)
}

/// Build a canonical selection set from raw document-wide match spans
/// (FEUX-0002 E-10). Spans may be unsorted or overlapping; each is expanded
/// to grapheme boundaries, and normalization merges collisions
/// deterministically. The cap is enforced with the observed count.
///
/// # Errors
///
/// [`SetError::TooMany`] when more than [`MAX_SELECTIONS`] spans are
/// supplied, [`SetError::OutOfBounds`] on any endpoint outside `text`,
/// [`SetError::Empty`] for zero spans.
pub fn selection_set_from_spans(
    text: &str,
    spans: &[(usize, usize)],
    primary_hint: Option<u32>,
) -> Result<SelectionSet, SetError> {
    if spans.len() > MAX_SELECTIONS {
        return Err(SetError::TooMany {
            observed: spans.len(),
        });
    }
    let candidates: Vec<RawSelection> = spans
        .iter()
        .enumerate()
        .map(|(index, &(start, end))| {
            if start > text.len() || end > text.len() {
                return Err(SetError::OutOfBounds {
                    at: if start > text.len() { start } else { end },
                });
            }
            let (start, end) = grapheme_expand(text, start, end);
            Ok(RawSelection {
                id: u32::try_from(index).ok(),
                anchor: start,
                head: end,
            })
        })
        .collect::<Result<Vec<_>, SetError>>()?;
    SelectionSet::normalize(text, candidates, primary_hint).map(|(set, _)| set)
}

/// The gaps *between* consecutive matches across the whole document
/// (FEUX-0002 E-10 `split_selection`): leading/trailing gaps are included
/// only when nonempty; adjacent matches produce no gap; zero-length gaps are
/// never constructed. Spans must be sorted ascending and non-overlapping
/// (the matcher's guarantee); the caller validates them.
#[must_use]
pub fn split_gaps(text: &str, spans: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut gaps = Vec::new();
    let mut cursor = 0usize;
    for &(start, end) in spans {
        debug_assert!(start >= cursor, "split_gaps requires ascending spans");
        if start > cursor {
            gaps.push((cursor, start));
        }
        cursor = end.max(cursor);
    }
    if cursor < text.len() {
        gaps.push((cursor, text.len()));
    }
    gaps
}

/// The delivered `editor.select_line` range: the line containing `at`
/// including its line break when one follows.
#[must_use]
pub fn containing_line(text: &str, at: usize) -> (usize, usize) {
    let at = at.min(text.len());
    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[at..].find('\n').map_or(text.len(), |i| at + i);
    (
        line_start,
        (line_end + usize::from(line_end < text.len())).min(text.len()),
    )
}

fn raw_of(member: &Selection) -> RawSelection {
    RawSelection {
        id: Some(member.id().as_raw()),
        anchor: member.anchor(),
        head: member.head(),
    }
}

/// Normalize keeping the current primary when its member survives
/// normalization; otherwise the canonical fallback (last survivor) applies.
fn normalize_with_hint_fallback(
    text: &str,
    candidates: Vec<RawSelection>,
    hint: SelectionId,
) -> Result<(SelectionSet, IdRemap), SetError> {
    SelectionSet::normalize(text, candidates, Some(hint.as_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(anchor: usize, head: usize) -> RawSelection {
        RawSelection {
            id: None,
            anchor,
            head,
        }
    }

    #[test]
    fn normalize_sorts_and_keeps_adjacent_nonempty_ranges_and_direction() {
        let text = "abcdefghij";
        let (set, _) = SelectionSet::normalize(
            text,
            vec![
                raw(8, 10),
                RawSelection {
                    id: Some(5),
                    anchor: 4,
                    head: 2,
                },
                raw(0, 2),
            ],
            None,
        )
        .unwrap();
        let members = set.members();
        assert_eq!(members.len(), 3);
        assert_eq!((members[0].start(), members[0].end()), (0, 2));
        // Direction preserved for the reversed member.
        assert_eq!((members[1].anchor(), members[1].head()), (4, 2));
        assert_eq!((members[2].start(), members[2].end()), (8, 10));
    }

    #[test]
    fn normalize_merges_overlaps_and_keeps_the_earliest_id() {
        let text = "abcdefghij";
        let (set, remap) = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: Some(7),
                    anchor: 2,
                    head: 6,
                },
                RawSelection {
                    id: Some(3),
                    anchor: 4,
                    head: 8,
                },
            ],
            None,
        )
        .unwrap();
        let members = set.members();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].id().as_raw(), 3, "smallest id value survives");
        assert_eq!((members[0].start(), members[0].end()), (2, 8));
        assert_eq!(remap.map(7), 3);
        assert_eq!(remap.map(3), 3);
    }

    #[test]
    fn normalize_absorbs_boundary_carets_but_keeps_adjacent_nonempty() {
        let text = "abcdefghij";
        // Caret exactly at a range boundary is absorbed.
        let (set, _) = SelectionSet::normalize(text, vec![raw(0, 5), raw(5, 5)], None).unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!((set.members()[0].start(), set.members()[0].end()), (0, 5));
        // Caret before the range is also absorbed.
        let (set, _) = SelectionSet::normalize(text, vec![raw(5, 5), raw(0, 5)], None).unwrap();
        assert_eq!(set.len(), 1);
        // A nonempty adjacent range stays separate even sharing the boundary.
        let (set, _) = SelectionSet::normalize(text, vec![raw(0, 5), raw(5, 7)], None).unwrap();
        assert_eq!(set.len(), 2);
        // Equal carets merge.
        let (set, _) = SelectionSet::normalize(text, vec![raw(3, 3), raw(3, 3)], None).unwrap();
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn normalize_rejects_too_many_out_of_bounds_and_non_boundary_endpoints() {
        let text = "aé漢";
        assert_eq!(
            SelectionSet::normalize(text, vec![] as Vec<RawSelection>, None),
            Err(SetError::Empty)
        );
        assert_eq!(
            SelectionSet::normalize(text, vec![raw(0, text.len() + 1)], None),
            Err(SetError::OutOfBounds { at: text.len() + 1 })
        );
        // Byte 2 splits the é (bytes 1..3).
        assert!(matches!(
            SelectionSet::normalize(text, vec![raw(1, 2)], None),
            Err(SetError::NotGraphemeBoundary { at: 2 })
        ));
        let many = vec![raw(0, 0); MAX_SELECTIONS + 1];
        assert!(matches!(
            SelectionSet::normalize(text, many, None),
            Err(SetError::TooMany { observed }) if observed == MAX_SELECTIONS + 1
        ));
    }

    #[test]
    fn primary_follows_the_hint_survivor_or_falls_back_to_last() {
        let text = "abcdefghij";
        let (set, _) = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: Some(1),
                    anchor: 0,
                    head: 2,
                },
                RawSelection {
                    id: Some(2),
                    anchor: 4,
                    head: 4,
                },
            ],
            Some(2),
        )
        .unwrap();
        assert_eq!(set.primary().id().as_raw(), 2);
        // No hint: last survivor.
        let (set, _) = SelectionSet::normalize(text, vec![raw(0, 2), raw(4, 4)], None).unwrap();
        assert_eq!(set.primary().start(), 4);
    }

    #[test]
    fn map_through_insertions_stick_right_and_preserve_direction() {
        // "abc" -> "aXbc": insert X at 1.
        let edits = [SpanEdit::new(1, 1, 1)];
        let (set, _) = SelectionSet::normalize("abc", vec![raw(1, 1)], None).unwrap();
        let (mapped, _) = set.map_through(&edits);
        let m = mapped.primary();
        assert_eq!(m.anchor(), 2, "caret at insertion sticks right");
        assert_eq!(m.head(), 2);

        // A forward selection spanning the insertion stays forward.
        let (set, _) = SelectionSet::normalize("abc", vec![raw(0, 2)], None).unwrap();
        let (mapped, _) = set.map_through(&edits);
        let m = mapped.primary();
        assert_eq!(m.anchor(), 0);
        assert_eq!(m.head(), 3);
    }

    #[test]
    fn map_through_deletions_collapse_interior_members() {
        // "abcdef" -> "adef": delete 1..3.
        let edits = [SpanEdit::new(1, 3, 0)];
        let (set, _) = SelectionSet::normalize("abcdef", vec![raw(2, 4)], None).unwrap();
        let (mapped, _) = set.map_through(&edits);
        let m = mapped.primary();
        assert_eq!(m.anchor(), 1);
        assert_eq!(m.head(), 2);
    }

    #[test]
    fn map_through_merges_collisions_deterministically() {
        // Two carets on either side of a deletion collapse onto the same
        // point and merge into one member.
        let edits = [SpanEdit::new(2, 4, 0)];
        let (set, _) = SelectionSet::normalize("abcdef", vec![raw(1, 1), raw(5, 5)], None).unwrap();
        let (mapped, _) = set.map_through(&edits);
        assert_eq!(mapped.len(), 2);
        let m = mapped.members();
        assert_eq!(m[0].start(), 1);
        assert_eq!(m[1].start(), 3);
    }

    #[test]
    fn duplicate_input_ids_are_reassigned_deterministically() {
        let text = "abcd";
        // Two distinct members claim id 2; the second is reassigned a
        // synthetic id, and the first keeps 2 so a primary hint resolves.
        let (set, _) = SelectionSet::normalize(
            text,
            vec![
                RawSelection {
                    id: Some(2),
                    anchor: 0,
                    head: 1,
                },
                RawSelection {
                    id: Some(2),
                    anchor: 2,
                    head: 3,
                },
            ],
            Some(2),
        )
        .unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!(set.members()[0].id().as_raw(), 2);
        assert_ne!(set.members()[1].id().as_raw(), 2);
        assert_eq!(
            set.primary().id().as_raw(),
            2,
            "hint resolves to the first claimant"
        );
    }

    #[test]
    fn validate_against_rejects_overlaps_out_of_bounds_and_bad_primary() {
        let text = "abcdef";
        let (set, _) = SelectionSet::normalize(text, vec![raw(0, 4)], None).unwrap();
        assert!(set.validate_against(text).is_ok());
        assert!(matches!(
            set.validate_against("ab"),
            Err(SetError::OutOfBounds { at: 4 })
        ));
        // A hand-built non-canonical set (overlapping members) is refused.
        let overlapping = SelectionSet {
            ordered: vec![
                Selection {
                    id: SelectionId::from_raw(1),
                    anchor: 0,
                    head: 4,
                },
                Selection {
                    id: SelectionId::from_raw(2),
                    anchor: 2,
                    head: 5,
                },
            ],
            primary: SelectionId::from_raw(1),
        };
        assert!(overlapping.validate_against(text).is_err());
        // A primary naming no member is refused.
        let bad_primary = SelectionSet {
            ordered: vec![Selection {
                id: SelectionId::from_raw(1),
                anchor: 0,
                head: 4,
            }],
            primary: SelectionId::from_raw(9),
        };
        assert!(matches!(
            bad_primary.validate_against(text),
            Err(SetError::InvalidPrimary)
        ));
    }

    // ---- FEUX-0002 X06: match-span set construction (E-10) ----

    #[test]
    fn selection_set_from_spans_expands_grapheme_boundaries_and_merges() {
        // "é" is decomposed (e + combining acute): a literal match of "e"
        // lands mid-grapheme and must widen to the whole cluster.
        // Decomposed "é" (e + combining acute U+0301, two bytes) so a
        // literal match of "e" lands mid-grapheme and must widen to the
        // whole cluster. Bytes: a 0, e 1, U+0301 2..4, space 4, b 5,
        // space 6, e 7, U+0301 8..10.
        let text = "a\u{65}\u{301} b \u{65}\u{301}";
        let (set, _) = SelectionSet::normalize(text, vec![raw(1, 4), raw(7, 10)], None).unwrap();
        assert_eq!(set.len(), 2);
        assert_eq!((set.members()[0].start(), set.members()[0].end()), (1, 4));
        assert_eq!((set.members()[1].start(), set.members()[1].end()), (7, 10));
        let from_spans = selection_set_from_spans(text, &[(1, 2), (2, 3), (7, 8)], None)
            .expect("expands and merges");
        assert_eq!(from_spans.len(), set.len());
        for (left, right) in from_spans.members().iter().zip(set.members()) {
            assert_eq!(
                (left.start(), left.end(), left.anchor(), left.head()),
                (right.start(), right.end(), right.anchor(), right.head())
            );
        }
    }

    #[test]
    fn selection_set_from_spans_refuses_cap_and_bounds() {
        let text = "abcdefghij";
        let spans: Vec<(usize, usize)> = (0..MAX_SELECTIONS + 1)
            .map(|i| (i.min(8), i.min(8) + 1))
            .collect();
        assert!(matches!(
            selection_set_from_spans(text, &spans, None),
            Err(SetError::TooMany { observed }) if observed == MAX_SELECTIONS + 1
        ));
        assert!(matches!(
            selection_set_from_spans(text, &[(0, 2), (9, 100)], None),
            Err(SetError::OutOfBounds { at: 100 })
        ));
        assert!(matches!(
            selection_set_from_spans(text, &[], None),
            Err(SetError::Empty)
        ));
    }

    #[test]
    fn split_gaps_never_builds_zero_length_gaps() {
        let text = "xxaxxbxx";
        // Matches [0,2] and [3,5]: gaps are (2,3) and (5,8).
        assert_eq!(split_gaps(text, &[(0, 2), (3, 5)]), vec![(2, 3), (5, 8)]);
        // Adjacent matches produce no gap between them.
        assert_eq!(split_gaps(text, &[(0, 2), (2, 3), (3, 5)]), vec![(5, 8)]);
        // Leading/trailing gaps included only when nonempty.
        assert_eq!(split_gaps(text, &[(0, 2)]), vec![(2, 8)]);
        assert_eq!(split_gaps(text, &[(2, 8)]), vec![(0, 2)]);
        // A match covering the whole document yields no gaps (NoGaps).
        assert!(split_gaps(text, &[(0, 8)]).is_empty());
        // Zero-width matches at every scalar: character gaps, never empty.
        let spans: Vec<(usize, usize)> = (0..=text.len()).map(|i| (i, i)).collect();
        let gaps = split_gaps(text, &spans);
        assert_eq!(gaps.len(), text.len());
        assert!(gaps.iter().all(|&(start, end)| start < end));
    }
}
