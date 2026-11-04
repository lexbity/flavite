//! Per-vault graph base service (RFC-0003 TD-12, W07, §10.1, §12.1;
//! RFC-0004 S6 incremental bridge).
//!
//! One service per vault, one bounded worker thread: it owns the *only*
//! database handle on the graph path, polls the index at a fixed cadence
//! plus host-sent invalidation hints, and advances a [`GraphMirror`] from
//! the typed change feed — reading only changed entities — then publishes
//! immutable [`GraphFrame`]s the UI consumes without ever waiting (D15:
//! the frame path is an atomic pointer load). The SQLite-backed feed bridge
//! lives here — below the facade boundary, never inside `flavite-graph` and
//! never exposed as a connection handle to the UI (§5.1).
//!
//! Publications stay *complete* (D7): the mirror emits a full [`GraphBatch`]
//! per bump and [`GraphPublisher`] still diffs consecutive complete sets, so
//! the dense node-record alignment invariant is untouched. Deletions survive
//! because the change feed logs every re-resolution flip (FR-1), and the
//! mirror's periodic parity guard (§6.5.4) rebuilds from the full consistent
//! read on any drift.
//!
//! A changed epoch (entity ids may be reused) rebuilds the model and
//! re-installs the layout worker instead of delta-feeding through the reset
//! (§4.14).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwapOption;
use crossbeam::channel::{Receiver, Sender, bounded};
use flavite_graph::PositionFrame;
use flavite_graph::{
    Delta, EdgeKind, GraphBatch, GraphEdgeRecord, GraphNodeRecord, GraphPublication,
    GraphPublisher, LayoutCommand, LayoutWorker, LayoutWorkerConfig, NodeId, PositionBuffer,
    RenderKey,
};
use flavite_index::GenerationWatch;
use rusqlite::{Connection, OpenFlags};

use crate::graph_mirror::{GraphMirror, MirrorStats, ParityOutcome, ParityPolicy, Synchronize};
use crate::graph_projection::{GraphProjection, GraphViewToken, ScopeRequest};
use crate::{Flavite, FlaviteError, StoredUserState};
use flavite_vault::session::Session;
use std::collections::BTreeMap;

/// Base feed poll cadence (§12.1: "One service per vault, 250 ms poll plus
/// invalidation hints").
pub const GRAPH_POLL_INTERVAL: Duration = Duration::from_millis(250);

fn workspace_layout_config() -> LayoutWorkerConfig {
    LayoutWorkerConfig {
        // Workspace dots have a 4–12 world-unit radius. The algorithm's
        // unit-scale default springs otherwise settle connected notes on top
        // of each other, making separate hits and labels impossible.
        params: flavite_graph::LayoutParams {
            repulsion: 20_000.0,
            ..Default::default()
        },
        min_step_interval: Duration::from_millis(16),
        ..LayoutWorkerConfig::default()
    }
}

/// Bounded hint queue: a saturated service coalesces into the next poll
/// rather than growing an unbounded backlog (§12.1 typed Busy).
const HINT_QUEUE_CAPACITY: usize = 8;

/// Why the service thread must poll right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Directive {
    /// Poll the bridge on the next loop pass.
    Poll,
    /// Re-install the layout worker from the current publication (explicit
    /// "Retry layout" after a worker panic, §12.1 restart policy).
    Relayout,
    #[cfg(test)]
    Panic,
}

/// One immutable base frame: the saved-index publication plus the position
/// buffer its topology feeds. Readers pair frames by [`RenderKey`] and load
/// positions atomically — the frame hand-off never blocks either side (D15).
#[derive(Debug)]
pub struct GraphFrame {
    publication: Arc<GraphPublication>,
    positions: Arc<PositionBuffer>,
    projection: Arc<GraphProjection>,
}

impl GraphFrame {
    /// Current relationships and per-view scope results at the frame version.
    pub fn projection(&self) -> &Arc<GraphProjection> {
        &self.projection
    }

    /// The effective publication, including revisioned open-buffer replacements.
    #[must_use]
    pub fn publication(&self) -> &Arc<GraphPublication> {
        &self.publication
    }

    /// The shared position buffer. Load the newest frame; the caller keeps
    /// a consistent `Arc` even while the worker publishes underneath.
    #[must_use]
    pub fn positions(&self) -> Arc<PositionFrame> {
        self.positions.load()
    }

