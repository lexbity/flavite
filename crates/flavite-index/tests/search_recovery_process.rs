#![cfg(unix)]

use flavite_index::{IndexActor, Query, open_or_init};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CHILD_ROOT: &str = "FLAVITE_TANTIVY_KILL_CHILD_ROOT";

#[test]
fn uncommitted_search_changes_replay_after_sigkill() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        child(std::path::Path::new(&root));
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "uncommitted_search_changes_replay_after_sigkill"])
        .env(CHILD_ROOT, temp.path())
        .env("FLAVITE_TANTIVY_READY", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("writer exited before ready marker: {status}")
        }
        assert!(Instant::now() < deadline, "writer did not become ready");
        thread::sleep(Duration::from_millis(5));
    }
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9));
    let db = temp.path().join("index.sqlite3");
    let mut reopened = IndexActor::new(&db, 1).unwrap();
    let page = reopened
        .search_notes(&Query::Text("crashreplaytoken".into()), 10)
        .unwrap();
    assert_eq!(page.generation, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(reopened.search_generations().unwrap(), (1, 1));
}

fn child(root: &std::path::Path) {
    let db = root.join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let mut index = IndexActor::new(&db, 1).unwrap();
    index
        .apply_saved_note(
            b"crash.md",
            "crash.md",
            b"# Crash recovery\n\ncrashreplaytoken",
            1,
            1,
        )
        .unwrap();
    assert_eq!(index.search_generations().unwrap(), (1, 0));
    std::fs::write(std::env::var_os("FLAVITE_TANTIVY_READY").unwrap(), b"ready").unwrap();
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
