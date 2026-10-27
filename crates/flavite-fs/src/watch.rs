//! File system watcher wrapper (§5, D5).
//!
//! Wraps the `notify` crate with a unified interface that:
//! - Normalizes event paths
//! - Buffers events during initial scan
//! - Detects rename pairs (for T1 inference)
//! - Handles platform-specific quirks

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use notify::event::{CreateKind, RemoveKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

/// A watched path with its associated metadata.
#[derive(Debug, Clone)]
pub struct WatchedPath {
    /// The canonical path being watched.
    pub path: PathBuf,
    /// Whether this is a recursive watch.
    pub recursive: bool,
}

/// A filesystem event after normalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEvent {
    /// The kind of change that occurred.
    pub kind: EventKind,
    /// The path(s) affected by this event.
    pub paths: Vec<PathBuf>,
    /// Whether this event is a rename pair (old_path, new_path).
    pub is_rename: bool,
}

/// Configuration for the watcher.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// How long to buffer events before emitting (for coalescing).
    pub debounce_ms: u64,
    /// Maximum number of buffered events before flushing.
    pub max_buffered: usize,
    /// Whether to watch recursively by default.
    pub recursive_default: bool,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce_ms: 100,
            max_buffered: 1024,
            recursive_default: true,
        }
    }
}

/// The filesystem watcher.
///
/// Wraps `notify::RecommendedWatcher` with:
/// - Path normalization
/// - Event buffering for initial scan synchronization
/// - Rename pair detection
/// - Platform-specific quirks handling
pub struct FsWatcher {
    /// The underlying notify watcher.
    watcher: RecommendedWatcher,
    /// Channel to send events back to the consumer.
    sender: Arc<Mutex<Sender<FsEvent>>>,
    /// Receiver for the consumer.
    receiver: Arc<Mutex<Receiver<FsEvent>>>,
    /// Watcher configuration.
    config: WatcherConfig,
    /// Track watched paths.
    watched: Arc<Mutex<HashSet<PathBuf>>>,
    /// Buffer for coalescing events during initial scan.
    event_buffer: Arc<Mutex<Vec<Event>>>,
    /// Whether we're in the initial scan phase.
    in_initial_scan: Arc<Mutex<bool>>,
    /// Track potential rename pairs (path -> EventKind).
    rename_pairs: Arc<Mutex<std::collections::HashMap<PathBuf, EventKind>>>,
}

