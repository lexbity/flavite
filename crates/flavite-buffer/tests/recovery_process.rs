#![cfg(unix)]

use flavite_buffer::recovery::{RecoveryConfig, RecoveryFile, RecoveryWriter};
use flavite_buffer::state::Buffer;
use flavite_buffer::undo::EditOp;
use flavite_syntax::text::{Span, TextByte};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CHILD_ROOT: &str = "FLAVITE_RECOVERY_KILL_CHILD_ROOT";

#[test]
fn recovery_checkpoint_survives_writer_process_kill() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        recovery_child(root.into());
        return;
    }

    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("checkpoint-ready");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "recovery_checkpoint_survives_writer_process_kill",
        ])
        .env(CHILD_ROOT, temp.path())
        .env("FLAVITE_RECOVERY_READY_MARKER", &marker)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    while !marker.exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("recovery child exited before checkpoint was durable: {status}");
        }
        assert!(
            Instant::now() < deadline,
            "recovery child did not checkpoint"
        );
        thread::sleep(Duration::from_millis(5));
    }

    child.kill().unwrap();
    let status = child.wait().unwrap();
    use std::os::unix::process::ExitStatusExt;
    assert_eq!(
        status.signal(),
        Some(9),
        "child must be killed with SIGKILL"
    );

    let writer = RecoveryWriter::new(RecoveryConfig::new(
        "crash-test-vault".into(),
        temp.path().join("recovery"),
        2,
    ));
    let recovered = writer
        .read_for_note_key(b"notes/crash.md")
        .expect("durable recovery checkpoint must survive process death");
    assert_eq!(recovered.to_source_bytes(), b"base plus unsaved edit");
}

fn recovery_child(root: std::path::PathBuf) {
    let marker =
        std::env::var_os("FLAVITE_RECOVERY_READY_MARKER").expect("parent supplies ready marker");
    let mut buffer = Buffer::from_text("base");
    buffer.apply_local_edit(EditOp::new(
        Span::new(TextByte(4), TextByte(4)),
        " plus unsaved edit".into(),
    ));

    let mut writer = RecoveryWriter::new(RecoveryConfig::new(
        "crash-test-vault".into(),
        root.join("recovery"),
        0,
    ));
    let snapshot = RecoveryFile::from_buffer(&buffer, 0, false);
    assert!(writer.write_snapshot(b"notes/crash.md", snapshot).is_some());
    std::fs::write(marker, b"ready").unwrap();
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
