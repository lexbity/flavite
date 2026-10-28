//! File system watcher wrapper (§4.4, D5).
//!
//! Watch registrations belong to one `RecommendedWatcher` and accumulate. Raw
//! notify events are drained in batches so remove/create pairs can be reported
//! as rename hints. A missed pair is safe: callers reconcile paths by content.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use notify::event::{CreateKind, RemoveKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

#[derive(Debug, Clone)]
pub struct WatchedPath {
    pub path: PathBuf,
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsEvent {
    pub kind: EventKind,
    pub paths: Vec<PathBuf>,
    pub is_rename: bool,
}

/// Portable classification of a normalized filesystem event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsChangeKind {
    Created,
    Modified,
    Deleted,
}

impl FsEvent {
    /// Map notify's platform event variants into the small vault-facing set.
    #[must_use]
    pub fn change_kind(&self) -> FsChangeKind {
        if self.is_rename {
            FsChangeKind::Modified
        } else if matches!(self.kind, EventKind::Create(_)) {
            FsChangeKind::Created
        } else if matches!(self.kind, EventKind::Remove(_)) {
            FsChangeKind::Deleted
        } else {
            FsChangeKind::Modified
        }
    }
}

#[derive(Debug, Clone)]
pub struct WatcherConfig {
    pub debounce_ms: u64,
    pub max_buffered: usize,
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

/// One OS watcher, with an accumulating registration set and a stoppable
/// event-drain thread.
pub struct FsWatcher {
    watcher: RecommendedWatcher,
    raw_receiver: Arc<Mutex<Receiver<notify::Result<Event>>>>,
    receiver: Arc<Mutex<Receiver<FsEvent>>>,
    output_sender: Sender<FsEvent>,
    config: WatcherConfig,
    watched: HashSet<PathBuf>,
    shutdown: Option<Sender<()>>,
    worker: Option<JoinHandle<()>>,
}

impl FsWatcher {
    pub fn new(config: WatcherConfig) -> std::result::Result<Self, crate::fs::FsError> {
        let (raw_sender, raw_receiver) = channel();
        let callback_sender = raw_sender.clone();
        let watcher = RecommendedWatcher::new(
            move |event| {
                let _ = callback_sender.send(event);
            },
            notify::Config::default(),
        )
        .map_err(|e| crate::fs::FsError::Io(format!("failed to create watcher: {e}")))?;
        let (output_sender, receiver) = channel();
        Ok(Self {
            watcher,
            raw_receiver: Arc::new(Mutex::new(raw_receiver)),
            receiver: Arc::new(Mutex::new(receiver)),
            output_sender,
            config,
            watched: HashSet::new(),
            shutdown: None,
            worker: None,
        })
    }

    pub fn with_defaults() -> std::result::Result<Self, crate::fs::FsError> {
        Self::new(WatcherConfig::default())
    }

    pub fn watch<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> std::result::Result<(), crate::fs::FsError> {
        let path = path.as_ref().to_path_buf();
        if self.watched.contains(&path) {
            return Ok(());
        }
        self.watcher
            .watch(
                &path,
                if self.config.recursive_default {
                    RecursiveMode::Recursive
                } else {
                    RecursiveMode::NonRecursive
                },
            )
            .map_err(|e| {
                crate::fs::FsError::Io(format!("failed to watch {}: {e}", path.display()))
            })?;
        self.watched.insert(path);
        Ok(())
    }

    /// Start a single drain thread. Calling it repeatedly returns the same
    /// receiver and does not create additional workers.
    pub fn start(&mut self) -> Arc<Mutex<Receiver<FsEvent>>> {
        if self.worker.is_some() {
            return self.receiver.clone();
        }
        let raw = self.raw_receiver.clone();
        let output = self.output_sender.clone();
        let config = self.config.clone();
        let (shutdown, stop) = channel();
        self.shutdown = Some(shutdown);
        self.worker = Some(
            thread::Builder::new()
                .name("flavite-fs-watch".into())
                .spawn(move || {
                    let interval = Duration::from_millis(config.debounce_ms.max(1));
                    let mut pending = Vec::new();
                    loop {
                        if stop.try_recv().is_ok() {
                            break;
                        }
                        let first = raw.lock().unwrap().recv_timeout(interval);
                        match first {
                            Ok(event) => pending.push(event),
                            Err(RecvTimeoutError::Disconnected) => break,
                            Err(RecvTimeoutError::Timeout) => {}
                        }
                        if !pending.is_empty() {
                            while let Ok(event) = raw.lock().unwrap().try_recv() {
                                pending.push(event);
                            }
                            let batch = std::mem::take(&mut pending);
                            for event in normalize_batch(batch, config.max_buffered) {
                                let _ = output.send(event);
                            }
                        }
                    }
                })
                .expect("watcher worker thread creation failed"),
        );
        self.receiver.clone()
    }

    pub fn end_initial_scan(&self) {}

