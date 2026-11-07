//! Saved-content search host state (RFC-0006 S06, D-07).
//!
//! Service logic moved out of the view: one explicit page size (50), real
//! session continuation (pages append once by ordinal; replayed tokens never
//! duplicate rows), immediate rejection of late outcomes when the query
//! changes (not only after the next debounce), explicit session close, and
//! a visible frozen snapshot/total with Refresh. Full-text mode is the
//! default; the indexed expression syntax is selected explicitly (D-05).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Debounce window before an edited query is admitted (NFR-06).
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
/// The explicit page size (S06: page=50 default; continuation appends by
/// ordinal, never by growing a limit).
pub const SEARCH_PAGE: u64 = flavite_api::search_sessions::DEFAULT_PAGE_SIZE;
/// Maximum submitted query length (NFR-12 text-setting bound).
const MAX_QUERY_BYTES: usize = 8192;

/// Which query grammar the panel submits (D-05: advanced indexed
/// expressions are selected explicitly).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchPanelMode {
    /// Whitespace-separated terms AND together; punctuation is text.
    #[default]
    FullText,
    /// Explicit expression syntax: operators, quotes, parentheses.
    Expression,
}

/// One materialized result row (presentation projection of the service
/// row; identity is the note, excerpts are display-only).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRowView {
    pub note_id: i64,
    pub path: String,
    pub title: String,
    pub excerpt: String,
}

/// Per-view search state.
#[derive(Debug, Default)]
pub struct SearchState {
    /// The edited query text.
    pub query: String,
    /// The selected query grammar.
    pub mode: SearchPanelMode,
    /// When the query last changed; the pump submits after the debounce.
    edited_at: Option<Instant>,
    /// Bumped on every edit: outcomes from an older query epoch are
    /// rejected immediately on arrival, not after the next debounce.
    query_epoch: u64,
    /// Monotonic per-tab submission sequence.
    sequence: u64,
    /// The live materialized session, when one is open.
    session: Option<SearchSessionId>,
    /// The frozen snapshot the results came from (D-07).
    pub snapshot: Option<(u64, u64)>,
    /// Exact total after materialization.
    pub total: Option<u64>,
    /// Rows appended so far, in canonical order (pages append once by
    /// ordinal; replay never duplicates).
    pub rows: Vec<SearchRowView>,
    /// Continuation token for the next page, when one exists.
    next: Option<PageToken>,
    /// Failure text from the newest submission.
    pub error: Option<String>,
    /// A submission is in flight; stale results are never painted.
    pub updating: bool,
}

impl SearchState {
    /// Whether a continuation token exists.
    #[must_use]
    pub fn has_next(&self) -> bool {
        self.next.is_some()
    }

    /// The empty render state for a tab that never searched. Renderers read
    /// through the host; the view owns no per-tab state of its own (S06).
    #[must_use]
    pub fn view_default() -> &'static Self {
        static EMPTY: std::sync::OnceLock<SearchState> = std::sync::OnceLock::new();
        EMPTY.get_or_init(SearchState::default)
    }

    fn clear_results(&mut self) {
        self.rows.clear();
        self.total = None;
        self.snapshot = None;
        self.next = None;
    }

    /// Prepare the next submission; returns the payload to submit.
    fn admit(&mut self, text: String, mode: SearchPanelMode) -> Option<SearchQueryOptions> {
        if text.trim().is_empty() {
            self.clear_results();
            self.edited_at = None;
            self.updating = false;
            self.error = None;
            return None;
        }
        if text.len() > MAX_QUERY_BYTES {
            self.edited_at = None;
            self.updating = false;
            self.error = Some("Query is too long".into());
            return None;
        }
        self.sequence = self.sequence.saturating_add(1);
        self.edited_at = None;
        self.updating = true;
        self.error = None;
        Some(SearchQueryOptions {
            mode: match mode {
                SearchPanelMode::FullText => SearchMode::FullText,
                SearchPanelMode::Expression => SearchMode::Expression,
            },
            text,
            page_size: SEARCH_PAGE,
        })
    }
}

/// Host-owned per-tab search states plus pending request correlation.
#[derive(Debug, Default)]
pub struct SearchHost {
    /// State keyed by owning tab.
    pub tabs: BTreeMap<TabId, SearchState>,
    /// In-flight submissions keyed by request token.
    requests: BTreeMap<RequestToken, (TabId, u64)>,
    /// Sessions awaiting explicit close (query changed or tab detached).
    pending_closes: Vec<SearchSessionId>,
}

