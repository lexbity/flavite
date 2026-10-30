//! Query service integration (RFC-0003 §20, W04).
//!
//! Saturating the 64 slots yields typed Busy (never silent loss), an
//! accepted core save still completes while the service is saturated, and
//! a closed requesting tab's stale tokens are simply drained and dropped by
//! the host — cancellation never revives a view.

use flavite_api::{
    ContextQueryResult, ExecutionResult, Flavite, Invocation, MAX_QUERY_SLOTS, Origin,
    QueryPayload, VaultConfig,
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

/// `MemFs` with a per-call delay on reads of one gated note, so query
/// workers stay busy while the test submits.
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

/// Fs for a shared Arc<GateFs>.
struct SharedFs(std::sync::Arc<GateFs>);

impl Fs for SharedFs {
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

fn fixture() -> (Flavite, PathBuf, std::sync::Arc<GateFs>, tempfile::TempDir) {
    let fs = std::sync::Arc::new(GateFs::new(PathBuf::from("/test/queries/linked.md")));
    let root = PathBuf::from("/test/queries");
    fs.create_dir_all(&root).expect("vault root");
    fs.write_new(&root.join("linked.md"), b"See [[target]].\n")
        .expect("note");
    fs.write_new(&root.join("target.md"), b"Target.\n")
        .expect("note");
    let cache = tempfile::tempdir().expect("cache dir");
    let db = cache.path().join("query-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index"));
    let identity = flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![9]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(SharedFs(fs.clone())) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
        .start()
        .expect("start");
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
    (flavite, root, fs, cache)
}

fn reconcile(flavite: &Flavite) {
    let result = flavite.execute(Invocation::new(
        1,
        "vault.reconcile",
        serde_json::json!({ "scope": "full", "mode": "fast" }),
        Origin::Gui,
    ));
    let ExecutionResult::Pending { request_id } = result else {
        return;
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        flavite.pump();
        let done = flavite.wait_for_operation(request_id);
        if !matches!(done, ExecutionResult::Pending { .. }) {
            assert!(done.is_success(), "reconcile finished: {done:?}");
            return;
        }
        assert!(Instant::now() < deadline, "reconcile never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn saturated_slots_return_busy_and_saves_still_finish() {
    let (flavite, root, fs, _cache) = fixture();
    reconcile(&flavite);

    // Hold every worker busy on gated reads of the linking note.
    fs.read_delay_ms.store(50, Ordering::SeqCst);
    let payload = QueryPayload::Backlinks {
        path: vec!["target.md".to_owned()],
    };
    let mut tokens = Vec::new();
    // Distinct requesting views: every slot's result must be served.
    for slot in 0..MAX_QUERY_SLOTS as u128 {
        match flavite.query_service().submit(slot, 1, 1, payload.clone()) {
            Ok(token) => tokens.push(token),
            Err(error) => panic!("slots must admit up to capacity: {error:?}"),
        }
    }
    assert!(
        flavite
            .query_service()
            .submit(999, 1, 1, payload.clone())
            .is_err(),
        "slot 65 must be typed-Busy, never queued"
    );

    // An accepted core save completes while the service is saturated.
    let open = flavite.execute(Invocation::new(
        2,
        "note.open",
        serde_json::json!({ "path": root.join("target.md"), "policy": "existing_only" }),
        Origin::Gui,
    ));
    let request_id = match open {
        ExecutionResult::Pending { request_id } => request_id,
        other => panic!("open must be admitted: {other:?}"),
    };
    let completion = flavite.wait_for_operation(request_id);
    assert!(completion.is_success(), "open finished while saturated");

    // Release the gate; every admitted query is served exactly once.
    fs.read_delay_ms.store(0, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut seen = 0;
    while seen < MAX_QUERY_SLOTS {
        assert!(Instant::now() < deadline, "queries never finished");
        let outcomes = flavite.query_service().drain();
        seen += outcomes.len();
        for outcome in outcomes {
            assert_eq!(outcome.token.session_nonce, tokens[0].session_nonce);
            assert!(
                matches!(outcome.result, ContextQueryResult::Backlinks { .. }),
                "backlink queries serve typed results"
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(flavite.query_service().in_flight(), 0);
}

#[test]
fn closed_request_tokens_are_drained_and_dropped_without_revival() {
    let (flavite, _root, _fs, _cache) = fixture();
    reconcile(&flavite);

    // A "tab" submits a preview, then closes: its tokens are drained as
    // plain data. Nothing re-submits or revives a view; the service keeps
    // no per-tab subscriptions.
    let served = |flavite: &Flavite| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while flavite.query_service().in_flight() > 0 {
            assert!(Instant::now() < deadline, "query never served");
            std::thread::sleep(Duration::from_millis(5));
        }
        flavite.query_service().drain()
    };
    let token = flavite
        .query_service()
        .submit(
            77,
            1,
            1,
            QueryPayload::Preview {
                path: vec!["target.md".to_owned()],
            },
        )
        .expect("admitted");
    let outcomes = served(&flavite);
    assert_eq!(outcomes.len(), 1);
    assert_eq!(outcomes[0].token, token);
    match &outcomes[0].result {
        ContextQueryResult::Preview { text, path, .. } => {
            assert_eq!(path, &vec!["target.md".to_owned()]);
            assert!(text.contains("Target"), "saved text is served: {text:?}");
        }
        other => panic!("preview served a typed result, got {other:?}"),
    }
    // The same view identity re-submitting with a newer request identity
    // supersedes: after both are served, only the newest token remains.
    let first = flavite
        .query_service()
        .submit(
            77,
            1,
            2,
            QueryPayload::Preview {
                path: vec!["target.md".to_owned()],
            },
        )
        .expect("admitted");
    let second = flavite
        .query_service()
        .submit(
            77,
            1,
            3,
            QueryPayload::Preview {
                path: vec!["linked.md".to_owned()],
            },
        )
        .expect("admitted");
    // Both queries settle fully before the drain, so the supersede rule is
    // observed deterministically: the map held only the newest token.
    let remaining = served(&flavite);
    let tokens: Vec<_> = remaining.iter().map(|outcome| outcome.token).collect();
    assert!(
        tokens.contains(&second),
        "newest result is served: {tokens:?}"
    );
    assert!(
        !tokens.contains(&first),
        "a late older sequence cannot overwrite or coexist with n+1"
    );
    assert_eq!(
        remaining.len(),
        1,
        "only the newest token for the view survives"
    );
}
