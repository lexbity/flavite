//! Host-side document registry: documents, view leases, and open flights.
//!
//! The registry maps validated locators and Session buffer IDs to view
//! leases (RFC-0003 §7.1, D-10, W03). Documents are the durable unit: two
//! tabs on the same note share one open flight and one BufferId, while each
//! lease keeps its own process-local incarnation. Closing a view releases
//! only its lease — the document entry, its Session buffer, and its history
//! stay with the Session. Closing a view never issues `note.close` (D-10).

use flavite_workspace::NoteLocator;
use std::collections::BTreeMap;

/// Host state behind one view lease on a document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewLease {
    /// Process-local resource generation, invalidating late editor events.
    pub incarnation: u64,
}

/// Host state behind one opened or opening document.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DocumentEntry {
    /// The Session buffer once the open completed.
    buffer: Option<u64>,
    /// The outstanding shared open flight, if any.
    open_request: Option<u64>,
    /// Live view leases on this document.
    leases: BTreeMap<flavite_workspace::TabId, ViewLease>,
}

/// A snapshot of one tab's document binding, for rendering and guards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocumentTabState {
    /// The note this tab displays.
    pub locator: NoteLocator,
    /// The Session buffer once the open completed.
    pub buffer: Option<u64>,
    /// Process-local resource generation for this lease.
    pub incarnation: u64,
}

/// What the host must do after attaching a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachDecision {
    /// The document already has a buffer; no open is needed.
    Ready,
    /// Another lease's open flight covers this document; the completion
    /// will install the buffer for every lease at once.
    FlightPending,
    /// This is the first lease; the host must issue the open and record the
    /// request with [`DocumentRegistry::track_request`].
    IssueOpen,
}

/// Outcome of a completed open flight: the document and every lease that
/// was waiting on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlightCompletion {
    /// The document the completed flight was opening.
    pub locator: NoteLocator,
    /// Every lease tab that shares the flight's outcome.
    pub tabs: Vec<flavite_workspace::TabId>,
}

/// Lease/registry keyed by validated locator (RFC-0003 W03 `documents.rs`).
#[derive(Debug, Default)]
pub struct DocumentRegistry {
    documents: BTreeMap<NoteLocator, DocumentEntry>,
    requests: BTreeMap<u64, NoteLocator>,
    next_incarnation: u64,
}

impl DocumentRegistry {
    /// Attach a note view lease and report whether an open must be issued.
    ///
    /// The returned incarnation is fresh for every lease, even when several
    /// leases share one document.
    pub fn attach(
        &mut self,
        tab: flavite_workspace::TabId,
        locator: NoteLocator,
    ) -> (flavite_workspace::TabId, u64, AttachDecision) {
        self.next_incarnation = self.next_incarnation.saturating_add(1).max(1);
        let incarnation = self.next_incarnation;
        let entry = self.documents.entry(locator.clone()).or_default();
        let fresh = entry.leases.insert(tab, ViewLease { incarnation });
        debug_assert!(fresh.is_none(), "a fresh tab cannot already hold a lease");
        let decision = if entry.buffer.is_some() {
            AttachDecision::Ready
        } else if entry.open_request.is_some() {
            AttachDecision::FlightPending
        } else {
            AttachDecision::IssueOpen
        };
        (tab, incarnation, decision)
    }

    /// Release one view's lease. The document entry is retained while its
    /// buffer or open flight persists, so reopening deduplicates and late
    /// completions stay correlated.
    pub fn detach(&mut self, tab: flavite_workspace::TabId) {
        let Some((locator, entry)) = self
            .documents
            .iter_mut()
            .find(|(_, entry)| entry.leases.contains_key(&tab))
            .map(|(locator, entry)| (locator.clone(), entry))
        else {
            return;
        };
        entry.leases.remove(&tab);
        if entry.buffer.is_none() && entry.open_request.is_none() && entry.leases.is_empty() {
            self.documents.remove(&locator);
        }
    }

    /// Record the outstanding open flight for a document's leases.
    pub fn track_request(&mut self, tab: flavite_workspace::TabId, request_id: u64) {
        let Some((locator, entry)) = self
            .documents
            .iter_mut()
            .find(|(_, entry)| entry.leases.contains_key(&tab))
            .map(|(locator, entry)| (locator.clone(), entry))
        else {
            return;
        };
        entry.open_request = Some(request_id);
        self.requests.insert(request_id, locator);
    }

    /// The document whose open flight completed, clearing the request.
    pub fn finish_request(&mut self, request_id: u64) -> Option<FlightCompletion> {
        let locator = self.requests.remove(&request_id)?;
        let entry = self.documents.get_mut(&locator)?;
        entry.open_request = None;
        Some(FlightCompletion {
            locator,
            tabs: entry.leases.keys().copied().collect(),
        })
    }