impl SearchHost {
    /// The render state for one tab (its own state, or the shared empty
    /// default before the first edit).
    #[must_use]
    pub fn view_state(&self, tab: TabId) -> &SearchState {
        match self.tabs.get(&tab) {
            Some(state) => state,
            None => SearchState::view_default(),
        }
    }

    /// Drop the state of a closed tab; its session is queued for close.
    pub fn detach(&mut self, tab: TabId) {
        if let Some(state) = self.tabs.remove(&tab)
            && let Some(session) = state.session
        {
            self.pending_closes.push(session);
        }
        self.requests.retain(|_, (owner, _)| *owner != tab);
    }

    /// Record an edited query; the pump admits it after the debounce. The
    /// live session is queued for close immediately: a changed query never
    /// keeps consuming a frozen snapshot session (D-07).
    pub fn edited(&mut self, tab: TabId, value: String) {
        let state = self.tabs.entry(tab).or_default();
        state.query = value;
        state.edited_at = Some(Instant::now());
        state.query_epoch = state.query_epoch.wrapping_add(1);
        if let Some(session) = state.session.take() {
            self.pending_closes.push(session);
        }
        state.clear_results();
        state.updating = false;
    }

    /// Switch the query grammar; re-admits the current query.
    pub fn set_mode(&mut self, tab: TabId, mode: SearchPanelMode) {
        let Some(state) = self.tabs.get_mut(&tab) else {
            return;
        };
        if state.mode == mode {
            return;
        }
        state.mode = mode;
        if !state.query.trim().is_empty() {
            state.edited_at = Some(Instant::now() - SEARCH_DEBOUNCE);
            state.query_epoch = state.query_epoch.wrapping_add(1);
            if let Some(session) = state.session.take() {
                self.pending_closes.push(session);
            }
            state.clear_results();
        }
    }

    /// Admit the focused query immediately (Enter); a debounce would
    /// surprise an explicit submit.
    pub fn submit_now(&mut self, tab: TabId) {
        if let Some(state) = self.tabs.get_mut(&tab) {
            state.edited_at = Some(Instant::now() - SEARCH_DEBOUNCE);
        }
    }

    /// Close queued sessions and submit every tab whose debounce elapsed.
    ///
    /// The API handle and per-tab incarnations are passed as plain values so
    /// the borrow of the search host never overlaps the app's.
    pub fn admit_due(
        &mut self,
        flavite: Option<&flavite_api::Flavite>,
        incarnations: &BTreeMap<TabId, u64>,
    ) {
        self.flush_closes(flavite);
        let due: Vec<TabId> = self
            .tabs
            .iter()
            .filter(|(_, state)| {
                state
                    .edited_at
                    .is_some_and(|edited| edited.elapsed() >= SEARCH_DEBOUNCE)
            })
            .map(|(tab, _)| *tab)
            .collect();
        for tab in due {
            let incarnation = incarnations.get(&tab).copied().unwrap_or(0);
            let (options, sequence, query_epoch) = {
                let Some(state) = self.tabs.get_mut(&tab) else {
                    continue;
                };
                let text = state.query.clone();
                let mode = state.mode;
                let sequence = state.sequence;
                let query_epoch = state.query_epoch;
                match state.admit(text, mode) {
                    Some(options) => (options, sequence, query_epoch),
                    None => continue,
                }
            };
            let Some(flavite) = flavite else {
                continue;
            };
            match flavite.query_service().submit(
                tab.raw(),
                incarnation,
                sequence,
                QueryPayload::SearchStart { options },
            ) {
                Ok(token) => {
                    self.requests.insert(token, (tab, query_epoch));
                }
                Err(_) => {
                    if let Some(state) = self.tabs.get_mut(&tab) {
                        state.updating = false;
                        state.error = Some("Search is busy; try again".into());
                    }
                }
            }
        }
    }

    /// Explicitly close every queued session (best effort; expiry cleans up
    /// anything the facade missed).
    pub fn flush_closes(&mut self, flavite: Option<&flavite_api::Flavite>) {
        if self.pending_closes.is_empty() {
            return;
        }
        let closes = std::mem::take(&mut self.pending_closes);
        let Some(flavite) = flavite else {
            self.pending_closes = closes;
            return;
        };
        for session in closes {
            let _ = flavite.search_close(session);
        }
    }

