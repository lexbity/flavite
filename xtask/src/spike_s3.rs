//! S3 spike, slice 1 of 2 — the watcher-truth probe (PLAN-C slice C1).
//!
//! Spec basis: §9 Phase 0 spike S3 ("watcher behavior per OS: rename events,
//! overflow, coalescing") and D5 (watcher events are hints; overflow → full
//! reconcile; periodic + focus reconcile; drift counter; racy-file rule).
//!
//! The probe records raw [`notify`] event streams for scripted filesystem
//! scenarios on the host OS and measures the per-OS behavior D5's parameters
//! are built on: single-modify delivery latency, rename pairing (inotify
//! cookie / FSEvents flags / `RENAMED_OLD_NAME` + `RENAMED_NEW_NAME`),
//! overflow signaling, coalescing, and silent loss. It uses the same crate and
//! major version as the production watcher wrapper (C5 `watch.rs`) will.
//!
//! The probe is a dev tool, not a product component. Its `--self-check` mode
//! asserts the D5 nomination property — *changed paths are eventually
//! nominated, or an explicit overflow/rescan signal is observed* — which CI
//! re-runs on Linux headless (scenarios 1–3). Raw event counts are
//! measurements and are never asserted to equal operation counts.
//!
//! Findings are recorded in `devdocs/spikes/S3-watcher.md`.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;

// ---------------------------------------------------------------------------
// D5 parameters under test (§4.4 event coalescing; named here so the reports
// and the spike doc cite them directly)
// ---------------------------------------------------------------------------

/// Quiet window: events for a path are keyed and coalesced until this much
/// time passes with no further event for that path (§4.4).
pub const QUIET_WINDOW: Duration = Duration::from_millis(300);

/// Hard cap on the coalescing window while events keep arriving (§4.4).
pub const MAX_QUIET_WINDOW: Duration = Duration::from_millis(2_000);

/// More events than this within one second enters bulk mode (§4.4).
pub const BULK_THRESHOLD_EVENTS_PER_SEC: u64 = 500;

/// A file with `mtime >= scan_time - RACY_WINDOW` is re-hashed next pass (D5).
pub const RACY_WINDOW: Duration = Duration::from_secs(2);

/// A full content-verification pass runs at least this often while the vault
/// is available (D5).
pub const VERIFY_INTERVAL: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// Deterministic scenario shapes (fixed counts, fixed names)
// ---------------------------------------------------------------------------

/// Scenario 1: scripted modifications of one note.
pub const MODIFY_OPS: usize = 20;

/// Spacing between scenario 1 modifications.
pub const MODIFY_SPACING: Duration = Duration::from_millis(50);

/// Scenario 3: files created then deleted in the storm.
pub const STORM_FILES: usize = 10_000;

/// Scenario 4: back-to-back writes in the tight burst phase.
pub const BURST_TIGHT_OPS: usize = 50;

/// Scenario 4: writes in each spaced phase.
pub const BURST_SPACED_OPS: usize = 10;

/// Scenario 4: spacings probed to bracket the OS-imposed coalescing window.
pub const BURST_SPACINGS_MS: [u64; 8] = [5, 10, 25, 50, 100, 200, 400, 800];

// Timeouts. Generous on purpose: the probe measures, and a slow CI box must
// never turn a measurement into a false failure.
const WATCHER_WARMUP: Duration = Duration::from_millis(250);
const SETTLE_QUIET: Duration = Duration::from_millis(300);
const SETTLE_CAP: Duration = Duration::from_secs(5);
const MODIFY_QUIET: Duration = Duration::from_millis(500);
const NOMINATE_CAP: Duration = Duration::from_secs(15);
const STORM_QUIET: Duration = Duration::from_millis(500);
const STORM_CAP: Duration = Duration::from_secs(60);
const BURST_QUIET: Duration = Duration::from_millis(300);
const BURST_CAP: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Scripted operations
// ---------------------------------------------------------------------------

/// A scripted filesystem operation. Scenario builders are pure and
/// deterministic (fixed counts, fixed names); runners execute them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Create a file with fixed contents.
    Create(PathBuf),
    /// Append one line to a file.
    Write(PathBuf),
    /// Rename `from` to `to`.
    Rename {
        /// Source path.
        from: PathBuf,
        /// Destination path.
        to: PathBuf,
    },
    /// Delete a file.
    Delete(PathBuf),
}

/// Scenario 1 script: `ops` appends to `<dir>/note.md`.
#[must_use]
pub fn modify_script(dir: &Path, ops: usize) -> Vec<Op> {
    let note = dir.join("note.md");
    (0..ops).map(|_| Op::Write(note.clone())).collect()
}

/// Scenario 2 script: create `<dir>/ideas.md`, then rename it to
/// `concepts.md`.
#[must_use]
pub fn rename_script(dir: &Path) -> Vec<Op> {
    vec![
        Op::Create(dir.join("ideas.md")),
        Op::Rename {
            from: dir.join("ideas.md"),
            to: dir.join("concepts.md"),
        },
    ]
}

/// The deterministic storm file name for index `i`.
#[must_use]
pub fn storm_name(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("storm-{i:05}.md"))
}

/// Scenario 3 script: create `files` storm files, then delete them all.
#[must_use]
pub fn storm_script(dir: &Path, files: usize) -> Vec<Op> {
    let mut ops = Vec::with_capacity(files.saturating_mul(2));
    for i in 0..files {
        ops.push(Op::Create(storm_name(dir, i)));
    }
    for i in 0..files {
        ops.push(Op::Delete(storm_name(dir, i)));
    }
    ops
}

/// One burst phase of scenario 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BurstPhase {
    /// Phase label recorded in the report.
    pub label: String,
    /// Spacing between writes (`Duration::ZERO` = back-to-back).
    pub spacing: Duration,
    /// Number of writes in the phase.
    pub ops: usize,
}

/// Scenario 4 phases: one tight burst, then spaced probes bracketing the
/// OS-imposed coalescing window.
#[must_use]
pub fn burst_phases() -> Vec<BurstPhase> {
    let mut phases = vec![BurstPhase {
        label: "tight".to_string(),
        spacing: Duration::ZERO,
        ops: BURST_TIGHT_OPS,
    }];
    for ms in BURST_SPACINGS_MS {
        phases.push(BurstPhase {
            label: format!("spaced-{ms}ms"),
            spacing: Duration::from_millis(ms),
            ops: BURST_SPACED_OPS,
        });
    }
    phases
}

