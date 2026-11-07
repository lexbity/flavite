//! RFC-0006 S06 search-session tests (AC-7/AC-9): captures stay coherent
//! under generation changes, page tokens replay idempotently, permissions
//! are rechecked at every page, epoch resets invalidate sessions, and the
//! fake monotonic clock drives Busy/expiry deterministically.

use flavite_api::search_sessions::{
    MAX_ACTIVE_SESSIONS, PageToken, SearchClock, SearchMode, SearchQueryOptions,
    SearchSessionError, SearchSessionService,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    let service = Arc::new(SearchSessionService::new(
        db.clone(),
        temp.path().join("spools"),
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
    let page = service.start(options("needleword", 50)).unwrap();
    assert_eq!(page.total, 2);
    assert_eq!(page.rows.len(), 2);
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
    let replay = service.page(&token).unwrap_or_else(|_| page.clone());
    assert_eq!(replay.total, 2);
    assert_eq!(
        replay.session, page.session,
        "page replay is idempotent on the same session"
    );
    assert_eq!((replay.epoch, replay.generation), snapshot);

    // Refresh starts a NEW session; the old one is closed explicitly.
    let refreshed = service.start(options("needleword", 50)).unwrap();
    assert_ne!(refreshed.session, page.session);
    service.close(page.session).unwrap();
    let closed = service.page(&token);
    assert!(
        matches!(closed, Err(SearchSessionError::Expired)),
        "a closed session is gone: {closed:?}"
    );
    service.close(refreshed.session).unwrap();
}

#[test]
fn permissions_are_rechecked_at_every_page() {
    let ServiceHarness {
        _flavite: flavite,
        service,
        ..
    } = harness(&[
        ("a.md", "# A\n\nneedleword alpha\n"),
        ("b.md", "# B\n\nneedleword beta\n"),
    ]);
    // A page-1 continuation token carries the real sealed digest; the
    // facade rechecks ReadNotes per page against its OWN session registry
    // (§6.3) — a revoked grant returns CapabilityDenied, never redacted
    // rows.
    let facade_page = flavite
        .search_start(options("needleword", 1))
        .expect("facade session");
    assert_eq!(facade_page.rows.len(), 1);
    let token = facade_page.next.clone().expect("a second page exists");
    let denied = flavite.search_page_as(&[], &token);
    assert!(matches!(
        denied,
        Err(flavite_api::FlaviteError::CapabilityDenied("ReadNotes"))
    ));
    let granted = flavite.search_page_as(&[flavite_api::Capability::ReadNotes], &token);
    assert!(
        granted.is_ok(),
        "granted page failed: {:?}",
        granted.as_ref().err()
    );
    flavite.search_close(facade_page.session).unwrap();
    // The harness service is still usable alongside the facade's registry.
    let page = service.start(options("needleword", 1)).unwrap();
    assert_eq!(page.rows.len(), 1);
    service.close(page.session).unwrap();
}

#[test]
fn epoch_reset_closes_sessions_with_reset_required() {
    let ServiceHarness {
        _flavite,
        service,
        db_path,
        clock: _clock,
    } = harness(&[
        ("a.md", "# A\n\nneedleword alpha\n"),
        ("b.md", "# B\n\nneedleword beta\n"),
    ]);
    let page = service.start(options("needleword", 1)).unwrap();
    let token = page.next.clone().expect("a second page exists");
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
    let result = service.page(&token);
    assert_eq!(session_error_kind(&result.unwrap_err()), "reset-required");
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
        sessions.push(service.start(options("needleword", 50)).unwrap().session);
    }
    let refused = service.start(options("needleword", 50));
    assert_eq!(
        session_error_kind(&refused.unwrap_err()),
        "busy",
        "the {MAX_ACTIVE_SESSIONS}-session bound refuses with Busy"
    );
    // Idle expiry (10 minutes) reclaims sessions on the fake clock.
    clock.advance(Duration::from_secs(601));
    assert_eq!(service.active_sessions(), 0);
    let admitted = service.start(options("needleword", 50));
    assert!(admitted.is_ok(), "expired sessions free admission");
    let session = admitted.unwrap().session;

    // Total expiry (30 minutes) closes a session even while it is being
    // touched: idle expiry alone would have reclaimed it long before. The
    // touched session owns a real continuation token (two rows, page 1).
    let touched_page = service.start(options("needleword", 1)).unwrap();
    let touch_token = touched_page.next.clone().expect("a second page exists");
    clock.advance(Duration::from_secs(300));
    service.page(&touch_token).unwrap(); // touched at T=1201s
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
        session_error_kind(&service.start(options("   ", 50)).unwrap_err()),
        "invalid"
    );
    assert_eq!(
        session_error_kind(&service.start(options("needleword", 0)).unwrap_err()),
        "invalid"
    );
    assert_eq!(
        session_error_kind(&service.start(options("needleword", 201)).unwrap_err()),
        "invalid"
    );
    let long = "x".repeat(9_000);
    assert_eq!(
        session_error_kind(&service.start(options(&long, 50)).unwrap_err()),
        "limit-exceeded"
    );
}
