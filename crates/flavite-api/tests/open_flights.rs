//! Cold-open flight integration (RFC-0003 W02, §8.3).
//!
//! Delays actor reads through a slow `Fs` wrapper, admits two same-path
//! opens plus an edit on an already-open different note, and asserts:
//! nonblocking edit completion, one installed `BufferId` for coalesced
//! callers, per-caller terminal results, cancelled-view isolation, and
//! ExistingOnly missing-note behavior.

use flavite_api::{ExecutionResult, Flavite, Invocation, LocalResult, Origin, VaultConfig};
use flavite_buffer::manager::BufferManager;
use flavite_fs::{DirEntry, Fs, MemFs, Metadata, Result};
use flavite_index::IndexActor;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// `MemFs` with a configurable delay and per-path read counting on
/// `read_if_exists`, so tests can hold cold opens in flight.
struct SlowFs {
    inner: MemFs,
    read_delay_ms: AtomicU64,
    reads_of_b: AtomicUsize,
    path_b: PathBuf,
}

impl SlowFs {
    fn new(path_b: PathBuf) -> Self {
        Self {
            inner: MemFs::new(),
            read_delay_ms: AtomicU64::new(0),
            reads_of_b: AtomicUsize::new(0),
            path_b,
        }
    }
}

impl Fs for SlowFs {
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
        if path == self.path_b {
            self.reads_of_b.fetch_add(1, Ordering::SeqCst);
            let delay = self.read_delay_ms.load(Ordering::SeqCst);
            if delay > 0 {
                std::thread::sleep(Duration::from_millis(delay));
            }
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
    path_a: PathBuf,
    path_b: PathBuf,
    #[allow(dead_code)]
    cache: tempfile::TempDir,
}

fn fixture() -> (Fixture, std::sync::Arc<SlowFs>) {
    let fs = std::sync::Arc::new(SlowFs::new(PathBuf::from("/test/flights/b.md")));
    let root = PathBuf::from("/test/flights");
    fs.create_dir_all(&root).expect("vault root");
    fs.write_new(&root.join("a.md"), b"note a").expect("note a");
    fs.write_new(&root.join("b.md"), b"note b").expect("note b");
    let cache = tempfile::tempdir().expect("fixture cache directory");
    let db = cache.path().join("flights-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index database"));
    let identity =
        flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![3, 1, 4]);
    struct ArcFs(std::sync::Arc<SlowFs>);
    impl Fs for ArcFs {
        fn canonicalize(&self, p: &Path) -> Result<PathBuf> {
            self.0.canonicalize(p)
        }
        fn resolve_symlink(&self, p: &Path) -> Result<PathBuf> {
            self.0.resolve_symlink(p)
        }
        fn encode_path_key(&self, p: &Path) -> Result<Vec<u8>> {
            self.0.encode_path_key(p)
        }
        fn decode_path_key(&self, k: &[u8]) -> Result<PathBuf> {
            self.0.decode_path_key(k)
        }
        fn validate_in_root(&self, r: &Path, t: &Path) -> Result<()> {
            self.0.validate_in_root(r, t)
        }
        fn read_if_exists(&self, p: &Path) -> Result<Option<Vec<u8>>> {
            self.0.read_if_exists(p)
        }
        fn read_capped(&self, p: &Path, m: u64) -> Result<Vec<u8>> {
            self.0.read_capped(p, m)
        }
        fn metadata(&self, p: &Path) -> Result<Metadata> {
            self.0.metadata(p)
        }
        fn read_dir(&self, p: &Path) -> Result<Vec<DirEntry>> {
            self.0.read_dir(p)
        }
        fn exists(&self, p: &Path) -> Result<bool> {
            self.0.exists(p)
        }
        fn is_placeholder(&self, p: &Path) -> Result<bool> {
            self.0.is_placeholder(p)
        }
        fn write_new(&self, p: &Path, b: &[u8]) -> Result<()> {
            self.0.write_new(p, b)
        }
        fn copy_mode(&self, f: &Path, t: &Path) -> Result<()> {
            self.0.copy_mode(f, t)
        }
        fn create_dir_all(&self, p: &Path) -> Result<()> {
            self.0.create_dir_all(p)
        }
        fn remove(&self, p: &Path) -> Result<()> {
            self.0.remove(p)
        }
        fn rename_replace(&self, f: &Path, t: &Path) -> Result<()> {
            self.0.rename_replace(f, t)
        }
        fn rename_no_replace(&self, f: &Path, t: &Path) -> Result<()> {
            self.0.rename_no_replace(f, t)
        }
        fn fsync_file(&self, p: &Path) -> Result<()> {
            self.0.fsync_file(p)
        }
        fn fsync_dir(&self, p: &Path) -> Result<()> {
            self.0.fsync_dir(p)
        }
    }
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(ArcFs(fs.clone())) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
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
    let flavite = Flavite::open(identity, session, VaultConfig::default()).expect("facade");
    let fixture = Fixture {
        flavite,
        path_a: root.join("a.md"),
        path_b: root.join("b.md"),
        root,
        cache,
    };
    (fixture, fs)
}

