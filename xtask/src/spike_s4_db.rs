//! S4 DB probe, slice 2 of 2 — WAL single-writer + read pool under reconcile
//! load (PLAN-C slice C4).
//!
//! Spec basis: §9 Phase 0 spike S4 ("WAL read/write concurrency under
//! reconcile load"), D3 (the core is thread-based; the index has exactly one
//! writer thread owning the SQLite writer connection and the tantivy
//! `IndexWriter`; readers use a pool of read-only WAL connections), D12
//! (SQLite's `index.generation` bumps inline on every applied change and is
//! authoritative; tantivy applies are immediate, commits deferred), §4.14
//! (epoch + generation index versions, the invalidation feed, checkpoint
//! validation on open), and §8 AC-15 (search-side latency budgets).
//!
//! Five scripted scenarios measure the numbers C7 (`pool.rs`, `tantivy.rs`)
//! and C8 (reconcile batch sizing) are sized from. All of them run the same
//! structural schema (the §4.3 subset relevant to concurrency: `meta`,
//! `notes`, `changes`, `search_docs`) on a real SQLite database in WAL mode
//! with `synchronous=FULL` — the conservative commit tier, so per-batch cost
//! includes the fsync.
//!
//! 1. **bulk** — a simulated bulk reconcile: one writer thread applies
//!    batched structural transactions while a pool of read-only WAL
//!    connections runs point / invalidation-feed / metadata reads. Measures
//!    read p50/p95/p99/max *during* writes, write throughput, per-batch
//!    (apply + generation bump) latency, `SQLITE_BUSY` counts, generation
//!    monotonicity, and commit → reader visibility lag.
//! 2. **pool** — reader-count sweep (1/2/4/8/16) over the same workload:
//!    evidence for the read-pool sizing recommendation in C7's `pool.rs`.
//! 3. **batch** — batch-size sweep (50/100/500/1000/5000 rows per structural
//!    write transaction) plus a generation-bump-only micro-bench under
//!    `synchronous=FULL` and `=NORMAL`: evidence for C8's reconcile batch
//!    sizing and for what a generation bump actually costs.
//! 4. **search** — the same reconcile with tantivy document applies
//!    interleaved on the one writer (D3), exactly one deferred commit
//!    carrying the §4.14 checkpoint payload at the end (D12), and open-time
//!    generation-check timing (SQLite epoch/generation vs the tantivy
//!    checkpoint).
//! 5. **wal-snapshot** — a held read transaction keeps seeing its snapshot
//!    while the writer commits further batches (and is never blocked by the
//!    reader); a fresh transaction sees the latest generation. This is the
//!    WAL behavior C7's read pool must respect: one fresh read transaction
//!    per query, never one held across operations.
//!
//! The fixture is slice 1's deterministic CJK-heavy `build_fixture`, so the
//! rows here and in `S4-search.md` describe the same corpus. WAL concurrency
//! was explicitly out of scope for the search probe; this probe owns it.
//!
//! `--self-check` asserts only the properties the production wrappers rely
//! on: readers never see `SQLITE_BUSY` while the writer writes, generations
//! are observed monotonically, every applied batch lands (final generation ==
//! batch count), the deferred search commit is a single commit whose
//! checkpoint matches SQLite, and the held-snapshot/stale reader rules hold
//! while the writer progresses. Everything else is measured, never
//! fabricated. Findings land in `devdocs/spikes/S4-db.md`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{Connection, ErrorCode, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::spike_s3::{LatencyStats, latency_stats};
use crate::spike_s4_search::{
    Note, Rng, commit_with_checkpoint, create_index, from_sql_i64, index_doc, read_checkpoint,
    reopen_index, sql_i64,
};

// ---------------------------------------------------------------------------
// Parameters under test (defaults + sweeps)
// ---------------------------------------------------------------------------

/// Default rows per structural write transaction — the probe's baseline
/// batch; the batch sweep (scenario 3) sizes C8's real value.
pub const DEFAULT_BATCH: usize = 500;

/// Default read-pool size (D3: readers use a pool of read-only WAL
/// connections).
pub const DEFAULT_READERS: usize = 8;

/// Reader-count sweep (scenario 2).
pub const POOL_SWEEP: &[usize] = &[1, 2, 4, 8, 16];

/// Batch-size sweep (scenario 3).
pub const BATCH_SWEEP: &[usize] = &[50, 100, 500, 1000, 5000];

/// Generation-bump-only micro-bench iterations per synchronous mode
/// (scenario 3).
pub const BUMP_ITERS: usize = 500;

/// Discarded warm-up iterations before the bump micro-bench starts sampling.
pub const BUMP_WARMUP: usize = 25;

/// The probe's SQLite epoch — the bootstrap `meta` row written by
/// [`open_fresh_db`] and the epoch the scenario-4 checkpoint payload must
/// carry (§4.14).
pub const PROBE_EPOCH: u64 = 1;

/// Bounded writer busy timeout: a pathological writer block (readers must
/// never cause one in WAL) fails the probe instead of hanging CI.
const WRITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Open-time generation-check iterations (scenario 4, §4.14 startup
/// validation).
pub const OPEN_CHECK_ITERS: usize = 10;

/// Notes run as a warm-up pass before each sweep, so its first row does not
/// pay one-off initialization costs.
pub const WARMUP_NOTES: usize = 1_000;

// ---------------------------------------------------------------------------
// Schema (the §4.3 structural subset) and connections
// ---------------------------------------------------------------------------

/// The structural schema the probe writes under concurrent read load: the
/// §4.3 tables/indices that matter for concurrency (`meta`, `notes`,
/// `changes` with its composite PK, `search_docs`). WAL + foreign keys.
const SCHEMA_SQL: &str = "
PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS notes(
  note_id      INTEGER PRIMARY KEY,
  path         TEXT NOT NULL,
  path_key     TEXT NOT NULL,
  stem_key     TEXT NOT NULL,
  title        TEXT NOT NULL,
  content_hash BLOB NOT NULL,
  size         INTEGER NOT NULL,
  mtime_ns     INTEGER NOT NULL,
  scan_ns      INTEGER NOT NULL,
  generation   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS notes_path_key ON notes(path_key);
CREATE TABLE IF NOT EXISTS changes(
  generation INTEGER NOT NULL,
  kind       TEXT NOT NULL CHECK(kind IN ('note','attachment','outgoing')),
  entity_id  INTEGER NOT NULL,
  PRIMARY KEY(generation, kind, entity_id)
);
CREATE TABLE IF NOT EXISTS search_docs(
  note_id   INTEGER PRIMARY KEY,
  body_text TEXT NOT NULL
);
";

/// SQLite `synchronous` mode for a probe connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncMode {
    /// `FULL` — sync the WAL at every commit (the conservative tier; what the
    /// reconcile scenarios measure).
    Full,
    /// `NORMAL` — sync only at checkpoint (measured only by the bump-only
    /// micro-bench, as a comparison tier).
    Normal,
}

impl SyncMode {
    /// The `PRAGMA synchronous` value for this mode.
    #[must_use]
    pub const fn pragma(self) -> &'static str {
        match self {
            Self::Full => "FULL",
            Self::Normal => "NORMAL",
        }
    }
}

/// Open a **fresh** probe database at `path`: any stale database/WAL files
/// from a previous run are removed first, then the schema is created in WAL
/// mode, `synchronous` set to `sync`, and bootstrap `meta` rows (epoch,
/// generation, log_floor) written so read-only peers always find them.
///
/// The returned connection stays open for the whole scenario — that is what
/// keeps the `-wal`/`-shm` files alive while read-only readers open.
///
/// # Errors
/// Returns an error if the directory or database cannot be created, or a
/// PRAGMA/meta write fails.
pub fn open_fresh_db(path: &Path, sync: SyncMode) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create db dir: {}", parent.display()))?;
    }
    // Fresh start: a reused `--dir` must not inherit a previous run's
    // generation counter or rows.
    for suffix in ["", "-wal", "-shm"] {
        let file = format!("{}{suffix}", path.display());
        let _ = std::fs::remove_file(file);
    }
    let conn = Connection::open(path).with_context(|| format!("open db: {}", path.display()))?;
    conn.execute_batch(SCHEMA_SQL)?;
    conn.pragma_update(None, "synchronous", sync.pragma())?;
    conn.execute_batch(&format!(
        "INSERT INTO meta(key,value) VALUES('epoch','{PROBE_EPOCH}') ON CONFLICT(key) DO NOTHING;
         INSERT INTO meta(key,value) VALUES('generation','0') ON CONFLICT(key) DO NOTHING;
         INSERT INTO meta(key,value) VALUES('log_floor','0') ON CONFLICT(key) DO NOTHING;"
    ))?;
    Ok(conn)
}

/// Open the **writer** connection for a reconcile scenario: a fresh WAL
/// database at `synchronous=FULL` (the conservative commit tier, so every
/// measured batch includes its fsync) with a bounded busy timeout so a
/// pathological block fails the probe instead of hanging it.
///
/// # Errors
/// Returns an error if the database cannot be created or the timeout cannot
/// be set.
fn open_writer(db_path: &Path) -> Result<Connection> {
    let conn = open_fresh_db(db_path, SyncMode::Full)?;
    conn.busy_timeout(WRITE_BUSY_TIMEOUT)?;
    Ok(conn)
}

