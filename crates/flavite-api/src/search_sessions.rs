//! Materialized saved-content search sessions (RFC-0006 S06, D-07, §6.3).
//!
//! Indexed expression search is a frozen **saved-content snapshot** per
//! search session, with immutable stable-order results and versioned
//! continuation. A session captures a coherent `(epoch, generation, G`
//! stamped searcher`)` snapshot through the ordered capture handshake, so
//! the result is coherent G or a typed Busy/Reset — never a mixed G/G+1
//! bundle. Results stream to a sealed private spool file; pages slice the
//! spool by ordinal and never rerun a query. Tokens bind the session, query
//! digest, snapshot version and ordinal; page replay is idempotent. Read
//! session permissions are intersected again at every page (§6.3). Expiry
//! is 10 idle minutes or 30 total minutes; an epoch reset closes sessions
//! with `ResetRequired`.

use crate::capability::Capability;
use blake3::Hasher;
use flavite_index::schema::{MetaKey, get_meta_u64};
use flavite_index::search_eval::{self, ScoredNote, Snapshot, Universe};
use flavite_index::search_session::{CaptureWaitError, SNAPSHOT_LIFETIME};
use flavite_vault::search_capture::SearchCaptureHandshake;
use flavite_vault::sync::MutexExt;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;

/// Default page size (S06: explicit page=50 default everywhere).
pub const DEFAULT_PAGE_SIZE: u64 = 50;
/// Minimum page size (D-07: allowed 1–200).
pub const MIN_PAGE_SIZE: u64 = 1;
/// Maximum page size (D-07).
pub const MAX_PAGE_SIZE: u64 = 200;
/// Active materialized sessions the service owns (NFR-10).
pub const MAX_ACTIVE_SESSIONS: usize = 4;
/// Idle expiry (D-07).
pub const IDLE_EXPIRY: Duration = Duration::from_secs(600);
/// Total expiry (D-07).
pub const TOTAL_EXPIRY: Duration = Duration::from_secs(1800);
/// Maximum submitted query bytes (NFR-12 bound shared with the UI).
pub const MAX_QUERY_BYTES: usize = 8192;
/// Sealed spool byte ceiling per session (NFR-7).
pub const MAX_SPOOL_BYTES_PER_SESSION: usize = 128 * 1024 * 1024;
/// Sealed spool byte ceiling across the service's live sessions (NFR-7).
pub const MAX_SPOOL_BYTES_TOTAL: usize = 512 * 1024 * 1024;

/// Search input mode (D-05/D-07): plain full-text (whitespace means AND)
/// versus the explicit expression syntax (operators, quotes, parentheses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchMode {
    FullText,
    Expression,
}

/// Query and paging options; part of the sealed query/options digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchQueryOptions {
    pub mode: SearchMode,
    pub text: String,
    pub page_size: u64,
}

impl SearchQueryOptions {
    /// Validate and clamp the options; page size bounds are enforced, the
    /// query length is bounded, and the digest binds mode+text+page size.
    ///
    /// # Errors
    /// Returns a typed error for empty/oversized text or out-of-range page
    /// sizes.
    pub fn validate(self) -> Result<Self, SearchSessionError> {
        let text = self.text.trim().to_owned();
        if text.is_empty() {
            return Err(SearchSessionError::Invalid {
                message: "Enter a search term".into(),
            });
        }
        if text.len() > MAX_QUERY_BYTES {
            return Err(SearchSessionError::LimitExceeded {
                kind: "query_bytes",
                observed: text.len(),
                limit: MAX_QUERY_BYTES,
                completeness: false,
            });
        }
        if !(MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&self.page_size) {
            return Err(SearchSessionError::Invalid {
                message: format!("page size must be {MIN_PAGE_SIZE}..={MAX_PAGE_SIZE}"),
            });
        }
        Ok(Self {
            mode: self.mode,
            text,
            page_size: self.page_size,
        })
    }

    fn digest(&self) -> [u8; 32] {
        let mut hasher = Hasher::new();
        hasher.update(b"flavite-search-options-v2");
        hasher.update(match self.mode {
            SearchMode::FullText => b"fulltext",
            SearchMode::Expression => b"expression",
        });
        hasher.update(self.text.as_bytes());
        hasher.update(&self.page_size.to_le_bytes());
        hasher.update(&flavite_index::PARSER_VERSION.to_le_bytes());
        *hasher.finalize().as_bytes()
    }
}

/// One materialized result row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchRow {
    pub note_id: i64,
    /// Vault-relative portable path.
    pub path: String,
    pub title: String,
    /// Relative-only relevance score; `None` for structural-only matches.
    pub score: Option<f32>,
    pub excerpt: String,
    /// Saved content hash of the source note (identity aid for consumers).
    pub content_hash: Vec<u8>,
}

/// Opaque service-chosen session identity (32 lowercase hex characters).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SearchSessionId(u128);

impl SearchSessionId {
    fn generate() -> Self {
        // OS randomness (uuid v4): the id is a lookup key, not authorization
        // (§6.1), but an unguessable id still keeps a co-resident process
        // from addressing another session by construction.
        Self(uuid::Uuid::new_v4().as_u128())
    }

