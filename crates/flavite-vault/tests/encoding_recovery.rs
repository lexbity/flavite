//! RFC-0006 S00 — encoding-aware recovery and format-binding intent (§6.10).
//!
//! A dirty UTF-16 draft survives a simulated Session crash: the durable
//! RecoveryFile v2 records the exact descriptor, and a fresh Session restores
//! the draft by replaying that recorded format — never a current guess. The
//! second test proves the save path's format-binding intent lifecycle: the
//! pre-save intent is retired and the confirmed entry binds the saved hash.

use blake3::hash;
use flavite_buffer::undo::UndoPolicy;
use flavite_buffer::{BufferId, BufferManager, EditOp, Revision};
use flavite_encoding::{DecodePolicy, SourceFormat, decode};
use flavite_fs::{Fs, MemFs};
use flavite_index::{IndexActor, open_or_init};
use flavite_syntax::text::{Span, TextByte};
use flavite_vault::VaultActorBuilder;
use flavite_vault::actor::VaultActorHandle;
use flavite_vault::channels::{ChannelSet, Notice};
use flavite_vault::session::{
    CommandResult, OpenPolicy, Session, SessionBuilder, SessionCommand, SessionResult,
};
use flavite_vault::types::{VaultConfig, VaultIdentity};
use std::path::{Path, PathBuf};
use std::time::Duration;

fn no_cancel() -> impl FnMut() -> bool {
    move || false
}

/// Independent std-only UTF-16LE reference encoder (never the codec under test).
fn reference_utf16le(text: &str, bom: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bom {
        out.extend_from_slice(&[0xFF, 0xFE]);
    }
    for unit in text.encode_utf16() {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

/// Replicate the session's `native_note_key` (unix: relative path bytes).
fn native_note_key(path: &Path, root: &Path) -> Vec<u8> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        relative.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        relative.to_string_lossy().as_bytes().to_vec()
    }
}

struct Harness {
    identity: VaultIdentity,
    actor: VaultActorHandle,
    fs: MemFs,
    root: PathBuf,
    #[allow(dead_code)]
    db_dir: tempfile::TempDir,
    #[allow(dead_code)]
    data_dir: tempfile::TempDir,
}

impl Harness {
    /// Build the vault stack once; sessions are created over it on demand
    /// (simulating a process restart while the stack state stays on disk).
    fn new() -> Self {
        let fs = MemFs::new();
        let root = PathBuf::from("/test/vault");
        fs.create_dir_all(&root).expect("vault root");
        let db_dir = tempfile::tempdir().expect("index dir");
        let db = db_dir.path().join("index.sqlite3");
        drop(open_or_init(&db).expect("index database"));
        let data_dir = tempfile::tempdir().expect("data dir");
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![7, 7, 7]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).expect("index actor"))
            .build()
            .expect("actor")
            .start()
            .expect("actor start");
        Self {
            identity,
            actor,
            fs,
            root,
            db_dir,
            data_dir,
        }
    }

    /// One Session rooted at the same identity, index and device-local data
    /// directory (the recovery store and encoding hints live there).
    fn session(&self) -> Session {
        let channels = ChannelSet::new();
        SessionBuilder::new()
            .with_identity(self.identity.clone())
            .with_actor(self.actor.clone())
            .with_buffer_manager(BufferManager::new())
            .with_pulse(channels.pulse)
            .with_notice(channels.notice)
            .with_config(VaultConfig {
                autosave_idle_interval: Duration::from_secs(3600),
                ..VaultConfig::default()
            })
            .with_data_dir(self.data_dir.path())
            .build()
            .expect("session")
    }
}

fn wait_terminal(session: &Session, result: SessionResult) -> SessionResult {
    match result {
        SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
        other => other,
    }
}

#[track_caller]
fn expect_success(result: SessionResult, what: &str) -> CommandResult {
    match result {
        SessionResult::Success(command) => command,
        other => panic!("{what} did not succeed: {other:?}"),
    }
}

fn open_to_terminal(session: &Session, path: PathBuf) -> SessionResult {
    wait_terminal(
        session,
        session.execute(SessionCommand::OpenNote {
            path,
            policy: OpenPolicy::CreateIfMissing,
        }),
    )
}