/// Apply one scripted operation to the real filesystem.
///
/// # Errors
/// Returns an error if the filesystem operation fails.
fn apply_op(op: &Op) -> Result<()> {
    match op {
        Op::Create(path) => {
            fs::write(path, b"# storm\n").context("create failed")?;
        }
        Op::Write(path) => {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(path)
                .with_context(|| format!("open for append failed: {}", path.display()))?;
            file.write_all(b"line\n").context("append failed")?;
        }
        Op::Rename { from, to } => {
            fs::rename(from, to)
                .with_context(|| format!("rename {} -> {} failed", from.display(), to.display()))?;
        }
        Op::Delete(path) => {
            fs::remove_file(path).with_context(|| format!("delete failed: {}", path.display()))?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Raw events and pure measurement helpers
// ---------------------------------------------------------------------------

/// A raw watcher event, captured for measurement. Paths are relative to the
/// scenario directory so reports are machine-comparable across runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RawEvent {
    /// Milliseconds since scenario start.
    pub at_ms: u64,
    /// Debug rendering of the notify `EventKind` (e.g. `Modify(Data(Any))`).
    pub kind: String,
    /// Event paths, relative to the scenario directory.
    pub paths: Vec<PathBuf>,
    /// The notify tracker attribute (the inotify rename cookie on Linux).
    pub tracker: Option<usize>,
    /// Whether the event carries the rescan flag (explicit overflow/loss
    /// signal, D5's "explicit overflow" branch).
    pub rescan: bool,
    /// The notify info attribute, when present (e.g. FSEvents drop reasons).
    pub info: Option<String>,
}

impl RawEvent {
    fn capture(event: &Event, root: &Path, at_ms: u64) -> Self {
        Self {
            at_ms,
            kind: format!("{:?}", event.kind),
            paths: event.paths.iter().map(|p| rel(root, p)).collect(),
            tracker: event.tracker(),
            rescan: event.need_rescan(),
            info: event.info().map(str::to_string),
        }
    }

    fn synthetic(kind: String, at_ms: u64, rescan: bool) -> Self {
        Self {
            at_ms,
            kind,
            paths: Vec::new(),
            tracker: None,
            rescan,
            info: None,
        }
    }

    /// Whether the event nominates exactly this (relative) path.
    fn mentions(&self, path: &Path) -> bool {
        self.paths.iter().any(|p| p == path)
    }

    /// Whether the event nominates `path` — exactly, or via an ancestor
    /// directory event (FSEvents may report directory-level events;
    /// reconciling that directory still finds the file).
    fn covers(&self, path: &Path) -> bool {
        self.mentions(path) || path.ancestors().skip(1).any(|a| self.mentions(a))
    }
}

/// Make `path` relative to `root` when possible (reports are relative).
fn rel(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

/// Milliseconds elapsed on `t0`'s clock, saturating at `u64::MAX`.
fn ms_since(t0: Instant) -> u64 {
    u64::try_from(t0.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Delivery-latency statistics over samples in one unit.
///
/// The unit is whatever the caller recorded: the watcher probe (C1) records
/// milliseconds, the filesystem probe (C2) records microseconds. Field names
/// are deliberately unit-neutral; renderers state the unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct LatencyStats {
    /// Number of samples.
    pub count: usize,
    /// Minimum sample.
    pub min: u64,
    /// Upper-median sample.
    pub median: u64,
    /// 95th-percentile sample (nearest rank).
    pub p95: u64,
    /// 99th-percentile sample (nearest rank; PLAN-C C4 reads p99 during writes).
    pub p99: u64,
    /// Maximum sample.
    pub max: u64,
}

/// Nearest-rank latency statistics. Empty input yields an all-zero report.
#[must_use]
pub fn latency_stats(samples: &[u64]) -> LatencyStats {
    if samples.is_empty() {
        return LatencyStats {
            count: 0,
            min: 0,
            median: 0,
            p95: 0,
            p99: 0,
            max: 0,
        };
    }
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let last = sorted.len() - 1;
    LatencyStats {
        count: sorted.len(),
        min: sorted[0],
        median: sorted[sorted.len() / 2],
        p95: sorted[(sorted.len() * 95 / 100).min(last)],
        p99: sorted[(sorted.len() * 99 / 100).min(last)],
        max: sorted[last],
    }
}

/// Nomination coverage of an expected path set against nominated paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Coverage {
    /// Number of expected paths.
    pub expected: usize,
    /// Expected paths with an event naming exactly that path.
    pub exact: usize,
    /// Expected paths covered only by an event naming an ancestor directory.
    pub ancestor: usize,
    /// Expected paths with no nominating event at all (silent loss).
    pub uncovered: usize,
    /// Whether an explicit overflow/rescan signal was observed.
    pub overflow_observed: bool,
}

impl Coverage {
    /// The D5 nomination property: every expected path is nominated (exactly
    /// or via an ancestor directory event), or an explicit overflow signal
    /// was observed (overflow triggers a full reconcile, so nomination is not
    /// required).
    #[must_use]
    pub fn property_holds(&self) -> bool {
        self.uncovered == 0 || self.overflow_observed
    }
}

/// Compute coverage of `expected` (relative paths) by `nominated` (relative
/// paths). A path is covered exactly when nominated itself, or via ancestor
/// when an event named one of its ancestor directories.
#[must_use]
pub fn coverage(
    expected: &HashSet<PathBuf>,
    nominated: &HashSet<PathBuf>,
    overflow: bool,
) -> Coverage {
    let mut exact = 0;
    let mut ancestor = 0;
    for path in expected {
        if nominated.contains(path) {
            exact += 1;
        } else if path.ancestors().skip(1).any(|a| nominated.contains(a)) {
            ancestor += 1;
        }
    }
    let total = expected.len();
    Coverage {
        expected: total,
        exact,
        ancestor,
        uncovered: total - exact - ancestor,
        overflow_observed: overflow,
    }
}

/// How the two sides of a rename can be paired from the raw event stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Pairing {
    /// One event carries both paths (notify synthesizes `RenameMode::Both`
    /// on Linux from matching inotify cookies).
    BothEvent,
    /// Two events share a tracker value (the inotify rename cookie).
    Cookie,
    /// A `From` event is immediately followed by a `To` event (Windows
    /// `RENAMED_OLD_NAME` + `RENAMED_NEW_NAME`; no cookie).
    Adjacent,
    /// Rename sides are observable but no pairing structure exists (macOS
    /// FSEvents reports `RenameMode::Any` per path with no cookie).
    Unpaired,
    /// No event mentions either side (silent loss).
    None,
}

/// Classify the rename-pairing mechanism from raw events. `from`/`to` are
/// relative to the scenario directory.
#[must_use]
pub fn classify_pairing(events: &[RawEvent], from: &Path, to: &Path) -> Pairing {
    if events.iter().any(|e| e.mentions(from) && e.mentions(to)) {
        return Pairing::BothEvent;
    }
    let from_trackers: HashSet<usize> = events
        .iter()
        .filter(|e| e.mentions(from))
        .filter_map(|e| e.tracker)
        .collect();
    let to_trackers: HashSet<usize> = events
        .iter()
        .filter(|e| e.mentions(to))
        .filter_map(|e| e.tracker)
        .collect();
    if from_trackers.intersection(&to_trackers).next().is_some() {
        return Pairing::Cookie;
    }
    let adjacent = events.windows(2).any(|w| {
        w[0].kind.contains("From")
            && w[0].mentions(from)
            && w[1].kind.contains("To")
            && w[1].mentions(to)
    });
    if adjacent {
        return Pairing::Adjacent;
    }
    if events.iter().any(|e| e.mentions(from) || e.mentions(to)) {
        return Pairing::Unpaired;
    }
    Pairing::None
}

/// Event-kind histogram.
fn kind_histogram<'a>(events: impl Iterator<Item = &'a RawEvent>) -> BTreeMap<String, u64> {
    let mut histogram = BTreeMap::new();
    for event in events {
        *histogram.entry(event.kind.clone()).or_insert(0) += 1;
    }
    histogram
}

/// Events beyond the first per distinct expected path (duplicate nominations).
fn count_duplicates(events: &[RawEvent], expected: &HashSet<PathBuf>) -> u64 {
    let mut seen = HashSet::new();
    let mut duplicates = 0_u64;
    for event in events {
        for path in &event.paths {
            if expected.contains(path) && !seen.insert(path.clone()) {
                duplicates += 1;
            }
        }
    }
    duplicates
}

// ---------------------------------------------------------------------------
// Watcher harness
// ---------------------------------------------------------------------------

/// A running watcher over one scenario directory. A background collector
/// thread timestamps events at receipt, so latency measurements are true
/// delivery latencies rather than drain-time artifacts.
struct Harness {
    watcher: Option<RecommendedWatcher>,
    events: Arc<Mutex<Vec<RawEvent>>>,
    collector: Option<thread::JoinHandle<()>>,
    stopped: Arc<AtomicBool>,
    /// Length of the event log already returned by a previous `drain`.
    drained: usize,
    t0: Instant,
}

impl Harness {
    /// Start a recursive watch on `root` (which must exist).
    ///
    /// # Errors
    /// Returns an error if the watcher cannot be created or the watch fails.
    fn start(root: &Path) -> Result<Self> {
        let (tx, rx) = mpsc::channel();
        let mut watcher = RecommendedWatcher::new(tx, Config::default())
            .context("failed to create the recommended watcher")?;
        watcher
            .watch(root, RecursiveMode::Recursive)
            .with_context(|| format!("failed to watch {}", root.display()))?;
        let events: Arc<Mutex<Vec<RawEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let stopped = Arc::new(AtomicBool::new(false));
        let collector_events = Arc::clone(&events);
        let collector_stopped = Arc::clone(&stopped);
        let collector_root = root.to_path_buf();
        let t0 = Instant::now();
        let collector = thread::spawn(move || {
            while let Ok(item) = rx.recv() {
                let at_ms = ms_since(t0);
                let event = match item {
                    Ok(event) => RawEvent::capture(&event, &collector_root, at_ms),
                    Err(err) => RawEvent::synthetic(format!("watcher-error: {err}"), at_ms, true),
                };
                collector_events
                    .lock()
                    .expect("events lock not poisoned")
                    .push(event);
            }
            // The channel disconnected. If the harness was not stopped
            // explicitly, the watcher died mid-scenario: record it once.
            if !collector_stopped.load(Ordering::SeqCst) {
                let at_ms = ms_since(t0);
                collector_events
                    .lock()
                    .expect("events lock not poisoned")
                    .push(RawEvent::synthetic(
                        "watcher-disconnected".to_string(),
                        at_ms,
                        false,
                    ));
            }
        });
        Ok(Self {
            watcher: Some(watcher),
            events,
            collector: Some(collector),
            stopped,
            drained: 0,
            t0,
        })
    }

    /// Milliseconds since the harness started.
    fn at(&self) -> u64 {
        ms_since(self.t0)
    }

    /// The full event log so far (receipt-ordered).
    fn collected(&self) -> Vec<RawEvent> {
        self.events
            .lock()
            .expect("events lock not poisoned")
            .clone()
    }

    /// Wait until `quiet` elapses with no new event, or `cap` total, then
    /// return the events collected **since the previous drain** (phase
    /// attribution: each drain window is disjoint from the ones before it).
    fn drain(&mut self, quiet: Duration, cap: Duration) -> Vec<RawEvent> {
        let deadline = Instant::now() + cap;
        let poll = Duration::from_millis(10).min(quiet);
        let mut last_len = self.collected().len();
        let mut last_activity = Instant::now();
        loop {
            thread::sleep(poll);
            let len = self.collected().len();
            if len != last_len {
                last_len = len;
                last_activity = Instant::now();
            }
            let now = Instant::now();
            if now >= deadline || now.duration_since(last_activity) >= quiet {
                break;
            }
        }
        let all = self.collected();
        let start = self.drained.min(all.len());
        let fresh = all[start..].to_vec();
        self.drained = all.len();
        fresh
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Mark stopped, then drop the watcher: the channel disconnects, the
        // collector thread exits, and the join below cannot hang.
        self.stopped.store(true, Ordering::SeqCst);
        drop(self.watcher.take());
        if let Some(collector) = self.collector.take() {
            let _ = collector.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Scenario reports
// ---------------------------------------------------------------------------

/// Scenario 1 report: single-file modify delivery latency.
#[derive(Debug, Clone, Serialize)]
pub struct ModifyReport {
    /// Scripted operations.
    pub ops: usize,
    /// Operations nominated by an event within the timeout.
    pub nominated: usize,
    /// Per-operation delivery latency (ms), in op order; `None` = unnominated.
    pub latencies_ms: Vec<Option<u64>>,
    /// Latency statistics over the nominated samples.
    pub stats: LatencyStats,
    /// All captured events.
    pub events: Vec<RawEvent>,
    /// Event-kind histogram.
    pub kind_histogram: BTreeMap<String, u64>,
    /// Nominating events beyond the first per operation (duplicates).
    pub duplicates: u64,
}

/// Scenario 2 report: rename pairing.
#[derive(Debug, Clone, Serialize)]
pub struct RenameReport {
    /// Raw events observed after the rename.
    pub events: Vec<RawEvent>,
    /// The pairing mechanism the OS/notify exposes.
    pub pairing: Pairing,
    /// Whether the source path was nominated.
    pub from_nominated: bool,
    /// Whether the destination path was nominated.
    pub to_nominated: bool,
}

/// Scenario 3 report: create/delete storm.
#[derive(Debug, Clone, Serialize)]
pub struct StormReport {
    /// Storm file count.
    pub files: usize,
    /// Create operations scripted.
    pub ops_create: usize,
    /// Delete operations scripted.
    pub ops_delete: usize,
    /// Events captured in the create phase.
    pub events_create: usize,
    /// Events captured in the delete phase.
    pub events_delete: usize,
    /// Create-phase coverage.
    pub create_coverage: Coverage,
    /// Delete-phase coverage.
    pub delete_coverage: Coverage,
    /// Duplicate nominations, create phase.
    pub create_duplicates: u64,
    /// Duplicate nominations, delete phase.
    pub delete_duplicates: u64,
    /// Overflow/rescan signals observed (both phases).
    pub overflow_signals: u64,
    /// Event-kind histogram (both phases).
    pub kind_histogram: BTreeMap<String, u64>,
    /// Arrival buckets: (bucket start ms, events) over both phases.
    pub arrival_buckets_100ms: Vec<(u64, u64)>,
}

/// Scenario 4 report: burst pattern / OS coalescing window.
#[derive(Debug, Clone, Serialize)]
pub struct BurstReport {
    /// One report per burst phase.
    pub phases: Vec<BurstPhaseReport>,
}

/// One scenario 4 phase.
#[derive(Debug, Clone, Serialize)]
pub struct BurstPhaseReport {
    /// Phase label.
    pub label: String,
    /// Spacing between writes (ms; 0 = back-to-back).
    pub spacing_ms: u64,
    /// Writes in the phase.
    pub ops: usize,
    /// Events captured for the phase.
    pub events: usize,
    /// Distinct paths nominated.
    pub distinct_paths: usize,
    /// The burst file was nominated at least once.
    pub nominated: bool,
    /// Trailing latency: last event at − last op at (ms).
    pub trailing_ms: u64,
}

// ---------------------------------------------------------------------------
// Scenario runners
// ---------------------------------------------------------------------------

/// Run scenario 1: scripted modifies of one note; measure delivery latency.
///
/// # Errors
/// Returns an error if the scenario directory or watcher cannot be set up.
pub fn run_modify(dir: &Path) -> Result<ModifyReport> {
    fs::create_dir_all(dir).context("scenario dir")?;
    let note = dir.join("note.md");
    fs::write(&note, b"# note\n").context("seed note")?;
    let mut harness = Harness::start(dir)?;
    thread::sleep(WATCHER_WARMUP);
    let _ = harness.drain(SETTLE_QUIET, SETTLE_CAP);

    let rel_note = PathBuf::from("note.md");
    let script = modify_script(dir, MODIFY_OPS);
    let mut op_times = Vec::with_capacity(script.len());
    for op in &script {
        apply_op(op)?;
        op_times.push(harness.at());
        thread::sleep(MODIFY_SPACING);
    }
    let events = harness.drain(MODIFY_QUIET, NOMINATE_CAP);

    let nominating: Vec<&RawEvent> = events.iter().filter(|e| e.covers(&rel_note)).collect();
    let mut latencies = Vec::with_capacity(op_times.len());
    let mut nominated = 0;
    for &op_at in &op_times {
        let latency = nominating
            .iter()
            .find(|e| e.at_ms >= op_at)
            .map(|e| e.at_ms.saturating_sub(op_at));
        if latency.is_some() {
            nominated += 1;
        }
        latencies.push(latency);
    }
    let samples: Vec<u64> = latencies.iter().flatten().copied().collect();
    let duplicates = u64::try_from(nominating.len().saturating_sub(nominated)).unwrap_or(u64::MAX);
    Ok(ModifyReport {
        ops: script.len(),
        nominated,
        latencies_ms: latencies,
        stats: latency_stats(&samples),
        kind_histogram: kind_histogram(events.iter()),
        duplicates,
        events,
    })
}

/// Run scenario 2: rename one note; record the raw rename event stream.
///
/// # Errors
/// Returns an error if the scenario directory or watcher cannot be set up.
pub fn run_rename(dir: &Path) -> Result<RenameReport> {
    fs::create_dir_all(dir).context("scenario dir")?;
    let script = rename_script(dir);
    let create = script.first().context("rename_script yields a create op")?;
    let rename = script.get(1).context("rename_script yields a rename op")?;
    apply_op(create)?;
    let mut harness = Harness::start(dir)?;
    thread::sleep(WATCHER_WARMUP);
    let _ = harness.drain(SETTLE_QUIET, SETTLE_CAP);
    apply_op(rename)?;
    let events = harness.drain(SETTLE_QUIET, SETTLE_CAP);
    let from = PathBuf::from("ideas.md");
    let to = PathBuf::from("concepts.md");
    Ok(RenameReport {
        from_nominated: events.iter().any(|e| e.covers(&from)),
        to_nominated: events.iter().any(|e| e.covers(&to)),
        pairing: classify_pairing(&events, &from, &to),
        events,
    })
}

/// Run scenario 3: create then delete `files` files; measure nomination,
/// coalescing, duplicates, overflow, and arrival shape.
///
/// # Errors
/// Returns an error if the scenario directory or watcher cannot be set up.
pub fn run_storm(dir: &Path, files: usize) -> Result<StormReport> {
    fs::create_dir_all(dir).context("scenario dir")?;
    let mut harness = Harness::start(dir)?;
    thread::sleep(WATCHER_WARMUP);

    let script = storm_script(dir, files);
    let (creates, deletes) = script.split_at(files);
    for op in creates {
        apply_op(op)?;
    }
    let create_events = harness.drain(STORM_QUIET, STORM_CAP);
    for op in deletes {
        apply_op(op)?;
    }
    let delete_events = harness.drain(STORM_QUIET, STORM_CAP);

    let expected: HashSet<PathBuf> = (0..files).map(|i| rel(dir, &storm_name(dir, i))).collect();
    let nominated_create: HashSet<PathBuf> =
        create_events.iter().flat_map(|e| e.paths.clone()).collect();
    let nominated_delete: HashSet<PathBuf> =
        delete_events.iter().flat_map(|e| e.paths.clone()).collect();
    let overflow_create = create_events.iter().any(|e| e.rescan);
    let overflow_delete = delete_events.iter().any(|e| e.rescan);
    let overflow_signals = create_events
        .iter()
        .chain(delete_events.iter())
        .filter(|e| e.rescan)
        .count();

    let mut buckets: BTreeMap<u64, u64> = BTreeMap::new();
    for event in create_events.iter().chain(delete_events.iter()) {
        *buckets.entry(event.at_ms / 100).or_insert(0) += 1;
    }

    Ok(StormReport {
        files,
        ops_create: creates.len(),
        ops_delete: deletes.len(),
        events_create: create_events.len(),
        events_delete: delete_events.len(),
        create_coverage: coverage(&expected, &nominated_create, overflow_create),
        delete_coverage: coverage(&expected, &nominated_delete, overflow_delete),
        create_duplicates: count_duplicates(&create_events, &expected),
        delete_duplicates: count_duplicates(&delete_events, &expected),
        overflow_signals: u64::try_from(overflow_signals).unwrap_or(u64::MAX),
        kind_histogram: kind_histogram(create_events.iter().chain(delete_events.iter())),
        arrival_buckets_100ms: buckets.into_iter().collect(),
    })
}

/// Run scenario 4 with the standard phase list: a tight burst plus spaced
/// probes bracketing the OS-imposed coalescing (quiet) window.
///
/// # Errors
/// Returns an error if the scenario directory or watcher cannot be set up.
pub fn run_burst(dir: &Path) -> Result<BurstReport> {
    run_burst_phases(dir, &burst_phases())
}

/// Run scenario 4 over a custom phase list (the standard list comes from
/// [`burst_phases`]).
///
/// # Errors
/// Returns an error if the scenario directory or watcher cannot be set up.
pub fn run_burst_phases(dir: &Path, phases: &[BurstPhase]) -> Result<BurstReport> {
    fs::create_dir_all(dir).context("scenario dir")?;
    let note = dir.join("burst.md");
    fs::write(&note, b"# burst\n").context("seed note")?;
    let mut harness = Harness::start(dir)?;
    thread::sleep(WATCHER_WARMUP);
    let _ = harness.drain(SETTLE_QUIET, SETTLE_CAP);

    let rel_note = PathBuf::from("burst.md");
    let mut reports = Vec::with_capacity(phases.len());
    for phase in phases {
        let mut last_op_at = harness.at();
        for _ in 0..phase.ops {
            apply_op(&Op::Write(dir.join("burst.md")))?;
            last_op_at = harness.at();
            if !phase.spacing.is_zero() {
                thread::sleep(phase.spacing);
            }
        }
        let events = harness.drain(BURST_QUIET, BURST_CAP);
        let nominated_paths: HashSet<PathBuf> =
            events.iter().flat_map(|e| e.paths.clone()).collect();
        let trailing_ms = events
            .last()
            .map(|e| e.at_ms.saturating_sub(last_op_at))
            .unwrap_or(0);
        reports.push(BurstPhaseReport {
            spacing_ms: u64::try_from(phase.spacing.as_millis()).unwrap_or(u64::MAX),
            label: phase.label.clone(),
            ops: phase.ops,
            events: events.len(),
            distinct_paths: nominated_paths.len(),
            nominated: events.iter().any(|e| e.covers(&rel_note)),
            trailing_ms,
        });
    }
    Ok(BurstReport { phases: reports })
}

// ---------------------------------------------------------------------------
// Run summary, selection, self-check
// ---------------------------------------------------------------------------

/// Which scenarios to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScenarioSelection {
    /// Scenario 1: single-file modify latency.
    pub modify: bool,
    /// Scenario 2: rename pairing.
    pub rename: bool,
    /// Scenario 3: create/delete storm.
    pub storm: bool,
    /// Scenario 4: burst pattern.
    pub burst: bool,
}

/// Parse a scenario list: `all` or a comma-separated subset of `1,2,3,4`.
///
/// # Errors
/// Returns an error on an unknown or empty scenario selection.
pub fn parse_selection(input: &str) -> Result<ScenarioSelection> {
    if input == "all" {
        return Ok(ScenarioSelection {
            modify: true,
            rename: true,
            storm: true,
            burst: true,
        });
    }
    let mut selection = ScenarioSelection::default();
    for part in input.split(',') {
        match part.trim() {
            "1" => selection.modify = true,
            "2" => selection.rename = true,
            "3" => selection.storm = true,
            "4" => selection.burst = true,
            other => {
                anyhow::bail!("unknown scenario '{other}' (expected 1, 2, 3, 4, or all)");
            }
        }
    }
    if !(selection.modify || selection.rename || selection.storm || selection.burst) {
        anyhow::bail!("empty scenario selection");
    }
    Ok(selection)
}

/// The recommended watcher backend on this OS (notify's naming).
#[must_use]
pub const fn watcher_backend() -> &'static str {
    if cfg!(target_os = "linux") {
        "inotify"
    } else if cfg!(target_os = "macos") {
        "FSEvents"
    } else if cfg!(target_os = "windows") {
        "ReadDirectoryChangesW"
    } else {
        "other (kqueue/poll)"
    }
}

/// One scenario's report, tagged with its scenario id.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "scenario", rename_all = "snake_case")]
pub enum ScenarioReport {
    /// Scenario 1.
    Modify(ModifyReport),
    /// Scenario 2.
    Rename(RenameReport),
    /// Scenario 3.
    Storm(StormReport),
    /// Scenario 4.
    Burst(BurstReport),
}

/// A named scenario report.
#[derive(Debug, Clone, Serialize)]
pub struct NamedReport {
    /// Scenario name (`1-modify`, `2-rename`, `3-storm`, `4-burst`).
    pub name: String,
    /// The report.
    pub report: ScenarioReport,
}

/// A full probe run.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    /// Host OS (`std::env::consts::OS`).
    pub os: String,
    /// Host architecture.
    pub arch: String,
    /// The notify recommended watcher backend on this OS.
    pub watcher_backend: String,
    /// The notify version note (the workspace pins the major; the exact locked
    /// version is recorded in the spike doc from `Cargo.lock`).
    pub notify: String,
    /// Scenario reports, in scenario order.
    pub scenarios: Vec<NamedReport>,
}

/// Run the selected scenarios, each in a fresh subdirectory of `vault`.
///
/// # Errors
/// Returns an error if any scenario fails to set up.
pub fn run_all(
    vault: &Path,
    selection: &ScenarioSelection,
    storm_files: usize,
) -> Result<RunSummary> {
    let mut scenarios = Vec::new();
    if selection.modify {
        let report = run_modify(&vault.join("s1-modify"))?;
        scenarios.push(NamedReport {
            name: "1-modify".to_string(),
            report: ScenarioReport::Modify(report),
        });
    }
    if selection.rename {
        let report = run_rename(&vault.join("s2-rename"))?;
        scenarios.push(NamedReport {
            name: "2-rename".to_string(),
            report: ScenarioReport::Rename(report),
        });
    }
    if selection.storm {
        let report = run_storm(&vault.join("s3-storm"), storm_files)?;
        scenarios.push(NamedReport {
            name: "3-storm".to_string(),
            report: ScenarioReport::Storm(report),
        });
    }
    if selection.burst {
        let report = run_burst(&vault.join("s4-burst"))?;
        scenarios.push(NamedReport {
            name: "4-burst".to_string(),
            report: ScenarioReport::Burst(report),
        });
    }
    Ok(RunSummary {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        watcher_backend: watcher_backend().to_string(),
        notify: "8 (workspace-pinned major)".to_string(),
        scenarios,
    })
}

/// Self-check scenario 1: every scripted modification was nominated.
///
/// # Errors
/// Returns an error naming the unnominated operations.
pub fn self_check_modify(report: &ModifyReport) -> Result<()> {
    if report.nominated == report.ops {
        Ok(())
    } else {
        anyhow::bail!(
            "{} of {} modifications were not nominated within the timeout",
            report.ops - report.nominated,
            report.ops
        )
    }
}

/// Self-check scenario 2: the rename was observable (not silently lost).
///
/// # Errors
/// Returns an error if no event mentions either side of the rename.
pub fn self_check_rename(report: &RenameReport) -> Result<()> {
    if report.pairing == Pairing::None {
        anyhow::bail!("rename was not observable: no event mentions either side")
    } else {
        Ok(())
    }
}

/// Self-check scenario 3: the D5 nomination property holds for both phases —
/// every changed path was nominated, or an explicit overflow/rescan signal
/// was observed (overflow triggers a full reconcile).
///
/// # Errors
/// Returns an error describing the silent loss.
pub fn self_check_storm(report: &StormReport) -> Result<()> {
    if report.create_coverage.property_holds() && report.delete_coverage.property_holds() {
        Ok(())
    } else {
        anyhow::bail!(
            "silent loss: create uncovered {}/{}, delete uncovered {}/{}, overflow signals {}",
            report.create_coverage.uncovered,
            report.create_coverage.expected,
            report.delete_coverage.uncovered,
            report.delete_coverage.expected,
            report.overflow_signals
        )
    }
}

/// Self-check scenario 4: every burst phase nominated the file at least once
/// (a burst must never be entirely silent).
///
/// # Errors
/// Returns an error naming the silent phases.
pub fn self_check_burst(report: &BurstReport) -> Result<()> {
    let silent: Vec<&str> = report
        .phases
        .iter()
        .filter(|p| !p.nominated)
        .map(|p| p.label.as_str())
        .collect();
    if silent.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("burst phases never nominated: {}", silent.join(", "))
    }
}

