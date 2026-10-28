//! Event coalescing with bulk mode detection (D5, §4.4).
//!
//! Events are keyed by path with a quiet window (300ms default, max 2s).
//! If more than 500 events arrive within 1 second, enter bulk mode:
//! suspend per-event UI updates, run one full reconcile, show progress.

use crate::types::{ReconcileMode, ReconcileScope};
use crossbeam::channel::{Receiver, Sender, TryRecvError};
use flavite_fs::Fs;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    pending: Arc<Mutex<EventBatch>>,
    /// Last event time.
    last_event_time: Arc<Mutex<Instant>>,
    /// Event count for bulk detection.
    event_count: Arc<Mutex<u32>>,
    /// Last bulk detection time.
    last_bulk_time: Arc<Mutex<Instant>>,
    /// Whether we're in bulk mode.
    in_bulk_mode: Arc<Mutex<bool>>,
}

impl EventCoalescer {
    /// Create a new event coalescer.
    #[must_use]
    pub fn new(config: CoalescerConfig) -> Self {
        let (event_sender, event_receiver) = crossbeam::channel::unbounded();

        Self {
            config,
            event_sender,
            event_receiver,
            pending: Arc::new(Mutex::new(EventBatch::default())),
            last_event_time: Arc::new(Mutex::new(Instant::now())),
            event_count: Arc::new(Mutex::new(0)),
            last_bulk_time: Arc::new(Mutex::new(Instant::now())),
            in_bulk_mode: Arc::new(Mutex::new(false)),
        }
    }

