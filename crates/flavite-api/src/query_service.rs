//! Bounded asynchronous note/backlink query service (RFC-0003 §10, W04).
//!
//! The workspace submits context queries with request tokens; bounded
//! workers execute saved-content reads and structural index queries off the
//! UI thread and publish typed results into slots. Slots are bounded, late
//! or superseded sequences never overwrite newer ones, and no SQLite,
//! index, or vault types cross this boundary.

use flavite_index::query::Query;
use flavite_vault::session::Session;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Live query slots the service admits (RFC §20: saturating 64 slots yields
/// typed `Busy`, never unbounded queueing).
pub const MAX_QUERY_SLOTS: usize = 64;

/// Upper bound on backlink rows per query (NFR-02 scale).
pub const MAX_BACKLINK_ROWS: usize = 200;

/// Caller-correlated identity of one context query (§7.1). The service
/// stamps the session nonce; the host owns tab, incarnation, and binding
/// sequence so a rebind or replaced view can never be repainted by an
/// older query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestToken {
    /// Session-allocated nonce namespace.
    pub session_nonce: u128,
    /// Requesting context tool tab (raw opaque ID).
    pub context_tab: u128,
    /// Requesting view incarnation.
    pub incarnation: u64,
    /// Binding publication the query was issued for.
    pub binding_sequence: u64,
    /// Monotonic per-service request sequence.
    pub request_sequence: u64,
}

/// What to query. Vault-relative path components keep native identity
/// without leaking locator types across the facade boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryPayload {
    /// Saved source text of one note.
    Preview {
        /// Vault-relative path components.
        path: Vec<String>,
    },
    /// Inbound links of one note.
    Backlinks {
        /// Vault-relative path components.
        path: Vec<String>,
    },
}

/// Index provenance of a served snapshot. Cache IDs never migrate by
/// numeric equality across epochs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provenance {
    /// Structural epoch.
    pub epoch: u64,
    /// Structural generation.
    pub generation: u64,
}

/// One inbound link row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacklinkRow {
    /// Vault-relative path of the linking note.
    pub source_path: String,
    /// Indexed title of the linking note.
    pub source_title: String,
    /// The line of the source containing a link occurrence, when the source
    /// could be read.
    pub excerpt: Option<String>,
}

/// Typed outcome of one query slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextQueryResult {
    /// Saved source text (dirty buffer content is never shown here).
    Preview {
        /// Vault-relative path components.
        path: Vec<String>,
        /// Saved text.
        text: String,
        /// Index provenance.
        provenance: Provenance,
    },
    /// Inbound links with provenance.
    Backlinks {
        /// Vault-relative path components.
        path: Vec<String>,
        /// Linking notes; empty means genuinely no backlinks.
        links: Vec<BacklinkRow>,
        /// Index provenance.
        provenance: Provenance,
    },
    /// The requested note does not exist in the saved vault.
    Missing,
    /// The query failed; the message is retryable presentation text.
    Failed {
        /// Bounded, redacted failure text.
        message: String,
    },
}

/// A completed query slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryOutcome {
    /// The token this outcome answers.
    pub token: RequestToken,
    /// The payload that was executed.
    pub payload: QueryPayload,
    /// The typed result.
    pub result: ContextQueryResult,
}

/// Why a submission was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuerySubmitError {
    /// Every slot is occupied; retry later (NFR-08: no silent loss).
    Busy,
}

struct Job {
    token: RequestToken,
    payload: QueryPayload,
}

/// Bounded async query service owned by the facade.
pub struct QueryService {
    // Manual `Debug` below: worker handles and locks are not meaningful in
    // diagnostics and must not leak identities.
    jobs: crossbeam::channel::Sender<Job>,
    ready: Arc<Mutex<BTreeMap<RequestToken, (QueryPayload, ContextQueryResult)>>>,
    in_flight: Arc<AtomicUsize>,
    session_nonce: u128,
    request_sequence: AtomicU64,
    _workers: Vec<std::thread::JoinHandle<()>>,
}

impl std::fmt::Debug for QueryService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryService")
            .field("in_flight", &self.in_flight())
            .field("session_nonce", &{
                // Nonce is identity, not secret content; keep it for tests.
                self.session_nonce
            })
            .finish_non_exhaustive()
    }
}

