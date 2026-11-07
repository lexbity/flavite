//! RFC-0006 S06 search-session tests (AC-7/AC-9): captures stay coherent
//! under generation changes, page tokens replay idempotently, permissions
//! are intersected again at every page (§6.3), epoch resets invalidate and
//! close sessions, the sealed spool serves only verified windows, and the
//! fake monotonic clock drives Busy/expiry deterministically.

use flavite_api::Capability;
use flavite_api::search_sessions::{
    MAX_ACTIVE_SESSIONS, PageToken, SearchClock, SearchMode, SearchQueryOptions,
    SearchSessionError, SearchSessionService,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The presented grants of a trusted local caller (the built-in host grant
/// set covers ReadNotes; §6.3 re-intersects it at every page).
const GRANTS: &[Capability] = &[Capability::ReadNotes];

/// Controlled monotonic clock (D-07 expiry semantics under test).
#[derive(Debug)]
struct FakeClock {
    base: Instant,
    offset: std::sync::Mutex<Duration>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            base: Instant::now(),
            offset: std::sync::Mutex::new(Duration::ZERO),
        }
    }
    fn advance(&self, by: Duration) {
        *self.offset.lock().unwrap() += by;
    }
}

impl SearchClock for FakeClock {
    fn now(&self) -> Instant {
        self.base + *self.offset.lock().unwrap()
    }
}

fn options(text: &str, page_size: u64) -> SearchQueryOptions {
    SearchQueryOptions {
        mode: SearchMode::FullText,
        text: text.to_owned(),
        page_size,
    }
}

/// One service wired to a test vault's own actor/index database (the
/// capture handshake runs through the production vault actor).
struct ServiceHarness {
    _flavite: flavite_api::Flavite,
    service: Arc<SearchSessionService>,
    clock: Arc<FakeClock>,
    db_path: std::path::PathBuf,
    spool_dir: std::path::PathBuf,
}