    #[must_use]
    pub fn as_hex(&self) -> String {
        format!("{:032x}", self.0)
    }

    /// Deterministic constructor for hosts that re-derive a session id from
    /// persisted data (lookup key, not authorization, §6.1).
    #[doc(hidden)]
    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(value)
    }
}

/// Continuation token: binds the session, query/options digest, snapshot
/// version and next ordinal. Replay is idempotent (D-07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageToken {
    pub session: SearchSessionId,
    pub query_digest: [u8; 32],
    pub epoch: u64,
    pub generation: u64,
    pub next_ordinal: u64,
}

/// One materialized page.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPageMaterialized {
    pub session: SearchSessionId,
    pub epoch: u64,
    pub generation: u64,
    pub total: u64,
    pub rows: Vec<SearchRow>,
    pub next: Option<PageToken>,
    /// Earlier rows of the same frozen result remain addressable (the
    /// served page ordinal is past the first).
    pub older_available: bool,
}

/// Typed session failures (§6.1 result distinctions).
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SearchSessionError {
    #[error("busy: {0}")]
    Busy(&'static str),
    #[error("invalid request: {message}")]
    Invalid { message: String },
    #[error("resource limit exceeded: {kind} observed {observed} of {limit}")]
    LimitExceeded {
        kind: &'static str,
        observed: usize,
        limit: usize,
        /// A refused limit never yields a truncated "complete" page (D-07).
        completeness: bool,
    },
    #[error("the search snapshot was reset; start a new session")]
    ResetRequired,
    #[error("the search session expired or is gone")]
    Expired,
    #[error("missing capability: {0}")]
    Denied(&'static str),
    #[error("search failed: {0}")]
    Failed(String),
}

/// Monotonic clock seam (expiry uses a fake clock under test).
pub trait SearchClock: Send + Sync {
    fn now(&self) -> Instant;
}

/// System clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl SearchClock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// The capture handshake provider (the vault Session).
pub trait SearchCaptureProvider: Send + Sync {
    /// Begin one ordered capture handshake.
    ///
    /// # Errors
    /// Returns an error when the actor is not reachable.
    fn begin(&self) -> Result<SearchCaptureHandshake, String>;
}

/// Whether presented grants include `ReadNotes` — the only requirement a
/// search session imposes; the grants are re-intersected at every page
/// (§6.3), not just at start.
fn grants_cover(grants: &[Capability]) -> bool {
    grants
        .iter()
        .any(|grant| grant.covers(&Capability::ReadNotes, None))
}

#[derive(Debug)]
struct SessionEntry {
    options: SearchQueryOptions,
    query_digest: [u8; 32],
    epoch: u64,
    generation: u64,
    total: u64,
    spool: PathBuf,
    /// Sealed spool size in bytes (NFR-7 service-wide accounting).
    sealed_bytes: u64,
    /// The capability snapshot presented at start; every page re-verifies
    /// it still covers `ReadNotes` (§6.3).
    grants: Vec<Capability>,
    created: Instant,
    last_used: Instant,
}

/// Bounded registry of materialized search sessions with sealed spools.
pub struct SearchSessionService {
    state: std::sync::Mutex<State>,
    db_path: std::sync::RwLock<PathBuf>,
    spool_dir: PathBuf,
    capture: Arc<dyn SearchCaptureProvider>,
    clock: Arc<dyn SearchClock>,
    /// One cached read-only connection for the per-page epoch probe; a
    /// fresh SQLite open per page cost O(page) syscall overhead.
    epoch_probe: std::sync::Mutex<Option<Connection>>,
}

struct State {
    sessions: BTreeMap<SearchSessionId, SessionEntry>,
    /// Admission slots reserved by starts that are still capturing/sealing,
    /// so concurrent starts cannot exceed the session bound (NFR-10).
    pending: BTreeSet<SearchSessionId>,
    /// Sum of sealed spool bytes over live sessions (NFR-7 service quota).
    sealed_bytes: u64,
}

impl std::fmt::Debug for SearchSessionService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchSessionService")
            .field("spool_dir", &self.spool_dir)
            .finish_non_exhaustive()
    }
}

