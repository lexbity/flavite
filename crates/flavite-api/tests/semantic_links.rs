//! Semantic link insertion end-to-end (RFC-0003 §9/§21, W05).
//!
//! Real Session/temporary vault/parser/resolver: exact source bytes, target
//! unchanged, single Undo/Redo, same-title notes, native paths, duplicate
//! intents, stale sources, index changes, capability and read-only policy,
//! and the raw-shape negative test. AC-W04/05 zero-mutation outcomes are
//! asserted after every rejected transition.

use flavite_api::{
    API_VERSION, ExecutionResult, Flavite, Invocation, LocalResult, Origin, Preconditions,
    RejectionReason, VaultConfig,
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

struct Fixture {
    flavite: Flavite,
    root: PathBuf,
    fs: std::sync::Arc<MemFs>,
    #[allow(dead_code)]
    cache: tempfile::TempDir,
    client: u128,
}

fn fixture(notes: &[(&str, &str)]) -> Fixture {
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
        VaultConfig::default(),
    );
    let flavite = Flavite::open(identity, session, VaultConfig::default()).expect("facade");
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

fn reconcile(flavite: &Flavite) {
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

fn next_id(flavite: &Flavite) -> u64 {
    flavite.command_registry().next_request_id()
}

impl Fixture {
    fn open_note(&self, name: &str) -> (u64, u64) {
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

    fn text(&self, buffer_id: u64) -> String {
        self.flavite
            .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
            .expect("view")
            .text
    }

    fn saved_bytes(&self, name: &str) -> Vec<u8> {
        self.fs
            .read_if_exists(&self.root.join(name))
            .expect("read")
            .unwrap_or_default()
    }

    /// Prepare and wait for the plan. Returns (sequence, plan_id, serialized).
    fn prepare(
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

    fn commit(&self, sequence: u64, plan_id: u128) -> ExecutionResult {
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

fn error_code(result: &ExecutionResult) -> String {
    match result {
        ExecutionResult::Local(LocalResult::Error(error)) => error.code.clone(),
        other => panic!("expected a typed local error, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn prepare_commit_undo_redo_and_target_immutability() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let target_before = fixture.saved_bytes("target.md");

    let (sequence, plan_id, serialized) = fixture.prepare(1, buffer_id, revision, 6, "target.md");
    assert_eq!(serialized, "[[target]]");

    let result = fixture.commit(sequence, plan_id);
    let ExecutionResult::Local(LocalResult::Success(success)) = &result else {
        panic!("commit must succeed: {result:?}");
    };
    let new_revision = success.data["revision"].as_u64().expect("new revision");
    assert_eq!(
        fixture.text(buffer_id),
        "Hello [[target]]world.\n",
        "exactly one occurrence at the caret"
    );
    assert_eq!(
        fixture.saved_bytes("target.md"),
        target_before,
        "the target note is unchanged by insertion"
    );

    // One undo removes it; redo restores it.
    let undo = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "undo",
        serde_json::json!({ "buffer_id": buffer_id, "revision": new_revision }),
        Origin::Gui,
    ));
    assert!(undo.is_success(), "undo: {undo:?}");
    assert_eq!(fixture.text(buffer_id), "Hello world.\n");
    let view = fixture
        .flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
        .expect("view");
    let redo = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "redo",
        serde_json::json!({ "buffer_id": buffer_id, "revision": view.revision }),
        Origin::Gui,
    ));
    assert!(redo.is_success(), "redo: {redo:?}");
    assert_eq!(fixture.text(buffer_id), "Hello [[target]]world.\n");
}

#[test]
fn same_title_notes_need_explicit_paths_and_stems_are_ambiguous() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("Systems/Emergence.md", "systems emergence\n"),
        ("Ideas/Emergence.md", "ideas emergence\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");

    // The exact path resolves unambiguously.
    let (sequence, plan_id, serialized) =
        fixture.prepare(1, buffer_id, revision, 6, "Systems/Emergence.md");
    assert_eq!(serialized, "[[Systems/Emergence]]");
    assert!(fixture.commit(sequence, plan_id).is_success());

    // The bare stem is refused: the app never guesses between same titles.
    let request_id = next_id(&fixture.flavite);
    let result = fixture.flavite.execute(Invocation::with_preconditions(
        request_id,
        "link.prepare",
        serde_json::json!({
            "intent": { "client": fixture.client, "sequence": 2 },
            "source": { "buffer_id": buffer_id, "revision": revision + 1, "caret": 6 },
            "target_path": "Emergence",
            "stable_link_policy": "disabled",
        }),
        Origin::Gui,
        Preconditions::new()
            .with_buffer_id(buffer_id)
            .with_revision(revision + 1)
            .requires_write(),
    ));
    let pending = match result {
        ExecutionResult::Pending { request_id } => request_id,
        other => panic!("prepare must be admitted: {other:?}"),
    };
    let completion = fixture.flavite.wait_for_operation(pending);
    assert_eq!(error_code(&completion), "link.ambiguous_target");
    assert_eq!(
        fixture.text(buffer_id),
        "Hello [[Systems/Emergence]]world.\n"
    );
}

#[test]
fn duplicate_intents_replay_cached_results_without_second_insert() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let (sequence, plan_id, _) = fixture.prepare(41, buffer_id, revision, 6, "target.md");

    assert!(fixture.commit(sequence, plan_id).is_success());
    let once = fixture.text(buffer_id);
    // The identical gesture replays its cached result: no second insert.
    let replay = fixture.commit(sequence, plan_id);
    assert!(replay.is_success(), "cached replay: {replay:?}");
    assert_eq!(fixture.text(buffer_id), once);

    // A different payload under a consumed gesture is a mismatch.
    assert_eq!(
        error_code(&fixture.commit(sequence, plan_id + 999)),
        "link.intent_mismatch"
    );
    assert_eq!(fixture.text(buffer_id), once);

    // An unconsumed sequence below the high-water mark is expired.
    assert_eq!(
        error_code(&fixture.commit(sequence - 1, plan_id)),
        "link.intent_expired"
    );
}

#[test]
fn stale_source_after_edit_rejects_with_zero_mutation() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let (sequence, plan_id, _) = fixture.prepare(1, buffer_id, revision, 6, "target.md");

    // The source moves while the plan is held (confirmation window).
    let edit = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "ops": [{ "range": [0, 0], "replacement": "PREFIX " }]
        }),
        Origin::Gui,
    ));
    assert!(edit.is_success());

    let result = fixture.commit(sequence, plan_id);
    assert_eq!(error_code(&result), "link.stale_source");
    assert_eq!(
        fixture.text(buffer_id),
        "PREFIX Hello world.\n",
        "zero mutation from the rejected commit"
    );
    assert_eq!(fixture.saved_bytes("target.md"), b"Target note.\n");
}