fn meta_u64(conn: &Connection, key: &str) -> Result<u64> {
    let value: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key=?1", params![key], |row| {
            row.get(0)
        })
        .optional()?;
    let value = value.with_context(|| format!("missing meta row '{key}'"))?;
    value
        .parse::<u64>()
        .with_context(|| format!("meta row '{key}' is not a u64: {value}"))
}

// ---------------------------------------------------------------------------
// The writer side: one structural write transaction per batch (D3/D12)
// ---------------------------------------------------------------------------

/// The probe's lookup keys. The fixture's paths are ASCII and unique, so the
/// probe does not re-derive Unicode normalization here (per-OS native vs
/// lookup keys are C5's scope, §4.16.2); the shape — a separate normalized
/// key column from the display path — is what this schema exercises.
fn path_key(path: &str) -> String {
    path.to_ascii_lowercase()
}

fn stem_key(path: &str) -> String {
    Path::new(path).file_stem().map_or_else(
        || path.to_ascii_lowercase(),
        |s| s.to_string_lossy().to_ascii_lowercase(),
    )
}

/// Apply one batch of notes as **one** structural write transaction: note
/// upserts + `search_docs` cache + `changes` invalidations + a single inline
/// `meta.generation` bump, committed atomically (D12: the generation bumps
/// inline on every applied change and is authoritative). Returns the instant
/// the commit became durable, for visibility-lag accounting.
///
/// # Errors
/// Returns an error if any statement or the commit fails.
fn apply_batch(conn: &mut Connection, chunk: &[&Note], generation: u64) -> Result<Instant> {
    let tx = conn.transaction()?;
    {
        let mut note_stmt = tx.prepare_cached(
            "INSERT INTO notes(note_id, path, path_key, stem_key, title, content_hash, size,
                               mtime_ns, scan_ns, generation)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(note_id) DO UPDATE SET
               title=excluded.title, content_hash=excluded.content_hash,
               size=excluded.size, generation=excluded.generation",
        )?;
        let mut change_stmt = tx.prepare_cached(
            "INSERT INTO changes(generation, kind, entity_id) VALUES(?1,'note',?2)",
        )?;
        let mut doc_stmt = tx.prepare_cached(
            "INSERT INTO search_docs(note_id, body_text) VALUES(?1,?2)
             ON CONFLICT(note_id) DO UPDATE SET body_text=excluded.body_text",
        )?;
        for note in chunk {
            let hash = blake3::hash(note.body.as_bytes());
            let size = i64::try_from(note.body.len()).unwrap_or(i64::MAX);
            note_stmt.execute(params![
                sql_i64(note.note_id),
                note.path,
                path_key(&note.path),
                stem_key(&note.path),
                note.title,
                hash.as_bytes(),
                size,
                sql_i64(note.note_id.wrapping_mul(1_000)),
                sql_i64(note.note_id.wrapping_mul(2_000)),
                sql_i64(generation),
            ])?;
            doc_stmt.execute(params![sql_i64(note.note_id), note.body])?;
            change_stmt.execute(params![sql_i64(generation), sql_i64(note.note_id)])?;
        }
        // One generation bump per structural transaction (§4.14).
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('generation',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![generation.to_string()],
        )?;
    }
    tx.commit()?;
    Ok(Instant::now())
}

fn elapsed_us(t: Instant) -> u64 {
    u64::try_from(t.elapsed().as_micros()).unwrap_or(0)
}

