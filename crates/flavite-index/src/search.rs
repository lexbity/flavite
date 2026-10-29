//! Search backend abstraction and Tantivy implementation (§4.3, D12).
//!
//! This module provides the persistent Tantivy implementation, D12 tokenizers,
//! and `SearchBackend` abstraction. The stub backend remains a lightweight test
//! double; `IndexActor` uses `TantivyBackend` in production.

use crate::query::Query as QueryAst;
use crate::schema::{MetaKey, get_meta_u64, set_meta};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, QueryParser, TermSetQuery};
use tantivy::schema::{
    Field, INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use tantivy::{Index, IndexReader, IndexWriter, TantivyDocument, doc};
use thiserror::Error;

/// Maximum number of note IDs to pass to search as a filter (D12).
pub const MAX_TANTIVY_FILTER_SIZE: usize = 10_000;

/// Default deferred commit interval in seconds (D12, R4).
pub const DEFAULT_COMMIT_INTERVAL_SECS: u64 = 5;

/// Persistent Tantivy index. The SQLite connection is used only for the
/// authoritative generation mirror; structural data remains in SQLite.
pub struct TantivyBackend {
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: SearchFields,
    db_path: PathBuf,
    index_dir: PathBuf,
    committed_generation: Mutex<u64>,
    dirty: Mutex<bool>,
}

impl std::fmt::Debug for TantivyBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TantivyBackend")
            .field("index_dir", &self.index_dir)
            .field("committed_generation", &self.committed_generation())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
struct SearchFields {
    id: Field,
    title: Field,
    aliases: Field,
    tags: Field,
    body: Field,
}

impl TantivyBackend {
    /// Open or create a persistent Tantivy index below `cache_dir/tantivy`.
    ///
    /// # Errors
    /// Returns an error when SQLite or Tantivy cannot be opened.
    pub fn open(db_path: &Path, cache_dir: &Path) -> Result<Self, SearchError> {
        let index_dir = cache_dir.join("tantivy");
        std::fs::create_dir_all(&index_dir)?;
        let index = if index_dir.join("meta.json").exists() {
            Index::open_in_dir(&index_dir).map_err(|e| SearchError::Tantivy(e.to_string()))?
        } else {
            Index::create_in_dir(&index_dir, search_schema())
                .map_err(|e| SearchError::Tantivy(e.to_string()))?
        };
        register_search_tokenizers(&index);
        let fields = search_fields(&index.schema())?;
        let reader = index
            .reader()
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        let writer = index
            .writer(50_000_000)
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        let conn = Connection::open(db_path)?;
        let committed = get_meta_u64(&conn, MetaKey::TantivyCommittedGeneration)?.unwrap_or(0);
        Ok(Self {
            index,
            reader,
            writer: Mutex::new(writer),
            fields,
            db_path: db_path.to_path_buf(),
            index_dir,
            committed_generation: Mutex::new(committed),
            dirty: Mutex::new(false),
        })
    }

    /// Add or replace a document, including well-known metadata fields.
    ///
    /// # Errors
    /// Returns an error when Tantivy cannot apply the document.
    pub fn apply_document_full(
        &self,
        note_id: i64,
        title: &str,
        aliases: &[String],
        tags: &[String],
        body: &str,
    ) -> Result<(), SearchError> {
        let writer = self
            .writer
            .lock()
            .map_err(|_| SearchError::Tantivy("writer lock poisoned".into()))?;
        writer.delete_term(tantivy::Term::from_field_u64(
            self.fields.id,
            note_id.cast_unsigned(),
        ));
        let mut document = doc!(self.fields.id => note_id.cast_unsigned(), self.fields.title => title, self.fields.body => body);
        for value in aliases {
            document.add_text(self.fields.aliases, value);
        }
        for value in tags {
            document.add_text(self.fields.tags, value);
        }
        writer
            .add_document(document)
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("dirty lock poisoned".into()))? = true;
        Ok(())
    }

    /// Add or replace a degraded note's title-only search document.
    ///
    /// Degraded rows intentionally have no body, aliases, or tags in the
    /// search index, both incrementally and after a rebuild.
    ///
    /// # Errors
    /// Returns an error when Tantivy cannot apply the document.
    pub fn apply_title_only_document(&self, note_id: i64, title: &str) -> Result<(), SearchError> {
        let writer = self
            .writer
            .lock()
            .map_err(|_| SearchError::Tantivy("writer lock poisoned".into()))?;
        writer.delete_term(tantivy::Term::from_field_u64(
            self.fields.id,
            note_id.cast_unsigned(),
        ));
        writer
            .add_document(
                doc!(self.fields.id => note_id.cast_unsigned(), self.fields.title => title),
            )
            .map_err(|error| SearchError::Tantivy(error.to_string()))?;
        *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("dirty lock poisoned".into()))? = true;
        Ok(())
    }

    /// Rebuild the search index from the authoritative current search-doc cache.
    ///
    /// # Errors
    /// Returns an error when SQLite cannot be read or the Tantivy index cannot be rebuilt.
    pub fn rebuild_from_sqlite(&self) -> Result<(), SearchError> {
        let conn = Connection::open(&self.db_path)?;
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| SearchError::Tantivy("writer lock poisoned".into()))?;
        writer
            .delete_all_documents()
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        let mut stmt = conn.prepare("SELECT n.note_id,n.title,COALESCE(s.body_text,''),n.parse_ok FROM notes n LEFT JOIN search_docs s USING(note_id)")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, bool>(3)?,
            ))
        })?;
        let mut documents = Vec::new();
        for row in rows {
            let (id, title, body, parse_ok) = row?;
            documents.push((id, title, body, parse_ok));
        }
        for (id, title, body, parse_ok) in documents {
            if !parse_ok {
                writer
                    .add_document(doc!(self.fields.id=>id.cast_unsigned(),self.fields.title=>title))
                    .map_err(|error| SearchError::Tantivy(error.to_string()))?;
                continue;
            }
            let aliases = conn
                .prepare("SELECT alias FROM aliases WHERE note_id=?1 ORDER BY alias_key")?
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let tags = conn
                .prepare("SELECT tag FROM tags WHERE note_id=?1 ORDER BY tag")?
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let mut document = doc!(self.fields.id=>id.cast_unsigned(),self.fields.title=>title,self.fields.body=>body);
            for alias in aliases {
                document.add_text(self.fields.aliases, alias);
            }
            for tag in tags {
                document.add_text(self.fields.tags, tag);
            }
            writer
                .add_document(document)
                .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        }
        writer
            .commit()
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        self.reader
            .reload()
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        let generation = get_meta_u64(&conn, MetaKey::Generation)?.unwrap_or(0);
        set_meta(
            &conn,
            MetaKey::TantivyCommittedGeneration,
            &generation.to_string(),
        )?;
        *self
            .committed_generation
            .lock()
            .map_err(|_| SearchError::Tantivy("generation lock poisoned".into()))? = generation;
        *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("dirty lock poisoned".into()))? = false;
        Ok(())
    }

    /// Replay changed IDs from SQLite's invalidation log, then commit once.
    ///
    /// # Errors
    /// Returns an error when the change log cannot be read or replay cannot commit.
    pub fn replay_since(&self, generation: u64) -> Result<(), SearchError> {
        let conn = Connection::open(&self.db_path)?;
        let mut stmt=conn.prepare("SELECT DISTINCT entity_id FROM changes WHERE generation > ?1 AND kind IN ('note','outgoing') ORDER BY entity_id")?;
        let ids = stmt
            .query_map([i64::try_from(generation).unwrap_or(i64::MAX)], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            let current: Option<(String,String)> = conn.query_row("SELECT n.title,COALESCE(s.body_text,'') FROM notes n LEFT JOIN search_docs s USING(note_id) WHERE n.note_id=?1", [id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
            if let Some((title, body)) = current {
                let aliases = conn
                    .prepare("SELECT alias FROM aliases WHERE note_id=?1 ORDER BY alias_key")?
                    .query_map([id], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                let tags = conn
                    .prepare("SELECT tag FROM tags WHERE note_id=?1 ORDER BY tag")?
                    .query_map([id], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                self.apply_document_full(id, &title, &aliases, &tags, &body)?;
            } else {
                self.delete_document(id)?;
            }
        }
        self.commit()
    }

    /// Current generation in SQLite.
    ///
    /// # Errors
    /// Returns an error when generation metadata cannot be read.
    pub fn sqlite_generation(&self) -> Result<u64, SearchError> {
        Ok(get_meta_u64(&Connection::open(&self.db_path)?, MetaKey::Generation)?.unwrap_or(0))
    }

    /// Whether uncommitted Tantivy operations exist.
    pub fn is_dirty(&self) -> bool {
        *self
            .dirty
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Search documents matching text, optionally intersecting a structural ID set.
    ///
    /// # Errors
    /// Returns an error when commit-before-search or query execution fails.
    pub fn search_text(
        &self,
        query: &str,
        note_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>, SearchError> {
        if self.is_dirty() {
            self.commit()?;
        }
        if note_ids.len() <= MAX_TANTIVY_FILTER_SIZE {
            search_reader(
                &self.index,
                &self.reader,
                self.fields,
                query,
                note_ids,
                limit,
            )
        } else {
            let mut hits =
                search_reader(&self.index, &self.reader, self.fields, query, &[], 100_000)?;
            let allowed = note_ids
                .iter()
                .copied()
                .collect::<std::collections::HashSet<_>>();
            hits.retain(|id| allowed.contains(id));
            hits.truncate(limit);
            Ok(hits)
        }
    }
}

impl SearchBackend for TantivyBackend {
    fn apply_document(&self, note_id: i64, title: &str, body: &str) -> Result<(), SearchError> {
        self.apply_document_full(note_id, title, &[], &[], body)
    }
    fn delete_document(&self, note_id: i64) -> Result<(), SearchError> {
        self.writer
            .lock()
            .map_err(|_| SearchError::Tantivy("writer lock poisoned".into()))?
            .delete_term(tantivy::Term::from_field_u64(
                self.fields.id,
                note_id.cast_unsigned(),
            ));
        *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("dirty lock poisoned".into()))? = true;
        Ok(())
    }
    fn commit(&self) -> Result<(), SearchError> {
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| SearchError::Tantivy("writer lock poisoned".into()))?;
        writer
            .commit()
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        self.reader
            .reload()
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        let conn = Connection::open(&self.db_path)?;
        let generation = get_meta_u64(&conn, MetaKey::Generation)?.unwrap_or(0);
        set_meta(
            &conn,
            MetaKey::TantivyCommittedGeneration,
            &generation.to_string(),
        )?;
        *self
            .committed_generation
            .lock()
            .map_err(|_| SearchError::Tantivy("generation lock poisoned".into()))? = generation;
        *self
            .dirty
            .lock()
            .map_err(|_| SearchError::Tantivy("dirty lock poisoned".into()))? = false;
        self.publish_checkpoint(
            get_meta_u64(&conn, MetaKey::Epoch)?.unwrap_or(1),
            generation,
        )?;
        Ok(())
    }
    fn committed_generation(&self) -> u64 {
        *self
            .committed_generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn set_committed_generation(&self, generation: u64) -> Result<(), SearchError> {
        let conn = Connection::open(&self.db_path)?;
        set_meta(
            &conn,
            MetaKey::TantivyCommittedGeneration,
            &generation.to_string(),
        )?;
        *self
            .committed_generation
            .lock()
            .map_err(|_| SearchError::Tantivy("generation lock poisoned".into()))? = generation;
        Ok(())
    }
    fn checkpoint(&self) -> Result<SearchCheckpoint, SearchError> {
        let conn = Connection::open(&self.db_path)?;
        let epoch = get_meta_u64(&conn, MetaKey::Epoch)?.unwrap_or(1);
        let generation = self.committed_generation();
        let payload = std::fs::read(self.index_dir.join("checkpoint")).unwrap_or_default();
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
        use std::io::Write;
        let path = self.index_dir.join("checkpoint");
        let temp = self.index_dir.join("checkpoint.tmp");
        let mut f = std::fs::File::create(&temp)?;
        write!(f, "{epoch}:{generation}")?;
        f.sync_all()?;
        std::fs::rename(temp, path)?;
        Ok(())
    }
    fn search(
        &self,
        query: &QueryAst,
        note_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>, SearchError> {
        match query {
            QueryAst::Text(text) => self.search_text(text, note_ids, limit),
            _ => Ok(Vec::new()),
        }
    }
    fn searcher(&self) -> Result<Box<dyn SearcherTrait>, SearchError> {
        if self.is_dirty() {
            self.commit()?;
        }
        Ok(Box::new(TantivySearcher {
            index: self.index.clone(),
            reader: self.reader.clone(),
            fields: self.fields,
        }))
    }
}

struct TantivySearcher {
    index: Index,
    reader: IndexReader,
    fields: SearchFields,
}

impl SearcherTrait for TantivySearcher {
    fn search(
        &self,
        query: &QueryAst,
        note_ids: &[i64],
        limit: usize,
    ) -> Result<Vec<i64>, SearchError> {
        match query {
            QueryAst::Text(text) => search_reader(
                &self.index,
                &self.reader,
                self.fields,
                text,
                note_ids,
                limit,
            ),
            _ => Ok(Vec::new()),
        }
    }
}

fn search_reader(
    index: &Index,
    reader: &IndexReader,
    fields: SearchFields,
    query: &str,
    note_ids: &[i64],
    limit: usize,
) -> Result<Vec<i64>, SearchError> {
    let parser = QueryParser::for_index(
        index,
        vec![fields.title, fields.aliases, fields.tags, fields.body],
    );
    let (parsed, _) = parser.parse_query_lenient(query);
    let parsed: Box<dyn tantivy::query::Query> = if note_ids.is_empty() {
        parsed
    } else {
        let terms = note_ids
            .iter()
            .map(|id| tantivy::Term::from_field_u64(fields.id, id.cast_unsigned()));
        let id_filter = TermSetQuery::new(terms);
        Box::new(BooleanQuery::new(vec![
            (Occur::Must, parsed),
            (Occur::Must, Box::new(id_filter)),
        ]))
    };
    let searcher = reader.searcher();
    let top = searcher
        .search(&parsed, &TopDocs::with_limit(100_000).order_by_score())
        .map_err(|e| SearchError::Tantivy(e.to_string()))?;
    let mut ids = Vec::new();
    for (_score, addr) in top {
        let document: TantivyDocument = searcher
            .doc(addr)
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        if let Some(id) = document
            .get_first(fields.id)
            .and_then(|value| value.as_u64())
            .and_then(|value| i64::try_from(value).ok())
        {
            ids.push(id);
            if ids.len() >= limit {
                break;
            }
        }
    }
    Ok(ids)
}

fn search_schema() -> Schema {
    let mut b = Schema::builder();
    b.add_u64_field("note_id", STORED | INDEXED);
    let opts = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("flavite")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    b.add_text_field("title", opts.clone());
    b.add_text_field("aliases", opts.clone());
    b.add_text_field("tags", opts.clone());
    b.add_text_field("body", opts);
    b.build()
}
fn search_fields(schema: &Schema) -> Result<SearchFields, SearchError> {
    Ok(SearchFields {
        id: schema
            .get_field("note_id")
            .map_err(|e| SearchError::Tantivy(e.to_string()))?,
        title: schema
            .get_field("title")
            .map_err(|e| SearchError::Tantivy(e.to_string()))?,
        aliases: schema
            .get_field("aliases")
            .map_err(|e| SearchError::Tantivy(e.to_string()))?,
        tags: schema
            .get_field("tags")
            .map_err(|e| SearchError::Tantivy(e.to_string()))?,
        body: schema
            .get_field("body")
            .map_err(|e| SearchError::Tantivy(e.to_string()))?,
    })
}
fn register_search_tokenizers(index: &Index) {
    index.tokenizers().register(
        "flavite",
        TextAnalyzer::builder(CjkUnigramTokenizer)
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .build(),
    );
}
#[derive(Clone, Default)]
struct CjkUnigramTokenizer;
struct CjkStream {
    tokens: Vec<Token>,
    position: usize,
    current: Token,
}
impl Tokenizer for CjkUnigramTokenizer {
    type TokenStream<'a> = CjkStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let mut tokens = Vec::new();
        let mut chars = text.char_indices().peekable();
        let mut position = 0;
        while let Some((start, c)) = chars.next() {
            if is_cjk(c) {
                let mut run = vec![(start, c)];
                while let Some(&(off, next)) = chars.peek() {
                    if is_cjk(next) {
                        run.push((off, next));
                        chars.next();
                    } else {
                        break;
                    }
                }
                // Unigrams keep single-character CJK searches useful while
                // matching multi-character queries through their components.
                for (off, ch) in run {
                    tokens.push(Token {
                        offset_from: off,
                        offset_to: off + ch.len_utf8(),
                        position,
                        text: ch.to_string(),
                        position_length: 1,
                    });
                    position += 1;
                }
            } else if c.is_alphanumeric() {
                let mut end = start + c.len_utf8();
                while let Some(&(off, next)) = chars.peek() {
                    if !is_cjk(next) && next.is_alphanumeric() {
                        end = off + next.len_utf8();
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token {
                    offset_from: start,
                    offset_to: end,
                    position,
                    text: text[start..end].to_owned(),
                    position_length: 1,
                });
                position += 1;
            }
        }
        CjkStream {
            tokens,
            position: 0,
            current: Token::default(),
        }
    }
}
impl TokenStream for CjkStream {
    fn advance(&mut self) -> bool {
        if self.position >= self.tokens.len() {
            return false;
        }
        self.current = self.tokens[self.position].clone();
        self.position += 1;
        true
    }
    fn token(&self) -> &Token {
        &self.current
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.current
    }
}
fn is_cjk(c: char) -> bool {
    matches!(c as u32,0x3040..=0x30ff|0x3400..=0x4dbf|0x4e00..=0x9fff|0xf900..=0xfaff|0x20000..=0x2fa1f|0xac00..=0xd7af|0x1100..=0x11ff|0x3130..=0x318f|0xff66..=0xff9f)
}

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

/// In-memory behavior stub for tests that only exercise commit coordination.
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
    fn explicit_generation_updates_stub_generation() {
        let temp_dir = tempdir().unwrap();
        let cache_dir = temp_dir.path().join("cache");
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        set_meta(&conn, MetaKey::Generation, "3").unwrap();

        let backend = StubSearchBackend::new(conn, &cache_dir).unwrap();
        backend.set_committed_generation(2).unwrap();
        assert_eq!(backend.committed_generation(), 2);
        assert_eq!(backend.committed_generation(), 2);
    }
}
