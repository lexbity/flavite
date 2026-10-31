//! Bounded asynchronous note/backlink query service (RFC-0003 §10, W04).
//!
//! The workspace submits context queries with request tokens; bounded
//! workers execute saved-content reads and structural index queries off the
//! UI thread and publish typed results into slots. Slots are bounded, late
//! or superseded sequences never overwrite newer ones, and no SQLite,
//! index, or vault types cross this boundary.

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
    /// Inspect one exact current connection occurrence before a removal decision.
    Occurrence {
        epoch: u64,
        occurrence: crate::LinkOccurrence,
    },
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
    /// This source is a current unsaved replacement of its saved outgoing links.
    pub unsaved: bool,
    /// Number of current directed occurrences from this source.
    pub occurrences: usize,
}

/// Typed outcome of one query slot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextQueryResult {
    /// A scanner-validated occurrence with normalized positions and exact bytes.
    Occurrence(OccurrenceDetail),
    /// Current open-buffer preview, clearly distinguished from saved text.
    BufferPreview {
        path: Vec<String>,
        text: String,
        buffer_id: u64,
        revision: u64,
        unsaved: bool,
    },
    /// A newer source/catalog projection is pending; stale results are not current.
    Updating,

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

/// Read-only chooser data, never a write capability. Commit still requires a
/// revisioned semantic plan, write authority, and current target validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceDetail {
    pub occurrence: crate::LinkOccurrence,
    pub epoch: u64,
    pub start: usize,
    pub end: usize,
    pub exact_source: String,
    pub excerpt: String,
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
    graph: Arc<arc_swap::ArcSwap<crate::GraphService>>,
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
    pub(crate) fn new(
        session: Arc<Session>,
        session_nonce: u128,
        graph: Arc<crate::GraphService>,
    ) -> Self {
        let graph = Arc::new(arc_swap::ArcSwap::from(graph));
        let (jobs, receiver) = crossbeam::channel::bounded::<Job>(MAX_QUERY_SLOTS);
        let ready: Arc<Mutex<BTreeMap<RequestToken, (QueryPayload, ContextQueryResult)>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::new();
        for worker_index in 0..2 {
            let receiver = receiver.clone();
            let session = Arc::clone(&session);
            let graph = Arc::clone(&graph);
            let ready = Arc::clone(&ready);
            let in_flight = Arc::clone(&in_flight);
            workers.push(
                std::thread::Builder::new()
                    .name(format!("flavite-query-{worker_index}"))
                    .spawn(move || {
                        while let Ok(job) = receiver.recv() {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                    execute(&session, &graph.load_full(), &job.payload)
                                }))
                                .unwrap_or(
                                    ContextQueryResult::Failed {
                                        message: "Context worker failed; retry the query".into(),
                                    },
                                );
                            publish(&ready, &in_flight, job, result);
                        }
                    })
                    .expect("query worker thread creation failed"),
            );
        }
        Self {
            jobs,
            graph,
            ready,
            in_flight,
            session_nonce,
            request_sequence: AtomicU64::new(0),
            _workers: workers,
        }
    }

    pub(crate) fn set_graph(&self, graph: Arc<crate::GraphService>) {
        self.graph.store(graph);
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
        if self
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_QUERY_SLOTS).then_some(count + 1)
            })
            .is_err()
        {
            return Err(QuerySubmitError::Busy);
        }
        let token = RequestToken {
            session_nonce: self.session_nonce,
            context_tab,
            incarnation,
            binding_sequence,
            request_sequence: self.request_sequence.fetch_add(1, Ordering::AcqRel),
        };
        if self.jobs.try_send(Job { token, payload }).is_err() {
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
        let drained = std::mem::take(&mut *ready);
        self.in_flight.fetch_sub(drained.len(), Ordering::AcqRel);
        drained
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
            if slots.remove(&token).is_some() {
                in_flight.fetch_sub(1, Ordering::AcqRel);
            }
        }
        slots.insert(job.token, (job.payload, result));
    } else {
        in_flight.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Execute one query against the Session read surface.
fn execute(
    session: &Arc<Session>,
    graph: &Arc<crate::GraphService>,
    payload: &QueryPayload,
) -> ContextQueryResult {
    match payload {
        QueryPayload::Occurrence { epoch, occurrence } => {
            inspect_occurrence(session, graph, *epoch, occurrence)
        }
        QueryPayload::Preview { path } => execute_preview(session, path),
        QueryPayload::Backlinks { path } => execute_backlinks(session, graph, path),
    }
}

fn inspect_occurrence(
    session: &Arc<Session>,
    graph: &Arc<crate::GraphService>,
    epoch: u64,
    occurrence: &crate::LinkOccurrence,
) -> ContextQueryResult {
    let failure = || ContextQueryResult::Failed {
        message: "Connection changed; refresh the occurrence chooser".into(),
    };
    let Some(frame) = graph.frame() else {
        return failure();
    };
    if frame.projection().saved.epoch != epoch
        || frame.projection().buffer_stamp != session.graph_buffer_stamp()
        || !frame.projection().occurrences.contains(occurrence)
    {
        return failure();
    }
    let content = match &occurrence.source {
        crate::OccurrenceSource::Buffer {
            buffer_id,
            revision,
        } => session
            .shared_buffer_snapshot(
                (*buffer_id).into(),
                Some(flavite_buffer::Revision::new(*revision)),
            )
            .ok()
            .map(|s| (s.text.to_string(), occurrence.start, occurrence.end)),
        crate::OccurrenceSource::Saved { content_hash } => session
            .read_saved_note(&session.root().join(&occurrence.source_path))
            .ok()
            .flatten()
            .filter(|bytes| blake3::hash(bytes).as_bytes().as_slice() == content_hash)
            .and_then(|bytes| {
                let snapshot = flavite_syntax::text::TextSnapshot::new(&bytes).ok()?;
                let start = snapshot
                    .source_to_text(flavite_syntax::text::SourceByte(occurrence.start))?
                    .0;
                let end = snapshot
                    .source_to_text(flavite_syntax::text::SourceByte(occurrence.end))?
                    .0;
                Some((snapshot.text().to_owned(), start, end))
            }),
    };
    let Some((text, start, end)) = content else {
        return failure();
    };
    let Ok(parsed) = flavite_syntax::pulldown_glue::parse_note(text.as_bytes()) else {
        return failure();
    };
    if !parsed.links.iter().any(|link| {
        link.span.start.0 == start
            && link.span.end.0 == end
            && link.target_raw == occurrence.target_raw
    }) {
        return failure();
    }
    let Some(exact_source) = text.get(start..end) else {
        return failure();
    };
    if exact_source.len() > 8192 {
        return ContextQueryResult::Failed {
            message: "Connection syntax exceeds the 8 KiB edit limit; edit source".into(),
        };
    }
    ContextQueryResult::Occurrence(OccurrenceDetail {
        occurrence: occurrence.clone(),
        epoch,
        start,
        end,
        exact_source: exact_source.into(),
        excerpt: exact_excerpt(&text, start, end).unwrap_or_default(),
    })
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
    let relative: PathBuf = path.iter().collect();
    if let Some((id, revision, ..)) = session
        .graph_buffer_stamp()
        .into_iter()
        .find(|(_, _, open, ..)| open.strip_prefix(session.root()).unwrap_or(open) == relative)
        && let Ok(snapshot) = session.shared_buffer_snapshot(id, Some(revision))
    {
        return ContextQueryResult::BufferPreview {
            path: path.to_vec(),
            text: snapshot.text.to_string(),
            buffer_id: id.as_u64(),
            revision: revision.as_u64(),
            unsaved: snapshot.state != flavite_buffer::state::BufferState::Clean,
        };
    }
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

fn execute_backlinks(
    session: &Arc<Session>,
    graph: &Arc<crate::GraphService>,
    path: &[String],
) -> ContextQueryResult {
    let Some(frame) = graph.frame() else {
        return ContextQueryResult::Updating;
    };
    let projection = frame.projection();
    if projection.buffer_stamp != session.graph_buffer_stamp() {
        return ContextQueryResult::Updating;
    }
    if projection
        .overlays
        .values()
        .any(|overlay| matches!(overlay, crate::OverlayState::Failed { .. }))
    {
        return ContextQueryResult::Failed {
            message: "Current backlinks unavailable: fix the source parse error".into(),
        };
    }
    let relative: PathBuf = path.iter().collect();
    let Some(target) = projection
        .effective
        .nodes
        .iter()
        .find(|n| n.native_path == relative)
    else {
        return ContextQueryResult::Missing;
    };
    let mut by_source: BTreeMap<flavite_graph::NodeId, Vec<&crate::LinkOccurrence>> =
        BTreeMap::new();
    for occurrence in projection
        .occurrences
        .iter()
        .filter(|o| o.edge.to == target.id)
    {
        by_source
            .entry(occurrence.edge.from)
            .or_default()
            .push(occurrence);
    }
    let mut links = Vec::new();
    for (source, occurrences) in by_source.into_iter().take(MAX_BACKLINK_ROWS) {
        let Some(record) = projection.effective.nodes.iter().find(|r| r.id == source) else {
            continue;
        };
        let occurrence = occurrences[0];
        let unsaved = matches!(occurrence.source, crate::OccurrenceSource::Buffer { .. });
        let excerpt = match &occurrence.source {
            crate::OccurrenceSource::Buffer {
                buffer_id,
                revision,
            } => session
                .shared_buffer_snapshot(
                    (*buffer_id).into(),
                    Some(flavite_buffer::Revision::new(*revision)),
                )
                .ok()
                .and_then(|s| exact_excerpt(&s.text, occurrence.start, occurrence.end)),
            crate::OccurrenceSource::Saved { content_hash } => session
                .read_saved_note(&session.root().join(&occurrence.source_path))
                .ok()
                .flatten()
                .filter(|bytes| blake3::hash(bytes).as_bytes().as_slice() == content_hash)
                .and_then(|bytes| {
                    let snapshot = flavite_syntax::text::TextSnapshot::new(&bytes).ok()?;
                    let start = snapshot
                        .source_to_text(flavite_syntax::text::SourceByte(occurrence.start))?
                        .0;
                    let end = snapshot
                        .source_to_text(flavite_syntax::text::SourceByte(occurrence.end))?
                        .0;
                    exact_excerpt(snapshot.text(), start, end)
                }),
        };
        links.push(BacklinkRow {
            source_path: record.path.clone(),
            source_title: record.title.clone(),
            excerpt,
            unsaved,
            occurrences: occurrences.len(),
        });
    }
    ContextQueryResult::Backlinks {
        path: path.to_vec(),
        links,
        provenance: Provenance {
            epoch: projection.saved.epoch,
            generation: projection.saved.generation,
        },
    }
}

fn exact_excerpt(text: &str, start: usize, end: usize) -> Option<String> {
    text.get(start..end)?;
    let line_start = text[..start].rfind('\n').map_or(0, |index| index + 1);
    let line_end = text[end..]
        .find('\n')
        .map_or(text.len(), |index| end + index);
    Some(text[line_start..line_end].trim_end().to_owned())
}

/// The first line of `text` containing a link occurrence whose target
/// mentions `stem`, found by the one shared scanner (never a second,
/// weaker parser). Excerpts are presentation-only; identity is the path.
#[cfg(test)]
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