    /// The key every per-view component must agree with (§10.3).
    #[must_use]
    pub fn render_key(&self) -> RenderKey {
        self.publication
            .render_key()
            .with_view(self.projection.digest, 0)
    }
}

/// The honest service phase the graph view renders (§13.4 loading states).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphServiceStatus {
    /// The index cache does not exist yet; the vault is still being
    /// indexed. The graph renders a loading state, not an empty network.
    Indexing {
        /// Human-readable wait reason (redacted, never path-bearing).
        reason: String,
    },
    /// Base frames are publishing live.
    Live,
    /// The bridge failed; the last good frame remains visible and read-
    /// only (insertion stays disabled until validation is current, §10.3).
    Failed {
        /// Typed failure surface (redacted).
        error: String,
    },
}

/// Bounded fixed-cardinality counters (§13.4 metrics; no per-note labels).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GraphServiceMetrics {
    /// Poll passes executed.
    pub polls_total: u64,
    /// Complete batches applied.
    pub batches_total: u64,
    /// Full-model rebuilds (first read and epoch changes).
    pub resets_total: u64,
    /// Stale/misaligned batches discarded without mutation.
    pub stale_dropped_total: u64,
    /// Bridge errors recorded since service start.
    pub bridge_errors_total: u64,
    /// Layout worker (re)spawns.
    pub worker_restarts_total: u64,
    /// Mirror delta synchronizations (RFC-0004 §6.5.5).
    pub mirror_deltas_total: u64,
    /// Mirror full rebuilds (epoch change, floor overrun, parity failure).
    pub mirror_resets_total: u64,
    /// Mirror parity comparisons executed.
    pub mirror_parity_checks_total: u64,
    /// Mirror parity mismatches (each forced or forces a rebuild).
    pub mirror_mismatches_total: u64,
    /// Deduplicated entities touched by the last mirror delta (last value).
    pub mirror_last_delta_entities: u64,
}

#[derive(Debug)]
struct Shared {
    frame: ArcSwapOption<GraphFrame>,
    status: std::sync::Mutex<GraphServiceStatus>,
    counters: Counters,
    scopes: std::sync::Mutex<BTreeMap<u128, ScopeRequest>>,
    retry_layout: AtomicBool,
    /// The saved-graph mirror (RFC-0004 S6). Locked by the service pump and
    /// by the on-demand doctor parity check; both are read-mostly and the
    /// critical sections are bounded reads on the graph connection.
    mirror: std::sync::Mutex<GraphMirror>,
    /// RFC-0005 S1: the watch half of the writer-side generation broadcast
    /// (when the host constructed the vault with a signal). Stored per
    /// service so S5's event loop can wake on it; until then the pump
    /// ignores it and freshness remains the 250 ms cadence + hint path —
    /// this field's presence must not change any pump behavior.
    #[allow(
        dead_code,
        reason = "wired in RFC-0005 S1 (constructor seam); the S5 event loop is its first consumer"
    )]
    index_watch: Option<Arc<GenerationWatch>>,
}

#[derive(Debug, Default)]
struct Counters {
    polls: AtomicU64,
    batches: AtomicU64,
    resets: AtomicU64,
    stale_dropped: AtomicU64,
    bridge_errors: AtomicU64,
    worker_restarts: AtomicU64,
    mirror_deltas: AtomicU64,
    mirror_resets: AtomicU64,
    mirror_parity_checks: AtomicU64,
    mirror_mismatches: AtomicU64,
    mirror_last_delta_entities: AtomicU64,
}

impl Counters {
    /// Publish one fixed-cardinality mirror counter set (§6.5.5). Called by
    /// whoever last drove the mirror (the pump or the doctor check) so the
    /// metrics always reflect the freshest mirror verdict.
    fn publish_mirror(&self, stats: MirrorStats) {
        self.mirror_deltas
            .store(stats.deltas_total, Ordering::Relaxed);
        self.mirror_resets
            .store(stats.resets_total, Ordering::Relaxed);
        self.mirror_parity_checks
            .store(stats.parity_checks_total, Ordering::Relaxed);
        self.mirror_mismatches
            .store(stats.mismatches_total, Ordering::Relaxed);
        self.mirror_last_delta_entities
            .store(stats.last_delta_entities, Ordering::Relaxed);
    }
}

