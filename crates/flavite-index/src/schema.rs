//! SQLite schema for the Flavite index (§4.3, D12).
//!
//! This module contains the complete schema from the specification, including
//! all tables, indexes, and constraints. The schema is versioned and migrations
//! are handled for in-place upgrades.

use rusqlite::{Connection, Result, params};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Current schema major version.
/// This is used in the cache path: `<os-cache>/flavite/<vault-uuid>/v<schema-major>/`
/// When this changes, old caches are not mutated and can coexist.
pub const SCHEMA_MAJOR: u32 = 1;

/// Current schema minor version for in-place migrations within the same major.
pub const SCHEMA_MINOR: u32 = 0;

/// Parser version - used to trigger background reindex when parser changes.
pub const PARSER_VERSION: u32 = 1;

/// Current epoch starts at 1 and increments when SQLite is recreated.
pub const INITIAL_EPOCH: u64 = 1;

/// The schema version stored in the meta table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaVersion {
    pub major: u32,
    pub minor: u32,
}

impl SchemaVersion {
    #[must_use]
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

/// Meta keys stored in the `meta` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetaKey {
    /// Schema version (major.minor as "major.minor")
    SchemaVersion,
    /// Parser version
    ParserVersion,
    /// Vault UUID (string)
    VaultUuid,
    /// Current epoch
    Epoch,
    /// Current generation within epoch
    Generation,
    /// Log floor - the oldest generation retained in the changes table
    LogFloor,
}

impl MetaKey {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::SchemaVersion => "schema_version",
            Self::ParserVersion => "parser_version",
            Self::VaultUuid => "vault_uuid",
            Self::Epoch => "epoch",
            Self::Generation => "generation",
            Self::LogFloor => "log_floor",
        }
    }
}

/// Attachment kinds as stored in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(u8)]
pub enum AttachmentKind {
    Image,
    Pdf,
    Audio,
    Video,
    Archive,
    Other,
}

impl AttachmentKind {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Pdf => "pdf",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Archive => "archive",
            Self::Other => "other",
        }
    }
}

impl std::str::FromStr for AttachmentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "image" => Ok(Self::Image),
            "pdf" => Ok(Self::Pdf),
            "audio" => Ok(Self::Audio),
            "video" => Ok(Self::Video),
            "archive" => Ok(Self::Archive),
            "other" => Ok(Self::Other),
            _ => Err(format!("unknown attachment kind: {s}")),
        }
    }
}

/// Resolution states for links.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResState {
    Resolved,
    Unresolved,
    Ambiguous,
}

impl ResState {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Unresolved => "unresolved",
            Self::Ambiguous => "ambiguous",
        }
    }
}

/// Value types for properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ValueType {
    Text,
    Num,
    Bool,
    Date,
    Datetime,
    Null,
}

impl ValueType {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Num => "num",
            Self::Bool => "bool",
            Self::Date => "date",
            Self::Datetime => "datetime",
            Self::Null => "null",
        }
    }
}

