//! RFC-0006 S00 — typed encoding commands through the Flavite facade
//! (`note.reopen_encoding`, `note.convert_encoding`, exposed descriptors).
//!
//! The stack is assembled over an `OsFs` vault at a per-test tempdir with an
//! explicit OS data directory (the device-local encoding-hint store must be
//! hermetic per test — the same shape as `recovery_close.rs`), and every
//! outcome below is a real typed core result, never a UI label.

use flavite_api::{ExecutionResult, Flavite, Invocation, LocalResult, Origin, VaultConfig};
use flavite_encoding::{DecodePolicy, SourceFormat, decode};
use flavite_fs::OsFs;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::{Session, SessionBuilder};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Fixture {
    flavite: Flavite,
    root: PathBuf,
    #[allow(dead_code)]
    temp: tempfile::TempDir,
}

/// A Flavite facade over a tempdir vault with a hermetic device-local data
/// directory (hint bindings and recovery checkpoints never touch user state).
fn fixture() -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("vault");
    std::fs::create_dir_all(&root).expect("vault root");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let cache = temp.path().join("cache");
    std::fs::create_dir_all(&cache).expect("cache dir");
    let db = cache.join("encoding-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index database"));
    let identity = flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![3, 1, 4]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(OsFs::new()) as Box<dyn flavite_fs::Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(flavite_index::IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
        .start()
        .expect("actor start");
    let channels = ChannelSet::new();
    let config = VaultConfig {
        autosave_idle_interval: Duration::from_secs(3600),
        ..VaultConfig::default()
    };
    let session: Session = SessionBuilder::new()
        .with_identity(identity.clone())
        .with_actor(actor)
        .with_buffer_manager(flavite_buffer::manager::BufferManager::new())
        .with_pulse(channels.pulse)
        .with_notice(channels.notice)
        .with_config(config.clone())
        .with_data_dir(&data)
        .build()
        .expect("session");
    let mut flavite = Flavite::open(identity, session, config).expect("facade");
    flavite.set_graph_db_path_for_test(db);
    Fixture { flavite, root, temp }
}

fn write_note(f: &Fixture, name: &str, bytes: &[u8]) -> PathBuf {
    let path = f.root.join(name);
    std::fs::write(&path, bytes).expect("seed note");
    path
}

fn note_bytes(_f: &Fixture, path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("note bytes on disk")
}

fn next_id(flavite: &Flavite) -> u64 {
    flavite.command_registry().next_request_id()
}

fn command(flavite: &Flavite, id: &'static str, args: serde_json::Value) -> ExecutionResult {
    flavite.execute(Invocation::new(next_id(flavite), id, args, Origin::Gui))
}

/// Pump until the admitted operation reaches a terminal result.
#[track_caller]
fn wait_for(flavite: &Flavite, request_id: u64) -> ExecutionResult {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        flavite.pump();
        let done = flavite.wait_for_operation(request_id);
        if !done.is_pending() {
            return done;
        }
        assert!(
            Instant::now() < deadline,
            "operation {request_id} never finished"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[track_caller]
fn success_data(result: ExecutionResult, what: &str) -> serde_json::Value {
    let debug = format!("{result:?}");
    result
        .into_success()
        .unwrap_or_else(|| panic!("{what} did not succeed: {debug}"))
        .data
}

/// Open a note through the API and return `(buffer_id, revision)`.
#[track_caller]
fn open_note(flavite: &Flavite, path: &Path) -> (u64, u64) {
    let result = command(
        flavite,
        "note.open",
        serde_json::json!({ "path": path, "policy": "existing_only" }),
    );
    let data = match result {
        ExecutionResult::Pending { request_id } => success_data(wait_for(flavite, request_id), "note.open"),
        ExecutionResult::Local(LocalResult::Success(success)) => success.data,
        other => panic!("note.open was not admitted: {other:?}"),
    };
    (
        data["buffer_id"].as_u64().expect("buffer id"),
        data["revision"].as_u64().expect("revision"),
    )
}

fn view(flavite: &Flavite, buffer_id: u64) -> flavite_api::BufferView {
    flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
        .expect("buffer view")
}

fn action_code(result: &ExecutionResult) -> String {
    match result {
        ExecutionResult::ActionRequired { code, .. } => code.clone(),
        other => panic!("expected ActionRequired, got {other:?}"),
    }
}

#[test]
fn reopen_with_absent_bom_strips_the_signature_and_reports_the_descriptor() {
    let f = fixture();
    let path = write_note(&f, "bom.md", b"\xef\xbb\xbfbom leading note\n");

    let (buffer_id, _revision) = open_note(&f.flavite, &path);
    assert!(view(&f.flavite, buffer_id).source_format.bom());

    // The raw hash the caller observed, read back from the vault bytes.
    let current = note_bytes(&f, &path);
    let raw_hash = hex::encode(blake3::hash(&current).as_bytes());

    let result = wait_for_terminal_reopen(
        &f,
        serde_json::json!({
            "path": path,
            "raw_hash": raw_hash,
            "encoding": "utf8",
            "bom": "absent",
        }),
    );
    success_data(result, "reopen encoding");

    let reopened_view = view(&f.flavite, buffer_id);
    assert_eq!(
        reopened_view.source_format,
        SourceFormat::utf8(),
        "absent BOM strips the signature: utf8 without a BOM"
    );
    // The signature-shaped leading bytes are preserved as CONTENT (§6.10).
    assert!(reopened_view.text.starts_with('\u{FEFF}'));
}

/// Reopen is synchronous at the core; the facade surface is still uniform.
fn wait_for_terminal_reopen(f: &Fixture, args: serde_json::Value) -> ExecutionResult {
    let result = command(&f.flavite, "note.reopen_encoding", args);
    if let ExecutionResult::Pending { request_id } = result {
        wait_for(&f.flavite, request_id)
    } else {
        result
    }
}

#[test]
fn convert_to_utf16le_bom_then_save_writes_bommed_bytes() {
    let f = fixture();
    let path = write_note(&f, "conv.md", b"convert me\n");

    let (buffer_id, revision) = open_note(&f.flavite, &path);
    let result = command(
        &f.flavite,
        "note.convert_encoding",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "encoding": "utf16le",
            "bom": "present",
        }),
    );
    let data = success_data(result, "convert encoding");
    let new_revision = data["revision"].as_u64().expect("new revision");
    assert!(new_revision > revision, "conversion advances the revision");

    let converted_view = view(&f.flavite, buffer_id);
    assert_eq!(
        converted_view.source_format,
        SourceFormat::utf16le_with_bom()
    );
    assert_eq!(converted_view.text, "convert me\n");

    let saved = command(&f.flavite, "note.save", serde_json::json!(path));
    if let ExecutionResult::Pending { request_id } = saved {
        success_data(wait_for(&f.flavite, request_id), "note.save");
    } else {
        assert!(saved.is_success(), "save: {saved:?}");
    }
    let on_disk = note_bytes(&f, &path);
    assert_eq!(
        &on_disk[..2],
        &[0xFF, 0xFE],
        "saved bytes are UTF-16LE with a BOM"
    );
    let redecoded = decode(&on_disk, DecodePolicy::Auto, &Default::default(), &mut || false)
        .expect("saved bytes decode");
    assert_eq!(redecoded.text(), "convert me\n");
    assert_eq!(redecoded.format(), SourceFormat::utf16le_with_bom());
}