/// The graph base service for one vault.
///
/// Dropping the last handle (or calling [`Self::shutdown`]) stops the
/// service thread within one poll interval; the thread releases its
/// database connection and shuts its layout worker down on exit.
#[derive(Debug)]
pub struct GraphService {
    shared: Arc<Shared>,
    hints: Sender<Directive>,
    stop: Arc<AtomicBool>,
    db_path: PathBuf,
    thread: std::sync::Mutex<Option<thread::JoinHandle<()>>>,
}

impl GraphService {
    /// Start the service against the vault's cache database path.
    ///
    /// A missing database file is a normal wait state (vault not yet
    /// reconciled): status stays [`GraphServiceStatus::Indexing`] and every
    /// poll retries, so the graph becomes live the moment the index exists
    /// — no startup race in the host.
    #[must_use]
    pub fn start(db_path: PathBuf) -> Arc<Self> {
        Self::start_with_session(db_path, None)
    }

    pub(crate) fn start_with_session(db_path: PathBuf, session: Option<Arc<Session>>) -> Arc<Self> {
        Self::start_with_policy(db_path, session, ParityPolicy::default(), None)
    }

    /// RFC-0005 S1 seam: start the service holding the generation broadcast's
    /// watch. The pump does not consume it yet (S5 rewrites the wake
    /// mechanics); until then freshness behavior is byte-identical to
    /// [`Self::start_with_session`].
    pub(crate) fn start_with_index_watch(
        db_path: PathBuf,
        session: Option<Arc<Session>>,
        index_watch: Option<Arc<GenerationWatch>>,
    ) -> Arc<Self> {
        Self::start_with_policy(db_path, session, ParityPolicy::default(), index_watch)
    }

    /// Test/bench seam: same service with a tightened mirror drift guard
    /// (RFC-0004 §6.5.4 intervals stay fixed in production).
    #[doc(hidden)]
    #[must_use]
    pub fn start_with_parity_policy(
        db_path: PathBuf,
        session: Option<Arc<Session>>,
        policy: ParityPolicy,
    ) -> Arc<Self> {
        Self::start_with_policy(db_path, session, policy, None)
    }

