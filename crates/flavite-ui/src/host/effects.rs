//! Host-side view registry: per-tab note state and effect application.
//!
//! The workspace model reports lifecycle effects; this registry owns the
//! host resources behind them (open requests and buffer bindings). Closing
//! a view releases only its entry — the Session buffer and its history
//! stay with the Session (D-10).

use flavite_workspace::{NoteLocator, TabId};
use std::collections::BTreeMap;

/// Host state behind one source-reading note tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoteTabState {
    /// The note this tab displays.
    pub locator: NoteLocator,
    /// The Session buffer once the open completed.
    pub buffer: Option<u64>,
    /// The outstanding open request, if any.
    pub request: Option<u64>,
}

/// Per-tab note state for all attached note views.
#[derive(Debug, Clone, Default)]
pub struct NoteRegistry {
    tabs: BTreeMap<TabId, NoteTabState>,
}

impl NoteRegistry {
    /// Attach a note view and produce its open path.
    pub fn attach(&mut self, tab: TabId, locator: NoteLocator) -> Option<(TabId, NoteLocator)> {
        let fresh = self.tabs.insert(
            tab,
            NoteTabState {
                locator: locator.clone(),
                buffer: None,
                request: None,
            },
        );
        debug_assert!(fresh.is_none(), "a fresh tab cannot already be attached");
        Some((tab, locator))
    }

    /// Release one view's host state.
    pub fn detach(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
    }

    /// Track the outstanding open request for a tab.
    pub fn track_request(&mut self, tab: TabId, request_id: u64) {
        if let Some(state) = self.tabs.get_mut(&tab) {
            state.request = Some(request_id);
        }
    }

    /// The tab whose open request completed, clearing the request.
    pub fn finish_request(&mut self, request_id: u64) -> Option<TabId> {
        let entry = self
            .tabs
            .iter_mut()
            .find(|(_, state)| state.request == Some(request_id))?;
        entry.1.request = None;
        Some(*entry.0)
    }

    /// Record the installed buffer for a tab.
    pub fn ready(&mut self, tab: TabId, buffer: Option<u64>) {
        if let Some(state) = self.tabs.get_mut(&tab) {
            state.buffer = buffer;
            state.request = None;
        }
    }

    /// Host state for one tab.
    #[must_use]
    pub fn state(&self, tab: &TabId) -> Option<&NoteTabState> {
        self.tabs.get(tab)
    }

    /// All attached tabs.
    #[must_use]
    pub fn tabs(&self) -> impl Iterator<Item = (TabId, &NoteTabState)> {
        self.tabs.iter().map(|(tab, state)| (*tab, state))
    }

    /// Whether any tab still awaits its open completion.
    #[must_use]
    pub fn any_pending(&self) -> bool {
        self.tabs.values().any(|state| state.request.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn locator(name: &str) -> NoteLocator {
        NoteLocator::from_components([name]).expect("locator")
    }

    #[test]
    fn attach_track_finish_and_detach_round_trip() {
        let mut registry = NoteRegistry::default();
        let tab = TabId::from_raw(11);
        assert_eq!(
            registry.attach(tab, locator("a.md")),
            Some((tab, locator("a.md")))
        );
        registry.track_request(tab, 9001);
        assert!(registry.any_pending());
        assert_eq!(registry.finish_request(9001), Some(tab));
        assert!(!registry.any_pending());
        registry.ready(tab, Some(7));
        assert_eq!(registry.state(&tab).and_then(|state| state.buffer), Some(7));
        registry.detach(tab);
        assert!(registry.state(&tab).is_none());
    }

    #[test]
    fn finishing_an_unknown_request_changes_nothing() {
        let mut registry = NoteRegistry::default();
        assert_eq!(registry.finish_request(1), None);
    }
}
