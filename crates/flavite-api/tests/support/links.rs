#![allow(dead_code)]
//! Shared real Session/parser/index fixtures for semantic and graph tests.
use flavite_api::{
    ExecutionResult, Flavite, Invocation, LocalResult, Origin, Preconditions, VaultConfig,
};
use flavite_buffer::manager::BufferManager;
use flavite_fs::{DirEntry, Fs, MemFs, Metadata, Result};
use flavite_index::IndexActor;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `Fs` view over a shared `MemFs` (integration fixture plumbing).
struct SharedFs(std::sync::Arc<MemFs>);

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

pub struct Fixture {
    pub flavite: Flavite,
    pub root: PathBuf,
    pub fs: std::sync::Arc<MemFs>,
    #[allow(dead_code)]
    pub cache: tempfile::TempDir,
    pub client: u128,
}

pub fn fixture(notes: &[(&str, &str)]) -> Fixture {
    let fs = std::sync::Arc::new(MemFs::new());
    let root = PathBuf::from("/test/semantic-links");
    fs.create_dir_all(&root).expect("vault root");
    for (name, body) in notes {
        if let Some(parent) = Path::new(name).parent()
            && !parent.as_os_str().is_empty()
        {
            fs.create_dir_all(&root.join(parent))
                .expect("note directory");
        }
        fs.write_new(&root.join(name), body.as_bytes())
            .expect("note");
    }
    let cache = tempfile::tempdir().expect("cache dir");
    let db = cache.path().join("semantic-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index"));
    let identity = flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![11]);
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
        VaultConfig {
            autosave_idle_interval: Duration::from_secs(3600),
            ..VaultConfig::default()
        },
    );
    let mut flavite = Flavite::open(identity, session, VaultConfig::default()).expect("facade");
    flavite.set_graph_db_path_for_test(db);
    reconcile(&flavite);
    let client = flavite.new_link_client();
    Fixture {
        flavite,
        root,
        fs,
        cache,
        client,
    }
}

pub fn reconcile(flavite: &Flavite) {
    let result = flavite.execute(Invocation::new(
        next_id(flavite),
        "vault.reconcile",
        serde_json::json!({ "scope": "full", "mode": "fast" }),
        Origin::Gui,
    ));
    if let ExecutionResult::Pending { request_id } = result {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            flavite.pump();
            let done = flavite.wait_for_operation(request_id);
            if !matches!(done, ExecutionResult::Pending { .. }) {
                assert!(done.is_success(), "reconcile: {done:?}");
                return;
            }
            assert!(Instant::now() < deadline, "reconcile never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

pub fn next_id(flavite: &Flavite) -> u64 {
    flavite.command_registry().next_request_id()
}

impl Fixture {
    pub fn open_note(&self, name: &str) -> (u64, u64) {
        let result = self.flavite.execute(Invocation::new(
            next_id(&self.flavite),
            "note.open",
            serde_json::json!({ "path": self.root.join(name), "policy": "existing_only" }),
            Origin::Gui,
        ));
        let request_id = match result {
            ExecutionResult::Pending { request_id } => request_id,
            other => panic!("open must be admitted: {other:?}"),
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

    pub fn text(&self, buffer_id: u64) -> String {
        self.flavite
            .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
            .expect("view")
            .text
    }

    pub fn saved_bytes(&self, name: &str) -> Vec<u8> {
        self.fs
            .read_if_exists(&self.root.join(name))
            .expect("read")
            .unwrap_or_default()
    }

    /// Prepare and wait for the plan. Returns (sequence, plan_id, serialized).
    pub fn prepare(
        &self,
        sequence: u64,
        buffer_id: u64,
        revision: u64,
        caret: usize,
        target_path: &str,
    ) -> (u64, u128, String) {
        let request_id = next_id(&self.flavite);
        let result = self.flavite.execute(Invocation::with_preconditions(
            request_id,
            "link.prepare",
            serde_json::json!({
                "intent": { "client": self.client, "sequence": sequence },
                "source": { "buffer_id": buffer_id, "revision": revision, "caret": caret },
                "target_path": target_path,
                "expected_target": self.flavite.link_target_identity(target_path).unwrap(),
                "stable_link_policy": "disabled",
            }),
            Origin::Gui,
            Preconditions::new()
                .with_buffer_id(buffer_id)
                .with_revision(revision)
                .requires_write(),
        ));
        let pending_id = match result {
            ExecutionResult::Pending { request_id } => request_id,
            other => panic!("prepare must be admitted: {other:?}"),
        };
        let completion = self.flavite.wait_for_operation(pending_id);
        let ExecutionResult::Local(LocalResult::Success(success)) = completion else {
            panic!("prepare did not succeed");
        };
        let plan_id = success.data["plan_id"]
            .as_str()
            .expect("plan id string")
            .parse::<u128>()
            .expect("plan id parses");
        let serialized = success.data["serialized_link"]
            .as_str()
            .expect("serialized link")
            .to_owned();
        (sequence, plan_id, serialized)
    }

    pub fn commit(&self, sequence: u64, plan_id: u128) -> ExecutionResult {
        self.flavite.execute(Invocation::with_preconditions(
            next_id(&self.flavite),
            "link.insert",
            serde_json::json!({
                "intent": { "client": self.client, "sequence": sequence },
                "plan_id": plan_id,
            }),
            Origin::Gui,
            Preconditions::new().requires_write(),
        ))
    }
}

pub fn error_code(result: &ExecutionResult) -> String {
    match result {
        ExecutionResult::Local(LocalResult::Error(error)) => error.code.clone(),
        other => panic!("expected a typed local error, got {other:?}"),
    }
}