    /// Request the next page for one tab by continuation token.
    pub fn next_page(&mut self, tab: TabId, flavite: Option<&flavite_api::Flavite>) {
        let (token, sequence, query_epoch) = {
            let Some(state) = self.tabs.get_mut(&tab) else {
                return;
            };
            let Some(token) = state.next.clone() else {
                return;
            };
            state.sequence = state.sequence.saturating_add(1);
            state.updating = true;
            (token, state.sequence, state.query_epoch)
        };
        let Some(flavite) = flavite else {
            return;
        };
        match flavite.query_service().submit(
            tab.raw(),
            0,
            sequence,
            QueryPayload::SearchPage { token },
        ) {
            Ok(request) => {
                self.requests.insert(request, (tab, query_epoch));
            }
            Err(_) => {
                if let Some(state) = self.tabs.get_mut(&tab) {
                    state.updating = false;
                    state.error = Some("Search is busy; try again".into());
                }
            }
        }
    }

    /// Refresh the tab's frozen snapshot: close the live session and
    /// re-admit the current query.
    pub fn refresh(&mut self, tab: TabId) {
        if let Some(state) = self.tabs.get_mut(&tab) {
            state.edited_at = Some(Instant::now() - SEARCH_DEBOUNCE);
            state.query_epoch = state.query_epoch.wrapping_add(1);
            if let Some(session) = state.session.take() {
                self.pending_closes.push(session);
            }
            state.clear_results();
        }
    }

    /// Route one drained outcome; returns true when it belonged to search.
    #[allow(clippy::too_many_lines)] // per-outcome append semantics are spelled out
    pub fn observe(&mut self, outcome: &QueryOutcome) -> bool {
        let Some((tab, query_epoch)) = self.requests.remove(&outcome.token) else {
            return false;
        };
        let Some(state) = self.tabs.get_mut(&tab) else {
            return false;
        };
        // Late outcomes are rejected immediately when the query changed,
        // not only after the next debounce (S06).
        let is_current = query_epoch == state.query_epoch;
        match &outcome.result {
            QueryResult::SearchStarted { page, .. } if is_current => {
                state.session = Some(page.session);
                state.snapshot = Some((page.epoch, page.generation));
                state.total = Some(page.total);
                state.rows = page
                    .rows
                    .iter()
                    .map(|row| SearchRowView {
                        note_id: row.note_id,
                        path: row.path.clone(),
                        title: row.title.clone(),
                        excerpt: row.excerpt.clone(),
                    })
                    .collect();
                state.next = page.next.clone();
                state.error = None;
                state.updating = false;
            }
            QueryResult::SearchPaged { page, ordinal } if is_current => {
                // Pages append once by ordinal; replayed tokens never
                // duplicate rows (S06).
                let expected = u64::try_from(state.rows.len()).unwrap_or(u64::MAX);
                if *ordinal == expected {
                    state.rows.extend(page.rows.iter().map(|row| SearchRowView {
                        note_id: row.note_id,
                        path: row.path.clone(),
                        title: row.title.clone(),
                        excerpt: row.excerpt.clone(),
                    }));
                    state.total = Some(page.total);
                    state.next = page.next.clone();
                    state.error = None;
                    state.updating = false;
                } else if *ordinal < expected {
                    // A replayed token for rows we already hold: drop it and
                    // keep the stored continuation.
                    state.updating = false;
                } else {
                    // A gap: the stored token is still the truth; allow the
                    // retry by keeping the request registered.
                    self.requests.insert(outcome.token, (tab, query_epoch));
                    return false;
                }
            }
            QueryResult::Failed { message } if is_current => {
                state.error = Some(message.clone());
                state.updating = false;
            }
            QueryResult::SearchClosed => {}
            // Updating/stale outcomes never repaint and stay correlated so a
            // duplicate completion is still recognized.
            _ => {
                self.requests.insert(outcome.token, (tab, query_epoch));
                return false;
            }
        }
        true
    }
}

