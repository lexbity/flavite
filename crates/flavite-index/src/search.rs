//! Search backend abstraction (§4.3, D12, §4.14).
//!
//! This module provides:
//! - `SearchBackend` trait: the abstraction point for search implementations
//! - Simplified stub implementation (tantivy integration will be added later)
//!
//! The full tantivy implementation with CJK bigram tokenizer, deferred commits,
//! and atomic checkpoint publication will be implemented in a follow-up.

use crate::query::Query as QueryAst;
use crate::schema::{MetaKey, get_meta_u64, set_meta};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;

/// Maximum number of note IDs to pass to search as a filter (D12).
pub const MAX_TANTIVY_FILTER_SIZE: usize = 10_000;

/// Default deferred commit interval in seconds (D12, R4).
pub const DEFAULT_COMMIT_INTERVAL_SECS: u64 = 5;

/// Search backend trait.
///
/// This trait allows swapping the search implementation (e.g., to SQLite FTS5)
/// without changing the rest of the index.
pub trait SearchBackend: Send + Sync {
    /// Apply a document to the index (add or update).
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the backend cannot record the change
    /// (a failed write or an inaccessible index).
    fn apply_document(&self, note_id: i64, title: &str, body: &str) -> Result<(), SearchError>;

    /// Delete a document from the index.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the backend cannot record the deletion.
    fn delete_document(&self, note_id: i64) -> Result<(), SearchError>;

    /// Commit pending changes to the search index.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the commit or its checkpoint
    /// publication fails.
    fn commit(&self) -> Result<(), SearchError>;

    /// Get the current committed generation.
    fn committed_generation(&self) -> u64;

    /// Set the committed generation.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the generation cannot be persisted.
    fn set_committed_generation(&self, generation: u64) -> Result<(), SearchError>;

    /// Get the checkpoint metadata (epoch, generation, payload).
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the metadata or the checkpoint payload
    /// cannot be read.
    fn checkpoint(&self) -> Result<SearchCheckpoint, SearchError>;

    /// Load the checkpoint from durable storage.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the durable checkpoint cannot be read.
    fn load_checkpoint(&mut self) -> Result<SearchCheckpoint, SearchError>;

    /// Publish a new checkpoint atomically.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the checkpoint file cannot be written,
    /// synced, or renamed into place.
    fn publish_checkpoint(&self, epoch: u64, generation: u64) -> Result<(), SearchError>;

    /// Search using a query with optional note ID filter.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the backend cannot evaluate the query.
    fn search(
        &self,
        query: &QueryAst,
        note_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>, SearchError>;

    /// Get a searcher for reading the index.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when no searcher can be acquired (for
    /// example, [`SearchError::NotInitialized`]).
    fn searcher(&self) -> Result<Box<dyn SearcherTrait>, SearchError>;
}

/// Checkpoint for search durability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchCheckpoint {
    pub epoch: u64,
    pub generation: u64,
    /// Opaque payload that represents the durable state.
    pub payload: Vec<u8>,
}

/// Trait for searcher to allow mocking.
pub trait SearcherTrait: Send + Sync {
    /// Search using a query with optional note ID filter.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the query cannot be evaluated.
    fn search(
        &self,
        query: &QueryAst,
        note_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>, SearchError>;
}

/// Search error type.
#[derive(Debug, Error)]
pub enum SearchError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Checkpoint error: {0}")]
    Checkpoint(String),

    #[error("Index not initialized")]
    NotInitialized,

    #[error("Tantivy error: {0}")]
    Tantivy(String),
}

/// Stub search backend implementation.
///
/// This is a placeholder that will be replaced with the full tantivy
/// implementation in a follow-up PR.
pub struct StubSearchBackend {
    conn: Arc<Mutex<Connection>>,
    cache_dir: PathBuf,
    committed_generation: Arc<Mutex<u64>>,
    dirty: Arc<Mutex<bool>>,
}

