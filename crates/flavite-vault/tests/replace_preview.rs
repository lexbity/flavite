//! RFC-0006 S07 — vault source Find and sealed replacement preview
//! (D-08/D-09) over a real Session + MemFs vault: frozen scope, per-note
//! guarded reads, exact counts, typed exclusions (dirty/conflicted/
//! orphaned buffers, missing/unreadable/oversize/undecodable), sealed
//! before/after payloads, NoChange rows, selection revisions, and the
//! draft opt-in refusal.

use blake3::hash;
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

fn open_to_terminal(session: &Session, path: PathBuf) -> SessionResult {
    wait_terminal(
        session,
        session.execute(SessionCommand::OpenNote {
            path,
            policy: OpenPolicy::CreateIfMissing,
        }),
    )
}

/// Seed a saved note (creating parents) and reconcile the index so the
/// frozen catalog sees it.
fn seed_note(h: &Harness, relative: &str, bytes: &[u8]) {
    let path = h.root.join(relative);
    if let Some(parent) = path.parent() {
        h.fs.create_dir_all(parent).expect("note directory");
    }
    h.fs.write_new(&path, bytes).expect("note writer");
    reconcile(&h.session);
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

fn find_start(session: &Session, query: &str) -> SessionResult {
    wait_terminal(
        session,
        session.execute(SessionCommand::SourceFindStart {
            request_id: rid(),
            request: flavite_vault::replace::preview::SourceFindRequest {
                query: query.to_owned(),
                mode: flavite_text_search::matcher::MatchMode::Literal,
                case_sensitive: false,
                whole_word: false,
            },
        }),
    )
}

fn find_page(
    session: &Session,
    finder_id: u128,
    note_index: u64,
    match_offset: u64,
    page_len: usize,
) -> SessionResult {
    session.execute(SessionCommand::SourceFindPage {
        finder_id,
        note_index,
        match_offset,
        page_len,
    })
}

fn preview(session: &Session, query: &str, replacement: &str) -> SessionResult {
    wait_terminal(
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
    )
}

fn started_finder(result: &CommandResult) -> (u128, flavite_vault::session::SourceFindPageData) {
    match result {
        CommandResult::SourceFindStarted {
            finder_id, page, ..
        } => (*finder_id, page.clone()),
        other => panic!("expected SourceFindStarted, got {other:?}"),
    }
}

#[test]
fn source_find_freezes_scope_and_excludes_dirty_buffers() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha beta alpha\n");
    seed_note(&h, "notes/b.md", b"alpha\n");
    seed_note(&h, "notes/dirty.md", b"dirty\n");
    // Dirty the dirty note through the real session.
    let dirty = open_to_terminal(&h.session, h.root.join("notes/dirty.md"));
    let (buffer_id, revision) = match expect_success(dirty, "open dirty") {
        CommandResult::NoteOpened {
            buffer_id,
            revision,
        } => (buffer_id, revision),
        other => panic!("{other:?}"),
    };
    let edits = expect_success(
        wait_terminal(
            &h.session,
            h.session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![flavite_buffer::EditOp::new(
                    flavite_syntax::text::Span::new(
                        flavite_syntax::text::TextByte(0),
                        flavite_syntax::text::TextByte(0),
                    ),
                    "changed ".to_owned(),
                )],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
                editor: None,
                operation: None,
            }),
        ),
        "dirty edit",
    );
    let _ = edits;

    let started = find_start(&h.session, "alpha");
    let (finder_id, page) = started_finder(&expect_success(started, "find"));
    // dirty.md is excluded (unsaved draft); a.md has 2 matches, b.md 1.
    assert_eq!(page.rows.len(), 2, "a.md and b.md rows");
    assert_eq!(page.rows[0].path, "notes/a.md");
    assert_eq!(page.rows[0].note_matches, 2);
    assert_eq!(page.rows[1].path, "notes/b.md");
    assert_eq!(page.rows[1].note_matches, 1);

    // The status result carried the exclusion.
    let CommandResult::SourceFindStarted { exclusions, .. } =
        expect_success(find_start(&h.session, "alpha"), "re-find")
    else {
        panic!("started");
    };
    assert!(exclusions.iter().any(|e| e.path == "notes/dirty.md"));
    let _ = finder_id;
}

