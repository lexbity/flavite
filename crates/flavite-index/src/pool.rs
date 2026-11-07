//! Connection pool for SQLite (D3).
//!
//! This module provides:
//! - A single writer connection (owns the SQLite writer connection and
//!   tantivy `IndexWriter`)
//! - A pool of read-only WAL connections for concurrent reads
//!
//! The pool size is based on S4-2's recommendations: default 4 connections,
//! hard cap 8.

use crate::changelog::{ChangeKind, ChangeLogWriter};
use crate::resolver::{
    AttImage, EntityPre, NoteImage, Resolver, ResolverDiagnostics, ResolverState,
    affected_resolution_keys, group_winner_in_tx, identity_relevant, refresh_id_conflict_groups,
    resolve_affected, target_stem_key,
};
use crate::schema::{AttachmentKind, MetaKey, get_meta_u64};
use crate::search::TantivyBackend;
use crate::search_writer::{SearchDocOp, SearchEnvelope, SearchWriterHandle, WriterReport};
use crate::signal::GenerationSignal;
use crate::types::IndexVersion;
use blake3::Hash;
use flavite_syntax::text::TextSnapshot;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Result, Transaction};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::Path;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

/// Default pool size (from S4-2 recommendations).
pub const DEFAULT_POOL_SIZE: usize = 4;

/// Hard cap on pool size (from S4-2 recommendations).
pub const MAX_POOL_SIZE: usize = 8;

/// Connection pool for read-only WAL connections.
#[derive(Debug)]
pub struct ReadPool {
    /// The database path.
    db_path: String,
    /// Available connections.
    available: Mutex<VecDeque<Connection>>,
    /// Connections in use.
    in_use: Mutex<usize>,
    /// Maximum pool size.
    max_size: usize,
    /// Condvar for waiting on connections.
    condvar: Condvar,
    /// Time when the pool was created.
    created_at: Instant,
}

/// Writer connection (exclusive access).
#[derive(Debug)]
pub struct Writer {
    conn: Connection,
}

/// The index actor owns both the writer and the read pool.
#[derive(Debug)]
pub struct IndexActor {
    writer: Writer,
    read_pool: ReadPool,
    /// Tantivy read side (searcher capture, checkpoint publication). All
    /// writes belong to the single ordered writer below (RFC-0006 S06).
    /// The one bounded ordered Tantivy writer owner (D-07). `None` marks a
    /// secondary handle on an index whose writer belongs to another actor
    /// in this process or another process: SQL applies still work (the
    /// owning writer catches up), and the tantivy directory is never
    /// deleted out from under the owner.
    search_writer: Option<SearchWriterHandle>,
    needs_reparse: bool,
    resolver: ResolverState,
    /// RFC-0005 S1: the writer-side generation broadcast. `None` keeps the
    /// actor signal-less (CLI/test pools); `Some` publishes every committed
    /// structural generation from the apply spine's commit sites (FR-1).
    signal: Option<GenerationSignal>,
}

/// Build one bounded excerpt around the first occurrence of any term.
///
/// Matching is case-insensitive on the saved body; the excerpt is
/// character-bounded (not byte-bounded) so multi-byte text is never split.
#[must_use]
pub fn build_excerpt(body: Option<&str>, terms: &[String]) -> String {
    let Some(body) = body.filter(|body| !body.is_empty()) else {
        return String::new();
    };
    let Some(anchor) = terms
        .iter()
        .filter_map(|term| {
            if term.is_empty() {
                return None;
            }
            find_case_insensitive(body, term)
        })
        .min()
    else {
        // No matched term found in the cached body (tokenization drift):
        // show the document head instead of an empty excerpt.
        return body.chars().take(EXCERPT_MAX_CHARS).collect();
    };
    let anchor = anchor.min(body.len());
    // Start at most one third of the excerpt budget before the match, on a
    // char boundary, so the excerpt opens with context.
    let before_anchor = &body[..anchor];
    let context = EXCERPT_MAX_CHARS / 3;
    let total = before_anchor.chars().count();
    let skip = total.saturating_sub(context);
    let start = before_anchor
        .char_indices()
        .nth(skip)
        .map_or(0, |(index, _)| index);
    let excerpt = &body[start..];
    excerpt.chars().take(EXCERPT_MAX_CHARS).collect()
}

/// Byte offset of the first case-insensitive occurrence of `needle`.
///
/// Walks char boundaries so multi-byte text is never split mid-char; the
/// fold comparison is per-char, which stays correct for ASCII terms and
/// one-char-per-char-folded Unicode alike.
fn find_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return None;
    }
    // Same per-char fold comparison as before, without collecting the whole
    // body into a char table first: excerpt anchoring runs per result row.
    for (start_byte, _) in haystack.char_indices() {
        let mut hay_chars = haystack[start_byte..].chars();
        let mut matched = true;
        for expected in needle.chars() {
            match hay_chars.next() {
                Some(actual) if actual.to_lowercase().eq(expected.to_lowercase()) => {}
                _ => {
                    matched = false;
                    break;
                }
            }
        }
        if matched {
            return Some(start_byte);
        }
    }
    None
}

/// Collect the distinct text-leaf terms of a query for excerpt anchoring.
#[must_use]
pub fn text_terms(query: &crate::query::Query) -> Vec<String> {
    match query {
        crate::query::Query::Text(leaf) => vec![leaf.term.clone()],
        crate::query::Query::And(parts) | crate::query::Query::Or(parts) => {
            let mut terms = Vec::new();
            for part in parts {
                terms.extend(text_terms(part));
            }
            terms
        }
        crate::query::Query::Not(inner) => text_terms(inner),
        _ => Vec::new(),
    }
}

/// Text-search results paired with the structural index version used to
/// fetch them. `has_more` is the pool-computed sentinel (RFC-0004 §4.4/§6.3,
/// D11): true exactly when a `limit+1`'th row exists — the assembly fetched
/// `limit+1` and reports whether it filled the probe slot.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchPage {
    /// Structural epoch.
    pub epoch: u64,
    /// Structural generation.
    pub generation: u64,
    /// Matching note ID, vault-relative path, title, and (text-search only)
    /// relative relevance score rows, in hit order.
    pub items: Vec<SearchNote>,
    /// True when more matching rows exist beyond `items` (which holds at
    /// most the requested `limit`).
    pub has_more: bool,
}

/// One matching note from a text or hybrid query (W10: with a bounded
/// saved-content excerpt around the first matched term).
#[derive(Debug, Clone, PartialEq)]
pub struct SearchNote {
    /// Indexed note ID (valid within the returned epoch).
    pub note_id: i64,
    /// Vault-relative portable path.
    pub path: String,
    /// Indexed title.
    pub title: String,
    /// Bounded saved-content excerpt, or empty for structural-only results.
    /// Dirty buffer text is never included (D-08).
    pub excerpt: String,
    /// Tantivy relative relevance score, descending in `SearchPage::items`
    /// order. `None` for structural-only pages. Relative-only (D11): never
    /// persisted, never compared across queries; not exposed across the
    /// facade until a consumer needs it.
    pub score: Option<f32>,
}

/// Excerpt length budget per result (§7.5-style bounded display text).
const EXCERPT_MAX_CHARS: usize = 160;

/// One catalog entry: the portable path string, the raw native path bytes
/// (authoritative identity), and bounded indexed completion metadata.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NoteCatalogEntry {
    /// Portable vault-relative path (display and join purposes).
    pub path: String,
    /// Exact native path bytes as stored by the index.
    pub path_native: Vec<u8>,
    /// Indexed note title.
    pub title: String,
    /// Indexed aliases, in canonical alias-key order.
    pub aliases: Vec<String>,
    /// Stable ID, omitted when conflicted.
    pub stable_id: Option<String>,
}

/// Continuation bound to one immutable catalog version and native ordering.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct NoteCatalogCursor {
    pub epoch: u64,
    pub generation: u64,
    pub native_path: Vec<u8>,
    pub note_id: i64,
}

/// A bounded catalog page with the index generation it came from.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NoteCatalogPage {
    pub next: Option<NoteCatalogCursor>,
    /// Entries in canonical path order, at most the requested limit.
    pub items: Vec<NoteCatalogEntry>,
    /// Index epoch.
    pub epoch: u64,
    /// Index generation.
    pub generation: u64,
    /// Total indexed notes, which may exceed the page length.
    pub total: u64,
}

/// Indexed source span for one inbound link used by journaled note renames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundLinkSpan {
    pub source_path_native: Vec<u8>,
    pub source_hash: [u8; 32],
    pub span_start: u64,
    pub span_end: u64,
    pub target_raw: String,
}

/// Pool error type.
#[derive(Debug, Error)]
pub enum PoolError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Pool exhausted: {0} connections in use, max {1}")]
    Exhausted(usize, usize),

    #[error("Connection timeout after {0:?}")]
    Timeout(Duration),

    #[error("Mutex poisoned")]
    Poisoned,
}

impl ReadPool {
    /// Create a new read pool.
    ///
    /// # Errors
    /// Returns a SQLite error if any read-only connection cannot be opened.
    pub fn new(db_path: &Path, size: usize) -> Result<Self> {
        let size = size.clamp(1, MAX_POOL_SIZE);
        let db_path = db_path.to_string_lossy().to_string();

        // Pre-populate with connections
        let mut available = VecDeque::with_capacity(size);
        for _ in 0..size {
            let conn = Self::open_read_only(&db_path)?;
            available.push_back(conn);
        }

        Ok(Self {
            db_path,
            available: Mutex::new(available),
            in_use: Mutex::new(0),
            max_size: size,
            condvar: Condvar::new(),
            created_at: Instant::now(),
        })
    }

    /// Open a read-only WAL connection.
    fn open_read_only(path: &str) -> Result<Connection> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )?;

        // Enable WAL mode (read-only connections can use WAL)
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Set read-only mode
        conn.pragma_update(None, "query_only", "true")?;

        Ok(conn)
    }

    /// Get a connection from the pool.
    ///
    /// This blocks until a connection is available or the timeout is reached.
    ///
    /// # Errors
    /// Returns a pool error if the pool is poisoned, exhausted, or times out.
    pub fn get(&self, timeout: Option<Duration>) -> Result<PooledConnection<'_>, PoolError> {
        let start = Instant::now();

        let mut available = self.available.lock().map_err(|_| PoolError::Poisoned)?;
        let mut in_use = self.in_use.lock().map_err(|_| PoolError::Poisoned)?;

        // Try to get an available connection
        if let Some(conn) = available.pop_front() {
            *in_use += 1;
            drop(in_use);

            return Ok(PooledConnection {
                conn: Some(conn),
                pool: self,
            });
        }

        // Check if we can create a new connection (allow growth up to MAX_POOL_SIZE)
        if *in_use < MAX_POOL_SIZE {
            // Create a new connection
            let conn = Self::open_read_only(&self.db_path)?;
            *in_use += 1;
            drop(available);
            drop(in_use);

            return Ok(PooledConnection {
                conn: Some(conn),
                pool: self,
            });
        }

        let in_use_val = *in_use;
        drop(in_use);

        // Pool exhausted, wait for a connection to be returned
        if let Some(timeout) = timeout {
            let elapsed = start.elapsed();
            if elapsed >= timeout {
                return Err(PoolError::Timeout(timeout));
            }

            let remaining = timeout.saturating_sub(elapsed);

            // Use the condvar to wait - it takes ownership of the guard
            let (mut available, _timeout_result) = self
                .condvar
                .wait_timeout(available, remaining)
                .map_err(|_| PoolError::Poisoned)?;

            // Check timeout
            if start.elapsed() >= timeout {
                return Err(PoolError::Timeout(timeout));
            }

            // Try again
            if let Some(conn) = available.pop_front() {
                let mut in_use = self.in_use.lock().map_err(|_| PoolError::Poisoned)?;
                *in_use += 1;
                drop(in_use);

                return Ok(PooledConnection {
                    conn: Some(conn),
                    pool: self,
                });
            }

            return Err(PoolError::Exhausted(in_use_val, self.max_size));
        }

        Err(PoolError::Exhausted(in_use_val, self.max_size))
    }

    /// Return a connection to the pool.
    ///
    /// # Errors
    /// Returns [`PoolError::Poisoned`] if pool bookkeeping is poisoned.
    pub fn return_conn(&self, conn: Connection) -> Result<(), PoolError> {
        let mut available = self.available.lock().map_err(|_| PoolError::Poisoned)?;

        available.push_back(conn);
        self.condvar.notify_one();

        let mut in_use = self.in_use.lock().map_err(|_| PoolError::Poisoned)?;
        *in_use = in_use.saturating_sub(1);

        Ok(())
    }

    /// Get pool statistics.
    ///
    /// # Errors
    /// Returns [`PoolError::Poisoned`] if pool bookkeeping is poisoned.
    pub fn stats(&self) -> Result<PoolStats, PoolError> {
        let available = self.available.lock().map_err(|_| PoolError::Poisoned)?;
        let in_use = self.in_use.lock().map_err(|_| PoolError::Poisoned)?;

        Ok(PoolStats {
            available: available.len(),
            in_use: *in_use,
            max_size: self.max_size,
            uptime: self.created_at.elapsed(),
        })
    }
}

/// Pooled connection guard.
///
/// When dropped, the connection is returned to the pool.
#[derive(Debug)]
pub struct PooledConnection<'a> {
    conn: Option<Connection>,
    pool: &'a ReadPool,
}

impl std::ops::Deref for PooledConnection<'_> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn
            .as_ref()
            .expect("PooledConnection conn should always be Some")
    }
}

impl Drop for PooledConnection<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = self.pool.return_conn(conn);
        }
    }
}

/// Pool statistics.
#[derive(Debug, Clone)]
pub struct PoolStats {
    pub available: usize,
    pub in_use: usize,
    pub max_size: usize,
    pub uptime: Duration,
}

impl Default for PoolStats {
    fn default() -> Self {
        Self {
            available: 0,
            in_use: 0,
            max_size: 0,
            uptime: Duration::from_secs(0),
        }
    }
}

impl Writer {
    /// Create a new writer connection.
    ///
    /// # Errors
    /// Returns a SQLite error if the database cannot be opened or configured.
    pub fn new(db_path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_URI
                | OpenFlags::SQLITE_OPEN_CREATE,
        )?;

        // Enable WAL mode
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Enable foreign keys
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Set synchronous to FULL for durability
        conn.pragma_update(None, "synchronous", "FULL")?;

        Ok(Self { conn })
    }

    /// Get the underlying connection.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// Generation values visible to the apply closure. The spine owns the bump
/// discipline: closures stamp rows with `counters.generation`, and the meta
/// update happens exactly once in the spine.
struct SpineCounters {
    generation: u64,
}

/// The closure's verdict after applying its row mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoreStep {
    /// Structural mutation happened: resolve, log, bump, commit.
    Proceed,
    /// Nothing structural changed: commit the touched stat columns with no
    /// generation bump and no change rows (content-hash / stat fast path).
    FastPath,
}