    pub fn receive(&self) -> std::result::Result<FsEvent, crate::fs::FsError> {
        self.receiver
            .lock()
            .unwrap()
            .recv()
            .map_err(|_| crate::fs::FsError::Io("watcher channel closed".into()))
    }

    pub fn try_receive(&self) -> std::result::Result<Option<FsEvent>, crate::fs::FsError> {
        match self.receiver.lock().unwrap().try_recv() {
            Ok(event) => Ok(Some(event)),
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err(crate::fs::FsError::Io("watcher channel closed".into()))
            }
        }
    }

    pub fn unwatch<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> std::result::Result<(), crate::fs::FsError> {
        let path = path.as_ref().to_path_buf();
        if self.watched.remove(&path) {
            self.watcher.unwatch(&path).map_err(|e| {
                crate::fs::FsError::Io(format!("failed to unwatch {}: {e}", path.display()))
            })?;
        }
        Ok(())
    }

    pub fn watched_paths(&self) -> Vec<PathBuf> {
        self.watched.iter().cloned().collect()
    }

    /// Stop and join the drain thread. Registrations remain until drop.
    pub fn shutdown(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn normalize_batch(events: Vec<notify::Result<Event>>, max_buffered: usize) -> Vec<FsEvent> {
    let limit = max_buffered.max(1);
    let overflowed = events.len() > limit;
    let events: Vec<Event> = events
        .into_iter()
        .take(limit)
        .map(|event| {
            event.unwrap_or_else(|error| {
                Event::new(EventKind::Other)
                    .add_path(PathBuf::from(format!("notify-error:{error}")))
            })
        })
        .collect();
    let mut removals: HashMap<PathBuf, Instant> = HashMap::new();
    let mut creates = Vec::new();
    let mut output = Vec::new();
    for event in events {
        let paths: Vec<_> = event.paths.iter().map(|p| normalize_path(p)).collect();
        if paths.is_empty() {
            continue;
        }
        match event.kind {
            EventKind::Remove(RemoveKind::File | RemoveKind::Any) => {
                removals.insert(paths[0].clone(), Instant::now());
            }
            EventKind::Create(CreateKind::File | CreateKind::Any) => {
                creates.push((event.kind, paths))
            }
            kind => output.push(FsEvent {
                kind,
                paths,
                is_rename: false,
            }),
        }
    }
    for (kind, paths) in creates {
        let paired = removals.keys().next().cloned();
        if let Some(from) = paired {
            removals.remove(&from);
            output.push(FsEvent {
                kind,
                paths: vec![from, paths[0].clone()],
                is_rename: true,
            });
        } else {
            output.push(FsEvent {
                kind,
                paths,
                is_rename: false,
            });
        }
    }
    for (path, _) in removals {
        output.push(FsEvent {
            kind: EventKind::Remove(RemoveKind::File),
            paths: vec![path],
            is_rename: false,
        });
    }
    if overflowed {
        // The path is only a nomination. VaultActor treats any batch as a
        // request for content verification, so this sentinel forces a scan.
        output.push(FsEvent {
            kind: EventKind::Other,
            paths: vec![PathBuf::from(".")],
            is_rename: false,
        });
    }
    output
}

fn normalize_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_pair_remove_and_create_rename_hints() {
        let events = vec![
            Ok(Event::new(EventKind::Remove(RemoveKind::File)).add_path("/old.md".into())),
            Ok(Event::new(EventKind::Create(CreateKind::File)).add_path("/new.md".into())),
        ];
        let result = normalize_batch(events, 10);
        assert_eq!(result.len(), 1);
        assert!(result[0].is_rename);
        assert_eq!(
            result[0].paths,
            vec![PathBuf::from("/old.md"), PathBuf::from("/new.md")]
        );
    }

    #[test]
    fn registrations_accumulate_and_shutdown_joins_worker() {
        let mut watcher = FsWatcher::with_defaults().unwrap();
        let one = tempfile::tempdir().unwrap();
        let two = tempfile::tempdir().unwrap();
        watcher.watch(one.path()).unwrap();
        watcher.watch(two.path()).unwrap();
        assert_eq!(watcher.watched_paths().len(), 2);
        let _events = watcher.start();
        assert!(watcher.worker.is_some());
        watcher.shutdown();
        assert!(watcher.worker.is_none());
    }

    #[test]
    fn rename_candidates_from_separate_batches_remain_create_and_delete() {
        let removed = normalize_batch(
            vec![Ok(
                Event::new(EventKind::Remove(RemoveKind::File)).add_path("/old.md".into())
            )],
            10,
        );
        let created = normalize_batch(
            vec![Ok(
                Event::new(EventKind::Create(CreateKind::File)).add_path("/new.md".into())
            )],
            10,
        );
        assert_eq!(removed.len(), 1);
        assert_eq!(created.len(), 1);
        assert!(!removed[0].is_rename);
        assert!(!created[0].is_rename);
    }
}