impl QueryService {
    pub(crate) fn new(session: Arc<Session>, session_nonce: u128) -> Self {
        let (jobs, receiver) = crossbeam::channel::unbounded::<Job>();
        let ready: Arc<Mutex<BTreeMap<RequestToken, (QueryPayload, ContextQueryResult)>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for worker_index in 0..2 {
            let receiver = receiver.clone();
            let session = Arc::clone(&session);
            let ready = Arc::clone(&ready);
            let in_flight = Arc::clone(&in_flight);
            workers.push(
                std::thread::Builder::new()
                    .name(format!("flavite-query-{worker_index}"))
                    .spawn(move || {
                        while let Ok(job) = receiver.recv() {
                            let result = execute(&session, &job.payload);
                            publish(&ready, &in_flight, job, result);
                        }
                    })
                    .expect("query worker thread creation failed"),
            );
        }
        Self {
            jobs,
            ready,
            in_flight,
            session_nonce,
            request_sequence: AtomicU64::new(0),
            _workers: workers,
        }
    }

    /// Submit a query for a context tool. The token is derived from the
    /// caller's tab/incarnation/binding identity plus a service-allocated
    /// session nonce and request sequence.
    ///
    /// # Errors
    /// Returns [`QuerySubmitError::Busy`] when every slot is occupied.
    pub fn submit(
        &self,
        context_tab: u128,
        incarnation: u64,
        binding_sequence: u64,
        payload: QueryPayload,
    ) -> Result<RequestToken, QuerySubmitError> {
        if self.in_flight.load(Ordering::Acquire) >= MAX_QUERY_SLOTS {
            return Err(QuerySubmitError::Busy);
        }
        self.in_flight.fetch_add(1, Ordering::AcqRel);
        let token = RequestToken {
            session_nonce: self.session_nonce,
            context_tab,
            incarnation,
            binding_sequence,
            request_sequence: self.request_sequence.fetch_add(1, Ordering::AcqRel),
        };
        if self.jobs.send(Job { token, payload }).is_err() {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            return Err(QuerySubmitError::Busy);
        }
        Ok(token)
    }

    /// Drain every completed slot. Results for superseded sequences are
    /// already dropped here: a late sequence `n` can never overwrite or
    /// coexist with a served `n+1` for the same requesting view.
    pub fn drain(&self) -> Vec<QueryOutcome> {
        let mut ready = self.ready.lock().expect("query slot lock");
        std::mem::take(&mut *ready)
            .into_iter()
            .map(|(token, (payload, result))| QueryOutcome {
                token,
                payload,
                result,
            })
            .collect()
    }

    /// Number of queries currently admitted but not yet served.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }
}

/// Publish a result, dropping older sequences for the same requesting view.
fn publish(
    ready: &Mutex<BTreeMap<RequestToken, (QueryPayload, ContextQueryResult)>>,
    in_flight: &AtomicUsize,
    job: Job,
    result: ContextQueryResult,
) {
    let mut slots = ready.lock().expect("query slot lock");
    // Superseded results for the same view identity are discarded in both
    // directions: a late sequence n can never overwrite or coexist with an
    // already-served n+1, and older entries never outlive a newer one.
    let same_view = |token: &RequestToken| {
        token.context_tab == job.token.context_tab && token.incarnation == job.token.incarnation
    };
    let newer_exists = slots.keys().any(|token| {
        same_view(token)
            && (token.binding_sequence, token.request_sequence)
                > (job.token.binding_sequence, job.token.request_sequence)
    });
    if !newer_exists {
        let superseded: Vec<RequestToken> = slots
            .keys()
            .filter(|token| {
                same_view(token)
                    && (token.binding_sequence, token.request_sequence)
                        < (job.token.binding_sequence, job.token.request_sequence)
            })
            .copied()
            .collect();
        for token in superseded {
            slots.remove(&token);
        }
        slots.insert(job.token, (job.payload, result));
    }
    in_flight.fetch_sub(1, Ordering::AcqRel);
}