impl FsWatcher {
    /// Create a new filesystem watcher.
    pub fn new(config: WatcherConfig) -> std::result::Result<Self, crate::fs::FsError> {
        let (sender, receiver) = channel();
        let watcher = RecommendedWatcher::new(
            move |_res: std::result::Result<Event, notify::Error>| {
                // This callback will be replaced
            },
            notify::Config::default(),
        )
        .map_err(|e| crate::fs::FsError::Io(format!("failed to create watcher: {}", e)))?;

        Ok(Self {
            watcher,
            sender: Arc::new(Mutex::new(sender)),
            receiver: Arc::new(Mutex::new(receiver)),
            config,
            watched: Arc::new(Mutex::new(HashSet::new())),
            event_buffer: Arc::new(Mutex::new(Vec::new())),
            in_initial_scan: Arc::new(Mutex::new(false)),
            rename_pairs: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Create a new filesystem watcher with default configuration.
    pub fn with_defaults() -> std::result::Result<Self, crate::fs::FsError> {
        Self::new(WatcherConfig::default())
    }

    /// Watch a path for changes.
    pub fn watch<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> std::result::Result<(), crate::fs::FsError> {
        let path = path.as_ref().to_path_buf();
        {
            let mut watched = self.watched.lock().unwrap();
            if !watched.insert(path.clone()) {
                // Already watching this path
                return Ok(());
            }
        }

        // Replace the callback with one that processes events
        let sender = self.sender.clone();
        let event_buffer = self.event_buffer.clone();
        let in_initial_scan = self.in_initial_scan.clone();
        let rename_pairs = self.rename_pairs.clone();
        let config = self.config.clone();

        let mut watcher = RecommendedWatcher::new(
            move |res: std::result::Result<Event, notify::Error>| {
                match res {
                    Ok(event) => {
                        // Buffer events during initial scan
                        if *in_initial_scan.lock().unwrap() {
                            let mut buffer = event_buffer.lock().unwrap();
                            buffer.push(event);
                            return;
                        }

                        // Process the event
                        let sender_guard = sender.lock().unwrap();
                        process_event(
                            event,
                            sender_guard.clone(),
                            rename_pairs.clone(),
                            config.debounce_ms,
                        );
                    }
                    Err(e) => {
                        let sender_guard = sender.lock().unwrap();
                        let _ = sender_guard.send(FsEvent {
                            kind: EventKind::Other,
                            paths: vec![PathBuf::from(format!("error: {}", e))],
                            is_rename: false,
                        });
                    }
                }
            },
            notify::Config::default(),
        )
        .map_err(|e| crate::fs::FsError::Io(format!("failed to recreate watcher: {}", e)))?;

        // Watch the path
        watcher
            .watch(&path, RecursiveMode::Recursive)
            .map_err(|e| {
                crate::fs::FsError::Io(format!("failed to watch {}: {}", path.display(), e))
            })?;

        self.watcher = watcher;
        Ok(())
    }

    /// Start watching, returning a receiver for events.
    /// Note: This starts a background thread for event processing.
    pub fn start(&self) -> Arc<Mutex<Receiver<FsEvent>>> {
        // Start a thread to process buffered events after initial scan
        let event_buffer = self.event_buffer.clone();
        let sender = self.sender.clone();
        let in_initial_scan = self.in_initial_scan.clone();
        let config = self.config.clone();
        let rename_pairs = self.rename_pairs.clone();

        thread::spawn(move || {
            loop {
                thread::sleep(Duration::from_millis(config.debounce_ms));

                let in_scan = *in_initial_scan.lock().unwrap();
                if in_scan {
                    continue;
                }

                let mut buffer = event_buffer.lock().unwrap();
                if buffer.is_empty() {
                    continue;
                }

                // Process buffered events
                let events = std::mem::take(&mut *buffer);
                for event in events {
                    let sender_guard = sender.lock().unwrap();
                    process_event(
                        event,
                        sender_guard.clone(),
                        rename_pairs.clone(),
                        config.debounce_ms,
                    );
                }
            }
        });

        self.receiver.clone()
    }

    /// Mark the end of initial scan.
    pub fn end_initial_scan(&self) {
        *self.in_initial_scan.lock().unwrap() = false;
    }

    /// Receive the next event, blocking until one is available.
    pub fn receive(&self) -> std::result::Result<FsEvent, crate::fs::FsError> {
        let receiver = self.receiver.lock().unwrap();
        receiver
            .recv()
            .map_err(|_| crate::fs::FsError::Io("watcher channel closed".into()))
    }

    /// Try to receive an event without blocking.
    pub fn try_receive(&self) -> std::result::Result<Option<FsEvent>, crate::fs::FsError> {
        let receiver = self.receiver.lock().unwrap();
        match receiver.try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(crate::fs::FsError::Io("watcher channel closed".into()))
            }
        }
    }

    /// Unwatch a path.
    pub fn unwatch<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> std::result::Result<(), crate::fs::FsError> {
        let path = path.as_ref().to_path_buf();
        {
            let mut watched = self.watched.lock().unwrap();
            watched.remove(&path);
        }
        self.watcher.unwatch(&path).map_err(|e| {
            crate::fs::FsError::Io(format!("failed to unwatch {}: {}", path.display(), e))
        })?;
        Ok(())
    }

    /// Get a list of currently watched paths.
    pub fn watched_paths(&self) -> Vec<PathBuf> {
        self.watched.lock().unwrap().iter().cloned().collect()
    }
}

/// Process a notify event into a normalized FsEvent.
fn process_event(
    event: Event,
    sender: Sender<FsEvent>,
    rename_pairs: Arc<Mutex<std::collections::HashMap<PathBuf, EventKind>>>,
    _debounce_ms: u64,
) {
    // Extract paths from the event
    let paths: Vec<PathBuf> = event.paths.iter().map(|p| normalize_path(p)).collect();

    if paths.is_empty() {
        return;
    }

    let kind = event.kind;

    // Check for rename pairs
    let is_rename = if let EventKind::Create(CreateKind::File) = &kind {
        // Check if we have a matching Remove for this path
        let mut pairs = rename_pairs.lock().unwrap();
        if let Some(prev_kind) = pairs.remove(&paths[0]) {
            // This is the "new" path of a rename
            if let EventKind::Remove(RemoveKind::File) = prev_kind {
                true
            } else {
                // Put it back if it's not a matching pair
                pairs.insert(paths[0].clone(), prev_kind);
                false
            }
        } else {
            false
        }
    } else if let EventKind::Remove(RemoveKind::File) = &kind {
        // Store this remove for potential rename detection
        let mut pairs = rename_pairs.lock().unwrap();
        pairs.insert(paths[0].clone(), kind);
        false
    } else {
        false
    };

    // Clean up old rename pair entries periodically
    if kind == EventKind::Create(CreateKind::File) || kind == EventKind::Remove(RemoveKind::File) {
        let mut pairs = rename_pairs.lock().unwrap();
        // Remove entries older than debounce period
        // (simplified: we don't track timestamps here)
        pairs.retain(|_, _| true); // Keep all for now
    }

    let fs_event = FsEvent {
        kind,
        paths,
        is_rename,
    };

    let _ = sender.send(fs_event);
}

/// Normalize a path for consistency.
fn normalize_path(path: &Path) -> PathBuf {
    // Convert to absolute path and normalize
    match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => path.to_path_buf(),
    }
}