#[test]
fn find_pages_a_single_note_with_many_matches_exactly_once() {
    let h = harness();
    let body = "x ".repeat(120) + "\n";
    seed_note(&h, "notes/one.md", body.as_bytes());
    let started = find_start(&h.session, "x");
    let (finder_id, first) = started_finder(&expect_success(started, "find"));
    assert_eq!(first.rows.len(), 1);
    assert_eq!(first.rows[0].matches.len(), 50);
    assert_eq!(first.rows[0].note_matches, 120);
    assert!(first.next_match_offset.is_some());

    let second = expect_success(
        find_page(&h.session, finder_id, first.rows[0].ordinal, 50, 50),
        "page 2",
    );
    let CommandResult::SourceFindPaged { page, .. } = second else {
        panic!("paged");
    };
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].matches.len(), 50);
    assert!(page.next_match_offset.is_some());

    let third = expect_success(
        find_page(&h.session, finder_id, page.rows[0].ordinal, 100, 50),
        "page 3",
    );
    let CommandResult::SourceFindPaged { page, .. } = third else {
        panic!("paged");
    };
    assert_eq!(page.rows[0].matches.len(), 20);
    assert!(page.next_match_offset.is_none());
    assert!(page.next_note_index.is_none());
}

#[test]
fn preview_seals_exact_before_after_payloads() {
    let h = harness();
    let source = b"alpha beta alpha\n";
    seed_note(&h, "notes/a.md", source);
    let result = preview(&h.session, "alpha", "X");
    let CommandResult::ReplacePreviewSealed {
        preview_id,
        manifest,
    } = expect_success(result, "preview")
    else {
        panic!("sealed");
    };
    assert_eq!(manifest.totals.scoped, 1);
    assert_eq!(manifest.totals.matched, 1);
    assert_eq!(manifest.totals.matches, 2);
    assert_eq!(manifest.notes.len(), 1);
    let note = &manifest.notes[0];
    assert_eq!(note.path, "notes/a.md");
    assert_eq!(note.match_count, 2);
    assert!(!note.no_change);
    assert_eq!(note.before.hash, hex::encode(hash(source).as_bytes()));
    // Exact serialized output: "X beta X\n" under the same descriptor/EOL.
    let expected = b"X beta X\n";
    assert_eq!(note.after.len, expected.len() as u64);
    // Sealed payloads exist under the private operation namespace.
    let manifest_path = h
        .root
        .join(".flavite/operations/replace")
        .join(&manifest.job_id)
        .join("manifest.json");
    assert!(
        h.fs.read_if_exists(&manifest_path).unwrap().is_some(),
        "manifest persists"
    );
    let after_path = h
        .root
        .join(".flavite/operations/replace")
        .join(&manifest.job_id)
        .join("payloads")
        .join(format!("{}.bin", note.after.hash));
    let after_bytes =
        h.fs.read_if_exists(&after_path)
            .unwrap()
            .expect("after payload");
    assert_eq!(after_bytes, expected);
    // The private tree never appears in the note catalog (not indexed).
    let catalog = h.session.note_catalog(500, None).expect("catalog");
    assert_eq!(catalog.total, 1);
    let _ = preview_id;
}

#[test]
fn preview_marks_nochange_rows_and_skips_undecodable_notes() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    seed_note(&h, "notes/weird.md", b"\x80\xbf"); // malformed UTF-8: strict decode fails
    let result = preview(&h.session, "alpha", "alpha");
    let CommandResult::ReplacePreviewSealed { manifest, .. } = expect_success(result, "preview")
    else {
        panic!("sealed");
    };
    let note = manifest
        .notes
        .iter()
        .find(|n| n.path == "notes/a.md")
        .expect("a");
    assert!(
        note.no_change,
        "replacement identical to source is NoChange"
    );
    assert!(
        manifest
            .exclusions
            .iter()
            .any(|e| e.path == "notes/weird.md")
    );
}

#[test]
fn draft_inclusion_is_an_explicit_opt_in_and_clean_lineage_is_recorded() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    open_to_terminal(&h.session, h.root.join("notes/a.md"));
    let result = preview(&h.session, "alpha", "X");
    let CommandResult::ReplacePreviewSealed { manifest, .. } = expect_success(result, "preview")
    else {
        panic!("sealed");
    };
    let note = &manifest.notes[0];
    assert!(
        note.open_lineage.is_some(),
        "clean open buffer records buffer lineage"
    );
    assert!(!manifest.include_drafts);
    assert!(note.draft_lineage.is_none());
    // Draft opt-in is honored since S09: the same open buffer, edited to
    // Dirty, becomes a Draft candidate carrying its exact lineage.
    let opened = open_to_terminal(&h.session, h.root.join("notes/a.md"));
    let (buffer_id, revision) = match expect_success(opened, "open") {
        CommandResult::NoteOpened {
            buffer_id,
            revision,
        } => (buffer_id, revision),
        other => panic!("{other:?}"),
    };
    let edited = expect_success(
        wait_terminal(
            &h.session,
            h.session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![flavite_buffer::EditOp::new(
                    flavite_syntax::text::Span::new(
                        flavite_syntax::text::TextByte(5),
                        flavite_syntax::text::TextByte(5),
                    ),
                    " more".to_owned(),
                )],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
                editor: None,
                operation: None,
            }),
        ),
        "dirty edit",
    );
    let dirty_revision = match edited {
        CommandResult::EditsApplied { new_revision, .. } => new_revision.as_u64(),
        other => panic!("{other:?}"),
    };
    let draft = wait_terminal(
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
    let CommandResult::ReplacePreviewSealed { manifest, .. } =
        expect_success(draft, "draft preview")
    else {
        panic!("sealed draft preview");
    };
    assert!(manifest.include_drafts);
    let draft_note = manifest
        .notes
        .iter()
        .find(|n| n.path == "notes/a.md")
        .expect("draft candidate");
    assert_eq!(
        draft_note.source_kind,
        flavite_vault::replace::SourceKind::Draft
    );
    let lineage = draft_note.draft_lineage.as_ref().expect("draft lineage");
    assert_eq!(lineage.buffer_id, buffer_id.as_u64());
    assert_eq!(lineage.preview_revision, dirty_revision);
    assert!(lineage.applied_revision.is_none());
    assert!(!lineage.checkpointed);
    // The draft's before payload is its serialized current content.
    assert_eq!(draft_note.before.len, 11);
    assert_eq!(draft_note.after.len, 7);
}

