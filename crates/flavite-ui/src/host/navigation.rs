//! Per-view back/forward navigation (RFC-0003 W10, §8.1).
//!
//! Each note tab owns a bounded visit history. Navigation replaces the
//! tab's note through the reducer's typed [`LayoutAction::NavigateTab`]:
//! the tab keeps its identity, position, and mode while the replaced note's
//! caret authority is dropped — the same contract as closing the note, so
//! a writing target can never silently cross notes (D-04/D-07).
//!
//! This host is device-session state; it is never persisted (FR-26).

use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::collections::BTreeMap;

/// Bounded history per note view (NFR-01: 100 entries per note view).
pub const MAX_NAV_ENTRIES: usize = 100;

/// One visited location: the note plus the content anchor byte the view
/// carried when navigation left it, so Back restores place (W10 goal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavEntry {
    /// The note the view showed.
    pub locator: NoteLocator,
    /// Viewport/content anchor byte when the view was left.
    pub anchor: Option<usize>,
}

/// One note tab's visit history.
#[derive(Debug, Default, Clone)]
struct NoteNav {
    current: Option<NavEntry>,
    back: Vec<NavEntry>,
    forward: Vec<NavEntry>,
}

/// Host-owned per-tab navigation histories.
#[derive(Debug, Default)]
pub struct NavHost {
    tabs: BTreeMap<TabId, NoteNav>,
}

impl NavHost {
    /// Drop the history of a closed tab.
    pub fn detach(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
    }

    /// Seed the initial location of a tab the first time its note becomes
    /// ready. Later ready events for the same tab are navigation results
    /// and are recorded by [`Self::record_replacement`] instead.
    pub fn seed(&mut self, tab: TabId, locator: NoteLocator) {
        let nav = self.tabs.entry(tab).or_default();
        if nav.current.is_none() {
            nav.current = Some(NavEntry {
                locator,
                anchor: None,
            });
        }
    }

    /// Record a committed in-tab replacement: the previous location moves
    /// to Back, Forward is dropped (a new branch of history), and the new
    /// location becomes current.
    pub fn record_replacement(
        &mut self,
        tab: TabId,
        previous: NoteLocator,
        previous_anchor: Option<usize>,
        next: NoteLocator,
    ) {
        let nav = self.tabs.entry(tab).or_default();
        if nav.back.len() >= MAX_NAV_ENTRIES {
            nav.back.remove(0);
        }
        nav.back.push(NavEntry {
            locator: previous,
            anchor: previous_anchor,
        });
        nav.forward.clear();
        nav.current = Some(NavEntry {
            locator: next,
            anchor: None,
        });
    }

    /// Step back: returns the entry to navigate to after moving the current
    /// location to Forward. `current_anchor` preserves the place we leave.
    pub fn go_back(&mut self, tab: TabId, current_anchor: Option<usize>) -> Option<NavEntry> {
        let nav = self.tabs.get_mut(&tab)?;
        let previous = nav.back.pop()?;
        let current = nav.current.get_or_insert_with(|| NavEntry {
            locator: previous.locator.clone(),
            anchor: None,
        });
        current.anchor = current_anchor;
        nav.forward.push(current.clone());
        nav.current = Some(previous.clone());
        Some(previous)
    }

    /// Step forward symmetrically.
    pub fn go_forward(&mut self, tab: TabId, current_anchor: Option<usize>) -> Option<NavEntry> {
        let nav = self.tabs.get_mut(&tab)?;
        let next = nav.forward.pop()?;
        let current = nav.current.get_or_insert_with(|| NavEntry {
            locator: next.locator.clone(),
            anchor: None,
        });
        current.anchor = current_anchor;
        nav.back.push(current.clone());
        nav.current = Some(next.clone());
        Some(next)
    }

    /// Whether Back has an entry for this tab.
    #[must_use]
    pub fn can_go_back(&self, tab: TabId) -> bool {
        self.tabs.get(&tab).is_some_and(|nav| !nav.back.is_empty())
    }

    /// Whether Forward has an entry for this tab.
    #[must_use]
    pub fn can_go_forward(&self, tab: TabId) -> bool {
        self.tabs
            .get(&tab)
            .is_some_and(|nav| !nav.forward.is_empty())
    }

    /// Adopt a renamed identity across every recorded entry (T0–T2).
    pub fn relocate(&mut self, from: &NoteLocator, to: &NoteLocator) {
        for nav in self.tabs.values_mut() {
            if nav
                .current
                .as_ref()
                .is_some_and(|entry| entry.locator == *from)
            {
                let anchor = nav.current.as_ref().and_then(|entry| entry.anchor);
                nav.current = Some(NavEntry {
                    locator: to.clone(),
                    anchor,
                });
            }
            for entry in nav.back.iter_mut().chain(nav.forward.iter_mut()) {
                if entry.locator == *from {
                    entry.locator = to.clone();
                }
            }
        }
    }
}

/// Build the reducer action for one navigation step. Returns `None` when
/// the target entry equals the tab's current note (nothing to do).
#[must_use]
pub fn navigate_action<'a>(
    app: &flavite_workspace::WorkspaceState,
    tab: TabId,
    entry: &'a NavEntry,
) -> Option<Box<dyn FnOnce(u64) -> LayoutAction + 'a>> {
    use flavite_workspace::ViewKind;
    let is_same =
        matches!(app.tab(&tab)?.kind(), ViewKind::Note(note) if note.locator() == &entry.locator);
    (!is_same).then(|| {
        Box::new(move |revision: u64| LayoutAction::NavigateTab {
            expected_revision: revision,
            tab,
            locator: entry.locator.clone(),
        }) as Box<dyn FnOnce(u64) -> LayoutAction + 'a>
    })
}