    fn start_with_policy(
        db_path: PathBuf,
        session: Option<Arc<Session>>,
        policy: ParityPolicy,
        index_watch: Option<Arc<GenerationWatch>>,
    ) -> Arc<Self> {
        let shared = Arc::new(Shared {
            frame: ArcSwapOption::empty(),
            status: std::sync::Mutex::new(GraphServiceStatus::Indexing {
                reason: "waiting for the index cache".into(),
            }),
            counters: Counters::default(),
            scopes: std::sync::Mutex::new(BTreeMap::new()),
            retry_layout: AtomicBool::new(false),
            mirror: std::sync::Mutex::new(GraphMirror::with_parity_policy(session.clone(), policy)),
            index_watch,
        });
        let (hints, hint_rx) = bounded::<Directive>(HINT_QUEUE_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));
        let thread_shared = Arc::clone(&shared);
        let thread_stop = Arc::clone(&stop);
        let thread_db = db_path.clone();
        let spawned = thread::Builder::new()
            .name("flavite-graph-feed".into())
            .spawn(move || {
                run_service(
                    &thread_db,
                    &thread_shared,
                    &hint_rx,
                    &thread_stop,
                    session.as_ref(),
                )
            });
        let thread = match spawned {
            Ok(handle) => Some(handle),
            Err(_) => {
                // A thread-spawn failure is a typed unavailable service,
                // never a panic carried into the host constructor.
                set_status(
                    &shared,
                    GraphServiceStatus::Failed {
                        error: "graph feed worker could not start".into(),
                    },
                );
                None
            }
        };
        Arc::new(Self {
            shared,
            hints,
            stop,
            db_path,
            thread: std::sync::Mutex::new(thread),
        })
    }

    /// Register or replace a bounded view scope. A late request cannot regress
    /// an incarnation or sequence; the worker publishes the same token.
    pub fn request_scope(&self, request: ScopeRequest) -> Result<(), &'static str> {
        request.scope.validate()?;
        let mut scopes = self
            .shared
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(old) = scopes.get(&request.token.tab) {
            if (old.token.incarnation, old.token.sequence)
                > (request.token.incarnation, request.token.sequence)
            {
                return Err("Superseded graph scope");
            }
            if old.token == request.token {
                return if old == &request {
                    Ok(())
                } else {
                    Err("Scope token reused with different parameters")
                };
            }
        }
        if !scopes.contains_key(&request.token.tab) && scopes.len() >= 128 {
            return Err("Graph scopes busy");
        }
        scopes.insert(request.token.tab, request);
        drop(scopes);
        self.notify_changed();
        Ok(())
    }

    pub fn release_scope(&self, tab: u128) {
        self.shared
            .scopes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&tab);
    }

    /// The newest base frame, if any has been published.
    #[must_use]
    pub fn frame(&self) -> Option<Arc<GraphFrame>> {
        self.shared.frame.load_full()
    }

    /// Current service status (drives honest loading/error states).
    #[must_use]
    pub fn status(&self) -> GraphServiceStatus {
        self.shared
            .status
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Fixed-cardinality counters for diagnostics (§13.4).
    #[must_use]
    pub fn metrics(&self) -> GraphServiceMetrics {
        let counters = &self.shared.counters;
        GraphServiceMetrics {
            polls_total: counters.polls.load(Ordering::Relaxed),
            batches_total: counters.batches.load(Ordering::Relaxed),
            resets_total: counters.resets.load(Ordering::Relaxed),
            stale_dropped_total: counters.stale_dropped.load(Ordering::Relaxed),
            bridge_errors_total: counters.bridge_errors.load(Ordering::Relaxed),
            worker_restarts_total: counters.worker_restarts.load(Ordering::Relaxed),
            mirror_deltas_total: counters.mirror_deltas.load(Ordering::Relaxed),
            mirror_resets_total: counters.mirror_resets.load(Ordering::Relaxed),
            mirror_parity_checks_total: counters.mirror_parity_checks.load(Ordering::Relaxed),
            mirror_mismatches_total: counters.mirror_mismatches.load(Ordering::Relaxed),
            mirror_last_delta_entities: counters.mirror_last_delta_entities.load(Ordering::Relaxed),
        }
    }

    /// On-demand read-only mirror parity (RFC-0004 §6.5.4, doctor
    /// `graph-mirror-parity`): lock the mirror, advance it to the current
    /// index version if the pump has not yet, and compare against a fresh
    /// full consistent read. This never writes and never forces a rebuild —
    /// the service's own scheduled guard does that; findings are returned
    /// for the doctor to surface.
    ///
    /// # Errors
    /// Returns the failure text when the full read or a catch-up fails.
    pub fn check_mirror_parity(&self, conn: &Connection) -> Result<ParityOutcome, String> {
        let mut mirror = self
            .shared
            .mirror
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome = mirror.parity_check(conn)?;
        self.shared.counters.publish_mirror(mirror.stats());
        Ok(outcome)
    }

    /// Wake the poller after a vault-changed pulse (§12.1 invalidation
    /// hints). A full queue coalesces into the next scheduled poll — the
    /// cadence bounds the work either way.
    pub fn notify_changed(&self) {
        let _ = self.hints.try_send(Directive::Poll);
    }

    /// Re-install the layout worker from the current publication (worker
    /// panic recovery and the explicit "Retry layout" control).
    pub fn relayout(&self) {
        self.shared.retry_layout.store(true, Ordering::Release);
        let _ = self.hints.try_send(Directive::Relayout);
    }

    /// The database path this service reads (diagnostics only; no handle
    /// ever escapes).
    #[must_use]
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Ask the thread to stop and join it. Idempotent.
    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.hints.try_send(Directive::Poll);
        if let Some(handle) = self
            .thread
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = handle.join();
        }
    }
}

impl Drop for GraphService {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.hints.try_send(Directive::Poll);
        // The thread is joined opportunistically by `shutdown`; dropping
        // without an explicit join still ends it within one poll interval.
    }
}

/// The service thread body: bridge + publisher + layout worker.
///
/// A panic here becomes a typed [`GraphServiceStatus::Failed`] and the
/// thread stops; the last good frame remains readable (§4.2).
fn run_service(
    db_path: &Path,
    shared: &Arc<Shared>,
    hint_rx: &Receiver<Directive>,
    stop: &AtomicBool,
    session: Option<&Arc<Session>>,
) {
    let mut restarts = std::collections::VecDeque::new();
    while !stop.load(Ordering::Acquire) {
        if !run_service_attempt(db_path, shared, hint_rx, stop, session) {
            break;
        }
        let now = std::time::Instant::now();
        restarts.retain(|at| now.duration_since(*at) < Duration::from_secs(60));
        if restarts.len() >= 3 {
            set_status(
                shared,
                GraphServiceStatus::Failed {
                    error: "Graph worker repeatedly failed; choose Retry service".into(),
                },
            );
            loop {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                if shared.retry_layout.swap(false, Ordering::AcqRel) {
                    restarts.clear();
                    break;
                }
                match hint_rx.recv_timeout(GRAPH_POLL_INTERVAL) {
                    Ok(Directive::Relayout) => {}
                    Err(crossbeam::channel::RecvTimeoutError::Disconnected) => return,
                    _ => {}
                }
            }
        }
        restarts.push_back(std::time::Instant::now());
        shared
            .counters
            .worker_restarts
            .fetch_add(1, Ordering::Relaxed);
    }
}