impl StubSearchBackend {
    /// Build a stub backend over `conn`, recovering the committed
    /// generation from the index metadata.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::Sqlite`] when the metadata row cannot be read.
    pub fn new(conn: Connection, cache_dir: &Path) -> Result<Self, SearchError> {
        let committed_gen = get_meta_u64(&conn, MetaKey::TantivyCommittedGeneration)?.unwrap_or(0);

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            cache_dir: cache_dir.to_path_buf(),
            committed_generation: Arc::new(Mutex::new(committed_gen)),
            dirty: Arc::new(Mutex::new(false)),
        })
    }

    fn checkpoint_path(&self, epoch: u64, generation: u64) -> PathBuf {
        self.cache_dir
            .join("checkpoints")
            .join(format!("checkpoint-{epoch}-{generation}"))
    }
}

impl SearchBackend for StubSearchBackend {
    fn apply_document(&self, _note_id: i64, _title: &str, _body: &str) -> Result<(), SearchError> {
        *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))? = true;
        Ok(())
    }

    fn delete_document(&self, _note_id: i64) -> Result<(), SearchError> {
        *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))? = true;
        Ok(())
    }

    fn commit(&self) -> Result<(), SearchError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))?;
        let generation_val = get_meta_u64(&conn, MetaKey::Generation)?.unwrap_or(0);

        // Update committed generation
        set_meta(
            &conn,
            MetaKey::TantivyCommittedGeneration,
            &generation_val.to_string(),
        )?;
        *self
            .committed_generation
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))? = generation_val;
        *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))? = false;

        // Publish checkpoint
        self.publish_checkpoint(
            get_meta_u64(&conn, MetaKey::Epoch)?.unwrap_or(1),
            generation_val,
        )?;

        Ok(())
    }

    fn committed_generation(&self) -> u64 {
        *self.committed_generation.lock().unwrap()
    }

    fn set_committed_generation(&self, generation: u64) -> Result<(), SearchError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))?;
        set_meta(
            &conn,
            MetaKey::TantivyCommittedGeneration,
            &generation.to_string(),
        )?;
        *self
            .committed_generation
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))? = generation;
        Ok(())
    }

    fn checkpoint(&self) -> Result<SearchCheckpoint, SearchError> {
        let conn = self
            .conn
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))?;
        let epoch = get_meta_u64(&conn, MetaKey::Epoch)?.unwrap_or(1);
        let generation = get_meta_u64(&conn, MetaKey::Generation)?.unwrap_or(0);

        let checkpoint_path = self.checkpoint_path(epoch, generation);
        let payload = if checkpoint_path.exists() {
            std::fs::read(&checkpoint_path)?
        } else {
            Vec::new()
        };

        Ok(SearchCheckpoint {
            epoch,
            generation,
            payload,
        })
    }

    fn load_checkpoint(&mut self) -> Result<SearchCheckpoint, SearchError> {
        self.checkpoint()
    }

    fn publish_checkpoint(&self, epoch: u64, generation: u64) -> Result<(), SearchError> {
        use std::fs::{self, File};
        use std::io::Write;

        let checkpoint_dir = self.cache_dir.join("checkpoints");
        fs::create_dir_all(&checkpoint_dir)?;

        let temp_path = checkpoint_dir.join(format!(".checkpoint-{epoch}-{generation}.tmp"));
        let final_path = self.checkpoint_path(epoch, generation);

        let mut file = File::create(&temp_path)?;
        file.write_all(&[])?; // Empty payload for now
        file.sync_all()?;

        fs::rename(&temp_path, &final_path)?;

        Ok(())
    }

    fn search(
        &self,
        _query: &QueryAst,
        _note_ids: &[i64],
        _limit: usize,
    ) -> Result<Vec<i64>, SearchError> {
        // Commit if dirty (commit-before-search, D12)
        if *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("Lock poisoned".into()))?
        {
            self.commit()?;
        }

        // Return empty results for now
        Ok(Vec::new())
    }

    fn searcher(&self) -> Result<Box<dyn SearcherTrait>, SearchError> {
        Ok(Box::new(StubSearcher))
    }
}

