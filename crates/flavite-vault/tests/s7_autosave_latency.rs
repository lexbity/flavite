//! RFC-0011 S7-T3 — autosave must never serialize on the host pump.
//!
//! The host pump runs on the UI thread inside the keystroke-critical frame.
//! S7 stages O(1) captures under the turn and encodes on a worker; this test
//! proves the frame stays flat with a 16 MiB note.
//!
//! Per R10, CI absolute time gates are avoided: the assertion is RELATIVE —
//! the pump-frame p99 while a 16 MiB autosave is in flight must stay within
//! 3× of the 1 KiB case (plus a small noise allowance). If encoding ran on
//! the pump thread, the 16 MiB frame would be milliseconds (materialize +
//! decode + codec encode ≈ O(n)) against a µs-scale 1 KiB frame. Completion
//! is asserted through the public buffer-state transition (Clean after the
//! save receipt lands).
//!
//! Run: `cargo test -p flavite-vault --test s7_autosave_latency -- --nocapture`

use flavite_buffer::manager::BufferManager;
use flavite_buffer::revision::BufferId;
use flavite_buffer::undo::{EditOp, UndoPolicy};
use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use flavite_vault::{ChannelSet, VaultActorBuilder, VaultConfig};
use std::time::{Duration, Instant};

fn open_note(session: &Session, path: &std::path::Path) -> u64 {
    let opened = session.execute(SessionCommand::OpenNote {
        path: path.to_path_buf(),
        policy: flavite_vault::session::OpenPolicy::ExistingOnly,
    });
    let opened = match opened {
        SessionResult::Pending { request_id } => {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                session.pump();
                let done = session.wait_for_operation(request_id);
                if !matches!(done, SessionResult::Pending { .. }) {
                    break done;
                }
                assert!(Instant::now() < deadline, "open never finished");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        other => other,
    };
    match opened {
        SessionResult::Success(flavite_vault::session::CommandResult::NoteOpened {
            buffer_id,
            ..
        }) => buffer_id.as_u64(),
        other => panic!("open failed: {other:?}"),
    }
}

fn make_session() -> (
    Session,
    flavite_fs::MemFs,
    std::path::PathBuf,
    std::path::PathBuf,
) {
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/s7");
    fs.create_dir_all(&root).unwrap();
    let small = root.join("small.md");
    let huge = root.join("huge.txt");
    fs.write_new(&small, b"a".repeat(1024).as_slice()).unwrap();
    fs.write_new(&huge, vec![b'x'; 16 * 1024 * 1024].as_slice())
        .unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("s7.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let data_dir = tempfile::tempdir().unwrap();
    let identity =
        flavite_vault::types::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![7, 7]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs.clone()))
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .build()
        .unwrap()
        .start()
        .unwrap();
    let channels = ChannelSet::new();
    let config = VaultConfig {
        autosave_idle_interval: Duration::ZERO,
        ..VaultConfig::default()
    };
    let session = Session::new_with_data_dir(
        identity,
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        config,
        Some(data_dir.path().to_path_buf()),
    );
    (session, fs, small, huge)
}

/// Type one character at EOF directly on the buffer (dirtied), via the real
/// session turn so the full keystroke path (including `last_edit` arming)
/// runs.
fn dirty_via_turn(session: &Session, buffer: u64) {
    let revision = session
        .shared_buffer_snapshot(BufferId::from(buffer), None)
        .expect("buffer snapshot")
        .revision
        .as_u64();
    let len = session
        .shared_buffer_snapshot(BufferId::from(buffer), None)
        .expect("buffer snapshot")
        .text
        .len();
    let operation = session
        .issue_editor_operation(
            BufferId::from(buffer),
            flavite_buffer::revision::Revision::new(revision),
            None,
        )
        .expect("operation ticket");
    let result = session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(buffer),
        revision: flavite_buffer::revision::Revision::new(revision),
        ops: vec![EditOp::new(
            flavite_syntax::text::Span::new(
                flavite_syntax::text::TextByte(len),
                flavite_syntax::text::TextByte(len),
            ),
            "z".into(),
        )],
        undo_policy: UndoPolicy::Typing,
        editor: None,
        operation,
        unit: None,
    });
    assert!(matches!(
        result,
        SessionResult::Success(flavite_vault::session::CommandResult::EditsApplied { .. })
    ));
}

/// Measure pump-frame latencies while an autosave for `buffer` is admitted
/// and in flight. Returns (p99 µs, max µs) over `frames` consecutive pumps.
fn pump_frame_stats(session: &Session, frames: usize) -> (u128, u128) {
    // The first pump stages (O(1)); the worker then encodes off-turn while
    // we keep pumping. Measure the whole window so the drain/submit frame is
    // included.
    let mut samples = Vec::with_capacity(frames);
    for _ in 0..frames {
        let start = Instant::now();
        session.pump();
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    let p99 = samples[(samples.len() - 1) * 99 / 100];
    let max = *samples.iter().max().unwrap();
    (p99, max)
}

/// Wait until the autosave durably landed on disk (the actor wrote the
/// note). This is the public completion observable: an admitted autosave
/// ends as a file write of the edited content.
fn wait_until_saved(
    fs: &flavite_fs::MemFs,
    session: &Session,
    path: &std::path::Path,
    expected_len: usize,
    deadline: Instant,
) {
    use flavite_fs::Fs;
    while Instant::now() < deadline {
        session.pump();
        if let Ok(Some(bytes)) = fs.read_if_exists(path)
            && bytes.len() == expected_len
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    let on_disk = fs.read_if_exists(path).ok().flatten().map(|b| b.len());
    panic!(
        "autosave never completed (file not updated within deadline); expected_len={expected_len} on_disk={on_disk:?} failures={}",
        session.autosave_failures_total()
    );
}

#[test]
fn pump_frame_stays_flat_while_16mib_autosave_encodes_off_thread() {
    let (session, fs, small_path, huge_path) = make_session();
    let small = open_note(&session, &small_path);
    let huge = open_note(&session, &huge_path);

    // 1 KiB case: baseline frame distribution.
    dirty_via_turn(&session, small);
    session.pump(); // stage
    let (small_p99, small_max) = pump_frame_stats(&session, 200);
    wait_until_saved(
        &fs,
        &session,
        &small_path,
        1024 + 1,
        Instant::now() + Duration::from_secs(20),
    );
    println!("s7-T3 small: pump p99={small_p99}us max={small_max}us");

    // 16 MiB case: if the pump encoded, frames would be O(n) — milliseconds.
    dirty_via_turn(&session, huge);
    session.pump(); // stage
    let (huge_p99, huge_max) = pump_frame_stats(&session, 200);
    println!("s7-T3 huge: pump p99={huge_p99}us max={huge_max}us");
    wait_until_saved(
        &fs,
        &session,
        &huge_path,
        16 * 1024 * 1024 + 1,
        Instant::now() + Duration::from_secs(8),
    );

    // Relative gate: 16 MiB frames within 3× of 1 KiB (+ noise allowance).
    let bound = small_p99.max(50) * 3 + 200;
    assert!(
        huge_p99 <= bound,
        "s7-T3 16 MiB autosave pump p99 {huge_p99}us > 3× 1 KiB ({bound}us) — encode on the pump thread"
    );
    assert!(
        huge_max < 5_000,
        "s7-T3 16 MiB autosave max frame {huge_max}us ≥ 5 ms — encode on the pump thread"
    );
}
