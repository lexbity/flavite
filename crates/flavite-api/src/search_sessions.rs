//! Materialized saved-content search sessions (RFC-0006 S06, D-07, §6.3).
//!
//! Indexed expression search is a frozen **saved-content snapshot** per
//! search session, with immutable stable-order results and versioned
//! continuation. A session captures a coherent `(epoch, generation, G`
//! stamped searcher`)` snapshot through the ordered capture handshake, so
//! the result is coherent G or a typed Busy/Reset — never a mixed G/G+1
//! bundle. Results stream to a sealed private spool file; pages slice the
//! spool by ordinal and never rerun a query. Tokens bind the session, query
//! digest, snapshot version and ordinal; page replay is idempotent. Expiry
//! is 10 idle minutes or 30 total minutes; an epoch reset closes sessions
//! with `ResetRequired`.

use blake3::Hasher;
use flavite_index::schema::{MetaKey, get_meta_u64};
use flavite_index::search_eval::{self, ScoredNote, Snapshot, Universe};
use flavite_index::search_session::{CaptureWaitError, SNAPSHOT_LIFETIME};
use flavite_vault::search_capture::SearchCaptureHandshake;
use flavite_vault::sync::MutexExt;
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
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

/// Search input mode (D-05/D-07): plain full-text (whitespace means AND)
/// versus the explicit expression syntax (operators, quotes, parentheses).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SearchMode {
    FullText,
    Expression,
}

/// Query and paging options; part of the sealed query/options digest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
        let bytes = *blake3::hash(&rand_bytes()).as_bytes();
        let mut value = [0u8; 16];
        value.copy_from_slice(&bytes[..16]);
        Self(u128::from_le_bytes(value))
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

fn rand_bytes() -> [u8; 16] {
    // Non-cryptographic uniqueness source: identity is a lookup key, not
    // authorization (§6.1). The process counter plus time is sufficient and
    // deterministic under tests.
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&COUNTER.fetch_add(1, Ordering::AcqRel).to_le_bytes());
    bytes[8..].copy_from_slice(
        &std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_le_bytes()[..8],
    );
    bytes
}

/// Continuation token: binds the session, query/options digest, snapshot
/// version and next ordinal. Replay is idempotent (D-07).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    },
    #[error("the search snapshot was reset; start a new session")]
    ResetRequired,
    #[error("the search session expired or is gone")]
    Expired,
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

#[derive(Debug)]
struct SessionEntry {
    options: SearchQueryOptions,
    query_digest: [u8; 32],
    epoch: u64,
    generation: u64,
    total: u64,
    spool: PathBuf,
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
}

