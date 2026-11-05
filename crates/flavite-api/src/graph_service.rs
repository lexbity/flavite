//! Per-vault graph base service (RFC-0003 TD-12, W07, §10.1, §12.1;
//! RFC-0004 S6 incremental bridge; RFC-0005 S5 event spine).
//!
//! One service per vault, one bounded worker thread: it owns the *only*
//! database handle on the graph path, wakes on the writer-side generation
//! broadcast (RFC-0005 S1) — not on a cadence — and advances a
//! [`GraphMirror`] from the typed change feed — reading only changed
//! entities — then publishes immutable [`GraphFrame`]s the UI consumes
//! without ever waiting (D15: the frame path is an atomic pointer load).
//! The SQLite-backed feed bridge lives here — below the facade boundary,
//! never inside `flavite-graph` and never exposed as a connection handle to
//! the UI (§5.1).
//!
//! # Wake mechanics (RFC-0005 §4.1/§5.2)
//!
//! The loop waits on three independent legs, none of which is trusted:
//!
//! 1. **Events** — every committed structural generation wakes the pump
//!    via the `GenerationWatch` (bounded(1), latest-wins). The change log
//!    stays the only correctness authority: a wake carries a *watermark*,
//!    compared against the mirror cursor; `watermark == cursor` with an
//!    unchanged buffer stamp skips the whole pump (FR-3) — no database
//!    open, no mirror lock.
//! 2. **The sweep** — a 5 s timeout wake re-checks currency against the
//!    database (one cursor meta read), never the channel: recovery for
//!    external writers (CLI on the same vault) and any dropped event.
//! 3. **The parity guard** (unchanged) — drift detection, orthogonal to
//!    staleness.
//!
//! Host-initiated kicks (scope changes, tests, explicit retries) ride the
//! control channel; `Directive::Relayout` doubles as the generic wake (its
//! layout side effect is gated by the `retry_layout` flag). The UI pulse
//! relay (`graph_notify_changed`) is retired — every vault commit reaches
//! the service through the writer-side broadcast, and its call sites died
//! with this slice (the facade symbol is deleted in S6).
//!
//! # Long-lived read connection (RFC-0005 §4.5/FR-6)
//!
//! One read-only connection is opened per service attempt and kept across
//! pumps, revalidated every wake against the index meta `vault_uuid`
//! (populated by the vault actor at open); mismatch or any read error drops
//! and reopens, so a stale handle never silently reads a ghost database
//! after a DB recreate. Every pump read still runs inside a short
//! transaction that ends before the pump returns.
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
use flavite_index::types::IndexVersion;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::graph_mirror::{GraphMirror, MirrorStats, ParityOutcome, ParityPolicy, Synchronize};
use crate::graph_projection::{GraphProjection, GraphViewToken, ScopeRequest};
use crate::{Flavite, FlaviteError, StoredUserState};
use flavite_vault::session::Session;
use std::collections::BTreeMap;

/// Sweep cadence (RFC-0005 §4.1): the recovery/external-writer leg of the
/// wake loop — never the mechanism. A quiet system wakes at 0.2 Hz, one
/// meta read per sweep (NFR-1).
pub const GRAPH_SWEEP_INTERVAL: Duration = Duration::from_secs(5);

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

/// Why the service thread must wake right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Directive {
    /// Re-install the layout worker from the current publication (explicit
    /// "Retry layout" after a worker panic, §12.1 restart policy). Doubles
    /// as the generic host-initiated wake (scope changes, tests, explicit
    /// kicks): the layout side effect is gated by the `retry_layout` flag,
    /// which only `GraphService::relayout` sets.
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
    /// Index wakes that led to a pump (FR-2; wakes dropped by the FR-3 skip
    /// are counted in [`Self::watermark_skips_total`] instead).
    pub wakes_total: u64,
    /// Sweep (timeout-driven) pumps executed (recovery/external writers).
    pub sweeps_total: u64,
    /// Index wakes skipped because the latest advertised watermark equaled
    /// the mirror cursor and the buffer stamp was unchanged (FR-3) — zero
    /// database work and no mirror lock taken for them.
    pub watermark_skips_total: u64,
    /// Long-lived read-connection reopens (vault_uuid/inode mismatch or a
    /// revalidation read error; FR-6). The first open is not a reopen.
    pub connection_reopens_total: u64,
    /// Debounce-deadline stamp rebuilds executed (RFC-0005 §5.2; counted
    /// from S6's trailing-edge policy — the S5 buffer arm never fires).
    pub debounced_rebuilds_total: u64,
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
    /// Cold builds of the F1 handoff snapshot (RFC-0005 S3; `view_arc`
    /// cache misses — stamp-only republishes never increment this).
    pub mirror_snapshots_built_total: u64,
}