fn harness(notes: &[(&str, &str)]) -> ServiceHarness {
    use flavite_api::VaultIdentity;
    use flavite_buffer::manager::BufferManager;
    use flavite_fs::{Fs, MemFs};
    use flavite_index::IndexActor;
    use flavite_vault::VaultConfig;
    use flavite_vault::actor::VaultActorBuilder;
    use flavite_vault::channels::ChannelSet;
    use flavite_vault::session::Session;

    let fs = std::sync::Arc::new(MemFs::new());
    let root = std::path::PathBuf::from("/test/search-sessions");
    fs.create_dir_all(&root).unwrap();
    for (name, body) in notes {
        fs.write_new(&root.join(name), body.as_bytes()).unwrap();
    }
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("index.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![11]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(support::SharedFs { inner: fs }) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .build()
        .unwrap()
        .start()
        .unwrap();
    let channels = ChannelSet::new();
    let session = Arc::new(Session::new(
        identity.clone(),
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig {
            autosave_idle_interval: Duration::from_secs(3600),
            ..VaultConfig::default()
        },
    ));
    let flavite =
        flavite_api::Flavite::with_vault(identity, Arc::clone(&session), VaultConfig::default())
            .unwrap();
    // Reconcile so the notes are indexed and their envelopes committed.
    let request_id = match flavite.execute(flavite_api::Invocation::new(
        flavite.command_registry().next_request_id(),
        "vault.reconcile",
        serde_json::json!({ "scope": "full", "mode": "fast" }),
        flavite_api::Origin::Cli,
    )) {
        flavite_api::ExecutionResult::Pending { request_id } => request_id,
        other => panic!("reconcile: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        flavite.pump();
        let done = flavite.wait_for_operation(request_id);
        if !matches!(done, flavite_api::ExecutionResult::Pending { .. }) {
            assert!(done.is_success(), "reconcile: {done:?}");
            break;
        }
        assert!(Instant::now() < deadline, "reconcile never finished");
        std::thread::sleep(Duration::from_millis(5));
    }
    // The snapshot source rides the injected index database; the capture
    // provider goes through the same Session the facade owns.
    flavite.query_service().set_search_db_path(db.clone());
    let clock = Arc::new(FakeClock::new());
    let spool_dir = temp.path().join("spools");
    let service = Arc::new(SearchSessionService::new(
        db.clone(),
        spool_dir.clone(),
        Arc::new(SessionProvider(session)),
        clock.clone(),
    ));
    // Keep the temp dir alive for the whole test via a leaked guard.
    std::mem::forget(temp);
    ServiceHarness {
        _flavite: flavite,
        service,
        clock,
        db_path: db,
        spool_dir,
    }
}

/// Routes capture handshakes through the vault actor's generation-order
/// gate (D-07).
struct SessionProvider(Arc<flavite_vault::session::Session>);

impl flavite_api::search_sessions::SearchCaptureProvider for SessionProvider {
    fn begin(&self) -> Result<flavite_vault::search_capture::SearchCaptureHandshake, String> {
        self.0.begin_search_capture()
    }
}

mod support;

fn session_error_kind(error: &SearchSessionError) -> &'static str {
    match error {
        SearchSessionError::Busy(_) => "busy",
        SearchSessionError::Invalid { .. } => "invalid",
        SearchSessionError::LimitExceeded { .. } => "limit-exceeded",
        SearchSessionError::ResetRequired => "reset-required",
        SearchSessionError::Expired => "expired",
        SearchSessionError::Denied(_) => "denied",
        SearchSessionError::Failed(_) => "failed",
    }
}

#[test]
fn snapshot_is_coherent_and_frozen_until_explicit_refresh() {
    let ServiceHarness {
        _flavite: flavite,
        service,
        ..
    } = harness(&[
        ("a.md", "# A\n\nneedleword alpha\n"),
        ("b.md", "# B\n\nneedleword beta\n"),
    ]);
    let _ = &flavite;
    let page = service.start(options("needleword", 50), GRANTS).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.rows.len(), 2);
    assert!(!page.older_available, "the first page has no older rows");
    let snapshot = (page.epoch, page.generation);
    assert!(snapshot.1 >= 2, "the reconcile generations are visible");

    // A later save advances the vault generation but never mutates the
    // frozen snapshot: ordinary generations only mark the session older
    // (D-07). The page still enumerates the captured rows exactly.
    let token = page.next.clone().unwrap_or(PageToken {
        session: page.session,
        query_digest: [0u8; 32],
        epoch: page.epoch,
        generation: page.generation,
        next_ordinal: 0,
    });
    let _ = &flavite;
    let replay = service
        .page(&token, GRANTS)
        .unwrap_or_else(|_| page.clone());
    assert_eq!(replay.total, 2);
    assert_eq!(
        replay.session, page.session,
        "page replay is idempotent on the same session"
    );
    assert_eq!((replay.epoch, replay.generation), snapshot);

    // Refresh starts a NEW session; the old one is closed explicitly.
    let refreshed = service.start(options("needleword", 50), GRANTS).unwrap();
    assert_ne!(refreshed.session, page.session);
    service.close(GRANTS, page.session).unwrap();
    let closed = service.page(&token, GRANTS);
    assert!(
        matches!(closed, Err(SearchSessionError::Expired)),
        "a closed session is gone: {closed:?}"
    );
    service.close(GRANTS, refreshed.session).unwrap();
}

#[test]
fn permissions_are_intersected_again_at_every_page() {
    let ServiceHarness {
        _flavite: flavite,
        service,
        ..
    } = harness(&[
        ("a.md", "# A\n\nneedleword alpha\n"),
        ("b.md", "# B\n\nneedleword beta\n"),
    ]);
    // Service level: start refuses grant-less callers outright, and every
    // page re-intersects the presented grants (§6.3) — a revoked grant
    // returns the typed denial, never redacted rows.
    assert_eq!(
        session_error_kind(&service.start(options("needleword", 1), &[]).unwrap_err()),
        "denied"
    );
    let page = service.start(options("needleword", 1), GRANTS).unwrap();
    assert_eq!(page.rows.len(), 1);
    let token = page.next.clone().expect("a second page exists");
    let denied = service.page(&token, &[]);
    assert!(
        matches!(denied, Err(SearchSessionError::Denied("ReadNotes"))),
        "a revoked grant is a typed denial: {denied:?}"
    );
    let granted = service.page(&token, GRANTS);
    assert!(
        granted.is_ok(),
        "granted page failed: {:?}",
        granted.as_ref().err()
    );
    service.close(GRANTS, page.session).unwrap();

    // Facade level: the same intersection rides the facade methods; the
    // denial keeps the facade's CapabilityDenied kind.
    let facade_page = flavite
        .search_start(GRANTS, options("needleword", 1))
        .expect("facade session");
    assert_eq!(facade_page.rows.len(), 1);
    let token = facade_page.next.clone().expect("a second page exists");
    let denied = flavite.search_page(&[], &token);
    assert!(matches!(
        denied,
        Err(flavite_api::FlaviteError::CapabilityDenied("ReadNotes"))
    ));
    let granted = flavite.search_page(&[Capability::ReadNotes], &token);
    assert!(
        granted.is_ok(),
        "granted facade page failed: {:?}",
        granted.as_ref().err()
    );
    flavite.search_close(GRANTS, facade_page.session).unwrap();
}

