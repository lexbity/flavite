//! Host-boundary integration (RFC-0003 §19, W03).
//!
//! Real vault stack (MemFs + SQLite index + VaultActor + Session + facade):
//! typed buffer status and unconditional read-only enforcement, the notice
//! acknowledgement ledger under a flood with a hidden UI, and a stalled open
//! reader that must not block edits, local undo, or terminal save delivery
//! for unrelated buffers.

use flavite_api::{
    BufferStatus, ExecutionResult, Flavite, Invocation, LocalResult, Origin, Preconditions,
    RejectionReason, VaultConfig,
};
use flavite_buffer::manager::BufferManager;
use flavite_fs::{DirEntry, Fs, MemFs, Metadata, Result};
use flavite_index::IndexActor;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;

use flavite_vault::session::Session;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

const NOTICE_LEDGER_CAPACITY: usize = 256;

/// `MemFs` with a configurable delay on reads of one gated note, so a cold
/// open can be held in flight while other work proceeds.
struct GateFs {
    inner: MemFs,
    gated_path: PathBuf,
    read_delay_ms: AtomicU64,
}

impl GateFs {
    fn new(gated_path: PathBuf) -> Self {
        Self {
            inner: MemFs::new(),
            gated_path,
            read_delay_ms: AtomicU64::new(0),
        }
    }
}

impl Fs for GateFs {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        self.inner.canonicalize(path)
    }
    fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
        self.inner.resolve_symlink(path)
    }
    fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
        self.inner.encode_path_key(path)
    }
    fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
        self.inner.decode_path_key(key)
    }
    fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()> {
        self.inner.validate_in_root(root, target)
    }
    fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let delay = self.read_delay_ms.load(Ordering::SeqCst);
        if delay > 0 && path == self.gated_path {
            std::thread::sleep(Duration::from_millis(delay));
        }
        self.inner.read_if_exists(path)
    }
    fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
        self.inner.read_capped(path, max)
    }
    fn metadata(&self, path: &Path) -> Result<Metadata> {
        self.inner.metadata(path)
    }
    fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }
    fn exists(&self, path: &Path) -> Result<bool> {
        self.inner.exists(path)
    }
    fn is_placeholder(&self, path: &Path) -> Result<bool> {
        self.inner.is_placeholder(path)
    }
    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.inner.write_new(path, bytes)
    }
    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.copy_mode(from, to)
    }
    fn create_dir_all(&self, path: &Path) -> Result<()> {
        self.inner.create_dir_all(path)
    }
    fn remove(&self, path: &Path) -> Result<()> {
        self.inner.remove(path)
    }
    fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.rename_replace(from, to)
    }
    fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.rename_no_replace(from, to)
    }
    fn fsync_file(&self, path: &Path) -> Result<()> {
        self.inner.fsync_file(path)
    }
    fn fsync_dir(&self, path: &Path) -> Result<()> {
        self.inner.fsync_dir(path)
    }
}

struct Fixture {
    flavite: Flavite,
    root: PathBuf,
    #[allow(dead_code)]
    cache: tempfile::TempDir,
    fs: std::sync::Arc<GateFs>,
}

fn fixture(notes: &[(&str, &str)]) -> Fixture {
    let fs = std::sync::Arc::new(GateFs::new(PathBuf::from("/test/boundary/slow.md")));
    let root = PathBuf::from("/test/boundary");
    fs.create_dir_all(&root).expect("vault root");
    for (name, body) in notes {
        fs.write_new(&root.join(name), body.as_bytes())
            .expect("note");
    }
    let cache = tempfile::tempdir().expect("fixture cache directory");
    let db = cache.path().join("boundary-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index database"));
    let identity =
        flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![7, 2, 9]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(FixtureFs(fs.clone())) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("vault actor")
        .start()
        .expect("actor start");
    let channels = ChannelSet::new();
    let session = Session::new(
        identity.clone(),
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig::default(),
    );
    let flavite = Flavite::with_session(
        session,
        flavite_api::ApiVaultRef::new(identity.uuid, identity.root),
        VaultConfig::default(),
    )
    .expect("facade");
    Fixture {
        flavite,
        root,
        cache,
        fs,
    }
}

/// `Fs` for an shared `Arc<GateFs>`.
struct FixtureFs(std::sync::Arc<GateFs>);

