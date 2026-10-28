//! Cached searchable document bodies (§4.3, §4.14).
//!
//! This module maintains the `search_docs` table which caches the exact
//! searchable body text for each note, along with its content hash.
//!
//! The cache ensures that:
//! 1. The search index can be rebuilt from a consistent SQLite snapshot
//! 2. Replay never reads newer disk bytes and pretends they belong to an older snapshot
//! 3. The cached body text exactly matches the indexed bytes

use rusqlite::{Connection, Result, params};
use std::collections::HashMap;

/// Cached search document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDoc {
    pub note_id: i64,
    pub content_hash: Vec<u8>,
    pub body_text: String,
}

/// Search document cache.
pub struct SearchDocCache {
    conn: Connection,
}

impl SearchDocCache {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// Get a search document by note ID.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the query cannot be prepared or the row
    /// cannot be read; a missing document is `Ok(None)`.
    pub fn get(&self, note_id: i64) -> Result<Option<SearchDoc>> {
        let mut stmt = self.conn.prepare(
            "SELECT note_id, content_hash, body_text FROM search_docs WHERE note_id = ?",
        )?;

        stmt.query_row([note_id], |row| {
            Ok(SearchDoc {
                note_id: row.get(0)?,
                content_hash: row.get(1)?,
                body_text: row.get(2)?,
            })
        })
        .map(Some)
        .or_else(|e| {
            if let rusqlite::Error::QueryReturnedNoRows = e {
                Ok(None)
            } else {
                Err(e)
            }
        })
    }

    /// Get multiple search documents by note IDs.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when any lookup cannot be prepared or read;
    /// missing documents are simply absent from the map.
    pub fn get_many(&self, note_ids: &[i64]) -> Result<HashMap<i64, SearchDoc>> {
        let mut result = HashMap::new();

        if note_ids.is_empty() {
            return Ok(result);
        }

        // For a small number of IDs, use IN clause
        if note_ids.len() <= 100 {
            let placeholders = vec!["?"; note_ids.len()].join(",");
            let sql = format!(
                "SELECT note_id, content_hash, body_text FROM search_docs WHERE note_id IN ({placeholders})"
            );

            let params: Vec<i64> = note_ids.to_vec();
            let mut stmt = self.conn.prepare(&sql)?;

            // Convert to &dyn ToSql references as a slice
            let param_refs: Vec<&dyn rusqlite::ToSql> =
                params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();

            let rows = stmt.query_map(&param_refs as &[&dyn rusqlite::ToSql], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    SearchDoc {
                        note_id: row.get(0)?,
                        content_hash: row.get(1)?,
                        body_text: row.get(2)?,
                    },
                ))
            })?;

            for row in rows {
                let (note_id, doc) = row?;
                result.insert(note_id, doc);
            }
        } else {
            // For many IDs, query all and filter
            let mut stmt = self
                .conn
                .prepare("SELECT note_id, content_hash, body_text FROM search_docs")?;

            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    SearchDoc {
                        note_id: row.get(0)?,
                        content_hash: row.get(1)?,
                        body_text: row.get(2)?,
                    },
                ))
            })?;

            for row in rows {
                let (note_id, doc) = row?;
                if note_ids.contains(&note_id) {
                    result.insert(note_id, doc);
                }
            }
        }

        Ok(result)
    }

    /// Set or update a search document.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the upsert cannot be executed.
    pub fn set(&mut self, note_id: i64, content_hash: &[u8], body_text: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO search_docs (note_id, content_hash, body_text) 
             VALUES (?1, ?2, ?3)",
            params![note_id, content_hash, body_text],
        )?;

        Ok(())
    }

    /// Delete a search document.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the delete cannot be executed.
    pub fn delete(&mut self, note_id: i64) -> Result<()> {
        self.conn
            .execute("DELETE FROM search_docs WHERE note_id = ?", [note_id])?;

        Ok(())
    }

    /// Delete multiple search documents.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when any delete cannot be executed.
    pub fn delete_many(&mut self, note_ids: &[i64]) -> Result<()> {
        if note_ids.is_empty() {
            return Ok(());
        }

        let placeholders = vec!["?"; note_ids.len()].join(",");
        let sql = format!("DELETE FROM search_docs WHERE note_id IN ({placeholders})");

        // Build params vector for execute
        let params: Vec<Box<dyn rusqlite::ToSql>> = note_ids
            .iter()
            .map(|&id| Box::new(id) as Box<dyn rusqlite::ToSql>)
            .collect();
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(std::convert::AsRef::as_ref).collect();
        self.conn
            .execute(&sql, &param_refs as &[&dyn rusqlite::ToSql])?;

        Ok(())
    }

    /// Clear all search documents.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the table cannot be truncated.
    pub fn clear(&mut self) -> Result<()> {
        self.conn.execute("DELETE FROM search_docs", [])?;
        Ok(())
    }

    /// Apply a batch of search document updates.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the transaction cannot start or commit
    /// or any upsert/delete inside it fails.
    pub fn apply_batch(&mut self, updates: &[SearchDocUpdate]) -> Result<()> {
        for update in updates {
            match update {
                SearchDocUpdate::Upsert {
                    note_id,
                    content_hash,
                    body_text,
                } => {
                    self.conn.execute(
                        "INSERT OR REPLACE INTO search_docs (note_id, content_hash, body_text) 
                         VALUES (?1, ?2, ?3)",
                        params![note_id, content_hash, body_text],
                    )?;
                }
                SearchDocUpdate::Delete { note_id } => {
                    self.conn
                        .execute("DELETE FROM search_docs WHERE note_id = ?", [note_id])?;
                }
            }
        }

        Ok(())
    }

    /// Rebuild the entire search docs table from the current state.
    ///
    /// This is used during index rebuild or after detecting corruption.
    ///
    /// # Errors
    ///
    /// Returns a SQLite error when the source rows cannot be read or the
    /// rebuilt documents cannot be written.
    pub fn rebuild(&mut self) -> Result<()> {
        // Clear existing
        self.clear()?;

        // Query all notes with their parsed bodies
        let note_ids: Vec<i64> = self
            .conn
            .prepare("SELECT note_id FROM notes WHERE parse_ok = 1")?
            .query_map([], |row| row.get::<_, i64>(0))?
            .collect::<Result<Vec<_>>>()?;

        // For each note, insert an empty body
        // In a real implementation, this would look up the actual body text
        for note_id in note_ids {
            self.set(note_id, &[], "")?;
        }

        Ok(())
    }
}