#[test]
fn selection_records_prefix_revision_and_digest() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    seed_note(&h, "notes/b.md", b"alpha\n");
    let result = preview(&h.session, "alpha", "X");
    let CommandResult::ReplacePreviewSealed {
        preview_id,
        manifest,
    } = expect_success(result, "preview")
    else {
        panic!("sealed");
    };
    // Select one note by native key.
    let key = manifest
        .notes
        .iter()
        .find(|n| n.path == "notes/a.md")
        .unwrap()
        .path_native
        .clone();
    let selected = expect_success(
        h.session.execute(SessionCommand::ReplaceSelect {
            preview_id,
            selected: ReplaceSelection::Keys(vec![key.clone()]),
        }),
        "select",
    );
    let CommandResult::ReplaceSelected {
        selection_revision,
        selection_digest,
    } = selected
    else {
        panic!("selected");
    };
    assert_eq!(selection_revision, 1);
    assert!(!selection_digest.is_empty());
    // Unknown key is refused.
    let unknown = h.session.execute(SessionCommand::ReplaceSelect {
        preview_id,
        selected: ReplaceSelection::Keys(vec!["deadbeef".to_owned()]),
    });
    assert!(matches!(unknown, SessionResult::Error { .. }));
    // All selection bumps the revision again.
    let all = expect_success(
        h.session.execute(SessionCommand::ReplaceSelect {
            preview_id,
            selected: ReplaceSelection::All,
        }),
        "select all",
    );
    let CommandResult::ReplaceSelected {
        selection_revision, ..
    } = all
    else {
        panic!("all");
    };
    assert_eq!(selection_revision, 2);
}

#[test]
fn unknown_finder_is_typed_expired_and_close_is_idempotent() {
    let h = harness();
    let page = h.session.execute(SessionCommand::SourceFindPage {
        finder_id: 1,
        note_index: 0,
        match_offset: 0,
        page_len: 50,
    });
    assert!(matches!(
        page,
        SessionResult::ActionRequired {
            code: "source-find-expired",
            ..
        }
    ));
    let closed = h
        .session
        .execute(SessionCommand::SourceFindClose { finder_id: 1 });
    assert!(matches!(
        closed,
        SessionResult::Success(_) | SessionResult::ActionRequired { .. }
    ));
}

#[test]
fn preview_without_helper_is_refused_typed_for_regex() {
    let h = harness();
    seed_note(&h, "notes/a.md", b"alpha\n");
    let result = wait_terminal(
        &h.session,
        h.session.execute(SessionCommand::ReplacePreviewPrepare {
            request_id: rid(),
            request: flavite_vault::replace::preview::PreviewRequest {
                query: "al.*a".to_owned(),
                replacement: "X".to_owned(),
                mode: flavite_text_search::matcher::MatchMode::Regex,
                case_sensitive: false,
                whole_word: false,
                include_drafts: false,
            },
        }),
    );
    match result {
        SessionResult::Error { message } => {
            // The regex helper is not installed in test environments; the
            // refusal is typed and never falls back to an in-process engine.
            assert!(
                message.contains("regex is unavailable")
                    || message.contains("invalid regular expression"),
                "typed refusal: {message}"
            );
        }
        SessionResult::Success(CommandResult::ReplacePreviewSealed { .. }) => {
            // A developer machine with the helper installed next to the test
            // binary is allowed to seal; the contract is the typed refusal
            // or a real helper round trip — never a silent in-process echo.
        }
        other => panic!("expected typed regex refusal, got {other:?}"),
    }
}
// probe appended to replace_preview.rs via feature flag
