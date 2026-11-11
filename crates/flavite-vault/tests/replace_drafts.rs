//! RFC-0006 S09 — explicit draft application (D-09/D-11) over a real
//! Session + MemFs vault: opt-in draft preview candidates, isolated
//! buffer-edit install with a durable recovery checkpoint (AppliedDraft,
//! never a forced file save), stale guards, and restart classification via
//! the checkpoint marker.

use flavite_buffer::manager::BufferManager;
use flavite_buffer::recovery::RecoveryFile;
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

fn open(session: &Session, relative: &str) -> (u64, u64) {
    let result = wait_terminal(
        session,
        session.execute(SessionCommand::OpenNote {
            path: PathBuf::from(format!("/test/vault/{relative}")),
            policy: OpenPolicy::ExistingOnly,
        }),
    );
    match expect_success(result, "open") {
        CommandResult::NoteOpened {
            buffer_id,
            revision,
        } => (buffer_id.as_u64(), revision.as_u64()),
        other => panic!("expected open, got {other:?}"),
    }
}

fn edit(session: &Session, buffer_id: u64, revision: u64, ops: Vec<flavite_buffer::EditOp>) -> u64 {
    let result = wait_terminal(
        session,
        session.execute(SessionCommand::ApplyEdits {
            buffer_id: buffer_id.into(),
            revision: flavite_buffer::revision::Revision::new(revision),
            ops,
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            editor: None,
            operation: None,
        }),
    );
    match expect_success(result, "edit") {
        CommandResult::EditsApplied { new_revision, .. } => new_revision.as_u64(),
        other => panic!("expected edit, got {other:?}"),
    }
}

fn span(start: usize, end: usize) -> flavite_syntax::Span<flavite_syntax::text::TextByte> {
    flavite_syntax::Span::new(
        flavite_syntax::text::TextByte(start),
        flavite_syntax::text::TextByte(end),
    )
}

fn draft_preview(session: &Session, query: &str, replacement: &str) -> (u128, u64) {
    let result = wait_terminal(
        session,
        session.execute(SessionCommand::ReplacePreviewPrepare {
            request_id: rid(),
            request: flavite_vault::replace::preview::PreviewRequest {
                query: query.to_owned(),
                replacement: replacement.to_owned(),
                mode: flavite_text_search::matcher::MatchMode::Literal,
                case_sensitive: false,
                whole_word: false,
                include_drafts: true,
            },
        }),
    );
    let preview_id = match expect_success(result, "draft preview") {
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

fn text_find(session: &Session, buffer_id: u64, revision: u64, query: &str) -> u64 {
    let result = wait_terminal(
        session,
        session.execute(SessionCommand::FindMatches {
            request_id: rid(),
            request: flavite_vault::text_edit::FindRequest {
                buffer_id: buffer_id.into(),
                revision: flavite_buffer::revision::Revision::new(revision),
                query: query.to_owned(),
                mode: flavite_text_search::matcher::MatchMode::Literal,
                case_sensitive: false,
                whole_word: false,
                scope: None,
                offset: 0,
            },
        }),
    );
    match expect_success(result, "find") {
        CommandResult::Found { total_matches, .. } => total_matches,
        other => panic!("expected find, got {other:?}"),
    }
}

/// List recovery files under the private data dir for this vault.
fn recovery_files(h: &Harness) -> Vec<std::path::PathBuf> {
    let dir = h
        .data_dir
        .path()
        .join("flavite")
        .join("vaults")
        .join(h.identity.uuid.to_string())
        .join("recovery");
    std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "bin"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn draft_apply_is_isolated_checkpointed_and_not_a_save() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    let (buffer_id, rev0) = open(&h.session, "notes/a.md");
    let rev1 = edit(
        &h.session,
        buffer_id,
        rev0,
        vec![flavite_buffer::EditOp::new(span(5, 5), " more".to_owned())],
    );
    assert_eq!(rev1, rev0 + 1);
    let (preview_id, revision) = draft_preview(&h.session, "alpha", "X");
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Completed);
    assert_eq!(totals.applied_drafts, 1);
    assert_eq!(totals.saved, 0);
    // Not a file save: the disk still holds the original bytes.
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("notes/a.md"))
            .unwrap()
            .unwrap(),
        b"alpha\n"
    );
    // The buffer now holds the after text ("X more\n") as an isolated
    // revision: rev1 (dirty) + 1 install revision.
    assert_eq!(text_find(&h.session, buffer_id, rev1 + 1, "X"), 1);
    assert_eq!(text_find(&h.session, buffer_id, rev1 + 1, "alpha"), 0);
    // A durable checkpoint covers the applied revision.
    let checkpoints: Vec<_> = recovery_files(&h)
        .into_iter()
        .filter_map(|path| RecoveryFile::from_bytes(&std::fs::read(&path).unwrap_or_default()))
        .filter(|file| file.header.is_checkpoint)
        .collect();
    assert_eq!(checkpoints.len(), 1, "one durable checkpoint");
    assert_eq!(checkpoints[0].text, "X more\n");
    assert_eq!(checkpoints[0].header.revision.as_u64(), rev1 + 1);
}

#[test]
fn draft_guard_skips_after_revision_changed() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    let (buffer_id, rev0) = open(&h.session, "notes/a.md");
    let rev1 = edit(
        &h.session,
        buffer_id,
        rev0,
        vec![flavite_buffer::EditOp::new(span(5, 5), " more".to_owned())],
    );
    let (preview_id, revision) = draft_preview(&h.session, "alpha", "X");
    // The user types again after the preview was sealed.
    let rev2 = edit(
        &h.session,
        buffer_id,
        rev1,
        vec![flavite_buffer::EditOp::new(span(10, 10), " two".to_owned())],
    );
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Partial);
    assert_eq!(totals.stale, 1);
    assert_eq!(totals.applied_drafts, 0);
    // Zero effects: the buffer still holds the user's newer text, the disk
    // is untouched, and no checkpoint was written.
    assert_eq!(text_find(&h.session, buffer_id, rev2, "two"), 1);
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("notes/a.md"))
            .unwrap()
            .unwrap(),
        b"alpha\n"
    );
    assert!(recovery_files(&h).into_iter().all(|path| {
        RecoveryFile::from_bytes(&std::fs::read(&path).unwrap_or_default())
            .is_none_or(|file| !file.header.is_checkpoint)
    }));
}

#[test]
fn draft_apply_restart_reclassifies_from_records_and_checkpoint() {
    let mut h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    let (buffer_id, rev0) = open(&h.session, "notes/a.md");
    let _rev1 = edit(
        &h.session,
        buffer_id,
        rev0,
        vec![flavite_buffer::EditOp::new(span(5, 5), " more".to_owned())],
    );
    let (preview_id, revision) = draft_preview(&h.session, "alpha", "X");
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (_phase, totals_before) = job_status(&h.session, job_id);
    assert_eq!(totals_before.applied_drafts, 1);

    // "Restart": a fresh Session over the same fs/data dir/identity.
    let fs = h.fs.clone();
    let _root = h.root.clone();
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
    let (phase, totals) = job_status(&session2, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Completed);
    assert_eq!(totals, totals_before);
    assert_eq!(totals.applied_drafts, 1);
    h.session = session2;
}