impl SearchSessionService {
    /// Create the service. Orphan spool files from previous runs are swept
    /// at startup (§6.3 cleanup rules); the spool directory is created
    /// best-effort and sealed lazily at first use.
    #[must_use]
    pub fn new(
        db_path: PathBuf,
        spool_dir: PathBuf,
        capture: Arc<dyn SearchCaptureProvider>,
        clock: Arc<dyn SearchClock>,
    ) -> Self {
        let _ = std::fs::create_dir_all(&spool_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&spool_dir, std::fs::Permissions::from_mode(0o700));
        }
        // Startup orphan sweep: no session survives a restart. Temp files
        // (`*.spool.tmp`) are swept alongside sealed spools.
        if let Ok(entries) = std::fs::read_dir(&spool_dir) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().contains(".spool") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Self {
            state: std::sync::Mutex::new(State {
                sessions: BTreeMap::new(),
                pending: BTreeSet::new(),
                sealed_bytes: 0,
            }),
            db_path: std::sync::RwLock::new(db_path),
            spool_dir,
            capture,
            clock,
            epoch_probe: std::sync::Mutex::new(None),
        }
    }

    /// Rebind the snapshot database path. The production default derives
    /// from the vault cache directory; test hosts that inject their own
    /// index database call this before the first capture.
    pub fn set_db_path(&self, path: PathBuf) {
        *self
            .db_path
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = path;
        // A replaced database invalidates the cached probe connection.
        *self.epoch_probe.lock_recovering() = None;
    }

    fn snapshot_db_path(&self) -> PathBuf {
        self.db_path
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Number of live (non-expired) sessions.
    #[must_use]
    pub fn active_sessions(&self) -> usize {
        let now = self.clock.now();
        let mut state = self.state.lock_recovering();
        self.reap(&mut state, now);
        state.sessions.len()
    }

    /// Start (or refresh) one session: capture a coherent snapshot,
    /// evaluate the complete AST, seal the result spool, and return the
    /// first page. Blocking; must run on a read worker, never on a UI or
    /// SQL-actor turn (D-07).
    ///
    /// # Errors
    /// Returns typed Busy/Denied/Invalid/Reset/Failed errors.
    pub fn start(
        &self,
        options: SearchQueryOptions,
        capabilities: &[Capability],
    ) -> Result<SearchPageMaterialized, SearchSessionError> {
        let options = options.validate()?;
        if !grants_cover(capabilities) {
            return Err(SearchSessionError::Denied("ReadNotes"));
        }
        let now = self.clock.now();
        // Reserve the admission slot atomically under the registry lock:
        // the reservation is pending until the spool seals, so concurrent
        // starts cannot exceed the session bound (NFR-10).
        let session_id = SearchSessionId::generate();
        {
            let mut state = self.state.lock_recovering();
            self.reap(&mut state, now);
            if state.sessions.len() + state.pending.len() >= MAX_ACTIVE_SESSIONS {
                return Err(SearchSessionError::Busy(
                    "search session limit reached; close or refresh a session",
                ));
            }
            state.pending.insert(session_id);
        }
        // Every error path below releases the reservation; only a sealed
        // session consumes it.
        struct SlotLease<'a> {
            service: &'a SearchSessionService,
            id: SearchSessionId,
            consumed: bool,
        }
        impl Drop for SlotLease<'_> {
            fn drop(&mut self) {
                if !self.consumed {
                    let mut state = self.service.state.lock_recovering();
                    state.pending.remove(&self.id);
                }
            }
        }
        let mut lease = SlotLease {
            service: self,
            id: session_id,
            consumed: false,
        };
        let query = match options.mode {
            SearchMode::FullText => flavite_index::Query::parse_typed(
                &options.text,
                flavite_index::query::QueryMode::Plain,
            ),
            SearchMode::Expression => flavite_index::Query::parse_typed(
                &options.text,
                flavite_index::query::QueryMode::Expression,
            ),
        }
        .map_err(|error| SearchSessionError::Invalid {
            message: error.to_string(),
        })?;
        // Capture: coherent (SQL snapshot at G, searcher stamped exactly G).
        let deadline = self.clock.now() + SNAPSHOT_LIFETIME;
        let handshake = self.capture.begin().map_err(SearchSessionError::Failed)?;
        handshake
            .wait_establish(deadline)
            .map_err(|_| SearchSessionError::Busy("search snapshot handshake"))?;
        let conn = Connection::open_with_flags(
            self.snapshot_db_path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|error| SearchSessionError::Failed(error.to_string()))?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|error| SearchSessionError::Failed(error.to_string()))?;
        let epoch = get_meta_u64(&tx, MetaKey::Epoch)
            .map_err(|error| SearchSessionError::Failed(error.to_string()))?
            .unwrap_or(1);
        let generation = get_meta_u64(&tx, MetaKey::Generation)
            .map_err(|error| SearchSessionError::Failed(error.to_string()))?
            .unwrap_or(0);
        handshake
            .ack
            .send((epoch, generation))
            .map_err(|_| SearchSessionError::Busy("search snapshot handshake"))?;
        let universe = Universe::from_tx(&tx).map_err(map_eval)?;
        let ticket = handshake
            .ticket
            .recv_timeout(deadline.saturating_duration_since(self.clock.now()))
            .map_err(|_| SearchSessionError::Busy("search snapshot handshake"))?
            .map_err(|message| SearchSessionError::Busy(handshake_busy_kind(&message)))?;
        let searcher = match ticket.wait(deadline) {
            Ok(searcher) => searcher,
            Err(CaptureWaitError::ResetRequired) => return Err(SearchSessionError::ResetRequired),
            Err(CaptureWaitError::Timeout) => {
                return Err(SearchSessionError::Busy("searcher capture"));
            }
            Err(CaptureWaitError::Disconnected) => {
                return Err(SearchSessionError::ResetRequired);
            }
        };
        // Evaluate the complete supported AST over the snapshot.
        let snapshot = Snapshot {
            universe: &universe,
            searcher: &searcher,
            tx: &tx,
        };
        let scored = search_eval::evaluate(&query, &snapshot).map_err(map_eval)?;
        let anchor_terms = flavite_index::text_terms(&query);
        let rows =
            materialize_rows(&tx, &scored, &anchor_terms).map_err(SearchSessionError::Failed)?;
        drop(tx);
        drop(conn);
        // Seal the spool before the session becomes visible.
        let digest = options.digest();
        let sealed = seal_spool(
            &self.spool_dir,
            &session_id,
            &digest,
            epoch,
            generation,
            &rows,
        )
        .map_err(|error| match error {
            SealError::Quota { observed, limit } => SearchSessionError::LimitExceeded {
                kind: "spool_bytes",
                observed,
                limit,
                completeness: false,
            },
            SealError::Failed(message) => SearchSessionError::Failed(message),
        })?;
        let total = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        // Page 0 is served from the in-memory rows at seal time; the spool
        // is read only for continuation pages.
        let first_rows = rows
            .into_iter()
            .take(usize::try_from(options.page_size).unwrap_or(0))
            .collect();
        let first = self.slice_at(&session_id, digest, epoch, generation, 0, first_rows, total);
        // NFR-7 service-wide quota: refuse (deleting the fresh spool) when
        // admitting this session would exceed the total sealed budget.
        let mut state = self.state.lock_recovering();
        state.pending.remove(&session_id);
        let projected = state.sealed_bytes.saturating_add(sealed.sealed_bytes);
        if projected > MAX_SPOOL_BYTES_TOTAL as u64 {
            lease.consumed = true;
            drop(state);
            let _ = std::fs::remove_file(&sealed.path);
            return Err(SearchSessionError::LimitExceeded {
                kind: "spool_total_bytes",
                observed: usize::try_from(projected).unwrap_or(usize::MAX),
                limit: MAX_SPOOL_BYTES_TOTAL,
                completeness: false,
            });
        }
        state.sealed_bytes = projected;
        state.sessions.insert(
            session_id,
            SessionEntry {
                options,
                query_digest: digest,
                epoch,
                generation,
                total,
                spool: sealed.path,
                sealed_bytes: sealed.sealed_bytes,
                grants: capabilities.to_owned(),
                created: now,
                last_used: now,
            },
        );
        drop(state);
        lease.consumed = true;
        debug_assert_eq!(first.generation, generation);
        Ok(first)
    }

    /// Fetch one page by token. Page replay is idempotent (D-07); read
    /// session permissions are intersected again at every page (§6.3): the
    /// presented grants and the session's stored snapshot must both cover
    /// `ReadNotes`.
    ///
    /// # Errors
    /// Returns typed Expired/Reset/Denied/Invalid errors.
    pub fn page(
        &self,
        token: &PageToken,
        capabilities: &[Capability],
    ) -> Result<SearchPageMaterialized, SearchSessionError> {
        if !grants_cover(capabilities) {
            return Err(SearchSessionError::Denied("ReadNotes"));
        }
        let now = self.clock.now();
        let (page_size, query_digest, spool, total, epoch) = {
            let mut state = self.state.lock_recovering();
            self.reap(&mut state, now);
            let Some(entry) = state.sessions.get(&token.session) else {
                return Err(SearchSessionError::Expired);
            };
            if entry.query_digest != token.query_digest
                || (entry.epoch, entry.generation) != (token.epoch, token.generation)
            {
                return Err(SearchSessionError::Expired);
            }
            // The stored snapshot still covers ReadNotes (§6.3).
            if !grants_cover(&entry.grants) {
                return Err(SearchSessionError::Denied("ReadNotes"));
            }
            if token.next_ordinal >= entry.total {
                return Err(SearchSessionError::Invalid {
                    message: "page ordinal is outside the materialized total".into(),
                });
            }
            (
                entry.options.page_size,
                entry.query_digest,
                entry.spool.clone(),
                entry.total,
                entry.epoch,
            )
        };
        // The epoch is rechecked against the current database outside the
        // registry lock (one cached read-only connection): an epoch reset
        // closes the session — entry and spool are removed — with
        // ResetRequired (D-07).
        let current_epoch = self.current_epoch().map_err(SearchSessionError::Failed)?;
        if current_epoch != epoch {
            let mut state = self.state.lock_recovering();
            self.remove_session(&mut state, token.session);
            return Err(SearchSessionError::ResetRequired);
        }
        let rows = match read_spool_rows(&spool, token.next_ordinal, page_size) {
            Ok(rows) => rows,
            // A gone spool file means the session is gone, not corrupt.
            Err(SpoolReadError::NotFound) => return Err(SearchSessionError::Expired),
            Err(SpoolReadError::Corrupt(message)) => {
                return Err(SearchSessionError::Failed(message));
            }
        };
        {
            let mut state = self.state.lock_recovering();
            if let Some(entry) = state.sessions.get_mut(&token.session) {
                entry.last_used = now;
            }
        }
        Ok(self.slice_at(
            &token.session,
            query_digest,
            epoch,
            token.generation,
            token.next_ordinal,
            rows,
            total,
        ))
    }

    /// Explicitly close one session and remove its spool.
    ///
    /// # Errors
    /// Returns `Expired` when the session is already gone.
    pub fn close(
        &self,
        capabilities: &[Capability],
        session: SearchSessionId,
    ) -> Result<(), SearchSessionError> {
        if !grants_cover(capabilities) {
            return Err(SearchSessionError::Denied("ReadNotes"));
        }
        let mut state = self.state.lock_recovering();
        if self.remove_session(&mut state, session) {
            Ok(())
        } else {
            Err(SearchSessionError::Expired)
        }
    }

    /// Remove one session entry, release its spool bytes from the service
    /// quota, and delete the spool file.
    fn remove_session(&self, state: &mut State, session: SearchSessionId) -> bool {
        match state.sessions.remove(&session) {
            Some(entry) => {
                state.sealed_bytes = state.sealed_bytes.saturating_sub(entry.sealed_bytes);
                let _ = std::fs::remove_file(&entry.spool);
                true
            }
            None => false,
        }
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "one page slice over the sealed snapshot's own identity fields"
    )]
    fn slice_at(
        &self,
        session: &SearchSessionId,
        query_digest: [u8; 32],
        epoch: u64,
        generation: u64,
        ordinal: u64,
        rows: Vec<SearchRow>,
        total: u64,
    ) -> SearchPageMaterialized {
        let next_ordinal = ordinal.saturating_add(u64::try_from(rows.len()).unwrap_or(0));
        let next = (next_ordinal < total).then_some(PageToken {
            session: *session,
            query_digest,
            epoch,
            generation,
            next_ordinal,
        });
        SearchPageMaterialized {
            session: *session,
            epoch,
            generation,
            total,
            rows,
            next,
            older_available: ordinal > 0,
        }
    }

    fn reap(&self, state: &mut State, now: Instant) {
        let expired: Vec<SearchSessionId> = state
            .sessions
            .iter()
            .filter(|(_, entry)| {
                now.duration_since(entry.last_used) >= IDLE_EXPIRY
                    || now.duration_since(entry.created) >= TOTAL_EXPIRY
            })
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            self.remove_session(state, id);
        }
    }

    /// The snapshot database's current epoch through one cached read-only
    /// connection. A probe error reopens once; a persistently broken
    /// database still fails the page.
    fn current_epoch(&self) -> Result<u64, String> {
        let mut probe = self.epoch_probe.lock_recovering();
        if let Some(conn) = probe.as_ref() {
            match get_meta_u64(conn, MetaKey::Epoch) {
                Ok(value) => return Ok(value.unwrap_or(1)),
                // The cached handle broke (replaced or locked database);
                // reopen once below instead of failing the page.
                Err(_) => *probe = None,
            }
        }
        let conn = Connection::open_with_flags(
            self.snapshot_db_path(),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .map_err(|error| error.to_string())?;
        let value = get_meta_u64(&conn, MetaKey::Epoch)
            .map_err(|error| error.to_string())?
            .unwrap_or(1);
        *probe = Some(conn);
        Ok(value)
    }
}