/// What the closure hands back to the spine: the post-image to mirror into
/// the overlay, the entity change rows, the fresh own-outgoing target keys
/// (collected where `target_key` is computed anyway, `pool.rs` link loop),
/// and the ids whose base records this transaction replaced or removed.
#[derive(Default)]
struct ApplyPlan {
    post: Option<EntityPre>,
    entity_rows: Vec<(ChangeKind, i64)>,
    /// Immutable Tantivy document operations prepared inside the apply
    /// transaction and published as this generation's search envelope
    /// (RFC-0006 S06, D-07). Empty ops publish the no-op watermark every
    /// committed generation still sends.
    search_ops: Vec<SearchDocOp>,
    fresh_target_keys: BTreeSet<String>,
    touched_notes: BTreeSet<i64>,
    touched_attachments: BTreeSet<i64>,
    /// Upserted note row needing the targeted self-flag refresh (§6.2.6).
    conflict_row: Option<i64>,
}

/// Outcome of [`IndexActor::apply_entity_core`].
#[derive(Debug, Clone, Copy)]
enum SpineOutcome {
    /// The required entity was absent; the transaction rolled back and
    /// nothing was written.
    RolledBack,
    /// Fast path committed without a generation bump.
    FastPath { epoch: u64, generation: u64 },
    /// Structural apply committed at `generation`.
    Applied { epoch: u64, generation: u64 },
}

/// Load the note pre-image (identity columns + alias keys) inside the
/// writer transaction before rows are mutated (§6.2.2; D3 requires the
/// pre-image in the affected key set).
fn load_note_pre_image(
    tx: &Transaction<'_>,
    path_native: &[u8],
) -> Result<Option<NoteImage>, String> {
    let row = tx
        .query_row(
            "SELECT note_id,path,path_native,path_key,stem_key,title,stable_id,stable_id_key,id_conflict,content_hash \
             FROM notes WHERE path_native=?1",
            [path_native],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<Vec<u8>>>(9)?,
                ))
            },
        )
        .optional()
        .map_err(|error| error.to_string())?;
    let Some((
        id,
        path,
        path_native,
        path_key,
        stem_key,
        title,
        stable_id,
        stable_id_key,
        id_conflict,
        content_hash,
    )) = row
    else {
        return Ok(None);
    };
    let aliases = {
        let mut statement = tx
            .prepare("SELECT alias_key FROM aliases WHERE note_id=?1 ORDER BY alias_key")
            .map_err(|error| error.to_string())?;
        statement
            .query_map([id], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| error.to_string())?
    };
    Ok(Some(NoteImage {
        id,
        path,
        path_native,
        path_key,
        stem_key,
        title,
        aliases,
        stable_id,
        stable_id_key,
        id_conflict: id_conflict != 0,
        content_hash,
    }))
}

