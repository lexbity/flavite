//! Pure selection/anchor position transforms (RFC-0003 §8.2, W03).
//!
//! These helpers map byte positions through known edit batches without any
//! I/O or model access. They implement the affinity contract for display
//! anchors: left affinity for selection starts, right affinity for ends,
//! right affinity for an insertion at a collapsed caret, and collapse to the
//! end of the replacement for positions intersecting a replacement. Mapping
//! never silently reauthorizes a writing target; callers decide validity.

use unicode_segmentation::UnicodeSegmentation;

/// Boundary affinity for a mapped position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    /// Stay before an insertion at the same offset.
    Left,
    /// Follow an insertion at the same offset.
    Right,
}

/// One replacement expressed in the coordinates of the text it applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextEdit {
    /// Start of the replaced range (byte offset, char boundary).
    pub start: usize,
    /// End of the replaced range (byte offset, char boundary).
    pub end: usize,
    /// Byte length of the replacement text.
    pub new_len: usize,
}

impl TextEdit {
    /// Build an edit from a replaced range and replacement length.
    #[must_use]
    pub fn new(start: usize, end: usize, new_len: usize) -> Self {
        Self {
            start,
            end: end.max(start),
            new_len,
        }
    }

    /// The signed length delta this edit applies to later offsets.
    #[must_use]
    pub fn delta(&self) -> isize {
        self.new_len as isize - (self.end - self.start) as isize
    }
}

/// Map one byte position through a single replacement.
///
/// - Positions at or before `edit.start` are unchanged under `Left` affinity;
///   under `Right` affinity a position exactly at `edit.start` follows the
///   inserted replacement (insertion at a collapsed caret).
/// - Positions strictly inside the replaced range collapse to the end of the
///   replacement (`edit.start + edit.new_len`).
/// - Positions at or after `edit.end` shift by the replacement delta; a
///   position exactly at `edit.end` keeps its right-side attachment.
#[must_use]
pub fn map_through_replacement(position: usize, edit: TextEdit, affinity: Affinity) -> usize {
    let delta = edit.delta();
    if position < edit.start {
        return position;
    }
    if position == edit.start {
        return match affinity {
            Affinity::Left => position,
            // Right affinity follows the replacement: the position lands at
            // the end of the inserted text (identical for pure insertions).
            Affinity::Right => edit.start + edit.new_len,
        };
    }
    if position < edit.end {
        // Intersecting replacement collapses to the end of the replacement.
        return edit.start + edit.new_len;
    }
    shift(position, delta)
}

/// Map a byte position through an ascending, non-overlapping edit batch.
///
/// Edits are expressed in the coordinates of the original text; each later
/// edit is shifted by the accumulated deltas of the earlier ones. Positions
/// are therefore transformed exactly once per intersecting edit.
#[must_use]
pub fn map_through_edits(position: usize, edits: &[TextEdit], affinity: Affinity) -> usize {
    let mut mapped = position;
    let mut shift = 0_isize;
    let mut previous_end = 0_usize;
    for edit in edits {
        debug_assert!(
            edit.start >= previous_end,
            "edit batch must be ascending and non-overlapping"
        );
        previous_end = edit.end;
        // Re-express this edit relative to the text after prior edits.
        let local = TextEdit::new(
            shift_pos(edit.start, shift),
            shift_pos(edit.end, shift),
            edit.new_len,
        );
        mapped = map_through_replacement(mapped, local, affinity);
        shift += edit.delta();
    }
    mapped
}

/// Map a selection (anchor, head) through an edit batch: the anchor uses
/// left affinity and the head uses right affinity, so a collapsed caret at
/// an insertion point follows the inserted text (§8.2).
#[must_use]
pub fn map_selection_through_edits(
    anchor: usize,
    head: usize,
    edits: &[TextEdit],
) -> (usize, usize) {
    (
        map_through_edits(anchor, edits, Affinity::Left),
        map_through_edits(head, edits, Affinity::Right),
    )
}

