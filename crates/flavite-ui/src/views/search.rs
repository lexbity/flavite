//! The saved-content Search view (W10).
//!
//! One view per tab: a query input with a 120 ms debounce (NFR-06), a
//! sequence-correlated submission through the bounded QueryService worker
//! pool, bounded excerpt rows, explicit paging, and an always-visible label
//! that results reflect saved content only (D-08: dirty text is absent).

use crate::app::{App, Message};
use crate::style;
use crate::workspace::tab_strip;
use flavite_api::{ContextQueryResult, QueryOutcome, QueryPayload, RequestToken, SearchResultRow};
use flavite_workspace::TabId;
use iced::widget::{Space, button, column, container, row, scrollable, text, text_input};
use iced::{Element, Length};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// Debounce window before an edited query is admitted (NFR-06).
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(120);
/// Page size per submission; paging is explicit.
pub const SEARCH_PAGE: u64 = 50;
/// Largest accepted page limit.
const MAX_PAGE_LIMIT: u64 = 200;
/// Maximum submitted query length (NFR-12 text-setting bound).
const MAX_QUERY_BYTES: usize = 8192;

/// Input widget id prefix for semantic tests.
pub const SEARCH_INPUT_ID_PREFIX: &str = "search-input-";

fn input_id(tab: TabId) -> iced::widget::Id {
    iced::widget::Id::from(format!("{SEARCH_INPUT_ID_PREFIX}{}", tab.raw()))
}

/// Per-view search state.
#[derive(Debug, Default)]
pub struct SearchState {
    /// The edited query text.
    pub query: String,
    /// The last admitted submission: text, binding sequence, page limit.
    submitted: Option<(String, u64, u64)>,
    /// When the query last changed; the pump submits after the debounce.
    edited_at: Option<Instant>,
    /// Monotonic per-tab submission sequence.
    sequence: u64,
    /// Page limit for the next submission (grows via Load more).
    page_limit: u64,
    /// Results of the newest admitted submission, in canonical path order.
    pub results: Vec<SearchResultRow>,
    /// Index provenance of the served page.
    pub provenance: Option<(u64, u64)>,
    /// Whether a further page exists for the served submission.
    pub has_more: bool,
    /// Failure text from the newest submission.
    pub error: Option<String>,
    /// A submission is in flight; stale results are never painted.
    pub updating: bool,
}

impl SearchState {
    fn admit(&mut self, text: String, limit: u64) -> Option<(String, u64, u64)> {
        if text.trim().is_empty() {
            self.submitted = None;
            self.results.clear();
            self.provenance = None;
            self.has_more = false;
            self.error = None;
            self.updating = false;
            self.edited_at = None;
            return None;
        }
        if self
            .submitted
            .as_ref()
            .is_some_and(|(prev, _, prev_limit)| *prev == text && *prev_limit == limit)
        {
            self.edited_at = None;
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
        self.submitted = Some((text, self.sequence, limit));
        self.submitted.clone()
    }
}

/// Host-owned per-tab search states plus pending request correlation.
#[derive(Debug, Default)]
pub struct SearchHost {
    /// State keyed by owning tab.
    pub tabs: BTreeMap<TabId, SearchState>,
    /// In-flight submissions keyed by request token.
    requests: BTreeMap<RequestToken, TabId>,
}

impl SearchHost {
    /// Drop the state of a closed tab.
    pub fn detach(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
        self.requests.retain(|_, owner| *owner != tab);
    }

    /// Record an edited query; the pump admits it after the debounce.
    pub fn edited(&mut self, tab: TabId, value: String) {
        let state = self.tabs.entry(tab).or_default();
        state.query = value;
        state.edited_at = Some(Instant::now());
    }

    /// Admit the focused query immediately (Enter); a debounce would
    /// surprise an explicit submit.
    pub fn submit_now(&mut self, tab: TabId) {
        if let Some(state) = self.tabs.get_mut(&tab) {
            state.edited_at = Some(Instant::now() - SEARCH_DEBOUNCE);
        }
    }

    /// Grow the page and re-admit the same text (Load more).
    pub fn load_more(&mut self, tab: TabId) {
        if let Some(state) = self.tabs.get_mut(&tab) {
            state.page_limit = (state.page_limit + SEARCH_PAGE).min(MAX_PAGE_LIMIT);
            state.edited_at = Some(Instant::now() - SEARCH_DEBOUNCE);
            // Force re-admission even though the text is unchanged: drop
            // the recorded submission so the limit change is observed.
            state.submitted = None;
        }
    }