impl Fs for FixtureFs {
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

impl Fixture {
    fn open_note(&self, name: &str) -> (u64, u64) {
        let invocation = Invocation::new(
            1,
            "note.open",
            serde_json::json!({ "path": self.root.join(name), "policy": "existing_only" }),
            Origin::Gui,
        );
        let result = self.flavite.execute(invocation);
        let request_id = match result {
            ExecutionResult::Pending { request_id } => request_id,
            other => panic!("expected pending open, got {other:?}"),
        };
        let completion = self.flavite.wait_for_operation(request_id);
        let ExecutionResult::Local(LocalResult::Success(success)) = completion else {
            panic!("open did not succeed");
        };
        let buffer_id = success.data["buffer_id"].as_u64().expect("buffer id");
        let view = self
            .flavite
            .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
            .expect("view");
        (buffer_id, view.revision)
    }
}

#[test]
fn typed_status_and_read_only_policy_reject_misleading_callers() {
    let mut fixture = fixture(&[("a.md", "hello")]);
    let (buffer_id, revision) = fixture.open_note("a.md");

    let view = fixture
        .flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
        .expect("view");
    assert!(matches!(view.state, BufferStatus::Clean));

    // The write policy is a vault property: a read-only vault rejects every
    // write even when the caller omits `requires_write` or spoofs other
    // preconditions. No caller field can opt into mutation (TD-17).
    let flavite = &mut fixture.flavite;
    flavite.set_read_only(true);
    let attempt = |request: u64, preconditions: Preconditions, args: serde_json::Value| {
        flavite.execute(Invocation::with_preconditions(
            request,
            "apply_edits",
            args,
            Origin::Gui,
            preconditions,
        ))
    };
    let edit_args = serde_json::json!({
        "buffer_id": buffer_id,
        "revision": revision,
        "ops": [{ "range": [0, 0], "replacement": "X" }]
    });
    // Correct, honest preconditions: still read-only.
    let result = attempt(
        2,
        Preconditions::new()
            .with_buffer_id(buffer_id)
            .with_revision(revision),
        edit_args.clone(),
    );
    assert!(
        matches!(&result, ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::ReadOnly),
        "read-only vault must reject writes: {result:?}"
    );
    // A caller that omits every precondition gets the same verdict.
    let result = attempt(3, Preconditions::default(), edit_args.clone());
    assert!(
        matches!(&result, ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::ReadOnly)
    );
    // Spoofed identity fields fail earlier with typed reasons, never mutate.
    let result = attempt(
        4,
        Preconditions::new()
            .with_buffer_id(buffer_id)
            .with_revision(revision + 100),
        edit_args.clone(),
    );
    assert!(
        matches!(&result, ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::StaleRevision)
    );
    let result = attempt(
        5,
        Preconditions::new()
            .with_buffer_id(buffer_id + 999)
            .with_revision(revision),
        edit_args,
    );
    assert!(
        matches!(&result, ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::BufferNotFound)
    );

    // Nothing was mutated: the typed view still reports the original text.
    let view = fixture
        .flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
        .expect("view");
    assert_eq!(view.text, "hello");
    assert!(matches!(view.state, BufferStatus::Clean));
}

#[test]
fn notice_flood_observes_every_id_without_auto_ack_and_saves_still_finish() {
    let fixture = fixture(&[("a.md", "hello")]);
    let (buffer_id, revision) = fixture.open_note("a.md");

    // Flood past the ledger capacity while the "UI" is hidden: polling with
    // zero credits delivers nothing and nothing is auto-acknowledged.
    for index in 0..(NOTICE_LEDGER_CAPACITY + 5) {
        fixture
            .flavite
            .inject_notice_for_test(Box::leak(format!("flood-{index}").into_boxed_str()));
    }
    assert!(fixture.flavite.poll_events(0).is_empty());
    assert_eq!(
        fixture.flavite.outstanding_notice_count(),
        0,
        "hidden UI must not receive or lose notices"
    );

    // Present incrementally with bounded credits, acknowledging only after
    // each entry is visibly presented; every distinct notice ID is observed.
    let mut observed = Vec::new();
    while observed.len() < NOTICE_LEDGER_CAPACITY + 5 {
        let credits = 10;
        let deliveries = fixture.flavite.poll_events(credits);
        assert!(
            !deliveries.is_empty(),
            "credit-based polling must make progress"
        );
        for delivery in deliveries {
            let Some(id) = delivery.notice_id else {
                panic!("injected notices must carry notice ids");
            };
            observed.push(id);
            assert!(fixture.flavite.acknowledge_notice(id));
        }
    }
    let unique = observed.iter().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(unique.len(), observed.len(), "no duplicate notice ids");
    assert_eq!(observed.len(), NOTICE_LEDGER_CAPACITY + 5);
    assert_eq!(fixture.flavite.outstanding_notice_count(), 0);

    // Accepted saves still finish while the ledger is exercised.
    let invocation = Invocation::new(
        3,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "ops": [{ "range": [5, 5], "replacement": " there" }]
        }),
        Origin::Gui,
    );
    assert!(fixture.flavite.execute(invocation).is_success());
    let save = Invocation::new(
        4,
        "note.save",
        serde_json::json!(fixture.root.join("a.md")),
        Origin::Gui,
    );
    let result = fixture.flavite.execute(save);
    let ExecutionResult::Pending { request_id } = result else {
        panic!("save must be admitted: {result:?}");
    };
    let completion = fixture.flavite.wait_for_operation(request_id);
    assert!(
        completion.is_success(),
        "accepted save must finish: {completion:?}"
    );
    let saved = fixture
        .fs
        .read_if_exists(&fixture.root.join("a.md"))
        .expect("read")
        .unwrap();
    assert_eq!(String::from_utf8(saved).expect("utf8"), "hello there");
}

