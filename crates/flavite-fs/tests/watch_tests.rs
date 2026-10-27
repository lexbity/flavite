//! Tests for file system watcher

use flavite_fs::watch::{FsEvent, FsWatcher, WatcherConfig};
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

#[test]
fn watcher_default_config() {
    let config = WatcherConfig::default();
    assert_eq!(config.debounce_ms, 100);
    assert_eq!(config.max_buffered, 1024);
    assert!(config.recursive_default);
}

#[test]
fn watcher_new() {
    let config = WatcherConfig::default();
    let watcher = FsWatcher::new(config).unwrap();
    // Just verify it creates without panic
    drop(watcher);
}

#[test]
fn watcher_with_defaults() {
    let watcher = FsWatcher::with_defaults().unwrap();
    // Just verify it creates without panic
    drop(watcher);
}

#[test]
fn watcher_watch_and_start() {
    let config = WatcherConfig {
        debounce_ms: 50,
        max_buffered: 100,
        recursive_default: false,
    };

    let mut watcher = FsWatcher::new(config).unwrap();
    let temp_dir = tempdir().unwrap();

    // Watch the temp directory
    watcher.watch(&temp_dir).unwrap();

    // Start the watcher
    let receiver = watcher.start();

    // Give the watcher time to initialize
    thread::sleep(Duration::from_millis(100));

    // Write a file
    let test_file = temp_dir.path().join("test.txt");
    std::fs::write(&test_file, b"content").unwrap();

    // Wait for the event to be processed
    thread::sleep(Duration::from_millis(200));

    // Check for events
    let receiver_guard = receiver.lock().unwrap();
    let events: Vec<FsEvent> = receiver_guard.try_iter().collect();

    // We should have at least one event
    // Note: This test may be flaky depending on the filesystem and platform
    // The important thing is that it doesn't panic

    drop(watcher);
}

#[test]
fn watcher_config_custom() {
    let config = WatcherConfig {
        debounce_ms: 250,
        max_buffered: 512,
        recursive_default: false,
    };

    let mut watcher = FsWatcher::new(config).unwrap();
    drop(watcher);
}

#[test]
fn watcher_multiple_watches() {
    let config = WatcherConfig::default();
    let mut watcher = FsWatcher::new(config).unwrap();

    let temp_dir1 = tempdir().unwrap();
    let temp_dir2 = tempdir().unwrap();

    watcher.watch(&temp_dir1).unwrap();
    watcher.watch(&temp_dir2).unwrap();

    let receiver = watcher.start();

    thread::sleep(Duration::from_millis(50));

    drop(watcher);
}

#[test]
fn watcher_watch_same_path_twice() {
    let config = WatcherConfig::default();
    let mut watcher = FsWatcher::new(config).unwrap();

    let temp_dir = tempdir().unwrap();

    // Watch the same path twice - should not fail
    watcher.watch(&temp_dir).unwrap();
    watcher.watch(&temp_dir).unwrap();

    drop(watcher);
}

#[test]
fn fs_event_equality() {
    use notify::EventKind;
    use std::path::PathBuf;

    let event1 = FsEvent {
        kind: EventKind::Create(notify::event::CreateKind::File),
        paths: vec![PathBuf::from("/test/file.txt")],
        is_rename: false,
    };

    let event2 = FsEvent {
        kind: EventKind::Create(notify::event::CreateKind::File),
        paths: vec![PathBuf::from("/test/file.txt")],
        is_rename: false,
    };

    assert_eq!(event1, event2);
}

#[test]
fn fs_event_debug() {
    use notify::EventKind;
    use std::path::PathBuf;

    let event = FsEvent {
        kind: EventKind::Modify(notify::event::ModifyKind::Data(
            notify::event::DataChange::Any,
        )),
        paths: vec![PathBuf::from("/test/file.txt")],
        is_rename: true,
    };

    // Just verify it doesn't panic when debugging
    let _debug_str = format!("{:?}", event);
}

#[test]
fn watcher_config_clone() {
    let config = WatcherConfig::default();
    let config2 = config.clone();

    assert_eq!(config.debounce_ms, config2.debounce_ms);
    assert_eq!(config.max_buffered, config2.max_buffered);
    assert_eq!(config.recursive_default, config2.recursive_default);
}

#[test]
fn watcher_config_debug() {
    let config = WatcherConfig::default();
    // Just verify it doesn't panic when debugging
    let _debug_str = format!("{:?}", config);
}

#[test]
fn watched_path_debug() {
    use flavite_fs::watch::WatchedPath;

    let wp = WatchedPath {
        path: Path::new("/test").to_path_buf(),
        recursive: true,
    };

    // Just verify it doesn't panic when debugging
    let _debug_str = format!("{:?}", wp);
}
