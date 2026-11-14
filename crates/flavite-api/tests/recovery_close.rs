//! W12 close/retain/discard/recovery policies end to end over a real OS
//! filesystem vault (RFC-0003 W12, D-10/D-12).
//!
//! Durability semantics cannot be observed on `MemFs`: retained recovery
//! copies live in the OS data directory (redirected here to a per-test
//! tempdir through the explicit data-dir builder argument) and must survive buffer close,
//! process-style crash simulation, and vault reopen. Every test builds a
//! real `OsFs` stack through `Flavite::open_root` and drives the typed API
//! commands (`note.close`, `recovery.flush`, `recovery.restore`,
//! `recovery.discard`, `note.discard_edits`).

use flavite_api::{ExecutionResult, Flavite, Invocation, LocalResult, Origin, VaultConfig};
use flavite_fs::OsFs;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The tests here redirect the OS data directory (which owns the recovery
/// snapshot store and the vault registry) to a per-test tempdir. The
/// environment is process-global, so the test bodies serialize on this lock
/// while any session that captures the directory is alive.
static DATA_DIR_LOCK: Mutex<()> = Mutex::new(());

struct Fixture {
    flavite: Flavite,
    root: PathBuf,
    /// Keeps the vault, data, and graph-cache directories alive.
    #[allow(dead_code)]
    temp: tempfile::TempDir,
}

/// Build a real-OsFs vault at a fresh tempdir with the OS data directory
/// redirected next to it. Autosave is disabled so close/discard policies
/// are the only thing that can move bytes.
///
/// The stack is assembled manually (actor + Session + facade over `OsFs`)
/// so the vault needs no registry, lock, or IPC state in any data
/// directory; the recovery store itself is real and lives under
/// the explicit data-dir builder argument.
#[must_use]
fn fixture(notes: &[(&str, &str)]) -> Fixture {
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("vault");
    std::fs::create_dir_all(&root).expect("vault root");
    for (name, body) in notes {
        let path = root.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("note directory");
        }
        std::fs::write(&path, body).expect("seed note");
    }
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let cache = temp.path().join("cache");
    std::fs::create_dir_all(&cache).expect("cache dir");
    let db = cache.join("index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index database"));
    let identity =
        flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![12, 4, 9]);
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
    let session = Session::new_with_data_dir(
        identity.clone(),
        actor,
        flavite_buffer::manager::BufferManager::new(),
        channels.pulse,
        channels.notice,
        config.clone(),
        Some(data),
    );
    let mut flavite = Flavite::open(identity, session, config).expect("facade");
    flavite.set_graph_db_path_for_test(db);
    Fixture {
        flavite,
        root,
        temp,
    }
}

fn next_id(flavite: &Flavite) -> u64 {
    flavite.command_registry().next_request_id()
}

fn command(flavite: &Flavite, id: &'static str, args: serde_json::Value) -> ExecutionResult {
    flavite.execute(editor_operations_fixture::admit(
        &flavite,
        Invocation::new(next_id(flavite), id, args, Origin::Gui),
    ))
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
    let success = result
        .into_success()
        .unwrap_or_else(|| panic!("{what} did not succeed: {debug}"));
    success.data
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
        ExecutionResult::Pending { request_id } => {
            success_data(wait_for(flavite, request_id), "note.open")
        }
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

/// Replace the whole buffer content through the core edit command.
#[track_caller]
fn edit(flavite: &Flavite, buffer_id: u64, replacement: &str) -> u64 {
    let current = view(flavite, buffer_id);
    let result = command(
        flavite,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": current.revision,
            "ops": [{ "range": [0, current.text.len()], "replacement": replacement }],
        }),
    );
    success_data(result, "apply_edits")["revision"]
        .as_u64()
        .expect("new revision")
}