#[test]
fn reopen_with_stale_hash_is_refused_typed() {
    let f = fixture();
    let path = write_note(&f, "stale.md", b"stale source bytes\n");
    open_note(&f.flavite, &path);

    let stale_hash = hex::encode(blake3::hash(b"older bytes\n").as_bytes());
    let result = wait_for_terminal_reopen(
        &f,
        serde_json::json!({
            "path": path,
            "raw_hash": stale_hash,
            "encoding": "utf8",
            "bom": "auto",
        }),
    );
    assert_eq!(action_code(&result), "encoding-stale-source");
    assert!(f.flavite.is_note_open(&path));
}

#[test]
fn reopen_with_dirty_buffer_is_refused_typed() {
    let f = fixture();
    let path = write_note(&f, "dirty.md", b"dirty base\n");
    let (buffer_id, revision) = open_note(&f.flavite, &path);

    let current = view(&f.flavite, buffer_id);
    let edit = command(
        &f.flavite,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": current.revision,
            "ops": [{ "range": [0, 0], "replacement": "draft " }],
        }),
    );
    assert!(edit.is_success(), "apply_edits: {edit:?}");
    let _ = revision;

    let raw_hash =
        hex::encode(blake3::hash(b"dirty base\n").as_bytes());
    let result = wait_for_terminal_reopen(
        &f,
        serde_json::json!({
            "path": path,
            "raw_hash": raw_hash,
            "encoding": "utf8",
            "bom": "auto",
        }),
    );
    assert_eq!(action_code(&result), "encoding-dirty-buffer");
    assert_eq!(
        view(&f.flavite, buffer_id).text,
        "draft dirty base\n",
        "unsaved text survives the refusal"
    );
}

#[test]
fn invalid_encoding_argument_is_rejected_not_silent() {
    let f = fixture();
    let path = write_note(&f, "plain.md", b"plain\n");
    let (buffer_id, revision) = open_note(&f.flavite, &path);

    let result = command(
        &f.flavite,
        "note.convert_encoding",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "encoding": "latin1",
            "bom": "absent",
        }),
    );
    assert!(
        !result.is_success(),
        "an unknown encoding family must not succeed: {result:?}"
    );
}

/// The shared buffer view exposes the exact descriptor of a UTF-16 source.
#[test]
fn buffer_view_exposes_utf16le_bom_descriptor() {
    let f = fixture();
    let mut bytes = vec![0xFF_u8, 0xFE];
    for unit in "u16 view\n".encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    let path = write_note(&f, "u16.md", &bytes);

    let (buffer_id, _revision) = open_note(&f.flavite, &path);
    let buffer_view = view(&f.flavite, buffer_id);
    assert_eq!(
        buffer_view.source_format,
        SourceFormat::utf16le_with_bom(),
        "source_format is exposed exactly"
    );
    assert_eq!(buffer_view.text, "u16 view\n");
}