#[test]
fn index_change_between_prepare_and_commit_retargets_nothing() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let (sequence, plan_id, _) = fixture.prepare(1, buffer_id, revision, 6, "target.md");

    // The target disappears from the vault and the index reconciles.
    fixture
        .fs
        .remove(&fixture.root.join("target.md"))
        .expect("removed");
    reconcile(&fixture.flavite);

    let result = fixture.commit(sequence, plan_id);
    assert_eq!(error_code(&result), "link.target_retargeted");
    assert_eq!(fixture.text(buffer_id), "Hello world.\n");
}

#[test]
fn non_ascii_targets_insert_and_round_trip_through_the_scanner() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("Ideas/漢字.md", "kanji\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let (sequence, plan_id, serialized) =
        fixture.prepare(1, buffer_id, revision, 6, "Ideas/漢字.md");
    assert_eq!(serialized, "[[Ideas/漢字]]");
    assert!(fixture.commit(sequence, plan_id).is_success());
    assert_eq!(fixture.text(buffer_id), "Hello [[Ideas/漢字]]world.\n");
}

#[test]
fn plugin_origin_without_capabilities_is_denied_and_read_only_vaults_refuse() {
    let mut fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");

    // A plugin origin without the declared capability is denied up front.
    let denied = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "link.prepare",
        serde_json::json!({
            "intent": { "client": fixture.client, "sequence": 1 },
            "source": { "buffer_id": buffer_id, "revision": revision, "caret": 6 },
            "target_path": "target.md",
            "stable_link_policy": "disabled",
        }),
        Origin::Plugin,
    ));
    assert!(matches!(
        &denied,
        ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::CapabilityDenied
    ));

    // Read-only enforcement is unconditional for both commands (TD-17).
    fixture.flavite.set_read_only(true);
    let prepare = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "link.prepare",
        serde_json::json!({
            "intent": { "client": fixture.client, "sequence": 1 },
            "source": { "buffer_id": buffer_id, "revision": revision, "caret": 6 },
            "target_path": "target.md",
            "stable_link_policy": "disabled",
        }),
        Origin::Gui,
    ));
    assert!(
        matches!(
            &prepare,
            ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::ReadOnly
        ),
        "link.prepare must be read-only rejected: {prepare:?}"
    );
    let insert = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "link.insert",
        serde_json::json!({
            "intent": { "client": fixture.client, "sequence": 1 },
            "plan_id": 1,
        }),
        Origin::Gui,
    ));
    assert!(
        matches!(
            &insert,
            ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::ReadOnly
        ),
        "link.insert must be read-only rejected: {insert:?}"
    );
    assert_eq!(fixture.text(buffer_id), "Hello world.\n");
}

#[test]
fn raw_edit_shape_is_rejected_with_an_explicit_pointer() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let rejected = fixture.flavite.execute(Invocation::with_preconditions(
        next_id(&fixture.flavite),
        "link.insert",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "ops": [{ "range": [0, 0], "replacement": "[[target]]" }]
        }),
        Origin::Gui,
        Preconditions::new()
            .with_buffer_id(buffer_id)
            .with_revision(revision)
            .requires_write(),
    ));
    match &rejected {
        ExecutionResult::Rejected(rejected) => {
            assert_eq!(rejected.reason, RejectionReason::InvalidArgs);
            assert_eq!(
                rejected.message.as_deref(),
                Some("use apply_edits for raw replacements")
            );
        }
        other => panic!("raw shape must be rejected: {other:?}"),
    }
    assert_eq!(fixture.text(buffer_id), "Hello world.\n");
}

#[test]
fn api_version_is_0_2_0_for_the_semantic_surface() {
    assert_eq!(API_VERSION, "0.2.0");
}
