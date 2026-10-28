#![forbid(unsafe_code)]

//! Tier 1 — durability-critical. SQLite (WAL) structural store, persisted
//! searchable document cache, link resolver, typed query
//! execution (§4.1, D12, D13, §4.3, §4.14).
//!
//! # Architecture
//!
//! The index consists of:
//! - **SQLite (WAL)**: The authoritative structural record (notes, attachments,
//!   links, tags, properties, etc.)
//! - **Search document cache**: Source text cached for a future search backend
//! - **Resolver**: Implements the D8 5-step link resolution algorithm
//! - **Query AST**: Typed query language (not SQL strings) for safe querying
//!
//! # Key Design Decisions
//!
//! - **Two generations**: SQLite's `index.generation` bumps inline on every
//!   applied change and is authoritative. A second counter,
//!   `tantivy.committed_generation`, records what tantivy has actually committed.
//! - **Deferred commits**: Document adds/deletes are applied to the tantivy
//!   `IndexWriter` immediately (in-memory, cheap), but `commit()` is deferred
//!   (commit-before-search, or at most every 5 s while dirty, or at end of
//!   bulk reconcile, or on clean shutdown).
//! - **Atomic checkpoint publication**: The search checkpoint is published
//!   atomically with its own durable commit. Replay uses this checkpoint as the
//!   authority, not the SQLite mirror.

pub mod change;
pub mod pool;
pub mod query;
pub mod resolver;
pub mod schema;
pub mod search_docs;
pub mod types;

// Re-export key types for convenience from types module
pub use types::*;

// Re-export schema items
pub use schema::*;

// Re-export query items
pub use query::*;

// Re-export resolver items
pub use resolver::*;

// Re-export change items (but not DEFAULT_LOG_BUDGET which conflicts with types)
pub use change::{ChangeBatch, InvalidationFeed};

// Re-export pool items
pub use pool::*;

// Re-export persisted search document cache operations.
pub use search_docs::{SearchDocBatch, SearchDocCache, SearchDocUpdate};

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn test_crate_compiles() {
        // This is just a sanity check that all modules compile
        assert_eq!(SCHEMA_MAJOR, 1);
        assert_eq!(SCHEMA_MINOR, 0);
        assert_eq!(PARSER_VERSION, 1);
        assert_eq!(INITIAL_EPOCH, 1);
        assert_eq!(MAX_NOTE_BYTES, 32 * 1024 * 1024);
        assert_eq!(ATTACHMENT_HASH_MAX, 64 * 1024 * 1024);
    }

    #[test]
    fn test_db_creation() {
        let temp = NamedTempFile::new().unwrap();
        let conn = open_or_init(temp.path()).unwrap();

        let version = DbVersion::get(&conn).unwrap();
        assert_eq!(version.schema_major, SCHEMA_MAJOR);
        assert_eq!(version.schema_minor, SCHEMA_MINOR);
        assert_eq!(version.epoch, INITIAL_EPOCH);
        assert_eq!(version.generation, 0);
    }

    #[test]
    fn test_generation_bump() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = open_or_init(temp.path()).unwrap();

        let gen1 = next_generation(&mut conn).unwrap();
        assert_eq!(gen1, 1);

        let gen2 = next_generation(&mut conn).unwrap();
        assert_eq!(gen2, 2);

        let current = get_generation(&conn).unwrap();
        assert_eq!(current, 2);
    }
}