/// Load the attachment pre-image inside the writer transaction.
fn load_att_pre_image(
    tx: &Transaction<'_>,
    path_native: &[u8],
) -> Result<Option<AttImage>, String> {
    tx.query_row(
        "SELECT att_id,path,path_native,path_key,stem_key FROM attachments WHERE path_native=?1",
        [path_native],
        |row| {
            Ok(AttImage {
                id: row.get(0)?,
                path: row.get(1)?,
                path_native: row.get(2)?,
                path_key: row.get(3)?,
                stem_key: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(|error| error.to_string())
}

/// Reserve an epoch-local identity even when the largest live row was deleted.
/// SQLite's default ROWID allocator can reuse it; active graph references cannot.
fn reserve_graph_identity(
    conn: &rusqlite::Connection,
    note: bool,
    reserve: bool,
) -> Result<(), String> {
    let (table, column, key) = if note {
        ("notes", "note_id", "note_identity_high_water")
    } else {
        ("attachments", "att_id", "attachment_identity_high_water")
    };
    let highest:i64=conn.query_row(&format!("SELECT MAX(COALESCE((SELECT CAST(value AS INTEGER) FROM meta WHERE key=?1),0),COALESCE(MAX({column}),0)) FROM {table}"),[key],|row|row.get(0)).map_err(|error|error.to_string())?;
    let next = highest
        .checked_add(i64::from(reserve))
        .ok_or("graph identity space exhausted")?;
    conn.execute("INSERT INTO meta(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",rusqlite::params![key,next.to_string()]).map_err(|error|error.to_string())?;
    Ok(())
}

/// Reserve search envelope capacity BEFORE the generation commits (D-07).
/// Every bumped generation publishes an envelope, including a no-op
/// watermark when no indexed text changed. A failed reservation must roll
/// the caller's transaction back — refusal happens before any effect.
fn reserve_search_envelope(
    handle: Option<&SearchWriterHandle>,
    ops: &[SearchDocOp],
) -> Result<Option<crate::search_writer::EnvelopeReservation>, String> {
    let Some(handle) = handle else {
        // A secondary handle publishes no envelopes: the owning writer
        // catches the derived view up from the authoritative cache.
        return Ok(None);
    };
    let approx_bytes = ops
        .iter()
        .map(SearchDocOp::approx_bytes)
        .sum::<usize>()
        .saturating_add(32);
    handle
        .reserve(approx_bytes)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// Publish the immutable generation envelope after the commit succeeded.
/// The reservation already held the capacity, so the only failure is a
/// writer death between reserve and enqueue. The apply is already durable
/// at that point, and the dead writer has reset pending captures — the next
/// envelope trips the owner's desync rebuild (the designed self-healing), so
/// this is logged and carried, never surfaced as an apply failure.
fn publish_search_envelope(
    reservation: Option<crate::search_writer::EnvelopeReservation>,
    handle: Option<&SearchWriterHandle>,
    epoch: u64,
    generation: u64,
    ops: &mut Vec<SearchDocOp>,
) {
    let (Some(reservation), Some(handle)) = (reservation, handle) else {
        return;
    };
    let envelope = SearchEnvelope {
        epoch,
        generation,
        ops: std::sync::Arc::from(std::mem::take(ops).into_boxed_slice()),
    };
    if let Err(error) = reservation.enqueue(handle, envelope) {
        tracing::warn!(
            %error,
            epoch,
            generation,
            "search envelope publish failed after commit; the writer rebuilds from the authoritative cache"
        );
    }
}

/// Read the advisory committed-generation mirror for the search writer.
fn committed_of(conn: &rusqlite::Connection) -> u64 {
    crate::schema::get_meta_u64(conn, MetaKey::TantivyCommittedGeneration)
        .unwrap_or(Some(0))
        .unwrap_or(0)
}

/// Whole-vault backstop walk on the writer transaction (D13; the sole
/// `re_resolve_all` call site in this file, per the RFC's grep gate).
/// Returns the flipped sources and the catalog rebuilt from the
/// transaction's own rows, to be installed at commit. A free function
/// because the transaction holds a borrow of the writer connection.
fn backstop_walk(
    tx: &Transaction<'_>,
) -> Result<(Vec<i64>, crate::catalog::ResolverCatalog), String> {
    let mut walk = Resolver::new(tx);
    let changed = walk.re_resolve_all()?;
    let catalog = walk
        .take_catalog()
        .ok_or("resolver catalog missing after backstop walk")?;
    Ok((changed, catalog))
}

impl IndexActor {
    /// Create a new index actor.
    ///
    /// # Errors
    /// Returns a SQLite error if the writer or read pool cannot be initialized.
    pub fn new(db_path: &Path, pool_size: usize) -> Result<Self> {
        Self::construct(db_path, pool_size, None, None)
    }

    /// Create a new index actor wired to a [`GenerationSignal`]
    /// (RFC-0005 §5.1/FR-1): every committed structural generation bump
    /// publishes its `IndexVersion` from the apply spine's commit sites,
    /// and `FastPath` commits stay silent. This is the production path —
    /// derived views subscribe to the matching [`crate::GenerationWatch`].
    ///
    /// # Errors
    /// Returns a SQLite error if the writer or read pool cannot be initialized.
    pub fn with_generation_signal(
        db_path: &Path,
        pool_size: usize,
        signal: GenerationSignal,
    ) -> Result<Self> {
        Self::construct(db_path, pool_size, None, Some(signal))
    }

    /// Create an actor whose resolver backstop fires on a custom generation
    /// cadence. Measurement seam for benches and RFC-0004 backstop tests
    /// (S2/S8); production callers use [`IndexActor::new`] with
    /// [`crate::resolver::RESOLVER_BACKSTOP_GENERATIONS`].
    ///
    /// # Errors
    /// Returns a SQLite error if the writer or read pool cannot be initialized.
    pub fn with_resolver_backstop(
        db_path: &Path,
        pool_size: usize,
        generations: u64,
    ) -> Result<Self> {
        Self::construct(db_path, pool_size, Some(generations.max(1)), None)
    }

    fn construct(
        db_path: &Path,
        pool_size: usize,
        backstop: Option<u64>,
        signal: Option<GenerationSignal>,
    ) -> Result<Self> {
        let parser_version = crate::schema::open_or_init(db_path)?;
        let needs_reparse = crate::schema::get_meta_u32(&parser_version, MetaKey::ParserVersion)?
            != Some(crate::schema::PARSER_VERSION);
        drop(parser_version);
        let writer = Writer::new(db_path)?;
        reserve_graph_identity(writer.conn(), true, false)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
        reserve_graph_identity(writer.conn(), false, false)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(error.into()))?;
        let read_pool = ReadPool::new(db_path, pool_size)?;
        let cache_dir = db_path.parent().unwrap_or_else(|| Path::new("."));
        let (search, search_recreated) = if let Ok(search) = TantivyBackend::open(cache_dir) {
            (search, false)
        } else {
            // Tantivy is a derived cache. A malformed or incompatible
            // directory is discarded and rebuilt from the structural DB.
            let path = cache_dir.join("tantivy");
            if path.exists() {
                std::fs::remove_dir_all(&path)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            }
            (
                TantivyBackend::open(cache_dir)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                true,
            )
        };
        let sqlite_epoch = crate::schema::get_meta_u64(writer.conn(), MetaKey::Epoch)?.unwrap_or(1);
        let sqlite_generation =
            crate::schema::get_meta_u64(writer.conn(), MetaKey::Generation)?.unwrap_or(0);
        // One bounded ordered writer owns the IndexWriter from here on
        // (RFC-0006 S06, D-07). Startup rebuilds from the authoritative
        // `search_docs` cache whenever the on-disk view does not exactly
        // match the structural generation — replay and rebuild share one
        // correct outcome, and no ephemeral capture survives a restart.
        // A busy writer lock marks this handle secondary: it applies SQL
        // only and leaves the derived view to the owning writer, instead
        // of deleting the owner's index directory.
        let search_writer = match crate::search_writer::spawn_search_writer(
            &search,
            sqlite_epoch,
            committed_of(writer.conn()),
        ) {
            Ok(handle) => Some(handle),
            Err(error) => {
                if error.contains("index lock") || error.contains("LockBusy") {
                    tracing::warn!(
                        %error,
                        "secondary index handle: the tantivy writer belongs to another actor"
                    );
                    None
                } else {
                    return Err(rusqlite::Error::ToSqlConversionFailure(error.into()));
                }
            }
        };
        if let Some(handle) = &search_writer
            && (search_recreated || handle.committed_frontier() != sqlite_generation)
        {
            let docs = crate::search_writer::read_rebuild_docs(writer.conn())
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
            handle
                .rebuild_blocking(sqlite_epoch, sqlite_generation, docs.into())
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
        }

        let resolver = match backstop {
            Some(generations) => ResolverState::with_backstop_interval(writer.conn(), generations)?,
            None => ResolverState::load(writer.conn())?,
        };
        Ok(Self {
            writer,
            read_pool,
            search_writer,
            needs_reparse,
            resolver,
            signal,
        })
    }

    /// Committed resolver diagnostics: backstop cadence and resolution
    /// strategy counters (surfaced by doctor via the vault actor's runtime
    /// meta drain, RFC-0004 §6.7/S3).
    #[must_use]
    pub fn resolver_diagnostics(&self) -> ResolverDiagnostics {
        self.resolver.diagnostics()
    }

    /// Force the next structural apply to run the whole-vault backstop walk
    /// (D13 explicit trigger; S3 full-verify jobs). Idempotent demand: a
    /// rolled-back apply leaves it standing.
    pub fn force_resolver_backstop(&mut self) {
        self.resolver.force_backstop();
    }

    /// Whether opening this index found a parser version that requires a
    /// full content verification pass.
    #[must_use]
    pub const fn needs_reparse(&self) -> bool {
        self.needs_reparse
    }

    /// Mark the parser version current after the startup `VerifyContent` pass.
    ///
    /// # Errors
    /// Returns an error when the SQLite metadata cannot be updated.
    pub fn mark_parser_current(&mut self) -> Result<(), String> {
        crate::schema::set_meta(
            self.writer.conn(),
            MetaKey::ParserVersion,
            &crate::schema::PARSER_VERSION.to_string(),
        )
        .map_err(|error| error.to_string())?;
        self.needs_reparse = false;
        Ok(())
    }

    /// Persist actor-owned runtime counters and connectivity state.
    ///
    /// # Errors
    /// Returns the SQLite error when the meta row cannot be written.
    pub fn set_runtime_meta(
        &mut self,
        key: crate::schema::MetaKey,
        value: &str,
    ) -> Result<(), String> {
        crate::schema::set_meta(self.writer.conn(), key, value).map_err(|error| error.to_string())
    }

    /// Get the writer connection.
    pub fn writer(&self) -> &Writer {
        &self.writer
    }

    /// Get the read pool.
    pub fn read_pool(&self) -> &ReadPool {
        &self.read_pool
    }

    /// Get pool statistics.
    ///
    /// # Errors
    /// Returns a pool error if the read-pool statistics cannot be read.
    pub fn pool_stats(&self) -> Result<PoolStats, PoolError> {
        self.read_pool.stats()
    }

    /// Read the structural index's authoritative epoch and generation.
    ///
    /// # Errors
    /// Returns the underlying error text when the `meta` table cannot be read.
    pub fn current_version(&self) -> Result<(u64, u64), String> {
        let epoch = get_meta_u64(self.writer.conn(), MetaKey::Epoch)
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        let generation = get_meta_u64(self.writer.conn(), MetaKey::Generation)
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        Ok((epoch, generation))
    }

    /// Resolve one editor link through the maintained resolver state — the
    /// same base+overlay every apply folds into — instead of rebuilding a
    /// full catalog per call on this actor thread. The base reloads only as
    /// a fallback, when the committed generation has run ahead of it
    /// (catalog.rs header: `load` is only needed at backstops).
    /// `source_path_key` is the folded vault-relative path of the source note.
    ///
    /// # Errors
    /// Returns an error when SQLite cannot look up the source or reload the
    /// resolver base.
    pub fn resolve_target(
        &mut self,
        target_key: &str,
        raw_target: &str,
        is_embed: bool,
        source_path_key: Option<&str>,
    ) -> Result<crate::resolver::Resolution, String> {
        let conn = self.writer.conn();
        let source_note_id = source_path_key
            .map(|path_key| {
                conn.query_row(
                    "SELECT note_id FROM notes WHERE path_key=?1 ORDER BY path_key LIMIT 1",
                    [path_key],
                    |row| row.get::<_, i64>(0),
                )
                .optional()
                .map_err(|error| error.to_string())
            })
            .transpose()?
            .flatten();
        let generation = get_meta_u64(conn, MetaKey::Generation)
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        if self.resolver.base().generation < generation {
            self.resolver
                .reload_base(conn)
                .map_err(|error| error.to_string())?;
        }
        Ok(self.resolver.overlay().resolve(
            self.resolver.base(),
            target_key,
            raw_target,
            is_embed,
            source_note_id,
        ))
    }

    /// List native path keys currently represented in the structural index.
    ///
    /// # Errors
    /// Returns the underlying error text when the query cannot be prepared or
    /// a row cannot be read.
    pub fn indexed_note_paths(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut statement = self
            .writer
            .conn()
            .prepare("SELECT path_native FROM notes")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|e| e.to_string())?;
        rows.map(|row| row.map_err(|e| e.to_string())).collect()
    }

    /// Return parsed note records used to infer rename pairs during a
    /// reconcile. The actor snapshots vanished rows before applying removals.
    ///
    /// # Errors
    /// Returns an error when the writer connection cannot read note rows.
    pub fn rename_note_snapshot(&self) -> Result<Vec<crate::types::Note>, String> {
        let mut statement = self.writer.conn().prepare(
            "SELECT note_id,path,path_native,path_key,stem_key,title,stable_id,id_conflict,content_hash,size,mtime_ns,scan_ns,frontmatter,preview,parse_ok,generation FROM notes WHERE parse_ok=1 AND content_hash IS NOT NULL ORDER BY path_key",
        ).map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([], |row| {
                Ok(crate::types::Note {
                    note_id: row.get(0)?,
                    path: row.get(1)?,
                    path_native: row.get(2)?,
                    path_key: row.get(3)?,
                    stem_key: row.get(4)?,
                    title: row.get(5)?,
                    stable_id: row.get(6)?,
                    id_conflict: row.get(7)?,
                    content_hash: row.get(8)?,
                    size: row.get(9)?,
                    mtime_ns: row.get(10)?,
                    scan_ns: row.get(11)?,
                    frontmatter: row.get(12)?,
                    preview: row.get(13)?,
                    parse_ok: row.get(14)?,
                    generation: row.get(15)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.map(|row| row.map_err(|error| error.to_string()))
            .collect()
    }

    /// Return indexed links targeting a note stem, including path-suffix and
    /// explicit `.md` forms. The caller MUST verify each source hash again.
    ///
    /// # Errors
    /// Returns the underlying error text when the lookup query fails.
    pub fn inbound_links_for_stem(&self, stem: &str) -> Result<Vec<InboundLinkSpan>, String> {
        let stem = stem.to_lowercase();
        let explicit = format!("{stem}.md");
        let mut statement = self
            .writer
            .conn()
            .prepare(
                "SELECT n.path_native,n.content_hash,l.span_start,l.span_end,l.dst_raw
             FROM links l JOIN notes n ON n.note_id=l.src
             WHERE l.by_id=0 AND (l.target_key=?1 OR l.target_key=?2
                OR l.target_key LIKE '%/' || ?1 OR l.target_key LIKE '%/' || ?2)
             ORDER BY n.path_native,l.span_start",
            )
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map((&stem, &explicit), |row| {
                let hash: Vec<u8> = row.get(1)?;
                let source_hash: [u8; 32] =
                    hash.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
                let start: i64 = row.get(2)?;
                let end: i64 = row.get(3)?;
                Ok(InboundLinkSpan {
                    source_path_native: row.get(0)?,
                    source_hash,
                    span_start: start.cast_unsigned(),
                    span_end: end.cast_unsigned(),
                    target_raw: row.get(4)?,
                })
            })
            .map_err(|e| e.to_string())?;
        rows.map(|row| row.map_err(|e| e.to_string())).collect()
    }

    /// Return resolved inbound embeds for one indexed attachment path.
    ///
    /// # Errors
    /// Returns the SQLite error text when the lookup fails or a source hash is
    /// malformed.
    pub fn inbound_embeds_for_attachment(
        &self,
        path_native: &[u8],
    ) -> Result<Vec<InboundLinkSpan>, String> {
        let mut statement = self
            .writer
            .conn()
            .prepare(
                "SELECT n.path_native,n.content_hash,l.span_start,l.span_end,l.dst_raw
                 FROM links l JOIN notes n ON n.note_id=l.src
                 JOIN attachments a ON a.att_id=l.dst_att
                 WHERE l.kind='embed' AND a.path_native=?1
                 ORDER BY n.path_native,l.span_start",
            )
            .map_err(|error| error.to_string())?;
        let rows = statement
            .query_map([path_native], |row| {
                let hash: Vec<u8> = row.get(1)?;
                let source_hash: [u8; 32] =
                    hash.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
                let start: i64 = row.get(2)?;
                let end: i64 = row.get(3)?;
                Ok(InboundLinkSpan {
                    source_path_native: row.get(0)?,
                    source_hash,
                    span_start: start.cast_unsigned(),
                    span_end: end.cast_unsigned(),
                    target_raw: row.get(4)?,
                })
            })
            .map_err(|error| error.to_string())?;
        rows.map(|row| row.map_err(|error| error.to_string()))
            .collect()
    }

    /// List native path keys for indexed attachments.
    ///
    /// # Errors
    /// Returns the underlying error text when the lookup query fails.
    pub fn indexed_attachment_paths(&self) -> Result<Vec<Vec<u8>>, String> {
        let mut statement = self
            .writer
            .conn()
            .prepare("SELECT path_native FROM attachments")
            .map_err(|e| e.to_string())?;
        let rows = statement
            .query_map([], |row| row.get::<_, Vec<u8>>(0))
            .map_err(|e| e.to_string())?;
        rows.map(|row| row.map_err(|e| e.to_string())).collect()
    }

    /// Whether indexed attachment metadata and, when available, content hash match.
    ///
    /// # Errors
    /// Returns the underlying error text when the lookup query fails.
    pub fn attachment_matches(
        &self,
        path_native: &[u8],
        size: u64,
        mtime_ns: i64,
        content_hash: Option<[u8; 32]>,
        hydrated: bool,
        scan_ns: i64,
    ) -> Result<bool, String> {
        let current: Option<(i64, i64, Option<Vec<u8>>, bool)> = self
            .writer
            .conn()
            .query_row(
                "SELECT size,mtime_ns,content_hash,hydrated FROM attachments WHERE path_native=?1",
                [path_native],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((old_size, old_mtime, old_hash, old_hydrated)) = current else {
            return Ok(false);
        };
        let fresh = mtime_ns >= scan_ns.saturating_sub(2_000_000_000);
        Ok(!fresh
            && u64::try_from(old_size).ok() == Some(size)
            && old_mtime == mtime_ns
            && old_hydrated == hydrated
            && content_hash.is_none_or(|hash| old_hash.as_deref() == Some(hash.as_slice())))
    }

    /// Check whether one native path key is already indexed.
    ///
    /// # Errors
    /// Returns the underlying error text when the lookup query fails.
    pub fn contains_note_path(&self, path_native: &[u8]) -> Result<bool, String> {
        self.writer
            .conn()
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM notes WHERE path_native=?1)",
                [path_native],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    /// Advertise a committed structural generation on the writer-side
    /// broadcast (RFC-0005 §5.1/§4.3, FR-1).
    ///
    /// Contract, enforced structurally at the spine's commit sites and by
    /// the `graph_event_contract.rs` property test over every public apply
    /// path:
    /// - MUST be called immediately after a `tx.commit()` that bumped
    ///   `MetaKey::Generation` succeeds — never before the commit is durable
    ///   (§4.9: a reader that observes the watermark must find that
    ///   generation in the database).
    /// - MUST NOT be called for `FastPath` commits (no bump, no change
    ///   rows — graph-invisible by contract).
    /// - A no-op when the pool was constructed signal-less (CLI/test pools).
    ///
    /// Cost on the writer thread: two atomic stores and one `try_send`
    /// (~50 ns, NFR-9) — strictly smaller than the change-row write it
    /// accompanies.
    fn publish_generation(&self, version: IndexVersion) {
        if let Some(signal) = &self.signal {
            signal.publish(version);
        }
    }

    /// Record the vault identity in the index `meta` table (RFC-0005
    /// FR-6): the graph service revalidates its long-lived read connection
    /// against this value on every wake and reopens on mismatch — the
    /// DB-recreate detector. Idempotent, and never a generation bump (no
    /// `publish_generation`; the graph is not notified).
    ///
    /// # Errors
    /// Returns the SQLite error text when the meta write fails.
    pub fn ensure_vault_uuid(&self, uuid: &str) -> Result<(), String> {
        crate::schema::set_meta(
            self.writer().conn(),
            crate::schema::MetaKey::VaultUuid,
            uuid,
        )
        .map_err(|error| error.to_string())
    }

    /// The single apply spine (§6.2 "How"): pre-image → mutate → post →
    /// generation bump → group-scoped `id_conflict` → resolution (targeted
    /// against the staged overlay, or the whole-vault backstop walk at D13
    /// cadence) → change rows → commit → fold. All five apply paths keep
    /// their distinct row shapes but delegate everything else here, so the
    /// sequence that previously existed five divergent times — including
    /// the divergent change-row writes of C4 — exists exactly once.
    ///
    /// This is also the signal choke point (RFC-0005 FR-1): the two sites
    /// that commit a bumped generation publish it via
    /// [`Self::publish_generation`] immediately on commit success; the two
    /// `FastPath` commit sites never publish.
    ///
    /// Transaction boundaries stay the writer's: one `conn.transaction()`
    /// owns every structural write, the generation meta bump, and the
    /// change rows, matching the commit discipline each path had before.
    /// On any error the transaction (and the overlay, which is dropped at
    /// the next fold rather than here) leaves no trace in the base —
    /// rollback safety is structural (§6.2.4).
    #[allow(
        clippy::too_many_lines,
        reason = "one transaction's pre/mutate/post/resolve/log/commit discipline; splitting would only relocate it"
    )]
    fn apply_entity_core<F>(
        &mut self,
        note: bool,
        require_present: bool,
        path_native: &[u8],
        mutate: F,
    ) -> Result<(SpineOutcome, ApplyPlan), String>
    where
        F: FnOnce(
            &Transaction<'_>,
            Option<&EntityPre>,
            &SpineCounters,
            &mut ApplyPlan,
        ) -> Result<CoreStep, String>,
    {
        let tx = self
            .writer
            .conn
            .transaction()
            .map_err(|error| error.to_string())?;
        let epoch = get_meta_u64(&tx, MetaKey::Epoch)
            .map_err(|error| error.to_string())?
            .unwrap_or(1);
        let current_generation = get_meta_u64(&tx, MetaKey::Generation)
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        let generation = current_generation
            .checked_add(1)
            .ok_or("index generation overflow")?;
        let pre: Option<EntityPre> = if note {
            load_note_pre_image(&tx, path_native)?.map(EntityPre::Note)
        } else {
            load_att_pre_image(&tx, path_native)?.map(EntityPre::Attachment)
        };
        if require_present && pre.is_none() {
            tx.rollback().map_err(|error| error.to_string())?;
            return Ok((SpineOutcome::RolledBack, ApplyPlan::default()));
        }
        let counters = SpineCounters { generation };
        let mut plan = ApplyPlan::default();
        let mutated = mutate(&tx, pre.as_ref(), &counters, &mut plan)?;
        if mutated == CoreStep::FastPath && !self.resolver.backstop_due() {
            // FastPath commit: stat columns only, no generation bump and no
            // change rows — graph-invisible by contract, so no
            // publish_generation (RFC-0005 FR-1).
            tx.commit().map_err(|error| error.to_string())?;
            return Ok((
                SpineOutcome::FastPath {
                    epoch,
                    generation: current_generation,
                },
                plan,
            ));
        }
        if mutated == CoreStep::FastPath {
            // RFC-0004 S3 (D13): a forced backstop demand (full-verify
            // job) rides even an unchanged-content apply. The whole-vault
            // walk decides: no flips ⇒ demand consumed without touching
            // the generation (the refreshed base is still installed at
            // commit); flips ⇒ drift repairs itself as a logged structural
            // apply (outgoing rows per FR-1 plus the touched entity row).
            let (changed, catalog) = backstop_walk(&tx)?;
            if changed.is_empty() {
                // The walk found no drift: no bump, no change rows, so this
                // commit is still a FastPath — no publish_generation
                // (RFC-0005 FR-1).
                tx.commit().map_err(|error| error.to_string())?;
                self.resolver.commit_backstop(catalog);
                return Ok((
                    SpineOutcome::FastPath {
                        epoch,
                        generation: current_generation,
                    },
                    plan,
                ));
            }
            let mut rows: Vec<(ChangeKind, i64)> = plan.entity_rows.clone();
            if rows.is_empty() {
                // The fast-path closure plans no entity rows; the touched
                // entity is nonetheless refreshed, and the apply paths
                // post-commit steps expect the id there.
                let entity_row = match pre.as_ref() {
                    Some(EntityPre::Note(image)) => Some((ChangeKind::Note, image.id)),
                    Some(EntityPre::Attachment(image)) => Some((ChangeKind::Attachment, image.id)),
                    None => None,
                };
                if let Some(row) = entity_row {
                    plan.entity_rows.push(row);
                    rows.push(row);
                }
            }
            rows.extend(changed.into_iter().map(|src| (ChangeKind::Outgoing, src)));
            let row_count = rows.len();
            // Search envelope capacity is reserved BEFORE the generation
            // commits (D-07); backpressure refuses the index admission here
            // and upstream maps it to an index-pending retry.
            let reservation =
                match reserve_search_envelope(self.search_writer.as_ref(), &plan.search_ops) {
                    Ok(reservation) => reservation,
                    Err(error) => {
                        let _ = tx.rollback();
                        return Err(error);
                    }
                };
            tx.execute(
                "UPDATE meta SET value=?1 WHERE key=?2",
                rusqlite::params![generation.to_string(), MetaKey::Generation.as_str()],
            )
            .map_err(|error| error.to_string())?;
            ChangeLogWriter::record_all(&tx, generation, &rows)?;
            tx.commit().map_err(|error| error.to_string())?;
            // FR-1 site: this commit bumped `MetaKey::Generation` (the walk
            // found drift and repaired it as a logged structural apply), so
            // the generation is broadcast immediately on commit success.
            self.publish_generation(IndexVersion::new(epoch, generation));
            publish_search_envelope(
                reservation,
                self.search_writer.as_ref(),
                epoch,
                generation,
                &mut plan.search_ops,
            );
            self.resolver.commit_backstop(catalog);
            self.resolver.note_committed_apply(row_count as u64);
            self.resolver.fold_overlay(epoch, generation);
            return Ok((SpineOutcome::Applied { epoch, generation }, plan));
        }
        tx.execute(
            "UPDATE meta SET value=?1 WHERE key=?2",
            rusqlite::params![generation.to_string(), MetaKey::Generation.as_str()],
        )
        .map_err(|error| error.to_string())?;
        let relevant = identity_relevant(pre.as_ref(), plan.post.as_ref());
        let mut stable_id_keys: BTreeSet<String> = BTreeSet::new();
        for image in pre.iter().chain(plan.post.iter()) {
            if let Some(key) = image.stable_id_key() {
                stable_id_keys.insert(key.clone());
            }
        }
        // Targeted id_conflict maintenance (§6.2.6): only the groups whose
        // membership can have moved, plus the applied row's own flag. Every
        // member whose stored flag actually flipped is FR-1-visible entity
        // state (`GraphNodeRecord.id_conflict` reads it) and travels into
        // the change rows below.
        let (group_updates, conflict_flips) = if relevant {
            refresh_id_conflict_groups(
                &tx,
                &stable_id_keys,
                if note { plan.conflict_row } else { None },
            )?
        } else {
            (0, std::collections::BTreeSet::new())
        };
        // The post-image mirrors the committed flag into the base.
        if group_updates > 0
            && let Some(EntityPre::Note(image)) = &mut plan.post
        {
            image.id_conflict = tx
                .query_row(
                    "SELECT id_conflict FROM notes WHERE note_id=?1",
                    [image.id],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(|error| error.to_string())?
                != 0;
        }
        let backstop = self.resolver.backstop_due();
        let changed_sources;
        let mut targeted_pass: Option<u64> = None;
        let mut backstop_catalog = None;
        if backstop {
            // D13 backstop site — reached only from the spine's two
            // backstop rides; the apply paths themselves never walk (the
            // grep gate holds the single `re_resolve_all` occurrence to
            // `backstop_walk` below). The walk reads this transaction's
            // own rows, so it needs no overlay; its rebuilt catalog is
            // installed at commit, never before, so a rolled-back backstop
            // cannot touch the base.
            let (changed, catalog) = backstop_walk(&tx)?;
            changed_sources = changed;
            backstop_catalog = Some(catalog);
        } else {
            {
                let overlay = self.resolver.overlay_mut();
                for id in &plan.touched_notes {
                    overlay.remove_note(*id);
                }
                for id in &plan.touched_attachments {
                    overlay.remove_attachment(*id);
                }
                match &plan.post {
                    Some(EntityPre::Note(image)) => overlay.upsert_note(image),
                    Some(EntityPre::Attachment(image)) => overlay.upsert_attachment(image),
                    None => {}
                }
            }
            for key in &stable_id_keys {
                let winner = group_winner_in_tx(&tx, key)?;
                self.resolver.overlay_mut().set_stable_winner(key, winner);
            }
            let affected_keys = affected_resolution_keys(
                relevant.then_some(pre.as_ref()).flatten(),
                relevant.then_some(plan.post.as_ref()).flatten(),
                &plan.fresh_target_keys,
            );
            let (changed, visited) = resolve_affected(&tx, &self.resolver, &affected_keys)?;
            changed_sources = changed;
            targeted_pass = (!affected_keys.is_empty()).then_some(visited);
        }
        // FR-1, D5: the entity rows plus one `outgoing(src)` row per source
        // whose link rows actually flipped. Duplicates collapse via the PK
        // when a flipped source is the applied entity itself (§4.6).
        let mut rows = plan.entity_rows.clone();
        rows.extend(
            changed_sources
                .into_iter()
                .map(|src| (ChangeKind::Outgoing, src)),
        );
        // Group-scoped `id_conflict` maintenance rewrote other members'
        // rows (§6.2.6): those flags are graph-visible entity state, so
        // each flipped member gets its own `note` row (FR-1) — the S6
        // mirror-equivalence gate is what makes this omission impossible.
        rows.extend(conflict_flips.into_iter().map(|id| (ChangeKind::Note, id)));
        ChangeLogWriter::record_all(&tx, generation, &rows)?;
        let change_rows = rows.len();
        // Search envelope capacity is reserved BEFORE the generation
        // commits (D-07); backpressure refuses the index admission here.
        let reservation =
            match reserve_search_envelope(self.search_writer.as_ref(), &plan.search_ops) {
                Ok(reservation) => reservation,
                Err(error) => {
                    let _ = tx.rollback();
                    return Err(error);
                }
            };
        tx.commit().map_err(|error| error.to_string())?;
        // FR-1 site: the structural apply bumped `MetaKey::Generation` in
        // this transaction, so the generation is broadcast immediately on
        // commit success — ahead of the in-memory fold, because the feed
        // (database) is the truth any subscriber verifies against.
        self.publish_generation(IndexVersion::new(epoch, generation));
        publish_search_envelope(
            reservation,
            self.search_writer.as_ref(),
            epoch,
            generation,
            &mut plan.search_ops,
        );
        // Commit fold (§6.2.4): SQLite writes are durable; in-memory state
        // follows. Counters track committed applies only.
        if let Some(catalog) = backstop_catalog {
            self.resolver.commit_backstop(catalog);
        }
        self.resolver.note_group_updates(group_updates);
        if let Some(visited) = targeted_pass {
            self.resolver.note_targeted_pass(visited);
        }
        self.resolver.note_committed_apply(change_rows as u64);
        self.resolver.fold_overlay(epoch, generation);
        Ok((SpineOutcome::Applied { epoch, generation }, plan))
    }

    /// Remove a vanished note and its dependent structural/search rows in one
    /// transaction. Returns whether an indexed note was removed.
    ///
    /// # Errors
    /// Returns the underlying error text when the transaction cannot be
    /// started, a query fails, or the commit fails.
    pub fn remove_note_path(&mut self, path_native: &[u8]) -> Result<bool, String> {
        let (outcome, _plan) =
            self.apply_entity_core(true, true, path_native, |tx, pre, _counters, plan| {
                let Some(EntityPre::Note(image)) = pre else {
                    return Err("missing note pre-image".to_owned());
                };
                let note_id = image.id;
                tx.execute("DELETE FROM notes WHERE note_id=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM properties WHERE note_id=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM search_docs WHERE note_id=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                plan.search_ops.push(SearchDocOp::Delete { note_id });
                plan.entity_rows.push((ChangeKind::Note, note_id));
                plan.touched_notes.insert(note_id);
                Ok(CoreStep::Proceed)
            })?;
        match outcome {
            SpineOutcome::RolledBack => Ok(false),
            SpineOutcome::Applied { .. } => Ok(true),
            SpineOutcome::FastPath { .. } => Err("note removal cannot take a fast path".to_owned()),
        }
    }

    /// Return true when stat data safely proves the indexed note unchanged.
    /// Recently modified files remain racy for two seconds and must be hashed.
    ///
    /// # Errors
    /// Returns an error when SQLite cannot read the indexed metadata.
    pub fn note_metadata_matches(
        &self,
        path_native: &[u8],
        size: u64,
        mtime_ns: i64,
        scan_ns: i64,
    ) -> Result<bool, String> {
        let indexed: Option<(i64, i64, i64)> = self
            .writer
            .conn()
            .query_row(
                "SELECT size,mtime_ns,scan_ns FROM notes WHERE path_native=?1",
                [path_native],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((indexed_size, indexed_mtime, _indexed_scan)) = indexed else {
            return Ok(false);
        };
        let size_matches = u64::try_from(indexed_size).ok() == Some(size);
        let not_racy = mtime_ns < scan_ns.saturating_sub(2_000_000_000);
        Ok(size_matches && indexed_mtime == mtime_ns && not_racy)
    }

    /// Apply the exact bytes installed by a save to SQLite in one structural
    /// transaction. The caller owns this actor on the sole index-writer thread.
    ///
    /// # Errors
    /// Returns an error if coordinate conversion or any SQLite step fails. A
    /// failed transaction leaves the prior structural state intact.
    pub fn apply_saved_note(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        bytes: &[u8],
        mtime_ns: i64,
        scan_ns: i64,
    ) -> Result<(u64, u64), String> {
        if bytes.len() > crate::types::MAX_NOTE_BYTES {
            return self.apply_unparsed_note(
                path_native,
                display_path,
                bytes.len() as u64,
                mtime_ns,
                scan_ns,
            );
        }
        // §6.10: strict decode under the central codec contract happens here,
        // exactly once. Malformed or ambiguous input is a failed/unparsed
        // entry with a typed decode status — never an empty success, never a
        // lossy reinterpretation.
        let decoded = flavite_encoding::decode(
            bytes,
            flavite_encoding::DecodePolicy::Auto,
            &flavite_encoding::DecodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        );
        let Ok(content) = decoded else {
            return self.apply_undecodable_note(
                path_native,
                display_path,
                u64::try_from(bytes.len()).map_err(|error| error.to_string())?,
                mtime_ns,
                scan_ns,
            );
        };
        let Ok(parsed) =
            flavite_syntax::pulldown_glue::parse_snapshot(TextSnapshot::from_decoded(content))
        else {
            return self.apply_degraded_saved_note(
                path_native,
                display_path,
                bytes,
                mtime_ns,
                scan_ns,
            );
        };
        self.apply_parsed_note(path_native, display_path, bytes, &parsed, mtime_ns, scan_ns)
    }

    /// Apply a note after the reconcile worker has parsed its immutable bytes.
    /// This keeps markdown parsing off the actor command loop while retaining
    /// the same writer transaction as `apply_saved_note`.
    ///
    /// # Errors
    /// Returns an error if coordinate conversion or any SQLite step fails.
    /// A failed transaction leaves the prior structural state intact.
    #[allow(clippy::too_many_lines)] // One transaction keeps note/search/change state atomic.
    pub fn apply_parsed_note(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        bytes: &[u8],
        parsed: &flavite_syntax::pulldown_glue::ParsedNote,
        mtime_ns: i64,
        scan_ns: i64,
    ) -> Result<(u64, u64), String> {
        // §6.10: the snapshot decoded exactly once (in `parse_note`/the
        // reconcile worker) is authoritative — this path never re-decodes,
        // so source spans map through the same composed codec map that
        // produced the parse.
        let snapshot = &parsed.snapshot;
        let (encoding_code, bom) = snapshot.format().map_or(("utf8", 0), |format| {
            (format.encoding_code(), i64::from(format.bom()))
        });
        let content_hash = Hash::from_bytes(*blake3::hash(bytes).as_bytes());
        let stem = std::path::Path::new(display_path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(display_path);
        let path_key = flavite_syntax::text::fold_key(display_path);
        let stem_key = flavite_syntax::text::fold_key(stem);
        let frontmatter = flavite_syntax::frontmatter::parse_frontmatter(&parsed.snapshot);
        let stable_id = frontmatter.as_ref().and_then(|value| {
            value
                .well_known
                .get(&flavite_syntax::wellknown::WellKnownKey::Id)
                .and_then(|values| values.first())
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        });
        let stable_id_key = stable_id.as_deref().map(flavite_syntax::text::fold_key);
        let title = flavite_syntax::wellknown::derive_title(
            frontmatter
                .as_ref()
                .and_then(|value| value.title.as_deref()),
            parsed
                .headings
                .iter()
                .find(|(level, _)| *level == pulldown_cmark::HeadingLevel::H1)
                .map(|(_, text)| text.as_str()),
            stem,
        );
        let aliases = frontmatter
            .as_ref()
            .and_then(|value| {
                value
                    .well_known
                    .get(&flavite_syntax::wellknown::WellKnownKey::Aliases)
            })
            .cloned()
            .unwrap_or_default();
        let mut alias_keys: Vec<String> = aliases
            .iter()
            .map(|alias| flavite_syntax::wellknown::normalize_tag(alias))
            .collect();
        alias_keys.sort();
        // Mirror the aliases table's PK: duplicate keys collapse to one row,
        // so the image (relevance compare + overlay indexing) must too.
        alias_keys.dedup();
        let frontmatter_tags = frontmatter.as_ref().map_or_else(Vec::new, |value| {
            value
                .well_known
                .iter()
                .filter(|(key, _)| {
                    matches!(
                        key,
                        flavite_syntax::wellknown::WellKnownKey::Tags
                            | flavite_syntax::wellknown::WellKnownKey::Tag
                    )
                })
                .flat_map(|(_, values)| values.iter().cloned())
                .collect()
        });
        let frontmatter_json = frontmatter
            .as_ref()
            .map(|value| serde_json::json!({ "yaml": value.yaml_text }))
            .map(|value| value.to_string());
        let size = i64::try_from(bytes.len()).map_err(|error| error.to_string())?;
        let (outcome, plan) = self.apply_entity_core(
            true,
            false,
            path_native,
            |tx, pre, counters, plan| {
                let existing_hash = match pre {
                    Some(EntityPre::Note(image)) => Some(image.content_hash.clone()),
                    Some(EntityPre::Attachment(_)) => {
                        return Err("attachment occupies the note path".to_owned());
                    }
                    None => None,
                };
                if existing_hash.flatten().as_deref() == Some(content_hash.as_bytes().as_slice()) {
                    tx.execute(
                        "UPDATE notes SET mtime_ns=?1,scan_ns=?2 WHERE path_native=?3",
                        rusqlite::params![mtime_ns, scan_ns, path_native],
                    )
                    .map_err(|error| error.to_string())?;
                    return Ok(CoreStep::FastPath);
                }
                reserve_graph_identity(tx, true, true)?;
                tx.execute(
                    "INSERT INTO notes(note_id,path,path_native,path_key,stem_key,title,stable_id,stable_id_key,id_conflict,content_hash,size,mtime_ns,scan_ns,frontmatter,preview,parse_ok,generation,encoding,bom,decode_status)
                     VALUES((SELECT CAST(value AS INTEGER) FROM meta WHERE key='note_identity_high_water'),?1,?2,?3,?4,?5,?6,?7,0,?8,?9,?10,?11,?12,?13,1,?14,?15,?16,'ok')
                     ON CONFLICT(path_native) DO UPDATE SET path=excluded.path,path_key=excluded.path_key,stem_key=excluded.stem_key,title=excluded.title,stable_id=excluded.stable_id,stable_id_key=excluded.stable_id_key,content_hash=excluded.content_hash,size=excluded.size,mtime_ns=excluded.mtime_ns,scan_ns=excluded.scan_ns,frontmatter=excluded.frontmatter,preview=excluded.preview,parse_ok=1,generation=excluded.generation,encoding=excluded.encoding,bom=excluded.bom,decode_status='ok'",
                    rusqlite::params![display_path, path_native, path_key, stem_key, title, stable_id, stable_id_key, content_hash.as_bytes(), size, mtime_ns, scan_ns, frontmatter_json, flavite_syntax::text::safe_prefix(parsed.snapshot.text(), 1024), i64::try_from(counters.generation).map_err(|error| error.to_string())?, encoding_code, bom],
                ).map_err(|error| error.to_string())?;
                let note_id: i64 = tx
                    .query_row(
                        "SELECT note_id FROM notes WHERE path_native = ?1",
                        [path_native],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM links WHERE src = ?1", [note_id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM tags WHERE note_id = ?1", [note_id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM aliases WHERE note_id = ?1", [note_id])
                    .map_err(|error| error.to_string())?;
                for (ord, link) in parsed.links.iter().enumerate() {
                    let source_span = snapshot
                        .span_to_source(link.span)
                        .ok_or("link span cannot be mapped to source bytes")?;
                    let target_key = flavite_syntax::text::fold_key(&link.target_raw);
                    let target_stem = target_stem_key(&link.target_raw, &target_key);
                    plan.fresh_target_keys.insert(target_key.clone());
                    tx.execute(
                        "INSERT INTO links(src,ord,kind,dst_raw,target_key,target_stem,anchor,alias,by_id,span_start,span_end,dst_note,dst_att,res_state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,NULL,'unresolved')",
                        rusqlite::params![note_id, i64::try_from(ord).map_err(|error| error.to_string())?, if link.is_embed { "embed" } else { "link" }, link.target_raw, target_key, target_stem, link.anchor, link.alias, i64::from(link.by_id), i64::try_from(source_span.start.0).map_err(|error| error.to_string())?, i64::try_from(source_span.end.0).map_err(|error| error.to_string())?],
                    ).map_err(|error| error.to_string())?;
                }
                for tag in &parsed.tags {
                    let value = parsed
                        .snapshot
                        .text()
                        .get(tag.start.0..tag.end.0)
                        .ok_or("tag span is invalid")?;
                    tx.execute(
                        "INSERT OR IGNORE INTO tags(note_id,tag,source) VALUES(?1,?2,'inline')",
                        rusqlite::params![note_id, value.trim_start_matches('#')],
                    )
                    .map_err(|error| error.to_string())?;
                }
                for tag in &frontmatter_tags {
                    tx.execute(
                        "INSERT OR IGNORE INTO tags(note_id,tag,source) VALUES(?1,?2,'frontmatter')",
                        rusqlite::params![note_id, flavite_syntax::wellknown::normalize_tag(tag)],
                    )
                    .map_err(|error| error.to_string())?;
                }
                for alias in &aliases {
                    tx.execute(
                        "INSERT OR IGNORE INTO aliases(note_id,alias,alias_key) VALUES(?1,?2,?3)",
                        rusqlite::params![note_id, alias, flavite_syntax::wellknown::normalize_tag(alias)],
                    )
                    .map_err(|error| error.to_string())?;
                }
                tx.execute("DELETE FROM properties WHERE note_id=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                if let Some(frontmatter) = &frontmatter {
                    for (key, values) in &frontmatter.properties {
                        let mut ordinal: i64 = 0;
                        let mut pending: Vec<_> = values.iter().rev().map(|(_, value)| value).collect();
                        while let Some(value) = pending.pop() {
                            use flavite_syntax::frontmatter::PropertyValue as V;
                            let (kind, text, number) = match value {
                                V::Text(value) | V::Raw(value) => ("text", Some(value.clone()), None),
                                V::Number(value) => (
                                    "num",
                                    None,
                                    value.parse::<f64>().ok().filter(|v| v.is_finite()),
                                ),
                                V::Bool(value) => (
                                    "bool",
                                    Some(value.to_string()),
                                    Some(f64::from(u8::from(*value))),
                                ),
                                V::Date(value) => ("date", Some(value.clone()), None),
                                V::DateTime(value) => ("datetime", Some(value.clone()), None),
                                V::Null => ("null", None, None),
                                V::List(values) => {
                                    pending.extend(values.iter().rev());
                                    continue;
                                }
                            };
                            tx.execute("INSERT INTO properties(note_id,key,ord,vtype,v_text,v_num) VALUES(?1,?2,?3,?4,?5,?6)", rusqlite::params![note_id, key, ordinal, kind, text, number]).map_err(|error| error.to_string())?;
                            ordinal += 1;
                        }
                    }
                }
                tx.execute(
                    "INSERT INTO search_docs(note_id,content_hash,body_text) VALUES(?1,?2,?3) ON CONFLICT(note_id) DO UPDATE SET content_hash=excluded.content_hash,body_text=excluded.body_text",
                    rusqlite::params![note_id, content_hash.as_bytes(), parsed.snapshot.text()],
                )
                .map_err(|error| error.to_string())?;
                // The envelope carries this generation's immutable document
                // content (D-07): no later reread of a newer `search_docs`
                // row can leak into it.
                let mut doc_tags: Vec<String> = parsed
                    .tags
                    .iter()
                    .filter_map(|tag| parsed.snapshot.text().get(tag.start.0..tag.end.0))
                    .map(|value| value.trim_start_matches('#').to_owned())
                    .collect();
                doc_tags.extend(frontmatter_tags.iter().map(|tag| {
                    flavite_syntax::wellknown::normalize_tag(tag)
                }));
                plan.search_ops.push(SearchDocOp::Upsert {
                    note_id,
                    content_hash: *content_hash.as_bytes(),
                    title: title.clone(),
                    aliases: aliases.clone(),
                    tags: doc_tags,
                    body: parsed.snapshot.text().to_owned(),
                });
                plan.post = Some(EntityPre::Note(NoteImage {
                    id: note_id,
                    path: display_path.to_owned(),
                    path_native: path_native.to_vec(),
                    path_key: path_key.clone(),
                    stem_key: stem_key.clone(),
                    title: title.clone(),
                    aliases: alias_keys.clone(),
                    stable_id: stable_id.clone(),
                    stable_id_key: stable_id_key.clone(),
                    id_conflict: false,
                    content_hash: Some(content_hash.as_bytes().to_vec()),
                }));
                plan.entity_rows.push((ChangeKind::Note, note_id));
                plan.touched_notes.insert(note_id);
                plan.conflict_row = Some(note_id);
                Ok(CoreStep::Proceed)
            },
        )?;
        match outcome {
            SpineOutcome::RolledBack => Err("parsed note apply cannot be a no-op".to_owned()),
            SpineOutcome::FastPath { epoch, generation } => Ok((epoch, generation)),
            SpineOutcome::Applied { epoch, generation } => {
                // The document content already went out with this
                // generation's ordered envelope (D-07).
                let _ = &plan;
                Ok((epoch, generation))
            }
        }
    }

    /// Apply parse-failed note bytes using the same title-only degraded row
    /// semantics as `apply_saved_note`.
    ///
    /// # Errors
    /// Returns an error if any SQLite step fails. A failed transaction leaves
    /// the prior structural state intact.
    pub fn apply_degraded_saved_note(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        bytes: &[u8],
        mtime_ns: i64,
        scan_ns: i64,
    ) -> Result<(u64, u64), String> {
        // §6.10: classify the strict decode explicitly. A source that fails
        // strict decoding is failed/unparsed with a typed decode status —
        // never an empty success — and carries no decodable preview.
        let decoded = flavite_encoding::decode(
            bytes,
            flavite_encoding::DecodePolicy::Auto,
            &flavite_encoding::DecodeLimits::default(),
            &flavite_encoding::NOT_CANCELLED,
        );
        let (preview, decode_status, encoding_code, bom) = match decoded {
            Ok(content) => (
                Some(flavite_syntax::text::safe_prefix(content.text(), 1024).to_owned()),
                "ok",
                content.format().encoding_code(),
                content.format().bom(),
            ),
            Err(_) => (None, "undecodable", "unknown", false),
        };
        self.apply_unparsed_note_inner(
            path_native,
            display_path,
            u64::try_from(bytes.len()).map_err(|error| error.to_string())?,
            mtime_ns,
            scan_ns,
            preview.as_deref(),
            decode_status,
            encoding_code,
            bom,
        )
    }

    /// Record a note whose bytes failed strict decoding (RFC-0006 §6.10).
    /// The row remains visible/searchable by title with `decode_status =
    /// 'undecodable'`; its bytes are never interpreted lossily.
    ///
    /// # Errors
    /// Returns an error when metadata conversion or an SQLite/search update fails.
    pub fn apply_undecodable_note(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        size: u64,
        mtime_ns: i64,
        scan_ns: i64,
    ) -> Result<(u64, u64), String> {
        self.apply_unparsed_note_inner(
            path_native,
            display_path,
            size,
            mtime_ns,
            scan_ns,
            None,
            "undecodable",
            "unknown",
            false,
        )
    }

    /// Record a note that is too large or cannot be parsed without retaining
    /// its content. The row remains visible and searchable by title only.
    ///
    /// # Errors
    /// Returns an error when metadata conversion or an SQLite/search update fails.
    pub fn apply_unparsed_note(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        size: u64,
        mtime_ns: i64,
        scan_ns: i64,
    ) -> Result<(u64, u64), String> {
        self.apply_unparsed_note_inner(
            path_native,
            display_path,
            size,
            mtime_ns,
            scan_ns,
            None,
            "not-decoded",
            "unknown",
            false,
        )
    }

    #[allow(clippy::too_many_arguments)] // decode-status/encoding columns ride along (§6.10)
    fn apply_unparsed_note_inner(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        size: u64,
        mtime_ns: i64,
        scan_ns: i64,
        preview: Option<&str>,
        decode_status: &'static str,
        encoding_code: &str,
        bom: bool,
    ) -> Result<(u64, u64), String> {
        let stem = Path::new(display_path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(display_path);
        let size = i64::try_from(size).map_err(|error| error.to_string())?;
        let (outcome, plan) =
            self.apply_entity_core(true, false, path_native, |tx, _pre, counters, plan| {
                let unchanged: bool = tx
                    .query_row(
                        "SELECT parse_ok=0 AND size=?1 AND mtime_ns=?2 AND decode_status=?4 FROM notes WHERE path_native=?3",
                        rusqlite::params![size, mtime_ns, path_native, decode_status],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?
                    .unwrap_or(false);
                if unchanged {
                    tx.execute(
                        "UPDATE notes SET scan_ns=?1 WHERE path_native=?2",
                        rusqlite::params![scan_ns, path_native],
                    )
                    .map_err(|error| error.to_string())?;
                    return Ok(CoreStep::FastPath);
                }
                reserve_graph_identity(tx, true, true)?;
                tx.execute("INSERT INTO notes(note_id,path,path_native,path_key,stem_key,title,stable_id,stable_id_key,id_conflict,content_hash,size,mtime_ns,scan_ns,frontmatter,preview,parse_ok,generation,encoding,bom,decode_status) VALUES((SELECT CAST(value AS INTEGER) FROM meta WHERE key='note_identity_high_water'),?1,?2,?3,?4,?5,NULL,NULL,0,NULL,?6,?7,?8,NULL,?9,0,?10,?11,?12,?13) ON CONFLICT(path_native) DO UPDATE SET path=excluded.path,path_key=excluded.path_key,stem_key=excluded.stem_key,title=excluded.title,stable_id=NULL,stable_id_key=NULL,content_hash=NULL,size=excluded.size,mtime_ns=excluded.mtime_ns,scan_ns=excluded.scan_ns,frontmatter=NULL,preview=excluded.preview,parse_ok=0,generation=excluded.generation,encoding=excluded.encoding,bom=excluded.bom,decode_status=excluded.decode_status", rusqlite::params![display_path,path_native,flavite_syntax::text::fold_key(display_path),flavite_syntax::text::fold_key(stem),stem,size,mtime_ns,scan_ns,preview,i64::try_from(counters.generation).map_err(|error| error.to_string())?,encoding_code,i64::from(bom),decode_status]).map_err(|error| error.to_string())?;
                let note_id: i64 = tx
                    .query_row(
                        "SELECT note_id FROM notes WHERE path_native=?1",
                        [path_native],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM links WHERE src=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM tags WHERE note_id=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM aliases WHERE note_id=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM properties WHERE note_id=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                tx.execute("DELETE FROM search_docs WHERE note_id=?1", [note_id])
                    .map_err(|error| error.to_string())?;
                // Degraded rows carry no search_docs body; the zero hash
                // records "not body-backed" (D-07 envelope content rule).
                plan.search_ops.push(SearchDocOp::TitleOnly {
                    note_id,
                    content_hash: [0u8; 32],
                    title: stem.to_owned(),
                });
                plan.post = Some(EntityPre::Note(NoteImage {
                    id: note_id,
                    path: display_path.to_owned(),
                    path_native: path_native.to_vec(),
                    path_key: flavite_syntax::text::fold_key(display_path),
                    stem_key: flavite_syntax::text::fold_key(stem),
                    title: stem.to_owned(),
                    aliases: Vec::new(),
                    stable_id: None,
                    stable_id_key: None,
                    id_conflict: false,
                    content_hash: None,
                }));
                plan.entity_rows.push((ChangeKind::Note, note_id));
                plan.touched_notes.insert(note_id);
                plan.conflict_row = Some(note_id);
                Ok(CoreStep::Proceed)
            })?;
        match outcome {
            SpineOutcome::RolledBack => Err("unparsed note apply cannot be a no-op".to_owned()),
            SpineOutcome::FastPath { epoch, generation } => Ok((epoch, generation)),
            SpineOutcome::Applied { epoch, generation } => {
                // The title-only document already went out with this
                // generation's ordered envelope (D-07).
                let _ = &plan;
                Ok((epoch, generation))
            }
        }
    }

    /// Insert or refresh one attachment record. Bytes are hashed only when
    /// hydrated and at or below [`crate::types::ATTACHMENT_HASH_MAX`].
    ///
    /// # Errors
    /// Returns an error if SQLite, metadata conversion, or link resolution fails.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_saved_attachment(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        bytes: Option<&[u8]>,
        size: u64,
        mtime_ns: i64,
        scan_ns: i64,
        hydrated: bool,
    ) -> Result<(u64, u64), String> {
        let path = Path::new(display_path);
        let stem = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(display_path);
        let path_key = flavite_syntax::text::fold_key(display_path);
        let stem_key = flavite_syntax::text::fold_key(stem);
        let ext = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let kind = match ext.as_str() {
            "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "avif" | "bmp" | "ico" => {
                AttachmentKind::Image
            }
            "pdf" => AttachmentKind::Pdf,
            "mp3" | "wav" | "ogg" | "flac" | "m4a" => AttachmentKind::Audio,
            "mp4" | "webm" | "mov" | "mkv" => AttachmentKind::Video,
            "zip" | "tar" | "gz" | "tgz" => AttachmentKind::Archive,
            _ => AttachmentKind::Other,
        };
        let content_hash = if hydrated && size <= crate::types::ATTACHMENT_HASH_MAX as u64 {
            bytes
                .filter(|bytes| bytes.len() <= crate::types::ATTACHMENT_HASH_MAX)
                .map(|bytes| blake3::hash(bytes).as_bytes().to_vec())
        } else {
            None
        };
        let size = i64::try_from(size).map_err(|error| error.to_string())?;
        let (outcome, _plan) = self.apply_entity_core(
            false,
            false,
            path_native,
            |tx, _pre, counters, plan| {
                reserve_graph_identity(tx, false, true)?;
                tx.execute(
                    "INSERT INTO attachments(att_id,path,path_native,path_key,stem_key,kind,mime,size,content_hash,width,height,mtime_ns,scan_ns,hydrated,generation) VALUES((SELECT CAST(value AS INTEGER) FROM meta WHERE key='attachment_identity_high_water'),?1,?2,?3,?4,?5,NULL,?6,?7,NULL,NULL,?8,?9,?10,?11) ON CONFLICT(path_native) DO UPDATE SET path=excluded.path,path_key=excluded.path_key,stem_key=excluded.stem_key,kind=excluded.kind,size=excluded.size,content_hash=excluded.content_hash,mtime_ns=excluded.mtime_ns,scan_ns=excluded.scan_ns,hydrated=excluded.hydrated,generation=excluded.generation",
                    rusqlite::params![display_path,path_native,path_key,stem_key,kind.as_str(),size,content_hash,mtime_ns,scan_ns,i64::from(hydrated),i64::try_from(counters.generation).map_err(|error| error.to_string())?],
                ).map_err(|error| error.to_string())?;
                let att_id: i64 = tx
                    .query_row(
                        "SELECT att_id FROM attachments WHERE path_native=?1",
                        [path_native],
                        |row| row.get(0),
                    )
                    .map_err(|error| error.to_string())?;
                plan.post = Some(EntityPre::Attachment(AttImage {
                    id: att_id,
                    path: display_path.to_owned(),
                    path_native: path_native.to_vec(),
                    path_key: path_key.clone(),
                    stem_key: stem_key.clone(),
                }));
                plan.entity_rows.push((ChangeKind::Attachment, att_id));
                plan.touched_attachments.insert(att_id);
                Ok(CoreStep::Proceed)
            },
        )?;
        match outcome {
            SpineOutcome::RolledBack | SpineOutcome::FastPath { .. } => {
                Err("attachment apply cannot be a no-op".to_owned())
            }
            SpineOutcome::Applied { epoch, generation } => Ok((epoch, generation)),
        }
    }

    /// Remove an attachment and re-resolve embeds that may have targeted it.
    ///
    /// # Errors
    /// Returns an error if SQLite or link resolution fails.
    pub fn remove_attachment_path(&mut self, path_native: &[u8]) -> Result<bool, String> {
        let (outcome, plan) =
            self.apply_entity_core(false, true, path_native, |tx, pre, _counters, plan| {
                let Some(EntityPre::Attachment(image)) = pre else {
                    return Err("missing attachment pre-image".to_owned());
                };
                let id = image.id;
                tx.execute("DELETE FROM attachments WHERE att_id=?1", [id])
                    .map_err(|error| error.to_string())?;
                plan.entity_rows.push((ChangeKind::Attachment, id));
                plan.touched_attachments.insert(id);
                Ok(CoreStep::Proceed)
            })?;
        match outcome {
            SpineOutcome::RolledBack => Ok(false),
            SpineOutcome::Applied { .. } => {
                debug_assert!(!plan.entity_rows.is_empty());
                Ok(true)
            }
            SpineOutcome::FastPath { .. } => {
                Err("attachment removal cannot take a fast path".to_owned())
            }
        }
    }

    /// Drain committed-version acknowledgements from the ordered writer and
    /// advance the advisory `tantivy_committed_generation` metadata
    /// monotonically through the sole SQL owner, only after Tantivy
    /// durability/reload (D-07). Desync reports are answered with a
    /// cache-based rebuild — never gap skipping.
    ///
    /// # Errors
    /// Returns the error text when metadata updates or a rebuild fail.
    pub fn drain_search_acks(&mut self) -> Result<(), String> {
        let Some(handle) = &self.search_writer else {
            return Ok(());
        };
        for ack in handle.drain_acks() {
            let current = committed_of(self.writer.conn());
            if ack.generation >= current {
                // The SQLite metadata row is the authoritative committed
                // marker (read by startup and capture provenance); the old
                // write-only checkpoint file — a per-ack fsync on this SQL
                // owner thread — had no readers and is gone.
                crate::schema::set_meta(
                    self.writer.conn(),
                    MetaKey::TantivyCommittedGeneration,
                    &ack.generation.to_string(),
                )
                .map_err(|error| error.to_string())?;
            }
        }
        if handle
            .drain_reports()
            .iter()
            .any(|report| matches!(report, WriterReport::Desynced))
        {
            let Some(handle) = self.search_writer.as_ref() else {
                return Ok(());
            };
            let epoch = handle.epoch();
            let generation = crate::schema::get_meta_u64(self.writer.conn(), MetaKey::Generation)
                .map_err(|error| error.to_string())?
                .unwrap_or(0);
            let docs = crate::search_writer::read_rebuild_docs(self.writer.conn())?;
            handle.rebuild_blocking(epoch, generation, docs.into())?;
        }
        Ok(())
    }

    /// The single ordered search writer handle (ordered capture enqueue,
    /// acknowledgements, capacity reservation); `None` on a secondary
    /// handle whose index writer belongs elsewhere.
    #[must_use]
    pub const fn search_writer(&self) -> Option<&SearchWriterHandle> {
        self.search_writer.as_ref()
    }

    /// A bounded native-identity note catalog for workspace shells and
    /// note pickers. Ordering is the canonical path order; entries carry
    /// the raw native path bytes so consumers never reconstruct identity
    /// from a display string.
    ///
    /// # Errors
    /// Returns the underlying error text when SQLite cannot be read.
    #[allow(clippy::too_many_lines)] // One read transaction binds metadata, page and aliases.
    pub fn note_catalog(
        &self,
        limit: usize,
        after: Option<NoteCatalogCursor>,
    ) -> Result<NoteCatalogPage, String> {
        let conn = self
            .read_pool
            .get(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
        let epoch = crate::schema::get_meta_u64(&tx, crate::schema::MetaKey::Epoch)
            .map_err(|e| e.to_string())?
            .unwrap_or(1);
        let generation = crate::schema::get_meta_u64(&tx, crate::schema::MetaKey::Generation)
            .map_err(|e| e.to_string())?
            .unwrap_or(0);
        if !(1..=500).contains(&limit) {
            return Err("catalog page limit must be 1..=500".into());
        }
        if let Some(cursor) = &after {
            if cursor.native_path.len() > 16 * 1024 {
                return Err("catalog cursor exceeds native-key limit".into());
            }
            if (cursor.epoch, cursor.generation) != (epoch, generation) {
                return Err("catalog reset required".into());
            }
        }
        let total: i64 = tx
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        let mut statement = tx
            .prepare("SELECT note_id, path, path_native, title, stable_id, id_conflict FROM notes WHERE ?1 OR (path_native, note_id) > (?2, ?3) ORDER BY path_native, note_id LIMIT ?4")
            .map_err(|e| e.to_string())?;
        let bounded_limit =
            i64::try_from(limit).map_err(|_| "note catalog limit does not fit i64".to_owned())?;
        let (first, native_path, note_id) = after.map_or((true, Vec::new(), 0), |cursor| {
            (false, cursor.native_path, cursor.note_id)
        });
        let rows = statement
            .query_map(
                rusqlite::params![first, native_path, note_id, bounded_limit + 1],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        NoteCatalogEntry {
                            path: row.get(1)?,
                            path_native: row.get(2)?,
                            title: row.get(3)?,
                            aliases: Vec::new(),
                            stable_id: if row.get::<_, bool>(5)? {
                                None
                            } else {
                                row.get(4)?
                            },
                        },
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        let mut indexed = rows
            .map(|row| row.map_err(|e| e.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        let has_more = indexed.len() > limit;
        indexed.truncate(limit);
        let next = if has_more {
            indexed.last().map(|(id, entry)| NoteCatalogCursor {
                epoch,
                generation,
                native_path: entry.path_native.clone(),
                note_id: *id,
            })
        } else {
            None
        };
        drop(statement);
        let note_ids = indexed.iter().map(|(id, _)| *id).collect::<Vec<_>>();
        let mut aliases = std::collections::HashMap::<i64, Vec<String>>::new();
        // Alias lookup remains bounded to this page and keeps parameter
        // batches below SQLite's variable limit.
        for chunk in note_ids.chunks(900) {
            if chunk.is_empty() {
                continue;
            }
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT note_id, alias FROM aliases WHERE note_id IN ({placeholders}) ORDER BY note_id, alias_key"
            );
            let mut statement = tx.prepare(&sql).map_err(|e| e.to_string())?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (note_id, alias) = row.map_err(|e| e.to_string())?;
                aliases.entry(note_id).or_default().push(alias);
            }
        }
        let items = indexed
            .drain(..)
            .map(|(note_id, mut entry)| {
                entry.aliases = aliases.remove(&note_id).unwrap_or_default();
                entry
            })
            .collect();
        tx.commit().map_err(|e| e.to_string())?;
        Ok(NoteCatalogPage {
            next,
            items,
            epoch,
            generation,
            total: u64::try_from(total.max(0)).unwrap_or(0),
        })
    }

    /// Search generation counters used by diagnostics and tests.
    ///
    /// # Errors
    /// Returns an error when SQLite generation metadata cannot be read.
    pub fn search_generations(&self) -> Result<(u64, u64), String> {
        Ok((
            crate::schema::get_meta_u64(self.writer.conn(), MetaKey::Generation)
                .map_err(|e| e.to_string())?
                .unwrap_or(0),
            self.search_writer
                .as_ref()
                .map_or(0, SearchWriterHandle::committed_frontier),
        ))
    }

    /// Evaluate a saved-content search against a freshly captured coherent
    /// snapshot, in-process. This is the synchronous seam for tests, benches
    /// and the graph filter membership path: the caller waits only on its
    /// own thread for the ordered writer to hand back the G-stamped
    /// searcher; production search sessions use the async handshake in
    /// [`crate::search_session`] through the API service instead.
    ///
    /// # Errors
    /// Returns the typed reset/error text when the capture or evaluation
    /// fails.
    pub fn evaluate_saved_search(
        &mut self,
        query: &crate::query::Query,
        limit: usize,
    ) -> Result<SearchPage, String> {
        self.drain_search_acks()?;
        let Some(handle) = &self.search_writer else {
            return Err("this index handle does not own the search writer".to_owned());
        };
        // D-07 (AC-9): pin the SQL read snapshot FIRST and capture at its
        // committed meta. The capture reply carries the writer's actual
        // frontier; a reply stamped at any other epoch/generation means a
        // commit raced the capture, and returning it would be the mixed
        // G/G+1 bundle the contract forbids — refuse with a retryable busy
        // instead.
        let conn = self
            .read_pool
            .get(Some(Duration::from_secs(2)))
            .map_err(|error| error.to_string())?;
        let tx = conn
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let epoch = crate::schema::get_meta_u64(&tx, MetaKey::Epoch)
            .map_err(|error| error.to_string())?
            .unwrap_or(1);
        let generation = crate::schema::get_meta_u64(&tx, MetaKey::Generation)
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        let outcome = handle.capture_blocking(epoch, generation);
        let searcher = match outcome? {
            crate::search_writer::CaptureOutcome::Searcher {
                epoch: stamped_epoch,
                generation: stamped_generation,
                searcher,
            } => {
                if (stamped_epoch, stamped_generation) != (epoch, generation) {
                    return Err("busy: search snapshot advanced during capture; retry".to_owned());
                }
                searcher
            }
            crate::search_writer::CaptureOutcome::ResetRequired => {
                return Err(crate::types::IndexError::ResetRequired.to_string());
            }
        };
        let universe =
            crate::search_eval::Universe::from_tx(&tx).map_err(|error| error.to_string())?;
        let snapshot = crate::search_eval::Snapshot {
            universe: &universe,
            searcher: &searcher,
            tx: &tx,
        };
        let mut rows =
            crate::search_eval::evaluate(query, &snapshot).map_err(|error| error.to_string())?;
        // The evaluation is complete; `has_more` never silently truncates —
        // the page reports the probe against the caller's limit (§6.3).
        let has_more = rows.len() > limit;
        rows.truncate(limit);
        let scored: Vec<(i64, Option<f32>)> =
            rows.iter().map(|row| (row.note_id, row.score)).collect();
        let terms: Vec<String> = text_terms(query);
        let items = self.search_note_rows(&scored, &terms)?;
        Ok(SearchPage {
            epoch,
            generation,
            items,
            has_more,
        })
    }

    /// Term/conjunction text search returning bare IDs — the degraded and
    /// title-only search assertions use this seam (D-08/D-09); scores are
    /// dropped.
    ///
    /// # Errors
    /// Returns the evaluation error text.
    #[cfg(test)]
    pub fn search_text_ids(
        &mut self,
        text: &str,
        _note_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>, String> {
        let page = self.evaluate_saved_search(
            &crate::query::Query::Text(crate::query::TextLeaf::all_fields(text)),
            limit,
        )?;
        Ok(page.items.into_iter().map(|row| row.note_id).collect())
    }

    /// Structural-only page for the general query endpoint (§6.1): typed
    /// AST in, keyset-paged SQL out. Text predicates are rejected
    /// explicitly — never an empty success (D-07/§6.3).
    ///
    /// # Errors
    /// Returns the typed text rejection or the underlying SQL error text.
    pub fn structural_page(
        &self,
        query: &crate::query::Query,
        limit: usize,
    ) -> Result<SearchPage, String> {
        if query.contains_text() {
            return Err(crate::types::IndexError::UnsupportedTextPredicate.to_string());
        }
        let conn = self
            .read_pool
            .get(Some(Duration::from_secs(2)))
            .map_err(|error| error.to_string())?;
        let page = query
            .execute(&conn, limit.saturating_add(1), None)
            .map_err(|error| error.to_string())?;
        drop(conn);
        // Same `limit+1` probe semantics the text path had (§6.3): the page
        // was fetched one deep; scores are `None` for structural results.
        let has_more = page.items.len() > limit;
        let scored: Vec<(i64, Option<f32>)> = page
            .items
            .into_iter()
            .take(limit)
            .map(|note| (note.note_id, None))
            .collect();
        let rows = self.search_note_rows(&scored, &[])?;
        Ok(SearchPage {
            epoch: page.epoch,
            generation: page.generation,
            items: rows,
            has_more,
        })
    }

    /// Assemble result rows from the hit list with one batched query per
    /// 512 ids (§6.3 — replaces the per-row N+1 assembly), restoring hit
    /// order through an id map. Bounded excerpts around the first matched
    /// term come from the indexed saved-body cache; unsaved edits are
    /// deliberately absent (D-08, labeled by the UI). Excerpt logic itself
    /// is unchanged (`build_excerpt`).
    fn search_note_rows(
        &self,
        hits: &[(i64, Option<f32>)],
        terms: &[String],
    ) -> Result<Vec<SearchNote>, String> {
        const ROW_CHUNK: usize = 512;
        let conn = self
            .read_pool
            .get(Some(Duration::from_secs(2)))
            .map_err(|error| error.to_string())?;
        let mut found: HashMap<i64, (String, String, Option<String>)> = HashMap::new();
        for chunk in hits.chunks(ROW_CHUNK) {
            let placeholders = (1..=chunk.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT n.note_id, n.path, n.title, d.body_text \
                 FROM notes n LEFT JOIN search_docs d ON d.note_id = n.note_id \
                 WHERE n.note_id IN ({placeholders})"
            );
            let rows = {
                let mut statement = conn.prepare(&sql).map_err(|error| error.to_string())?;
                statement
                    .query_map(
                        rusqlite::params_from_iter(chunk.iter().map(|(id, _)| id)),
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, Option<String>>(3)?,
                            ))
                        },
                    )
                    .map_err(|error| error.to_string())?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| error.to_string())?
            };
            for (note_id, path, title, body) in rows {
                found.insert(note_id, (path, title, body));
            }
        }
        let mut rows = Vec::with_capacity(hits.len());
        for (note_id, score) in hits {
            // Deleted between fetch and assembly (or never present): omit,
            // exactly as the historical per-row lookup did for a miss.
            if let Some((path, title, body)) = found.get(note_id) {
                let excerpt = build_excerpt(body.as_deref(), terms);
                rows.push(SearchNote {
                    note_id: *note_id,
                    path: path.clone(),
                    title: title.clone(),
                    excerpt,
                    score: *score,
                });
            }
        }
        Ok(rows)
    }

    /// Current structural index version.
    ///
    /// # Errors
    /// Returns an error if either version counter cannot be read from SQLite.
    pub fn version(&self) -> Result<(u64, u64), String> {
        let conn = self.writer.conn();
        Ok((
            get_meta_u64(conn, MetaKey::Epoch)
                .map_err(|e| e.to_string())?
                .unwrap_or(1),
            get_meta_u64(conn, MetaKey::Generation)
                .map_err(|e| e.to_string())?
                .unwrap_or(0),
        ))
    }
}

/// Test burst seam for the D-07 admission contract: writer backpressure
/// refuses the index admission (never after the durable write); a test
/// burst retries the cache application instead of failing.
#[cfg(test)]
impl IndexActor {
    fn remove_note_path_retried(&mut self, path_native: &[u8]) -> Result<bool, String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match self.remove_note_path(path_native) {
                Ok(removed) => return Ok(removed),
                Err(error)
                    if error.contains("queue is full") || error.contains("budget exceeded") =>
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "search writer never drained its envelope queue: {error}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => return Err(error),
            }
        }
    }

    fn apply_saved_note_retried(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        bytes: &[u8],
        mtime_ns: i64,
        scan_ns: i64,
    ) -> Result<(u64, u64), String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match self.apply_saved_note(path_native, display_path, bytes, mtime_ns, scan_ns) {
                Ok(applied) => return Ok(applied),
                Err(error)
                    if error.contains("queue is full") || error.contains("budget exceeded") =>
                {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "search writer never drained its envelope queue: {error}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_schema;
    use rusqlite::params;
    use tempfile::NamedTempFile;

    #[test]
    fn test_pool_creation() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path();

        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();
        drop(conn);

        let pool = ReadPool::new(path, DEFAULT_POOL_SIZE).unwrap();

        let stats = pool.stats().unwrap();
        assert_eq!(stats.available, DEFAULT_POOL_SIZE);
        assert_eq!(stats.in_use, 0);
        assert_eq!(stats.max_size, DEFAULT_POOL_SIZE);
    }

    #[test]
    fn metadata_fast_path_rehashes_recently_modified_notes() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("index.sqlite3");
        drop(crate::open_or_init(&db_path).unwrap());
        let mut index = IndexActor::new(&db_path, 1).unwrap();
        index
            .apply_saved_note_retried(b"a.md", "a.md", b"body", 10_000_000_000, 10_000_000_000)
            .unwrap();
        assert!(
            !index
                .note_metadata_matches(b"a.md", 4, 10_000_000_000, 11_000_000_000)
                .unwrap()
        );
        assert!(
            index
                .note_metadata_matches(b"a.md", 4, 10_000_000_000, 13_000_000_000)
                .unwrap()
        );
    }

    #[test]
    fn test_get_return() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path();

        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();
        drop(conn);

        let pool = ReadPool::new(path, DEFAULT_POOL_SIZE).unwrap();

        let pc = pool.get(None).unwrap();
        let stats = pool.stats().unwrap();
        assert_eq!(stats.available, DEFAULT_POOL_SIZE - 1);
        assert_eq!(stats.in_use, 1);

        drop(pc);

        let stats = pool.stats().unwrap();
        assert_eq!(stats.available, DEFAULT_POOL_SIZE);
        assert_eq!(stats.in_use, 0);
    }

    #[test]
    fn test_pool_grow() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path();

        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();
        drop(conn);

        let pool = ReadPool::new(path, 2).unwrap();

        let pc1 = pool.get(None).unwrap();
        let pc2 = pool.get(None).unwrap();

        // Pool should grow to max_size
        let pc3 = pool.get(None).unwrap();

        let stats = pool.stats().unwrap();
        assert_eq!(stats.in_use, 3);
        assert_eq!(stats.max_size, 2);

        drop(pc1);
        drop(pc2);
        drop(pc3);
    }

    #[test]
    fn test_writer_creation() {
        let temp = NamedTempFile::new().unwrap();
        let path = temp.path();

        let writer = Writer::new(path).unwrap();

        // Verify WAL mode is enabled
        let wal_mode: String = writer
            .conn()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();

        assert_eq!(wal_mode, "wal");
    }

    #[test]
    fn catalog_keyset_pages_replay_and_reject_changed_versions() {
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("catalog.sqlite3");
        drop(crate::schema::open_or_init(&db).unwrap());
        let mut index = IndexActor::new(&db, 1).unwrap();
        for ordinal in (0..603).rev() {
            let path = format!("note-{ordinal:04}.md");
            index
                .apply_saved_note_retried(
                    path.as_bytes(),
                    &path,
                    b"---
aliases: [Alias]
---
body",
                    1,
                    1,
                )
                .unwrap();
        }
        assert!(index.note_catalog(0, None).is_err());
        assert!(index.note_catalog(501, None).is_err());
        let first = index.note_catalog(500, None).unwrap();
        assert_eq!(first.items.len(), 500);
        let cursor = first.next.unwrap();
        let second = index.note_catalog(500, Some(cursor.clone())).unwrap();
        let replay = index.note_catalog(500, Some(cursor.clone())).unwrap();
        assert_eq!(second.items.len(), 103);
        assert!(second.next.is_none());
        assert_eq!(
            second
                .items
                .iter()
                .map(|e| &e.path_native)
                .collect::<Vec<_>>(),
            replay
                .items
                .iter()
                .map(|e| &e.path_native)
                .collect::<Vec<_>>()
        );
        assert!(second.items.iter().all(|e| e.aliases == ["Alias"]));
        let all = first
            .items
            .iter()
            .chain(&second.items)
            .map(|e| e.path_native.clone())
            .collect::<Vec<_>>();
        assert!(all.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(all.len(), 603);
        index
            .apply_saved_note_retried(b"new.md", "new.md", b"new", 2, 2)
            .unwrap();
        assert!(
            index
                .note_catalog(500, Some(cursor))
                .unwrap_err()
                .contains("reset required")
        );
    }

    #[test]
    fn saved_note_updates_structure_search_cache_and_generation_atomically() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("index.sqlite3");
        drop(crate::schema::open_or_init(&db_path).unwrap());
        let mut index = IndexActor::new(&db_path, 1).unwrap();
        let original = b"---\ntitle: Alpha\naliases: [A, B]\ntags: [frontmatter]\n---\n# Heading\n\n[[Beta|alias]] ![[image.png]] #project";
        let (epoch, generation) = index
            .apply_saved_note_retried(b"notes/a.md", "notes/a.md", original, 98, 99)
            .unwrap();
        assert_eq!((epoch, generation), (1, 1));
        let catalog = index.note_catalog(50, None).unwrap();
        assert_eq!(catalog.items.len(), 1);
        assert_eq!(catalog.items[0].path, "notes/a.md");
        assert_eq!(catalog.items[0].title, "Alpha");
        assert_eq!(catalog.items[0].aliases, ["A", "B"]);
        assert_eq!(catalog.items[0].stable_id, None);

        {
            let conn = index.writer().conn();
            let note: (String, Vec<u8>, i64) = conn
                .query_row(
                    "SELECT title, content_hash, generation FROM notes WHERE path_native=?1",
                    [b"notes/a.md"],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .unwrap();
            assert_eq!(note.0, "Alpha");
            assert_eq!(note.1, blake3::hash(original).as_bytes());
            assert_eq!(note.2, 1);
            let (search_body, link_count, tag_count, changes): (String, i64, i64, i64) = conn
                .query_row(
                    "SELECT s.body_text,
                    (SELECT COUNT(*) FROM links l WHERE l.src=s.note_id),
                    (SELECT COUNT(*) FROM tags t WHERE t.note_id=s.note_id),
                    (SELECT COUNT(*) FROM changes c WHERE c.generation=1)
                 FROM search_docs s",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();
            assert_eq!(search_body, std::str::from_utf8(original).unwrap());
            // S2 (D4/§6.2.3): generation 1 carries the applied note's row only.
            // Both links resolve to nothing, so no link row flipped and no
            // `outgoing` row is emitted — truthful logging, not unconditional.
            assert_eq!((link_count, tag_count, changes), (2, 2, 1));
            let alias_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM aliases", [], |row| row.get(0))
                .unwrap();
            assert_eq!(alias_count, 2);
        }
        assert_eq!(index.version().unwrap(), (1, 1));

        let (_, next_generation) = index
            .apply_saved_note_retried(b"notes/a.md", "notes/a.md", b"# Updated", 100, 101)
            .unwrap();
        assert_eq!(next_generation, 2);
        let conn = index.writer().conn();
        let note_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(note_count, 1);
        let updated_search_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM search_docs WHERE body_text=?1",
                params!["# Updated"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(updated_search_count, 1);
        let (_, unchanged_generation) = index
            .apply_saved_note_retried(b"notes/a.md", "notes/a.md", b"# Updated", 102, 103)
            .unwrap();
        assert_eq!(unchanged_generation, 2);
        assert_eq!(index.version().unwrap(), (1, 2));
        index
            .apply_saved_note_retried(b"notes/b.md", "notes/b.md", &[0xff], 104, 105)
            .unwrap();
        assert_eq!(index.version().unwrap(), (1, 3));
        let parse_ok: bool = index
            .writer()
            .conn()
            .query_row(
                "SELECT parse_ok FROM notes WHERE path_native=?1",
                [b"notes/b.md".as_slice()],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!parse_ok);
    }

    fn searchable_index() -> (tempfile::TempDir, IndexActor) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        drop(crate::schema::open_or_init(&db).unwrap());
        let index = IndexActor::new(&db, 1).unwrap();
        (dir, index)
    }

    #[test]
    fn multibyte_note_over_1kb_saves_with_valid_preview() {
        let (_dir, mut index) = searchable_index();
        let bytes = format!("# CJK\n{}", "界".repeat(600));
        index
            .apply_saved_note_retried(b"cjk.md", "cjk.md", bytes.as_bytes(), 1, 1)
            .unwrap();
        let preview: String = index
            .writer()
            .conn()
            .query_row(
                "SELECT preview FROM notes WHERE path_native=?1",
                [b"cjk.md"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(preview.len() <= 1024);
        assert!(preview.is_char_boundary(preview.len()));
    }

    #[test]
    fn ingest_writes_the_folded_stable_id_key() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_saved_note_retried(b"id.md", "id.md", b"---\nid: Stable-1\n---\n# Id", 1, 1)
            .unwrap();
        let key: Option<String> = index
            .writer()
            .conn()
            .query_row(
                "SELECT stable_id_key FROM notes WHERE path_native=?1",
                [b"id.md"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(key.as_deref(), Some("stable-1"));
    }

    #[test]
    fn unparsed_note_is_listed_and_shrinking_reingests_content() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_unparsed_note(b"large.md", "large.md", 40_000_000, 1, 1)
            .unwrap();
        let row: (bool, Option<Vec<u8>>) = index
            .writer()
            .conn()
            .query_row(
                "SELECT parse_ok,content_hash FROM notes WHERE path_native=?1",
                [b"large.md"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row, (false, None));
        index
            .apply_saved_note_retried(b"large.md", "large.md", b"# Back\n\nreparsedword", 2, 2)
            .unwrap();
        let parse_ok: bool = index
            .writer()
            .conn()
            .query_row(
                "SELECT parse_ok FROM notes WHERE path_native=?1",
                [b"large.md"],
                |row| row.get(0),
            )
            .unwrap();
        assert!(parse_ok);
        assert_eq!(
            index
                .search_text_ids("reparsedword", &[], 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn degraded_note_search_is_title_only_and_matches_after_rebuild() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_unparsed_note(b"degraded.md", "Visible Title.md", 40_000_000, 1, 1)
            .unwrap();
        assert_eq!(index.search_text_ids("Visible", &[], 10).unwrap().len(), 1);
        assert!(
            index
                .search_text_ids("hiddenbody", &[], 10)
                .unwrap()
                .is_empty()
        );
        /* rebuild happens at construct */
        assert_eq!(index.search_text_ids("Visible", &[], 10).unwrap().len(), 1);
        assert!(
            index
                .search_text_ids("hiddenbody", &[], 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn resolver_failure_rolls_back_note_apply() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_saved_note_retried(b"source.md", "source.md", b"[[target]]", 1, 1)
            .unwrap();
        let generation = index.current_version().unwrap().1;
        index
            .writer()
            .conn()
            .execute_batch("CREATE TRIGGER fail_resolution BEFORE UPDATE ON links BEGIN SELECT RAISE(ABORT, 'forced resolver failure'); END;")
            .unwrap();
        assert!(
            index
                .apply_saved_note_retried(b"target.md", "target.md", b"# Target", 2, 2)
                .is_err()
        );
        assert!(!index.contains_note_path(b"target.md").unwrap());
        assert_eq!(index.current_version().unwrap().1, generation);
    }

    #[test]
    fn schema_mismatch_rebuilds_and_parser_version_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        let conn = crate::schema::open_or_init(&db).unwrap();
        crate::schema::set_meta(&conn, MetaKey::SchemaVersion, "2.0").unwrap();
        crate::schema::set_meta(&conn, MetaKey::ParserVersion, "0").unwrap();
        conn.execute("INSERT INTO notes(path,path_native,path_key,stem_key,title,stable_id,stable_id_key,id_conflict,content_hash,size,mtime_ns,scan_ns,frontmatter,preview,parse_ok,generation) VALUES('stale.md',?1,'stale.md','stale','Stale',NULL,NULL,0,NULL,1,1,1,NULL,NULL,0,1)", [b"stale.md"])
            .unwrap();
        drop(conn);

        let conn = crate::schema::open_or_init(&db).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM notes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
        crate::schema::set_meta(&conn, MetaKey::ParserVersion, "0").unwrap();
        drop(conn);

        let mut index = IndexActor::new(&db, 1).unwrap();
        assert!(index.needs_reparse());
        index.mark_parser_current().unwrap();
        assert!(!index.needs_reparse());
    }

    #[test]
    fn acknowledged_save_is_visible_to_immediate_search_and_frontier_converges() {
        let (_dir, mut index) = searchable_index();
        for n in 0..25 {
            let path = format!("notes/{n}.md");
            let body = format!("# Note {n}\n\nunique_saved_content_{n}");
            index
                .apply_saved_note_retried(path.as_bytes(), &path, body.as_bytes(), n, n)
                .unwrap();
        }
        // The mid-stream search captures a coherent snapshot through the
        // ordered writer: the searcher is stamped at the current generation
        // and sees every acknowledged save (RFC-0006 S06, D-07).
        let mid = index
            .evaluate_saved_search(
                &crate::query::Query::Text(crate::query::TextLeaf::all_fields(
                    "unique_saved_content_24",
                )),
                10,
            )
            .unwrap();
        assert_eq!(mid.items.len(), 1);
        for n in 25..50 {
            let path = format!("notes/{n}.md");
            let body = format!("# Note {n}\n\nunique_saved_content_{n}");
            index
                .apply_saved_note_retried(path.as_bytes(), &path, body.as_bytes(), n, n)
                .unwrap();
        }
        let hits = index
            .evaluate_saved_search(
                &crate::query::Query::Text(crate::query::TextLeaf::all_fields(
                    "unique_saved_content_49",
                )),
                10,
            )
            .unwrap();
        assert_eq!(hits.items.len(), 1);
        assert_eq!(hits.items[0].path, "notes/49.md");
        // Commit/reload runs on the writer thread; the acknowledged-version
        // mirror advances monotonically through the SQL owner's ack drain.
        index.drain_search_acks().unwrap();
        assert_eq!(index.search_generations().unwrap(), (50, 50));
    }

    #[test]
    fn envelope_commit_publishes_committed_generation_without_a_timer() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_saved_note_retried(b"timer.md", "timer.md", b"timercommitword", 1, 1)
            .unwrap();
        // The single ordered writer commits each generation's envelope as
        // it arrives; there is no commit-before-search timer anymore (the
        // RFC-0006 S06 contract deleted it).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if index.search_generations().unwrap() == (1, 1) {
                break;
            }
            index.drain_search_acks().unwrap();
            assert!(
                std::time::Instant::now() < deadline,
                "search writer never committed the generation envelope"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn uncommitted_changes_replay_after_reopen_and_counter_converges() {
        let (dir, mut index) = searchable_index();
        let db = dir.path().join("index.sqlite3");
        for n in 0..4 {
            let path = format!("{n}.md");
            let body = format!("# Recovery\n\nreplayneedle{n}");
            index
                .apply_saved_note_retried(path.as_bytes(), &path, body.as_bytes(), n, n)
                .unwrap();
        }
        assert_eq!(index.search_generations().unwrap(), (4, 0));
        drop(index);
        let mut reopened = IndexActor::new(&db, 1).unwrap();
        let result = reopened
            .evaluate_saved_search(
                &crate::query::Query::Text(crate::query::TextLeaf::all_fields("replayneedle3")),
                10,
            )
            .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(reopened.search_generations().unwrap(), (4, 4));
    }

    #[test]
    fn ahead_and_corrupt_search_cache_rebuild_without_touching_structure() {
        let (dir, mut index) = searchable_index();
        let db = dir.path().join("index.sqlite3");
        index
            .apply_saved_note_retried(b"one.md", "one.md", b"# Title\n\nrebuildneedle", 1, 1)
            .unwrap();
        index
            .evaluate_saved_search(
                &crate::query::Query::Text(crate::query::TextLeaf::all_fields("rebuildneedle")),
                10,
            )
            .unwrap();
        drop(index);
        let conn = Connection::open(&db).unwrap();
        crate::schema::set_meta(&conn, MetaKey::TantivyCommittedGeneration, "99").unwrap();
        drop(conn);
        let mut rebuilt = IndexActor::new(&db, 1).unwrap();
        assert_eq!(
            rebuilt
                .evaluate_saved_search(
                    &crate::query::Query::Text(crate::query::TextLeaf::all_fields("rebuildneedle")),
                    10
                )
                .unwrap()
                .items
                .len(),
            1
        );
        assert_eq!(rebuilt.search_generations().unwrap(), (1, 1));
        let count: i64 = rebuilt
            .writer()
            .conn()
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        drop(rebuilt);
        std::fs::write(dir.path().join("tantivy/meta.json"), b"corrupt").unwrap();
        let mut recovered = IndexActor::new(&db, 1).unwrap();
        assert_eq!(
            recovered
                .evaluate_saved_search(
                    &crate::query::Query::Text(crate::query::TextLeaf::all_fields("rebuildneedle")),
                    10
                )
                .unwrap()
                .items
                .len(),
            1
        );
        assert_eq!(recovered.search_generations().unwrap(), (1, 1));
        let count: i64 = recovered
            .writer()
            .conn()
            .query_row("SELECT COUNT(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn cjk_bigram_and_diacritic_folding_find_notes() {
        let (_dir, mut index) = searchable_index();
        for (path, body) in [
            ("ja.md", "東京の地図 𠀀文字"),
            ("zh.md", "北京数据中心"),
            ("ko.md", "서울시 한국어"),
            ("accent.md", "un café noir"),
        ] {
            index
                .apply_saved_note_retried(path.as_bytes(), path, body.as_bytes(), 1, 1)
                .unwrap();
        }
        for query in ["東京", "京", "数据", "서울", "𠀀", "cafe"] {
            assert!(
                !index
                    .evaluate_saved_search(
                        &crate::query::Query::Text(crate::query::TextLeaf::all_fields(query)),
                        10
                    )
                    .unwrap()
                    .items
                    .is_empty(),
                "query {query:?}"
            );
        }
    }

    #[test]
    fn attachment_ingest_single_path_resolves_non_ascii_embed_targets() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_saved_note_retried(
                b"notes/source.md",
                "notes/source.md",
                b"---\nid: stable-1\n---\n![[diagram.png]] ![[\xe5\x9b\xbe\xe8\xa1\xa8.png]] [[id:stable-1]]",
                1,
                1,
            )
            .unwrap();
        index
            .apply_saved_attachment(
                b"images/diagram.png",
                "images/diagram.png",
                Some(b"png-bytes"),
                9,
                1,
                1,
                true,
            )
            .unwrap();
        index
            .apply_saved_attachment(
                "图像/图表.png".as_bytes(),
                "图像/图表.png",
                Some(b"png-bytes"),
                9,
                1,
                1,
                true,
            )
            .unwrap();
        let connection = Connection::open(&index.read_pool.db_path).unwrap();
        let id: Option<String> = connection
            .query_row(
                "SELECT stable_id FROM notes WHERE path='notes/source.md'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(id.as_deref(), Some("stable-1"));
        let resolved: i64 = connection.query_row("SELECT count(*) FROM links WHERE res_state='resolved' AND ((dst_att IS NOT NULL) OR (dst_note IS NOT NULL))", [], |row| row.get(0)).unwrap();
        assert_eq!(resolved, 3);
        let hash: Option<Vec<u8>> = connection
            .query_row(
                "SELECT content_hash FROM attachments WHERE path='images/diagram.png'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(hash.unwrap().len(), 32);
        assert!(index.remove_attachment_path(b"images/diagram.png").unwrap());
        let unresolved: i64 = connection
            .query_row(
                "SELECT count(*) FROM links WHERE res_state='unresolved'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unresolved, 1);
    }

    #[test]
    fn resolver_persists_ambiguous_winner_and_handles_folded_ids_and_suffixes() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_saved_note_retried(
                b"z/copy.md",
                "z/copy.md",
                "---\nid: Été\n---\n# Copy".as_bytes(),
                1,
                1,
            )
            .unwrap();
        index
            .apply_saved_note_retried(
                b"a/copy.md",
                "a/copy.md",
                "---\nid: E\u{301}té\n---\n# Copy".as_bytes(),
                1,
                1,
            )
            .unwrap();
        index
            .apply_saved_note_retried(
                b"folder/source.md",
                "folder/source.md",
                "[[id:ÉTÉ]] [[folder/idea]] [[idea]]".as_bytes(),
                1,
                1,
            )
            .unwrap();
        index
            .apply_saved_note_retried(b"folder/idea.md", "folder/idea.md", b"# Local", 1, 1)
            .unwrap();
        index
            .apply_saved_note_retried(b"other/idea.md", "other/idea.md", b"# Other", 1, 1)
            .unwrap();

        let connection = index.writer().conn();
        let id_target: (String, Option<i64>, bool) = connection
            .query_row(
                "SELECT res_state,dst_note,(SELECT id_conflict FROM notes WHERE note_id=dst_note) FROM links WHERE dst_raw='id:ÉTÉ'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get::<_, Option<bool>>(2)?.unwrap_or(false))),
            )
            .unwrap();
        assert_eq!(id_target.0, "resolved");
        let winner_path: String = connection
            .query_row(
                "SELECT path FROM notes WHERE note_id=?1",
                [id_target.1.unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(winner_path, "a/copy.md");
        assert!(!id_target.2);
        let conflict_count: i64 = connection
            .query_row(
                "SELECT count(*) FROM notes WHERE id_conflict=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(conflict_count, 1);

        let suffix_target: String = connection
            .query_row(
                "SELECT n.path FROM links l JOIN notes n ON n.note_id=l.dst_note WHERE l.dst_raw='folder/idea'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(suffix_target, "folder/idea.md");
        let ambiguous: (String, Option<i64>) = connection
            .query_row(
                "SELECT res_state,dst_note FROM links WHERE dst_raw='idea'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(ambiguous.0, "ambiguous");
        let picked: String = connection
            .query_row(
                "SELECT path FROM notes WHERE note_id=?1",
                [ambiguous.1.unwrap()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(picked, "folder/idea.md");
    }

    #[test]
    fn oversized_attachment_is_indexed_without_a_content_hash() {
        let (_dir, mut index) = searchable_index();
        let size = crate::types::ATTACHMENT_HASH_MAX as u64 + 1;
        index
            .apply_saved_attachment(
                b"video/movie.mp4",
                "video/movie.mp4",
                None,
                size,
                1,
                1,
                true,
            )
            .unwrap();
        let hash: Option<Vec<u8>> = index
            .writer()
            .conn()
            .query_row("SELECT content_hash FROM attachments", [], |row| row.get(0))
            .unwrap();
        assert!(hash.is_none());
    }

    #[test]
    #[allow(clippy::items_after_statements)]
    fn incremental_index_matches_full_rebuild_after_random_mutations() {
        use proptest::prelude::*;
        proptest!(|(ops in prop::collection::vec((0u8..4, 0u8..6, 0u8..6), 1..40))| {
            let (_dir, mut incremental) = searchable_index();
            let mut files = std::collections::BTreeMap::<String, Vec<u8>>::new();
            for (action, source, target) in ops {
                let source_path = format!("n{source}.md");
                match action {
                    0 | 1 => {
                        let text = format!("# Note {source}\n\nvalue-{target} [[n{target}]]");
                        let bytes = text.into_bytes();
                        incremental.apply_saved_note_retried(source_path.as_bytes(), &source_path, &bytes, 1, 1).unwrap();
                        files.insert(source_path, bytes);
                    }
                    2 => {
                        if files.remove(&source_path).is_some() {
                            incremental.remove_note_path_retried(source_path.as_bytes()).unwrap();
                        }
                    }
                    _ => {
                        let renamed_path = format!("renamed{source}.md");
                        if let Some(bytes) = files.remove(&source_path) {
                            incremental.remove_note_path_retried(source_path.as_bytes()).unwrap();
                            incremental.apply_saved_note_retried(renamed_path.as_bytes(), &renamed_path, &bytes, 1, 1).unwrap();
                            files.insert(renamed_path, bytes);
                        }
                    }
                }
            }
            let (_rebuild_dir, mut rebuilt) = searchable_index();
            for (path, bytes) in &files {
                rebuilt.apply_saved_note_retried(path.as_bytes(), path, bytes, 1, 1).unwrap();
            }
            fn logical_dump(index: &IndexActor) -> Vec<(String, Option<Vec<u8>>, bool)> {
            let mut stmt = index.writer().conn().prepare("SELECT path,content_hash,parse_ok FROM notes ORDER BY path").unwrap();
                stmt.query_map([], |row| Ok((row.get::<_, String>(0)?,row.get::<_, Option<Vec<u8>>>(1)?,row.get::<_, bool>(2)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
            }
            prop_assert_eq!(logical_dump(&incremental), logical_dump(&rebuilt));
            let links = |index: &IndexActor| {
                let mut stmt = index.writer().conn().prepare("SELECT n.path,l.dst_raw,l.res_state,COALESCE(d.path,a.path,'') FROM links l JOIN notes n ON n.note_id=l.src LEFT JOIN notes d ON d.note_id=l.dst_note LEFT JOIN attachments a ON a.att_id=l.dst_att ORDER BY n.path,l.ord").unwrap();
                stmt.query_map([], |row| Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?,row.get::<_,String>(2)?,row.get::<_,String>(3)?))).unwrap().collect::<Result<Vec<_>,_>>().unwrap()
            };
            prop_assert_eq!(links(&incremental), links(&rebuilt));
            let tantivy_docs = |index: &mut IndexActor| {
                let mut docs = index
                    .evaluate_saved_search(&crate::query::Query::Text(crate::query::TextLeaf::all_fields("value")), 100)
                    .unwrap()
                    .items
                    .into_iter()
                    .map(|row| (row.path, row.title))
                    .collect::<Vec<_>>();
                docs.sort();
                docs
            };
            prop_assert_eq!(tantivy_docs(&mut incremental), tantivy_docs(&mut rebuilt));
        });
    }

    #[test]
    fn hybrid_search_uses_filter_and_intersection_paths() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_saved_note_retried(
                b"a.md",
                "a.md",
                b"---\ntags: [project]\n---\ntextneedle",
                1,
                1,
            )
            .unwrap();
        index
            .apply_saved_note_retried(
                b"b.md",
                "b.md",
                b"---\ntags: [other]\n---\ntextneedle",
                1,
                1,
            )
            .unwrap();
        let query = crate::query::Query::And(vec![
            crate::query::Query::Tag {
                name: "project".into(),
                hierarchical: false,
            },
            crate::query::Query::Text(crate::query::TextLeaf::all_fields("textneedle")),
        ]);
        // The complete evaluator intersects the text leaf with the
        // structural branch through bitsets — there is no giant-filter
        // fallback or top-hit approximation to exercise anymore (§6.3).
        let filtered = index.evaluate_saved_search(&query, 10).unwrap();
        assert_eq!(filtered.items.len(), 1);
        assert_eq!(filtered.items[0].path, "a.md");
        assert_eq!(
            filtered.items[0].score,
            Some(1.0).map(|_| filtered.items[0].score.unwrap_or(0.0))
        );
    }

    #[test]
    fn structural_endpoint_rejects_text_predicates_explicitly() {
        let (_dir, index) = searchable_index();
        let query = crate::query::Query::Text(crate::query::TextLeaf::all_fields("anything"));
        let error = index.structural_page(&query, 10).unwrap_err();
        assert!(
            error.contains("text predicates are not supported"),
            "unexpected error: {error}"
        );
        let structural = crate::query::Query::Unresolved;
        assert!(index.structural_page(&structural, 10).is_ok());
    }
}