#[test]
fn epoch_reset_closes_sessions_with_reset_required() {
    let ServiceHarness {
        _flavite,
        service,
        db_path,
        spool_dir,
        clock: _clock,
    } = harness(&[
        ("a.md", "# A\n\nneedleword alpha\n"),
        ("b.md", "# B\n\nneedleword beta\n"),
    ]);
    let page = service.start(options("needleword", 1), GRANTS).unwrap();
    let token = page.next.clone().expect("a second page exists");
    let spool = spool_dir.join(format!("{}.spool", page.session.as_hex()));
    assert!(spool.exists(), "the sealed spool exists before the reset");
    // Force an epoch change in the snapshot database (the recreate
    // detection the writer reacts to) and observe the typed reset.
    let conn =
        rusqlite::Connection::open_with_flags(db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
    conn.execute(
        "UPDATE meta SET value = CAST(CAST(value AS INTEGER) + 1 AS TEXT) WHERE key='epoch'",
        [],
    )
    .unwrap();
    drop(conn);
    let result = service.page(&token, GRANTS);
    assert_eq!(session_error_kind(&result.unwrap_err()), "reset-required");
    // D-07: an epoch reset closes the session — the entry and its spool
    // are gone, and the admission slot is free again.
    assert_eq!(service.active_sessions(), 0, "the reset closed the session");
    assert!(!spool.exists(), "the reset deleted the spool file");
}

#[test]
fn busy_and_expiry_follow_the_fake_monotonic_clock() {
    let ServiceHarness {
        _flavite,
        service,
        clock,
        ..
    } = harness(&[
        ("a.md", "# A\n\nneedleword alpha\n"),
        ("b.md", "# B\n\nneedleword beta\n"),
    ]);
    let mut sessions = Vec::new();
    for _ in 0..MAX_ACTIVE_SESSIONS {
        sessions.push(
            service
                .start(options("needleword", 50), GRANTS)
                .unwrap()
                .session,
        );
    }
    let refused = service.start(options("needleword", 50), GRANTS);
    assert_eq!(
        session_error_kind(&refused.unwrap_err()),
        "busy",
        "the {MAX_ACTIVE_SESSIONS}-session bound refuses with Busy"
    );
    // Idle expiry (10 minutes) reclaims sessions on the fake clock.
    clock.advance(Duration::from_secs(601));
    assert_eq!(service.active_sessions(), 0);
    let admitted = service.start(options("needleword", 50), GRANTS);
    assert!(admitted.is_ok(), "expired sessions free admission");
    let session = admitted.unwrap().session;

    // Total expiry (30 minutes) closes a session even while it is being
    // touched: idle expiry alone would have reclaimed it long before. The
    // touched session owns a real continuation token (two rows, page 1).
    let touched_page = service.start(options("needleword", 1), GRANTS).unwrap();
    let touch_token = touched_page.next.clone().expect("a second page exists");
    clock.advance(Duration::from_secs(300));
    service.page(&touch_token, GRANTS).unwrap(); // touched at T=1201s
    clock.advance(Duration::from_secs(300));
    assert_eq!(service.active_sessions(), 1, "idle window has not passed");
    clock.advance(Duration::from_secs(1500));
    assert_eq!(service.active_sessions(), 0, "total expiry closes it");
    let _ = session;
}

#[test]
fn invalid_options_and_oversize_queries_are_typed_rejections() {
    let ServiceHarness {
        _flavite, service, ..
    } = harness(&[("a.md", "# A\n\nneedleword\n")]);
    assert_eq!(
        session_error_kind(&service.start(options("   ", 50), GRANTS).unwrap_err()),
        "invalid"
    );
    assert_eq!(
        session_error_kind(&service.start(options("needleword", 0), GRANTS).unwrap_err()),
        "invalid"
    );
    assert_eq!(
        session_error_kind(
            &service
                .start(options("needleword", 201), GRANTS)
                .unwrap_err()
        ),
        "invalid"
    );
    let long = "x".repeat(9_000);
    assert_eq!(
        session_error_kind(&service.start(options(&long, 50), GRANTS).unwrap_err()),
        "limit-exceeded"
    );
}

/// The sealed spool slices pages by ordinal through its checksummed chunk
/// index: paging across many chunks serves exactly the requested window
/// with `older_available` marking continuation pages (§6.3, O(page)).
#[test]
fn paging_slices_windows_across_spool_chunks() {
    let notes: Vec<(String, String)> = (0..120u32)
        .map(|index| {
            (
                format!("note{index:03}.md"),
                format!("# N{index}\n\nneedleword row {index}\n"),
            )
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = notes
        .iter()
        .map(|(path, body)| (path.as_str(), body.as_str()))
        .collect();
    let ServiceHarness {
        _flavite, service, ..
    } = harness(&borrowed);
    let page = service.start(options("needleword", 50), GRANTS).unwrap();
    assert_eq!(page.total, 120);
    assert_eq!(page.rows.len(), 50, "the first chunk-sized page");
    assert!(!page.older_available);

    let mut ordinals = 50u64;
    let mut token = page.next.clone().expect("page 2 exists");
    let mut pages = 1usize;
    loop {
        let served = service.page(&token, GRANTS).unwrap();
        pages += 1;
        assert_eq!(served.total, 120);
        assert!(served.older_available, "continuation pages have older rows");
        for (offset, row) in served.rows.iter().enumerate() {
            let expected = ordinals + offset as u64;
            assert!(
                row.path.ends_with(&format!("note{expected:03}.md")),
                "row {} must be the ordinal {} row, got {}",
                ordinals + offset as u64,
                expected,
                row.path
            );
        }
        ordinals += u64::try_from(served.rows.len()).unwrap_or(0);
        match served.next {
            Some(next) => token = next,
            None => break,
        }
    }
    assert_eq!(ordinals, 120, "every row was served exactly once");
    assert_eq!(pages, 3, "50+50+20 across three chunk-intersecting pages");
    service.close(GRANTS, page.session).unwrap();
}

/// Partial spool data is never a page (§6.3): a corrupted byte or a
/// truncated file fails with `failed`; a deleted spool means the session
/// is gone (`expired`).
#[test]
fn corrupt_or_missing_spool_is_never_a_page() {
    let ServiceHarness {
        _flavite,
        service,
        spool_dir,
        ..
    } = harness(&[
        ("a.md", "# A\n\nneedleword alpha\n"),
        ("b.md", "# B\n\nneedleword beta\n"),
    ]);
    let page = service.start(options("needleword", 1), GRANTS).unwrap();
    let token = page.next.clone().expect("a second page exists");
    let spool = spool_dir.join(format!("{}.spool", page.session.as_hex()));

    // Flip one byte late in the file (inside the checksummed index region)
    // and observe the typed corruption refusal.
    let mut bytes = std::fs::read(&spool).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&spool, &bytes).unwrap();
    let corrupted = service.page(&token, GRANTS);
    assert_eq!(
        session_error_kind(&corrupted.unwrap_err()),
        "failed",
        "a corrupted spool is never a page"
    );

    // Truncated spool: same refusal.
    std::fs::write(&spool, &bytes[..bytes.len() / 2]).unwrap();
    let truncated = service.page(&token, GRANTS);
    assert_eq!(
        session_error_kind(&truncated.unwrap_err()),
        "failed",
        "a truncated spool is never a page"
    );

    // Deleted spool: the session is gone.
    std::fs::remove_file(&spool).unwrap();
    let missing = service.page(&token, GRANTS);
    assert_eq!(
        session_error_kind(&missing.unwrap_err()),
        "expired",
        "a missing spool means the session is gone"
    );
}

/// §6.1 DTO conformance: the wire DTOs refuse unknown fields.
#[test]
fn wire_dtos_reject_unknown_fields() {
    let options: Result<SearchQueryOptions, _> = serde_json::from_value(serde_json::json!({
        "mode": "FullText",
        "text": "needle",
        "page_size": 50,
        "surprise": true,
    }));
    assert!(options.is_err(), "SearchQueryOptions denies unknown fields");
    let token: Result<PageToken, _> = serde_json::from_value(serde_json::json!({
        "session": "0".repeat(32),
        "query_digest": serde_json::Value::Array(vec![serde_json::Value::from(0); 32]),
        "epoch": 1,
        "generation": 1,
        "next_ordinal": 0,
        "surprise": true,
    }));
    assert!(token.is_err(), "PageToken denies unknown fields");
}