fn u64_of(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

#[allow(clippy::cast_precision_loss)]
fn throughput(notes: usize, wall: Duration) -> f64 {
    let us = u64::try_from(wall.as_micros()).unwrap_or(0).max(1) as f64;
    notes as f64 / us * 1_000_000.0
}

#[allow(clippy::cast_precision_loss)]
fn ratio(part: u64, whole: u64) -> f64 {
    if whole == 0 {
        0.0
    } else {
        part as f64 / whole as f64
    }
}

// ---------------------------------------------------------------------------
// Read pool (D3: readers use a pool of read-only WAL connections)
// ---------------------------------------------------------------------------

fn is_busy(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if matches!(e.code, ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    )
}

/// One reader thread's raw results.
#[derive(Default)]
struct ReadSamples {
    queries: u64,
    busy: u64,
    errors: u64,
    first_error: Option<String>,
    point_us: Vec<u64>,
    feed_us: Vec<u64>,
    meta_us: Vec<u64>,
    gen_violations: u64,
    /// First time this reader observed each generation (for visibility lag).
    observations: Vec<(u64, Instant)>,
}

/// One read iteration: the workload query, then the authoritative generation
/// read (D12) that the monotonicity and visibility checks are derived from.
///
/// # Errors
/// Returns the raw SQLite error so the loop can classify busy vs real errors.
fn read_once(
    conn: &Connection,
    kind: usize,
    paths: &[String],
    rng: &mut Rng,
    feed_cursor: &mut u64,
) -> std::result::Result<u64, rusqlite::Error> {
    match kind {
        0 => {
            // Point lookup: the resolver/query shape (one indexed key hit).
            let idx = rng.below(paths.len());
            conn.query_row(
                "SELECT title FROM notes WHERE path_key=?1",
                params![paths[idx]],
                |_| Ok(()),
            )
            .optional()?;
        }
        1 => {
            // Invalidation-feed read (§4.14, the GraphFeed shape): range scan
            // over the `changes` PK from this reader's own cursor.
            let mut stmt = conn.prepare(
                "SELECT generation FROM changes WHERE generation > ?1
                 ORDER BY generation LIMIT 64",
            )?;
            let mut cursor = *feed_cursor;
            let rows =
                stmt.query_map(params![sql_i64(*feed_cursor)], |row| row.get::<_, i64>(0))?;
            for row in rows {
                let g = from_sql_i64(row?);
                if g > cursor {
                    cursor = g;
                }
            }
            *feed_cursor = cursor;
        }
        _ => {}
    }
    let generation: String =
        conn.query_row("SELECT value FROM meta WHERE key='generation'", [], |row| {
            row.get(0)
        })?;
    generation.parse::<u64>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

/// The read loop for one pooled connection: a **fresh read transaction per
/// query** (the usage rule C7's `pool.rs` must keep), never holding a
/// snapshot across iterations.
fn reader_loop(
    mut conn: Connection,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicUsize>,
    paths: Arc<Vec<String>>,
    seed: u64,
) -> Result<ReadSamples> {
    let mut samples = ReadSamples::default();
    let mut rng = Rng::new(seed);
    let mut feed_cursor = 0u64;
    let mut last_seen = 0u64;
    let mut first = true;
    while !stop.load(Ordering::Relaxed) {
        let kind = rng.below(3);
        let started = Instant::now();
        let read: std::result::Result<u64, rusqlite::Error> = (|| {
            let tx = conn.transaction()?;
            let observed = read_once(&tx, kind, &paths, &mut rng, &mut feed_cursor)?;
            tx.commit()?;
            Ok(observed)
        })();
        match read {
            Ok(observed) => {
                let observed_at = Instant::now();
                let us = elapsed_us(started);
                samples.queries += 1;
                match kind {
                    0 => samples.point_us.push(us),
                    1 => samples.feed_us.push(us),
                    _ => samples.meta_us.push(us),
                }
                if observed < last_seen {
                    samples.gen_violations += 1;
                }
                if observed > last_seen {
                    samples.observations.push((observed, observed_at));
                    last_seen = observed;
                }
            }
            Err(e) => {
                if is_busy(&e) {
                    samples.busy += 1;
                } else {
                    samples.errors += 1;
                    if samples.first_error.is_none() {
                        samples.first_error = Some(e.to_string());
                    }
                }
            }
        }
        if first {
            // The writer must not start before every pooled connection has
            // completed (or failed) its first read — otherwise a fast
            // reconcile could finish before any read overlaps it.
            ready.fetch_add(1, Ordering::SeqCst);
            first = false;
        }
    }
    Ok(samples)
}

fn spawn_readers(
    db_path: &Path,
    readers: usize,
    stop: &Arc<AtomicBool>,
    ready: &Arc<AtomicUsize>,
    paths: &Arc<Vec<String>>,
    seed: u64,
) -> Result<Vec<JoinHandle<Result<ReadSamples>>>> {
    let mut handles = Vec::with_capacity(readers);
    for i in 0..readers {
        let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("open read-only connection #{}", i + 1))?;
        let stop = Arc::clone(stop);
        let ready = Arc::clone(ready);
        let paths = Arc::clone(paths);
        let thread_seed = seed.wrapping_add(u64_of(i)).wrapping_add(1);
        handles.push(std::thread::spawn(move || {
            reader_loop(conn, stop, ready, paths, thread_seed)
        }));
    }
    Ok(handles)
}

fn wait_until_ready(ready: &AtomicUsize, readers: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while ready.load(Ordering::SeqCst) < readers && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn join_readers(handles: Vec<JoinHandle<Result<ReadSamples>>>) -> Result<Vec<ReadSamples>> {
    let mut out = Vec::with_capacity(handles.len());
    for (i, handle) in handles.into_iter().enumerate() {
        match handle.join() {
            Ok(Ok(samples)) => out.push(samples),
            Ok(Err(e)) => return Err(e.context(format!("reader thread {i} failed"))),
            Err(_) => anyhow::bail!("reader thread {i} panicked"),
        }
    }
    Ok(out)
}

fn clone_commits(commits: &Mutex<Vec<(u64, Instant)>>) -> Vec<(u64, Instant)> {
    commits
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// Merged reader statistics (queries, busy/errors, per-kind latency, reader
/// generation monotonicity, commit → first-observation visibility lag).
struct ReadAggregate {
    queries: u64,
    busy: u64,
    errors: u64,
    first_error: Option<String>,
    read_us: LatencyStats,
    point_us: LatencyStats,
    feed_us: LatencyStats,
    meta_us: LatencyStats,
    gen_monotonic: bool,
    visibility_us: LatencyStats,
}

fn aggregate_reads(samples: &[ReadSamples], commits: &[(u64, Instant)]) -> ReadAggregate {
    let commit_at: HashMap<u64, Instant> = commits.iter().copied().collect();
    let (mut queries, mut busy, mut errors) = (0u64, 0u64, 0u64);
    let mut first_error = None;
    let mut gen_monotonic = true;
    let (mut point, mut feed, mut meta) = (Vec::new(), Vec::new(), Vec::new());
    let mut visibilities = Vec::new();
    for s in samples {
        queries += s.queries;
        busy += s.busy;
        errors += s.errors;
        if s.gen_violations > 0 {
            gen_monotonic = false;
        }
        if first_error.is_none() {
            first_error.clone_from(&s.first_error);
        }
        point.extend_from_slice(&s.point_us);
        feed.extend_from_slice(&s.feed_us);
        meta.extend_from_slice(&s.meta_us);
        for &(generation, observed_at) in &s.observations {
            if let Some(&committed_at) = commit_at.get(&generation) {
                let lag = u64::try_from(
                    observed_at
                        .saturating_duration_since(committed_at)
                        .as_micros(),
                )
                .unwrap_or(u64::MAX);
                visibilities.push(lag);
            }
        }
    }
    let mut merged = Vec::with_capacity(point.len() + feed.len() + meta.len());
    merged.extend_from_slice(&point);
    merged.extend_from_slice(&feed);
    merged.extend_from_slice(&meta);
    ReadAggregate {
        queries,
        busy,
        errors,
        first_error,
        read_us: latency_stats(&merged),
        point_us: latency_stats(&point),
        feed_us: latency_stats(&feed),
        meta_us: latency_stats(&meta),
        gen_monotonic,
        visibility_us: latency_stats(&visibilities),
    }
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// Scenario 1 report: one writer + a read pool under bulk reconcile load.
#[derive(Debug, Clone, Serialize)]
pub struct BulkReport {
    /// Notes applied.
    pub notes: usize,
    /// Rows per structural write transaction (clamped to the fixture size).
    pub batch: usize,
    /// Read-only connections in the pool.
    pub readers: usize,
    /// Structural write transactions performed.
    pub batches: usize,
    /// Writer wall time, ms.
    pub wall_ms: u64,
    /// Notes per second (writer throughput).
    pub write_throughput: f64,
    /// Per-batch transaction latency — applies + the inline generation bump +
    /// the WAL commit fsync (µs).
    pub batch_txn_us: LatencyStats,
    /// Final authoritative generation (D12).
    pub generation_final: u64,
    /// Writer-side generations strictly increased across commits.
    pub generation_monotonic: bool,
    /// Total reads served during the writes.
    pub read_queries: u64,
    /// All read kinds merged (µs).
    pub read_us: LatencyStats,
    /// Point-lookup reads (µs).
    pub read_point_us: LatencyStats,
    /// Invalidation-feed reads (µs).
    pub read_feed_us: LatencyStats,
    /// Metadata reads (µs).
    pub read_meta_us: LatencyStats,
    /// Every reader observed non-decreasing generations.
    pub reader_generation_monotonic: bool,
    /// `SQLITE_BUSY`/`SQLITE_LOCKED` seen by readers during writes.
    pub busy: u64,
    /// Non-busy errors seen by readers.
    pub errors: u64,
    /// The first non-busy error, if any (honesty field).
    pub first_error: Option<String>,
    /// Commit → first-reader-observation lag (µs): visibility of committed
    /// generations to pooled readers on fresh transactions.
    pub visibility_us: LatencyStats,
}

/// One reader-count sweep row (scenario 2).
#[derive(Debug, Clone, Serialize)]
pub struct PoolRow {
    /// Connections in the read pool.
    pub readers: usize,
    /// Notes applied.
    pub notes: usize,
    /// Rows per structural write transaction.
    pub batch: usize,
    /// Structural write transactions performed.
    pub batches: usize,
    /// Total reads served.
    pub read_queries: u64,
    /// All reads merged (µs).
    pub read_us: LatencyStats,
    /// Writer throughput, notes/s.
    pub write_throughput: f64,
    /// Writer wall time, ms.
    pub wall_ms: u64,
    /// `SQLITE_BUSY`/`SQLITE_LOCKED` seen by readers.
    pub busy: u64,
    /// Non-busy reader errors.
    pub errors: u64,
    /// Final generation.
    pub generation_final: u64,
    /// Writer-side generations strictly increased.
    pub generation_monotonic: bool,
    /// Readers observed non-decreasing generations.
    pub reader_generation_monotonic: bool,
}

/// Scenario 2 report: read-pool sizing evidence.
#[derive(Debug, Clone, Serialize)]
pub struct PoolReport {
    /// One row per pool size in `POOL_SWEEP`.
    pub rows: Vec<PoolRow>,
}

/// One batch-size sweep row (scenario 3).
#[derive(Debug, Clone, Serialize)]
pub struct BatchRow {
    /// Rows per structural write transaction actually used (clamped).
    pub batch: usize,
    /// Structural write transactions performed.
    pub batches: usize,
    /// Per-batch transaction latency (µs): apply + generation bump + fsync.
    pub batch_txn_us: LatencyStats,
    /// Writer throughput, notes/s.
    pub write_throughput: f64,
    /// Writer wall time, ms.
    pub wall_ms: u64,
    /// Total reads served during this batch size's writes.
    pub read_queries: u64,
    /// Read p99 (µs) during this batch size's writes.
    pub read_p99_us: u64,
    /// `SQLITE_BUSY`/`SQLITE_LOCKED` seen by readers.
    pub busy: u64,
    /// Non-busy reader errors.
    pub errors: u64,
    /// Final generation.
    pub generation_final: u64,
    /// Writer-side generations strictly increased.
    pub generation_monotonic: bool,
    /// Readers observed non-decreasing generations.
    pub reader_generation_monotonic: bool,
}

/// Scenario 3 report: batch-size sweep + generation-bump-only micro-bench.
#[derive(Debug, Clone, Serialize)]
pub struct BatchReport {
    /// One row per batch size in `BATCH_SWEEP`.
    pub rows: Vec<BatchRow>,
    /// Bump-only iterations per synchronous mode.
    pub bump_iterations: usize,
    /// Generation-bump-only transaction latency under `synchronous=FULL`
    /// (µs): the fixed cost each structural batch pays on top of its rows.
    pub bump_full_us: LatencyStats,
    /// The same under `synchronous=NORMAL` (µs): the fsync tier's share of
    /// that fixed cost.
    pub bump_normal_us: LatencyStats,
}

/// Scenario 4 report: tantivy writer ops during reconcile + open-time
/// generation check.
#[derive(Debug, Clone, Serialize)]
pub struct SearchReport {
    /// Notes applied.
    pub notes: usize,
    /// Rows per structural write transaction.
    pub batch: usize,
    /// Read-only connections in the pool.
    pub readers: usize,
    /// Structural write transactions performed.
    pub batches: usize,
    /// Writer wall time, ms (SQLite + interleaved tantivy applies).
    pub wall_ms: u64,
    /// Notes per second (writer throughput).
    pub write_throughput: f64,
    /// Per-batch SQLite transaction latency (µs).
    pub sqlite_batch_us: LatencyStats,
    /// Per-batch tantivy `add_document` latency (µs, no commit — D12
    /// applies are immediate, commits deferred).
    pub tantivy_batch_us: LatencyStats,
    /// Total µs spent in SQLite batch transactions.
    pub sqlite_total_us: u64,
    /// Total µs spent applying tantivy documents.
    pub tantivy_total_us: u64,
    /// `tantivy_total_us / sqlite_total_us` — what the search view adds to a
    /// reconcile's apply phase.
    pub apply_overhead_ratio: f64,
    /// Tantivy commits performed (D12: exactly one, at reconcile end).
    pub commit_count: u32,
    /// The deferred commit's latency (µs), checkpoint payload included.
    pub commit_us: u64,
    /// Open-time generation check latency (µs): read SQLite epoch/generation
    /// + tantivy checkpoint payload and compare (§4.14 startup validation).
    pub open_check_us: LatencyStats,
    /// Final SQLite generation.
    pub generation_final: u64,
    /// SQLite epoch.
    pub sqlite_epoch: u64,
    /// The checkpoint generation read from the last tantivy commit.
    pub checkpoint_generation: u64,
    /// The checkpoint payload `(epoch, generation)` equals SQLite's — the
    /// publication invariant of §4.14.
    pub checkpoint_consistent: bool,
    /// Total reads served during the writes.
    pub read_queries: u64,
    /// All reads merged (µs).
    pub read_us: LatencyStats,
    /// `SQLITE_BUSY`/`SQLITE_LOCKED` seen by readers.
    pub busy: u64,
    /// Non-busy reader errors.
    pub errors: u64,
    /// Readers observed non-decreasing generations.
    pub reader_generation_monotonic: bool,
}

/// Scenario 5 report: WAL snapshot isolation + writer progress under an open
/// read transaction.
#[derive(Debug, Clone, Serialize)]
pub struct WalSnapshotReport {
    /// Batches committed before the read transaction was opened.
    pub batches_before_hold: usize,
    /// Batches committed **while** the read transaction was held open.
    pub batches_with_open_reader: usize,
    /// Generation the held transaction saw when it opened.
    pub held_before: u64,
    /// Generation the held transaction still sees after the writer commits.
    pub held_after: u64,
    /// Authoritative generation after the writer's second phase.
    pub writer_generation: u64,
    /// Generation a fresh read transaction sees afterwards.
    pub fresh_generation: u64,
    /// Wall time for the batches written with an open reader (µs).
    pub write_with_open_reader_us: u64,
    /// The held transaction still sees its old snapshot (stale).
    pub held_is_stale: bool,
    /// A fresh transaction sees the latest committed generation.
    pub fresh_is_current: bool,
    /// The writer committed every batch despite the open read transaction
    /// (readers do not block the single writer).
    pub writer_not_blocked: bool,
    /// The writer's error, if its second phase failed (honesty field: a
    /// blocked writer is reported, never silently dropped).
    pub writer_error: Option<String>,
}

/// A scenario report.
#[derive(Debug, Clone, Serialize)]
pub enum ScenarioReport {
    /// Scenario 1: bulk reconcile under concurrent reads.
    Bulk(BulkReport),
    /// Scenario 2: reader-count sweep.
    Pool(PoolReport),
    /// Scenario 3: batch-size sweep + bump micro-bench.
    Batch(BatchReport),
    /// Scenario 4: tantivy ops + open-time generation check.
    Search(SearchReport),
    /// Scenario 5: WAL snapshot behavior.
    WalSnapshot(WalSnapshotReport),
}

/// A named scenario report.
#[derive(Debug, Clone, Serialize)]
pub struct NamedReport {
    /// Scenario name (`1-bulk`, …, `5-wal-snapshot`).
    pub name: String,
    /// The report.
    pub report: ScenarioReport,
}

/// A full probe run.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    /// Host OS.
    pub os: String,
    /// Host architecture.
    pub arch: String,
    /// Filesystem hosting the probe databases (Linux: from `/proc/self/mounts`;
    /// elsewhere `unknown`).
    pub fs: String,
    /// Fixture size (notes).
    pub notes: usize,
    /// Default batch size used unless overridden per scenario.
    pub batch: usize,
    /// Default read-pool size used for the bulk/search scenarios.
    pub readers: usize,
    /// Scenario reports, in scenario order.
    pub scenarios: Vec<NamedReport>,
}

// ---------------------------------------------------------------------------
// Scenario selection
// ---------------------------------------------------------------------------

/// Which scenarios to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScenarioSelection {
    /// Scenario 1: bulk reconcile under concurrent reads.
    pub bulk: bool,
    /// Scenario 2: reader-count sweep.
    pub pool: bool,
    /// Scenario 3: batch-size sweep + bump micro-bench.
    pub batch: bool,
    /// Scenario 4: tantivy ops + open-time generation check.
    pub search: bool,
    /// Scenario 5: WAL snapshot behavior.
    pub wal: bool,
}

/// Parse a scenario list: `all` or a comma-separated subset of `1..5`.
///
/// # Errors
/// Returns an error on an unknown or empty scenario selection.
pub fn parse_selection(input: &str) -> Result<ScenarioSelection> {
    if input == "all" {
        return Ok(ScenarioSelection {
            bulk: true,
            pool: true,
            batch: true,
            search: true,
            wal: true,
        });
    }
    let mut selection = ScenarioSelection::default();
    for part in input.split(',') {
        match part.trim() {
            "1" => selection.bulk = true,
            "2" => selection.pool = true,
            "3" => selection.batch = true,
            "4" => selection.search = true,
            "5" => selection.wal = true,
            other => anyhow::bail!("unknown scenario '{other}' (expected 1..5 or all)"),
        }
    }
    if !(selection.bulk || selection.pool || selection.batch || selection.search || selection.wal) {
        anyhow::bail!("empty scenario selection");
    }
    Ok(selection)
}

// ---------------------------------------------------------------------------
// Scenario runners
// ---------------------------------------------------------------------------

/// The writer half's results, shared by scenarios 1 and 4.
struct WriteOutcome {
    generation: u64,
    batch_us: Vec<u64>,
    tantivy_us: Vec<u64>,
    wall: Duration,
}

/// Run the writer side (batched structural transactions over `notes`), with
/// the given per-batch hook for scenario 4's tantivy applies. The closure
/// runs after each batch's SQLite commit, on the same (single) writer
/// thread — D3's one-writer discipline.
///
/// `start_generation` is the generation already committed before this run
/// (0 for a fresh database; scenario 5's second phase continues from its
/// first), so generations never regress across phases.
fn write_bulk(
    conn: &mut Connection,
    commits: &Arc<Mutex<Vec<(u64, Instant)>>>,
    notes: &[Note],
    batch: usize,
    start_generation: u64,
    mut after_batch: impl FnMut(&Note) -> Result<()>,
) -> Result<WriteOutcome> {
    let mut generation = start_generation;
    let mut batch_us = Vec::with_capacity(notes.len().div_ceil(batch).max(1));
    let mut tantivy_us = Vec::new();
    let started = Instant::now();
    for chunk in notes.chunks(batch) {
        generation += 1;
        let refs: Vec<&Note> = chunk.iter().collect();
        let t0 = Instant::now();
        let committed_at = apply_batch(conn, &refs, generation)?;
        batch_us.push(elapsed_us(t0));
        commits
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((generation, committed_at));
        let t1 = Instant::now();
        for note in chunk {
            after_batch(note)?;
        }
        tantivy_us.push(elapsed_us(t1));
    }
    Ok(WriteOutcome {
        generation,
        batch_us,
        tantivy_us,
        wall: started.elapsed(),
    })
}

/// Scenario 1 runner: a simulated bulk reconcile — one writer applying
/// batched structural transactions while `readers` pooled read-only
/// connections run point/feed/meta reads against the same database.
///
/// # Errors
/// Returns an error if the database, readers, or any write fails.
pub fn run_bulk(dir: &Path, notes: &[Note], batch: usize, readers: usize) -> Result<BulkReport> {
    if notes.is_empty() {
        anyhow::bail!("run_bulk requires a non-empty fixture");
    }
    let batch = batch.clamp(1, notes.len());
    std::fs::create_dir_all(dir).with_context(|| format!("create dir: {}", dir.display()))?;
    let db_path = dir.join("index.db");
    let mut conn = open_writer(&db_path)?;

    let paths = Arc::new(notes.iter().map(|n| n.path.clone()).collect::<Vec<_>>());
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicUsize::new(0));
    let commits = Arc::new(Mutex::new(Vec::new()));
    let handles = spawn_readers(&db_path, readers, &stop, &ready, &paths, 0xB0_0C)?;
    wait_until_ready(&ready, readers);

    let write_outcome: Result<WriteOutcome> =
        write_bulk(&mut conn, &commits, notes, batch, 0, |_| Ok(()));
    stop.store(true, Ordering::SeqCst);
    let samples = join_readers(handles)?;
    let commits_vec = clone_commits(&commits);
    let agg = aggregate_reads(&samples, &commits_vec);
    let outcome = write_outcome?;

    let batches = outcome.batch_us.len();
    Ok(BulkReport {
        notes: notes.len(),
        batch,
        readers,
        batches,
        wall_ms: u64::try_from(outcome.wall.as_millis()).unwrap_or(0),
        write_throughput: throughput(notes.len(), outcome.wall),
        batch_txn_us: latency_stats(&outcome.batch_us),
        generation_final: outcome.generation,
        generation_monotonic: commits_vec.windows(2).all(|w| w[0].0 < w[1].0)
            && outcome.generation == u64_of(batches),
        read_queries: agg.queries,
        read_us: agg.read_us,
        read_point_us: agg.point_us,
        read_feed_us: agg.feed_us,
        read_meta_us: agg.meta_us,
        reader_generation_monotonic: agg.gen_monotonic,
        busy: agg.busy,
        errors: agg.errors,
        first_error: agg.first_error,
        visibility_us: agg.visibility_us,
    })
}

// ---------------------------------------------------------------------------
// Scenario 2: read-pool sweep (C7 `pool.rs` sizing evidence)
// ---------------------------------------------------------------------------

/// Warm-up pass over up to [`WARMUP_NOTES`] notes at the sweep's baseline
/// parameters, so the first measured row does not pay one-off initialization
/// (page cache, statement caches, WAL growth).
fn warm_up(dir: &Path, notes: &[Note], batch: usize, readers: usize) -> Result<()> {
    let take = WARMUP_NOTES.min(notes.len());
    if take == 0 {
        return Ok(());
    }
    run_bulk(&dir.join("warmup"), &notes[..take], batch, readers)?;
    Ok(())
}

/// Scenario 2 runner: the bulk workload repeated once per pool size in
/// [`POOL_SWEEP`] — the reader-count evidence for C7's `pool.rs`.
///
/// # Errors
/// Returns an error if any sweep row fails.
pub fn run_pool_sweep(dir: &Path, notes: &[Note], batch: usize) -> Result<PoolReport> {
    if notes.is_empty() {
        anyhow::bail!("run_pool_sweep requires a non-empty fixture");
    }
    let batch = batch.clamp(1, notes.len());
    std::fs::create_dir_all(dir).with_context(|| format!("create dir: {}", dir.display()))?;
    warm_up(dir, notes, batch, DEFAULT_READERS)?;

    let mut rows = Vec::with_capacity(POOL_SWEEP.len());
    for &readers in POOL_SWEEP {
        let report = run_bulk(&dir.join(format!("pool-{readers}")), notes, batch, readers)?;
        rows.push(PoolRow {
            readers,
            notes: report.notes,
            batch: report.batch,
            batches: report.batches,
            read_queries: report.read_queries,
            read_us: report.read_us,
            write_throughput: report.write_throughput,
            wall_ms: report.wall_ms,
            busy: report.busy,
            errors: report.errors,
            generation_final: report.generation_final,
            generation_monotonic: report.generation_monotonic,
            reader_generation_monotonic: report.reader_generation_monotonic,
        });
    }
    Ok(PoolReport { rows })
}

// ---------------------------------------------------------------------------
// Scenario 3: batch-size sweep + generation-bump micro-bench (C8 sizing)
// ---------------------------------------------------------------------------

/// One bump-only transaction: open a transaction, overwrite
/// `meta.generation`, commit. No rows — this is the fixed per-batch cost
/// (the §4.14 inline generation bump + the WAL commit fsync) every structural
/// batch pays on top of its rows.
fn bump_only(conn: &mut Connection, iterations: usize) -> Result<Vec<u64>> {
    let mut samples = Vec::with_capacity(iterations);
    for i in 0..iterations {
        let generation = u64_of(i + 1);
        let t0 = Instant::now();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO meta(key,value) VALUES('generation',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![generation.to_string()],
        )?;
        tx.commit()?;
        samples.push(elapsed_us(t0));
    }
    Ok(samples)
}

/// Bump-only statistics for one synchronous mode: a discarded warm-up, then
/// [`BUMP_ITERS`] measured transactions.
fn measure_bump_mode(path: &Path, sync: SyncMode) -> Result<LatencyStats> {
    let mut conn = open_fresh_db(path, sync)?;
    bump_only(&mut conn, BUMP_WARMUP)?;
    let samples = bump_only(&mut conn, BUMP_ITERS)?;
    Ok(latency_stats(&samples))
}

/// The generation-bump micro-bench at both synchronous tiers: `(FULL, NORMAL)`.
fn measure_bump(dir: &Path) -> Result<(LatencyStats, LatencyStats)> {
    let full = measure_bump_mode(&dir.join("bump-full.db"), SyncMode::Full)?;
    let normal = measure_bump_mode(&dir.join("bump-normal.db"), SyncMode::Normal)?;
    Ok((full, normal))
}

/// Scenario 3 runner: the bulk workload repeated once per batch size in
/// [`BATCH_SWEEP`] (readers held at `readers`), plus the generation-bump-only
/// micro-bench under `synchronous=FULL` and `=NORMAL`. This is the evidence
/// for C8's reconcile batch sizing.
///
/// # Errors
/// Returns an error if any sweep row or the micro-bench fails.
pub fn run_batch_sweep(
    dir: &Path,
    notes: &[Note],
    batch: usize,
    readers: usize,
) -> Result<BatchReport> {
    if notes.is_empty() {
        anyhow::bail!("run_batch_sweep requires a non-empty fixture");
    }
    let batch = batch.clamp(1, notes.len());
    std::fs::create_dir_all(dir).with_context(|| format!("create dir: {}", dir.display()))?;
    warm_up(dir, notes, batch, readers)?;

    let mut rows = Vec::with_capacity(BATCH_SWEEP.len());
    for &size in BATCH_SWEEP {
        let report = run_bulk(&dir.join(format!("batch-{size}")), notes, size, readers)?;
        rows.push(BatchRow {
            batch: report.batch,
            batches: report.batches,
            batch_txn_us: report.batch_txn_us,
            write_throughput: report.write_throughput,
            wall_ms: report.wall_ms,
            read_queries: report.read_queries,
            read_p99_us: report.read_us.p99,
            busy: report.busy,
            errors: report.errors,
            generation_final: report.generation_final,
            generation_monotonic: report.generation_monotonic,
            reader_generation_monotonic: report.reader_generation_monotonic,
        });
    }
    let (bump_full_us, bump_normal_us) = measure_bump(dir)?;
    Ok(BatchReport {
        rows,
        bump_iterations: BUMP_ITERS,
        bump_full_us,
        bump_normal_us,
    })
}

// ---------------------------------------------------------------------------
// Scenario 4: tantivy writer ops during reconcile + open-time generation check
// ---------------------------------------------------------------------------

/// Scenario 4 runner: the same reconcile as scenario 1, with tantivy document
/// applies interleaved on the one writer thread after every SQLite batch (D3),
/// exactly **one** deferred commit at reconcile end carrying the §4.14
/// checkpoint payload (D12), and the open-time generation check — read SQLite
/// `epoch`/`generation` on a fresh connection, read the tantivy checkpoint,
/// compare — timed over [`OPEN_CHECK_ITERS`] iterations.
///
/// # Errors
/// Returns an error if the index, database, readers, or any write fails.
pub fn run_search(
    dir: &Path,
    notes: &[Note],
    batch: usize,
    readers: usize,
) -> Result<SearchReport> {
    if notes.is_empty() {
        anyhow::bail!("run_search requires a non-empty fixture");
    }
    let batch = batch.clamp(1, notes.len());
    std::fs::create_dir_all(dir).with_context(|| format!("create dir: {}", dir.display()))?;

    let index_dir = dir.join("search-idx");
    if index_dir.exists() {
        std::fs::remove_dir_all(&index_dir)
            .with_context(|| format!("remove stale index dir: {}", index_dir.display()))?;
    }
    let (index, fields) = create_index(Some(&index_dir))?;
    let mut writer = index.writer_with_num_threads(2, 64_000_000)?;

    let db_path = dir.join("index.db");
    let mut conn = open_writer(&db_path)?;
    let paths = Arc::new(notes.iter().map(|n| n.path.clone()).collect::<Vec<_>>());
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicUsize::new(0));
    let commits = Arc::new(Mutex::new(Vec::new()));
    let handles = spawn_readers(&db_path, readers, &stop, &ready, &paths, 0x5E_A4)?;
    wait_until_ready(&ready, readers);

    // Tantivy applies ride the single writer thread, after each SQLite batch
    // commit — never on a reader thread (D3), never committed per batch (D12).
    let write_outcome: Result<WriteOutcome> =
        write_bulk(&mut conn, &commits, notes, batch, 0, |note| {
            writer.add_document(index_doc(&fields, note))?;
            Ok(())
        });
    stop.store(true, Ordering::SeqCst);
    let samples = join_readers(handles)?;
    let commits_vec = clone_commits(&commits);
    let agg = aggregate_reads(&samples, &commits_vec);
    let outcome = write_outcome?;

    // Exactly one deferred commit, publishing the checkpoint atomically.
    let t_commit = Instant::now();
    commit_with_checkpoint(&mut writer, &index, PROBE_EPOCH, outcome.generation)?;
    let commit_us = elapsed_us(t_commit);
    let commit_count = 1u32;
    drop(writer);

    // Open-time generation check (§4.14 startup validation): a fresh SQLite
    // read connection vs the durable tantivy checkpoint payload.
    let (reopened, _fields) = reopen_index(&index_dir)?;
    let check_conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut open_check_us = Vec::with_capacity(OPEN_CHECK_ITERS);
    let mut sqlite_epoch = 0u64;
    let mut checkpoint_generation = 0u64;
    let mut checkpoint_consistent = true;
    for _ in 0..OPEN_CHECK_ITERS {
        let t0 = Instant::now();
        sqlite_epoch = meta_u64(&check_conn, "epoch")?;
        let sqlite_generation = meta_u64(&check_conn, "generation")?;
        let checkpoint = read_checkpoint(&reopened)?.unwrap_or((0, 0));
        open_check_us.push(elapsed_us(t0));
        checkpoint_generation = checkpoint.1;
        if checkpoint != (sqlite_epoch, sqlite_generation) {
            checkpoint_consistent = false;
        }
    }

    let sqlite_total: u64 = outcome.batch_us.iter().sum();
    let tantivy_total: u64 = outcome.tantivy_us.iter().sum();
    Ok(SearchReport {
        notes: notes.len(),
        batch,
        readers,
        batches: outcome.batch_us.len(),
        wall_ms: u64::try_from(outcome.wall.as_millis()).unwrap_or(0),
        write_throughput: throughput(notes.len(), outcome.wall),
        sqlite_batch_us: latency_stats(&outcome.batch_us),
        tantivy_batch_us: latency_stats(&outcome.tantivy_us),
        sqlite_total_us: sqlite_total,
        tantivy_total_us: tantivy_total,
        apply_overhead_ratio: ratio(tantivy_total, sqlite_total),
        commit_count,
        commit_us,
        open_check_us: latency_stats(&open_check_us),
        generation_final: outcome.generation,
        sqlite_epoch,
        checkpoint_generation,
        checkpoint_consistent,
        read_queries: agg.queries,
        read_us: agg.read_us,
        busy: agg.busy,
        errors: agg.errors,
        reader_generation_monotonic: agg.gen_monotonic,
    })
}