/// Map a position from `old` text coordinates into `new` text coordinates
/// through their common prefix/suffix edges (used by snapshot replacement).
///
/// Positions inside the changed middle keep their offset into the middle,
/// clamped to its new length; positions in the shared suffix keep their
/// distance from the end of the text. This mirrors the historical snapshot
/// replacement behavior; edit-batch affinity lives in
/// [`map_through_edits`].
#[must_use]
pub fn map_through_snapshot(
    position: usize,
    prefix: usize,
    old_middle_end: usize,
    new_middle_end: usize,
    old_len: usize,
    new_len: usize,
) -> usize {
    let mapped = if position <= prefix {
        position
    } else if position >= old_middle_end {
        new_len.saturating_sub(old_len.saturating_sub(position))
    } else {
        prefix
            + position
                .saturating_sub(prefix)
                .min(new_middle_end.saturating_sub(prefix))
    };
    mapped.min(new_len)
}

/// Clamp to the text length and round down to the nearest char boundary.
#[must_use]
pub fn snap_to_char_boundary(text: &str, position: usize) -> usize {
    let mut mapped = position.min(text.len());
    while !text.is_char_boundary(mapped) {
        mapped = mapped.saturating_sub(1);
    }
    mapped
}

/// Clamp to the text length and round down to the start of the grapheme
/// cluster containing the position, so an anchor never splits a cluster.
#[must_use]
pub fn snap_to_grapheme(text: &str, position: usize) -> usize {
    let clamped = position.min(text.len());
    if clamped == text.len() {
        return text.len();
    }
    let mut start = 0;
    for (offset, _) in text.grapheme_indices(true) {
        if offset > clamped {
            break;
        }
        start = offset;
    }
    start
}

fn shift(position: usize, delta: isize) -> usize {
    shift_pos(position, delta)
}

