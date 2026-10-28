//! Event coalescing with bulk mode detection (D5, §4.4).
//!
//! Events are keyed by path with a quiet window (300ms default, max 2s).
//! If more than 500 events arrive within 1 second, enter bulk mode:
//! suspend per-event UI updates, run one full reconcile, show progress.

use crate::types::{ReconcileMode, ReconcileScope};
use crossbeam::channel::{Receiver, Sender, TryRecvError};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Monotonic time seam used to make batching deadlines deterministic in tests.
pub trait Clock: Send + Sync {
    fn now(&self) -> Duration;
}

#[derive(Debug)]
struct SystemClock(Instant);
impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

#[derive(Default)]
struct CoalescerState {
    pending: EventBatch,
    first_event: Option<Duration>,
    last_event: Option<Duration>,
    arrivals: Vec<Duration>,
    bulk_pending: bool,
    manual: Option<(ReconcileScope, ReconcileMode)>,
}

/// A coalesced event batch.
#[derive(Debug, Clone)]
pub struct EventBatch {
    /// The scope of events.
    pub scope: ReconcileScope,
    /// The mode to use for reconcile.
    pub mode: ReconcileMode,
    /// The paths that changed.
    pub paths: HashSet<PathBuf>,
    /// Whether this is a bulk mode batch.
    pub is_bulk: bool,
}

impl EventBatch {
    /// Create a new event batch.
    #[must_use]
    pub fn new(scope: ReconcileScope, mode: ReconcileMode) -> Self {
        Self {
            scope,
            mode,
            paths: HashSet::new(),
            is_bulk: false,
        }
    }

    /// Create a bulk mode batch.
    #[must_use]
    pub fn bulk(scope: ReconcileScope, paths: HashSet<PathBuf>) -> Self {
        Self {
            scope,
            mode: ReconcileMode::VerifyContent,
            paths,
            is_bulk: true,
        }
    }

    /// Add a path to the batch.
    pub fn add_path(&mut self, path: PathBuf) {
        self.paths.insert(path);
    }

    /// Whether the batch is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// Get the number of paths in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.paths.len()
    }
}

impl Default for EventBatch {
    fn default() -> Self {
        Self::new(ReconcileScope::Full, ReconcileMode::FastMetadata)
    }
}

/// Event for the coalescer.
#[derive(Debug, Clone)]
pub enum CoalescerEvent {
    /// A path was nominated by the watcher.
    PathNominated(PathBuf),
    /// A bulk mode trigger (overflow detected).
    BulkTrigger(HashSet<PathBuf>),
    /// A manual reconcile request.
    ManualReconcile(ReconcileScope, ReconcileMode),
}

/// Coalescer configuration.
#[derive(Debug, Clone)]
pub struct CoalescerConfig {
    /// Quiet window duration (default: 300ms).
    pub quiet_window: Duration,
    /// Maximum coalescing window (default: 2s).
    pub max_window: Duration,
    /// Threshold for bulk mode (events per second, default: 500).
    pub bulk_threshold: u32,
}

impl Default for CoalescerConfig {
    fn default() -> Self {
        Self {
            quiet_window: Duration::from_millis(300),
            max_window: Duration::from_secs(2),
            bulk_threshold: 500,
        }
    }
}

/// The event coalescer.
///
/// Collects filesystem events and batches them into reconcile operations.
#[derive(Clone)]
pub struct EventCoalescer {
    /// Configuration.
    config: CoalescerConfig,
    /// Event sender.
    event_sender: Sender<CoalescerEvent>,
    /// Event receiver.
    event_receiver: Receiver<CoalescerEvent>,
    /// Pending batch.
    state: Arc<Mutex<CoalescerState>>,
    clock: Arc<dyn Clock>,
}

impl EventCoalescer {
    /// Create a new event coalescer.
    #[must_use]
    pub fn new(config: CoalescerConfig) -> Self {
        Self::with_clock(config, Arc::new(SystemClock(Instant::now())))
    }