// ---------------------------------------------------------------------------
// Scenario 5: WAL snapshot isolation + writer progress under an open reader
// ---------------------------------------------------------------------------

/// Scenario 5 runner: commit half the fixture, open a read transaction and
/// **hold** it, commit the rest with that transaction open, then compare what
/// the held transaction sees against a fresh transaction.
///
/// This is the WAL behavior C7's read pool must respect: a held read
/// transaction stays on its snapshot (stale) and never blocks the single
/// writer, while a fresh transaction per query sees the latest generation.
///
/// # Errors
/// Returns an error if the database or any read fails (a writer failure is
/// reported in [`WalSnapshotReport::writer_error`] instead).
pub fn run_wal_snapshot(dir: &Path, notes: &[Note], batch: usize) -> Result<WalSnapshotReport> {
    if notes.len() < 2 {
        anyhow::bail!("run_wal_snapshot requires at least two notes");
    }
    let batch = batch.clamp(1, notes.len());
    std::fs::create_dir_all(dir).with_context(|| format!("create dir: {}", dir.display()))?;
    let db_path = dir.join("index.db");
    let mut writer_conn = open_writer(&db_path)?;
    let commits = Arc::new(Mutex::new(Vec::new()));
    let split = notes.len() / 2;

    // Phase 1: batches committed before the read transaction opens.
    let phase1 = write_bulk(
        &mut writer_conn,
        &commits,
        &notes[..split],
        batch,
        0,
        |_| Ok(()),
    )?;
    let batches_before_hold = phase1.batch_us.len();

    // Hold a read transaction across the writer's second phase.
    let mut reader_conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let held = reader_conn.transaction()?;
    let held_before = meta_u64(&held, "generation")?;
    let t0 = Instant::now();
    let phase2: Result<WriteOutcome> = write_bulk(
        &mut writer_conn,
        &commits,
        &notes[split..],
        batch,
        phase1.generation,
        |_| Ok(()),
    );
    let write_with_open_reader_us = elapsed_us(t0);
    let writer_error = phase2.as_ref().err().map(|e| format!("{e:#}"));
    let batches_with_open_reader = phase2.as_ref().map_or(0, |o| o.batch_us.len());
    let held_after = meta_u64(&held, "generation")?;
    drop(held);
    let fresh_generation = meta_u64(&reader_conn, "generation")?;
    drop(reader_conn);

    let writer_generation = meta_u64(&writer_conn, "generation")?;
    let expected_batches = notes[split..].len().div_ceil(batch);
    Ok(WalSnapshotReport {
        batches_before_hold,
        batches_with_open_reader,
        held_before,
        held_after,
        writer_generation,
        fresh_generation,
        write_with_open_reader_us,
        held_is_stale: held_before == held_after && held_before < writer_generation,
        fresh_is_current: fresh_generation == writer_generation,
        writer_not_blocked: writer_error.is_none() && batches_with_open_reader == expected_batches,
        writer_error,
    })
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

/// The filesystem hosting `dir`: on Linux the `fs type` of the longest
/// matching mount in `/proc/self/mounts` (WAL fsync cost is filesystem
/// dependent, so the spike doc records it); `unknown` elsewhere or when it
/// cannot be determined.
#[must_use]
pub fn filesystem_of(dir: &Path) -> String {
    std::fs::canonicalize(dir)
        .ok()
        .and_then(|canonical| mount_fs_type(&canonical))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(target_os = "linux")]
fn mount_fs_type(canonical: &Path) -> Option<String> {
    let mounts = std::fs::read_to_string("/proc/self/mounts").ok()?;
    let mut best: Option<(usize, &str)> = None;
    for line in mounts.lines() {
        let mut fields = line.split_whitespace();
        let (_device, mount, fs_type) = (fields.next()?, fields.next()?, fields.next()?);
        let mount = mount.replace("\\040", " ");
        if canonical.starts_with(&mount) && best.is_none_or(|(len, _)| mount.len() > len) {
            best = Some((mount.len(), fs_type));
        }
    }
    best.map(|(_, fs)| fs.to_string())
}

#[cfg(not(target_os = "linux"))]
fn mount_fs_type(_canonical: &Path) -> Option<String> {
    None
}

/// Run the selected scenarios against fresh databases rooted at `dir`.
///
/// # Errors
/// Returns an error if any selected scenario fails.
pub fn run_all(
    dir: &Path,
    notes: &[Note],
    batch: usize,
    readers: usize,
    selection: &ScenarioSelection,
) -> Result<RunSummary> {
    let mut scenarios = Vec::new();
    if selection.bulk {
        let report = run_bulk(&dir.join("bulk"), notes, batch, readers)?;
        scenarios.push(NamedReport {
            name: "1-bulk".to_string(),
            report: ScenarioReport::Bulk(report),
        });
    }
    if selection.pool {
        let report = run_pool_sweep(&dir.join("pool"), notes, batch)?;
        scenarios.push(NamedReport {
            name: "2-pool".to_string(),
            report: ScenarioReport::Pool(report),
        });
    }
    if selection.batch {
        let report = run_batch_sweep(&dir.join("batch"), notes, batch, readers)?;
        scenarios.push(NamedReport {
            name: "3-batch".to_string(),
            report: ScenarioReport::Batch(report),
        });
    }
    if selection.search {
        let report = run_search(&dir.join("search"), notes, batch, readers)?;
        scenarios.push(NamedReport {
            name: "4-search".to_string(),
            report: ScenarioReport::Search(report),
        });
    }
    if selection.wal {
        let report = run_wal_snapshot(&dir.join("wal"), notes, batch)?;
        scenarios.push(NamedReport {
            name: "5-wal-snapshot".to_string(),
            report: ScenarioReport::WalSnapshot(report),
        });
    }
    Ok(RunSummary {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        fs: filesystem_of(dir),
        notes: notes.len(),
        batch,
        readers,
        scenarios,
    })
}

// ---------------------------------------------------------------------------
// Self-checks (the properties the production wrappers rely on)
// ---------------------------------------------------------------------------

/// Shared reader-side invariants: no `SQLITE_BUSY`, no other errors, and
/// reads that actually overlapped the writer.
fn check_readers(name: &str, busy: u64, errors: u64, first_error: Option<&str>) -> Result<()> {
    if busy != 0 {
        anyhow::bail!("{name}: readers saw {busy} SQLITE_BUSY/LOCKED reads during writes");
    }
    if errors != 0 {
        anyhow::bail!(
            "{name}: readers saw {errors} non-busy errors (first: {})",
            first_error.unwrap_or("<unrecorded>")
        );
    }
    Ok(())
}

/// Shared generation invariants: writer generations strictly increased to the
/// batch count, and every reader observed non-decreasing generations.
fn check_generations(
    name: &str,
    generation_final: u64,
    batches: usize,
    writer_monotonic: bool,
    reader_monotonic: bool,
) -> Result<()> {
    if !writer_monotonic {
        anyhow::bail!("{name}: writer-side generations were not strictly increasing");
    }
    if generation_final != u64_of(batches) {
        anyhow::bail!("{name}: final generation {generation_final} != batch count {batches}");
    }
    if !reader_monotonic {
        anyhow::bail!("{name}: a reader observed a decreasing generation");
    }
    Ok(())
}

/// Self-check scenario 1: readers never block or error during writes, every
/// reader overlapped the writer, and generations are monotonic on both sides.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_bulk(report: &BulkReport) -> Result<()> {
    check_readers(
        "1-bulk",
        report.busy,
        report.errors,
        report.first_error.as_deref(),
    )?;
    check_generations(
        "1-bulk",
        report.generation_final,
        report.batches,
        report.generation_monotonic,
        report.reader_generation_monotonic,
    )?;
    if report.read_queries < u64_of(report.readers) {
        anyhow::bail!(
            "1-bulk: only {} reads from {} pooled connections — reads did not overlap the writer",
            report.read_queries,
            report.readers
        );
    }
    if report.read_us.count == 0 {
        anyhow::bail!("1-bulk: no read latencies recorded");
    }
    Ok(())
}