fn run_service_attempt(
    db_path: &Path,
    shared: &Arc<Shared>,
    hint_rx: &Receiver<Directive>,
    stop: &AtomicBool,
    session: Option<&Arc<Session>>,
) -> bool {
    let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut publisher = GraphPublisher::new();
        let mut layout = LayoutState::default();
        let mut last_signature = None;
        while !stop.load(Ordering::Acquire) {
            match hint_rx.recv_timeout(GRAPH_POLL_INTERVAL) {
                #[cfg(test)]
                Ok(Directive::Panic) => panic!("injected graph worker failure"),
                Err(crossbeam::channel::RecvTimeoutError::Disconnected) => break,
                _ => {}
            }
            if shared.retry_layout.swap(false, Ordering::AcqRel) {
                layout.relayout(shared);
                last_signature = None;
            }
            if stop.load(Ordering::Acquire) {
                break;
            }
            shared.counters.polls.fetch_add(1, Ordering::Relaxed);
            let scopes = shared
                .scopes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let read = (|| -> Result<Option<(Arc<GraphProjection>, bool)>, String> {
                if !db_path.exists() {
                    return Err("Index not available yet".to_owned());
                }
                let mut connection =
                    Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                        .map_err(|_| "Graph index unavailable")?;
                connection
                    .busy_timeout(Duration::from_millis(50))
                    .map_err(|_| "Graph index unavailable")?;
                let previous = shared.frame.load_full();
                // RFC-0004 S6: advance the mirror by the change feed, run
                // the §6.5.4 drift guard when due, and hand the complete
                // canonical view to the projection builder. `force_reset`
                // says the publisher must rebuild the model (§6.5.2).
                let mut mirror = shared
                    .mirror
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let mut force_reset = false;
                match mirror.synchronize(&connection)? {
                    Synchronize::UpToDate => {}
                    Synchronize::Delta { reset, .. } => {
                        force_reset = reset;
                    }
                    Synchronize::Reset { reason } => {
                        tracing::info!(
                            target: "flavite_api::graph_service",
                            ?reason,
                            "graph mirror rebuilding from a full consistent read"
                        );
                        mirror.force_reset(&connection)?;
                        force_reset = true;
                    }
                }
                // Poisoned delta machinery — a failed splice or a
                // contract-violating row: never publish suspect output;
                // rebuild from the full truth and reset the model in this
                // same pump (§4.7).
                if mirror.is_poisoned() {
                    mirror.force_reset(&connection)?;
                    force_reset = true;
                } else if mirror.parity_due(std::time::Instant::now()) {
                    // Parity guard (D8, §6.5.4): counter-driven, inside
                    // this loop — no dedicated thread. A mismatch rebuilds
                    // the view before the projection is built, so the
                    // drifted state never reaches a published frame.
                    let parity = mirror.parity_check(&connection)?;
                    if !parity.matched() {
                        tracing::warn!(
                            target: "flavite_api::graph_service",
                            epoch = parity.version.epoch,
                            generation = parity.version.generation,
                            findings = ?parity.findings,
                            "graph mirror drifted from the full consistent read; rebuilding"
                        );
                        mirror.force_reset(&connection)?;
                        force_reset = true;
                    }
                }
                shared.counters.publish_mirror(mirror.stats());
                let graph = mirror.view().ok_or("graph mirror holds no view")?;
                crate::graph_projection::build(
                    &mut connection,
                    graph,
                    session,
                    &scopes,
                    previous.as_ref().map(|frame| frame.projection.as_ref()),
                )
                .map(|projection| projection.map(|projection| (Arc::new(projection), force_reset)))
            })();
            match read {
                Ok(None) => {
                    if let Some(frame) = shared.frame.load_full()
                        && layout.drive(frame.publication(), shared)
                        && let Some(positions) = &layout.positions
                        && !Arc::ptr_eq(positions, &frame.positions)
                    {
                        shared.frame.store(Some(Arc::new(GraphFrame {
                            publication: frame.publication.clone(),
                            projection: frame.projection.clone(),
                            positions: positions.clone(),
                        })));
                    }
                    set_status(shared, GraphServiceStatus::Live);
                }
                Ok(Some((projection, force_reset))) => {
                    if force_reset {
                        // A rebuild of the saved view at an already
                        // published version (floor overrun, parity repair)
                        // must re-publish: drop the dedup so the corrected
                        // frame replaces the drifted one (§6.5.4).
                        last_signature = None;
                    }
                    let signature = (
                        projection.effective.epoch,
                        projection.effective.generation,
                        projection.digest,
                        scopes,
                    );
                    if last_signature.as_ref() == Some(&signature) {
                        continue;
                    }
                    let reset = force_reset
                        || !publisher.initialized()
                        || publisher.epoch() != projection.effective.epoch;
                    let publication =
                        publisher.apply_current(&projection.effective, reset, projection.digest);
                    shared
                        .counters
                        .resets
                        .store(publisher.resets_total, Ordering::Relaxed);
                    shared
                        .counters
                        .stale_dropped
                        .store(publisher.stale_dropped_total, Ordering::Relaxed);
                    if layout.drive(publication.as_ref(), shared)
                        && let Some(buffer) = layout.positions.clone()
                    {
                        shared.frame.store(Some(Arc::new(GraphFrame {
                            publication,
                            positions: buffer,
                            projection,
                        })));
                        shared.counters.batches.fetch_add(1, Ordering::Relaxed);
                        set_status(shared, GraphServiceStatus::Live);
                        last_signature = Some(signature);
                    }
                }
                Err(error) => {
                    shared
                        .counters
                        .bridge_errors
                        .fetch_add(1, Ordering::Relaxed);
                    set_status(
                        shared,
                        if shared.frame.load().is_none() {
                            GraphServiceStatus::Indexing { reason: error }
                        } else {
                            GraphServiceStatus::Failed { error }
                        },
                    );
                }
            }
        }
        layout.shutdown();
    }));
    if guarded.is_err() {
        set_status(
            shared,
            GraphServiceStatus::Failed {
                error: "graph feed worker panicked; retry layout from the graph view".into(),
            },
        );
        shared
            .counters
            .bridge_errors
            .fetch_add(1, Ordering::Relaxed);
    }
    guarded.is_err()
}

