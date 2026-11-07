//! Search read side and Tantivy implementation (§4.3, RFC-0006 S06).
//!
//! This module provides the persistent Tantivy index read side, the D12
//! tokenizers, and the captured-searcher leaf evaluation used by the search
//! session evaluator (§6.3). All writes go through the single ordered
//! writer owner in [`crate::search_writer`]; this module never mutates the
//! index and never commits synchronously before a search (D-07: the old
//! commit-before-search contract is deleted).

use crate::query::{TextField, TextLeaf};
use tantivy::collector::TopDocs;
use tantivy::query::{BooleanQuery, Occur, PhraseQuery, Query as TantivyQuery, TermQuery};
use tantivy::schema::{
    Field, INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, TextAnalyzer, Token, TokenStream, Tokenizer,
};
use tantivy::{Index, IndexReader, TantivyDocument, Term};
use thiserror::Error;

/// One scored text-search hit. The score is Tantivy-internal and
/// relative-only (D11): never persisted, never compared across queries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    pub note_id: i64,
    pub score: f32,
}

/// Read side of the persistent Tantivy index. Structural data remains in
/// SQLite; the search index is a derived view owned by the writer thread.
#[derive(Clone)]
pub struct TantivyBackend {
    index: Index,
    reader: IndexReader,
    fields: SearchFields,
    index_dir: std::path::PathBuf,
}

impl std::fmt::Debug for TantivyBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TantivyBackend")
            .field("index_dir", &self.index_dir)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy)]
pub struct SearchFields {
    pub id: Field,
    pub title: Field,
    pub aliases: Field,
    pub tags: Field,
    pub body: Field,
}

impl TantivyBackend {
    /// Open or create a persistent Tantivy index below `cache_dir/tantivy`.
    ///
    /// # Errors
    /// Returns an error when Tantivy cannot be opened.
    pub fn open(cache_dir: &std::path::Path) -> Result<Self, SearchError> {
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
        // Manual reload policy: the single ordered writer thread is the
        // only reload trigger (right after each commit, so a completed
        // capture is stamped exactly at the generation it waited for).
        // The OnCommit default would run a background reload that races
        // the writer's own reload on the shared reader.
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()
            .map_err(|e: tantivy::TantivyError| SearchError::Tantivy(e.to_string()))?;
        Ok(Self {
            index,
            reader,
            fields,
            index_dir,
        })
    }

    /// Create the writer handed to the single ordered writer thread. Only
    /// [`crate::search_writer::spawn_search_writer`] may call this.
    ///
    /// # Errors
    /// Returns an error when the Tantivy writer cannot be created.
    pub fn create_writer(&self) -> Result<tantivy::IndexWriter, SearchError> {
        self.index
            .writer(50_000_000)
            .map_err(|e| SearchError::Tantivy(e.to_string()))
    }

    /// The shared reader. The writer thread reloads it after every commit;
    /// every searcher derived from this reader observes committed state.
    #[must_use]
    pub fn reader(&self) -> IndexReader {
        self.reader.clone()
    }

    #[must_use]
    pub const fn fields(&self) -> SearchFields {
        self.fields
    }

}

/// A searcher captured by the ordered writer, stamped with the generation
/// whose envelope was last committed. Immutable and clonable: every
/// evaluation against it observes exactly the same index state (D-07).
#[derive(Clone)]
pub struct CapturedSearcher {
    pub(crate) searcher: tantivy::Searcher,
    pub(crate) fields: SearchFields,
}

impl std::fmt::Debug for CapturedSearcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapturedSearcher")
            .field("num_docs", &self.searcher.num_docs())
            .finish_non_exhaustive()
    }
}

impl CapturedSearcher {
    /// Evaluate one typed text leaf (§6.3): explicit field scope, pinned
    /// analyzer, term or phrase input. There is no lenient query parser
    /// here: the term is tokenized by the pinned analyzer; a phrase leaf —
    /// or any term that tokenizes to more than one token — must match the
    /// token sequence positionally. Membership is complete up to `limit`
    /// (the evaluator passes the snapshot's document bound, never a fixed
    /// top-hit approximation).
    ///
    /// # Errors
    /// Returns an error when the query or collection fails.
    pub fn leaf_hits(&self, leaf: &TextLeaf, limit: usize) -> Result<Vec<Hit>, SearchError> {
        if leaf.term.is_empty() {
            // Literal empty query matches nothing (D-05: Idle, not "match
            // everywhere").
            return Ok(Vec::new());
        }
        let Some(mut analyzer) = self.searcher.index().tokenizers().get("flavite") else {
            return Err(SearchError::Tantivy("flavite analyzer missing".into()));
        };
        let tokens = tokenize_terms(&mut analyzer, &leaf.term);
        if tokens.is_empty() {
            return Ok(Vec::new());
        }
        let fields: Vec<Field> = if leaf.fields.is_empty() {
            vec![
                self.fields.title,
                self.fields.aliases,
                self.fields.tags,
                self.fields.body,
            ]
        } else {
            leaf.fields
                .iter()
                .map(|field| match field {
                    TextField::Title => self.fields.title,
                    TextField::Aliases => self.fields.aliases,
                    TextField::Tags => self.fields.tags,
                    TextField::Body => self.fields.body,
                })
                .collect()
        };
        let mut clauses: Vec<(Occur, Box<dyn TantivyQuery>)> = Vec::new();
        for field in fields {
            let clause: Box<dyn TantivyQuery> = if leaf.phrase || tokens.len() > 1 {
                let terms: Vec<Term> = tokens
                    .iter()
                    .map(|token| Term::from_field_text(field, token))
                    .collect();
                Box::new(PhraseQuery::new(terms))
            } else {
                Box::new(TermQuery::new(
                    Term::from_field_text(field, &tokens[0]),
                    IndexRecordOption::WithFreqsAndPositions,
                ))
            };
            clauses.push((Occur::Should, clause));
        }
        let query: Box<dyn TantivyQuery> = if clauses.len() == 1 {
            clauses.remove(0).1
        } else {
            Box::new(BooleanQuery::new(clauses))
        };
        let top = self
            .searcher
            .search(&*query, &TopDocs::with_limit(limit).order_by_score())
            .map_err(|e| SearchError::Tantivy(e.to_string()))?;
        let mut hits = Vec::new();
        for (score, addr) in top {
            if !score.is_finite() {
                return Err(SearchError::Tantivy("non-finite score".into()));
            }
            let document: TantivyDocument = self
                .searcher
                .doc(addr)
                .map_err(|e| SearchError::Tantivy(e.to_string()))?;
            if let Some(id) = document
                .get_first(self.fields.id)
                .and_then(|value| value.as_u64())
                .and_then(|value| i64::try_from(value).ok())
            {
                hits.push(Hit { note_id: id, score });
                if hits.len() >= limit {
                    break;
                }
            }
        }
        Ok(hits)
    }
}

/// Tokenize query text with the pinned analyzer.
fn tokenize_terms(analyzer: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let mut stream = analyzer.token_stream(text);
    let mut tokens = Vec::new();
    while stream.advance() {
        tokens.push(stream.token().text.clone());
    }
    tokens
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

/// Search error type.
#[derive(Debug, Error)]
pub enum SearchError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Tantivy error: {0}")]
    Tantivy(String),
}