/// Drain events until a `RecoveryOffer` for `path` is observable.
#[track_caller]
fn wait_for_recovery_offer(flavite: &Flavite, path: &Path) {
    let wanted = path.to_string_lossy().into_owned();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        flavite.pump();
        for delivery in flavite.poll_events(64) {
            if let flavite_api::Event::RecoveryOffer { note_path, .. } = delivery.event
                && note_path == wanted
            {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "no recovery offer arrived for {wanted}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn close_retaining_recovery_writes_a_revision_bound_receipt() {
    let _data_dir = DATA_DIR_LOCK.lock().unwrap();
    let f = fixture(&[("a.md", "prior body\n")]);
    let flavite = &f.flavite;
    let path = f.root.join("a.md");
    let (buffer_id, _) = open_note(flavite, &path);
    let edited = "edited body\n";
    let revision = edit(flavite, buffer_id, edited);

    // The explicit durable checkpoint returns a receipt bound to the exact
    // revision (and buffer) whose bytes it retained.
    let receipt = success_data(
        command(
            flavite,
            "recovery.flush",
            serde_json::json!({ "path": path }),
        ),
        "recovery.flush",
    );
    assert_eq!(
        receipt["revision"].as_u64(),
        Some(revision),
        "the receipt covers the edited revision: {receipt}"
    );
    assert_eq!(
        receipt["buffer_id"].as_u64(),
        Some(buffer_id),
        "the receipt names the retained buffer: {receipt}"
    );

    // Close with retention: the dirty buffer is dropped only once a durable
    // snapshot provably covers it.
    let closed = command(
        flavite,
        "note.close",
        serde_json::json!({ "path": path, "policy": "retain_recovery" }),
    );
    assert!(
        closed.is_success(),
        "retain_recovery close must succeed: {closed:?}"
    );
    assert!(!flavite.is_note_open(&path), "the buffer is dropped");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"prior body\n",
        "a retaining close never writes the disk file"
    );

    // Reopen: the retained copy outlives the buffer and is offered again.
    let (reopened, _) = open_note(flavite, &path);
    assert!(reopened > 0, "the note reopens into a fresh buffer");
    wait_for_recovery_offer(flavite, &path);

    // The retained copy restores the exact pre-close bytes.
    let restored = success_data(
        command(
            flavite,
            "recovery.restore",
            serde_json::json!({ "path": path }),
        ),
        "recovery.restore",
    );
    let restored_buffer = restored["buffer_id"].as_u64().expect("restored buffer id");
    assert_eq!(
        view(flavite, restored_buffer).text,
        edited,
        "the retained copy restores the pre-close content"
    );
}

#[test]
fn plain_close_refuses_dirty_and_pending_save() {
    let _data_dir = DATA_DIR_LOCK.lock().unwrap();
    let f = fixture(&[("a.md", "disk body\n")]);
    let flavite = &f.flavite;
    let path = f.root.join("a.md");
    let (buffer_id, _) = open_note(flavite, &path);
    edit(flavite, buffer_id, "dirty body\n");

    // A dirty buffer refuses a plain close.
    let refused = command(
        flavite,
        "note.close",
        serde_json::json!({ "path": path, "policy": "clean" }),
    );
    match refused {
        ExecutionResult::ActionRequired { code, .. } => {
            assert_eq!(code, "dirty-buffer-close", "dirty close is refused");
        }
        other => panic!("a dirty plain close must be refused: {other:?}"),
    }
    assert!(
        flavite.is_note_open(&path),
        "a refused close keeps the buffer open"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"disk body\n",
        "the refused close wrote nothing"
    );

    // A save in flight refuses every close.
    let save = command(
        flavite,
        "note.save",
        serde_json::json!(path.to_string_lossy()),
    );
    let save_request = match save {
        ExecutionResult::Pending { request_id } => request_id,
        other => panic!("the save should be admitted: {other:?}"),
    };
    let pending = command(
        flavite,
        "note.close",
        serde_json::json!({ "path": path, "policy": "clean" }),
    );
    match pending {
        ExecutionResult::ActionRequired { code, .. } => {
            assert_eq!(code, "save-pending", "a save flight blocks the close");
        }
        other => panic!("a close during a save flight must be refused: {other:?}"),
    }

    // Pump the save to completion; the clean close then succeeds.
    let saved = wait_for(flavite, save_request);
    assert!(saved.is_success(), "the save completes: {saved:?}");
    let closed = command(
        flavite,
        "note.close",
        serde_json::json!({ "path": path, "policy": "clean" }),
    );
    assert!(
        closed.is_success(),
        "a clean buffer closes once the save is done: {closed:?}"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"dirty body\n",
        "the completed save reached the disk"
    );
}

#[test]
fn discard_edits_requires_the_disk_file_and_keeps_history() {
    let _data_dir = DATA_DIR_LOCK.lock().unwrap();
    let f = fixture(&[("a.md", "disk body\n")]);
    let flavite = &f.flavite;
    let path = f.root.join("a.md");
    let (buffer_id, _) = open_note(flavite, &path);
    let pre_discard = "edited body\n";
    edit(flavite, buffer_id, pre_discard);

    let discarded = success_data(
        command(
            flavite,
            "note.discard_edits",
            serde_json::json!({ "path": path }),
        ),
        "note.discard_edits",
    );
    assert_eq!(
        discarded["buffer_id"].as_u64(),
        Some(buffer_id),
        "the buffer survives the reload"
    );

    // The buffer is reloaded from the exact disk bytes.
    let reloaded = view(flavite, buffer_id);
    assert_eq!(reloaded.text, "disk body\n");
    assert!(
        matches!(reloaded.state, flavite_buffer::state::BufferState::Clean),
        "the reloaded buffer is clean"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"disk body\n",
        "the disk file is untouched by the discard"
    );

    // The pre-discard content is retrievable through local history.
    let history = success_data(
        command(
            flavite,
            "note.history",
            serde_json::json!({ "path": path, "limit": 20 }),
        ),
        "note.history",
    );
    let entries = history["entries"].as_array().expect("history entries");
    let entry = entries
        .iter()
        .find(|entry| entry["reason"].as_str() == Some("conflict_resolution"))
        .expect("the discarded bytes are snapshotted into local history");
    let hash = entry["hash"].as_str().expect("snapshot hash").to_owned();
    let restored = success_data(
        command(
            flavite,
            "note.restore_history",
            serde_json::json!({
                "path": path,
                "buffer_id": buffer_id,
                "revision": view(flavite, buffer_id).revision,
                "hash": hash,
            }),
        ),
        "note.restore_history",
    );
    assert!(
        restored["revision"].as_u64().is_some(),
        "the history restore completes: {restored}"
    );
    assert_eq!(
        view(flavite, buffer_id).text,
        pre_discard,
        "history hands back the discarded bytes"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"disk body\n",
        "a history restore never writes the disk"
    );

    // Discard the restored edits again to leave the buffer at disk parity.
    success_data(
        command(
            flavite,
            "note.discard_edits",
            serde_json::json!({ "path": path }),
        ),
        "second discard",
    );
    assert_eq!(view(flavite, buffer_id).text, "disk body\n");

    // Discarding requires the disk file: with it gone, the host is asked.
    std::fs::remove_file(&path).expect("remove the disk file");
    let deleted = command(
        flavite,
        "note.discard_edits",
        serde_json::json!({ "path": path }),
    );
    match deleted {
        ExecutionResult::ActionRequired { code, .. } => {
            assert_eq!(code, "file-deleted", "a missing disk file is reported");
        }
        other => panic!("discarding without a disk file must ask the host: {other:?}"),
    }
}

#[test]
fn restore_then_discard_recovery_round_trip() {
    let _data_dir = DATA_DIR_LOCK.lock().unwrap();
    let f = fixture(&[("a.md", "disk body\n")]);
    let flavite = &f.flavite;
    let path = f.root.join("a.md");
    let (buffer_id, _) = open_note(flavite, &path);
    edit(flavite, buffer_id, "retained body\n");

    // Retain the unsaved edits and drop the buffer (test 1's flow).
    let closed = command(
        flavite,
        "note.close",
        serde_json::json!({ "path": path, "policy": "retain_recovery" }),
    );
    assert!(closed.is_success(), "retain close succeeds: {closed:?}");
    assert!(!flavite.is_note_open(&path));
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"disk body\n",
        "retaining never writes the disk"
    );

    // Restore: the retained copy comes back into an open buffer.
    let restored = success_data(
        command(
            flavite,
            "recovery.restore",
            serde_json::json!({ "path": path }),
        ),
        "recovery.restore",
    );
    let restored_buffer = restored["buffer_id"].as_u64().expect("restored buffer id");
    assert_eq!(
        view(flavite, restored_buffer).text,
        "retained body\n",
        "the retained bytes are restored into the buffer"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"disk body\n",
        "a restore never writes the disk"
    );

    // Discard: the retained copy is gone; a second restore fails while the
    // already-restored open buffer is left alone.
    success_data(
        command(
            flavite,
            "recovery.discard",
            serde_json::json!({ "path": path }),
        ),
        "recovery.discard",
    );
    let second = command(
        flavite,
        "recovery.restore",
        serde_json::json!({ "path": path }),
    );
    let second_debug = format!("{second:?}");
    assert!(
        second.into_error().is_some(),
        "a discarded snapshot cannot be restored again: {second_debug}"
    );
    assert_eq!(
        view(flavite, restored_buffer).text,
        "retained body\n",
        "the discard leaves the open buffer untouched"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"disk body\n",
        "the disk file is still untouched"
    );
}