/// Create the complete schema in a new SQLite database.
///
/// This initializes:
/// - PRAGMA `journal_mode=WAL`
/// - PRAGMA `foreign_keys=ON`
/// - All tables from §4.3
/// - All indexes from §4.3
/// - Initial meta values
///
/// # Errors
///
/// Returns a SQLite error if a PRAGMA cannot be applied, the transaction
/// cannot start, or any DDL statement fails (for example, when the target
/// file is not a valid SQLite database).
#[allow(
    clippy::too_many_lines,
    reason = "one transaction's worth of §4.3 DDL; splitting it would only relocate SQL strings"
)]
pub fn create_schema(conn: &mut Connection) -> Result<()> {
    // PRAGMAs - must be set before creating tables and before starting transaction
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    let tx = conn.transaction()?;

    // Create tables in the order they're defined in §4.3

    // meta table
    tx.execute_batch(
        "CREATE TABLE meta(
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    )?;

    // notes table
    tx.execute_batch(
        "CREATE TABLE notes(
            note_id      INTEGER PRIMARY KEY,
            path         TEXT NOT NULL,
            path_native  BLOB NOT NULL UNIQUE,
            path_key     TEXT NOT NULL,
            stem_key     TEXT NOT NULL,
            title        TEXT NOT NULL,
            stable_id    TEXT,
            id_conflict  INTEGER NOT NULL DEFAULT 0,
            content_hash BLOB NOT NULL,
            size         INTEGER NOT NULL,
            mtime_ns     INTEGER NOT NULL,
            scan_ns      INTEGER NOT NULL,
            frontmatter  TEXT,
            preview      TEXT,
            parse_ok     INTEGER NOT NULL DEFAULT 1,
            generation   INTEGER NOT NULL
        )",
    )?;

    // notes indexes
    tx.execute_batch(
        "CREATE INDEX notes_stem ON notes(stem_key);
         CREATE INDEX notes_path_key ON notes(path_key);
         CREATE INDEX notes_stable_id ON notes(stable_id)",
    )?;

    // attachments table
    tx.execute_batch(
        "CREATE TABLE attachments(
            att_id       INTEGER PRIMARY KEY,
            path         TEXT NOT NULL,
            path_native  BLOB NOT NULL UNIQUE,
            path_key     TEXT NOT NULL,
            stem_key     TEXT NOT NULL,
            kind         TEXT NOT NULL CHECK(kind IN ('image','pdf','audio','video','archive','other')),
            mime         TEXT,
            size         INTEGER NOT NULL,
            content_hash BLOB,
            width        INTEGER,
            height       INTEGER,
            mtime_ns     INTEGER NOT NULL,
            scan_ns      INTEGER NOT NULL,
            hydrated     INTEGER NOT NULL DEFAULT 1,
            generation   INTEGER NOT NULL
        )")?;

    // attachments indexes
    tx.execute_batch(
        "CREATE INDEX attachments_stem ON attachments(stem_key);
         CREATE INDEX attachments_path_key ON attachments(path_key)",
    )?;

    // aliases table
    tx.execute_batch(
        "CREATE TABLE aliases(
            note_id INTEGER REFERENCES notes ON DELETE CASCADE,
            alias TEXT,
            alias_key TEXT,
            PRIMARY KEY(note_id, alias_key)
        )",
    )?;
    tx.execute_batch("CREATE INDEX aliases_key ON aliases(alias_key)")?;

    // links table
    tx.execute_batch(
        "CREATE TABLE links(
            src        INTEGER NOT NULL REFERENCES notes ON DELETE CASCADE,
            ord        INTEGER NOT NULL,
            kind       TEXT NOT NULL CHECK(kind IN ('link','embed')),
            dst_raw    TEXT NOT NULL,
            target_key TEXT NOT NULL,
            anchor     TEXT,
            alias      TEXT,
            by_id      INTEGER NOT NULL DEFAULT 0,
            span_start INTEGER NOT NULL,
            span_end   INTEGER NOT NULL,
            dst_note   INTEGER REFERENCES notes ON DELETE SET NULL,
            dst_att    INTEGER REFERENCES attachments ON DELETE SET NULL,
            res_state  TEXT NOT NULL CHECK(res_state IN ('resolved','unresolved','ambiguous')),
            PRIMARY KEY(src, ord)
        )",
    )?;

    // links indexes
    tx.execute_batch(
        "CREATE INDEX links_dst ON links(dst_note);
         CREATE INDEX links_dstatt ON links(dst_att);
         CREATE INDEX links_unresolved ON links(target_key) WHERE res_state <> 'resolved';
         CREATE INDEX links_target ON links(target_key)",
    )?;

    // tags table
    tx.execute_batch(
        "CREATE TABLE tags(
            note_id INTEGER REFERENCES notes ON DELETE CASCADE,
            tag TEXT NOT NULL,
            source TEXT CHECK(source IN ('inline','frontmatter')),
            PRIMARY KEY(note_id, tag, source)
        )",
    )?;
    tx.execute_batch("CREATE INDEX tags_tag ON tags(tag)")?;

    // properties table
    tx.execute_batch(
        "CREATE TABLE properties(
            note_id INTEGER REFERENCES notes ON DELETE CASCADE,
            key TEXT NOT NULL,
            ord INTEGER NOT NULL DEFAULT 0,
            vtype TEXT NOT NULL CHECK(vtype IN ('text','num','bool','date','datetime','null')),
            v_text TEXT,
            v_num REAL,
            PRIMARY KEY(note_id, key, ord)
        )",
    )?;
    tx.execute_batch(
        "CREATE INDEX props_text ON properties(key, v_text);
         CREATE INDEX props_num ON properties(key, v_num)",
    )?;

    // ext_facts table
    tx.execute_batch(
        "CREATE TABLE ext_facts(
            note_id INTEGER REFERENCES notes ON DELETE CASCADE,
            ns TEXT NOT NULL,
            key TEXT NOT NULL,
            value_json TEXT NOT NULL,
            PRIMARY KEY(note_id, ns, key)
        )",
    )?;

    // search_docs table - exact indexed source, rebuildable cache
    tx.execute_batch(
        "CREATE TABLE search_docs(
            note_id INTEGER PRIMARY KEY REFERENCES notes ON DELETE CASCADE,
            content_hash BLOB NOT NULL,
            body_text TEXT NOT NULL
        )",
    )?;

    // changes table - no cascading FK; deletion records survive
    tx.execute_batch(
        "CREATE TABLE changes(
            generation INTEGER NOT NULL,
            kind TEXT NOT NULL CHECK(kind IN ('note','attachment','outgoing')),
            entity_id INTEGER NOT NULL,
            PRIMARY KEY(generation, kind, entity_id)
        )",
    )?;

    // Insert initial meta values
    let current_epoch: u64 = INITIAL_EPOCH;
    let current_generation: u64 = 0;

    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)",
        params![
            MetaKey::SchemaVersion.as_str(),
            format!("{}.{}", SCHEMA_MAJOR, SCHEMA_MINOR)
        ],
    )?;

    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)",
        params![MetaKey::ParserVersion.as_str(), PARSER_VERSION.to_string()],
    )?;

    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)",
        params![MetaKey::Epoch.as_str(), current_epoch.to_string()],
    )?;

    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)",
        params![MetaKey::Generation.as_str(), current_generation.to_string()],
    )?;

    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)",
        params![MetaKey::LogFloor.as_str(), "0"],
    )?;

    tx.commit()?;

    Ok(())
}