fn map_eval(error: search_eval::EvalError) -> SearchSessionError {
    match error {
        search_eval::EvalError::LimitExceeded {
            kind,
            observed,
            limit,
            // The evaluator refuses the whole query — no truncated page is
            // ever sealed — so the served result is never complete.
            completeness: _,
        } => SearchSessionError::LimitExceeded {
            kind,
            observed,
            limit,
            completeness: false,
        },
        other => SearchSessionError::Failed(other.to_string()),
    }
}

/// The vault actor's capture refusal is a plain string — the actor boundary
/// (`SearchCaptureHandshake::ticket`) carries no typed refusal variants, so
/// the Busy-vs-admission distinction relies on the documented "timed out"
/// substring. Centralized here so the sniff lives in exactly one place; if
/// the actor grows typed refusals, map structurally instead.
fn handshake_busy_kind(message: &str) -> &'static str {
    if message.contains("timed out") {
        "search snapshot handshake"
    } else {
        "capture admission"
    }
}

/// Assemble bounded display rows for the scored results from one read
/// transaction (batched per 512 ids; excerpts come from the indexed
/// saved-body cache; dirty buffer text is never included, D-08).
fn materialize_rows(
    tx: &Connection,
    scored: &[ScoredNote],
    anchor_terms: &[String],
) -> Result<Vec<SearchRow>, String> {
    fn row_error(error: rusqlite::Error) -> String {
        error.to_string()
    }
    const ROW_CHUNK: usize = 512;
    /// Excerpt bound (§6.1 DTO): at most 512 UTF-8 bytes.
    const MAX_EXCERPT_BYTES: usize = 512;
    /// Title bound (§6.1 DTO): at most 4 KiB.
    const MAX_TITLE_BYTES: usize = 4 * 1024;
    let mut found: std::collections::HashMap<i64, (String, String, Option<String>, Vec<u8>)> =
        std::collections::HashMap::new();
    for chunk in scored.chunks(ROW_CHUNK) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT n.note_id, n.path, n.title, d.body_text, n.content_hash \
             FROM notes n LEFT JOIN search_docs d ON d.note_id = n.note_id \
             WHERE n.note_id IN ({placeholders})"
        );
        let mut statement = tx.prepare(&sql).map_err(row_error)?;
        let rows = statement
            .query_map(
                rusqlite::params_from_iter(chunk.iter().map(|row| row.note_id)),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<Vec<u8>>>(4)?,
                    ))
                },
            )
            .map_err(row_error)?;
        for row in rows {
            let (note_id, path, title, body, hash) = row.map_err(row_error)?;
            found.insert(note_id, (path, title, body, hash.unwrap_or_default()));
        }
    }
    let mut rows = Vec::with_capacity(scored.len());
    for entry in scored {
        let Some((path, title, body, hash)) = found.get(&entry.note_id) else {
            continue;
        };
        let excerpt = flavite_index::pool::build_excerpt(body.as_deref(), anchor_terms);
        rows.push(SearchRow {
            note_id: entry.note_id,
            path: path.clone(),
            title: clip_utf8(title, MAX_TITLE_BYTES),
            score: entry.score,
            excerpt: clip_utf8(&excerpt, MAX_EXCERPT_BYTES),
            content_hash: hash.clone(),
        });
    }
    Ok(rows)
}