fn open_via_api(flavite: &Flavite, path: &Path, policy: Option<&str>) -> ExecutionResult {
    let mut args = serde_json::json!({ "path": path });
    if let Some(policy) = policy {
        args["policy"] = serde_json::json!(policy);
    }
    let request_id = flavite.command_registry().next_request_id();
    flavite.execute(Invocation::new(request_id, "note.open", args, Origin::Gui))
}

/// Pump completions until `wanted` terminal results arrive or the deadline
/// passes, returning every drained completion in arrival order.
fn drain_completions(
    flavite: &Flavite,
    wanted: usize,
    deadline: Instant,
) -> Vec<(u64, ExecutionResult)> {
    let mut drained = Vec::new();
    while drained.len() < wanted && Instant::now() < deadline {
        flavite.pump();
        for completion in flavite.drain_operation_completions() {
            drained.push((completion.request_id, completion.result));
        }
        if drained.len() < wanted {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    drained
}

#[test]
fn same_path_opens_coalesce_into_one_buffer_with_per_caller_results() {
    let (fixture, fs) = fixture();
    let flavite = &fixture.flavite;
    // Note A opens before any delay so its identity is installed up front.
    let direct = open_via_api(flavite, &fixture.path_a, None);
    let open_a = match direct {
        ExecutionResult::Pending { request_id } => flavite
            .wait_for_operation(request_id)
            .into_success()
            .expect("note a opens"),
        ExecutionResult::Local(LocalResult::Success(success)) => success,
        other => panic!("note a open failed: {other:?}"),
    };
    let buffer_a = open_a.data["buffer_id"].as_u64().expect("buffer id");
    // wait_for_operation already consumed the one completion for note A.
    let completions_a = flavite.drain_operation_completions();
    assert!(
        completions_a.is_empty(),
        "note a's completion is consumed exactly once"
    );
    fs.read_delay_ms.store(250, Ordering::SeqCst);

    // Admit two same-path cold opens: one flight, two request ids.
    let first = open_via_api(flavite, &fixture.path_b, None);
    let second = open_via_api(flavite, &fixture.path_b, None);
    let first_request = match &first {
        ExecutionResult::Pending { request_id } => *request_id,
        other => panic!("first open should be pending, got {other:?}"),
    };
    let second_request = match &second {
        ExecutionResult::Pending { request_id } => *request_id,
        other => panic!("second open should be pending, got {other:?}"),
    };
    assert_ne!(first_request, second_request, "each caller keeps its id");

    // Open the same note again: immediate identity, no flight.
    let direct = open_via_api(flavite, &fixture.path_a, None);
    let open_again = match direct {
        ExecutionResult::Local(LocalResult::Success(success)) => success,
        other => panic!("already-open identity must return immediately, got {other:?}"),
    };
    assert_eq!(open_again.data["buffer_id"].as_u64(), Some(buffer_a));

    // While the flight is in-flight, an unrelated command must not block.
    let edit_request = flavite.command_registry().next_request_id();
    let view = flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_a), None)
        .expect("buffer view");
    let edit = flavite.execute(Invocation::with_preconditions(
        edit_request,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_a,
            "revision": view.revision,
            "ops": [{ "range": [6, 6], "replacement": " edited" }],
        }),
        Origin::Gui,
        flavite_api::Preconditions::new()
            .with_buffer_id(buffer_a)
            .with_revision(view.revision),
    ));
    assert!(
        edit.is_success(),
        "an edit on an open note must complete while a cold open is in flight: {edit:?}"
    );

    // Exactly one read of the coalesced path.
    let completions = drain_completions(flavite, 2, Instant::now() + Duration::from_secs(5));
    assert_eq!(
        completions.len(),
        2,
        "both callers receive their own terminal result"
    );
    let request_ids: std::collections::BTreeSet<u64> =
        completions.iter().map(|(id, _)| *id).collect();
    assert!(request_ids.contains(&first_request));
    assert!(request_ids.contains(&second_request));
    let mut opened_buffers = std::collections::BTreeSet::new();
    for (_, result) in completions {
        let success = result.into_success().expect("coalesced open completes");
        opened_buffers.insert(success.data["buffer_id"].as_u64().expect("buffer id"));
    }
    assert_eq!(
        opened_buffers.len(),
        1,
        "coalesced callers share one installed BufferId"
    );
    assert_eq!(
        fs.reads_of_b.load(Ordering::SeqCst),
        1,
        "one flight performs exactly one read"
    );
    let view = flavite
        .get_buffer_view(
            flavite_buffer::BufferId::from(*opened_buffers.first().expect("one")),
            None,
        )
        .expect("installed view");
    assert_eq!(view.path, fixture.path_b);
    assert_eq!(view.text, "note b");

    // A later open returns the installed identity immediately.
    let again = open_via_api(flavite, &fixture.path_b, None);
    assert!(
        again.is_success(),
        "the installed buffer opens without a flight"
    );
}

