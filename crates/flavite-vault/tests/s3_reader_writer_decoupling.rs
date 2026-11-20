//! RFC-0011 S3-T2 (AC-8 / FR-13): reader/writer decoupling.
//!
//! A reader thread materializes 1 MiB roots in a loop (the graph-pump
//! pattern) while the writer types 500 keystrokes. The keystroke p99 must
//! stay within 2× of an isolated run: readers hold no `driving_turn` and
//! hold the buffer-manager lock only for the O(1) root publication — the
//! O(n) materialization happens on the reader's own thread, off the locks.
//!
//! Run: `cargo test -p flavite-vault --test s3_reader_writer_decoupling -- --nocapture`

use flavite_buffer::revision::{BufferId, Revision};
use flavite_buffer::undo::{EditOp, UndoPolicy};
use flavite_vault::session::{Session, SessionCommand, SessionResult};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn fixture() -> (Session, u64) {
    use flavite_buffer::manager::BufferManager;
    use flavite_fs::{Fs, MemFs};
    use flavite_index::IndexActor;
    use flavite_vault::{ChannelSet, VaultActorBuilder, VaultConfig};
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/s3");
    fs.create_dir_all(&root).unwrap();
    // A 1 MiB note: the materialization class the reader loop exercises.
    let body = "x".repeat(1024 * 1024);
    fs.write_new(&root.join("note.md"), body.as_bytes()).unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("s3.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let data_dir = tempfile::tempdir().unwrap();
    let identity = flavite_vault::types::VaultIdentity::new(
        uuid::Uuid::now_v7(),
        root.clone(),
        vec![7, 3], // fingerprints
    );
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs))
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .build()
        .unwrap()
        .start()
        .unwrap();
    let channels = ChannelSet::new();
    let session = Session::new_with_data_dir(
        identity,
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig::default(),
        Some(data_dir.path().to_path_buf()),
    );
    let opened = session.execute(SessionCommand::OpenNote {
        path: root.join("note.md"),
        policy: flavite_vault::session::OpenPolicy::ExistingOnly,
    });
    let opened = match opened {
        SessionResult::Pending { request_id } => {
            let deadline = Instant::now() + Duration::from_secs(10);
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
        }) => (session, buffer_id.as_u64()),
        other => panic!("open failed: {other:?}"),
    }
}

/// Type `count` characters at EOF through the real session turn; returns
/// per-keystroke latencies in microseconds.
fn type_keystrokes(session: &Session, buffer: u64, count: usize) -> Vec<u128> {
    let mut revision = session
        .shared_buffer_snapshot(BufferId::from(buffer), None)
        .expect("buffer snapshot")
        .revision
        .as_u64();
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let at = if revision == 1 { 1024 * 1024 } else { usize::MAX };
        let current_len = session
            .shared_buffer_snapshot(BufferId::from(buffer), None)
            .expect("buffer snapshot")
            .text
            .len();
        let at = if at == usize::MAX { current_len } else { at };
        let edit = EditOp::new(
            flavite_syntax::text::Span::new(
                flavite_syntax::text::TextByte(at),
                flavite_syntax::text::TextByte(at),
            ),
            "z".into(),
        );
        let operation = session
            .issue_editor_operation(BufferId::from(buffer), Revision::new(revision), None)
            .expect("operation ticket");
        let start = Instant::now();
        let result = session.execute(SessionCommand::ApplyEdits {
            buffer_id: BufferId::from(buffer),
            revision: Revision::new(revision),
            ops: vec![edit],
            undo_policy: UndoPolicy::Typing,
            editor: None,
            operation,
            unit: None,
        });
        samples.push(start.elapsed().as_micros());
        match result {
            SessionResult::Success(flavite_vault::session::CommandResult::EditsApplied {
                new_revision,
                ..
            }) => revision = new_revision.as_u64(),
            other => panic!("keystroke failed: {other:?}"),
        }
    }
    samples
}

fn percentile(mut samples: Vec<u128>, percent: usize) -> u128 {
    samples.sort_unstable();
    let idx = (samples.len().saturating_sub(1)) * percent / 100;
    samples.get(idx).copied().unwrap_or(0)
}

#[test]
fn reader_materialization_loop_does_not_convoy_keystrokes() {
    let (session, buffer) = fixture();
    let session = Arc::new(session);

    // Warm the session and the editor path (edit applies, publication).
    let _ = type_keystrokes(&session, buffer, 30);

    // Isolated run (no reader): the baseline.
    let quiet = type_keystrokes(&session, buffer, 300);
    let quiet_p99 = percentile(quiet.clone(), 99);

    // Contended run: a reader thread materializes 1 MiB roots in a loop —
    // the graph-pump pattern (fetch snapshot slot, then full materialize).
    let reader_session = session.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let reader_stop = stop.clone();
    let reader = std::thread::spawn(move || {
        let mut materializations = 0u64;
        while !reader_stop.load(Ordering::Relaxed) {
            for snapshot in reader_session.graph_buffer_snapshots() {
                let _ = snapshot.text.materialize();
                materializations += 1;
            }
        }
        materializations
    });
    let contended = type_keystrokes(&session, buffer, 500);
    stop.store(true, Ordering::Relaxed);
    let materializations = reader.join().expect("reader thread");
    drop(session);

    let contended_p99 = percentile(contended.clone(), 99);
    let contended_max = *contended.iter().max().unwrap_or(&0);
    println!(
        "s3-T2 quiet n={} p99={quiet_p99}us; contended n={} p99={contended_p99}us max={contended_max}us; reader materializations={materializations}",
        quiet.len(),
        contended.len(),
    );

    // The reader must have done real O(n) work — otherwise the test proves
    // nothing about the decoupling.
    assert!(
        materializations >= 100,
        "reader loop did not materialize enough roots ({materializations})"
    );

    // Keystroke p99 during reader contention stays within 2× of the
    // isolated run. The baseline is floored at a small absolute value so a
    // quiet µs-scale p99 cannot make the bound absurdly tight against
    // scheduler noise (the 2× rule is about lock convoying, not sub-100µs
    // timer jitter).
    let floor = quiet_p99.max(100);
    let bound = floor * 2;
    assert!(
        contended_p99 <= bound,
        "keystroke p99 under reader contention {contended_p99}us > 2× isolated ({bound}us) — lock convoy remains"
    );
    // No pathological tail, either.
    assert!(contended_max < 5_000, "contended keystroke tail {contended_max}us ≥ 5ms");
}