/// Clip to `max` UTF-8 bytes at a char boundary (§6.1 DTO bounds); a cut
/// never splits a character.
fn clip_utf8(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_owned();
    }
    let mut cut = max;
    while !value.is_char_boundary(cut) {
        cut -= 1;
    }
    value[..cut].to_owned()
}

// ---------------------------------------------------------------------------
// Sealed spool files (§6.3): schema 2, private device-cache files.
//
// Byte layout (all integers little-endian):
//   magic         [u8; 8]   `FLVSPL02`
//   header_check  [u8; 32]  blake3 over the header bytes
//   header_len    u32
//   header        JSON object: schema=2, session, query_digest, epoch,
//                 generation, total, chunk_rows, index_offset, index_len,
//                 index_check (64 lowercase hex chars)
//   chunks        concatenated JSON row arrays with NO inline framing: a
//                 chunk's extent and declared row count live only in the
//                 checksummed index (framing is authenticated by the index,
//                 per-chunk payload integrity by the record's payload hash)
//   index         one record per chunk: offset u64, rows u32, payload
//                 blake3 [u8; 32]
//
// The header authenticates the index byte-for-byte through `index_check`,
// so the reader verifies header + index first and then seeks directly to
// the chunks intersecting the requested ordinal window — O(page) per page,
// never a full-file rescan. Partial spool data is never a page.
// ---------------------------------------------------------------------------