use flavite_api::search_sessions::{PageToken, SearchMode, SearchQueryOptions, SearchSessionId};
use flavite_api::{ContextQueryResult as QueryResult, QueryOutcome, QueryPayload, RequestToken};
use flavite_workspace::TabId;

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_api::search_sessions::SearchPageMaterialized;
    use std::sync::Arc;

    fn token(sequence: u64) -> RequestToken {
        RequestToken {
            session_nonce: 7,
            purpose: flavite_api::QueryPurpose::Search,
            context_tab: 1,
            incarnation: 0,
            binding_sequence: sequence,
            request_sequence: sequence,
        }
    }

    fn paged_page(
        ordinal: u64,
        rows: usize,
        total: u64,
        next: Option<u64>,
    ) -> SearchPageMaterialized {
        SearchPageMaterialized {
            session: flavite_api::search_sessions::SearchSessionId::from_u128(9),
            epoch: 1,
            generation: 4,
            total,
            rows: (0..rows)
                .map(|index| flavite_api::search_sessions::SearchRow {
                    note_id: i64::try_from(ordinal * 100 + u64::try_from(index).unwrap_or(0))
                        .unwrap_or(0),
                    path: format!("n{}.md", index),
                    title: "t".into(),
                    score: Some(1.0),
                    excerpt: String::new(),
                    content_hash: Vec::new(),
                })
                .collect(),
            next: next.map(|next_ordinal| PageToken {
                session: flavite_api::search_sessions::SearchSessionId::from_u128(9),
                query_digest: [0u8; 32],
                epoch: 1,
                generation: 4,
                next_ordinal,
            }),
        }
    }

    #[test]
    fn replayed_page_outcomes_never_duplicate_rows() {
        let mut host = SearchHost::default();
        let tab = TabId::from_raw(1);
        {
            let state = host.tabs.entry(tab).or_default();
            state.query_epoch = 3;
            state.session = Some(flavite_api::search_sessions::SearchSessionId::from_u128(9));
        }
        // The page-1 outcome paints rows 0..50.
        let token2 = token(2);
        host.requests.insert(token2, (tab, 3));
        let outcome = QueryOutcome {
            token: token2,
            payload: QueryPayload::SearchPage {
                token: PageToken {
                    session: flavite_api::search_sessions::SearchSessionId::from_u128(9),
                    query_digest: [0u8; 32],
                    epoch: 1,
                    generation: 4,
                    next_ordinal: 0,
                },
            },
            result: QueryResult::SearchPaged {
                page: Arc::new(paged_page(0, 50, 120, Some(50))),
                ordinal: 0,
            },
        };
        assert!(host.observe(&outcome));
        assert_eq!(host.tabs[&tab].rows.len(), 50);
        // The SAME token replays: the ordinal already exists, so no row is
        // appended twice (S06: pages append once by ordinal).
        let replay = QueryOutcome {
            token: token2,
            payload: outcome.payload.clone(),
            result: QueryResult::SearchPaged {
                page: Arc::new(paged_page(0, 50, 120, Some(50))),
                ordinal: 0,
            },
        };
        // Re-correlate the replay (the first observe consumed the entry).
        host.requests.insert(token2, (tab, 3));
        assert!(host.observe(&replay));
        assert_eq!(host.tabs[&tab].rows.len(), 50, "replay must not duplicate");
        // A gap (ordinal ahead of the held rows) is re-correlated for retry.
        let token3 = token(3);
        host.requests.insert(token3, (tab, 3));
        let gap = QueryOutcome {
            token: token3,
            payload: QueryPayload::SearchPage {
                token: PageToken {
                    session: flavite_api::search_sessions::SearchSessionId::from_u128(9),
                    query_digest: [0u8; 32],
                    epoch: 1,
                    generation: 4,
                    next_ordinal: 200,
                },
            },
            result: QueryResult::SearchPaged {
                page: Arc::new(paged_page(200, 20, 220, None)),
                ordinal: 200,
            },
        };
        assert!(!host.observe(&gap), "a gapped page stays correlated");
        assert_eq!(host.tabs[&tab].rows.len(), 50);
    }

    #[test]
    fn outcomes_from_an_older_query_epoch_are_rejected_immediately() {
        let mut host = SearchHost::default();
        let tab = TabId::from_raw(1);
        {
            let state = host.tabs.entry(tab).or_default();
            state.query_epoch = 5;
        }
        let stale = token(1);
        host.requests.insert(stale, (tab, 4));
        let outcome = QueryOutcome {
            token: stale,
            payload: QueryPayload::SearchStart {
                options: SearchQueryOptions {
                    mode: SearchMode::FullText,
                    text: "old".into(),
                    page_size: 50,
                },
            },
            result: QueryResult::SearchStarted {
                page: Arc::new(paged_page(0, 10, 10, None)),
                page_size: 50,
            },
        };
        // The outcome's query epoch (4) no longer matches the state (5):
        // rejected immediately, not after the next debounce.
        assert!(!host.observe(&outcome));
        assert!(host.tabs[&tab].rows.is_empty());
        assert!(host.tabs[&tab].total.is_none());
    }
}