/// Stub searcher implementation.
struct StubSearcher;

impl SearcherTrait for StubSearcher {
    fn search(
        &self,
        _query: &QueryAst,
        _note_ids: &[i64],
        _limit: usize,
    ) -> Result<Vec<i64>, SearchError> {
        Ok(Vec::new())
    }
}

/// Deferred commit manager.
///
/// This manages the commit-before-search policy and periodic commits.
pub struct CommitManager {
    backend: Arc<Mutex<Box<dyn SearchBackend>>>,
    conn: Connection,
}

impl CommitManager {
    pub fn new(backend: Arc<Mutex<Box<dyn SearchBackend>>>, conn: Connection) -> Self {
        Self { backend, conn }
    }

    /// Commit if there are pending changes.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] when the generation metadata cannot be read
    /// or the backend commit fails.
    pub fn commit_if_dirty(&self) -> Result<(), SearchError> {
        let backend = self
            .backend
            .lock()
            .map_err(|_| SearchError::Tantivy("Failed to lock backend".to_string()))?;

        // Check if dirty by comparing committed generation with current generation
        let current_gen = get_meta_u64(&self.conn, MetaKey::Generation)?.unwrap_or(0);
        if backend.committed_generation() < current_gen {
            backend.commit()
        } else {
            Ok(())
        }
    }

    /// Ensure commit-before-search: commit if there are pending changes.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError`] under the same conditions as
    /// [`CommitManager::commit_if_dirty`].
    pub fn ensure_committed(&self) -> Result<(), SearchError> {
        self.commit_if_dirty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_schema;
    use tempfile::{NamedTempFile, tempdir};

    #[test]
    fn test_stub_backend() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");

        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        let backend = StubSearchBackend::new(conn, &cache_dir).unwrap();
        assert_eq!(backend.committed_generation(), 0);
    }

    #[test]
    fn test_commit() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");

        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        let backend = StubSearchBackend::new(conn, &cache_dir).unwrap();

        // Mark as dirty
        backend.apply_document(1, "title", "body").unwrap();

        backend.commit().unwrap();

        assert_eq!(backend.committed_generation(), 0); // Generation is still 0
    }

    #[test]
    fn test_checkpoint() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");

        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        let backend = StubSearchBackend::new(conn, &cache_dir).unwrap();

        let checkpoint = backend.checkpoint().unwrap();
        assert_eq!(checkpoint.epoch, 1);
        assert_eq!(checkpoint.generation, 0);
    }

    #[test]
    fn dirty_search_commits_and_publishes_checkpoint() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        set_meta(&conn, MetaKey::Generation, "4").unwrap();

        let mut backend = StubSearchBackend::new(conn, &cache_dir).unwrap();
        backend.apply_document(8, "title", "body").unwrap();
        backend.delete_document(9).unwrap();
        assert!(
            backend
                .search(&QueryAst::Unresolved, &[], 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(backend.committed_generation(), 4);
        let checkpoint = backend.checkpoint().unwrap();
        assert_eq!((checkpoint.epoch, checkpoint.generation), (1, 4));
        assert!(cache_dir.join("checkpoints/checkpoint-1-4").exists());
        assert_eq!(
            backend
                .searcher()
                .unwrap()
                .search(&QueryAst::Text("x".into()), &[], 1)
                .unwrap(),
            Vec::<i64>::new()
        );
        assert_eq!(backend.load_checkpoint().unwrap(), checkpoint);
    }

    #[test]
    fn explicit_generation_and_commit_manager_are_idempotent() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        set_meta(&conn, MetaKey::Generation, "3").unwrap();

        let backend = StubSearchBackend::new(conn, &cache_dir).unwrap();
        backend.set_committed_generation(2).unwrap();
        assert_eq!(backend.committed_generation(), 2);
        let db = Connection::open(temp.path()).unwrap();
        let manager = CommitManager::new(Arc::new(Mutex::new(Box::new(backend))), db);
        manager.ensure_committed().unwrap();
        manager.commit_if_dirty().unwrap();
    }
}
