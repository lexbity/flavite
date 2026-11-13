//! RFC-0006 S00 — Session-level encoding workflow (open/edit/save/reopen/
//! convert/choice) over a real Session + MemFs vault (§6.10).
//!
//! The harness mirrors the in-crate `Session` tests but builds the stack
//! directly here (the in-crate harness is `#[cfg(test)]`), with an explicit
//! data directory so encoding-hint bindings and recovery checkpoints are
//! hermetic per test.

use blake3::hash;
use flavite_buffer::undo::UndoPolicy;
use flavite_buffer::{BufferId, BufferManager, EditOp, Revision};
use flavite_encoding::{
    DecodeLimits, DecodePolicy, FileEncoding, NOT_CANCELLED, SourceFormat, decode,
};
use flavite_fs::{Fs, MemFs};
use flavite_index::{IndexActor, open_or_init};
use flavite_syntax::text::{Span, TextByte};
use flavite_vault::channels::ChannelSet;
use flavite_vault::encoding::BomMode;
use flavite_vault::session::{
    CommandResult, OpenPolicy, Session, SessionBuilder, SessionCommand, SessionResult,
};
use flavite_vault::types::{VaultConfig, VaultIdentity};
use flavite_vault::{VaultActorBuilder, VaultActorHandle};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Independent std-only UTF-16 reference encoder (never the codec under test).
fn reference_utf16(text: &str, le: bool, bom: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if bom {
        out.extend_from_slice(if le { &[0xFF, 0xFE] } else { &[0xFE, 0xFF] });
    }
    for unit in text.encode_utf16() {
        let encoded = if le {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        out.extend_from_slice(&encoded);
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
    session: Session,
    fs: MemFs,
    root: PathBuf,
    #[allow(dead_code)]
    db_dir: tempfile::TempDir,
    #[allow(dead_code)]
    data_dir: tempfile::TempDir,
}

/// Build a Session over MemFs with hermetic index + device-local data dirs,
/// exactly like the in-crate `test_session` harness but with the explicit
/// data dir the hint store and recovery writer root under.
fn harness() -> Harness {
    let fs = MemFs::new();
    let root = PathBuf::from("/test/vault");
    fs.create_dir_all(&root).expect("vault root");
    let db_dir = tempfile::tempdir().expect("index dir");
    let db = db_dir.path().join("index.sqlite3");
    drop(open_or_init(&db).expect("index database"));
    let data_dir = tempfile::tempdir().expect("data dir");
    let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![1, 2, 3]);
    let actor: VaultActorHandle = VaultActorBuilder::new()
        .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
        .start()
        .expect("actor start");
    let channels = ChannelSet::new();
    let session = SessionBuilder::new()
        .with_identity(identity)
        .with_actor(actor)
        .with_buffer_manager(BufferManager::new())
        .with_pulse(channels.pulse)
        .with_notice(channels.notice)
        .with_config(VaultConfig {
            autosave_idle_interval: Duration::from_secs(3600),
            ..VaultConfig::default()
        })
        .with_data_dir(data_dir.path())
        .build()
        .expect("session");
    Harness {
        session,
        fs,
        root,
        db_dir,
        data_dir,
    }
}

/// Drive an admitted flight to its terminal result.
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

fn expect_action_required(result: SessionResult, code: &'static str) -> Option<String> {
    match result {
        SessionResult::ActionRequired { code: got, data } if got == code => data,
        other => panic!("expected ActionRequired({code}), got {other:?}"),
    }
}

/// Open through the async flight and wait, mirroring the in-crate helper.
fn open_to_terminal(session: &Session, path: PathBuf) -> SessionResult {
    wait_terminal(
        session,
        session.execute(SessionCommand::OpenNote {
            path,
            policy: OpenPolicy::CreateIfMissing,
        }),
    )
}

fn save_to_terminal(session: &Session, path: PathBuf) -> SessionResult {
    wait_terminal(session, session.execute(SessionCommand::SaveNote { path }))
}

fn opened_id_revision(result: &CommandResult) -> (BufferId, Revision) {
    match result {
        CommandResult::NoteOpened {
            buffer_id,
            revision,
        } => (*buffer_id, *revision),
        other => panic!("expected NoteOpened, got {other:?}"),
    }
}

/// Open a note and return its (buffer id, revision, normalized text, format).
#[track_caller]
fn open_note(session: &Session, path: &Path) -> (BufferId, Revision, String, SourceFormat) {
    let opened = expect_success(open_to_terminal(session, path.to_path_buf()), "note.open");
    let (buffer_id, revision) = opened_id_revision(&opened);
    let snapshot = session
        .shared_buffer_snapshot(buffer_id, None)
        .expect("shared snapshot");
    (
        buffer_id,
        revision,
        snapshot.text.to_string(),
        snapshot.source_format,
    )
}

#[test]
fn utf16le_bom_open_edit_save_preserves_descriptor_and_bytes() {
    let h = harness();
    let text = "A😀\r\nsecond\n";
    let original = reference_utf16(text, true, true);
    let path = h.root.join("u16.md");
    h.fs.write_new(&path, &original).expect("seed utf16le note");

    let (buffer_id, _revision, buffer_text, format) = open_note(&h.session, &path);
    assert_eq!(buffer_text, "A😀\nsecond\n", "supplementary chars survive");
    assert_eq!(format, SourceFormat::utf16le_with_bom());

    // One edit in the middle of the normalized layer: 's' of "second".
    let start = buffer_text.find("second").expect("needle");
    let result = h.session.execute(SessionCommand::ApplyEdits {
        buffer_id,
        revision: session_revision(&h.session, buffer_id),
        ops: vec![EditOp::new(
            Span::new(TextByte(start), TextByte(start + 1)),
            "X".to_owned(),
        )],
        undo_policy: UndoPolicy::Isolated,
        editor: None,
        operation: None,
    });
    let applied = expect_success(result, "apply edits");
    let CommandResult::EditsApplied { new_revision, .. } = applied else {
        panic!("expected EditsApplied, got {applied:?}");
    };

    let saved = expect_success(save_to_terminal(&h.session, path.clone()), "save");
    let CommandResult::NoteSaved {
        hash: saved_hash, ..
    } = saved
    else {
        panic!("expected NoteSaved, got {saved:?}");
    };
    assert_eq!(
        saved_hash,
        hash(&reference_utf16("A😀\r\nXecond\n", true, true))
            .as_bytes()
            .to_vec()
    );

    // The disk bytes are UTF-16LE with BOM and carry exactly the edit.
    let on_disk =
        h.fs.read_if_exists(&path)
            .expect("read")
            .expect("note exists after save");
    assert_eq!(&on_disk[..2], &[0xFF, 0xFE], "BOM preserved byte-exactly");
    let redecoded = decode(
        &on_disk,
        DecodePolicy::Auto,
        &DecodeLimits::default(),
        &NOT_CANCELLED,
    )
    .expect("saved bytes decode");
    assert_eq!(redecoded.text(), "A😀\r\nXecond\n");
    assert_eq!(redecoded.format(), SourceFormat::utf16le_with_bom());
    assert!(redecoded.text().contains("\r\n"), "CRLF preserved");

    // The saved receipt hash matches the exact bytes on disk.
    assert_eq!(saved_hash.as_slice(), hash(&on_disk).as_bytes());
    let _ = new_revision;
}

fn session_revision(session: &Session, buffer_id: BufferId) -> Revision {
    session
        .shared_buffer_snapshot(buffer_id, None)
        .expect("snapshot")
        .revision
}

/// Reopen under an explicit family reinterprets the same bytes, keeps the
/// text, and binds the confirmed hint to the observed raw hash.
#[test]
fn reopen_encoding_binds_hint_and_preserves_text() {
    let h = harness();
    let bytes: &[u8] = b"\xef\xbb\xbfbom note body\n";
    let path = h.root.join("bom.md");
    h.fs.write_new(&path, bytes).expect("seed utf8+bom note");

    let (buffer_id, revision, text, format) = open_note(&h.session, &path);
    assert_eq!(text, "bom note body\n");
    assert_eq!(format, SourceFormat::utf8_with_bom());

    let raw_hash: [u8; 32] = *hash(bytes).as_bytes();
    let result = h.session.execute(SessionCommand::ReopenEncoding {
        path: path.clone(),
        expected_raw_hash: raw_hash,
        encoding: FileEncoding::Utf8,
        bom: BomMode::Auto,
    });
    let reopened = expect_success(result, "reopen encoding");
    let CommandResult::EncodingReopened {
        buffer_id: reopened_id,
        revision: reopened_revision,
    } = reopened
    else {
        panic!("expected EncodingReopened, got {reopened:?}");
    };
    assert_eq!(reopened_id, buffer_id, "same shared buffer");
    assert!(reopened_revision > revision, "reopen advances the revision");

    let snapshot = h
        .session
        .shared_buffer_snapshot(buffer_id, None)
        .expect("snapshot after reopen");
    assert_eq!(
        snapshot.text.to_string(),
        "bom note body\n",
        "text unchanged"
    );
    // `auto` strips the matching signature for the requested family.
    assert_eq!(snapshot.source_format, SourceFormat::utf8_with_bom());

    // The hint binding is recorded under the hex native key.
    let hex_key = hex::encode(native_note_key(&path, &h.root));
    let view = h.session.encoding_hints();
    let entry = view
        .record
        .entries
        .get(&hex_key)
        .expect("confirmed hint entry exists");
    assert_eq!(entry.encoding, FileEncoding::Utf8);
    assert!(entry.bom);
    assert_eq!(entry.bound_raw_hash, raw_hash);
}

/// A dirty buffer refuses reopen with the typed dirty-buffer action; unsaved
/// text is never implicitly discarded.
#[test]
fn reopen_encoding_refuses_dirty_buffer() {
    let h = harness();
    let path = h.root.join("dirty.md");
    h.fs.write_new(&path, b"saved base\n").expect("seed");
    let (buffer_id, revision, _text, _format) = open_note(&h.session, &path);

    let result = h.session.execute(SessionCommand::ApplyEdits {
        buffer_id,
        revision,
        ops: vec![EditOp::new(
            Span::new(TextByte(0), TextByte(0)),
            "draft ".to_owned(),
        )],
        undo_policy: UndoPolicy::Isolated,
        editor: None,
        operation: None,
    });
    expect_success(result, "apply edits");

    let raw_hash: [u8; 32] = *hash(b"saved base\n").as_bytes();
    let result = h.session.execute(SessionCommand::ReopenEncoding {
        path: path.clone(),
        expected_raw_hash: raw_hash,
        encoding: FileEncoding::Utf8,
        bom: BomMode::Auto,
    });
    expect_action_required(result, "encoding-dirty-buffer");

    // The draft is intact and no hint was bound for this refusal.
    let snapshot = h
        .session
        .shared_buffer_snapshot(buffer_id, None)
        .expect("snapshot");
    assert_eq!(snapshot.text.to_string(), "draft saved base\n");
    let view = h.session.encoding_hints();
    assert!(
        !view
            .record
            .entries
            .contains_key(&hex::encode(native_note_key(&path, &h.root))),
        "a refused reopen binds no hint"
    );
}

/// A stale raw-hash precondition is refused before any decode of bytes the
/// caller never saw.
#[test]
fn reopen_encoding_refuses_stale_source() {
    let h = harness();
    let path = h.root.join("stale.md");
    h.fs.write_new(&path, b"current bytes\n").expect("seed");
    open_note(&h.session, &path);

    let stale_hash: [u8; 32] = *hash(b"older bytes the caller saw once\n").as_bytes();
    let result = h.session.execute(SessionCommand::ReopenEncoding {
        path: path.clone(),
        expected_raw_hash: stale_hash,
        encoding: FileEncoding::Utf8,
        bom: BomMode::Auto,
    });
    expect_action_required(result, "encoding-stale-source");
    assert!(h.session.is_open(&path), "refusal leaves the buffer alone");
}

/// Convert changes the save format with one isolated boundary; the next save
/// writes the converted bytes to disk.
#[test]
fn convert_encoding_then_save_writes_utf16be_bom() {
    let h = harness();
    let path = h.root.join("convert.md");
    h.fs.write_new(&path, b"# converted\n\nbody\n")
        .expect("seed");
    let (buffer_id, revision, text, format) = open_note(&h.session, &path);
    assert_eq!(format, SourceFormat::utf8());
    assert_eq!(text, "# converted\n\nbody\n");

    let result = h.session.execute(SessionCommand::ConvertEncoding {
        buffer_id,
        revision,
        target: SourceFormat::utf16be_with_bom(),
    });
    let converted = expect_success(result, "convert encoding");
    let CommandResult::EncodingConverted {
        buffer_id: converted_id,
        new_revision,
    } = converted
    else {
        panic!("expected EncodingConverted, got {converted:?}");
    };
    assert_eq!(converted_id, buffer_id);
    assert!(new_revision > revision, "conversion advances the revision");

    let snapshot = h
        .session
        .shared_buffer_snapshot(buffer_id, None)
        .expect("snapshot after convert");
    assert_eq!(
        snapshot.text.to_string(),
        text,
        "text unchanged by conversion"
    );
    assert_eq!(snapshot.source_format, SourceFormat::utf16be_with_bom());

    expect_success(save_to_terminal(&h.session, path.clone()), "save");
    let on_disk =
        h.fs.read_if_exists(&path)
            .expect("read")
            .expect("note exists");
    assert_eq!(&on_disk[..2], &[0xFE, 0xFF], "UTF-16BE BOM on disk");
    let redecoded = decode(
        &on_disk,
        DecodePolicy::Auto,
        &DecodeLimits::default(),
        &NOT_CANCELLED,
    )
    .expect("saved bytes decode");
    assert_eq!(redecoded.text(), "# converted\n\nbody\n");
    assert_eq!(redecoded.format(), SourceFormat::utf16be_with_bom());
}

/// Malformed bytes require an explicit encoding choice and never install an
/// empty editable buffer (no-empty-fallback, FR-24).
#[test]
fn malformed_open_requires_choice_and_installs_nothing() {
    let h = harness();
    let malformed: &[u8] = b"plain \xff bytes\n";
    let path = h.root.join("broken.md");
    h.fs.write_new(&path, malformed)
        .expect("seed malformed note");

    let result = open_to_terminal(&h.session, path.clone());
    let data = expect_action_required(result, "encoding-choice-required");
    let payload = data.expect("choice payload carries the sanitized reason");
    assert!(
        payload.contains("invalid-utf8"),
        "payload names the typed kind: {payload}"
    );
    // The observed raw hash is present so a caller can reopen against
    // exactly the bytes it saw.
    assert!(
        payload.contains(&hex::encode(hash(malformed).as_bytes())),
        "payload carries the observed raw hash: {payload}"
    );

    assert!(
        !h.session.is_open(&path),
        "no buffer is installed for malformed input"
    );
    assert!(h.session.get_buffer_id(&path).is_none());
}
