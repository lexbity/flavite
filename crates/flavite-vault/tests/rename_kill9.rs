use flavite_fs::{Fs, OsFs};
use flavite_vault::{
    CommandResult, SessionCommand, SessionResult, Vault, VaultConfig,
    journal::{JournalExecutor, TransactionItemKind, TransactionItemState},
    types::RenameStrategy,
};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn undo_child_process() {
    let Ok(root) = std::env::var("FLAVITE_RENAME_KILL_ROOT") else {
        return;
    };
    let root = PathBuf::from(root);
    let marker = root.join(".undo-durable");
    let vault = Vault::open(
        root.clone(),
        Box::new(OsFs::new()) as Box<dyn Fs + Send + Sync>,
        VaultConfig::default(),
    )
    .unwrap();
    let session = vault.session();
    let links = root.join("links.md");
    let (buffer_id, _) = match session.execute(SessionCommand::OpenNote { path: links }) {
        SessionResult::Success(CommandResult::NoteOpened {
            buffer_id,
            revision,
        }) => (buffer_id, revision),
        other => panic!("open inbound buffer failed: {other:?}"),
    };
    let renamed = session.execute(SessionCommand::RenameNote {
        from: root.join("note.md"),
        to: root.join("renamed.md"),
        strategy: Some(RenameStrategy::Rewrite),
    });
    let renamed = match renamed {
        SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
        result => result,
    };
    assert!(
        matches!(
            renamed,
            SessionResult::Success(CommandResult::NoteRenamed { .. })
        ),
        "{renamed:?}"
    );

    let revision = session.buffer_snapshot(buffer_id, None).unwrap().revision;
    let undo = session.execute(SessionCommand::Undo {
        buffer_id,
        revision,
    });
    assert!(
        matches!(
            undo,
            SessionResult::Success(CommandResult::UndoPerformed { .. })
        ),
        "{undo:?}"
    );
    session.recovery_tick();
    std::fs::write(marker, b"undo-durable").unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[cfg(unix)]
#[test]
fn undo_cancellation_survives_kill9_and_reopen_without_journal_replay() {
    use std::os::unix::process::ExitStatusExt;

    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("vault");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("note.md"), b"hello").unwrap();
    let link_bytes = b"See [[note]]";
    std::fs::write(root.join("links.md"), link_bytes).unwrap();
    let test_exe = std::env::current_exe().unwrap();
    let mut child = Command::new(test_exe)
        .arg("--exact")
        .arg("undo_child_process")
        .arg("--nocapture")
        .env("FLAVITE_RENAME_KILL_ROOT", &root)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let marker = root.join(".undo-durable");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline && !marker.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("undo child exited before durable marker: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "child did not finish rename and durable undo"
    );
    child.kill().unwrap();
    let killed = child.wait().unwrap();
    assert_eq!(killed.signal(), Some(9));

    let before_recovery = std::fs::read(root.join("links.md")).unwrap();
    let report = JournalExecutor::new(&OsFs::new(), &root).recover().unwrap();
    assert!(!report.rolled_forward.is_empty() || !report.done.is_empty());
    assert_eq!(
        std::fs::read(root.join("links.md")).unwrap(),
        before_recovery
    );
    let journal = root.join(".flavite/journal");
    let manifest_path = std::fs::read_dir(&journal)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .expect("buffer-bound transaction manifest is retained for undo cancellation");
    let manifest: flavite_vault::JournalManifest =
        serde_json::from_slice(&std::fs::read(manifest_path).unwrap()).unwrap();
    assert!(manifest.items.iter().any(|item| {
        item.kind == TransactionItemKind::Patch && item.state == TransactionItemState::Cancelled
    }));
}