/// Run the self-checks for every scenario in the summary. Returns the list of
/// failures; an empty list means the D5 nomination property held everywhere.
#[must_use]
pub fn self_check(summary: &RunSummary) -> Vec<String> {
    let mut failures = Vec::new();
    for named in &summary.scenarios {
        let result = match &named.report {
            ScenarioReport::Modify(r) => self_check_modify(r),
            ScenarioReport::Rename(r) => self_check_rename(r),
            ScenarioReport::Storm(r) => self_check_storm(r),
            ScenarioReport::Burst(r) => self_check_burst(r),
        };
        if let Err(failure) = result {
            failures.push(format!("scenario {}: {failure}", named.name));
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

impl RunSummary {
    /// Render the measured matrix as markdown tables (for the spike doc).
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = format!(
            "Measured on {} ({}, {} backend, notify {}).\n\n",
            self.os, self.arch, self.watcher_backend, self.notify
        );
        for named in &self.scenarios {
            match &named.report {
                ScenarioReport::Modify(r) => render_modify(&mut out, r),
                ScenarioReport::Rename(r) => render_rename(&mut out, r),
                ScenarioReport::Storm(r) => render_storm(&mut out, r),
                ScenarioReport::Burst(r) => render_burst(&mut out, r),
            }
        }
        out
    }
}

/// Render a compact human-readable summary (bin stdout).
#[must_use]
pub fn render_summary(summary: &RunSummary) -> String {
    let mut out = format!(
        "s3_watcher_probe: os={} arch={} backend={} notify={}\n",
        summary.os, summary.arch, summary.watcher_backend, summary.notify
    );
    for named in &summary.scenarios {
        out.push_str(&format!("scenario {}: ", named.name));
        match &named.report {
            ScenarioReport::Modify(r) => out.push_str(&format!(
                "{} ops, {}/{} nominated, latency median {} ms / p95 {} ms, {} events",
                r.ops,
                r.nominated,
                r.ops,
                r.stats.median,
                r.stats.p95,
                r.events.len()
            )),
            ScenarioReport::Rename(r) => out.push_str(&format!(
                "pairing {:?}, from={} to={}, {} events",
                r.pairing,
                r.from_nominated,
                r.to_nominated,
                r.events.len()
            )),
            ScenarioReport::Storm(r) => out.push_str(&format!(
                "{} files, create exact {}/{}, delete exact {}/{}, duplicates {}/{}, overflow {}",
                r.files,
                r.create_coverage.exact,
                r.create_coverage.expected,
                r.delete_coverage.exact,
                r.delete_coverage.expected,
                r.create_duplicates,
                r.delete_duplicates,
                r.overflow_signals
            )),
            ScenarioReport::Burst(r) => {
                let tight = r.phases.first();
                out.push_str(&format!(
                    "{} phases, tight burst {} events for {} ops",
                    r.phases.len(),
                    tight.map(|p| p.events).unwrap_or(0),
                    tight.map(|p| p.ops).unwrap_or(0)
                ));
            }
        }
        out.push('\n');
    }
    out
}

fn render_histogram(histogram: &BTreeMap<String, u64>) -> String {
    histogram
        .iter()
        .map(|(kind, count)| format!("`{kind}`: {count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_modify(out: &mut String, r: &ModifyReport) {
    out.push_str("### Scenario 1 — single-file modify (delivery latency)\n\n");
    out.push_str("| metric | value |\n| --- | --- |\n");
    out.push_str(&format!("| operations | {} |\n", r.ops));
    out.push_str(&format!(
        "| nominated within timeout | {}/{} |\n",
        r.nominated, r.ops
    ));
    out.push_str(&format!(
        "| delivery latency min / median / p95 / max (ms) | {} / {} / {} / {} |\n",
        r.stats.min, r.stats.median, r.stats.p95, r.stats.max
    ));
    out.push_str(&format!("| events captured | {} |\n", r.events.len()));
    out.push_str(&format!(
        "| duplicate nominations (beyond first per op) | {} |\n",
        r.duplicates
    ));
    out.push_str(&format!(
        "| kind histogram | {} |\n\n",
        render_histogram(&r.kind_histogram)
    ));
}

fn render_rename(out: &mut String, r: &RenameReport) {
    out.push_str("### Scenario 2 — rename pairing (`ideas.md` → `concepts.md`)\n\n");
    out.push_str("| metric | value |\n| --- | --- |\n");
    out.push_str(&format!("| events observed | {} |\n", r.events.len()));
    out.push_str(&format!("| pairing mechanism | {:?} |\n", r.pairing));
    out.push_str(&format!("| source nominated | {} |\n", r.from_nominated));
    out.push_str(&format!("| destination nominated | {} |\n", r.to_nominated));
    let raw: Vec<String> = r
        .events
        .iter()
        .map(|e| {
            let paths = e
                .paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let tracker = e
                .tracker
                .map(|t| format!(" tracker={t}"))
                .unwrap_or_default();
            let rescan = if e.rescan { " rescan" } else { "" };
            let info = e
                .info
                .as_ref()
                .map(|i| format!(" info=\"{i}\""))
                .unwrap_or_default();
            format!(
                "{}ms `{}` [{}] {}{}{}",
                e.at_ms, e.kind, paths, tracker, rescan, info
            )
        })
        .collect();
    out.push_str(&format!("| raw events | {} |\n\n", raw.join("; ")));
}

fn render_storm(out: &mut String, r: &StormReport) {
    out.push_str("### Scenario 3 — create/delete storm\n\n");
    out.push_str("| metric | create phase | delete phase |\n| --- | --- | --- |\n");
    out.push_str(&format!(
        "| operations | {} | {} |\n",
        r.ops_create, r.ops_delete
    ));
    out.push_str(&format!(
        "| events | {} | {} |\n",
        r.events_create, r.events_delete
    ));
    out.push_str(&format!(
        "| paths nominated exactly | {} | {} |\n",
        r.create_coverage.exact, r.delete_coverage.exact
    ));
    out.push_str(&format!(
        "| paths covered via ancestor event | {} | {} |\n",
        r.create_coverage.ancestor, r.delete_coverage.ancestor
    ));
    out.push_str(&format!(
        "| uncovered (silent loss) | {} | {} |\n",
        r.create_coverage.uncovered, r.delete_coverage.uncovered
    ));
    out.push_str(&format!(
        "| duplicate nominations | {} | {} |\n",
        r.create_duplicates, r.delete_duplicates
    ));
    out.push_str(&format!(
        "| overflow (rescan) signals (both phases) | {} |\n",
        r.overflow_signals
    ));
    let buckets: Vec<String> = r
        .arrival_buckets_100ms
        .iter()
        .map(|(ms, n)| format!("{ms}ms: {n}"))
        .collect();
    out.push_str(&format!(
        "| arrival buckets (100 ms) | {} |\n",
        buckets.join(", ")
    ));
    out.push_str(&format!(
        "| kind histogram | {} |\n\n",
        render_histogram(&r.kind_histogram)
    ));
}

fn render_burst(out: &mut String, r: &BurstReport) {
    out.push_str("### Scenario 4 — burst pattern (OS coalescing window)\n\n");
    out.push_str(
        "| phase | spacing (ms) | ops | events | events/op | nominated | trailing (ms) |\n",
    );
    out.push_str("| --- | --- | --- | --- | --- | --- | --- |\n");
    for p in &r.phases {
        out.push_str(&format!(
            "| {} | {} | {} | {} | {}/{} | {} | {} |\n",
            p.label, p.spacing_ms, p.ops, p.events, p.events, p.ops, p.nominated, p.trailing_ms
        ));
    }
    let max_coalesced = r
        .phases
        .iter()
        .filter(|p| p.events < p.ops)
        .map(|p| p.spacing_ms)
        .max()
        .unwrap_or(0);
    let min_uncoalesced = r
        .phases
        .iter()
        .filter(|p| p.events >= p.ops)
        .map(|p| p.spacing_ms)
        .min();
    let bracket = match min_uncoalesced {
        Some(ms) => format!("{ms} ms"),
        None => "none observed".to_string(),
    };
    out.push_str(&format!(
        "\nLargest spacing still coalesced: {max_coalesced} ms; smallest spacing with one event per op: {bracket}.\n\n"
    ));
}

// ---------------------------------------------------------------------------
// Temp vault
// ---------------------------------------------------------------------------

/// A temporary vault directory, removed on drop.
#[derive(Debug)]
pub struct TempVault(PathBuf);

impl TempVault {
    /// Create a uniquely named temp vault directory for `tag`.
    ///
    /// # Panics
    /// Panics if the directory cannot be created.
    #[must_use]
    pub fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!(
            "flavite-s3-{tag}-{}-{id}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("failed to create temp vault");
        Self(path)
    }

    /// The vault root path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempVault {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- Scenario builders: deterministic shapes ---

    #[test]
    fn modify_script_shape() {
        let dir = Path::new("/vault");
        let script = modify_script(dir, 3);
        assert_eq!(script.len(), 3);
        assert!(
            script
                .iter()
                .all(|op| *op == Op::Write(dir.join("note.md")))
        );
    }

    #[test]
    fn rename_script_shape() {
        let dir = Path::new("/vault");
        let script = rename_script(dir);
        assert_eq!(script.len(), 2);
        assert_eq!(script[0], Op::Create(dir.join("ideas.md")));
        assert_eq!(
            script[1],
            Op::Rename {
                from: dir.join("ideas.md"),
                to: dir.join("concepts.md"),
            }
        );
    }

    #[test]
    fn storm_script_shape_and_determinism() {
        let dir = Path::new("/vault");
        let script = storm_script(dir, 4);
        assert_eq!(script.len(), 8);
        assert_eq!(script[0], Op::Create(dir.join("storm-00000.md")));
        assert_eq!(script[3], Op::Create(dir.join("storm-00003.md")));
        assert_eq!(script[4], Op::Delete(dir.join("storm-00000.md")));
        assert_eq!(script[7], Op::Delete(dir.join("storm-00003.md")));
        assert_eq!(script, storm_script(dir, 4));
    }

    #[test]
    fn burst_phases_shape() {
        let phases = burst_phases();
        assert_eq!(phases.len(), 1 + BURST_SPACINGS_MS.len());
        assert_eq!(phases[0].label, "tight");
        assert_eq!(phases[0].spacing, Duration::ZERO);
        assert_eq!(phases[0].ops, BURST_TIGHT_OPS);
        for (i, &ms) in BURST_SPACINGS_MS.iter().enumerate() {
            assert_eq!(phases[i + 1].label, format!("spaced-{ms}ms"));
            assert_eq!(phases[i + 1].spacing, Duration::from_millis(ms));
            assert_eq!(phases[i + 1].ops, BURST_SPACED_OPS);
        }
    }

    // --- Latency statistics ---

    #[test]
    fn latency_stats_empty() {
        let stats = latency_stats(&[]);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.max, 0);
    }

    #[test]
    fn latency_stats_known_samples() {
        let stats = latency_stats(&[9, 1, 4, 2, 7]);
        assert_eq!(stats.count, 5);
        assert_eq!(stats.min, 1);
        assert_eq!(stats.median, 4);
        assert_eq!(stats.max, 9);
        assert_eq!(stats.p95, 9);
        assert_eq!(stats.p99, 9);
    }

    #[test]
    fn latency_stats_single_sample() {
        let stats = latency_stats(&[42]);
        assert_eq!(stats.count, 1);
        assert_eq!(stats.median, 42);
        assert_eq!(stats.p95, 42);
        assert_eq!(stats.p99, 42);
        assert_eq!(stats.max, 42);
    }

    #[test]
    fn latency_stats_p99_nearest_rank() {
        // 0..=99 → nearest-rank index `len*99/100` = 99 (the max), and
        // `len*95/100` = 95 for p95.
        let samples: Vec<u64> = (0..100).collect();
        let stats = latency_stats(&samples);
        assert_eq!(stats.p95, 95);
        assert_eq!(stats.p99, 99);
    }

    // --- Coverage ---

    #[test]
    fn coverage_exact_and_ancestor() {
        let mut expected = HashSet::new();
        expected.insert(PathBuf::from("a.md"));
        expected.insert(PathBuf::from("b.md"));
        expected.insert(PathBuf::from("c.md"));
        let mut nominated = HashSet::new();
        nominated.insert(PathBuf::from("a.md"));
        // An event on the scenario directory itself covers the rest.
        nominated.insert(PathBuf::from(""));
        let cov = coverage(&expected, &nominated, false);
        assert_eq!(cov.exact, 1);
        assert_eq!(cov.ancestor, 2);
        assert_eq!(cov.uncovered, 0);
        assert!(cov.property_holds());
    }

    #[test]
    fn coverage_silent_loss_fails_property() {
        let mut expected = HashSet::new();
        expected.insert(PathBuf::from("a.md"));
        expected.insert(PathBuf::from("b.md"));
        let nominated = HashSet::new();
        let cov = coverage(&expected, &nominated, false);
        assert_eq!(cov.uncovered, 2);
        assert!(!cov.property_holds());
    }

    #[test]
    fn coverage_overflow_excuses_loss() {
        let mut expected = HashSet::new();
        expected.insert(PathBuf::from("a.md"));
        let nominated = HashSet::new();
        let cov = coverage(&expected, &nominated, true);
        assert_eq!(cov.uncovered, 1);
        assert!(cov.property_holds());
    }

    // --- Rename pairing classification ---

    fn ev(at_ms: u64, kind: &str, paths: &[&str], tracker: Option<usize>) -> RawEvent {
        RawEvent {
            at_ms,
            kind: kind.to_string(),
            paths: paths.iter().map(PathBuf::from).collect(),
            tracker,
            rescan: false,
            info: None,
        }
    }

    fn from_to() -> (PathBuf, PathBuf) {
        (PathBuf::from("ideas.md"), PathBuf::from("concepts.md"))
    }

    #[test]
    fn pairing_both_event() {
        let (from, to) = from_to();
        let events = vec![ev(
            10,
            "Modify(Name(Both))",
            &["ideas.md", "concepts.md"],
            Some(7),
        )];
        assert_eq!(classify_pairing(&events, &from, &to), Pairing::BothEvent);
    }

    #[test]
    fn pairing_cookie() {
        let (from, to) = from_to();
        let events = vec![
            ev(10, "Modify(Name(From))", &["ideas.md"], Some(7)),
            ev(11, "Modify(Name(To))", &["concepts.md"], Some(7)),
        ];
        assert_eq!(classify_pairing(&events, &from, &to), Pairing::Cookie);
    }

    #[test]
    fn pairing_adjacent_without_cookie() {
        let (from, to) = from_to();
        let events = vec![
            ev(10, "Modify(Name(From))", &["ideas.md"], None),
            ev(11, "Modify(Name(To))", &["concepts.md"], None),
        ];
        assert_eq!(classify_pairing(&events, &from, &to), Pairing::Adjacent);
    }

    #[test]
    fn pairing_unpaired_fsevent_style() {
        let (from, to) = from_to();
        let events = vec![
            ev(10, "Modify(Name(Any))", &["ideas.md"], None),
            ev(12, "Modify(Name(Any))", &["concepts.md"], None),
        ];
        assert_eq!(classify_pairing(&events, &from, &to), Pairing::Unpaired);
    }

    #[test]
    fn pairing_none_when_silent() {
        let (from, to) = from_to();
        let events = vec![ev(10, "Create(File)", &["other.md"], None)];
        assert_eq!(classify_pairing(&events, &from, &to), Pairing::None);
    }

    // --- Selection parsing ---

    #[test]
    fn parse_selection_all() {
        let selection = parse_selection("all").expect("all parses");
        assert_eq!(
            selection,
            ScenarioSelection {
                modify: true,
                rename: true,
                storm: true,
                burst: true
            }
        );
    }

    #[test]
    fn parse_selection_subset() {
        let selection = parse_selection("1,3").expect("subset parses");
        assert!(selection.modify);
        assert!(!selection.rename);
        assert!(selection.storm);
        assert!(!selection.burst);
    }

    #[test]
    fn parse_selection_rejects_unknown_and_empty() {
        assert!(parse_selection("5").is_err());
        assert!(parse_selection("").is_err());
        assert!(parse_selection("1,x").is_err());
    }

    // --- Self-check logic (pure) ---

    fn modify_report(ops: usize, nominated: usize) -> ModifyReport {
        ModifyReport {
            ops,
            nominated,
            latencies_ms: vec![None; ops],
            stats: latency_stats(&[]),
            events: Vec::new(),
            kind_histogram: BTreeMap::new(),
            duplicates: 0,
        }
    }

    #[test]
    fn self_check_modify_passes_when_all_nominated() {
        assert!(self_check_modify(&modify_report(20, 20)).is_ok());
    }

    #[test]
    fn self_check_modify_fails_on_loss() {
        assert!(self_check_modify(&modify_report(20, 19)).is_err());
    }

    #[test]
    fn self_check_rename_fails_only_when_silent() {
        let (from, to) = from_to();
        let silent = RenameReport {
            events: Vec::new(),
            pairing: Pairing::None,
            from_nominated: false,
            to_nominated: false,
        };
        assert!(self_check_rename(&silent).is_err());
        let observable = RenameReport {
            events: vec![ev(1, "Modify(Name(Any))", &["ideas.md"], None)],
            pairing: classify_pairing(
                &[ev(1, "Modify(Name(Any))", &["ideas.md"], None)],
                &from,
                &to,
            ),
            from_nominated: true,
            to_nominated: false,
        };
        assert!(self_check_rename(&observable).is_ok());
    }

    #[test]
    fn self_check_burst_fails_on_silent_phase() {
        let report = BurstReport {
            phases: vec![
                BurstPhaseReport {
                    label: "tight".to_string(),
                    spacing_ms: 0,
                    ops: 50,
                    events: 3,
                    distinct_paths: 1,
                    nominated: true,
                    trailing_ms: 2,
                },
                BurstPhaseReport {
                    label: "spaced-800ms".to_string(),
                    spacing_ms: 800,
                    ops: 10,
                    events: 0,
                    distinct_paths: 0,
                    nominated: false,
                    trailing_ms: 0,
                },
            ],
        };
        assert!(self_check_burst(&report).is_err());
    }

    #[test]
    fn summary_self_check_collects_failures() {
        let summary = RunSummary {
            os: "test".to_string(),
            arch: "test".to_string(),
            watcher_backend: "test".to_string(),
            notify: "test".to_string(),
            scenarios: vec![NamedReport {
                name: "1-modify".to_string(),
                report: ScenarioReport::Modify(modify_report(20, 19)),
            }],
        };
        let failures = self_check(&summary);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("1-modify"));
    }

    // --- Live watcher tests (real backend, generous timeouts) ---

    #[test]
    fn live_modify_nominates_every_op() {
        let vault = TempVault::new("live-modify");
        let report = run_modify(vault.path()).expect("scenario 1 runs");
        self_check_modify(&report).expect("every modification is nominated");
    }

    #[test]
    fn live_rename_is_observable() {
        let vault = TempVault::new("live-rename");
        let report = run_rename(vault.path()).expect("scenario 2 runs");
        self_check_rename(&report).expect("the rename is observable");
    }

    #[test]
    fn live_storm_nominates_or_signals_overflow() {
        let vault = TempVault::new("live-storm");
        let report = run_storm(vault.path(), 100).expect("scenario 3 runs");
        self_check_storm(&report).expect("the nomination property holds");
    }

    #[test]
    fn live_burst_never_fully_silent() {
        let vault = TempVault::new("live-burst");
        let phases = vec![
            BurstPhase {
                label: "tight".to_string(),
                spacing: Duration::ZERO,
                ops: 20,
            },
            BurstPhase {
                label: "spaced-200ms".to_string(),
                spacing: Duration::from_millis(200),
                ops: 3,
            },
        ];
        let report = run_burst_phases(vault.path(), &phases).expect("scenario 4 runs");
        self_check_burst(&report).expect("no burst phase is entirely silent");
    }
}
