//! RFC-0006 S09 — guarded Revert (D-11) over a real Session + MemFs
//! vault: saved-note restoration under the after-hash precondition,
//! external-write staleness skips, draft restorations with revision
//! lineage guards (later typing / Undo-to-identical-text are
//! conservatively skipped), and idempotent replay.

use flavite_buffer::manager::BufferManager;
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
        .with_identity(identity)
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

fn preview_all(session: &Session, query: &str, replacement: &str) -> (u128, u64) {
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

fn revert(session: &Session, job_id: u128) -> SessionResult {
    wait_terminal(
        session,
        session.execute(SessionCommand::ReplaceRevert {
            request_id: rid(),
            job_id,
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
        }),
    );
    match expect_success(result, "edit") {
        CommandResult::EditsApplied { new_revision, .. } => new_revision.as_u64(),
        other => panic!("expected edit, got {other:?}"),
    }
}

fn undo(session: &Session, buffer_id: u64, revision: u64) -> u64 {
    let result = wait_terminal(
        session,
        session.execute(SessionCommand::Undo {
            buffer_id: buffer_id.into(),
            revision: flavite_buffer::revision::Revision::new(revision),
        }),
    );
    match expect_success(result, "undo") {
        CommandResult::UndoPerformed { new_revision, .. } => new_revision.as_u64(),
        other => panic!("expected undo, got {other:?}"),
    }
}

fn span(start: usize, end: usize) -> flavite_syntax::Span<flavite_syntax::text::TextByte> {
    flavite_syntax::Span::new(
        flavite_syntax::text::TextByte(start),
        flavite_syntax::text::TextByte(end),
    )
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

#[test]
fn revert_restores_saved_notes_under_after_hash_guard() {
    let h = harness();
    seed_note(&h, "a.md", b"hello 0\n");
    seed_note(&h, "b.md", b"hello 1\n");
    let (preview_id, revision) = preview_all(&h.session, "hello", "world");
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Completed);
    assert_eq!(totals.saved, 2);
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("a.md")).unwrap().unwrap(),
        b"world 0\n"
    );

    let reverted = revert(&h.session, job_id);
    let revert_job = applied_job_id(&reverted);
    assert_eq!(revert_job, job_id);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Completed);
    assert_eq!(totals.reverted, 2);
    assert_eq!(totals.reverted_drafts, 0);
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("a.md")).unwrap().unwrap(),
        b"hello 0\n"
    );
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("b.md")).unwrap().unwrap(),
        b"hello 1\n"
    );

    // Idempotent replay: a second Revert returns the same terminal receipt.
    let replay = revert(&h.session, job_id);
    let CommandResult::ReplaceJobFinished {
        job_id: replay_id,
        phase: replay_phase,
        ..
    } = expect_success(replay, "revert replay")
    else {
        panic!("revert replay finished");
    };
    assert_eq!(replay_id, job_id);
    assert_eq!(replay_phase, flavite_vault::replace::JobPhase::Completed);
}

#[test]
fn revert_skips_notes_changed_by_external_write() {
    let h = harness();
    seed_note(&h, "a.md", b"hello 0\n");
    seed_note(&h, "b.md", b"hello 1\n");
    let (preview_id, revision) = preview_all(&h.session, "hello", "world");
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("a.md")).unwrap().unwrap(),
        b"world 0\n"
    );
    // A noncooperating external writer replaces a.md after the apply.
    let path = h.root.join("a.md");
    h.fs.remove(&path).unwrap();
    h.fs.write_new(&path, b"third 0\n").unwrap();
    let reverted = revert(&h.session, job_id);
    let revert_job = applied_job_id(&reverted);
    assert_eq!(revert_job, job_id);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Partial);
    assert_eq!(totals.reverted, 1, "b.md restored");
    assert_eq!(totals.stale, 1, "a.md skipped under the after-hash guard");
    // The external writer's bytes are untouched.
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("a.md")).unwrap().unwrap(),
        b"third 0\n"
    );
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("b.md")).unwrap().unwrap(),
        b"hello 1\n"
    );
}

#[test]
fn draft_revert_restores_before_text_with_lineage_guard() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    let (buffer_id, rev0) = open(&h.session, "notes/a.md");
    let rev1 = edit(
        &h.session,
        buffer_id,
        rev0,
        vec![flavite_buffer::EditOp::new(span(5, 5), " more".to_owned())],
    );
    let (preview_id, revision) = {
        let result = wait_terminal(
            &h.session,
            h.session.execute(SessionCommand::ReplacePreviewPrepare {
                request_id: rid(),
                request: flavite_vault::replace::preview::PreviewRequest {
                    query: "alpha".to_owned(),
                    replacement: "X".to_owned(),
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
                &h.session,
                h.session.execute(SessionCommand::ReplaceSelect {
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
    };
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Completed);
    assert_eq!(totals.applied_drafts, 1);
    // Apply installs exactly one revision after rev1.
    assert_eq!(text_find(&h.session, buffer_id, rev1 + 1, "X"), 1);

    let reverted = revert(&h.session, job_id);
    let revert_job = applied_job_id(&reverted);
    assert_eq!(revert_job, job_id);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Completed);
    assert_eq!(totals.reverted_drafts, 1);
    // The buffer is back to the before text; the disk never changed.
    assert_eq!(text_find(&h.session, buffer_id, rev1 + 2, "alpha"), 1);
    assert_eq!(text_find(&h.session, buffer_id, rev1 + 2, "X"), 0);
    assert_eq!(
        h.fs.read_if_exists(&h.root.join("notes/a.md"))
            .unwrap()
            .unwrap(),
        b"alpha\n"
    );
}

#[test]
fn draft_revert_conservatively_skips_later_typing_even_if_text_identical() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    let (buffer_id, rev0) = open(&h.session, "notes/a.md");
    let rev1 = edit(
        &h.session,
        buffer_id,
        rev0,
        vec![flavite_buffer::EditOp::new(span(5, 5), " more".to_owned())],
    );
    let (preview_id, revision) = {
        let result = wait_terminal(
            &h.session,
            h.session.execute(SessionCommand::ReplacePreviewPrepare {
                request_id: rid(),
                request: flavite_vault::replace::preview::PreviewRequest {
                    query: "alpha".to_owned(),
                    replacement: "X".to_owned(),
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
                &h.session,
                h.session.execute(SessionCommand::ReplaceSelect {
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
    };
    let result = apply(&h.session, preview_id, revision);
    let job_id = applied_job_id(&result);

    // The user types after the apply (revision advances), then undoes back
    // to text identical to the applied after-content — a different
    // revision. The conservative guard must still skip the revert.
    let rev_typed = edit(
        &h.session,
        buffer_id,
        rev1 + 1,
        vec![flavite_buffer::EditOp::new(span(7, 7), "!".to_owned())],
    );
    let rev_identical = undo(&h.session, buffer_id, rev_typed);
    assert_eq!(text_find(&h.session, buffer_id, rev_identical, "X"), 1);

    let reverted = revert(&h.session, job_id);
    let revert_job = applied_job_id(&reverted);
    assert_eq!(revert_job, job_id);
    let (phase, totals) = job_status(&h.session, job_id);
    assert_eq!(phase, flavite_vault::replace::JobPhase::Partial);
    assert_eq!(totals.reverted_drafts, 0);
    assert_eq!(totals.stale, 1, "later typing invalidates the revert guard");
    // The buffer is untouched: still the (identical) after text.
    assert_eq!(text_find(&h.session, buffer_id, rev_identical, "X"), 1);
    assert_eq!(text_find(&h.session, buffer_id, rev_identical, "alpha"), 0);
}