#[test]
fn cancelled_view_isolation_and_existing_only_missing_notes() {
    let (fixture, fs) = fixture();
    let flavite = &fixture.flavite;
    fs.read_delay_ms.store(120, Ordering::SeqCst);

    // "Cancelled" caller: its view goes away after admission. The flight
    // stays lossless and unrelated callers are unaffected.
    let cancelled = open_via_api(flavite, &fixture.path_b, None);
    let cancelled_request = match cancelled {
        ExecutionResult::Pending { request_id } => request_id,
        other => panic!("expected pending, got {other:?}"),
    };
    drop(cancelled);

    // CreateIfMissing keeps the established absent-file creation guard.
    let created = open_via_api(flavite, &fixture.root.join("fresh.md"), None);
    let fresh_request = match created {
        ExecutionResult::Pending { request_id } => request_id,
        other => panic!("expected pending create, got {other:?}"),
    };
    // ExistingOnly for a missing note must complete as a real failure
    // without creating a buffer.
    let missing = fixture.root.join("missing.md");
    let existing_only = open_via_api(flavite, &missing, Some("existing_only"));
    let missing_request = match existing_only {
        ExecutionResult::Pending { request_id } => request_id,
        ExecutionResult::Local(_) => panic!("existing_only cold open must take the flight"),
        other => panic!("unexpected existing_only result: {other:?}"),
    };
    let drained = drain_completions(flavite, 3, Instant::now() + Duration::from_secs(5));
    let results: std::collections::HashMap<u64, ExecutionResult> = drained.into_iter().collect();
    let cancelled_result = results
        .get(&cancelled_request)
        .expect("cancelled view's flight stays lossless");
    assert!(
        cancelled_result.is_success(),
        "the cancelled view's note still opens"
    );
    let missing_result = results
        .get(&missing_request)
        .cloned()
        .expect("existing_only completion arrives");
    let missing_error = missing_result
        .into_error()
        .expect("ExistingOnly missing note fails");
    assert!(
        missing_error.message.to_lowercase().contains("not found"),
        "missing note failure names the cause: {}",
        missing_error.message
    );
    assert!(
        !flavite.is_note_open(&missing),
        "no buffer is created for a missing ExistingOnly note"
    );
    assert!(
        results
            .get(&fresh_request)
            .is_some_and(|result| result.is_success()),
        "CreateIfMissing installs an empty buffer for an absent note"
    );
    assert!(
        flavite.is_note_open(&fixture.root.join("fresh.md")),
        "the created note is open"
    );
}

#[test]
fn open_flights_admission_is_bounded() {
    let (fixture, _fs) = fixture();
    let flavite = &fixture.flavite;
    // 128 pending opens are supported; 129 distinct paths exceed the cap
    // and admission reports Busy without losing an admitted flight.
    let mut admitted = 0;
    for index in 0..130 {
        let path = fixture.root.join(format!("bulk{index}.md"));
        match open_via_api(flavite, &path, None) {
            ExecutionResult::Pending { .. } => admitted += 1,
            ExecutionResult::Rejected(rejected)
                if rejected.reason == flavite_api::RejectionReason::Busy =>
            {
                assert_eq!(admitted, 128, "admission stops exactly at the bound");
                return;
            }
            other => panic!("unexpected bulk open result: {other:?}"),
        }
    }
    panic!("the 129th open should have been refused as Busy");
}