struct State {
    sessions: BTreeMap<SearchSessionId, SessionEntry>,
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
        // Startup orphan sweep: no session survives a restart.
        if let Ok(entries) = std::fs::read_dir(&spool_dir) {
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|ext| ext == "spool") {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        Self {
            state: std::sync::Mutex::new(State {
                sessions: BTreeMap::new(),
            }),
            db_path: std::sync::RwLock::new(db_path),
            spool_dir,
            capture,
            clock,
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
    /// Returns typed Busy/Invalid/Reset/Failed errors.
    pub fn start(
        &self,
        options: SearchQueryOptions,
    ) -> Result<SearchPageMaterialized, SearchSessionError> {
        let options = options.validate()?;
        let now = self.clock.now();
        {
            let mut state = self.state.lock_recovering();
            self.reap(&mut state, now);
            if state.sessions.len() >= MAX_ACTIVE_SESSIONS {
                return Err(SearchSessionError::Busy(
                    "search session limit reached; close or refresh a session",
                ));
            }
        }
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
            .map_err(|message| {
                SearchSessionError::Busy(if message.contains("timed out") {
                    "search snapshot handshake"
                } else {
                    "capture admission"
                })
            })?;
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
        let session_id = SearchSessionId::generate();
        let digest = options.digest();
        let spool = seal_spool(
            &self.spool_dir,
            &session_id,
            &digest,
            epoch,
            generation,
            &rows,
        )
        .map_err(SearchSessionError::Failed)?;
        let total = u64::try_from(rows.len()).unwrap_or(u64::MAX);
        {
            let mut state = self.state.lock_recovering();
            state.sessions.insert(
                session_id,
                SessionEntry {
                    options: options.clone(),
                    query_digest: digest,
                    epoch,
                    generation,
                    total,
                    spool,
                    created: now,
                    last_used: now,
                },
            );
        }
        let first = self.slice(&session_id, 0);
        debug_assert_eq!(first.generation, generation);
        Ok(first)
    }

    /// Fetch one page by token. Page replay is idempotent; permissions are
    /// rechecked by the caller (facade) at every page (§6.3).
    ///
    /// # Errors
    /// Returns typed Expired/Reset/Invalid errors.
    pub fn page(&self, token: &PageToken) -> Result<SearchPageMaterialized, SearchSessionError> {
        let now = self.clock.now();
        let (options, query_digest, spool, total, epoch) = {
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
            if token.next_ordinal >= entry.total {
                return Err(SearchSessionError::Invalid {
                    message: "page ordinal is outside the materialized total".into(),
                });
            }
            // The epoch is rechecked against the current database: an epoch
            // reset closes sessions with ResetRequired (D-07).
            let current_epoch =
                current_epoch(&self.snapshot_db_path()).map_err(SearchSessionError::Failed)?;
            if current_epoch != entry.epoch {
                return Err(SearchSessionError::ResetRequired);
            }
            (
                entry.options.clone(),
                entry.query_digest,
                entry.spool.clone(),
                entry.total,
                entry.epoch,
            )
        };
        let rows = read_spool_rows(&spool, token.next_ordinal, options.page_size)
            .map_err(SearchSessionError::Failed)?;
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
    pub fn close(&self, session: SearchSessionId) -> Result<(), SearchSessionError> {
        let mut state = self.state.lock_recovering();
        match state.sessions.remove(&session) {
            Some(entry) => {
                let _ = std::fs::remove_file(&entry.spool);
                Ok(())
            }
            None => Err(SearchSessionError::Expired),
        }
    }

    fn slice(&self, session: &SearchSessionId, ordinal: u64) -> SearchPageMaterialized {
        let (page_size, query_digest, epoch, generation, total) = {
            let state = self.state.lock_recovering();
            match state.sessions.get(session) {
                Some(entry) => (
                    entry.options.page_size,
                    entry.query_digest,
                    entry.epoch,
                    entry.generation,
                    entry.total,
                ),
                None => return self.empty_page(*session),
            }
        };
        let rows = read_spool_rows(&spool_path(&self.spool_dir, session), ordinal, page_size)
            .unwrap_or_default();
        self.slice_at(
            session,
            query_digest,
            epoch,
            generation,
            ordinal,
            rows,
            total,
        )
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
        }
    }

    fn empty_page(&self, session: SearchSessionId) -> SearchPageMaterialized {
        SearchPageMaterialized {
            session,
            epoch: 0,
            generation: 0,
            total: 0,
            rows: Vec::new(),
            next: None,
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
            if let Some(entry) = state.sessions.remove(&id) {
                let _ = std::fs::remove_file(&entry.spool);
            }
        }
    }
}

fn map_eval(error: search_eval::EvalError) -> SearchSessionError {
    match error {
        search_eval::EvalError::LimitExceeded {
            kind,
            observed,
            limit,
        } => SearchSessionError::LimitExceeded {
            kind,
            observed,
            limit,
        },
        other => SearchSessionError::Failed(other.to_string()),
    }
}

fn current_epoch(db_path: &Path) -> Result<u64, String> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .map_err(|error| error.to_string())?;
    Ok(get_meta_u64(&conn, MetaKey::Epoch)
        .map_err(|error| error.to_string())?
        .unwrap_or(1))
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
            title: title.clone(),
            score: entry.score,
            excerpt,
            content_hash: hash.clone(),
        });
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// Sealed spool files (§6.3): private device-cache files, schema 1, with a
// checksummed header/query digest/version and numbered chunks. Partial spool
// data is never a page.
// ---------------------------------------------------------------------------

const SPOOL_MAGIC: &[u8; 8] = b"FLVSPL01";
const SPOOL_CHUNK_ROWS: usize = 50;

fn spool_path(dir: &Path, session: &SearchSessionId) -> PathBuf {
    dir.join(format!("{}.spool", session.as_hex()))
}