#[derive(Debug)]
struct Shared {
    frame: ArcSwapOption<GraphFrame>,
    status: std::sync::Mutex<GraphServiceStatus>,
    counters: Counters,
    scopes: std::sync::Mutex<BTreeMap<u128, ScopeRequest>>,
    retry_layout: AtomicBool,
    /// The saved-graph mirror (RFC-0004 S6). Locked by the service pump
    /// and by the on-demand doctor parity check; no lock holder ever
    /// writes the database through it. True invariant (FR-8, RFC-0005 S3):
    /// the pump's critical section is exactly `synchronize` → poison →
    /// parity-when-due → `view_arc` — the overlay assembly (`build`) runs
    /// on the `Arc<SavedGraph>` snapshot handoff **after the guard is
    /// dropped**, so a concurrent `check_mirror_parity` waits at most one
    /// synchronize (≈1 ms p99) plus, when the guard is due, one parity
    /// full read — never an overlay pass (NFR-7; proven by
    /// `doctor_parity_never_waits_behind_overlay_assembly`).
    mirror: std::sync::Mutex<GraphMirror>,
    /// RFC-0005 S1: the watch half of the writer-side generation broadcast
    /// (when the host constructed the vault with a signal). The S5 wake
    /// loop waits on its receiver; absent a watch (CLI/test pools), the
    /// loop falls back to control-channel + sweep wakes.
    index_watch: Option<Arc<GenerationWatch>>,
    /// FR-3's lock-free mirror-cursor shadow (RFC-0005 S5): the cursor and
    /// health the mutex holder last published. Written only while the
    /// mirror mutex is held, at the end of every critical section that can
    /// move the cursor (pump, `check_mirror_parity`); read by the wake loop
    /// to decide the watermark skip **without ever locking the mirror**
    /// (FR-3). `mirror_ready == false` (cold mirror, or a pump that ended
    /// poisoned/errored) makes the skip unconditionally conservative.
    mirror_cursor_epoch: AtomicU64,
    mirror_cursor_generation: AtomicU64,
    mirror_ready: AtomicBool,
}

impl Shared {
    /// Publish the mirror's cursor + health into the lock-free shadow.
    /// Call while holding the mirror mutex, after every section that could
    /// have moved the cursor (§4.11: the shadow is written by the mutex
    /// holder only, so a reader never sees it run ahead of the mirror).
    fn publish_mirror_cursor(&self, mirror: &GraphMirror) {
        let cursor = mirror.cursor();
        self.mirror_cursor_epoch
            .store(cursor.epoch, Ordering::Relaxed);
        self.mirror_cursor_generation
            .store(cursor.generation, Ordering::Relaxed);
        self.mirror_ready
            .store(!mirror.is_poisoned(), Ordering::Release);
    }

    /// Whether the shadowed cursor equals `version` exactly (epoch included
    /// — §4.9: any epoch mismatch never skips).
    fn cursor_matches(&self, version: IndexVersion) -> bool {
        self.mirror_cursor_generation.load(Ordering::Relaxed) == version.generation
            && self.mirror_cursor_epoch.load(Ordering::Relaxed) == version.epoch
    }
}

#[derive(Debug, Default)]
struct Counters {
    wakes: AtomicU64,
    sweeps: AtomicU64,
    watermark_skips: AtomicU64,
    connection_reopens: AtomicU64,
    /// RFC-0005 §5.2: deadline-driven stamp rebuilds. The counter and the
    /// wake arm are wired in S5; the trailing-edge debounce policy that
    /// increments it lands in S6 (buffer signal → 150 ms deadline fire).
    debounced_rebuilds: AtomicU64,
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
    mirror_snapshots_built: AtomicU64,
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
        self.mirror_snapshots_built
            .store(stats.snapshots_built_total, Ordering::Relaxed);
    }
}