    /// Submit every tab whose debounce elapsed (called from the pump).
    ///
    /// The API handle and per-tab incarnations are passed as plain values so
    /// the borrow of the search host never overlaps the app's.
    pub fn admit_due(
        &mut self,
        flavite: Option<&flavite_api::Flavite>,
        incarnations: &BTreeMap<TabId, u64>,
    ) {
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
            let (text, sequence, limit) = {
                let Some(state) = self.tabs.get_mut(&tab) else {
                    continue;
                };
                let text = state.query.clone();
                let limit = state.page_limit;
                let Some(admitted) = state.admit(text, limit) else {
                    continue;
                };
                admitted
            };
            let Some(flavite) = flavite else {
                continue;
            };
            let payload = QueryPayload::Search { text, limit };
            match flavite
                .query_service()
                .submit(tab.raw(), incarnation, sequence, payload)
            {
                Ok(token) => {
                    self.requests.insert(token, tab);
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

    /// Route one drained outcome; returns true when it belonged to search.
    pub fn observe(&mut self, outcome: &QueryOutcome) -> bool {
        let Some(tab) = self.requests.remove(&outcome.token) else {
            return false;
        };
        let Some(state) = self.tabs.get_mut(&tab) else {
            return false;
        };
        // Only the newest admitted sequence may paint (cancellation).
        let is_current = state
            .submitted
            .as_ref()
            .is_some_and(|(_, sequence, _)| *sequence == outcome.token.binding_sequence);
        match &outcome.result {
            ContextQueryResult::Search {
                items,
                provenance,
                has_more,
                ..
            } if is_current => {
                state.results = items.clone();
                state.provenance = Some((provenance.epoch, provenance.generation));
                state.has_more = *has_more;
                state.error = None;
                state.updating = false;
            }
            ContextQueryResult::Failed { message } if is_current => {
                state.error = Some(message.clone());
                state.updating = false;
            }
            // Updating/stale outcomes never repaint and stay correlated so a
            // duplicate completion is still recognized.
            _ => {
                self.requests.insert(outcome.token, tab);
                return false;
            }
        }
        true
    }
}

/// Render the Search view for one tab. The input renders even before the
/// first edit so the view is immediately usable after opening.
#[allow(clippy::too_many_lines)]
pub fn view(app: &App, tab: TabId) -> Element<'_, Message> {
    let metrics = app.appearance.metrics();
    let default = SearchState::default();
    let state = app.search.tabs.get(&tab).unwrap_or(&default);
    let query = state.query.clone();
    let results_snapshot = state.results.clone();
    let mut results = column![].spacing(2);
    for result in &results_snapshot {
        results = results.push(result_row(*metrics, result, tab));
    }
    let _ = &results_snapshot;
    if state.results.is_empty() && state.error.is_none() && !state.updating {
        results = results.push(
            text(if state.query.trim().is_empty() {
                "Search matches saved note content: words, tags, links, properties."
            } else {
                "No saved notes match."
            })
            .style(style::dim_text),
        );
    }
    let status = if state.updating {
        String::from("Searching…")
    } else if let Some(error) = &state.error {
        error.clone()
    } else if let Some((epoch, generation)) = state.provenance {
        format!("Saved content · index {epoch}/{generation} · unsaved edits are not searched")
    } else {
        String::from("Saved content only — unsaved edits are not searched")
    };
    let load_more = state.has_more.then(|| {
        button(text("Load more"))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press(Message::SearchLoadMore { tab })
    });

    container(
        column![
            row![
                text_input("Search saved notes…", &query)
                    .id(input_id(tab))
                    .on_input(move |value| Message::SearchQuery { tab, value })
                    .on_submit(Message::SearchSubmitNow { tab })
                    .padding(tab_strip::chip_padding())
                    .size(metrics.text_sm)
                    .width(Length::Fill),
            ]
            .spacing(metrics.spacing_sm),
            text(status).size(metrics.text_sm).style(style::dim_text),
            container(scrollable(results).height(Length::Fill))
                .width(Length::Fill)
                .height(Length::Fill)
                .padding(metrics.spacing_sm)
                .style(style::card),
            row![
                Space::new().width(Length::Fill).height(Length::Fixed(0.0)),
                load_more,
            ]
            .spacing(metrics.spacing_sm),
        ]
        .spacing(metrics.spacing_sm),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(metrics.spacing_lg)
    .style(style::content)
    .into()
}

fn result_row(
    metrics: crate::style::Metrics,
    result: &SearchResultRow,
    tab: TabId,
) -> Element<'static, Message> {
    let locator = flavite_workspace::NoteLocator::from_relative(std::path::Path::new(&result.path));
    let open = locator.ok().map(|locator| {
        Message::OpenIntent(crate::host::open_intents::OpenIntent::reuse(locator, None))
    });
    let mut body = column![
        text(result.title.clone()),
        text(result.path.clone())
            .size(metrics.text_sm)
            .style(style::dim_text),
    ]
    .spacing(2);
    if !result.excerpt.is_empty() {
        body = body.push(
            text(result.excerpt.clone())
                .size(metrics.text_sm)
                .style(style::dim_text),
        );
    }
    let _ = tab;
    match open {
        Some(message) => button(body)
            .width(Length::Fill)
            .padding(iced::Padding {
                top: 6.0,
                bottom: 6.0,
                left: 10.0,
                right: 10.0,
            })
            .style(style::list_row(false))
            .on_press(message)
            .into(),
        None => container(body)
            .width(Length::Fill)
            .padding(iced::Padding {
                top: 6.0,
                bottom: 6.0,
                left: 10.0,
                right: 10.0,
            })
            .into(),
    }
}
