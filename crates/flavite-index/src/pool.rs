//! Connection pool for SQLite (D3).
//!
//! This module provides:
//! - A single writer connection (owns the SQLite writer connection and
//!   tantivy `IndexWriter`)
//! - A pool of read-only WAL connections for concurrent reads
//!
//! The pool size is based on S4-2's recommendations: default 4 connections,
//! hard cap 8.

use crate::schema::{MetaKey, get_meta_u64};
use crate::search::SearchBackend;
use blake3::Hash;
use flavite_syntax::{pulldown_glue::parse_note, text::TextSnapshot};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Result};
use std::collections::VecDeque;
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
    search: crate::search::TantivyBackend,
    last_search_commit: Instant,
}

/// Text-search results paired with the structural index version used to fetch them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPage {
    /// Structural epoch.
    pub epoch: u64,
    /// Structural generation.
    pub generation: u64,
    /// Matching note ID, vault-relative path, and title rows.
    pub items: Vec<SearchNote>,
}

/// One matching note from a text or hybrid query.
pub type SearchNote = (i64, String, String);

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

impl IndexActor {
    /// Create a new index actor.
    ///
    /// # Errors
    /// Returns a SQLite error if the writer or read pool cannot be initialized.
    pub fn new(db_path: &Path, pool_size: usize) -> Result<Self> {
        let writer = Writer::new(db_path)?;
        let read_pool = ReadPool::new(db_path, pool_size)?;
        let cache_dir = db_path.parent().unwrap_or_else(|| Path::new("."));
        let (search, search_recreated) =
            if let Ok(search) = crate::search::TantivyBackend::open(db_path, cache_dir) {
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
                    crate::search::TantivyBackend::open(db_path, cache_dir)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
                    true,
                )
            };
        let sqlite_generation =
            crate::schema::get_meta_u64(writer.conn(), MetaKey::Generation)?.unwrap_or(0);
        let committed = search.committed_generation();
        if search_recreated || committed > sqlite_generation {
            search
                .rebuild_from_sqlite()
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        } else if committed < sqlite_generation {
            let floor = crate::schema::get_meta_u64(writer.conn(), MetaKey::LogFloor)?.unwrap_or(0);
            if committed < floor {
                search
                    .rebuild_from_sqlite()
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            } else {
                search
                    .replay_since(committed)
                    .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            }
        }

        Ok(Self {
            writer,
            read_pool,
            search,
            last_search_commit: Instant::now(),
        })
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

