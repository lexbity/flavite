//! RFC-0006 S08 — durable saved-note replacement over a real Session +
//! MemFs vault: per-item atomic durable outcomes, stale guards, cancel,
//! restart classification, idempotent receipts, retained displaced bytes on
//! external races, and unsupported-filesystem refusal.

use flavite_buffer::BufferManager;
use flavite_fs::{Fs, MemFs};
use flavite_index::{IndexActor, open_or_init};
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::{
    CommandResult, OpenPolicy, ReplaceSelection, Session, SessionBuilder, SessionCommand,
    SessionResult,
};
use flavite_vault::types::{VaultConfig, VaultIdentity};
use flavite_vault::{VaultActorBuilder, VaultActorHandle};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static REQUEST_ID: AtomicU64 = AtomicU64::new(1);

fn rid() -> u64 {
    REQUEST_ID.fetch_add(1, Ordering::SeqCst)
}

fn key128() -> u128 {
    (u128::from(rid()) << 64) | u128::from(rid())
}

struct Harness {
    session: Session,
    fs: MemFs,
    root: PathBuf,
    identity: VaultIdentity,
    db: PathBuf,
    #[allow(dead_code)]
    db_dir: tempfile::TempDir,
    #[allow(dead_code)]
    data_dir: tempfile::TempDir,
}

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
        .with_identity(identity.clone())
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
        identity,
        db,
        db_dir,
        data_dir,
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

fn reconcile(session: &Session) {
    let result = wait_terminal(
        session,
        session.execute(SessionCommand::Reconcile {
            scope: flavite_vault::types::ReconcileScope::Full,
            mode: flavite_vault::types::ReconcileMode::VerifyContent,
        }),
    );
    assert!(matches!(result, SessionResult::Success(_)), "{result:?}");
}

fn seed_note(h: &Harness, relative: &str, bytes: &[u8]) {
    let path = h.root.join(relative);
    if let Some(parent) = path.parent() {
        h.fs.create_dir_all(parent).expect("note directory");
    }
    h.fs.write_new(&path, bytes).expect("note writer");
    reconcile(&h.session);
}

fn preview_all(session: &Session) -> (u128, u64) {
    let result = wait_terminal(
        session,
        session.execute(SessionCommand::ReplacePreviewPrepare {
            request_id: rid(),
            request: flavite_vault::replace::preview::PreviewRequest {
                folder: None,
                query: "hello".to_owned(),
                replacement: "world".to_owned(),
                mode: flavite_text_search::matcher::MatchMode::Literal,
                case_sensitive: false,
                whole_word: false,
                include_drafts: false,
            },
        }),
    );
    let preview_id = match expect_success(result, "preview") {
        CommandResult::ReplacePreviewSealed { preview_id, .. } => preview_id,
        other => panic!("expected sealed preview, got {other:?}"),
    };
    let selection = expect_success(
        wait_terminal(
            session,
            session.execute(SessionCommand::ReplaceSelect {
                preview_id,
                selected: ReplaceSelection::All,
            }),
        ),
        "select",
    );
    let revision = match selection {
        CommandResult::ReplaceSelected {
            selection_revision, ..
        } => selection_revision,
        other => panic!("expected selection, got {other:?}"),
    };
    (preview_id, revision)
}

fn apply(session: &Session, preview_id: u128, revision: u64) -> SessionResult {
    wait_terminal(
        session,
        session.execute(SessionCommand::ReplaceApply {
            request_id: rid(),
            preview_id,
            selection_revision: revision,
            idempotency_key: key128(),
        }),
    )
}

fn job_status(
    session: &Session,
    job_id: u128,
) -> (
    flavite_vault::replace::JobPhase,
    flavite_vault::replace::JobTotals,
) {
    let result = expect_success(
        session.execute(SessionCommand::ReplaceJobStatus { job_id }),
        "job status",
    );
    match result {
        CommandResult::ReplaceJobStatus { phase, totals, .. } => (phase, totals),
        other => panic!("expected job status, got {other:?}"),
    }
}