    /// Construct with a deterministic clock (primarily for tests).
    #[must_use]
    pub fn with_clock(config: CoalescerConfig, clock: Arc<dyn Clock>) -> Self {
        let (event_sender, event_receiver) = crossbeam::channel::unbounded();
        Self {
            config,
            event_sender,
            event_receiver,
            state: Arc::new(Mutex::new(CoalescerState {
                pending: EventBatch::default(),
                ..CoalescerState::default()
            })),
            clock,
        }
    }

    /// Create a new event coalescer with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(CoalescerConfig::default())
    }

    /// Send an event to the coalescer.
    pub fn send(&self, event: CoalescerEvent) {
        self.ingest(event);
    }

    /// Try to receive a coalesced batch.
    ///
    /// Returns Some(batch) if there are events to process, None if the
    /// quiet window hasn't expired or there are no events.
    #[must_use]
    pub fn try_recv_batch(&self) -> Option<EventBatch> {
        loop {
            match self.event_receiver.try_recv() {
                Ok(event) => self.ingest(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }
        let now = self.clock.now();
        let mut state = self.state.lock().unwrap();
        if let Some((scope, mode)) = state.manual.take() {
            let mut batch = std::mem::take(&mut state.pending);
            batch.scope = scope;
            batch.mode = mode;
            state.reset_window();
            return Some(batch);
        }
        let last = state.last_event?;
        let first = state.first_event.unwrap_or(last);
        let forced = now.saturating_sub(first) >= self.config.max_window;
        let quiet = now.saturating_sub(last) >= self.config.quiet_window;
        if !forced && !quiet {
            return None;
        }
        if state.pending.is_empty() && !state.bulk_pending {
            state.reset_window();
            return None;
        }
        let mut batch = std::mem::take(&mut state.pending);
        if state.bulk_pending {
            batch.scope = ReconcileScope::Full;
            batch.mode = ReconcileMode::VerifyContent;
            batch.is_bulk = true;
        }
        state.bulk_pending = false;
        state.reset_window();
        Some(batch)
    }

    fn ingest(&self, event: CoalescerEvent) {
        let now = self.clock.now();
        let mut state = self.state.lock().unwrap();
        match event {
            CoalescerEvent::PathNominated(path) => state.pending.add_path(path),
            CoalescerEvent::BulkTrigger(paths) => {
                state.pending.paths.extend(paths);
                state.bulk_pending = true;
            }
            CoalescerEvent::ManualReconcile(scope, mode) => {
                state.manual = Some((scope, mode));
            }
        }
        state.first_event.get_or_insert(now);
        state.last_event = Some(now);
        state
            .arrivals
            .retain(|at| now.saturating_sub(*at) <= Duration::from_secs(1));
        state.arrivals.push(now);
        if state.arrivals.len() >= self.config.bulk_threshold as usize {
            state.bulk_pending = true;
        }
    }

    /// Receive a coalesced batch, waiting if necessary.
    pub fn recv_batch(&self) -> EventBatch {
        loop {
            if let Some(batch) = self.try_recv_batch() {
                return batch;
            }
            if let Ok(event) = self.event_receiver.recv_timeout(Duration::from_millis(10)) {
                self.ingest(event);
            }
        }
    }

    /// Get the event sender.
    #[must_use]
    pub fn event_sender(&self) -> Sender<CoalescerEvent> {
        self.event_sender.clone()
    }

    /// Get a reference to the pending batch (for testing).
    #[must_use]
    pub fn pending(&self) -> Arc<Mutex<EventBatch>> {
        // Compatibility accessor; returns a snapshot that is intentionally
        // detached so callers cannot violate the coalescer's timing state.
        Arc::new(Mutex::new(self.state.lock().unwrap().pending.clone()))
    }

    /// Reset the coalescer (for testing).
    pub fn reset(&self) {
        *self.state.lock().unwrap() = CoalescerState {
            pending: EventBatch::default(),
            ..CoalescerState::default()
        };

        // Clear the channel
        while self.event_receiver.try_recv().is_ok() {}
    }
}

impl CoalescerState {
    fn reset_window(&mut self) {
        self.first_event = None;
        self.last_event = None;
        self.arrivals.clear();
    }
}

/// A watcher event wrapper.
#[derive(Debug, Clone)]
pub enum WatcherEvent {
    /// A file was created.
    Created(PathBuf),
    /// A file was modified.
    Modified(PathBuf),
    /// A file was deleted.
    Deleted(PathBuf),
    /// A file was renamed.
    Renamed { from: PathBuf, to: PathBuf },
    /// An overflow was detected.
    Overflow,
}

/// Watcher wrapper that integrates with the coalescer.
pub struct VaultWatcher {
    /// Event sender for coalescer events.
    event_sender: Sender<CoalescerEvent>,
}

impl VaultWatcher {
    /// Create a new vault watcher.
    pub fn new(coalescer: &EventCoalescer) -> Self {
        Self {
            event_sender: coalescer.event_sender(),
        }
    }

    /// Notify the coalescer of a watcher event.
    pub fn notify(&self, event: WatcherEvent) {
        match event {
            WatcherEvent::Created(path) => {
                let _ = self
                    .event_sender
                    .try_send(CoalescerEvent::PathNominated(path));
            }
            WatcherEvent::Modified(path) => {
                let _ = self
                    .event_sender
                    .try_send(CoalescerEvent::PathNominated(path));
            }
            WatcherEvent::Deleted(path) => {
                let _ = self
                    .event_sender
                    .try_send(CoalescerEvent::PathNominated(path));
            }
            WatcherEvent::Renamed { from, to } => {
                // Both paths need to be nominated
                let _ = self
                    .event_sender
                    .try_send(CoalescerEvent::PathNominated(from.clone()));
                let _ = self
                    .event_sender
                    .try_send(CoalescerEvent::PathNominated(to));
            }
            WatcherEvent::Overflow => {
                let _ = self
                    .event_sender
                    .try_send(CoalescerEvent::BulkTrigger(HashSet::new()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct FakeClock(AtomicU64);
    impl FakeClock {
        fn advance(&self, amount: Duration) {
            self.0
                .fetch_add(amount.as_millis() as u64, Ordering::SeqCst);
        }
    }
    impl Clock for FakeClock {
        fn now(&self) -> Duration {
            Duration::from_millis(self.0.load(Ordering::SeqCst))
        }
    }

    #[test]
    fn test_event_batch() {
        let mut batch = EventBatch::new(ReconcileScope::Full, ReconcileMode::FastMetadata);
        assert!(batch.is_empty());

        batch.add_path(PathBuf::from("/test/file1.md"));
        batch.add_path(PathBuf::from("/test/file2.md"));

        assert!(!batch.is_empty());
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn test_coalescer_basic() {
        let coalescer = EventCoalescer::with_defaults();

        // Send some events
        coalescer.send(CoalescerEvent::PathNominated(PathBuf::from(
            "/test/file1.md",
        )));
        coalescer.send(CoalescerEvent::PathNominated(PathBuf::from(
            "/test/file2.md",
        )));

        // Give the quiet window time to pass
        std::thread::sleep(Duration::from_millis(350));

        // Try to receive a batch
        let batch = coalescer.try_recv_batch();
        assert!(batch.is_some());
        let batch = batch.unwrap();
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn test_coalescer_quiet_window() {
        let config = CoalescerConfig {
            quiet_window: Duration::from_secs(1),
            max_window: Duration::from_secs(2),
            bulk_threshold: 500,
        };
        let coalescer = EventCoalescer::new(config);

        // Send an event
        coalescer.send(CoalescerEvent::PathNominated(PathBuf::from(
            "/test/file.md",
        )));

        // Try immediately - should be None (quiet window not expired)
        let batch = coalescer.try_recv_batch();
        assert!(batch.is_none());
    }

    #[test]
    fn watcher_nominations_and_overflow_produce_batches() {
        let coalescer = EventCoalescer::new(CoalescerConfig {
            quiet_window: Duration::ZERO,
            max_window: Duration::from_secs(5),
            bulk_threshold: 500,
        });
        let watcher = VaultWatcher::new(&coalescer);
        watcher.notify(WatcherEvent::Created(PathBuf::from("created.md")));
        watcher.notify(WatcherEvent::Modified(PathBuf::from("modified.md")));
        watcher.notify(WatcherEvent::Deleted(PathBuf::from("deleted.md")));
        watcher.notify(WatcherEvent::Renamed {
            from: PathBuf::from("old.md"),
            to: PathBuf::from("new.md"),
        });
        let batch = coalescer.try_recv_batch().unwrap();
        assert_eq!(batch.len(), 5);

        watcher.notify(WatcherEvent::Overflow);
        let overflow = coalescer.try_recv_batch().unwrap();
        assert_eq!(overflow.scope, ReconcileScope::Full);
        assert_eq!(overflow.mode, ReconcileMode::VerifyContent);
        assert!(overflow.is_empty());

        coalescer.reset();
        assert!(coalescer.try_recv_batch().is_none());
        assert!(coalescer.pending().lock().unwrap().is_empty());
    }

    #[test]
    fn quiet_expiry_emits_without_a_followup_event_and_deduplicates_paths() {
        let clock = Arc::new(FakeClock::default());
        let coalescer = EventCoalescer::with_clock(
            CoalescerConfig {
                quiet_window: Duration::from_millis(300),
                max_window: Duration::from_secs(2),
                bulk_threshold: 500,
            },
            clock.clone(),
        );
        let path = PathBuf::from("note.md");
        coalescer.send(CoalescerEvent::PathNominated(path.clone()));
        coalescer.send(CoalescerEvent::PathNominated(path));
        assert!(coalescer.try_recv_batch().is_none());
        clock.advance(Duration::from_millis(300));
        let batch = coalescer.try_recv_batch().unwrap();
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn max_window_forces_batch_during_continuous_arrival() {
        let clock = Arc::new(FakeClock::default());
        let coalescer = EventCoalescer::with_clock(
            CoalescerConfig {
                quiet_window: Duration::from_millis(300),
                max_window: Duration::from_secs(2),
                bulk_threshold: 500,
            },
            clock.clone(),
        );
        coalescer.send(CoalescerEvent::PathNominated("a.md".into()));
        for n in 1..=20 {
            clock.advance(Duration::from_millis(100));
            coalescer.send(CoalescerEvent::PathNominated(format!("{n}.md").into()));
            if n < 20 {
                assert!(coalescer.try_recv_batch().is_none());
            }
        }
        let batch = coalescer.try_recv_batch().unwrap();
        assert_eq!(batch.len(), 21);
    }

    #[test]
    fn bulk_threshold_emits_one_full_verification_batch() {
        let clock = Arc::new(FakeClock::default());
        let coalescer = EventCoalescer::with_clock(
            CoalescerConfig {
                quiet_window: Duration::from_millis(1),
                max_window: Duration::from_secs(2),
                bulk_threshold: 3,
            },
            clock.clone(),
        );
        for name in ["a.md", "b.md", "c.md"] {
            coalescer.send(CoalescerEvent::PathNominated(name.into()));
            clock.advance(Duration::from_millis(2));
        }
        let batch = coalescer.try_recv_batch().unwrap();
        assert!(batch.is_bulk);
        assert_eq!(batch.scope, ReconcileScope::Full);
        assert_eq!(batch.mode, ReconcileMode::VerifyContent);
        assert_eq!(batch.len(), 3);
        assert!(coalescer.try_recv_batch().is_none());
    }
}