/// The service's layout-worker ownership (spawn on demand, replace on
/// reset, restart on death).
#[derive(Default)]
struct LayoutState {
    worker: Option<LayoutWorker>,
    positions: Option<Arc<PositionBuffer>>,
    last_topology: Option<(u64, u64)>,
}

impl LayoutState {
    /// Drive the worker with one publication. Returns whether a frame is
    /// ready to be published (worker alive + positions installed).
    fn drive(&mut self, publication: &GraphPublication, shared: &Arc<Shared>) -> bool {
        let topology = (publication.epoch, publication.topology_version);
        if self.last_topology == Some(topology)
            && self.worker.as_ref().is_some_and(LayoutWorker::is_alive)
        {
            return true;
        }
        if publication.reset && self.worker.is_some() {
            self.shutdown();
            self.positions = None;
        }
        let dead = self
            .worker
            .as_ref()
            .is_some_and(|worker| !worker.is_alive());
        if self.worker.is_none() || dead {
            if dead {
                shared
                    .counters
                    .worker_restarts
                    .fetch_add(1, Ordering::Relaxed);
            }
            return match LayoutWorker::spawn(workspace_layout_config()) {
                Ok(worker) => {
                    let _ = worker.send(LayoutCommand::Replace(Arc::clone(&publication.snapshot)));
                    let _ = worker.send(LayoutCommand::Start);
                    self.positions = Some(worker.positions());
                    self.worker = Some(worker);
                    self.last_topology = Some(topology);
                    true
                }
                Err(error) => {
                    set_status(
                        shared,
                        GraphServiceStatus::Failed {
                            error: error.to_string(),
                        },
                    );
                    false
                }
            };
        }
        let Some(worker) = self.worker.as_ref() else {
            return false;
        };
        if publication.reset {
            let _ = worker.send(LayoutCommand::Replace(Arc::clone(&publication.snapshot)));
            let _ = worker.send(LayoutCommand::Start);
        } else if !publication.deltas.is_empty() {
            let deltas = publication.deltas.iter().copied().collect::<Vec<Delta>>();
            let _ = worker.send(LayoutCommand::Apply(deltas));
        }
        self.last_topology = Some(topology);
        true
    }