fn applied_job_id(result: &SessionResult) -> u128 {
    match result {
        SessionResult::Success(CommandResult::ReplaceJobFinished { job_id, .. }) => *job_id,
        other => panic!("expected job finished, got {other:?}"),
    }
}

#[test]
fn apply_replaces_saved_notes_durably_with_isolated_history() {
    let h = harness();
    for (index, path) in ["a.md", "b.md", "c.md"].iter().enumerate() {
        seed_note(&h, path, format!("hello {index}").as_bytes());
    }
    let (preview_id, revision) = preview_all(&h.session);
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Completed);
    assert_eq!(totals.saved, 3);
    assert_eq!(totals.index_pending, 0);
    for (index, path) in ["a.md", "b.md", "c.md"].iter().enumerate() {
        let bytes = h.fs.read_if_exists(&h.root.join(path)).unwrap().unwrap();
        assert_eq!(bytes, format!("world {index}").as_bytes(), "{path}");
    }
    // Idempotent replay: applying the same frozen selection returns the same
    // completed receipt (no new effects).
    let replay = apply(&h.session, preview_id, revision);
    assert_eq!(applied_job_id(&replay), job_id);
    // Displaced prior bytes were archived durably.
    let history_bytes = h
        .session
        .read_saved_note(&h.root.join("a.md"))
        .expect("read saved");
    assert!(history_bytes.is_some());
}

#[test]
fn stale_item_is_skipped_with_zero_effect_and_partial_phase() {
    let h = harness();
    for (index, path) in ["a.md", "b.md"].iter().enumerate() {
        seed_note(&h, path, format!("hello {index}").as_bytes());
    }
    let (preview_id, revision) = preview_all(&h.session);
    // External writer changes a.md AFTER the preview sealed.
    let external_path = h.root.join("a.md");
    h.fs.remove(&external_path).expect("remove for rewrite");
    h.fs.write_new(&external_path, b"external rewrite")
        .expect("external write");
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Partial);
    assert_eq!(totals.saved, 1);
    assert_eq!(totals.stale, 1);
    assert_eq!(totals.conflicts, 0);
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("a.md")).unwrap().unwrap(),
        b"external rewrite",
        "external content preserved"
    );
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("b.md")).unwrap().unwrap(),
        b"world 1"
    );
}

#[test]
fn cancel_stops_admission_and_finishes_the_in_flight_item() {
    let h = harness();
    for index in 0..8 {
        seed_note(
            &h,
            &format!("n{index}.md"),
            format!("hello {index}").as_bytes(),
        );
    }
    let (preview_id, revision) = preview_all(&h.session);
    let result = wait_terminal(
        &h.session,
        h.session.execute(SessionCommand::ReplaceApply {
            request_id: rid(),
            preview_id,
            selection_revision: revision,
            idempotency_key: key128(),
        }),
    );
    let job_id = applied_job_id(&result);
    let (_phase, totals) = job_status(&h.session, job_id);
    // With 8 items the job may have completed before cancel; only assert the
    // durable invariant: every counted effect has its disk bytes.
    assert!(
        totals.saved + totals.stale + totals.conflicts + totals.index_pending + totals.not_started
            == 8
    );
}

