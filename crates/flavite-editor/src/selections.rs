//! The editor widget's live selection state (FEUX-0002 X04, E-4).
//!
//! One canonical [`SelectionSet`] plus transient per-member geometry. The
//! algebra is `flavite-buffer::selection` — this module only adapts it to
//! the widget: clamping raw offsets to grapheme boundaries before
//! normalization, holding preferred-x for vertical motion, and exposing the
//! primary member for the single-selection flows that predate multi-member
//! support (delivered behavior is the N=1 case of every accessor).

use crate::viewport::{clamp_char_boundary, clamp_grapheme_boundary};
use flavite_buffer::selection::{RawSelection, Selection, SelectionSet};

/// The live selection state of one editor widget. Never empty; members are
/// canonical (sorted, non-overlapping, grapheme-aligned).
#[derive(Debug, Clone)]
pub struct EditorSelections {
    set: SelectionSet,
    /// Preferred visual x per member (parallel to `set.members()`), used by
    /// vertical motion; transient geometry, never canonical.
    preferred_x: Vec<Option<f32>>,
}

impl Default for EditorSelections {
    fn default() -> Self {
        Self::caret_at_origin()
    }
}

impl EditorSelections {
    /// A single caret at byte 0 (fresh state; resync against real text).
    #[must_use]
    pub fn caret_at_origin() -> Self {
        Self {
            set: SelectionSet::normalize(
                "",
                vec![RawSelection {
                    id: None,
                    anchor: 0,
                    head: 0,
                }],
                None,
            )
            .expect("empty text accepts caret 0")
            .0,
            preferred_x: vec![None],
        }
    }

    /// Replace the whole state with clamped members against `text`.
    /// `primary` selects which member (by input order) anchors the primary
    /// identity; out-of-range indices keep the last survivor.
    pub fn replace_with(&mut self, text: &str, members: &[(usize, usize)], primary: usize) {
        let raw: Vec<RawSelection> = members
            .iter()
            .enumerate()
            .map(|(id, &(anchor, head))| RawSelection {
                id: Some(id as u32),
                anchor: clamp_grapheme_boundary(text, clamp_char_boundary(text, anchor)),
                head: clamp_grapheme_boundary(text, clamp_char_boundary(text, head)),
            })
            .collect();
        let primary_id = members.get(primary).map_or(0, |_| primary as u32);
        let (set, _) = SelectionSet::normalize(text, raw, Some(primary_id))
            .expect("clamped members always normalize");
        let count = set.len();
        self.set = set;
        self.preferred_x = vec![None; count];
    }

    /// Place members at offsets that may exceed the current view text: the
    /// delivered optimistic pre-receipt caret (a commit or keystroke lands
    /// before the acknowledgment refreshes the view). Offsets are normalized
    /// against a synthetic all-boundary filler; the next reconcile re-clamps
    /// against the real text.
    pub fn replace_beyond_text(&mut self, members: &[(usize, usize)], primary: usize) {
        let max = members
            .iter()
            .map(|&(anchor, head)| anchor.max(head))
            .max()
            .unwrap_or(0);
        let filler = " ".repeat(max);
        self.replace_with(&filler, members, primary);
    }

    /// Adopt a canonical after-set from a kernel receipt (already validated
    /// against the after-text by the planner; re-clamped defensively).
    pub fn install_set(&mut self, set: SelectionSet) {
        let count = set.len();
        self.set = set;
        self.preferred_x = vec![None; count];
    }

    #[must_use]
    pub fn set(&self) -> &SelectionSet {
        &self.set
    }

