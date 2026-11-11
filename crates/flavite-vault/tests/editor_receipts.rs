//! Session-level editor operation ticket integration (FEUX-0002 E-5/AC-5):
//! issue → apply → duplicate/expiry across the real single-writer path.

use flavite_buffer::revision::{BufferId, Revision};
use flavite_vault::editor_receipts::{OperationId, Outcome};
use flavite_vault::session::{Session, SessionCommand};

fn fixture() -> (Session, u64) {
    use flavite_buffer::manager::BufferManager;
    use flavite_fs::{Fs, MemFs};
    use flavite_index::{GenerationSignal, IndexActor};
    use flavite_vault::{ChannelSet, VaultActorBuilder, VaultConfig};
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/tickets");
    fs.create_dir_all(&root).unwrap();
    fs.write_new(&root.join("note.md"), b"abc\ndef\n").unwrap();
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("tickets.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let data_dir = tempfile::tempdir().unwrap();
    let identity =
        flavite_vault::types::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![11]);
    let (signal, watch) = GenerationSignal::new();
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs))
        .with_identity(identity.clone())
        .with_index(IndexActor::with_generation_signal(&db, 1, signal).unwrap())
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
        flavite_vault::session::SessionResult::Pending { request_id } => {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                session.pump();
                let done = session.wait_for_operation(request_id);
                if !matches!(done, flavite_vault::session::SessionResult::Pending { .. }) {
                    break done;
                }
                assert!(std::time::Instant::now() < deadline, "open never finished");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        other => other,
    };
    let buffer = match opened {
        flavite_vault::session::SessionResult::Success(
            flavite_vault::session::CommandResult::NoteOpened { buffer_id, .. },
        ) => buffer_id.as_u64(),
        other => panic!("open failed: {other:?}"),
    };
    let _ = watch;
    (session, buffer)
}

#[test]
fn issued_operation_applies_once_and_replays_refuse() {
    let (session, buffer) = fixture();
    let op = session
        .issue_editor_operation(BufferId::from(buffer))
        .unwrap();
    let edit = flavite_buffer::undo::EditOp::new(
        flavite_syntax::text::Span::new(
            flavite_syntax::text::TextByte(0),
            flavite_syntax::text::TextByte(0),
        ),
        "X".into(),
    );
    let first = session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(buffer),
        revision: Revision::new(0),
        ops: vec![edit.clone()],
        undo_policy: flavite_buffer::undo::UndoPolicy::Isolated,
        editor: None,
        operation: Some(op),
    });
    assert!(
        matches!(
            first,
            flavite_vault::session::SessionResult::Success(
                flavite_vault::session::CommandResult::EditsApplied { .. }
            )
        ),
        "first apply must succeed: {first:?}"
    );
    // The same operation cannot apply twice.
    let second = session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(buffer),
        revision: Revision::new(1),
        ops: vec![edit],
        undo_policy: flavite_buffer::undo::UndoPolicy::Isolated,
        editor: None,
        operation: Some(op),
    });
    assert!(
        matches!(
            second,
            flavite_vault::session::SessionResult::Error { ref message } if message == "AlreadyApplied"
        ),
        "duplicate must be refused: {second:?}"
    );
}

#[test]
fn cancelled_operation_reports_cancelled_and_unknown_is_refused() {
    let (session, buffer) = fixture();
    let op = session
        .issue_editor_operation(BufferId::from(buffer))
        .unwrap();
    assert!(session.cancel_editor_operation(&op));
    let edit = flavite_buffer::undo::EditOp::new(
        flavite_syntax::text::Span::new(
            flavite_syntax::text::TextByte(0),
            flavite_syntax::text::TextByte(0),
        ),
        "Y".into(),
    );
    let result = session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(buffer),
        revision: Revision::new(0),
        ops: vec![edit],
        undo_policy: flavite_buffer::undo::UndoPolicy::Isolated,
        editor: None,
        operation: Some(op),
    });
    assert!(
        matches!(
            result,
            flavite_vault::session::SessionResult::Error { ref message } if message == "Cancelled"
        ),
        "cancelled must refuse: {result:?}"
    );

    let forged = OperationId {
        buffer_id: buffer,
        buffer_incarnation: 1,
        sequence: 99,
        ticket: 0xdead_beef,
    };
    let result = session.execute(SessionCommand::ApplyEdits {
        buffer_id: BufferId::from(buffer),
        revision: Revision::new(0),
        ops: Vec::new(),
        undo_policy: flavite_buffer::undo::UndoPolicy::Isolated,
        editor: None,
        operation: Some(forged),
    });
    assert!(
        matches!(
            result,
            flavite_vault::session::SessionResult::Error { ref message } if message == "UnknownOperation"
        ),
        "forged must be unknown: {result:?}"
    );
    let _ = Outcome::Applied;
}