    fn relayout(&mut self, shared: &Arc<Shared>) {
        let Some(frame) = shared.frame.load_full() else {
            return;
        };
        if let Some(worker) = self.worker.as_ref().filter(|worker| worker.is_alive()) {
            let _ = worker.send(LayoutCommand::Replace(Arc::clone(
                &frame.publication().snapshot,
            )));
            let _ = worker.send(LayoutCommand::Start);
            return;
        }
        self.shutdown();
        shared
            .counters
            .worker_restarts
            .fetch_add(1, Ordering::Relaxed);
        if let Ok(worker) = LayoutWorker::spawn(workspace_layout_config()) {
            let _ = worker.send(LayoutCommand::Replace(frame.publication().snapshot.clone()));
            let _ = worker.send(LayoutCommand::Start);
            self.positions = Some(worker.positions());
            self.worker = Some(worker);
        }
    }

    fn shutdown(&mut self) {
        self.last_topology = None;
        if let Some(worker) = self.worker.take() {
            let _ = worker.shutdown();
        }
    }
}

fn set_status(shared: &Arc<Shared>, status: GraphServiceStatus) {
    let mut guard = shared
        .status
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *guard != status {
        *guard = status;
    }
}

// ---------------------------------------------------------------------------
// Index bridge (SQLite; this is the only graph-path owner of a handle)
// ---------------------------------------------------------------------------

/// Read the complete current node and edge sets inside one transaction.
pub(crate) fn read_current(
    transaction: &rusqlite::Transaction<'_>,
    epoch: u64,
    generation: u64,
) -> Result<GraphBatch, String> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    {
        let mut statement = transaction
            .prepare(
                "SELECT note_id, path, path_native, title, stable_id, id_conflict
                 FROM notes ORDER BY note_id",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                let path_native: Vec<u8> = row.get(2)?;
                let title: String = row.get(3)?;
                let stable_id: Option<String> = row.get(4)?;
                let conflict: i64 = row.get(5)?;
                Ok(GraphNodeRecord::note(
                    u64::try_from(id).unwrap_or(0),
                    path,
                    path_native,
                    title,
                    stable_id,
                    conflict != 0,
                ))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            nodes.push(row.map_err(|error| error.to_string())?);
        }
    }
    {
        let mut statement = transaction
            .prepare("SELECT att_id, path, path_native FROM attachments ORDER BY att_id")
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                let path: String = row.get(1)?;
                let path_native: Vec<u8> = row.get(2)?;
                Ok(GraphNodeRecord::attachment(
                    u64::try_from(id).unwrap_or(0),
                    path,
                    path_native,
                ))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            nodes.push(row.map_err(|error| error.to_string())?);
        }
    }
    {
        let mut statement = transaction
            .prepare(
                "SELECT src, kind, dst_note, dst_att
                 FROM links
                 WHERE dst_note IS NOT NULL OR dst_att IS NOT NULL",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                let src: i64 = row.get(0)?;
                let kind: String = row.get(1)?;
                let dst_note: Option<i64> = row.get(2)?;
                let dst_att: Option<i64> = row.get(3)?;
                Ok((src, kind, dst_note, dst_att))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            let (src, kind, dst_note, dst_att) = row.map_err(|error| error.to_string())?;
            let edge_kind = if kind == "link" {
                EdgeKind::Link
            } else {
                EdgeKind::Embed
            };
            let to = match (dst_note, dst_att) {
                (Some(note), _) => NodeId::note(u64::try_from(note).unwrap_or(0)),
                (None, Some(att)) => NodeId::attachment(u64::try_from(att).unwrap_or(0)),
                (None, None) => continue,
            };
            edges.push(GraphEdgeRecord {
                from: NodeId::note(u64::try_from(src).unwrap_or(0)),
                to,
                kind: edge_kind,
            });
        }
    }
    // Deduplicate parallel `(from, to, kind)` rows: two `[[B]]` spans in
    // one note are one edge (the graph shows connectivity, not
    // multiplicity).
    edges.sort();
    edges.dedup();
    Ok(GraphBatch {
        epoch,
        generation,
        nodes,
        edges,
    })
}