#[test]
fn stalled_open_reader_does_not_block_unrelated_edits_undo_and_saves() {
    let fixture = fixture(&[("a.md", "note a"), ("slow.md", "slow note")]);
    let (buffer_a, revision_a) = fixture.open_note("a.md");

    // Hold the cold open of the slow note in flight on its read worker.
    fixture.fs.read_delay_ms.store(3_000, Ordering::SeqCst);
    let slow_open = Invocation::new(
        5,
        "note.open",
        serde_json::json!({ "path": fixture.root.join("slow.md"), "policy": "existing_only" }),
        Origin::Gui,
    );
    let slow_request = match fixture.flavite.execute(slow_open) {
        ExecutionResult::Pending { request_id } => request_id,
        other => panic!("slow open must be admitted: {other:?}"),
    };

    // Edits, local undo, and terminal save delivery for the unrelated
    // buffer remain serviceable while the reader is stalled.
    let edit = Invocation::new(
        6,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_a,
            "revision": revision_a,
            "ops": [{ "range": [5, 6], "replacement": "A" }]
        }),
        Origin::Gui,
    );
    let edit_result = fixture.flavite.execute(edit);
    assert!(
        edit_result.is_success(),
        "edit must succeed: {edit_result:?}"
    );
    let undo = Invocation::new(
        7,
        "undo",
        serde_json::json!({ "buffer_id": buffer_a, "revision": revision_a + 1 }),
        Origin::Gui,
    );
    assert!(fixture.flavite.execute(undo).is_success());
    let view = fixture
        .flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_a), None)
        .expect("view");
    assert_eq!(view.text, "note a", "local undo reverted the edit");

    let redo = Invocation::new(
        8,
        "redo",
        serde_json::json!({ "buffer_id": buffer_a, "revision": revision_a + 2 }),
        Origin::Gui,
    );
    assert!(fixture.flavite.execute(redo).is_success());
    let save = Invocation::new(
        9,
        "note.save",
        serde_json::json!(fixture.root.join("a.md")),
        Origin::Gui,
    );
    let ExecutionResult::Pending { request_id } = fixture.flavite.execute(save) else {
        panic!("save must be admitted while a reader is stalled");
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        fixture.flavite.pump();
        let completion = fixture.flavite.wait_for_operation(request_id);
        if !matches!(completion, ExecutionResult::Pending { .. }) {
            assert!(completion.is_success(), "save must finish: {completion:?}");
            break;
        }
        assert!(Instant::now() < deadline, "save never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    let saved = fixture
        .fs
        .read_if_exists(&fixture.root.join("a.md"))
        .expect("read")
        .unwrap();
    assert_eq!(String::from_utf8(saved).expect("utf8"), "note A");

    // The stalled reader never blocked core progress; releasing it lets the
    // cold open install normally.
    fixture.fs.read_delay_ms.store(0, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        fixture.flavite.pump();
        let completion = fixture.flavite.wait_for_operation(slow_request);
        if !matches!(completion, ExecutionResult::Pending { .. }) {
            assert!(
                completion.is_success(),
                "slow open must finish: {completion:?}"
            );
            break;
        }
        assert!(Instant::now() < deadline, "slow open never finished");
        std::thread::sleep(Duration::from_millis(10));
    }
}