/// Open or initialize a database at the given path.
///
/// # Errors
///
/// Returns a SQLite error when the database cannot be opened or created,
/// its PRAGMAs cannot be applied, or a fresh schema cannot be written.
pub fn open_or_init(path: &Path) -> Result<Connection> {
    let mut conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE
            | rusqlite::OpenFlags::SQLITE_OPEN_CREATE
            | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )?;

    // Enable WAL mode and foreign keys
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    // Check if the schema is initialized
    let schema_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='meta'",
            [],
            |row| {
                let count: i64 = row.get(0)?;
                Ok(count > 0)
            },
        )
        .unwrap_or(false);

    if !schema_exists {
        create_schema(&mut conn)?;
    }

    Ok(conn)
}

/// Get a meta value as a string.
///
/// # Errors
///
/// Returns a SQLite error when the query fails; a missing key is `Ok(None)`.
pub fn get_meta(conn: &Connection, key: MetaKey) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        [key.as_str()],
        |row| row.get::<_, String>(0),
    )
    .map(Some)
    .or_else(|e| {
        if let rusqlite::Error::QueryReturnedNoRows = e {
            Ok(None)
        } else {
            Err(e)
        }
    })
}

/// Get a meta value as a u64.
///
/// # Errors
///
/// Returns a SQLite error when the query fails. A missing key is `Ok(None)`;
/// an unparseable value reads as `Some(0)`.
pub fn get_meta_u64(conn: &Connection, key: MetaKey) -> Result<Option<u64>> {
    get_meta(conn, key).map(|v| v.map(|s| s.parse().unwrap_or(0)))
}

/// Get a meta value as a u32.
///
/// # Errors
///
/// Returns a SQLite error when the query fails. A missing key is `Ok(None)`;
/// an unparseable value reads as `Some(0)`.
pub fn get_meta_u32(conn: &Connection, key: MetaKey) -> Result<Option<u32>> {
    get_meta(conn, key).map(|v| v.map(|s| s.parse().unwrap_or(0)))
}

/// Set a meta value.
///
/// # Errors
///
/// Returns a SQLite error when the upsert cannot be executed.
pub fn set_meta(conn: &Connection, key: MetaKey, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
        params![key.as_str(), value],
    )?;
    Ok(())
}

/// Increment the generation and return the new value.
///
/// # Errors
///
/// Returns a SQLite error when the transaction cannot start or commit, the
/// query fails, or the stored generation is not a valid `u64`.
pub fn next_generation(conn: &mut Connection) -> Result<u64> {
    let tx = conn.transaction()?;
    let current: u64 = tx.query_row(
        "SELECT value FROM meta WHERE key = 'generation'",
        [],
        |row| {
            let s: String = row.get(0)?;
            s.parse::<u64>().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            })
        },
    )?;
    let new = current + 1;
    tx.execute(
        "UPDATE meta SET value = ?1 WHERE key = 'generation'",
        [new.to_string()],
    )?;
    tx.commit()?;
    Ok(new)
}