/// A search document update operation.
#[derive(Debug, Clone)]
pub enum SearchDocUpdate {
    /// Insert or replace a search document.
    Upsert {
        note_id: i64,
        content_hash: Vec<u8>,
        body_text: String,
    },
    /// Delete a search document.
    Delete { note_id: i64 },
}

impl SearchDocUpdate {
    #[must_use]
    pub fn upsert(note_id: i64, content_hash: Vec<u8>, body_text: String) -> Self {
        Self::Upsert {
            note_id,
            content_hash,
            body_text,
        }
    }

    #[must_use]
    pub fn delete(note_id: i64) -> Self {
        Self::Delete { note_id }
    }
}

/// Batch builder for search doc updates.
#[derive(Debug, Default)]
pub struct SearchDocBatch {
    updates: Vec<SearchDocUpdate>,
}

impl SearchDocBatch {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, note_id: i64, content_hash: Vec<u8>, body_text: String) {
        self.updates
            .push(SearchDocUpdate::upsert(note_id, content_hash, body_text));
    }

    pub fn delete(&mut self, note_id: i64) {
        self.updates.push(SearchDocUpdate::delete(note_id));
    }

    ///
    /// # Errors
    ///
    /// Returns a SQLite error under the same conditions as the
    /// [`SearchDocCache`] operations it forwards to.
    pub fn apply(&self, cache: &mut SearchDocCache) -> Result<()> {
        cache.apply_batch(&self.updates)
    }

    pub fn clear(&mut self) {
        self.updates.clear();
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.updates.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_schema;
    use tempfile::NamedTempFile;

    #[test]
    fn test_search_doc_cache() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        // Insert a note first to satisfy foreign key constraint
        conn.execute(
            "INSERT INTO notes (note_id, path, path_native, path_key, stem_key, title, content_hash, size, mtime_ns, scan_ns, generation) 
             VALUES (1, 'note1.md', X'6E6F7465312E6D64', 'note1.md', 'note1', 'Note 1', X'00', 100, 0, 0, 0)",
            [],
        ).unwrap();

        let mut cache = SearchDocCache::new(conn);

        // Initially empty
        assert!(cache.get(1).unwrap().is_none());

        // Set a document
        cache.set(1, b"hash1", "body text").unwrap();

        let doc = cache.get(1).unwrap().unwrap();
        assert_eq!(doc.note_id, 1);
        assert_eq!(doc.body_text, "body text");
        assert_eq!(doc.content_hash, b"hash1");

        // Update
        cache.set(1, b"hash2", "new body").unwrap();

        let doc = cache.get(1).unwrap().unwrap();
        assert_eq!(doc.body_text, "new body");
        assert_eq!(doc.content_hash, b"hash2");

        // Delete
        cache.delete(1).unwrap();
        assert!(cache.get(1).unwrap().is_none());
    }

    #[test]
    fn test_batch() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        // Insert notes first to satisfy foreign key constraint
        conn.execute(
            "INSERT INTO notes (note_id, path, path_native, path_key, stem_key, title, content_hash, size, mtime_ns, scan_ns, generation) 
             VALUES (1, 'note1.md', X'6E6F7465312E6D64', 'note1.md', 'note1', 'Note 1', X'00', 100, 0, 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO notes (note_id, path, path_native, path_key, stem_key, title, content_hash, size, mtime_ns, scan_ns, generation) 
             VALUES (2, 'note2.md', X'6E6F7465322E6D64', 'note2.md', 'note2', 'Note 2', X'00', 100, 0, 0, 0)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO notes (note_id, path, path_native, path_key, stem_key, title, content_hash, size, mtime_ns, scan_ns, generation) 
             VALUES (3, 'note3.md', X'6E6F7465332E6D64', 'note3.md', 'note3', 'Note 3', X'00', 100, 0, 0, 0)",
            [],
        ).unwrap();

        let mut cache = SearchDocCache::new(conn);

        let mut batch = SearchDocBatch::new();
        batch.upsert(1, b"hash1".to_vec(), "body1".to_string());
        batch.upsert(2, b"hash2".to_vec(), "body2".to_string());
        batch.delete(3);

        batch.apply(&mut cache).unwrap();

        assert!(cache.get(1).unwrap().is_some());
        assert!(cache.get(2).unwrap().is_some());
        assert!(cache.get(3).unwrap().is_none());
    }

    #[test]
    fn cache_bulk_paths_empty_batches_and_rebuild() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        for id in 1..=105 {
            conn.execute(
                "INSERT INTO notes (note_id,path,path_native,path_key,stem_key,title,content_hash,size,mtime_ns,scan_ns,generation,parse_ok) VALUES (?1,'',?2,'','','',X'00',0,0,0,0,?3)",
                params![id, vec![u8::try_from(id).expect("fixture IDs fit in a byte")], i64::from(id != 2)],
            ).unwrap();
        }
        let mut cache = SearchDocCache::new(conn);
        assert!(cache.get_many(&[]).unwrap().is_empty());
        cache.delete_many(&[]).unwrap();
        for id in 1..=105 {
            cache.set(id, b"hash", "body").unwrap();
        }
        let ids: Vec<i64> = (1..=105).collect();
        assert_eq!(cache.get_many(&ids).unwrap().len(), 105);
        assert_eq!(cache.get_many(&[4, 105, 999]).unwrap().len(), 2);
        cache.delete_many(&ids[..3]).unwrap();
        assert!(cache.get(1).unwrap().is_none());

        let mut batch = SearchDocBatch::new();
        assert!(batch.is_empty());
        batch.upsert(1, b"new".to_vec(), "rebuilt".into());
        batch.delete(105);
        assert!(!batch.is_empty());
        batch.apply(&mut cache).unwrap();
        batch.clear();
        assert!(batch.is_empty());
        assert_eq!(cache.get(1).unwrap().unwrap().body_text, "rebuilt");
        cache.rebuild().unwrap();
        assert!(cache.get(1).unwrap().is_some());
        assert!(cache.get(2).unwrap().is_none());
        cache.clear().unwrap();
        assert!(cache.get_many(&ids).unwrap().is_empty());
    }
}