// ---------------------------------------------------------------------------
// Facade surface (UI consumes typed snapshots; no handles cross the boundary)
// ---------------------------------------------------------------------------

impl Flavite {
    /// The newest base graph frame, if the service has published one.
    /// A non-blocking atomic load: the UI never waits on the feed or layout
    /// worker (§12.1).
    #[must_use]
    pub fn graph_frame(&self) -> Option<Arc<GraphFrame>> {
        self.graph.frame()
    }

    /// Whether the published graph covers every current open buffer instance.
    /// A new edit immediately makes the old relationships visibly updating.
    pub fn graph_is_current(&self) -> bool {
        self.graph
            .frame()
            .is_some_and(|frame| frame.projection.buffer_stamp == self.session.graph_buffer_stamp())
    }

    pub fn graph_request_scope(
        &self,
        token: GraphViewToken,
        scope: flavite_graph::GraphScope,
    ) -> Result<(), &'static str> {
        self.graph.request_scope(ScopeRequest { token, scope })
    }

    pub fn graph_release_scope(&self, tab: u128) {
        self.graph.release_scope(tab);
    }

    /// Honest service phase for the graph view's loading/error states.
    #[must_use]
    pub fn graph_status(&self) -> GraphServiceStatus {
        self.graph.status()
    }

    /// Bounded graph service counters for diagnostics (§13.4).
    #[must_use]
    pub fn graph_metrics(&self) -> GraphServiceMetrics {
        self.graph.metrics()
    }

    /// Hint the graph service after a vault-content change (§12.1).
    pub fn graph_notify_changed(&self) {
        self.graph.notify_changed();
    }

    /// Explicitly re-install the layout worker from the current base
    /// (worker-panic recovery and the graph view's Retry control).
    pub fn graph_relayout(&self) {
        self.graph.relayout();
    }

    /// Read the vault's graph-pins document (W07/TD-11).
    ///
    /// # Errors
    /// Typed user-state failures; a read-only vault can still read.
    pub fn read_graph_pins(&self) -> Result<Option<StoredUserState>, FlaviteError> {
        self.vault_user_state
            .read_graph_pins()
            .map_err(FlaviteError::UserState)
    }

    /// Write the vault's graph-pins document under a generation receipt.
    ///
    /// Read-only vaults reject writes with a typed error: device-local
    /// layout/preferences still save (§11.2), but graph pins modify vault
    /// user state.
    ///
    /// # Errors
    /// Typed user-state failures, including the read-only refusal and
    /// generation mismatch.
    pub fn write_graph_pins(
        &self,
        expected_generation: u64,
        payload: &[u8],
    ) -> Result<u64, FlaviteError> {
        self.vault_user_state
            .write_graph_pins(expected_generation, payload, !self.read_only)
            .map_err(FlaviteError::UserState)
    }

    /// Test-only seam: point the graph feed at an explicit cache database
    /// so integration fixtures with an injected index actor are exercised
    /// end-to-end. Never part of the production path.
    #[doc(hidden)]
    pub fn set_graph_db_path_for_test(&mut self, path: PathBuf) {
        self.graph = crate::graph_service::GraphService::start_with_session(
            path,
            Some(Arc::clone(&self.session)),
        );
        self.queries.set_graph(Arc::clone(&self.graph));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wait(service: &GraphService, predicate: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !predicate() {
            assert!(
                std::time::Instant::now() < deadline,
                "service did not settle: {:?}",
                service.status()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn panicked_feed_restarts_three_times_then_requires_explicit_retry() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.sqlite");
        drop(flavite_index::open_or_init(&db).unwrap());
        let service = GraphService::start(db);
        wait(&service, || service.frame().is_some());
        for failures in 1..=4 {
            service.hints.send(Directive::Panic).unwrap();
            wait(&service, || {
                service.metrics().bridge_errors_total >= failures
            });
            if failures <= 3 {
                wait(&service, || {
                    service.metrics().worker_restarts_total >= failures
                        && service.status() == GraphServiceStatus::Live
                });
            }
        }
        assert_eq!(service.metrics().worker_restarts_total, 3);
        assert!(matches!(
            service.status(),
            GraphServiceStatus::Failed { .. }
        ));
        service.relayout();
        wait(&service, || {
            service.metrics().worker_restarts_total == 4
                && service.status() == GraphServiceStatus::Live
        });
        service.shutdown();
    }
}