/// The graph base service for one vault.
///
/// Dropping the last handle (or calling [`Self::shutdown`]) stops the
/// service thread promptly; the thread releases its database connection and
/// shuts its layout worker down on exit.
#[derive(Debug)]
pub struct GraphService {
    shared: Arc<Shared>,
    hints: Sender<Directive>,
    stop: Arc<AtomicBool>,
    /// Wake the wake-loop select on shutdown (the select cannot wait on the
    /// atomic alone).
    stop_tx: Sender<()>,
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
    /// watch. The S5 wake loop is its consumer.
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

    /// Test/bench seam: [`Self::start_with_parity_policy`] with the
    /// generation watch (RFC-0005 S5 event-loop tests).
    #[doc(hidden)]
    #[must_use]
    pub fn start_with_parity_policy_and_watch(
        db_path: PathBuf,
        session: Option<Arc<Session>>,
        policy: ParityPolicy,
        index_watch: Option<Arc<GenerationWatch>>,
    ) -> Arc<Self> {
        Self::start_with_policy(db_path, session, policy, index_watch)
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
            // FR-3 shadow: cold cursor (0, 0) matches no advertised
            // generation and `mirror_ready == false` blocks the skip until
            // the first successful pump publishes the real cursor.
            mirror_cursor_epoch: AtomicU64::new(0),
            mirror_cursor_generation: AtomicU64::new(0),
            mirror_ready: AtomicBool::new(false),
        });
        let (hints, hint_rx) = bounded::<Directive>(HINT_QUEUE_CAPACITY);
        let (stop_tx, stop_rx) = bounded::<()>(1);
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
                    &stop_rx,
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
            stop_tx,
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
            wakes_total: counters.wakes.load(Ordering::Relaxed),
            sweeps_total: counters.sweeps.load(Ordering::Relaxed),
            watermark_skips_total: counters.watermark_skips.load(Ordering::Relaxed),
            connection_reopens_total: counters.connection_reopens.load(Ordering::Relaxed),
            debounced_rebuilds_total: counters.debounced_rebuilds.load(Ordering::Relaxed),
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
            mirror_snapshots_built_total: counters.mirror_snapshots_built.load(Ordering::Relaxed),
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
        let outcome = mirror.parity_check(conn);
        self.shared.counters.publish_mirror(mirror.stats());
        // The parity check may catch the mirror up (bounded synchronize) —
        // publish the cursor shadow so a following index wake can skip
        // against the freshest cursor without touching the mutex (FR-3).
        self.shared.publish_mirror_cursor(&mirror);
        outcome
    }

    /// Wake the service after a host-side change the writer broadcast does
    /// not cover (scope registrations, explicit test kicks). Sends a control
    /// wake; the retry-layout flag is untouched, so no layout reinstall
    /// happens. The §12.1 UI pulse relay is fully retired — every vault
    /// commit reaches this service through the writer-side broadcast
    /// (RFC-0005 S5); the facade symbol (`Flavite::graph_notify_changed`)
    /// is removed in S6.
    pub fn notify_changed(&self) {
        let _ = self.hints.try_send(Directive::Relayout);
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
        let _ = self.stop_tx.try_send(());
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
        let _ = self.stop_tx.try_send(());
        // The thread is joined opportunistically by `shutdown`; dropping
        // without an explicit join still ends it promptly.
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
    stop_rx: &Receiver<()>,
    stop: &AtomicBool,
    session: Option<&Arc<Session>>,
) {
    let mut restarts = std::collections::VecDeque::new();
    while !stop.load(Ordering::Acquire) {
        if !run_service_attempt(db_path, shared, hint_rx, stop_rx, stop, session) {
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
                crossbeam::select! {
                    recv(hint_rx) -> _ => {}
                    recv(stop_rx) -> _ => return,
                    default(GRAPH_SWEEP_INTERVAL) => {}
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

/// The long-lived graph read connection (RFC-0005 §4.5/FR-6): opened once
/// per service attempt, kept across pumps, and revalidated on every pump
/// (skipped wakes touch no database at all — FR-3) against the index meta
/// `vault_uuid` **and** the file's identity on disk. A DB recreate swaps
/// the inode — a handle on the old fd keeps reading the ghost database
/// silently and would match its own stale uuid forever — so revalidation
/// compares both. Any mismatch or read error drops and reopens; the fresh
/// handle then rides the existing epoch/reset path (`Synchronize::Reset` /
/// `Synchronize::Delta{reset}`) into a correct view.
struct MaybeConnection {
    conn: Option<Connection>,
    vault_uuid: Option<String>,
    file_identity: u64,
    /// A successful open after the first one is a reopen (FR-6 telemetry).
    ever_connected: bool,
}

impl MaybeConnection {
    fn new() -> Self {
        Self {
            conn: None,
            vault_uuid: None,
            file_identity: 0,
            ever_connected: false,
        }
    }

    /// Open the read-only handle and capture the vault identity (content
    /// uuid + on-disk file identity). On failure no handle is retained.
    fn open(&mut self, db_path: &Path, counters: &Counters) -> Result<(), String> {
        if !db_path.exists() {
            return Err("Index not available yet".to_owned());
        }
        let connection = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|_| "Graph index unavailable")?;
        connection
            .busy_timeout(Duration::from_millis(50))
            .map_err(|_| "Graph index unavailable")?;
        let uuid = read_vault_uuid(&connection)?;
        let identity = db_file_identity(db_path)?;
        if self.ever_connected {
            counters.connection_reopens.fetch_add(1, Ordering::Relaxed);
            tracing::info!(
                target: "flavite_api::graph_service",
                "graph read connection reopened (vault identity changed)"
            );
        }
        self.ever_connected = true;
        self.conn = Some(connection);
        self.vault_uuid = uuid;
        self.file_identity = identity;
        Ok(())
    }

    /// Revalidate and borrow the long-lived connection. Reads the live
    /// `vault_uuid` and the on-disk file identity on every pump; a mismatch
    /// (or any read error — e.g. the file was swapped mid-read) drops the
    /// handle and reopens once.
    fn ensure(&mut self, db_path: &Path, counters: &Counters) -> Result<&mut Connection, String> {
        if self.conn.is_none() {
            self.open(db_path, counters)?;
            return self
                .conn
                .as_mut()
                .ok_or_else(|| "Graph index unavailable".to_owned());
        }
        let stale = match (
            db_file_identity(db_path),
            read_vault_uuid(self.conn.as_ref().expect("conn present")),
        ) {
            (Ok(identity), Ok(uuid)) => identity != self.file_identity || uuid != self.vault_uuid,
            (Err(error), _) | (_, Err(error)) => {
                tracing::warn!(
                    target: "flavite_api::graph_service",
                    %error,
                    "graph index revalidation failed; reopening"
                );
                true
            }
        };
        if stale {
            self.conn = None;
            self.open(db_path, counters)?;
        }
        Ok(self.conn.as_mut().expect("opened connection"))
    }
}

fn read_vault_uuid(conn: &Connection) -> Result<Option<String>, String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key=?1",
        ["vault_uuid"],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| format!("cannot read graph index identity: {error}"))
}

/// The database file's identity on disk (inode on unix; a constant 0 —
/// i.e. uuid-only revalidation — elsewhere). Errors when the file is gone:
/// a vanished database is exactly the stale-handle case FR-6 names.
fn db_file_identity(db_path: &Path) -> Result<u64, String> {
    let metadata =
        std::fs::metadata(db_path).map_err(|error| format!("cannot stat graph index: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(metadata.ino())
    }
    #[cfg(not(unix))]
    {
        drop(metadata);
        Ok(0)
    }
}

fn run_service_attempt(
    db_path: &Path,
    shared: &Arc<Shared>,
    hint_rx: &Receiver<Directive>,
    stop_rx: &Receiver<()>,
    stop: &AtomicBool,
    session: Option<&Arc<Session>>,
) -> bool {
    let guarded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut publisher = GraphPublisher::new();
        let mut layout = LayoutState::default();
        let mut last_signature = None;
        let mut connection = MaybeConnection::new();
        // RFC-0005 S5 wake sources (§5.2): the generation watch, the buffer
        // channel (a never-sending stub until S6 adds the debounced stamp
        // policy — the arm exists so S6 is policy, not plumbing), the
        // control channel, and stop. Without a watch (CLI/test pools) the
        // index arm waits on a never-filled stub, so the select shape is
        // identical in every wiring.
        let (_index_stub_tx, index_stub_rx) = bounded::<IndexVersion>(1);
        let mut index_rx = shared
            .index_watch
            .as_ref()
            .map(|watch| watch.receiver().clone())
            .unwrap_or_else(|| index_stub_rx.clone());
        let (_buffer_tx, buffer_rx) = bounded::<()>(1);
        // Sweep cadence (RFC-0005 §4.1/§5.2): recovery/external-writer
        // leg, never the mechanism. The first deadline is immediate so a
        // cold service bootstraps without waiting for an event (the old
        // t=0 poll).
        let mut next_sweep_at = std::time::Instant::now();
        // FR-3 stamp leg: the session-side stamp version observed at the
        // start of the last pump (no session ⇒ trivially unchanged).
        let mut observed_stamp_version = session.map_or(0, |session| session.graph_stamp_version());
        while !stop.load(Ordering::Acquire) {
            let timeout = next_sweep_at.saturating_duration_since(std::time::Instant::now());
            let mut index_wake = false;
            let mut buffer_wake = false;
            let mut directive: Option<Directive> = None;
            let mut stopped = false;
            crossbeam::select! {
                recv(index_rx) -> message => {
                    match message {
                        Ok(_version) => { index_wake = true; }
                        // Every sender is gone (the host dropped the index):
                        // the arm would fire every pass — retire it onto the
                        // stub. The sweep is the recovery spine (§4.1 leg 2).
                        Err(_) => { index_rx = index_stub_rx.clone(); }
                    }
                }
                // RFC-0005 §5.2: buffer stamp wake. S5 wires the arm inert;
                // S6 turns this into the trailing-edge 150 ms debounce.
                recv(buffer_rx) -> _ => { buffer_wake = true; }
                recv(hint_rx) -> message => {
                    match message {
                        Ok(message) => { directive = Some(message); }
                        // The GraphService handle is gone — the old
                        // `Disconnected => break` contract.
                        Err(_) => { stopped = true; }
                    }
                }
                recv(stop_rx) -> _ => { stopped = true; }
                default(timeout) => {}
            }
            // Drain anything that arrived while selecting: latest-wins
            // coalescing leaves at most one meaningful wake per pass.
            while index_rx.try_recv().is_ok() {
                index_wake = true;
            }
            while let Ok(message) = hint_rx.try_recv() {
                directive = Some(message);
            }
            match directive {
                #[cfg(test)]
                Some(Directive::Panic) => panic!("injected graph worker failure"),
                // A `Relayout` directive is a wake only. The layout
                // reinstall effect rides on the `retry_layout` flag, which
                // `GraphService::relayout` sets before it sends — so plain
                // host kicks (scope changes, tests) never reseed the layout.
                _ => {}
            }
            if stopped || stop.load(Ordering::Acquire) {
                break;
            }
            let now = std::time::Instant::now();
            let sweep_due = now >= next_sweep_at;
            if sweep_due {
                next_sweep_at = now + GRAPH_SWEEP_INTERVAL;
                shared.counters.sweeps.fetch_add(1, Ordering::Relaxed);
            }
            let relayout_requested = shared.retry_layout.swap(false, Ordering::AcqRel);
            if relayout_requested {
                layout.relayout(shared);
                last_signature = None;
            }
            // FR-3: skip a pure index wake when the latest advertised
            // watermark equals the mirror-cursor shadow (no lock), the
            // mirror is healthy, and no buffer transition moved the stamp
            // since the last pump — no DB open, no DB read, no mirror lock.
            // Every other wake reason forces the pump, and the sweep always
            // verifies against the database (FR-5), never the channel.
            let forced = sweep_due || directive.is_some() || relayout_requested || buffer_wake;
            if index_wake && !forced {
                let skip = shared.mirror_ready.load(Ordering::Acquire)
                    && shared
                        .index_watch
                        .as_ref()
                        .is_some_and(|watch| shared.cursor_matches(watch.watermark()))
                    && session.is_none_or(|session| {
                        session.graph_stamp_version() == observed_stamp_version
                    });
                if skip {
                    shared
                        .counters
                        .watermark_skips
                        .fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            }
            if index_wake {
                shared.counters.wakes.fetch_add(1, Ordering::Relaxed);
            } else if !forced {
                // The select timed out at a non-sweep deadline (impossible
                // while the sweep is the only timeout source) or a drained
                // wake vanished: loop around without touching anything.
                continue;
            }
            // The stamp leg pairs with this capture: whatever the pump
            // builds reflects the buffers as of this read (a transition
            // mid-pump only makes the NEXT skip conservative, never wrong).
            let stamp_at_pump_start = session.map_or(0, |session| session.graph_stamp_version());
            let scopes = shared
                .scopes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let read = (|| -> Result<Option<(Arc<GraphProjection>, bool)>, String> {
                let conn = connection.ensure(db_path, &shared.counters)?;
                let previous = shared.frame.load_full();
                // RFC-0004 S6: advance the mirror by the change feed, run
                // the §6.5.4 drift guard when due. RFC-0005 F1 (FR-8): the
                // complete saved state leaves the mutex as an
                // `Arc<SavedGraph>` snapshot handoff — the critical
                // section below is exactly synchronize → poison → parity →
                // view_arc, and `build`'s overlay assembly runs after the
                // guard is dropped. A concurrent doctor `check_mirror_parity`
                // therefore waits at most one synchronize/parity cycle,
                // never an overlay pass (NFR-7). `force_reset` says the
                // publisher must rebuild the model (§6.5.2).
                let (snapshot, force_reset) = {
                    let mut mirror = shared
                        .mirror
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let mut force_reset = false;
                    match mirror.synchronize(conn)? {
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
                            mirror.force_reset(conn)?;
                            force_reset = true;
                        }
                    }
                    // Poisoned delta machinery — a failed splice or a
                    // contract-violating row: never publish suspect output;
                    // rebuild from the full truth and reset the model in
                    // this same pump (§4.7).
                    if mirror.is_poisoned() {
                        mirror.force_reset(conn)?;
                        force_reset = true;
                    } else if mirror.parity_due(std::time::Instant::now()) {
                        // Parity guard (D8, §6.5.4): counter-driven, inside
                        // this loop — no dedicated thread. A mismatch
                        // rebuilds the view before the projection is built,
                        // so the drifted state never reaches a published
                        // frame.
                        let parity = mirror.parity_check(conn)?;
                        if !parity.matched() {
                            tracing::warn!(
                                target: "flavite_api::graph_service",
                                epoch = parity.version.epoch,
                                generation = parity.version.generation,
                                findings = ?parity.findings,
                                "graph mirror drifted from the full consistent read; rebuilding"
                            );
                            mirror.force_reset(conn)?;
                            force_reset = true;
                        }
                    }
                    // Counters publish last within the section: the set
                    // handed to `GraphServiceMetrics` includes this pump's
                    // snapshot build, so a settled service satisfies the
                    // handoff invariant (snapshots == deltas + resets) at
                    // any later read (§4.6/§5.2 accounting).
                    let snapshot = mirror.view_arc().ok_or("graph mirror holds no view")?;
                    shared.counters.publish_mirror(mirror.stats());
                    // FR-3 shadow: published last in the section, by the
                    // mutex holder — the wake loop's next skip decision
                    // reads exactly this cursor without touching the mutex.
                    shared.publish_mirror_cursor(&mirror);
                    (snapshot, force_reset)
                };
                crate::graph_projection::build(
                    conn,
                    &snapshot,
                    session,
                    &scopes,
                    previous.as_ref().map(|frame| frame.projection.as_ref()),
                )
                .map(|projection| projection.map(|projection| (Arc::new(projection), force_reset)))
            })();
            observed_stamp_version = stamp_at_pump_start;
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
                    // A failed pump may have left the mirror poisoned with
                    // the cursor unmoved: keep the FR-3 skip conservative
                    // until a successful pump re-publishes the shadow.
                    shared.mirror_ready.store(false, Ordering::Release);
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

    /// Wake the graph service after an explicit host-side change the
    /// writer broadcast does not cover (scope registration lives in the
    /// service itself; RFC-0005 §12.1 relay semantics are retired — index
    /// commits reach the service through the generation broadcast since
    /// S5, buffer stamp wakes arrive in S6). All in-tree call sites were
    /// removed in RFC-0005 S5; the symbol is deleted in S6.
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
        self.graph = crate::graph_service::GraphService::start_with_index_watch(
            path,
            Some(Arc::clone(&self.session)),
            self.generation_watch.clone(),
        );
        self.queries.set_graph(Arc::clone(&self.graph));
    }

    /// Test-only seam: recreate the graph service with an explicit
    /// generation watch (RFC-0005 S5) — fixtures that own a
    /// [`flavite_index::GenerationSignal`] hand its watch here so index
    /// commits reach the service through the same event spine as
    /// production. Never part of the production path.
    #[doc(hidden)]
    pub fn set_graph_service_for_test(
        &mut self,
        path: PathBuf,
        index_watch: Option<Arc<flavite_index::GenerationWatch>>,
    ) {
        self.generation_watch = index_watch;
        self.set_graph_db_path_for_test(path);
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
