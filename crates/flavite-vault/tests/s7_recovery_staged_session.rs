//! RFC-0011 S7-T2 — session-level recovery through the staged path.
//!
//! `Session::recovery_tick` (and the background recovery worker) capture
//! StagedRecovery under the buffer-manager lock and serialize off it. This
//! test drives that public staged path end to end:
//!
//! 1. Edit → dirty; `recovery_tick` writes a staged snapshot.
//! 2. Crash mid-write is simulated by tearing the written file (the exact
//!    artifact a crash before the atomic rename leaves). `RestoreRecovery`
//!    must refuse it — never restore partial bytes.
//! 3. A further staged tick heals the recovery; `RestoreRecovery` then
//!    lands the edited content byte-for-byte.
//!
//! Run: `cargo test -p flavite-vault --test s7_recovery_staged_session -- --nocapture`

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

fn make_session(path: &std::path::Path) -> Session {
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/s7r");
    fs.create_dir_all(&root).unwrap();
    fs.write_new(path, b"original draft\n").unwrap();
    let db_dir = tempfile::tempdir().unwrap();
    let db = db_dir.path().join("s7r.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let data_dir = tempfile::tempdir().unwrap();
    let identity =
        flavite_vault::types::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![7, 7]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs))
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .build()
        .unwrap()
        .start()
        .unwrap();
    let channels = ChannelSet::new();
    Session::new_with_data_dir(
        identity,
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig::default(),
        Some(data_dir.path().to_path_buf()),
    )
}

fn edit_via_turn(session: &Session, buffer: u64, suffix: &str) {
    let snapshot = session
        .shared_buffer_snapshot(BufferId::from(buffer), None)
        .expect("buffer snapshot");
    let revision = snapshot.revision;
    let len = snapshot.text.len();
    let operation = session
        .issue_editor_operation(BufferId::from(buffer), revision, None)
        .expect("operation ticket");
    let result = session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(buffer),
        revision,
        ops: vec![EditOp::new(
            flavite_syntax::text::Span::new(
                flavite_syntax::text::TextByte(len),
                flavite_syntax::text::TextByte(len),
            ),
            suffix.to_string(),
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

fn current_text(session: &Session, buffer: u64) -> String {
    session
        .shared_buffer_snapshot(BufferId::from(buffer), None)
        .expect("buffer snapshot")
        .text
        .materialize()
        .to_string()
}

#[test]
fn staged_recovery_session_round_trip_and_torn_write_rejection() {
    let path = std::path::PathBuf::from("/test/s7r/note.md");
    let session = make_session(&path);
    let buffer = open_note(&session, &path);

    // Edit → dirty: the draft we must be able to restore.
    edit_via_turn(&session, buffer, " unsaved edit");
    let expected = current_text(&session, buffer);
    assert_eq!(expected, "original draft\n unsaved edit");

    // 1. Staged recovery flush writes a snapshot (Session::flush_recovery
    //    uses StagedRecovery::from_buffer under the lock).
    let written = session.flush_recovery().expect("staged flush");
    assert_eq!(
        written.len(),
        1,
        "one dirty note -> one staged recovery file"
    );
    let recovery_path = written[0].clone();

    // 2. Crash mid-write: tear the destination file. The reader must
    //    reject it, and RestoreRecovery must refuse — never restore partial
    //    bytes.
    let bytes = std::fs::read(&recovery_path).unwrap();
    std::fs::write(&recovery_path, &bytes[..bytes.len() / 2]).unwrap();
    let refused = session.execute(SessionCommand::RestoreRecovery { path: path.clone() });
    assert!(
        matches!(refused, SessionResult::Error { .. }),
        "torn recovery must be refused, got {refused:?}"
    );

    // 3. The next staged flush heals; restore then lands the draft
    //    byte-for-byte.
    let healed = session.flush_recovery().expect("healing staged flush");
    assert_eq!(healed.len(), 1, "healing staged write");
    let restored = session.execute(SessionCommand::RestoreRecovery { path: path.clone() });
    assert!(
        matches!(
            restored,
            SessionResult::Success(flavite_vault::session::CommandResult::RecoveryRestored { .. })
        ),
        "restore failed after heal: {restored:?}"
    );
    assert_eq!(
        current_text(&session, buffer),
        expected,
        "byte-exact restore"
    );
}