const SPOOL_MAGIC: &[u8; 8] = b"FLVSPL02";
const SPOOL_CHUNK_ROWS: usize = 50;
/// magic + header checksum + header length.
const SPOOL_PREAMBLE: usize = 8 + 32 + 4;
/// One index record: offset u64 + rows u32 + payload blake3.
const SPOOL_RECORD_BYTES: usize = 8 + 4 + 32;

fn spool_path(dir: &Path, session: &SearchSessionId) -> PathBuf {
    dir.join(format!("{}.spool", session.as_hex()))
}

struct SealedSpool {
    path: PathBuf,
    sealed_bytes: u64,
}

enum SealError {
    Quota { observed: usize, limit: usize },
    Failed(String),
}

fn seal_spool(
    dir: &Path,
    session: &SearchSessionId,
    digest: &[u8; 32],
    epoch: u64,
    generation: u64,
    rows: &[SearchRow],
) -> Result<SealedSpool, SealError> {
    use std::io::Write;
    // Serialize every chunk up front: chunk extents — and therefore the
    // index and header — are known before any byte is written, and a quota
    // breach is refused without creating a file at all.
    let mut payloads: Vec<Vec<u8>> = Vec::with_capacity(rows.len().div_ceil(SPOOL_CHUNK_ROWS));
    let mut payload_bytes = 0usize;
    for chunk in rows.chunks(SPOOL_CHUNK_ROWS) {
        let payload =
            serde_json::to_vec(chunk).map_err(|error| SealError::Failed(error.to_string()))?;
        payload_bytes = payload_bytes.saturating_add(payload.len());
        if payload_bytes > MAX_SPOOL_BYTES_PER_SESSION {
            return Err(SealError::Quota {
                observed: payload_bytes,
                limit: MAX_SPOOL_BYTES_PER_SESSION,
            });
        }
        payloads.push(payload);
    }
    // The header embeds `index_offset` and the index checksum, both of
    // which depend on the header's own byte length; iterate to the fixed
    // point (the checksum is fixed-width hex, so the digit count stabilizes
    // after at most a couple of rounds).
    let mut header_len = SPOOL_PREAMBLE;
    let (header, header_checksum, index, _) = loop {
        // Layout: preamble | header | payload region | index. Chunk offsets
        // start at the payload region, not at the index that follows it.
        let payload_start = SPOOL_PREAMBLE + header_len;
        let offset = u64::try_from(payload_start + payload_bytes)
            .map_err(|_| SealError::Failed("spool layout overflows u64".into()))?;
        // Index records: chunk offset, declared row count, payload hash.
        let mut candidate_index = Vec::with_capacity(payloads.len() * SPOOL_RECORD_BYTES);
        let mut chunk_offset = u64::try_from(payload_start)
            .map_err(|_| SealError::Failed("spool layout overflows u64".into()))?;
        for (chunk, payload) in rows.chunks(SPOOL_CHUNK_ROWS).zip(&payloads) {
            candidate_index.extend_from_slice(&chunk_offset.to_le_bytes());
            candidate_index.extend_from_slice(
                &u32::try_from(chunk.len())
                    .map_err(|_| SealError::Failed("chunk row count overflows u32".into()))?
                    .to_le_bytes(),
            );
            candidate_index.extend_from_slice(blake3::hash(payload).as_bytes());
            chunk_offset += u64::try_from(payload.len())
                .map_err(|_| SealError::Failed("spool layout overflows u64".into()))?;
        }
        let index_checksum = blake3::hash(&candidate_index);
        let candidate = serde_json::json!({
            "schema": 2,
            "session": session.as_hex(),
            "query_digest": hex::encode(digest),
            "epoch": epoch,
            "generation": generation,
            "total": rows.len(),
            "chunk_rows": SPOOL_CHUNK_ROWS,
            "index_offset": offset,
            "index_len": payloads.len() * SPOOL_RECORD_BYTES,
            "index_check": hex::encode(index_checksum.as_bytes()),
        })
        .to_string();
        if candidate.len() == header_len {
            let header_checksum = blake3::hash(candidate.as_bytes());
            break (candidate, header_checksum, candidate_index, offset);
        }
        header_len = candidate.len();
    };
    let path = spool_path(dir, session);
    let temp = path.with_extension("spool.tmp");
    let file =
        std::fs::File::create(&temp).map_err(|error| SealError::Failed(error.to_string()))?;
    // Restrict the spool before any content byte exists on disk.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| SealError::Failed(error.to_string()))?;
    }
    let mut writer = std::io::BufWriter::new(file);
    writer
        .write_all(SPOOL_MAGIC)
        .map_err(|error| SealError::Failed(error.to_string()))?;
    writer
        .write_all(header_checksum.as_bytes())
        .map_err(|error| SealError::Failed(error.to_string()))?;
    writer
        .write_all(
            &u32::try_from(header.len())
                .map_err(|_| SealError::Failed("spool header overflows u32".into()))?
                .to_le_bytes(),
        )
        .map_err(|error| SealError::Failed(error.to_string()))?;
    writer
        .write_all(header.as_bytes())
        .map_err(|error| SealError::Failed(error.to_string()))?;
    for payload in &payloads {
        writer
            .write_all(payload)
            .map_err(|error| SealError::Failed(error.to_string()))?;
    }
    writer
        .write_all(&index)
        .map_err(|error| SealError::Failed(error.to_string()))?;
    writer
        .flush()
        .map_err(|error| SealError::Failed(error.to_string()))?;
    let file = writer
        .into_inner()
        .map_err(|error| SealError::Failed(error.to_string()))?;
    file.sync_all()
        .map_err(|error| SealError::Failed(error.to_string()))?;
    std::fs::rename(&temp, &path).map_err(|error| SealError::Failed(error.to_string()))?;
    let sealed_bytes = u64::try_from(SPOOL_PREAMBLE + header.len() + payload_bytes + index.len())
        .map_err(|_| SealError::Failed("spool size overflows u64".into()))?;
    Ok(SealedSpool { path, sealed_bytes })
}