fn shift_pos(position: usize, delta: isize) -> usize {
    if delta >= 0 {
        position.saturating_add(delta as usize)
    } else {
        position.saturating_sub((-delta) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(start: usize, end: usize, replacement: &str) -> TextEdit {
        TextEdit::new(start, end, replacement.len())
    }

    #[test]
    fn insertion_at_caret_follows_with_right_affinity() {
        // "abc" -> "abXc": insert X at offset 2.
        let insertion = edit(2, 2, "X");
        assert_eq!(map_through_replacement(2, insertion, Affinity::Left), 2);
        assert_eq!(map_through_replacement(2, insertion, Affinity::Right), 3);
        // 'c' at 3 shifts past the inserted byte.
        assert_eq!(map_through_replacement(3, insertion, Affinity::Left), 4);
    }

    #[test]
    fn selection_start_uses_left_and_end_uses_right_affinity() {
        // Replace "b" (1..2) with "YY" in "abc".
        let replacement = edit(1, 2, "YY");
        assert_eq!(map_through_replacement(1, replacement, Affinity::Left), 1);
        assert_eq!(map_through_replacement(2, replacement, Affinity::Right), 3);
    }

    #[test]
    fn intersecting_position_collapses_to_replacement_end() {
        let replacement = edit(1, 4, "Z");
        assert_eq!(map_through_replacement(2, replacement, Affinity::Left), 2);
        assert_eq!(map_through_replacement(3, replacement, Affinity::Right), 2);
    }

    #[test]
    fn deletion_keeps_suffix_distance() {
        // "abcd" -> "ad": delete 1..3.
        let deletion = edit(1, 3, "");
        assert_eq!(map_through_replacement(2, deletion, Affinity::Left), 1);
        assert_eq!(map_through_replacement(3, deletion, Affinity::Right), 1);
        assert_eq!(map_through_replacement(4, deletion, Affinity::Left), 2);
    }

    #[test]
    fn batch_maps_once_and_shifts_later_edits() {
        // "abcdef" -> "aBCDEF": replace b (1..2) with "BC" then f (5..6) with "EF".
        let edits = [edit(1, 2, "BC"), edit(5, 6, "EF")];
        // A caret after both edits (offset 6) shifts by both deltas.
        assert_eq!(map_through_edits(6, &edits, Affinity::Left), 8);
        // A caret between the edits (offset 3) shifts only by the first delta.
        assert_eq!(map_through_edits(3, &edits, Affinity::Left), 4);
        // A caret at the first insertion point with right affinity follows it.
        assert_eq!(map_through_edits(1, &edits, Affinity::Right), 3);
    }

    #[test]
    fn selection_mapping_uses_split_affinities() {
        let edits = [edit(2, 2, "XY")];
        let (anchor, head) = map_selection_through_edits(2, 2, &edits);
        assert_eq!(anchor, 2);
        assert_eq!(head, 4);
    }

    #[test]
    fn snapshot_mapping_matches_edge_semantics() {
        // "hello world" -> "hello brave world": middle "brave " inserted at 6.
        // Interior offsets keep their offset into the middle, clamped.
        assert_eq!(map_through_snapshot(8, 6, 11, 17, 11, 17), 8);
        assert_eq!(map_through_snapshot(10, 6, 11, 17, 11, 17), 10);
        // Shared-suffix positions keep their distance from the end.
        assert_eq!(map_through_snapshot(11, 6, 11, 17, 11, 17), 17);
        assert_eq!(map_through_snapshot(9, 6, 11, 17, 11, 17), 9);
        // The prefix boundary is unchanged.
        assert_eq!(map_through_snapshot(6, 6, 11, 17, 11, 17), 6);
    }

    #[test]
    fn snapping_never_splits_characters_or_graphemes() {
        // "é" is two bytes but one char; "aé" prefix at 2 splits the char.
        let text = "aéz";
        assert_eq!(snap_to_char_boundary(text, 2), 1);
        assert_eq!(snap_to_char_boundary(text, 4), 4);
        // The flag emoji is a multi-codepoint grapheme cluster.
        let flags = "🇦🇧z";
        assert_eq!(snap_to_grapheme(flags, 4), 0);
        assert_eq!(snap_to_grapheme(flags, 8), 8);
        assert_eq!(snap_to_grapheme(flags, 100), 9);
    }

    #[test]
    fn positions_survive_random_unicode_edit_batches() {
        // Deterministic LCG: repeated runs always cover the same corpus.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let units = ["a", "é", "漢", "🇦🇧", "\r\n", "\n", "b"];
        for _ in 0..2_000 {
            let old: String = (0..(next() % 12 + 1))
                .map(|_| units[(next() as usize) % units.len()])
                .collect();
            let edits: Vec<TextEdit> = (0..(next() % 3 + 1))
                .map(|_| {
                    let start = snap_to_char_boundary(&old, (next() as usize) % (old.len() + 1));
                    let end = snap_to_char_boundary(&old, (next() as usize) % (old.len() + 1));
                    TextEdit::new(start.min(end), end.max(start), (next() as usize) % 5)
                })
                .collect();
            let mut edits = edits;
            edits.sort_by_key(|edit| (edit.start, edit.end));
            // Drop overlaps so the ascending precondition holds.
            let mut clean: Vec<TextEdit> = Vec::new();
            for candidate in edits {
                if clean.last().is_none_or(|last| last.end <= candidate.start) {
                    clean.push(candidate);
                }
            }
            for position in 0..=old.len() {
                if !old.is_char_boundary(position) {
                    continue;
                }
                let mapped = map_through_edits(position, &clean, Affinity::Left);
                let mapped_right = map_through_edits(position, &clean, Affinity::Right);
                // Mapping is monotone in affinity and never exceeds the new length.
                assert!(mapped <= mapped_right);
                let new_len: usize = clean.iter().rev().fold(old.len(), |len, edit| {
                    len.saturating_sub(edit.end.saturating_sub(edit.start)) + edit.new_len
                });
                assert!(mapped <= new_len && mapped_right <= new_len);
            }
        }
    }
}