/// Self-check scenario 2: every pool size in the sweep held the same
/// reader-side and generation invariants.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_pool(report: &PoolReport) -> Result<()> {
    if report.rows.len() != POOL_SWEEP.len() {
        anyhow::bail!(
            "2-pool: {} rows, expected {}",
            report.rows.len(),
            POOL_SWEEP.len()
        );
    }
    for row in &report.rows {
        let name = format!("2-pool(readers={})", row.readers);
        check_readers(&name, row.busy, row.errors, None)?;
        check_generations(
            &name,
            row.generation_final,
            row.batches,
            row.generation_monotonic,
            row.reader_generation_monotonic,
        )?;
        if row.read_queries < u64_of(row.readers) {
            anyhow::bail!(
                "{name}: only {} reads from {} pooled connections",
                row.read_queries,
                row.readers
            );
        }
    }
    Ok(())
}

/// Self-check scenario 3: every batch size held the reader/generation
/// invariants, and both bump micro-bench tiers sampled [`BUMP_ITERS`]
/// transactions.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_batch(report: &BatchReport) -> Result<()> {
    if report.rows.len() != BATCH_SWEEP.len() {
        anyhow::bail!(
            "3-batch: {} rows, expected {}",
            report.rows.len(),
            BATCH_SWEEP.len()
        );
    }
    for row in &report.rows {
        let name = format!("3-batch(batch={})", row.batch);
        check_readers(&name, row.busy, row.errors, None)?;
        check_generations(
            &name,
            row.generation_final,
            row.batches,
            row.generation_monotonic,
            row.reader_generation_monotonic,
        )?;
        if row.read_queries == 0 {
            anyhow::bail!("{name}: reads did not overlap the writer");
        }
    }
    if report.bump_full_us.count != BUMP_ITERS {
        anyhow::bail!(
            "3-batch: synchronous=FULL bump bench sampled {} of {BUMP_ITERS} iterations",
            report.bump_full_us.count
        );
    }
    if report.bump_normal_us.count != BUMP_ITERS {
        anyhow::bail!(
            "3-batch: synchronous=NORMAL bump bench sampled {} of {BUMP_ITERS} iterations",
            report.bump_normal_us.count
        );
    }
    Ok(())
}