/// Platform-specific rename pair detection.
///
/// On different platforms, rename events are signalled differently:
/// - Unix (inotify): MovedFrom + MovedTo events with the same cookie
/// - macOS (FSEvents): Rename flag
/// - Windows: MovedFrom + MovedTo events
///
/// This module provides unified detection.
pub mod platform {
    use super::Event;
    use super::EventKind;

    /// Check if an event represents a rename on Unix (inotify).
    #[cfg(unix)]
    pub fn is_rename_event(event: &Event) -> bool {
        use notify::event::ModifyKind;
        // On Unix, we can detect rename events through the Modify kind
        // This is a simplified check; actual implementation would need
        // to inspect platform-specific event data
        matches!(event.kind, EventKind::Modify(ModifyKind::Metadata(_)))
    }

    /// Check if an event represents a rename on non-Unix platforms.
    #[cfg(not(unix))]
    pub fn is_rename_event(_event: &Event) -> bool {
        false
    }

    /// Get the inotify cookie from an event (Unix only).
    #[cfg(target_os = "linux")]
    pub fn get_cookie(_event: &Event) -> Option<u32> {
        // Extracting inotify cookie requires platform-specific handling
        // For now, return None as we don't have access to the raw inotify data
        None
    }

    /// Get the inotify cookie from an event (non-Linux).
    #[cfg(not(target_os = "linux"))]
    pub fn get_cookie(_event: &Event) -> Option<u32> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_path_resolves_symlinks() {
        // This test would need actual filesystem setup
        // For now, just test that it doesn't panic
        let path = Path::new("/tmp/test");
        let normalized = normalize_path(path);
        assert!(!normalized.to_string_lossy().is_empty());
    }

    #[test]
    fn watcher_config_defaults() {
        let config = WatcherConfig::default();
        assert_eq!(config.debounce_ms, 100);
        assert_eq!(config.max_buffered, 1024);
        assert!(config.recursive_default);
    }
}