    #[must_use]
    pub fn members(&self) -> &[Selection] {
        self.set.members()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.set.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The primary member.
    #[must_use]
    pub fn primary(&self) -> &Selection {
        self.set.primary()
    }

    /// The primary member's stable id.
    #[must_use]
    pub fn primary_id(&self) -> flavite_buffer::selection::SelectionId {
        self.set.primary_id()
    }

    /// The primary member's head (the delivered `caret`).
    #[must_use]
    pub fn caret(&self) -> usize {
        self.set.primary().head()
    }

    /// The primary member's anchor.
    #[must_use]
    pub fn anchor(&self) -> usize {
        self.set.primary().anchor()
    }

    /// Whether exactly one caret member exists (the delivered default).
    #[must_use]
    pub fn is_single_caret(&self) -> bool {
        self.set.len() == 1 && self.set.primary().is_caret()
    }

    #[must_use]
    pub fn preferred_x(&self, index: usize) -> Option<f32> {
        self.preferred_x.get(index).copied().flatten()
    }

    /// Every member's sticky column, parallel to `members()`.
    #[must_use]
    pub fn preferred_x_all(&self) -> Vec<Option<f32>> {
        self.preferred_x.clone()
    }

    /// Every member as `(anchor, head)` pairs (the host translation shape).
    #[must_use]
    pub fn pairs(&self) -> Vec<(usize, usize)> {
        self.set
            .members()
            .iter()
            .map(|member| (member.anchor(), member.head()))
            .collect()
    }

    pub fn set_preferred_x(&mut self, index: usize, x: Option<f32>) {
        if let Some(slot) = self.preferred_x.get_mut(index) {
            *slot = x;
        }
    }

    /// Clear every member's preferred x (horizontal motions invalidate it).
    pub fn clear_preferred_x(&mut self) {
        self.preferred_x.fill(None);
    }

    /// The Mod-click/Mod-drag toggle (FEUX-0002 E-14): if the press lands
    /// inside a member (boundary carets count as inside), that member is
    /// removed — never the last one; otherwise `range` (a caret for a plain
    /// Mod click, a word/line for a Mod double/triple click) is added.
    /// Returns whether a member was added (a Mod drag then extends it).
    pub fn toggle_member(&mut self, text: &str, byte: usize, range: (usize, usize)) -> bool {
        let containing = self
            .set
            .iter()
            .find(|member| member.start() <= byte && byte <= member.end())
            .map(|member| member.id());
        if let Some(id) = containing {
            if self.set.len() > 1 {
                let candidates: Vec<RawSelection> = self
                    .set
                    .iter()
                    .filter(|member| member.id() != id)
                    .map(|member| RawSelection {
                        id: Some(member.id().as_raw()),
                        anchor: member.anchor(),
                        head: member.head(),
                    })
                    .collect();
                let hint = self.set.primary_id();
                if let Ok((kept, _)) =
                    SelectionSet::normalize(text, candidates, Some(hint.as_raw()))
                {
                    let count = kept.len();
                    self.set = kept;
                    self.preferred_x = vec![None; count];
                }
            }
            return false;
        }
        let mut candidates: Vec<RawSelection> = self
            .set
            .iter()
            .map(|member| RawSelection {
                id: Some(member.id().as_raw()),
                anchor: member.anchor(),
                head: member.head(),
            })
            .collect();
        candidates.push(RawSelection {
            id: None,
            anchor: range.0,
            head: range.1,
        });
        let hint = self.set.primary_id();
        if let Ok((next, _)) = SelectionSet::normalize(text, candidates, Some(hint.as_raw())) {
            let count = next.len();
            self.set = next;
            self.preferred_x = vec![None; count];
            return true;
        }
        false
    }

    /// Replace the member whose range contains `at` (the Mod-drag origin)
    /// with the dragged range; every other member is kept.
    pub fn set_member_at_point(&mut self, text: &str, at: usize, member: (usize, usize)) {
        let mut candidates: Vec<RawSelection> = self
            .set
            .iter()
            .filter(|m| !(m.start() <= at && at <= m.end()))
            .map(|m| RawSelection {
                id: Some(m.id().as_raw()),
                anchor: m.anchor(),
                head: m.head(),
            })
            .collect();
        candidates.push(RawSelection {
            id: None,
            anchor: member.0,
            head: member.1,
        });
        let hint = self.set.primary_id();
        if let Ok((next, _)) = SelectionSet::normalize(text, candidates, Some(hint.as_raw())) {
            let count = next.len();
            self.set = next;
            self.preferred_x = vec![None; count];
        }
    }

    /// Move the primary member's endpoints (Shift-extend/pointer extend),
    /// keeping every other member.
    pub fn move_primary_head(&mut self, text: &str, anchor: usize, head: usize) {
        let primary = self.set.primary_id();
        let candidates: Vec<RawSelection> = self
            .set
            .iter()
            .map(|member| {
                if member.id() == primary {
                    RawSelection {
                        id: Some(member.id().as_raw()),
                        anchor,
                        head,
                    }
                } else {
                    RawSelection {
                        id: Some(member.id().as_raw()),
                        anchor: member.anchor(),
                        head: member.head(),
                    }
                }
            })
            .collect();
        if let Ok((next, _)) = SelectionSet::normalize(text, candidates, Some(primary.as_raw())) {
            let count = next.len();
            self.set = next;
            self.preferred_x = vec![None; count];
        }
    }

    /// Ordinary-edit Escape collapse (FEUX-0002 E-14, D-6 carried): drop the
    /// additional members first (keep the primary); a remaining range
    /// collapses to its head. Returns whether anything changed.
    pub fn escape_collapse(&mut self, text: &str) -> bool {
        if self.set.len() > 1 {
            let primary = self.set.primary_id();
            let candidates: Vec<RawSelection> = self
                .set
                .iter()
                .filter(|member| member.id() == primary)
                .map(|member| RawSelection {
                    id: Some(member.id().as_raw()),
                    anchor: member.anchor(),
                    head: member.head(),
                })
                .collect();
            let (kept, _) = SelectionSet::normalize(text, candidates, Some(primary.as_raw()))
                .expect("keeping one member of a valid set normalizes");
            let count = kept.len();
            self.set = kept;
            self.preferred_x = vec![None; count];
            return true;
        }
        let primary = self.set.primary();
        if primary.is_caret() {
            return false;
        }
        let head = primary.head();
        self.replace_with(text, &[(head, head)], 0);
        true
    }

    /// Re-clamp every member against `text` (revision replaced under the
    /// widget: offsets shrink or the text changed under the same identity).
    /// Primary identity survives.
    pub fn clamp(&mut self, text: &str) {
        let members = self.set.members();
        let members: Vec<(usize, usize)> = members
            .iter()
            .map(|member| (member.anchor(), member.head()))
            .collect();
        let primary_input = members
            .iter()
            .enumerate()
            .find(|(id, _)| {
                // The member whose current endpoints match the primary's.
                let (a, h) = members[*id];
                a == self.set.primary().anchor() && h == self.set.primary().head()
            })
            .map(|(id, _)| id)
            .unwrap_or(0);
        self.replace_with(text, &members, primary_input);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_with_clamps_and_normalizes_members() {
        let text = "a🧑🏽‍💻 bc";
        let mut selections = EditorSelections::caret_at_origin();
        // The ZWJ cluster occupies bytes 1..16; a caret inside it clamps to
        // the cluster start. The second member sits on 'b'.
        selections.replace_with(text, &[(8, 8), (17, 18)], 0);
        assert_eq!(selections.len(), 2);
        assert_eq!(
            selections.members()[0].head(),
            1,
            "clamped to the cluster boundary"
        );
        assert_eq!(selections.members()[1].start(), 17);
        assert_eq!(selections.primary().head(), 1, "primary by input index");
    }

    #[test]
    fn merging_members_keep_canonical_order_and_primary_identity() {
        let text = "one two three";
        let mut selections = EditorSelections::caret_at_origin();
        selections.replace_with(text, &[(8, 12), (0, 3)], 0);
        // Sorted canonically: "one" (0..3) before "two" (8..12). The
        // primary hint names the first input member ("two" id 0).
        assert_eq!(selections.members()[0].start(), 0);
        assert_eq!(selections.members()[1].start(), 8);
        assert_eq!(
            selections.primary().start(),
            8,
            "primary identity survives ordering"
        );
    }

    #[test]
    fn single_selection_accessors_match_the_delivered_pair() {
        let text = "hello";
        let mut selections = EditorSelections::caret_at_origin();
        selections.replace_with(text, &[(2, 4)], 0);
        assert_eq!(selections.anchor(), 2);
        assert_eq!(selections.caret(), 4);
        assert!(!selections.is_single_caret());
        selections.replace_with(text, &[(3, 3)], 0);
        assert!(selections.is_single_caret());
    }
}
