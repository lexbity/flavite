//! Shared test support (RFC-0003 W01 `tests/support/`): one MemFs-backed
//! vault fixture builder and one Fs-delegating wrapper, used by every UI
//! integration test instead of per-file copies.

#![allow(dead_code)]

use flavite_fs::{DirEntry, Fs, MemFs, Metadata, Result};
use std::path::{Path, PathBuf};

/// `Fs` view over a shared `MemFs` (fixture plumbing for actor builders).
pub struct SharedFs(pub std::sync::Arc<MemFs>);

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

/// One note to seed into the fixture vault.
pub struct SeedNote {
    /// Vault-relative path components.
    pub components: &'static [&'static str],
    /// Note body.
    pub body: &'static str,
}

/// A fully assembled vault stack: real Session/index, MemFs contents, and
/// the attached UI [`App`].
pub struct UiFixture {
    pub app: flavite_ui::app::App,
    pub fs: std::sync::Arc<MemFs>,
    #[allow(dead_code)]
    pub cache: tempfile::TempDir,
}

/// Build the standard fixture: MemFs vault with `notes`, a SQLite index in
/// a tempdir, a real VaultActor/Session/facade, and the attached App with
/// device-local user state pointed at a disposable root.
///
/// # Panics
/// Panics on fixture plumbing failures (they are test defects, not
/// behaviors under test).
#[must_use]
pub fn ui_fixture(seed: u64, root: &str, notes: &[SeedNote]) -> UiFixture {
    let identity = flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), PathBuf::from(root), {
        let mut key = vec![seed as u8; 3];
        key[0] = (seed % 251) as u8;
        key
    });
    ui_fixture_with_identity(seed, identity, notes)
}

/// Build the fixture bound to a caller-chosen vault identity (restart
/// tests need the SAME identity so persisted vault keys match).
#[must_use]
pub fn ui_fixture_with_identity(
    seed: u64,
    identity: flavite_api::VaultIdentity,
    notes: &[SeedNote],
) -> UiFixture {
    use flavite_api::VaultConfig;
    use flavite_buffer::manager::BufferManager;
    use flavite_index::IndexActor;
    use flavite_ui::app::App;
    use flavite_vault::actor::VaultActorBuilder;
    use flavite_vault::channels::ChannelSet;
    use flavite_vault::session::Session;

    let fs = std::sync::Arc::new(MemFs::new());
    let root_path = identity.root.clone();
    fs.create_dir_all(&root_path).expect("vault root");
    for note in notes {
        let path = root_path.join(note.components.iter().collect::<PathBuf>());
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent).expect("note directory");
        }
        fs.write_new(&path, note.body.as_bytes()).expect("note");
    }
    let cache = tempfile::tempdir().expect("cache dir");
    let db = cache.path().join("fixture-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index"));
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(SharedFs(fs.clone())) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
        .start()
        .expect("start");
    let channels = ChannelSet::new();
    let _session = Session::new(
        identity.clone(),
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig::default(),
    );
    let (flavite, fs, cache) = vault_stack(seed, identity, notes);
    UiFixture {
        app: App::from_flavite(flavite),
        fs,
        cache,
    }
}

/// Build the vault stack WITHOUT attaching the UI app: tests that must seed
/// device-local state before attach (production ordering) use this.
///
/// # Panics
/// Panics on fixture plumbing failures.
#[must_use]
pub fn vault_stack(
    seed: u64,
    identity: flavite_api::VaultIdentity,
    notes: &[SeedNote],
) -> (
    flavite_api::Flavite,
    std::sync::Arc<MemFs>,
    tempfile::TempDir,
) {
    use flavite_api::{Flavite, VaultConfig};
    use flavite_buffer::manager::BufferManager;
    use flavite_index::IndexActor;
    use flavite_vault::actor::VaultActorBuilder;
    use flavite_vault::channels::ChannelSet;
    use flavite_vault::session::Session;

    let fs = std::sync::Arc::new(MemFs::new());
    let root_path = identity.root.clone();
    fs.create_dir_all(&root_path).expect("vault root");
    for note in notes {
        let path = root_path.join(note.components.iter().collect::<PathBuf>());
        if let Some(parent) = path.parent() {
            fs.create_dir_all(parent).expect("note directory");
        }
        fs.write_new(&path, note.body.as_bytes()).expect("note");
    }
    let cache = tempfile::tempdir().expect("cache dir");
    let db = cache.path().join("fixture-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index"));
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
    let mut flavite = Flavite::open(identity, session, VaultConfig::default()).expect("facade");
    flavite.set_user_state_root_for_test(PathBuf::from(format!("/test/ui-fixture-device/{seed}")));
    (flavite, fs, cache)
}

/// Reconcile a pre-attach vault stack, waiting for the async completion.
///
/// # Panics
/// Panics if the reconcile is rejected or times out (fixture plumbing).
pub fn reconcile(flavite: &flavite_api::Flavite) {
    use flavite_api::{ExecutionResult, Invocation, Origin};
    use std::time::{Duration, Instant};
    let result = flavite.execute(Invocation::new(
        flavite.command_registry().next_request_id(),
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
            assert!(done.is_success(), "reconcile: {done:?}");
            return;
        }
        assert!(Instant::now() < deadline, "reconcile never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
}