/// Fault-injected crash around the durable write phase of a save flight
/// (`fault` feature). The retained recovery copy was flushed durably BEFORE
/// the crash; the crashed process's filesystem state is revived and the
/// vault is reopened: the retained copy must be intact and recover the
/// exact pre-crash buffer content.
#[cfg(feature = "fault")]
mod fault_injected {
    use super::{DATA_DIR_LOCK, command, edit, open_note, success_data, view, wait_for};
    use flavite_api::{ExecutionResult, Flavite, LocalResult, VaultConfig};
    use flavite_fs::{
        DirEntry, FaultBoundary, FaultFs, FaultKind, FaultScript, Fs, MemFs, Metadata, Result,
    };
    use flavite_vault::actor::VaultActorBuilder;
    use flavite_vault::channels::ChannelSet;
    use flavite_vault::session::Session;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    /// `Fs` view over a shared fault-injected filesystem, so the test keeps
    /// a handle for `is_alive`/`revive` while the actor owns a boxed `Fs`.
    struct SharedFaultFs(Arc<FaultFs<MemFs>>);

    impl Fs for SharedFaultFs {
        fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
            self.0.canonicalize(path)
        }
        fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
            self.0.resolve_symlink(path)
        }
        fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
            self.0.encode_path_key(path)
        }
        fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
            self.0.decode_path_key(key)
        }
        fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()> {
            self.0.validate_in_root(root, target)
        }
        fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
            self.0.read_if_exists(path)
        }
        fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
            self.0.read_capped(path, max)
        }
        fn metadata(&self, path: &Path) -> Result<Metadata> {
            self.0.metadata(path)
        }
        fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
            self.0.read_dir(path)
        }
        fn exists(&self, path: &Path) -> Result<bool> {
            self.0.exists(path)
        }
        fn is_placeholder(&self, path: &Path) -> Result<bool> {
            self.0.is_placeholder(path)
        }
        fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
            self.0.write_new(path, bytes)
        }
        fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
            self.0.copy_mode(from, to)
        }
        fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.0.create_dir_all(path)
        }
        fn remove(&self, path: &Path) -> Result<()> {
            self.0.remove(path)
        }
        fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
            self.0.rename_replace(from, to)
        }
        fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
            self.0.rename_no_replace(from, to)
        }
        fn fsync_file(&self, path: &Path) -> Result<()> {
            self.0.fsync_file(path)
        }
        fn fsync_dir(&self, path: &Path) -> Result<()> {
            self.0.fsync_dir(path)
        }
    }

    #[test]
    fn flush_survives_crash_at_the_snapshot_write() {
        let _data_dir = DATA_DIR_LOCK.lock().unwrap();
        let temp = tempfile::tempdir().expect("tempdir");
        let data = temp.path().join("data");
        std::fs::create_dir_all(&data).expect("data dir");
        let cache = temp.path().join("cache");
        std::fs::create_dir_all(&cache).expect("cache dir");
        let db = cache.join("graph.sqlite3");
        drop(flavite_index::open_or_init(&db).expect("graph database"));

        // A real-content vault behind a fault-injected filesystem. The
        // `copy_mode` boundary is reached only by the durable write phase of
        // a save flight (staging the replacement), never by the background
        // recovery checkpoint writer, so the crash lands deterministically
        // inside the save.
        let memory = MemFs::new();
        memory
            .create_dir_all(Path::new("/vault"))
            .expect("vault root");
        memory
            .write_new(Path::new("/vault/a.md"), b"prior body\n")
            .expect("seed note");
        let script = FaultScript::new().fault_at(
            "copy_mode",
            1,
            FaultBoundary::Before,
            FaultKind::ProcessCrash,
        );
        let fault_fs = Arc::new(FaultFs::new_mem(memory, script));

        let identity = flavite_api::VaultIdentity::new(
            uuid::Uuid::now_v7(),
            PathBuf::from("/vault"),
            vec![7, 1, 2],
        );
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(SharedFaultFs(fault_fs.clone())) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(flavite_index::IndexActor::new(&db, 1).expect("index actor"))
            .build()
            .expect("actor")
            .start()
            .expect("actor start");
        let channels = ChannelSet::new();
        let mut config = VaultConfig::default();
        config.autosave_idle_interval = Duration::from_secs(3600);
        let session = Session::new_with_data_dir(
            identity.clone(),
            actor,
            flavite_buffer::manager::BufferManager::new(),
            channels.pulse,
            channels.notice,
            config.clone(),
            Some(data.clone()),
        );
        let mut flavite = Flavite::open(identity.clone(), session, config).expect("facade");
        flavite.set_graph_db_path_for_test(db.clone());

        let path = PathBuf::from("/vault/a.md");
        let (buffer_id, _) = open_note(&flavite, &path);
        let edited = "edited body\n";
        edit(&flavite, buffer_id, edited);

        // Retain the unsaved edits durably BEFORE the crash. The recovery
        // writer persists outside the fault-injected filesystem (the OS
        // data directory), so the retained copy cannot be corrupted by the
        // crash that follows.
        let receipt = success_data(
            command(
                &flavite,
                "recovery.flush",
                serde_json::json!({ "path": path }),
            ),
            "recovery.flush",
        );
        assert!(
            receipt["revision"].as_u64().is_some(),
            "the flush returns a revision-bound receipt: {receipt}"
        );

        // The save flight crashes at its durable write boundary.
        let save = command(
            &flavite,
            "note.save",
            serde_json::json!(path.to_string_lossy()),
        );
        let save_request = match save {
            ExecutionResult::Pending { request_id } => request_id,
            other => panic!("the save should be admitted: {other:?}"),
        };
        let finished = wait_for(&flavite, save_request);
        assert!(
            matches!(finished, ExecutionResult::Local(LocalResult::Error(_))),
            "the crashed save fails: {finished:?}"
        );
        assert!(
            !fault_fs.is_alive(),
            "the injected crash fired during the save: {backtrace}",
            backtrace = fault_fs
                .trace()
                .into_iter()
                .map(|entry| format!("{entry:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        );

        // Revive the crashed filesystem and reopen the vault with a fresh
        // session over the same identity (same recovery store).
        drop(flavite);
        let revived = Arc::new(fault_fs.revive());
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(SharedFaultFs(revived.clone())) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(flavite_index::IndexActor::new(&db, 1).expect("index actor"))
            .build()
            .expect("actor")
            .start()
            .expect("actor start");
        let channels = ChannelSet::new();
        let mut config = VaultConfig::default();
        config.autosave_idle_interval = Duration::from_secs(3600);
        let session = Session::new_with_data_dir(
            identity.clone(),
            actor,
            flavite_buffer::manager::BufferManager::new(),
            channels.pulse,
            channels.notice,
            config.clone(),
            Some(data),
        );
        let mut flavite = Flavite::open(identity, session, config).expect("facade reopens");
        flavite.set_graph_db_path_for_test(db);

        // The note file survived old-or-new complete (never partial).
        let reopened_path = PathBuf::from("/vault/a.md");
        let disk = revived
            .read_if_exists(&reopened_path)
            .expect("revived read")
            .expect("note survives the crash");
        assert!(
            disk == b"prior body\n" || disk == b"edited body\n",
            "the note file is old-or-new complete after the crash: {disk:?}"
        );

        // The retained recovery copy is intact and recovers the exact
        // pre-crash buffer content.
        open_note(&flavite, &reopened_path);
        let restored = success_data(
            command(
                &flavite,
                "recovery.restore",
                serde_json::json!({ "path": reopened_path }),
            ),
            "recovery.restore",
        );
        let restored_buffer = restored["buffer_id"].as_u64().expect("restored buffer id");
        assert_eq!(
            view(&flavite, restored_buffer).text,
            edited,
            "the retained copy survives the crash and restores the pre-crash content"
        );
    }
}

#[path = "support/editor_operations.rs"]
mod editor_operations_fixture;