/// Self-check scenario 4: readers held their invariants during the tantivy
/// interleaving, the reconcile performed exactly one deferred commit whose
/// checkpoint matches SQLite, and the open-time generation check ran.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_search(report: &SearchReport) -> Result<()> {
    check_readers("4-search", report.busy, report.errors, None)?;
    if report.generation_final != u64_of(report.batches) {
        anyhow::bail!(
            "4-search: final generation {} != batch count {}",
            report.generation_final,
            report.batches
        );
    }
    if !report.reader_generation_monotonic {
        anyhow::bail!("4-search: a reader observed a decreasing generation");
    }
    if report.read_queries < u64_of(report.readers) {
        anyhow::bail!(
            "4-search: only {} reads from {} pooled connections",
            report.read_queries,
            report.readers
        );
    }
    if report.commit_count != 1 {
        anyhow::bail!(
            "4-search: {} tantivy commits, expected exactly one deferred commit",
            report.commit_count
        );
    }
    if !report.checkpoint_consistent {
        anyhow::bail!(
            "4-search: checkpoint ({}, {}) != SQLite ({}, {})",
            report.sqlite_epoch,
            report.checkpoint_generation,
            report.sqlite_epoch,
            report.generation_final
        );
    }
    if report.open_check_us.count != OPEN_CHECK_ITERS {
        anyhow::bail!(
            "4-search: open-time generation check sampled {} of {OPEN_CHECK_ITERS} iterations",
            report.open_check_us.count
        );
    }
    Ok(())
}

