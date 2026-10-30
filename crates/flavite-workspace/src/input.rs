//! Per-view editor interaction state (RFC-0003 D-03/D-04/D-07, §8.2).
//!
//! This module contains values only. Widget runtimes may be destroyed and
//! recreated when a tab moves; the acknowledged caret, selection, viewport,
//! and composition state are owned by the host and travel with the tab.

use crate::ids::TabId;

/// One selection in UTF-8 byte coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Selection {
    /// Anchor end of the selection.
    pub anchor: usize,
    /// Active caret end of the selection.
    pub head: usize,
}

impl Selection {
    /// Create a selection or collapsed caret.
    #[must_use]
    pub const fn new(anchor: usize, head: usize) -> Self {
        Self { anchor, head }
    }

    /// Create a collapsed caret.
    #[must_use]
    pub const fn caret(at: usize) -> Self {
        Self {
            anchor: at,
            head: at,
        }
    }

    /// Whether this is a collapsed caret.
    #[must_use]
    pub const fn is_caret(self) -> bool {
        self.anchor == self.head
    }

    /// Ordered byte range.
    #[must_use]
    pub const fn range(self) -> (usize, usize) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }
}

/// Whether an IME owns an uncommitted preedit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CompositionState {
    /// No active preedit.
    #[default]
    Idle,
    /// An uncommitted preedit is active in the widget runtime.
    Active,
}

/// Acknowledged logical editor state for one note tab.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EditorInteraction {
    /// Tab identity. This prevents a retained widget event from writing into a
    /// replacement view occupying the same toolkit slot.
    pub tab: TabId,
    /// Process-local generation of the view resource.
    pub incarnation: u64,
    /// Bound Session buffer, when the note has opened.
    pub buffer_id: Option<u64>,
    /// Revision whose byte coordinates the selections refer to.
    pub revision: u64,
    /// Monotonic interaction sequence assigned by the host.
    pub sequence: u64,
    /// Bounded selections. The host rejects excess entries before storing.
    pub selections: Vec<Selection>,
    /// First visible text byte used to restore the viewport after reparenting.
    pub viewport_anchor: usize,
    /// Composition is transient but visible to layout admission.
    pub composition: CompositionState,
    /// Whether this note editor currently owns keyboard focus.
    pub focused: bool,
}

impl EditorInteraction {
    /// Construct the initial state for a new view incarnation.
    #[must_use]
    pub fn new(tab: TabId, incarnation: u64) -> Self {
        Self {
            tab,
            incarnation,
            buffer_id: None,
            revision: 0,
            sequence: 0,
            selections: vec![Selection::caret(0)],
            viewport_anchor: 0,
            composition: CompositionState::Idle,
            focused: false,
        }
    }

    /// Validate an acknowledgement against its bound note snapshot.
    ///
    /// # Errors
    /// Returns an error when the acknowledgement belongs to a replaced view,
    /// has an invalid selection count, or contains an out-of-range byte offset.
    pub fn validate(
        &self,
        tab: TabId,
        incarnation: u64,
        text_len: usize,
    ) -> Result<(), &'static str> {
        if self.tab != tab || self.incarnation != incarnation {
            return Err("interaction belongs to a replaced view");
        }
        if self.selections.is_empty() || self.selections.len() > 256 {
            return Err("interaction must contain 1..=256 selections");
        }
        if self.viewport_anchor > text_len
            || self
                .selections
                .iter()
                .any(|selection| selection.anchor > text_len || selection.head > text_len)
        {
            return Err("interaction byte position is outside the current text");
        }
        Ok(())
    }
}

/// Revision and view identity authorizing a semantic insertion at one
/// explicitly acknowledged collapsed caret.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WritingTarget {
    pub tab: TabId,
    pub incarnation: u64,
    pub buffer_id: u64,
    pub revision: u64,
    pub interaction_sequence: u64,
    pub caret_text_byte: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selection_range_is_ordered_and_caret_is_collapsed() {
        let selection = Selection::new(9, 3);
        assert_eq!(selection.range(), (3, 9));
        assert!(!selection.is_caret());
        assert!(Selection::caret(4).is_caret());
    }

    #[test]
    fn interaction_rejects_replaced_views_and_out_of_bounds_positions() {
        let tab = TabId::from_raw(4);
        let mut state = EditorInteraction::new(tab, 2);
        assert!(state.validate(tab, 2, 10).is_ok());
        assert_eq!(
            state.validate(tab, 3, 10),
            Err("interaction belongs to a replaced view")
        );
        state.selections[0] = Selection::caret(11);
        assert_eq!(
            state.validate(tab, 2, 10),
            Err("interaction byte position is outside the current text")
        );
    }
}