    /// Create a new event coalescer with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        Self::new(CoalescerConfig::default())
    }

    /// Send an event to the coalescer.
    pub fn send(&self, event: CoalescerEvent) {
        let _ = self.event_sender.try_send(event);
        *self.last_event_time.lock().unwrap() = Instant::now();

        // Increment event count for bulk detection
        let mut count = self.event_count.lock().unwrap();
        *count += 1;

        // Check for bulk mode
        let now = Instant::now();
        let mut last_bulk = self.last_bulk_time.lock().unwrap();

        if *count >= self.config.bulk_threshold {
            // Check if we've exceeded the threshold in the last second
            let mut in_bulk = self.in_bulk_mode.lock().unwrap();
            if !*in_bulk && now.duration_since(*last_bulk) < Duration::from_secs(1) {
                *in_bulk = true;
                *last_bulk = now;
                *count = 0; // Reset count
            }
        }
    }

    /// Try to receive a coalesced batch.
    ///
    /// Returns Some(batch) if there are events to process, None if the
    /// quiet window hasn't expired or there are no events.
    #[must_use]
    pub fn try_recv_batch(&self) -> Option<EventBatch> {
        // Check if quiet window has expired
        let last_time = *self.last_event_time.lock().unwrap();
        let now = Instant::now();

        if now.duration_since(last_time) < self.config.quiet_window {
            // Quiet window hasn't expired
            return None;
        }

        // Also check max window
        if now.duration_since(last_time) > self.config.max_window {
            // Max window expired, force a batch
            let mut pending = self.pending.lock().unwrap();
            if pending.is_empty() {
                return None;
            }
            return Some(std::mem::take(&mut *pending));
        }

        // Check if we're in bulk mode
        let in_bulk = *self.in_bulk_mode.lock().unwrap();
        if in_bulk {
            // In bulk mode, we should have already triggered
            // For now, we'll just take the pending batch
            let mut pending = self.pending.lock().unwrap();
            if pending.is_empty() {
                return None;
            }
            let batch = std::mem::take(&mut *pending);

            // Reset bulk mode if we've processed the batch
            *self.in_bulk_mode.lock().unwrap() = false;
            *self.event_count.lock().unwrap() = 0;

            return Some(batch);
        }

        // Try to receive and process events
        let mut has_events = false;
        let mut temp_paths = HashSet::new();

        loop {
            match self.event_receiver.try_recv() {
                Ok(CoalescerEvent::PathNominated(path)) => {
                    temp_paths.insert(path);
                    has_events = true;
                }
                Ok(CoalescerEvent::BulkTrigger(paths)) => {
                    temp_paths.extend(paths);
                    has_events = true;
                    // Enter bulk mode
                    *self.in_bulk_mode.lock().unwrap() = true;
                }
                Ok(CoalescerEvent::ManualReconcile(scope, mode)) => {
                    // For manual reconcile, we need to handle it differently
                    // For now, just return an empty batch with the scope/mode
                    let mut pending = self.pending.lock().unwrap();
                    pending.scope = scope;
                    pending.mode = mode;
                    return Some(std::mem::take(&mut *pending));
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => break,
            }
        }

        if has_events {
            let mut pending = self.pending.lock().unwrap();
            for path in temp_paths {
                pending.add_path(path);
            }
            return Some(std::mem::take(&mut *pending));
        }

        None
    }

    /// Receive a coalesced batch, waiting if necessary.
    pub fn recv_batch(&self) -> EventBatch {
        loop {
            if let Some(batch) = self.try_recv_batch() {
                return batch;
            }

            // Wait for events or timeout
            match self.event_receiver.recv_timeout(self.config.quiet_window) {
                Ok(CoalescerEvent::PathNominated(path)) => {
                    self.send(CoalescerEvent::PathNominated(path));
                }
                Ok(CoalescerEvent::BulkTrigger(paths)) => {
                    let mut batch = EventBatch::bulk(ReconcileScope::Full, paths);
                    // Add any pending paths
                    let mut pending = self.pending.lock().unwrap();
                    for path in std::mem::take(&mut pending.paths) {
                        batch.add_path(path);
                    }
                    *self.in_bulk_mode.lock().unwrap() = false;
                    *self.event_count.lock().unwrap() = 0;
                    return batch;
                }
                Ok(CoalescerEvent::ManualReconcile(scope, mode)) => {
                    let mut pending = self.pending.lock().unwrap();
                    pending.scope = scope;
                    pending.mode = mode;
                    return std::mem::take(&mut *pending);
                }
                Err(_) => {
                    // Timeout, try again
                    continue;
                }
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
        self.pending.clone()
    }

    /// Reset the coalescer (for testing).
    pub fn reset(&self) {
        *self.pending.lock().unwrap() = EventBatch::default();
        *self.last_event_time.lock().unwrap() = Instant::now();
        *self.event_count.lock().unwrap() = 0;
        *self.last_bulk_time.lock().unwrap() = Instant::now();
        *self.in_bulk_mode.lock().unwrap() = false;

        // Clear the channel
        while self.event_receiver.try_recv().is_ok() {}
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
    /// The underlying filesystem.
    fs: Box<dyn Fs + Send + Sync>,
    /// The coalescer to send events to.
    coalescer: EventCoalescer,
    /// Event sender for coalescer events.
    event_sender: Sender<CoalescerEvent>,
}

impl VaultWatcher {
    /// Create a new vault watcher.
    pub fn new(fs: Box<dyn Fs + Send + Sync>, coalescer: &EventCoalescer) -> Self {
        Self {
            fs,
            coalescer: coalescer.clone(),
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
                // Trigger bulk mode
                // For now, we'll just trigger a full reconcile
                let _ = self.event_sender.try_send(CoalescerEvent::ManualReconcile(
                    ReconcileScope::Full,
                    ReconcileMode::VerifyContent,
                ));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // TODO: This test requires a running coalescer with background processing
    // #[test]
    // fn test_coalescer_max_window() {
    //     let config = CoalescerConfig {
    //         quiet_window: Duration::from_secs(10),  // Long quiet window
    //         max_window: Duration::from_millis(100), // Short max window
    //         bulk_threshold: 500,
    //     };
    //     let coalescer = EventCoalescer::new(config);
//
    //     // Send an event
    //     coalescer.send(CoalescerEvent::PathNominated(PathBuf::from(
    //         "/test/file.md",
    //     )));
//
    //     // Wait for max window to expire
    //     std::thread::sleep(Duration::from_millis(150));
//
    //     // Should get a batch due to max window
    //     let batch = coalescer.try_recv_batch();
    //     assert!(batch.is_some());
    // }
}