/// A dirty UTF-16 draft survives a Session crash: the fresh Session offers the
/// recovery, restore replays the RECORDED format (UTF-16LE+BOM), and the draft
/// text is intact (§6.10, AC-25).
#[test]
fn utf16_dirty_draft_recovery_replays_recorded_format() {
    let h = Harness::new();
    let disk_text = "saved base\r\nsecond line\n";
    let original = reference_utf16le(disk_text, true);
    let path = h.root.join("draft.md");
    h.fs.write_new(&path, &original).expect("seed utf16le note");

    // Session A: open, edit (dirty), durably checkpoint the recovery.
    let session_a = h.session();
    let opened = expect_success(
        open_to_terminal(&session_a, path.clone()),
        "first open",
    );
    let (buffer_id, revision) = match opened {
        CommandResult::NoteOpened {
            buffer_id,
            revision,
        } => (buffer_id, revision),
        other => panic!("expected NoteOpened, got {other:?}"),
    };
    let result = session_a.execute(SessionCommand::ApplyEdits {
        buffer_id,
        revision,
        ops: vec![EditOp::new(Span::new(TextByte(0), TextByte(0)), "DIRTY DRAFT ".to_owned())],
        undo_policy: UndoPolicy::Isolated,
    });
    expect_success(result, "apply draft edit");

    let flushed = expect_success(
        session_a.execute(SessionCommand::FlushRecovery { path: path.clone() }),
        "flush recovery",
    );
    let CommandResult::RecoveryFlushed {
        buffer_id: flushed_id,
        revision: flushed_revision,
        ..
    } = flushed
    else {
        panic!("expected RecoveryFlushed, got {flushed:?}");
    };
    assert_eq!(flushed_id, buffer_id);
    assert!(flushed_revision >= revision);

    // Simulate the crash: the Session (and its recovery writer state) is gone.
    drop(session_a);

    // Session B: the same stack state, a fresh Session over the same data dir.
    let session_b = h.session();
    let opened = expect_success(
        open_to_terminal(&session_b, path.clone()),
        "open after crash",
    );
    let CommandResult::NoteOpened {
        buffer_id: fresh_id,
        revision: fresh_revision,
    } = opened
    else {
        panic!("expected NoteOpened, got {opened:?}");
    };
    assert_ne!(
        fresh_id, buffer_id,
        "a fresh Session installs a fresh buffer instance"
    );

    // The open offered the recorded draft instead of silently dropping it.
    let notices = session_b.drain_notices();
    assert!(
        notices
            .iter()
            .any(|notice| matches!(notice, Notice::RecoveryOffer { note } if note.path == path)),
        "recovery offer expected, got {notices:?}"
    );

    // Restore replays the recovery: the draft text is intact and the buffer's
    // descriptor is the RECORDED utf16le+bom — descriptor replay, not a guess.
    let restored = expect_success(
        wait_terminal(
            &session_b,
            session_b.execute(SessionCommand::RestoreRecovery { path: path.clone() }),
        ),
        "restore recovery",
    );
    let CommandResult::RecoveryRestored {
        buffer_id: restored_id,
        revision: restored_revision,
    } = restored
    else {
        panic!("expected RecoveryRestored, got {restored:?}");
    };
    assert_eq!(restored_id, fresh_id);
    assert!(restored_revision > fresh_revision, "restore is a new revision");

    let snapshot = session_b
        .shared_buffer_snapshot(restored_id, None)
        .expect("snapshot after restore");
    assert_eq!(
        snapshot.text.to_string(),
        "DIRTY DRAFT saved base\nsecond line\n",
        "the draft edit survived the crash"
    );
    assert_eq!(
        snapshot.source_format,
        SourceFormat::utf16le_with_bom(),
        "the recorded descriptor is replayed, not re-guessed"
    );
    assert_eq!(snapshot.state, flavite_buffer::state::BufferState::Dirty);

    // The restored draft still serializes under its recorded descriptor.
    let expected = reference_utf16le("DIRTY DRAFT saved base\r\nsecond line\n", true);
    let redecoded = decode(&expected, DecodePolicy::Auto, &Default::default(), &mut no_cancel())
        .expect("expected bytes decode");
    assert_eq!(redecoded.text(), "DIRTY DRAFT saved base\r\nsecond line\n");
}

/// After a UTF-16 save completes, the hint record has no stale pre-save
/// intent for the key and the confirmed entry binds the exact saved hash.
#[test]
fn utf16_save_retires_intent_and_binds_saved_hash() {
    let h = Harness::new();
    let path = h.root.join("intent.md");
    h.fs
        .write_new(&path, &reference_utf16le("intent base\n", true))
        .expect("seed utf16le note");

    let session = h.session();
    let opened = expect_success(open_to_terminal(&session, path.clone()), "open");
    let (buffer_id, revision) = match opened {
        CommandResult::NoteOpened {
            buffer_id,
            revision,
        } => (buffer_id, revision),
        other => panic!("expected NoteOpened, got {other:?}"),
    };
    let result = session.execute(SessionCommand::ApplyEdits {
        buffer_id,
        revision,
        ops: vec![EditOp::new(Span::new(TextByte(11), TextByte(11)), "more".to_owned())],
        undo_policy: UndoPolicy::Isolated,
    });
    expect_success(result, "apply edit");

    let saved = expect_success(
        wait_terminal(&session, session.execute(SessionCommand::SaveNote { path: path.clone() })),
        "save",
    );
    let CommandResult::NoteSaved { hash: saved_hash, .. } = saved else {
        panic!("expected NoteSaved, got {saved:?}");
    };

    let on_disk = h
        .fs
        .read_if_exists(&path)
        .expect("read")
        .expect("note exists after save");
    let hex_key = hex::encode(native_note_key(&path, &h.root));
    let view = session.encoding_hints();

    // The pre-save binding intent was resolved: nothing stale remains.
    assert!(
        view.record.intents.get(&hex_key).is_none(),
        "stale intent must be retired after the durable save, got {:?}",
        view.record.intents.get(&hex_key)
    );
    // The confirmed entry binds exactly the bytes that reached disk.
    let entry = view
        .record
        .entries
        .get(&hex_key)
        .expect("confirmed hint entry exists after save");
    assert_eq!(entry.encoding, flavite_encoding::FileEncoding::Utf16Le);
    assert!(entry.bom);
    assert_eq!(entry.bound_raw_hash, *hash(&on_disk).as_bytes());
    assert_eq!(saved_hash.as_slice(), hash(&on_disk).as_bytes());
}