/// Why a spool read failed. A missing file means the session is gone, not
/// corrupt.
enum SpoolReadError {
    NotFound,
    Corrupt(String),
}

/// Read rows `[ordinal, ordinal + limit)` from a sealed spool: verify the
/// header and the whole checksummed index, then seek directly to the chunks
/// intersecting the requested ordinal window, verifying each chunk payload
/// against its index record. Partial spool data is never a page.
fn read_spool_rows(
    path: &Path,
    ordinal: u64,
    limit: u64,
) -> Result<Vec<SearchRow>, SpoolReadError> {
    use std::io::{Read, Seek, SeekFrom};
    let corrupt = |message: &str| SpoolReadError::Corrupt(message.to_owned());
    let mut file = std::fs::File::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            SpoolReadError::NotFound
        } else {
            SpoolReadError::Corrupt(error.to_string())
        }
    })?;
    let file_len = file
        .metadata()
        .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?
        .len();
    if file_len < SPOOL_PREAMBLE as u64 {
        return Err(corrupt("spool file is truncated"));
    }
    let mut preamble = [0u8; SPOOL_PREAMBLE];
    file.read_exact(&mut preamble)
        .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?;
    if &preamble[..8] != SPOOL_MAGIC {
        return Err(corrupt("spool file has an unknown format"));
    }
    let header_len = usize::try_from(u32::from_le_bytes(
        preamble[40..44].try_into().expect("4 bytes"),
    ))
    .map_err(|_| corrupt("spool header length overflows"))?;
    if SPOOL_PREAMBLE + header_len > usize::try_from(file_len).unwrap_or(usize::MAX) {
        return Err(corrupt("spool header is truncated"));
    }
    let mut header_bytes = vec![0u8; header_len];
    file.read_exact(&mut header_bytes)
        .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?;
    if blake3::hash(&header_bytes).as_bytes().as_slice() != &preamble[8..40] {
        return Err(corrupt("spool header checksum mismatch"));
    }
    let header: serde_json::Value = serde_json::from_slice(&header_bytes)
        .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?;
    if header.get("schema").and_then(serde_json::Value::as_u64) != Some(2) {
        return Err(corrupt("unsupported spool schema"));
    }
    let Some(total) = header.get("total").and_then(serde_json::Value::as_u64) else {
        return Err(corrupt("spool header lacks a total"));
    };
    let Some(chunk_rows) = header.get("chunk_rows").and_then(serde_json::Value::as_u64) else {
        return Err(corrupt("spool header lacks a chunk size"));
    };
    if chunk_rows == 0 {
        return Err(corrupt("spool chunk size is zero"));
    }
    let Some(index_offset) = header
        .get("index_offset")
        .and_then(serde_json::Value::as_u64)
    else {
        return Err(corrupt("spool header lacks an index offset"));
    };
    let Some(index_len) = header.get("index_len").and_then(serde_json::Value::as_u64) else {
        return Err(corrupt("spool header lacks an index length"));
    };
    let index_check = header
        .get("index_check")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| hex::decode(value).ok())
        .filter(|bytes| bytes.len() == 32)
        .ok_or_else(|| corrupt("spool header lacks an index checksum"))?;
    let records = usize::try_from(index_len / SPOOL_RECORD_BYTES as u64)
        .map_err(|_| corrupt("spool index length overflows"))?;
    if index_len % SPOOL_RECORD_BYTES as u64 != 0
        || records != usize::try_from(total.div_ceil(chunk_rows)).unwrap_or(usize::MAX)
    {
        return Err(corrupt(
            "spool index record count disagrees with the header total",
        ));
    }
    if index_offset < (SPOOL_PREAMBLE + header_len) as u64
        || index_offset
            .checked_add(index_len)
            .is_none_or(|end| end > file_len)
    {
        return Err(corrupt("spool index is outside the file"));
    }
    // Verify the whole checksummed index before touching any chunk.
    file.seek(SeekFrom::Start(index_offset))
        .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?;
    let mut index = vec![0u8; usize::try_from(index_len).unwrap_or(usize::MAX)];
    file.read_exact(&mut index)
        .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?;
    if blake3::hash(&index).as_bytes().as_slice() != index_check.as_slice() {
        return Err(corrupt("spool index checksum mismatch"));
    }
    if ordinal >= total {
        return Ok(Vec::new());
    }
    // Chunks intersecting [ordinal, ordinal + limit): contiguous by
    // construction, located directly through the verified index.
    let want_end = ordinal.saturating_add(limit);
    let window_end = want_end.min(total);
    let first_chunk = usize::try_from(ordinal / chunk_rows).unwrap_or(usize::MAX);
    let last_chunk = usize::try_from((window_end - 1) / chunk_rows).unwrap_or(usize::MAX);
    if last_chunk >= records {
        return Err(corrupt("spool index is missing requested chunks"));
    }
    let mut result: Vec<SearchRow> = Vec::new();
    for record in first_chunk..=last_chunk {
        let base = record * SPOOL_RECORD_BYTES;
        let chunk_offset = u64::from_le_bytes(index[base..base + 8].try_into().expect("8 bytes"));
        let declared_rows =
            u32::from_le_bytes(index[base + 8..base + 12].try_into().expect("4 bytes"));
        let payload_check = &index[base + 12..base + SPOOL_RECORD_BYTES];
        let chunk_end = if record + 1 < records {
            u64::from_le_bytes(
                index[base + SPOOL_RECORD_BYTES..base + SPOOL_RECORD_BYTES + 8]
                    .try_into()
                    .expect("8 bytes"),
            )
        } else {
            index_offset
        };
        if chunk_offset < (SPOOL_PREAMBLE + header_len) as u64
            || chunk_end > index_offset
            || chunk_end <= chunk_offset
        {
            return Err(corrupt("spool chunk extent is outside the chunk region"));
        }
        let payload_len = usize::try_from(chunk_end - chunk_offset)
            .map_err(|_| corrupt("spool chunk overflows"))?;
        file.seek(SeekFrom::Start(chunk_offset))
            .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?;
        let mut payload = vec![0u8; payload_len];
        file.read_exact(&mut payload)
            .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?;
        if blake3::hash(&payload).as_bytes().as_slice() != payload_check {
            return Err(corrupt("spool chunk checksum mismatch"));
        }
        let mut chunk_payload: Vec<SearchRow> = serde_json::from_slice(&payload)
            .map_err(|error| SpoolReadError::Corrupt(error.to_string()))?;
        if u64::try_from(chunk_payload.len()).unwrap_or(u64::MAX) != u64::from(declared_rows) {
            return Err(corrupt("spool chunk row count mismatch"));
        }
        // Slice this chunk's overlap with the requested window; every
        // record in the selected range overlaps it by construction.
        let chunk_start = u64::try_from(record)
            .map_err(|_| corrupt("spool chunk ordinal overflows"))?
            .saturating_mul(chunk_rows);
        let chunk_end_row = chunk_start.saturating_add(u64::from(declared_rows));
        let skip = usize::try_from(ordinal.saturating_sub(chunk_start)).unwrap_or(0);
        let take = usize::try_from(
            window_end
                .min(chunk_end_row)
                .saturating_sub(ordinal.max(chunk_start)),
        )
        .unwrap_or(0);
        result.extend(chunk_payload.drain(..).skip(skip).take(take));
    }
    let want = usize::try_from(window_end - ordinal).unwrap_or(0);
    if result.len() != want {
        return Err(corrupt("spool is missing requested rows"));
    }
    Ok(result)
}