/// Execute one query against the Session read surface.
fn execute(session: &Arc<Session>, payload: &QueryPayload) -> ContextQueryResult {
    match payload {
        QueryPayload::Preview { path } => execute_preview(session, path),
        QueryPayload::Backlinks { path } => execute_backlinks(session, path),
    }
}

fn root_path(session: &Arc<Session>, components: &[String]) -> PathBuf {
    let mut path = session.root().clone();
    for component in components {
        path.push(component);
    }
    path
}

fn provenance(session: &Arc<Session>) -> Result<(u64, u64), String> {
    let page = session.note_catalog(1)?;
    Ok((page.epoch, page.generation))
}

fn execute_preview(session: &Arc<Session>, path: &[String]) -> ContextQueryResult {
    let full = root_path(session, path);
    let bytes = match session.read_saved_note(&full) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return ContextQueryResult::Missing,
        Err(message) => return ContextQueryResult::Failed { message },
    };
    let (epoch, generation) = match provenance(session) {
        Ok(values) => values,
        Err(message) => return ContextQueryResult::Failed { message },
    };
    ContextQueryResult::Preview {
        path: path.to_vec(),
        text: String::from_utf8_lossy(&bytes).into_owned(),
        provenance: Provenance { epoch, generation },
    }
}

fn execute_backlinks(session: &Arc<Session>, path: &[String]) -> ContextQueryResult {
    let (epoch, generation) = match provenance(session) {
        Ok(values) => values,
        Err(message) => return ContextQueryResult::Failed { message },
    };
    let target_raw = path.last().map(String::as_str).unwrap_or_default();
    let stem = target_raw.strip_suffix(".md").unwrap_or(target_raw);
    let target_key = path.join("/");
    let resolution = session.resolve_target(target_key, stem.to_owned(), false, None);
    let note_ref = match resolution {
        Ok(resolution) if resolution.is_ambiguous => {
            return ContextQueryResult::Failed {
                message: "backlink target resolves to more than one note".to_owned(),
            };
        }
        Ok(resolution) => match resolution.note {
            Some(note) => note,
            None => {
                return ContextQueryResult::Failed {
                    message: "backlink target is not a resolved note".to_owned(),
                };
            }
        },
        Err(message) => return ContextQueryResult::Failed { message },
    };
    let page = match session.search_notes(Query::LinksTo(note_ref), MAX_BACKLINK_ROWS) {
        Ok(page) => page,
        Err(message) => return ContextQueryResult::Failed { message },
    };
    let target_stem = stem.to_owned();
    let links = page
        .items
        .into_iter()
        .map(|(_note_id, source_path, source_title)| {
            let excerpt = session
                .read_saved_note(&session.root().join(&source_path))
                .ok()
                .flatten()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .and_then(|text| link_excerpt(&text, &target_stem));
            BacklinkRow {
                source_path,
                source_title,
                excerpt,
            }
        })
        .collect();
    ContextQueryResult::Backlinks {
        path: path.to_vec(),
        links,
        provenance: Provenance { epoch, generation },
    }
}

/// The first line of `text` containing a link occurrence whose target
/// mentions `stem`, found by the one shared scanner (never a second,
/// weaker parser). Excerpts are presentation-only; identity is the path.
fn link_excerpt(text: &str, stem: &str) -> Option<String> {
    let parsed = flavite_syntax::pulldown_glue::parse_note(text.as_bytes()).ok()?;
    let link = parsed.links.iter().find(|link| {
        link.target_raw.contains(stem)
            || link
                .alias
                .as_deref()
                .is_some_and(|alias| alias.contains(stem))
    })?;
    let line_start = text[..link.span.start.0]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let line_end = text[link.span.end.0..]
        .find('\n')
        .map_or(text.len(), |index| link.span.end.0 + index);
    Some(text[line_start..line_end].trim_end().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_excerpt_finds_the_first_mentioning_link_line() {
        let text =
            "intro\nsee [[Other]] first\nthen [[Alpha target]] and [[Alpha target|alias]]\ntail";
        assert_eq!(
            link_excerpt(text, "Alpha target"),
            Some("then [[Alpha target]] and [[Alpha target|alias]]".to_owned())
        );
        assert_eq!(link_excerpt(text, "Missing"), None);
    }
}