/// Get the current epoch.
///
/// # Errors
///
/// Returns a SQLite error when the query fails or the stored epoch is not
/// a valid `u64`.
pub fn get_epoch(conn: &Connection) -> Result<u64> {
    conn.query_row("SELECT value FROM meta WHERE key = 'epoch'", [], |row| {
        let s: String = row.get(0)?;
        s.parse::<u64>().map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(e),
            )
        })
    })
}

/// Get the current generation.
///
/// # Errors
///
/// Returns a SQLite error when the query fails or the stored generation is
/// not a valid `u64`.
pub fn get_generation(conn: &Connection) -> Result<u64> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'generation'",
        [],
        |row| {
            let s: String = row.get(0)?;
            s.parse::<u64>().map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            })
        },
    )
}

/// Version info stored in the database.
#[derive(Debug, Clone)]
pub struct DbVersion {
    pub epoch: u64,
    pub generation: u64,
    pub schema_major: u32,
    pub schema_minor: u32,
    pub parser_version: u32,
}

impl DbVersion {
    /// Read the version info from the database metadata.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when a metadata read fails or the stored
    /// epoch/generation are not valid `u64` values.
    pub fn get(conn: &Connection) -> Result<Self> {
        let schema_ver = get_meta(conn, MetaKey::SchemaVersion)?
            .unwrap_or_else(|| format!("{SCHEMA_MAJOR}.{SCHEMA_MINOR}"));
        let (major, minor) = schema_ver.split_once('.').map_or((0, 0), |(m, n)| {
            (m.parse().unwrap_or(0), n.parse().unwrap_or(0))
        });

        Ok(Self {
            epoch: get_epoch(conn)?,
            generation: get_generation(conn)?,
            schema_major: major,
            schema_minor: minor,
            parser_version: get_meta_u32(conn, MetaKey::ParserVersion)?.unwrap_or(0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_create_schema() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        // Verify meta table has our values
        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, format!("{SCHEMA_MAJOR}.{SCHEMA_MINOR}"));

        // Verify tables exist
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        // We have: meta, notes, attachments, aliases, links, tags, properties, ext_facts, search_docs, changes = 10 tables
        assert_eq!(table_count, 10);
    }

    #[test]
    fn test_db_version() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        let version = DbVersion::get(&conn).unwrap();
        assert_eq!(version.schema_major, SCHEMA_MAJOR);
        assert_eq!(version.schema_minor, SCHEMA_MINOR);
        assert_eq!(version.epoch, INITIAL_EPOCH);
        assert_eq!(version.generation, 0);
    }

    #[test]
    fn attachment_kinds_and_meta_helpers_round_trip() {
        for (name, kind) in [
            ("image", AttachmentKind::Image),
            ("pdf", AttachmentKind::Pdf),
            ("audio", AttachmentKind::Audio),
            ("video", AttachmentKind::Video),
            ("archive", AttachmentKind::Archive),
            ("other", AttachmentKind::Other),
        ] {
            assert_eq!(name.parse::<AttachmentKind>().unwrap(), kind);
            assert_eq!(kind.as_str(), name);
        }
        assert!("unknown".parse::<AttachmentKind>().is_err());

        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        set_meta(&conn, MetaKey::VaultUuid, "vault").unwrap();
        assert_eq!(
            get_meta(&conn, MetaKey::VaultUuid).unwrap().as_deref(),
            Some("vault")
        );
        assert_eq!(
            get_meta(&conn, MetaKey::ParserVersion).unwrap(),
            Some(PARSER_VERSION.to_string())
        );
        assert_eq!(
            get_meta(&conn, MetaKey::LogFloor).unwrap(),
            Some("0".into())
        );
        assert_eq!(
            get_meta_u32(&conn, MetaKey::ParserVersion).unwrap(),
            Some(PARSER_VERSION)
        );
        assert_eq!(get_meta_u32(&conn, MetaKey::VaultUuid).unwrap(), Some(0));
        assert_eq!(get_epoch(&conn).unwrap(), INITIAL_EPOCH);
        assert_eq!(get_generation(&conn).unwrap(), 0);
        assert_eq!(next_generation(&mut conn).unwrap(), 1);
        assert_eq!(get_generation(&conn).unwrap(), 1);
        assert_eq!(DbVersion::get(&conn).unwrap().generation, 1);

        set_meta(&conn, MetaKey::Epoch, "invalid").unwrap();
        assert!(get_epoch(&conn).is_err());
        set_meta(&conn, MetaKey::Generation, "invalid").unwrap();
        assert!(get_generation(&conn).is_err());
    }
}