    /// Record the installed buffer for every lease of the tab's document.
    pub fn ready(&mut self, tab: flavite_workspace::TabId, buffer: Option<u64>) {
        let Some(entry) = self
            .documents
            .values_mut()
            .find(|entry| entry.leases.contains_key(&tab))
        else {
            return;
        };
        entry.buffer = buffer;
        entry.open_request = None;
    }

    /// Install a completed flight's buffer for a document by locator.
    pub fn install_buffer(&mut self, locator: &NoteLocator, buffer: Option<u64>) {
        if let Some(entry) = self.documents.get_mut(locator) {
            entry.buffer = buffer;
            entry.open_request = None;
        }
    }

    /// Host state for one tab's lease.
    #[must_use]
    pub fn state(&self, tab: &flavite_workspace::TabId) -> Option<DocumentTabState> {
        self.documents.iter().find_map(|(locator, entry)| {
            entry.leases.get(tab).map(|lease| DocumentTabState {
                locator: locator.clone(),
                buffer: entry.buffer,
                incarnation: lease.incarnation,
            })
        })
    }

    /// The Session buffer behind a tab, if its document is open.
    #[must_use]
    pub fn buffer(&self, tab: &flavite_workspace::TabId) -> Option<u64> {
        self.state(tab).and_then(|state| state.buffer)
    }

    /// Current process-local incarnation for an attached view.
    #[must_use]
    pub fn incarnation(&self, tab: flavite_workspace::TabId) -> Option<u64> {
        self.state(&tab).map(|state| state.incarnation)
    }

    /// The locator a tab displays.
    #[must_use]
    pub fn locator_of(&self, tab: flavite_workspace::TabId) -> Option<NoteLocator> {
        self.state(&tab).map(|state| state.locator)
    }

    /// All attached lease tabs with their document state.
    pub fn tabs(&self) -> impl Iterator<Item = (flavite_workspace::TabId, DocumentTabState)> + '_ {
        self.documents.iter().flat_map(|(locator, entry)| {
            entry.leases.iter().map(move |(tab, lease)| {
                (
                    *tab,
                    DocumentTabState {
                        locator: locator.clone(),
                        buffer: entry.buffer,
                        incarnation: lease.incarnation,
                    },
                )
            })
        })
    }

    /// Whether any lease still awaits its open completion.
    #[must_use]
    pub fn any_pending(&self) -> bool {
        self.documents
            .values()
            .any(|entry| entry.open_request.is_some() && !entry.leases.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_workspace::TabId;

    fn locator(name: &str) -> NoteLocator {
        NoteLocator::from_components([name]).expect("locator")
    }

    #[test]
    fn two_leases_share_one_document_and_flight() {
        let mut registry = DocumentRegistry::default();
        let first = TabId::from_raw(11);
        let second = TabId::from_raw(12);
        let (_, incarnation_first, decision) = registry.attach(first, locator("a.md"));
        assert_eq!(decision, AttachDecision::IssueOpen);
        registry.track_request(first, 9001);
        let (_, incarnation_second, decision) = registry.attach(second, locator("a.md"));
        assert_eq!(decision, AttachDecision::FlightPending);
        assert_ne!(incarnation_first, incarnation_second);

        let completion = registry.finish_request(9001).expect("flight completion");
        assert_eq!(completion.tabs, vec![first, second]);
        registry.ready(first, Some(7));
        assert_eq!(registry.buffer(&first), Some(7));
        assert_eq!(registry.buffer(&second), Some(7));
        assert!(!registry.any_pending());
    }

    #[test]
    fn lease_close_retains_the_document_buffer() {
        let mut registry = DocumentRegistry::default();
        let first = TabId::from_raw(11);
        let second = TabId::from_raw(12);
        registry.attach(first, locator("a.md"));
        registry.attach(second, locator("a.md"));
        registry.ready(first, Some(7));
        registry.detach(first);
        assert_eq!(registry.buffer(&second), Some(7));
        // The document survives with no leases: reopening reuses the buffer.
        registry.detach(second);
        let (_, _, decision) = registry.attach(first, locator("a.md"));
        assert_eq!(decision, AttachDecision::Ready);
        assert_eq!(registry.buffer(&first), Some(7));
    }

    #[test]
    fn completion_for_a_detached_lease_still_installs_the_buffer() {
        let mut registry = DocumentRegistry::default();
        let first = TabId::from_raw(11);
        registry.attach(first, locator("a.md"));
        registry.track_request(first, 9001);
        registry.detach(first);
        // The flight was outstanding, so the document entry is retained.
        let completion = registry.finish_request(9001).expect("flight retained");
        assert!(completion.tabs.is_empty());
    }

    #[test]
    fn finishing_an_unknown_request_changes_nothing() {
        let mut registry = DocumentRegistry::default();
        assert!(registry.finish_request(1).is_none());
    }
}