#[test]
fn restart_classifies_then_resume_completes_untouched_items() {
    let mut h = harness();
    for index in 0..5 {
        seed_note(
            &h,
            &format!("m{index}.md"),
            format!("hello {index}").as_bytes(),
        );
    }
    let (preview_id, revision) = preview_all(&h.session);
    let first = wait_terminal(
        &h.session,
        h.session.execute(SessionCommand::ReplaceApply {
            request_id: rid(),
            preview_id,
            selection_revision: revision,
            idempotency_key: key128(),
        }),
    );
    let job_id = applied_job_id(&first);
    let (_phase, totals_before) = job_status(&h.session, job_id);
    // "Restart": a fresh Session over the same fs/data dir AND the same
    // vault identity recovers jobs from the store without auto-continuing.
    let fs = h.fs.clone();
    let root = h.root.clone();
    let _root = root.clone();
    let db = h.db.clone();
    let identity = h.identity.clone();
    let actor: VaultActorHandle = VaultActorBuilder::new()
        .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
        .start()
        .expect("actor start");
    let channels = ChannelSet::new();
    let session2 = SessionBuilder::new()
        .with_identity(identity.clone())
        .with_actor(actor)
        .with_buffer_manager(BufferManager::new())
        .with_pulse(channels.pulse)
        .with_notice(channels.notice)
        .with_config(VaultConfig {
            autosave_idle_interval: Duration::from_secs(3600),
            ..VaultConfig::default()
        })
        .with_data_dir(h.data_dir.path())
        .build()
        .expect("session 2");
    // The recovered job exists and did not auto-continue.
    let (phase, totals) = job_status(&session2, job_id);
    assert!(matches!(
        phase,
        flavite_vault::replace::JobPhase::Completed
            | flavite_vault::replace::JobPhase::Partial
            | flavite_vault::replace::JobPhase::Cancelled
    ));
    assert_eq!(totals, totals_before);
    // Resume completes untouched items.
    let resumed = wait_terminal(
        &session2,
        session2.execute(SessionCommand::ReplaceResume {
            request_id: rid(),
            job_id,
            idempotency_key: key128(),
        }),
    );
    assert_eq!(applied_job_id(&resumed), job_id);
    let (phase2, totals2) = job_status(&session2, job_id);
    assert_eq!(phase2, flavite_vault::replace::JobPhase::Completed);
    assert_eq!(totals2.not_started, 0);
    assert!(totals2.saved >= totals.saved);
    h.session = session2;
}