/// Self-check scenario 5: the held transaction stayed on its stale snapshot,
/// a fresh transaction saw the latest generation, and the writer committed
/// every batch despite the open reader.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_wal(report: &WalSnapshotReport) -> Result<()> {
    if report.batches_with_open_reader == 0 {
        anyhow::bail!("5-wal-snapshot: no batches were written with the reader open");
    }
    if !report.held_is_stale {
        anyhow::bail!(
            "5-wal-snapshot: held transaction was not stale (before {} after {} writer {})",
            report.held_before,
            report.held_after,
            report.writer_generation
        );
    }
    if !report.fresh_is_current {
        anyhow::bail!(
            "5-wal-snapshot: fresh transaction saw {} but the writer committed {}",
            report.fresh_generation,
            report.writer_generation
        );
    }
    if !report.writer_not_blocked {
        anyhow::bail!(
            "5-wal-snapshot: writer was blocked by an open read transaction ({})",
            report.writer_error.as_deref().unwrap_or("unrecorded")
        );
    }
    Ok(())
}

/// Run every self-check for the selected scenarios; returns failure messages.
#[must_use]
pub fn self_check(summary: &RunSummary) -> Vec<String> {
    let mut failures = Vec::new();
    for named in &summary.scenarios {
        let result = match &named.report {
            ScenarioReport::Bulk(r) => self_check_bulk(r),
            ScenarioReport::Pool(r) => self_check_pool(r),
            ScenarioReport::Batch(r) => self_check_batch(r),
            ScenarioReport::Search(r) => self_check_search(r),
            ScenarioReport::WalSnapshot(r) => self_check_wal(r),
        };
        if let Err(e) = result {
            failures.push(format!("{}: {e}", named.name));
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// A one-line-per-scenario terminal summary (the sweeps print one line per
/// row).
#[must_use]
pub fn render_summary(summary: &RunSummary) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "s4_db_probe: os={} arch={} fs={} notes={} batch={} readers={}\n",
        summary.os, summary.arch, summary.fs, summary.notes, summary.batch, summary.readers
    ));
    for named in &summary.scenarios {
        match &named.report {
            ScenarioReport::Bulk(r) => out.push_str(&format!(
                "scenario 1-bulk: {} notes in {} batches ({:.0} notes/s, batch p50/p95/p99 {}/{}/{} us); reads {} p50/p95/p99/max {}/{}/{}/{} us; visibility p99 {} us; busy={} errors={}\n",
                r.notes, r.batches, r.write_throughput, r.batch_txn_us.median,
                r.batch_txn_us.p95, r.batch_txn_us.p99, r.read_queries, r.read_us.median,
                r.read_us.p95, r.read_us.p99, r.read_us.max, r.visibility_us.p99, r.busy,
                r.errors
            )),
            ScenarioReport::Pool(r) => {
                out.push_str("scenario 2-pool:\n");
                for row in &r.rows {
                    out.push_str(&format!(
                        "  readers={:<2} reads={} read p50/p95/p99 {}/{}/{} us throughput {:.0} notes/s busy={} errors={}\n",
                        row.readers, row.read_queries, row.read_us.median, row.read_us.p95,
                        row.read_us.p99, row.write_throughput, row.busy, row.errors
                    ));
                }
            }
            ScenarioReport::Batch(r) => {
                out.push_str(&format!(
                    "scenario 3-batch: bump-only FULL p50/p95 {} / {} us; NORMAL p50/p95 {} / {} us ({} iters)\n",
                    r.bump_full_us.median,
                    r.bump_full_us.p95,
                    r.bump_normal_us.median,
                    r.bump_normal_us.p95,
                    r.bump_iterations
                ));
                for row in &r.rows {
                    out.push_str(&format!(
                        "  batch={:<5} batches={} txn p50/p95/p99 {}/{}/{} us throughput {:.0} notes/s read p99 {} us busy={} errors={}\n",
                        row.batch, row.batches, row.batch_txn_us.median, row.batch_txn_us.p95,
                        row.batch_txn_us.p99, row.write_throughput, row.read_p99_us, row.busy,
                        row.errors
                    ));
                }
            }
            ScenarioReport::Search(r) => out.push_str(&format!(
                "scenario 4-search: sqlite batch p95 {} us vs tantivy apply p95 {} us (overhead {:.2}x); one commit {} us; open-check p95 {} us; checkpoint consistent={}; reads {} busy={} errors={}\n",
                r.sqlite_batch_us.p95,
                r.tantivy_batch_us.p95,
                r.apply_overhead_ratio,
                r.commit_us,
                r.open_check_us.p95,
                r.checkpoint_consistent,
                r.read_queries,
                r.busy,
                r.errors
            )),
            ScenarioReport::WalSnapshot(r) => out.push_str(&format!(
                "scenario 5-wal-snapshot: held stale={} fresh current={} writer not blocked={}; {} batches with open reader in {} us\n",
                r.held_is_stale,
                r.fresh_is_current,
                r.writer_not_blocked,
                r.batches_with_open_reader,
                r.write_with_open_reader_us
            )),
        }
    }
    out
}