fn seal_spool(
    dir: &Path,
    session: &SearchSessionId,
    digest: &[u8; 32],
    epoch: u64,
    generation: u64,
    rows: &[SearchRow],
) -> Result<PathBuf, String> {
    use std::io::Write;
    let path = spool_path(dir, session);
    let temp = path.with_extension("spool.tmp");
    let mut header = serde_json::json!({
        "schema": 1,
        "session": session.as_hex(),
        "query_digest": hex::encode(digest),
        "epoch": epoch,
        "generation": generation,
        "total": rows.len(),
    })
    .to_string();
    header.push('\n');
    let header_checksum = blake3::hash(header.as_bytes());
    let file = std::fs::File::create(&temp).map_err(|error| error.to_string())?;
    let mut writer = std::io::BufWriter::new(file);
    writer
        .write_all(SPOOL_MAGIC)
        .map_err(|error| error.to_string())?;
    writer
        .write_all(header_checksum.as_bytes())
        .map_err(|error| error.to_string())?;
    writer
        .write_all(header.as_bytes())
        .map_err(|error| error.to_string())?;
    for chunk in rows.chunks(SPOOL_CHUNK_ROWS) {
        let payload = serde_json::to_vec(chunk).map_err(|error| error.to_string())?;
        writer
            .write_all(&(u32::try_from(payload.len()).unwrap_or(u32::MAX)).to_le_bytes())
            .map_err(|error| error.to_string())?;
        writer
            .write_all(blake3::hash(&payload).as_bytes())
            .map_err(|error| error.to_string())?;
        writer
            .write_all(&payload)
            .map_err(|error| error.to_string())?;
    }
    writer.flush().map_err(|error| error.to_string())?;
    let file = writer.into_inner().map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())?;
    std::fs::rename(&temp, &path).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(path)
}

/// Read rows `[ordinal, ordinal + limit)` from a sealed spool, verifying the
/// header and every chunk checksum along the way. Partial spool data is
/// never a page.
fn read_spool_rows(path: &Path, ordinal: u64, limit: u64) -> Result<Vec<SearchRow>, String> {
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    if bytes.len() < 8 + 32 {
        return Err("spool file is truncated".into());
    }
    if &bytes[..8] != SPOOL_MAGIC {
        return Err("spool file has an unknown format".into());
    }
    let header_end = bytes[8 + 32..]
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or("spool header is truncated")?
        + 8
        + 32
        + 1; // include the terminating newline the checksum covers
    let header_bytes = &bytes[8 + 32..header_end];
    if blake3::hash(header_bytes).as_bytes().as_slice() != &bytes[8..8 + 32] {
        return Err("spool header checksum mismatch".into());
    }
    let header: serde_json::Value =
        serde_json::from_slice(header_bytes).map_err(|error| error.to_string())?;
    if header.get("schema").and_then(serde_json::Value::as_u64) != Some(1) {
        return Err("unsupported spool schema".into());
    }
    // Stream the numbered chunks, collecting the requested window.
    let mut cursor = header_end;
    let mut result: Vec<SearchRow> = Vec::new();
    let mut ordinal_cursor = 0u64;
    let want_end = ordinal.saturating_add(limit);
    while cursor < bytes.len() {
        if cursor + 4 + 32 > bytes.len() {
            return Err("spool chunk header is truncated".into());
        }
        let len = u32::from_le_bytes([
            bytes[cursor],
            bytes[cursor + 1],
            bytes[cursor + 2],
            bytes[cursor + 3],
        ]) as usize;
        cursor += 4;
        let checksum = &bytes[cursor..cursor + 32];
        cursor += 32;
        let payload_end = cursor
            .checked_add(len)
            .ok_or("spool chunk length overflows")?;
        if payload_end > bytes.len() {
            return Err("spool chunk payload is truncated".into());
        }
        let payload = &bytes[cursor..payload_end];
        if blake3::hash(payload).as_bytes().as_slice() != checksum {
            return Err("spool chunk checksum mismatch".into());
        }
        cursor = payload_end;
        let chunk_rows: Vec<SearchRow> =
            serde_json::from_slice(payload).map_err(|error| error.to_string())?;
        let chunk_len = u64::try_from(chunk_rows.len()).unwrap_or(0);
        let chunk_end = ordinal_cursor + chunk_len;
        if chunk_end > ordinal && ordinal_cursor < want_end {
            let skip = ordinal.saturating_sub(ordinal_cursor) as usize;
            let take = (want_end.min(chunk_end) - ordinal.max(ordinal_cursor)) as usize;
            result.extend(chunk_rows.into_iter().skip(skip).take(take));
        }
        ordinal_cursor = chunk_end;
        if ordinal_cursor >= want_end {
            break;
        }
    }
    if ordinal_cursor < want_end && want_end <= total_of(&header) {
        return Err("spool is missing requested rows".into());
    }
    Ok(result)
}

fn total_of(header: &serde_json::Value) -> u64 {
    header
        .get("total")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}