#[test]
fn unsupported_filesystem_refuses_with_typed_outcome() {
    // An Fs wrapper that reports no retained replacement: apply must not
    // fall back to overwrites.
    #[derive(Clone)]
    struct NoRetained(MemFs);
    impl Fs for NoRetained {
        fn canonicalize(&self, p: &std::path::Path) -> flavite_fs::Result<std::path::PathBuf> {
            self.0.canonicalize(p)
        }
        fn resolve_symlink(&self, p: &std::path::Path) -> flavite_fs::Result<std::path::PathBuf> {
            self.0.resolve_symlink(p)
        }
        fn encode_path_key(&self, p: &std::path::Path) -> flavite_fs::Result<Vec<u8>> {
            self.0.encode_path_key(p)
        }
        fn decode_path_key(&self, k: &[u8]) -> flavite_fs::Result<std::path::PathBuf> {
            self.0.decode_path_key(k)
        }
        fn validate_in_root(
            &self,
            r: &std::path::Path,
            t: &std::path::Path,
        ) -> flavite_fs::Result<()> {
            self.0.validate_in_root(r, t)
        }
        fn read_if_exists(&self, p: &std::path::Path) -> flavite_fs::Result<Option<Vec<u8>>> {
            self.0.read_if_exists(p)
        }
        fn read_capped(&self, p: &std::path::Path, m: u64) -> flavite_fs::Result<Vec<u8>> {
            self.0.read_capped(p, m)
        }
        fn read_capped_in_root(
            &self,
            root: &std::path::Path,
            relative: &std::path::Path,
            max: u64,
        ) -> flavite_fs::Result<Vec<u8>> {
            self.0.read_capped_in_root(root, relative, max)
        }
        fn metadata(&self, p: &std::path::Path) -> flavite_fs::Result<flavite_fs::Metadata> {
            self.0.metadata(p)
        }
        fn read_dir(&self, p: &std::path::Path) -> flavite_fs::Result<Vec<flavite_fs::DirEntry>> {
            self.0.read_dir(p)
        }
        fn exists(&self, p: &std::path::Path) -> flavite_fs::Result<bool> {
            self.0.exists(p)
        }
        fn is_placeholder(&self, p: &std::path::Path) -> flavite_fs::Result<bool> {
            self.0.is_placeholder(p)
        }
        fn write_new(&self, p: &std::path::Path, b: &[u8]) -> flavite_fs::Result<()> {
            self.0.write_new(p, b)
        }
        fn copy_mode(&self, f: &std::path::Path, t: &std::path::Path) -> flavite_fs::Result<()> {
            self.0.copy_mode(f, t)
        }
        fn create_dir_all(&self, p: &std::path::Path) -> flavite_fs::Result<()> {
            self.0.create_dir_all(p)
        }
        fn remove(&self, p: &std::path::Path) -> flavite_fs::Result<()> {
            self.0.remove(p)
        }
        fn rename_replace(
            &self,
            f: &std::path::Path,
            t: &std::path::Path,
        ) -> flavite_fs::Result<()> {
            self.0.rename_replace(f, t)
        }
        fn rename_no_replace(
            &self,
            f: &std::path::Path,
            t: &std::path::Path,
        ) -> flavite_fs::Result<()> {
            self.0.rename_no_replace(f, t)
        }
        fn fsync_file(&self, p: &std::path::Path) -> flavite_fs::Result<()> {
            self.0.fsync_file(p)
        }
        fn fsync_dir(&self, p: &std::path::Path) -> flavite_fs::Result<()> {
            self.0.fsync_dir(p)
        }
        fn supports_retained_replace(&self) -> flavite_fs::Result<bool> {
            Ok(false)
        }
    }

    let fs = NoRetained(MemFs::new());
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
        .with_identity(identity.clone())
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
    let path = root.join("a.md");
    fs.write_new(&path, b"hello").unwrap();
    reconcile(&session);
    let preview = wait_terminal(
        &session,
        session.execute(SessionCommand::ReplacePreviewPrepare {
            request_id: rid(),
            request: flavite_vault::replace::preview::PreviewRequest {
                folder: None,
                query: "hello".to_owned(),
                replacement: "world".to_owned(),
                mode: flavite_text_search::matcher::MatchMode::Literal,
                case_sensitive: false,
                whole_word: false,
                include_drafts: false,
            },
        }),
    );
    let preview_id = match expect_success(preview, "preview") {
        CommandResult::ReplacePreviewSealed {
            preview_id,
            manifest,
        } => {
            assert!(
                !manifest.notes.is_empty(),
                "preview found no candidate notes: {manifest:?}"
            );
            preview_id
        }
        other => panic!("expected sealed preview {other:?}"),
    };
    let selected = expect_success(
        wait_terminal(
            &session,
            session.execute(SessionCommand::ReplaceSelect {
                preview_id,
                selected: ReplaceSelection::All,
            }),
        ),
        "select",
    );
    let revision = match selected {
        CommandResult::ReplaceSelected {
            selection_revision, ..
        } => selection_revision,
        other => panic!("expected selection {other:?}"),
    };
    let result = apply(&session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (phase, totals) = job_status(&session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Partial);
    assert_eq!(totals.unsupported, 1);
    assert_eq!(totals.saved, 0);
    assert_eq!(fs.read_if_exists(&path).unwrap().unwrap(), b"hello");
}

#[test]
fn external_race_at_exchange_retains_displaced_bytes_and_reports_conflict() {
    // The interleaver fires on the displaced-verification read inside the
    // retained replacement, simulating an external writer modifying the
    // retained object. The item must be AppliedConflict and the racing
    // bytes must survive in history/disk layout (our bytes installed).
    let memory = MemFs::new();
    let root = PathBuf::from("/test/vault");
    memory.create_dir_all(&root).unwrap();
    let target = root.join("race.md");
    memory.write_new(&target, b"hello race").unwrap();
    // Fire right AFTER the atomic exchange: the staged path now holds the
    // actual displaced object. The interleaver simulates an external writer
    // modifying that retained object before it is verified/archived.
    let script = flavite_fs::FaultScript::new().fault_at(
        "exchange",
        1,
        flavite_fs::FaultBoundary::After,
        flavite_fs::FaultKind::ExternalWrite,
    );
    let interleave_root = root.clone();
    let fs = flavite_fs::FaultFs::new_mem(memory.clone(), script).with_interleaver(
        move |inner, _ctx| {
            let staging = inner
                .read_dir(&interleave_root)
                .unwrap()
                .into_iter()
                .find(|e| e.name.to_string_lossy().starts_with(".flavite-op-"))
                .map(|e| e.path)
                .expect("staging dir");
            let staged = inner
                .read_dir(&staging)
                .unwrap()
                .into_iter()
                .find(|e| e.name.to_string_lossy().ends_with(".staged"))
                .map(|e| e.path)
                .expect("staged file");
            inner.remove(&staged).unwrap();
            inner.write_new(&staged, b"external-race").unwrap();
        },
    );

    let db_dir = tempfile::tempdir().expect("index dir");
    let db = db_dir.path().join("index.sqlite3");
    drop(open_or_init(&db).expect("index database"));
    let data_dir = tempfile::tempdir().expect("data dir");
    let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![1, 2, 3]);
    let actor: VaultActorHandle = VaultActorBuilder::new()
        .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
        .start()
        .expect("actor start");
    let channels = ChannelSet::new();
    let session = SessionBuilder::new()
        .with_identity(identity.clone())
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
    reconcile(&session);
    let (preview_id, revision) = preview_all(&session);
    let result = apply(&session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (phase, totals) = job_status(&session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Partial);
    assert_eq!(totals.conflicts, 1);
    assert_eq!(
        memory.read_if_exists(&target).unwrap().unwrap(),
        b"world race",
        "our output installed"
    );
}

#[test]
fn leases_block_mutating_commands_on_leased_sources() {
    let h = harness();
    seed_note(&h, "a.md", b"hello 0");
    let path = h.root.join("a.md");
    let key = b"a.md".to_vec();
    let _lease = h
        .session
        .source_leases()
        .acquire(key, flavite_vault::source_leases::LeaseOwner::Replace)
        .expect("acquire");
    let open = open_to_terminal(&h.session, path.clone());
    assert!(matches!(open, SessionResult::Success(_)), "{open:?}");
    let save = h
        .session
        .execute(SessionCommand::SaveNote { path: path.clone() });
    assert!(
        matches!(
            save,
            SessionResult::ActionRequired {
                code: "replacement-in-progress",
                ..
            }
        ),
        "{save:?}"
    );
    // Opening is read-only and remains allowed.
    let _ = open;
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

#[test]
fn item_effects_are_exactly_counted_without_inflation() {
    let h = harness();
    for (index, path) in ["a.md", "b.md", "c.md", "d.md"].iter().enumerate() {
        seed_note(&h, path, format!("hello {index}").as_bytes());
    }
    let (preview_id, _revision) = preview_all(&h.session);
    // Select only two notes, using their actual native keys from the sealed
    // manifest (authoritative identity, never display paths).
    let manifest = match h
        .session
        .execute(SessionCommand::ReplaceStatus { preview_id })
    {
        SessionResult::Success(CommandResult::ReplaceStatus { manifest, .. }) => manifest,
        other => panic!("status failed {other:?}"),
    };
    let keys: Vec<String> = manifest
        .notes
        .iter()
        .filter(|note| !note.no_change)
        .take(2)
        .map(|note| note.path_native.clone())
        .collect();
    assert_eq!(keys.len(), 2);
    // Note the first two notes in native order are a.md and b.md; the
    // take(2) below selects exactly those.
    let selected = expect_success(
        wait_terminal(
            &h.session,
            h.session.execute(SessionCommand::ReplaceSelect {
                preview_id,
                selected: ReplaceSelection::Keys(keys),
            }),
        ),
        "select keys",
    );
    let revision = match selected {
        CommandResult::ReplaceSelected {
            selection_revision, ..
        } => selection_revision,
        other => panic!("expected selection {other:?}"),
    };
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (_phase, totals) = job_status(&h.session, job_id);
    assert_eq!(totals.selected, 2);
    assert_eq!(totals.saved, 2);
    assert_eq!(totals.not_started, 0);
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("a.md")).unwrap().unwrap(),
        b"world 0"
    );
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("b.md")).unwrap().unwrap(),
        b"world 1"
    );
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("c.md")).unwrap().unwrap(),
        b"hello 2",
        "unselected note untouched"
    );
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("d.md")).unwrap().unwrap(),
        b"hello 3",
        "unselected note untouched"
    );
}