    /// Read the indexed note title/alias snapshot for editor autocomplete.
    ///
    /// # Errors
    /// Returns a pool or SQLite error when the read connection is unavailable.
    pub fn completion_candidates(&self) -> Result<Vec<crate::types::CompletionCandidate>, String> {
        let connection = self
            .read_pool
            .get(Some(std::time::Duration::from_secs(2)))
            .map_err(|error| error.to_string())?;
        let mut aliases_by_note = std::collections::HashMap::<i64, Vec<String>>::new();
        {
            let mut statement = connection
                .prepare("SELECT note_id, alias FROM aliases ORDER BY note_id, alias_key")
                .map_err(|error| error.to_string())?;
            let aliases = statement
                .query_map([], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| error.to_string())?;
            for alias in aliases {
                let (note_id, value) = alias.map_err(|error| error.to_string())?;
                aliases_by_note.entry(note_id).or_default().push(value);
            }
        }
        let mut statement = connection
            .prepare(
                "SELECT note_id, path, title, stable_id, id_conflict FROM notes ORDER BY path_key, path",
            )
            .map_err(|error| error.to_string())?;
        let note_rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, bool>(4)?,
                ))
            })
            .map_err(|error| error.to_string())?;
        let mut candidates = Vec::new();
        for row in note_rows {
            let (note_id, path, title, stable_id, id_conflict) =
                row.map_err(|error| error.to_string())?;
            let target = path.strip_suffix(".md").unwrap_or(&path).to_owned();
            candidates.push(crate::types::CompletionCandidate {
                target,
                title,
                aliases: aliases_by_note.remove(&note_id).unwrap_or_default(),
                stable_id: (!id_conflict).then_some(stable_id).flatten(),
            });
        }
        Ok(candidates)
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

    /// Return indexed links targeting a note stem, including path-suffix and
    /// explicit `.md` forms. The caller MUST verify each source hash again.
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

    /// List native path keys for indexed attachments.
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

    /// Insert or refresh one attachment row and advance structural generation.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_attachment(
        &mut self,
        path_native: &[u8],
        path: &str,
        path_key: &str,
        stem_key: &str,
        kind: crate::schema::AttachmentKind,
        size: u64,
        content_hash: Option<[u8; 32]>,
        mtime_ns: i64,
        scan_ns: i64,
        hydrated: bool,
    ) -> Result<u64, String> {
        let conn = &mut self.writer.conn;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let generation = get_meta_u64(&tx, MetaKey::Generation)
            .map_err(|e| e.to_string())?
            .unwrap_or(0)
            .saturating_add(1);
        tx.execute("INSERT INTO attachments(path,path_native,path_key,stem_key,kind,mime,size,content_hash,width,height,mtime_ns,scan_ns,hydrated,generation) VALUES(?1,?2,?3,?4,?5,NULL,?6,?7,NULL,NULL,?8,?9,?10,?11) ON CONFLICT(path_native) DO UPDATE SET path=excluded.path,path_key=excluded.path_key,stem_key=excluded.stem_key,kind=excluded.kind,size=excluded.size,content_hash=excluded.content_hash,mtime_ns=excluded.mtime_ns,scan_ns=excluded.scan_ns,hydrated=excluded.hydrated,generation=excluded.generation",
            rusqlite::params![path, path_native, path_key, stem_key, kind.as_str(), i64::try_from(size).map_err(|e| e.to_string())?, content_hash.map(|hash| hash.to_vec()), mtime_ns, scan_ns, hydrated, i64::try_from(generation).map_err(|e| e.to_string())?]).map_err(|e| e.to_string())?;
        let filename = Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path)
            .to_lowercase();
        tx.execute("UPDATE links SET dst_note=NULL,dst_att=(SELECT MIN(att_id) FROM attachments WHERE stem_key=links.target_key OR lower(path)=links.target_key OR lower(path) LIKE '%/' || links.target_key),res_state=CASE WHEN (SELECT COUNT(*) FROM attachments WHERE stem_key=links.target_key OR lower(path)=links.target_key OR lower(path) LIKE '%/' || links.target_key)=1 THEN 'resolved' ELSE 'ambiguous' END WHERE kind='embed' AND (target_key=?1 OR target_key=?2 OR target_key=?3)", rusqlite::params![stem_key, filename, path_key]).map_err(|e| e.to_string())?;
        crate::schema::set_meta(&tx, MetaKey::Generation, &generation.to_string())
            .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(generation)
    }

    /// Remove an attachment by native path key.
    pub fn remove_attachment_path(&mut self, path_native: &[u8]) -> Result<bool, String> {
        let conn = &mut self.writer.conn;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let changed = tx
            .execute(
                "DELETE FROM attachments WHERE path_native=?1",
                [path_native],
            )
            .map_err(|e| e.to_string())?
            != 0;
        if changed {
            let generation = get_meta_u64(&tx, MetaKey::Generation)
                .map_err(|e| e.to_string())?
                .unwrap_or(0)
                .saturating_add(1);
            crate::schema::set_meta(&tx, MetaKey::Generation, &generation.to_string())
                .map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(changed)
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

    /// Remove a vanished note and its dependent structural/search rows in one
    /// transaction. Returns whether an indexed note was removed.
    ///
    /// # Errors
    /// Returns the underlying error text when the transaction cannot be
    /// started, a query fails, or the commit fails.
    pub fn remove_note_path(&mut self, path_native: &[u8]) -> Result<bool, String> {
        let tx = self.writer.conn.transaction().map_err(|e| e.to_string())?;
        let note_id: Option<i64> = tx
            .query_row(
                "SELECT note_id FROM notes WHERE path_native=?1",
                [path_native],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some(note_id) = note_id else {
            tx.rollback().map_err(|e| e.to_string())?;
            return Ok(false);
        };
        let generation = get_meta_u64(&tx, MetaKey::Generation)
            .map_err(|error| error.to_string())?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or("index generation overflow")?;
        tx.execute("DELETE FROM notes WHERE note_id=?1", [note_id])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM search_docs WHERE note_id=?1", [note_id])
            .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE meta SET value=?1 WHERE key=?2",
            rusqlite::params![generation.to_string(), MetaKey::Generation.as_str()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO changes(generation,kind,entity_id) VALUES(?1,'note_deleted',?2)",
            rusqlite::params![
                i64::try_from(generation).map_err(|e| e.to_string())?,
                note_id
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(true)
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
        let indexed: Option<(i64, i64, i64, bool)> = self
            .writer
            .conn()
            .query_row(
                "SELECT size,mtime_ns,scan_ns,parse_ok FROM notes WHERE path_native=?1",
                [path_native],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((indexed_size, indexed_mtime, _indexed_scan, parse_ok)) = indexed else {
            return Ok(false);
        };
        let size_matches = u64::try_from(indexed_size).ok() == Some(size);
        let not_racy = mtime_ns < scan_ns.saturating_sub(2_000_000_000);
        Ok(parse_ok && size_matches && indexed_mtime == mtime_ns && not_racy)
    }

    /// Apply the exact bytes installed by a save to SQLite in one structural
    /// transaction. The caller owns this actor on the sole index-writer thread.
    ///
    /// # Errors
    /// Returns an error if parsing, coordinate conversion, or any SQLite step
    /// fails. A failed transaction leaves the prior structural state intact.
    #[allow(clippy::too_many_lines)] // One transaction keeps note/search/change state atomic.
    pub fn apply_saved_note(
        &mut self,
        path_native: &[u8],
        display_path: &str,
        bytes: &[u8],
        mtime_ns: i64,
        scan_ns: i64,
    ) -> Result<(u64, u64), String> {
        let parsed = parse_note(bytes).map_err(|error| format!("parse saved note: {error}"))?;
        let snapshot = TextSnapshot::new(bytes).map_err(|error| error.to_string())?;
        let content_hash = Hash::from_bytes(*blake3::hash(bytes).as_bytes());
        let conn = &mut self.writer.conn;
        let tx = conn.transaction().map_err(|e| e.to_string())?;
        let current_generation: u64 = tx
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [MetaKey::Generation.as_str()],
                |row| {
                    let value: String = row.get(0)?;
                    value.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        let existing_hash: Option<Vec<u8>> = tx
            .query_row(
                "SELECT content_hash FROM notes WHERE path_native = ?1",
                [path_native],
                |row| row.get(0),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        if existing_hash.as_deref() == Some(content_hash.as_bytes().as_slice()) {
            let epoch = tx
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    [MetaKey::Epoch.as_str()],
                    |row| {
                        let value: String = row.get(0)?;
                        value.parse().map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                0,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })
                    },
                )
                .map_err(|error| error.to_string())?;
            tx.execute(
                "UPDATE notes SET mtime_ns=?1,scan_ns=?2 WHERE path_native=?3",
                rusqlite::params![mtime_ns, scan_ns, path_native],
            )
            .map_err(|error| error.to_string())?;
            tx.commit().map_err(|error| error.to_string())?;
            return Ok((epoch, current_generation));
        }
        let generation = current_generation
            .checked_add(1)
            .ok_or("index generation overflow")?;
        let epoch = tx
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [MetaKey::Epoch.as_str()],
                |row| {
                    let value: String = row.get(0)?;
                    value.parse().map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })
                },
            )
            .map_err(|e| e.to_string())?;
        let stem = std::path::Path::new(display_path)
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or(display_path);
        let path_key = display_path.to_lowercase();
        let stem_key = stem.to_lowercase();
        let frontmatter = flavite_syntax::frontmatter::parse_frontmatter(&parsed.snapshot);
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
        let size = i64::try_from(bytes.len()).map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO notes(path,path_native,path_key,stem_key,title,stable_id,id_conflict,content_hash,size,mtime_ns,scan_ns,frontmatter,preview,parse_ok,generation)
             VALUES(?1,?2,?3,?4,?5,NULL,0,?6,?7,?8,?9,?10,?11,1,?12)
             ON CONFLICT(path_native) DO UPDATE SET path=excluded.path,path_key=excluded.path_key,stem_key=excluded.stem_key,title=excluded.title,content_hash=excluded.content_hash,size=excluded.size,mtime_ns=excluded.mtime_ns,scan_ns=excluded.scan_ns,frontmatter=excluded.frontmatter,preview=excluded.preview,parse_ok=1,generation=excluded.generation",
            rusqlite::params![display_path, path_native, path_key, stem_key, title, content_hash.as_bytes(), size, mtime_ns, scan_ns, frontmatter_json, &parsed.snapshot.text()[..parsed.snapshot.text().len().min(1024)], i64::try_from(generation).map_err(|e| e.to_string())?],
        ).map_err(|e| e.to_string())?;
        let note_id: i64 = tx
            .query_row(
                "SELECT note_id FROM notes WHERE path_native = ?1",
                [path_native],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM links WHERE src = ?1", [note_id])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM tags WHERE note_id = ?1", [note_id])
            .map_err(|e| e.to_string())?;
        tx.execute("DELETE FROM aliases WHERE note_id = ?1", [note_id])
            .map_err(|e| e.to_string())?;
        for (ord, link) in parsed.links.iter().enumerate() {
            let source_span = snapshot
                .span_to_source(link.span)
                .ok_or("link span cannot be mapped to source bytes")?;
            let target_key = link.target_raw.to_lowercase();
            tx.execute(
                "INSERT INTO links(src,ord,kind,dst_raw,target_key,anchor,alias,by_id,span_start,span_end,dst_note,dst_att,res_state) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,NULL,NULL,'unresolved')",
                rusqlite::params![note_id, i64::try_from(ord).map_err(|e| e.to_string())?, if link.is_embed { "embed" } else { "link" }, link.target_raw, target_key, link.anchor, link.alias, i64::from(link.by_id), i64::try_from(source_span.start.0).map_err(|e| e.to_string())?, i64::try_from(source_span.end.0).map_err(|e| e.to_string())?],
            ).map_err(|e| e.to_string())?;
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
            .map_err(|e| e.to_string())?;
        }
        for tag in frontmatter_tags {
            tx.execute(
                "INSERT OR IGNORE INTO tags(note_id,tag,source) VALUES(?1,?2,'frontmatter')",
                rusqlite::params![note_id, flavite_syntax::wellknown::normalize_tag(&tag)],
            )
            .map_err(|e| e.to_string())?;
        }
        for alias in aliases {
            tx.execute(
                "INSERT OR IGNORE INTO aliases(note_id,alias,alias_key) VALUES(?1,?2,?3)",
                rusqlite::params![
                    note_id,
                    alias,
                    flavite_syntax::wellknown::normalize_tag(&alias)
                ],
            )
            .map_err(|e| e.to_string())?;
        }
        tx.execute(
            "INSERT INTO search_docs(note_id,content_hash,body_text) VALUES(?1,?2,?3) ON CONFLICT(note_id) DO UPDATE SET content_hash=excluded.content_hash,body_text=excluded.body_text",
            rusqlite::params![note_id, content_hash.as_bytes(), parsed.snapshot.text()],
        ).map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE meta SET value=?1 WHERE key=?2",
            rusqlite::params![generation.to_string(), MetaKey::Generation.as_str()],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO changes(generation,kind,entity_id) VALUES(?1,'note',?2)",
            rusqlite::params![
                i64::try_from(generation).map_err(|e| e.to_string())?,
                note_id
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO changes(generation,kind,entity_id) VALUES(?1,'outgoing',?2)",
            rusqlite::params![
                i64::try_from(generation).map_err(|e| e.to_string())?,
                note_id
            ],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        let mut alias_stmt = conn
            .prepare("SELECT alias FROM aliases WHERE note_id=?1 ORDER BY alias_key")
            .map_err(|e| e.to_string())?;
        let aliases = alias_stmt
            .query_map([note_id], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let mut tag_stmt = conn
            .prepare("SELECT tag FROM tags WHERE note_id=?1 ORDER BY tag")
            .map_err(|e| e.to_string())?;
        let tags = tag_stmt
            .query_map([note_id], |row| row.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        self.search
            .apply_document_full(note_id, &title, &aliases, &tags, parsed.snapshot.text())
            .map_err(|e| e.to_string())?;
        Ok((epoch, generation))
    }

    /// Search the derived Tantivy view. Dirty documents are committed before
    /// the query so every acknowledged save is visible (D4/D12).
    ///
    /// # Errors
    /// Returns an error when a commit or Tantivy search fails.
    pub fn search_text(
        &mut self,
        text: &str,
        note_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>, String> {
        let hits = self
            .search
            .search_text(text, note_ids, limit)
            .map_err(|e| e.to_string())?;
        self.last_search_commit = Instant::now();
        Ok(hits)
    }

    /// Commit the search view after bulk reconcile or when its deferred timer expires.
    ///
    /// # Errors
    /// Returns an error when Tantivy or generation metadata cannot be committed.
    pub fn commit_search_if_due(&mut self, force: bool) -> Result<(), String> {
        if force
            || (self.search.is_dirty()
                && self.last_search_commit.elapsed()
                    >= std::time::Duration::from_secs(crate::search::DEFAULT_COMMIT_INTERVAL_SECS))
        {
            self.search.commit().map_err(|e| e.to_string())?;
            self.last_search_commit = Instant::now();
        }
        Ok(())
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
            self.search.committed_generation(),
        ))
    }

    /// Execute text search and the supported `AND` hybrid form against the
    /// structural read pool. Results are returned with stable note metadata.
    ///
    /// # Errors
    /// Returns an error when the query shape is unsupported or either index fails.
    #[allow(clippy::type_complexity)]
    pub fn search_notes(
        &mut self,
        query: &crate::query::Query,
        limit: usize,
    ) -> Result<SearchPage, String> {
        self.search_notes_with_filter_limit(query, limit, crate::search::MAX_TANTIVY_FILTER_SIZE)
    }

    #[allow(clippy::type_complexity)]
    fn search_notes_with_filter_limit(
        &mut self,
        query: &crate::query::Query,
        limit: usize,
        filter_limit: usize,
    ) -> Result<SearchPage, String> {
        use crate::query::Query;
        let (text, structural) = match query {
            Query::Text(text) => (text.clone(), None),
            Query::And(parts) => {
                let mut text = Vec::new();
                let mut filters = Vec::new();
                for part in parts {
                    if let Query::Text(value) = part {
                        text.push(value.as_str());
                    } else {
                        filters.push(part.clone());
                    }
                }
                (text.join(" "), Some(Query::And(filters)))
            }
            _ => return Err("query does not contain a text clause".into()),
        };
        if text.trim().is_empty() {
            return Err("text query must contain at least one searchable token".into());
        }
        let structural_ids = if let Some(structural) = structural {
            if matches!(&structural, Query::And(parts) if parts.is_empty()) {
                Vec::new()
            } else {
                let conn = self
                    .read_pool
                    .get(Some(Duration::from_secs(2)))
                    .map_err(|e| e.to_string())?;
                structural
                    .execute(&conn, 100_000, None)
                    .map_err(|e| e.to_string())?
                    .items
                    .into_iter()
                    .map(|v| v.note_id)
                    .collect::<Vec<_>>()
            }
        } else {
            Vec::new()
        };
        let has_structural = matches!(query, Query::And(_));
        if has_structural && structural_ids.is_empty() {
            // Even an empty hybrid result is still a search operation: preserve
            // D12's commit-before-search ordering for every admitted query.
            self.search_text(&text, &[], 1)?;
            let (epoch, generation) = self.version()?;
            return Ok(SearchPage {
                epoch,
                generation,
                items: Vec::new(),
            });
        }
        let filter = if has_structural && structural_ids.len() <= filter_limit {
            structural_ids.as_slice()
        } else {
            &[]
        };
        let mut hits = self.search_text(
            &text,
            filter,
            if has_structural && structural_ids.len() > filter_limit {
                100_000
            } else {
                limit.saturating_add(1)
            },
        )?;
        if has_structural && structural_ids.len() > filter_limit {
            let allowed = structural_ids
                .into_iter()
                .collect::<std::collections::HashSet<_>>();
            hits.retain(|id| allowed.contains(id));
        }
        hits.truncate(limit.saturating_add(1));
        hits.truncate(limit);
        let conn = self
            .read_pool
            .get(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        let mut rows = Vec::new();
        for id in hits {
            if let Some(row) = conn
                .query_row(
                    "SELECT note_id,path,title FROM notes WHERE note_id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?
            {
                rows.push(row);
            }
        }
        let (epoch, generation) = self.version()?;
        Ok(SearchPage {
            epoch,
            generation,
            items: rows,
        })
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
        let temp = NamedTempFile::new().unwrap();
        drop(crate::open_or_init(temp.path()).unwrap());
        let mut index = IndexActor::new(temp.path(), 1).unwrap();
        index
            .apply_saved_note(b"a.md", "a.md", b"body", 10_000_000_000, 10_000_000_000)
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
    fn saved_note_updates_structure_search_cache_and_generation_atomically() {
        let temp = NamedTempFile::new().unwrap();
        drop(crate::schema::open_or_init(temp.path()).unwrap());
        let mut index = IndexActor::new(temp.path(), 1).unwrap();
        let original = b"---\ntitle: Alpha\naliases: [A, B]\ntags: [frontmatter]\n---\n# Heading\n\n[[Beta|alias]] ![[image.png]] #project";
        let (epoch, generation) = index
            .apply_saved_note(b"notes/a.md", "notes/a.md", original, 98, 99)
            .unwrap();
        assert_eq!((epoch, generation), (1, 1));
        let candidates = index.completion_candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].target, "notes/a");
        assert_eq!(candidates[0].title, "Alpha");
        assert_eq!(candidates[0].aliases, ["A", "B"]);

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
            assert_eq!((link_count, tag_count, changes), (2, 2, 2));
            let alias_count: i64 = conn
                .query_row("SELECT COUNT(*) FROM aliases", [], |row| row.get(0))
                .unwrap();
            assert_eq!(alias_count, 2);
        }
        assert_eq!(index.version().unwrap(), (1, 1));

        let (_, next_generation) = index
            .apply_saved_note(b"notes/a.md", "notes/a.md", b"# Updated", 100, 101)
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
            .apply_saved_note(b"notes/a.md", "notes/a.md", b"# Updated", 102, 103)
            .unwrap();
        assert_eq!(unchanged_generation, 2);
        assert_eq!(index.version().unwrap(), (1, 2));
        assert!(
            index
                .apply_saved_note(b"notes/b.md", "notes/b.md", &[0xff], 104, 105)
                .is_err()
        );
        assert_eq!(index.version().unwrap(), (1, 2));
    }

    fn searchable_index() -> (tempfile::TempDir, IndexActor) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        drop(crate::schema::open_or_init(&db).unwrap());
        let index = IndexActor::new(&db, 1).unwrap();
        (dir, index)
    }

    #[test]
    fn acknowledged_save_is_visible_to_immediate_search_and_commits_once() {
        let (_dir, mut index) = searchable_index();
        for n in 0..25 {
            let path = format!("notes/{n}.md");
            let body = format!("# Note {n}\n\nunique_saved_content_{n}");
            index
                .apply_saved_note(path.as_bytes(), &path, body.as_bytes(), n, n)
                .unwrap();
        }
        let mid = index
            .search_notes(
                &crate::query::Query::Text("unique_saved_content_24".into()),
                10,
            )
            .unwrap();
        assert_eq!(mid.items.len(), 1);
        assert_eq!(
            index.search_generations().unwrap(),
            (25, 25),
            "the mid-stream query commits preceding saves"
        );
        for n in 25..50 {
            let path = format!("notes/{n}.md");
            let body = format!("# Note {n}\n\nunique_saved_content_{n}");
            index
                .apply_saved_note(path.as_bytes(), &path, body.as_bytes(), n, n)
                .unwrap();
        }
        let (generation, committed) = index.search_generations().unwrap();
        assert_eq!(generation, 50);
        assert_eq!(
            committed, 25,
            "writes must remain deferred after the mid-stream query"
        );
        let hits = index
            .search_notes(
                &crate::query::Query::Text("unique_saved_content_49".into()),
                10,
            )
            .unwrap();
        assert_eq!(hits.items.len(), 1);
        assert_eq!(hits.items[0].1, "notes/49.md");
        assert_eq!(index.search_generations().unwrap(), (50, 50));
    }

    #[test]
    fn deferred_commit_timer_commits_dirty_writer_after_five_seconds() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_saved_note(b"timer.md", "timer.md", b"timercommitword", 1, 1)
            .unwrap();
        index.commit_search_if_due(false).unwrap();
        assert_eq!(index.search_generations().unwrap(), (1, 0));
        index.last_search_commit = Instant::now().checked_sub(Duration::from_secs(6)).unwrap();
        index.commit_search_if_due(false).unwrap();
        assert_eq!(index.search_generations().unwrap(), (1, 1));
    }

    #[test]
    fn uncommitted_changes_replay_after_reopen_and_counter_converges() {
        let (dir, mut index) = searchable_index();
        let db = dir.path().join("index.sqlite3");
        for n in 0..4 {
            let path = format!("{n}.md");
            let body = format!("# Recovery\n\nreplayneedle{n}");
            index
                .apply_saved_note(path.as_bytes(), &path, body.as_bytes(), n, n)
                .unwrap();
        }
        assert_eq!(index.search_generations().unwrap(), (4, 0));
        drop(index);
        let mut reopened = IndexActor::new(&db, 1).unwrap();
        let result = reopened
            .search_notes(&crate::query::Query::Text("replayneedle3".into()), 10)
            .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(reopened.search_generations().unwrap(), (4, 4));
    }

    #[test]
    fn ahead_and_corrupt_search_cache_rebuild_without_touching_structure() {
        let (dir, mut index) = searchable_index();
        let db = dir.path().join("index.sqlite3");
        index
            .apply_saved_note(b"one.md", "one.md", b"# Title\n\nrebuildneedle", 1, 1)
            .unwrap();
        index.commit_search_if_due(true).unwrap();
        drop(index);
        let conn = Connection::open(&db).unwrap();
        crate::schema::set_meta(&conn, MetaKey::TantivyCommittedGeneration, "99").unwrap();
        drop(conn);
        let mut rebuilt = IndexActor::new(&db, 1).unwrap();
        assert_eq!(
            rebuilt
                .search_notes(&crate::query::Query::Text("rebuildneedle".into()), 10)
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
                .search_notes(&crate::query::Query::Text("rebuildneedle".into()), 10)
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
            ("ja.md", "東京の地図"),
            ("zh.md", "北京数据中心"),
            ("ko.md", "서울시 한국어"),
            ("accent.md", "un café noir"),
        ] {
            index
                .apply_saved_note(path.as_bytes(), path, body.as_bytes(), 1, 1)
                .unwrap();
        }
        for query in ["東京", "数据", "서울", "cafe"] {
            assert!(
                !index
                    .search_notes(&crate::query::Query::Text(query.into()), 10)
                    .unwrap()
                    .items
                    .is_empty(),
                "query {query:?}"
            );
        }
    }

    #[test]
    fn hybrid_search_uses_filter_and_intersection_paths() {
        let (_dir, mut index) = searchable_index();
        index
            .apply_saved_note(
                b"a.md",
                "a.md",
                b"---\ntags: [project]\n---\ntextneedle",
                1,
                1,
            )
            .unwrap();
        index
            .apply_saved_note(
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
            crate::query::Query::Text("textneedle".into()),
        ]);
        let filtered = index
            .search_notes_with_filter_limit(&query, 10, 10)
            .unwrap();
        assert_eq!(filtered.items.len(), 1);
        assert_eq!(filtered.items[0].1, "a.md");
        let intersected = index.search_notes_with_filter_limit(&query, 10, 0).unwrap();
        assert_eq!(intersected.items.len(), 1);
        assert_eq!(intersected.items[0].1, "a.md");
    }
}