impl RunSummary {
    /// Render the full markdown report (the spike doc's measured tables).
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "\n## Measured — {} ({}, {})\n\n| scenario | result |\n| --- | --- |\n",
            self.os, self.arch, self.fs
        ));
        for named in &self.scenarios {
            let cell = match &named.report {
                ScenarioReport::Bulk(r) => format!(
                    "{} notes → {} batches ({:.0} notes/s); batch p95 {} µs; reads {} p99 {} µs; visibility p99 {} µs; busy {} / errors {}",
                    r.notes,
                    r.batches,
                    r.write_throughput,
                    r.batch_txn_us.p95,
                    r.read_queries,
                    r.read_us.p99,
                    r.visibility_us.p99,
                    r.busy,
                    r.errors
                ),
                ScenarioReport::Pool(r) => format!(
                    "{} pool sizes; read p99 by size: {}; busy {} / errors {}",
                    r.rows.len(),
                    r.rows
                        .iter()
                        .map(|row| format!("{}→{} µs", row.readers, row.read_us.p99))
                        .collect::<Vec<_>>()
                        .join(", "),
                    r.rows.iter().map(|row| row.busy).sum::<u64>(),
                    r.rows.iter().map(|row| row.errors).sum::<u64>()
                ),
                ScenarioReport::Batch(r) => format!(
                    "{} batch sizes; txn p95 by size: {}; bump-only FULL p50 {} µs vs NORMAL p50 {} µs; busy {} / errors {}",
                    r.rows.len(),
                    r.rows
                        .iter()
                        .map(|row| format!("{}→{} µs", row.batch, row.batch_txn_us.p95))
                        .collect::<Vec<_>>()
                        .join(", "),
                    r.bump_full_us.median,
                    r.bump_normal_us.median,
                    r.rows.iter().map(|row| row.busy).sum::<u64>(),
                    r.rows.iter().map(|row| row.errors).sum::<u64>()
                ),
                ScenarioReport::Search(r) => format!(
                    "SQLite batch p95 {} µs + tantivy apply p95 {} µs (overhead {:.2}×); 1 commit {} µs; open-check p95 {} µs; checkpoint consistent {}",
                    r.sqlite_batch_us.p95,
                    r.tantivy_batch_us.p95,
                    r.apply_overhead_ratio,
                    r.commit_us,
                    r.open_check_us.p95,
                    r.checkpoint_consistent
                ),
                ScenarioReport::WalSnapshot(r) => format!(
                    "held stale {} / fresh current {} / writer not blocked {}; {} batches with an open reader in {} µs",
                    r.held_is_stale,
                    r.fresh_is_current,
                    r.writer_not_blocked,
                    r.batches_with_open_reader,
                    r.write_with_open_reader_us
                ),
            };
            out.push_str(&format!("| {} | {} |\n", named.name, cell));
        }

        for named in &self.scenarios {
            match &named.report {
                ScenarioReport::Pool(r) => {
                    out.push_str(
                        "\n### Scenario 2 — read-pool sweep\n\n| readers | reads | read p50 (µs) | read p95 (µs) | read p99 (µs) | read max (µs) | throughput (notes/s) | busy | errors |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n",
                    );
                    for row in &r.rows {
                        out.push_str(&format!(
                            "| {} | {} | {} | {} | {} | {} | {:.0} | {} | {} |\n",
                            row.readers,
                            row.read_queries,
                            row.read_us.median,
                            row.read_us.p95,
                            row.read_us.p99,
                            row.read_us.max,
                            row.write_throughput,
                            row.busy,
                            row.errors
                        ));
                    }
                }
                ScenarioReport::Batch(r) => {
                    out.push_str(
                        "\n### Scenario 3 — batch-size sweep\n\n| batch | batches | txn p50 (µs) | txn p95 (µs) | txn p99 (µs) | throughput (notes/s) | read p99 (µs) | busy | errors |\n| --- | --- | --- | --- | --- | --- | --- | --- | --- |\n",
                    );
                    for row in &r.rows {
                        out.push_str(&format!(
                            "| {} | {} | {} | {} | {} | {:.0} | {} | {} | {} |\n",
                            row.batch,
                            row.batches,
                            row.batch_txn_us.median,
                            row.batch_txn_us.p95,
                            row.batch_txn_us.p99,
                            row.write_throughput,
                            row.read_p99_us,
                            row.busy,
                            row.errors
                        ));
                    }
                    out.push_str(&format!(
                        "\nGeneration-bump-only transaction ({} iterations, no rows): synchronous=FULL p50 {} µs / p95 {} µs / p99 {} µs; synchronous=NORMAL p50 {} µs / p95 {} µs / p99 {} µs.\n",
                        r.bump_iterations, r.bump_full_us.median, r.bump_full_us.p95,
                        r.bump_full_us.p99, r.bump_normal_us.median, r.bump_normal_us.p95,
                        r.bump_normal_us.p99
                    ));
                }
                _ => {}
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use rusqlite::TransactionBehavior;

    use super::*;
    use crate::spike_s4_search::build_fixture;

    #[test]
    fn fresh_db_bootstraps_meta() {
        let vault = crate::spike_s3::TempVault::new("s4-db-meta");
        let path = vault.path().join("index.db");
        let conn = open_fresh_db(&path, SyncMode::Full).expect("fresh db opens");
        assert_eq!(meta_u64(&conn, "epoch").expect("epoch"), PROBE_EPOCH);
        assert_eq!(meta_u64(&conn, "generation").expect("generation"), 0);
        assert_eq!(meta_u64(&conn, "log_floor").expect("log_floor"), 0);
    }

    #[test]
    fn stale_db_is_replaced_not_reused() {
        let vault = crate::spike_s3::TempVault::new("s4-db-stale");
        let path = vault.path().join("index.db");
        let first = open_fresh_db(&path, SyncMode::Full).expect("first open");
        first
            .execute(
                "INSERT INTO meta(key,value) VALUES('generation','99')
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [],
            )
            .expect("stale generation write");
        drop(first);
        let second = open_fresh_db(&path, SyncMode::Full).expect("second open");
        assert_eq!(meta_u64(&second, "generation").expect("generation"), 0);
    }

    #[test]
    fn busy_errors_classify_as_busy() {
        let vault = crate::spike_s3::TempVault::new("s4-db-busy");
        let path = vault.path().join("index.db");
        let mut writer = open_fresh_db(&path, SyncMode::Full).expect("writer opens");
        let mut contender = Connection::open(&path).expect("contender opens");
        let held = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("immediate write transaction");
        held.execute("INSERT INTO meta(key,value) VALUES('lock','held')", [])
            .expect("write under the held lock");

        // No busy timeout on the contender: the second immediate writer must
        // fail fast with SQLITE_BUSY (or LOCKED), which `is_busy` classifies.
        let result = contender.transaction_with_behavior(TransactionBehavior::Immediate);
        let err = match result {
            Ok(_) => panic!("second immediate writer must not acquire the write lock"),
            Err(e) => e,
        };
        assert!(
            is_busy(&err),
            "expected busy/locked classification, got: {err}"
        );
        drop(held);
    }

    #[test]
    fn visibility_lag_joins_commit_times() {
        let base = Instant::now();
        let commits = vec![(1u64, base), (2u64, base + Duration::from_millis(1))];
        let mut samples = ReadSamples {
            queries: 2,
            ..ReadSamples::default()
        };
        samples
            .observations
            .push((1, base + Duration::from_micros(500)));
        samples.observations.push((
            2,
            base + Duration::from_millis(1) + Duration::from_micros(200),
        ));

        let agg = aggregate_reads(&[samples], &commits);
        assert_eq!(agg.visibility_us.count, 2);
        assert_eq!(agg.visibility_us.min, 200);
        assert_eq!(agg.visibility_us.max, 500);
        assert!(agg.gen_monotonic);
    }

    #[test]
    fn parse_selection_all_and_subsets() {
        let all = parse_selection("all").expect("all parses");
        assert!(all.bulk && all.pool && all.batch && all.search && all.wal);
        let subset = parse_selection("1,4").expect("subset parses");
        assert!(subset.bulk);
        assert!(!subset.pool);
        assert!(!subset.batch);
        assert!(subset.search);
        assert!(!subset.wal);
        assert!(parse_selection("6").is_err());
        assert!(parse_selection("").is_err());
    }

    #[test]
    fn filesystem_of_reports_a_filesystem() {
        let vault = crate::spike_s3::TempVault::new("s4-db-fs");
        let fs = filesystem_of(vault.path());
        assert!(!fs.is_empty(), "filesystem must not be empty");
        #[cfg(target_os = "linux")]
        assert_ne!(fs, "unknown", "Linux must resolve the mount's fs type");
    }

    #[test]
    fn live_bulk_overlaps_reads_with_writes() {
        let notes = build_fixture(300);
        let vault = crate::spike_s3::TempVault::new("s4-db-bulk");
        let report = run_bulk(vault.path(), &notes, 50, 4).expect("bulk runs");
        self_check_bulk(&report).expect("bulk self-check");
        assert!(report.read_queries >= 4, "every pooled connection read");
        assert!(report.visibility_us.count > 0, "visibility lag recorded");
    }

    #[test]
    fn live_pool_sweep_holds_invariants_at_every_size() {
        let notes = build_fixture(200);
        let vault = crate::spike_s3::TempVault::new("s4-db-pool");
        let report = run_pool_sweep(vault.path(), &notes, 100).expect("pool sweep runs");
        self_check_pool(&report).expect("pool self-check");
        assert_eq!(report.rows.len(), POOL_SWEEP.len());
    }

    #[test]
    fn live_batch_sweep_and_bump_microbench() {
        let notes = build_fixture(200);
        let vault = crate::spike_s3::TempVault::new("s4-db-batch");
        let report = run_batch_sweep(vault.path(), &notes, 100, 4).expect("batch sweep runs");
        self_check_batch(&report).expect("batch self-check");
        assert!(report.bump_full_us.count > 0);
        assert!(report.bump_normal_us.count > 0);
    }

    #[test]
    fn live_search_defers_one_commit_and_checks_open_time() {
        let notes = build_fixture(200);
        let vault = crate::spike_s3::TempVault::new("s4-db-search");
        let report = run_search(vault.path(), &notes, 50, 4).expect("search runs");
        self_check_search(&report).expect("search self-check");
        assert_eq!(report.commit_count, 1);
        assert_eq!(report.open_check_us.count, OPEN_CHECK_ITERS);
        assert!(report.checkpoint_consistent);
    }

    #[test]
    fn live_wal_snapshot_keeps_held_reader_stale_and_writer_free() {
        let notes = build_fixture(200);
        let vault = crate::spike_s3::TempVault::new("s4-db-wal");
        let report = run_wal_snapshot(vault.path(), &notes, 25).expect("wal snapshot runs");
        self_check_wal(&report).expect("wal self-check");
        assert!(report.held_before < report.writer_generation);
        assert_eq!(report.fresh_generation, report.writer_generation);
    }

    #[test]
    fn run_all_selected_self_checks_pass() {
        let notes = build_fixture(300);
        let vault = crate::spike_s3::TempVault::new("s4-db-all");
        let selection = parse_selection("all").expect("all parses");
        let summary = run_all(vault.path(), &notes, 50, 4, &selection).expect("run_all runs");
        let failures = self_check(&summary);
        assert!(failures.is_empty(), "self-check failures: {failures:?}");
        assert_eq!(summary.scenarios.len(), 5);

        let markdown = summary.render_markdown();
        for name in ["1-bulk", "2-pool", "3-batch", "4-search", "5-wal-snapshot"] {
            assert!(markdown.contains(name), "markdown missing {name}");
        }
        let terminal = render_summary(&summary);
        assert!(terminal.starts_with("s4_db_probe: "));
    }
}
