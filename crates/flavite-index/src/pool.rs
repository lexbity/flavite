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

        Ok(Self { writer, read_pool })
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
            tx.rollback().map_err(|error| error.to_string())?;
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
        Ok((epoch, generation))
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
}
