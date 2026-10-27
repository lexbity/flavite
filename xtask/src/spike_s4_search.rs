//! S4 search probe, slice 1 of 2 — CJK bigram quality + deferred commits under
//! autosave load (PLAN-C slice C3).
//!
//! Spec basis: §9 Phase 0 spike S4 ("tantivy CJK bigram quality,
//! deferred-commit behavior under autosave load — commit-before-search latency,
//! `committed_generation` replay"), D12 (SQLite is the record; tantivy is a
//! derived search view with deferred commits; bigram tokenization for
//! Han/Kana/Hangul runs), §4.7 (search UX), §4.14 (index versions, change
//! retention, search checkpoints — the checkpoint is published atomically with
//! its own durable commit, and the SQLite mirror is diagnostic only), §8 AC-15.
//!
//! The probe builds a deterministic CJK-heavy fixture, measures bigram-tokenizer
//! precision/recall against a plain (non-CJK) pipeline on a curated JA/ZH/KO
//! query set, simulates 1 s autosave writes over a 10-minute window (measuring
//! commit frequency and commit-before-search latency), simulates a crash
//! between apply and commit (measuring `committed_generation` replay timing and
//! correctness), and proves atomic checkpoint publication across crash
//! boundaries (before/after the SQLite mirror update, deletion before search
//! commit, and reader reload).
//!
//! It uses the same tantivy major (0.27) the production index adapter (C7
//! `tantivy.rs`) will use, and a real SQLite (WAL) mirror for the
//! checkpoint-publication crash boundaries. WAL single-writer/read-pool
//! concurrency is C4's scope, not this probe's.
//!
//! The probe is a dev tool, not product code. Its `--self-check` mode asserts
//! the properties the production wrappers rely on; CI re-runs it on Linux
//! headless with a scaled-down fixture. What is measured is recorded, never
//! fabricated. Findings land in `devdocs/spikes/S4-search.md`.

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Serialize;
use tantivy::collector::TopDocs;
use tantivy::query::QueryParser;
use tantivy::schema::{
    Field, INDEXED, IndexRecordOption, STORED, Schema, TextFieldIndexing, TextOptions, Value,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer, Token,
    TokenStream, Tokenizer,
};
use tantivy::{Index, IndexWriter, TantivyDocument, Term, doc};

use crate::spike_s3::{LatencyStats, latency_stats};

// ---------------------------------------------------------------------------
// D12 parameters under test
// ---------------------------------------------------------------------------

/// Autosave idle interval (1 s, default on) — one write per second (§4.7).
pub const AUTOSAVE_INTERVAL_SECS: u64 = 1;

/// Deferred-commit timer: commit at most every 5 s while dirty (D12, R4).
pub const COMMIT_INTERVAL_SECS: u64 = 5;

/// The autosave simulation window (10 minutes) (§4.7).
pub const AUTOSAVE_WINDOW_SECS: u64 = 600;

// ---------------------------------------------------------------------------
// CJK detection + bigram tokenizer (D12: bigram tokenization for Han/Kana/Hangul runs)
// ---------------------------------------------------------------------------

/// Whether a char belongs to a Han/Kana/Hangul run that the bigram tokenizer
/// segments. CJK punctuation (U+3000–U+303F, fullwidth forms) is intentionally
/// excluded so it acts as a token separator, matching the plain pipeline.
#[must_use]
pub fn is_cjk_script(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x3040..=0x309F   // Hiragana
        | 0x30A0..=0x30FF // Katakana
        | 0x3400..=0x4DBF // CJK Unified Ideographs Extension A
        | 0x4E00..=0x9FFF // CJK Unified Ideographs
        | 0xF900..=0xFAFF // CJK Compatibility Ideographs
        | 0xAC00..=0xD7AF // Hangul Syllables
        | 0x1100..=0x11FF // Hangul Jamo
        | 0x3130..=0x318F // Hangul Compatibility Jamo
        | 0xFF66..=0xFF9F // Halfwidth Katakana
    )
}

/// Segment `text` into bigram tokens for CJK runs and whole-word tokens for
/// non-CJK runs. A CJK run of length 1 emits a unigram; length ≥ 2 emits
/// overlapping bigrams. Non-CJK alphanumeric runs emit one word token (the
/// `SimpleTokenizer` rule). This is the pure core of [`CjkBigramTokenizer`] and
/// is unit-tested directly.
#[must_use]
pub fn tokenize_cjk_bigram(text: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut position = 0usize;
    let mut chars = text.char_indices().peekable();
    while let Some((start, c)) = chars.next() {
        if is_cjk_script(c) {
            let mut run: Vec<(usize, char)> = vec![(start, c)];
            while let Some(&(off, cc)) = chars.peek() {
                if is_cjk_script(cc) {
                    run.push((off, cc));
                    chars.next();
                } else {
                    break;
                }
            }
            if run.len() == 1 {
                let (off, ch) = run[0];
                tokens.push(Token {
                    offset_from: off,
                    offset_to: off + ch.len_utf8(),
                    position,
                    text: ch.to_string(),
                    position_length: 1,
                });
                position += 1;
            } else {
                for i in 0..run.len() - 1 {
                    let (from, _first) = run[i];
                    let (to, last) = run[i + 1];
                    tokens.push(Token {
                        offset_from: from,
                        offset_to: to + last.len_utf8(),
                        position,
                        text: text[from..to + last.len_utf8()].to_string(),
                        position_length: 1,
                    });
                    position += 1;
                }
            }
        } else if c.is_alphanumeric() {
            let mut end = start + c.len_utf8();
            while let Some(&(off, cc)) = chars.peek() {
                if !is_cjk_script(cc) && cc.is_alphanumeric() {
                    end = off + cc.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(Token {
                offset_from: start,
                offset_to: end,
                position,
                text: text[start..end].to_string(),
                position_length: 1,
            });
            position += 1;
        }
        // else: whitespace/punctuation/CJK punctuation — a separator, skipped.
    }
    tokens
}

/// A tantivy [`Tokenizer`] that bigram-segments CJK runs and word-segments
/// non-CJK runs (D12). Built on [`tokenize_cjk_bigram`].
#[derive(Clone, Default)]
pub struct CjkBigramTokenizer;

/// Token stream produced by [`CjkBigramTokenizer`]. Tokens are pre-computed for
/// the whole text (note bodies are small); `advance` walks them.
pub struct CjkBigramTokenStream {
    tokens: Vec<Token>,
    idx: usize,
    cur: Token,
}

impl Tokenizer for CjkBigramTokenizer {
    type TokenStream<'a> = CjkBigramTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        CjkBigramTokenStream {
            tokens: tokenize_cjk_bigram(text),
            idx: 0,
            cur: Token::default(),
        }
    }
}

impl TokenStream for CjkBigramTokenStream {
    fn advance(&mut self) -> bool {
        if self.idx < self.tokens.len() {
            self.cur = self.tokens[self.idx].clone();
            self.idx += 1;
            true
        } else {
            false
        }
    }

    fn token(&self) -> &Token {
        &self.cur
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.cur
    }
}

// ---------------------------------------------------------------------------
// Fixture (deterministic, CJK-heavy)
// ---------------------------------------------------------------------------

/// A generated note in the fixture.
#[derive(Debug, Clone)]
pub struct Note {
    /// Stable numeric identity (the tantivy `note_id`).
    pub note_id: u64,
    /// Vault-relative path.
    pub path: String,
    /// Title (CJK-heavy).
    pub title: String,
    /// Body (CJK-heavy, plus a unique per-note Latin sentinel for verification).
    pub body: String,
}

const JA_PHRASES: &[&str] = &[
    "東京都",
    "渋谷区",
    "京都寺",
    "富士山",
    "桜祭り",
    "寿司店",
    "ラーメン",
    "電車駅",
    "図書館",
    "神社",
];
const ZH_PHRASES: &[&str] = &[
    "北京市",
    "上海市",
    "人工智能",
    "机器学习",
    "数据中心",
    "长城",
    "珠江",
    "黄河",
];
const KO_PHRASES: &[&str] = &[
    "서울시",
    "부산시",
    "한국어",
    "서울타워",
    "데이터",
    "한글",
    "강원도",
];
const EN_PHRASES: &[&str] = &[
    "search engine",
    "index rebuild",
    "tantivy commit",
    "autosave latency",
    "replay checkpoint",
    "wal reader",
];

/// A curated query with its language and whether it is a CJK query.
#[derive(Debug, Clone, Copy)]
pub struct ProbeQuery {
    /// The query text (a term or substring of a fixture phrase).
    pub text: &'static str,
    /// Language label (`ja`, `zh`, `ko`, `en`).
    pub lang: &'static str,
    /// Whether this is a CJK (Han/Kana/Hangul) query.
    pub cjk: bool,
}

/// The curated query set: CJK multi-char terms and substrings (where bigram
/// should excel), one single-char CJK query (the known bigram limitation, for
/// honesty), and Latin control queries.
pub const QUERIES: &[ProbeQuery] = &[
    ProbeQuery {
        text: "東京",
        lang: "ja",
        cjk: true,
    },
    ProbeQuery {
        text: "渋谷",
        lang: "ja",
        cjk: true,
    },
    ProbeQuery {
        text: "京都",
        lang: "ja",
        cjk: true,
    },
    ProbeQuery {
        text: "富士山",
        lang: "ja",
        cjk: true,
    },
    ProbeQuery {
        text: "桜",
        lang: "ja",
        cjk: true,
    },
    ProbeQuery {
        text: "北京",
        lang: "zh",
        cjk: true,
    },
    ProbeQuery {
        text: "上海",
        lang: "zh",
        cjk: true,
    },
    ProbeQuery {
        text: "人工智能",
        lang: "zh",
        cjk: true,
    },
    ProbeQuery {
        text: "机器",
        lang: "zh",
        cjk: true,
    },
    ProbeQuery {
        text: "서울",
        lang: "ko",
        cjk: true,
    },
    ProbeQuery {
        text: "부산",
        lang: "ko",
        cjk: true,
    },
    ProbeQuery {
        text: "한국",
        lang: "ko",
        cjk: true,
    },
    ProbeQuery {
        text: "데이터",
        lang: "ko",
        cjk: true,
    },
    ProbeQuery {
        text: "search",
        lang: "en",
        cjk: false,
    },
    ProbeQuery {
        text: "tantivy",
        lang: "en",
        cjk: false,
    },
    ProbeQuery {
        text: "autosave",
        lang: "en",
        cjk: false,
    },
];

/// A tiny deterministic PRNG (xorshift64) so the fixture is reproducible with no
/// extra dependency.
pub struct Rng(u64);

impl Rng {
    /// Create a PRNG from `seed`.
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1)
    }

    /// The next pseudo-random `u64`.
    #[must_use]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A pseudo-random index in `0..n` (`n > 0`).
    #[must_use]
    pub fn below(&mut self, n: usize) -> usize {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        {
            (self.next_u64() % n as u64) as usize
        }
    }
}

fn lang_phrases(lang: &str) -> &'static [&'static str] {
    match lang {
        "ja" => JA_PHRASES,
        "zh" => ZH_PHRASES,
        "ko" => KO_PHRASES,
        _ => EN_PHRASES,
    }
}

const LANGS: &[&str] = &["ja", "zh", "ko", "en"];

/// Build a deterministic `n`-note CJK-heavy fixture. Each note's body carries a
/// unique `uniquenote<i>` Latin sentinel so replay/publication can verify exact
/// document presence without relying on shared vocabulary.
#[must_use]
pub fn build_fixture(n: usize) -> Vec<Note> {
    let mut rng = Rng::new(0x53_34_u64); // "S4"
    let mut notes = Vec::with_capacity(n);
    for i in 0..n {
        let lang = LANGS[i % LANGS.len()];
        let phrases = lang_phrases(lang);
        let title_phrase = phrases[rng.below(phrases.len())];
        let title = format!("{title_phrase} · {i}");
        let mut body = String::new();
        for _ in 0..6 {
            body.push_str(phrases[rng.below(phrases.len())]);
            body.push('。');
        }
        body.push_str(EN_PHRASES[rng.below(EN_PHRASES.len())]);
        body.push(' ');
        body.push_str(&format!("uniquenote{i}"));
        notes.push(Note {
            note_id: (i as u64) + 1,
            path: format!("notes/note-{i}.md"),
            title,
            body,
        });
    }
    notes
}

// ---------------------------------------------------------------------------
// Index construction (schema, tokenizers, fields)
// ---------------------------------------------------------------------------

/// Field handles for the probe index.
#[derive(Debug, Clone, Copy)]
pub struct Fields {
    /// The stored+indexed note identity.
    pub note_id: Field,
    /// Title indexed with the bigram pipeline.
    pub title_bigram: Field,
    /// Body indexed with the bigram pipeline.
    pub body_bigram: Field,
    /// Title indexed with the plain pipeline.
    pub title_plain: Field,
    /// Body indexed with the plain pipeline.
    pub body_plain: Field,
}

/// Convert a `u64` into the `i64` SQLite stores (clamping — note IDs and
/// generations stay well within `i64` for any realistic fixture).
#[must_use]
pub fn sql_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Convert a SQLite `i64` back into a `u64`.
#[must_use]
pub fn from_sql_i64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn text_options(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer(tokenizer)
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    )
}

fn build_schema() -> Schema {
    let mut sb = Schema::builder();
    sb.add_u64_field("note_id", STORED | INDEXED);
    sb.add_text_field("title_bigram", text_options("cjk_bigram"));
    sb.add_text_field("body_bigram", text_options("cjk_bigram"));
    sb.add_text_field("title_plain", text_options("plain"));
    sb.add_text_field("body_plain", text_options("plain"));
    sb.build()
}

fn bigram_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(CjkBigramTokenizer)
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

fn plain_analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(40))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

fn register_tokenizers(index: &Index) {
    let tokenizers = index.tokenizers();
    tokenizers.register("cjk_bigram", bigram_analyzer());
    tokenizers.register("plain", plain_analyzer());
}

fn fields_from_index(index: &Index) -> Result<Fields> {
    let schema = index.schema();
    Ok(Fields {
        note_id: schema.get_field("note_id")?,
        title_bigram: schema.get_field("title_bigram")?,
        body_bigram: schema.get_field("body_bigram")?,
        title_plain: schema.get_field("title_plain")?,
        body_plain: schema.get_field("body_plain")?,
    })
}

/// Create a fresh index (RAM when `dir` is `None`, otherwise on disk) and
/// register the probe tokenizers.
///
/// # Errors
/// Returns an error if the index cannot be created or a field is missing.
pub fn create_index(dir: Option<&Path>) -> Result<(Index, Fields)> {
    let schema = build_schema();
    let index = match dir {
        None => Index::create_in_ram(schema),
        Some(d) => {
            std::fs::create_dir_all(d)
                .with_context(|| format!("create index dir: {}", d.display()))?;
            Index::create_in_dir(d, schema)
                .with_context(|| format!("create index in dir: {}", d.display()))?
        }
    };
    register_tokenizers(&index);
    let fields = fields_from_index(&index)?;
    Ok((index, fields))
}

/// Reopen an existing on-disk index and re-register the probe tokenizers (they
/// are not persisted).
///
/// # Errors
/// Returns an error if the index cannot be opened or a field is missing.
pub fn reopen_index(dir: &Path) -> Result<(Index, Fields)> {
    let index =
        Index::open_in_dir(dir).with_context(|| format!("open index in dir: {}", dir.display()))?;
    register_tokenizers(&index);
    let fields = fields_from_index(&index)?;
    Ok((index, fields))
}

fn index_doc(fields: &Fields, note: &Note) -> TantivyDocument {
    doc!(
        fields.note_id => note.note_id,
        fields.title_bigram => note.title.as_str(),
        fields.body_bigram => note.body.as_str(),
        fields.title_plain => note.title.as_str(),
        fields.body_plain => note.body.as_str(),
    )
}

fn index_body_only(fields: &Fields, note_id: u64, body: &str) -> TantivyDocument {
    doc!(
        fields.note_id => note_id,
        fields.body_bigram => body,
        fields.body_plain => body,
    )
}

/// Search the index and return the matching `note_id`s. `bigram` selects the
/// pipeline (bigram vs plain fields); the query is tokenized by the field's
/// registered tokenizer, exactly as production would.
///
/// # Errors
/// Returns an error if the search fails.
pub fn search_note_ids(
    index: &Index,
    fields: &Fields,
    reader: &tantivy::IndexReader,
    query: &str,
    bigram: bool,
    limit: usize,
) -> Result<Vec<u64>> {
    let default_fields = if bigram {
        vec![fields.title_bigram, fields.body_bigram]
    } else {
        vec![fields.title_plain, fields.body_plain]
    };
    let parser = QueryParser::for_index(index, default_fields);
    let (q, _errs) = parser.parse_query_lenient(query);
    let searcher = reader.searcher();
    let top = searcher.search(&q, &TopDocs::with_limit(limit).order_by_score())?;
    let mut ids = Vec::with_capacity(top.len());
    for (_score, addr) in top {
        let retrieved: TantivyDocument = searcher.doc(addr)?;
        if let Some(id) = retrieved.get_first(fields.note_id).and_then(|v| v.as_u64()) {
            ids.push(id);
        }
    }
    Ok(ids)
}

// ---------------------------------------------------------------------------
// Checkpoint publication (§4.14: the checkpoint is published atomically with
// the search commit, via a commit payload — NOT an independently updated counter)
// ---------------------------------------------------------------------------

/// Format a search checkpoint `(epoch, generation)` as a commit payload.
#[must_use]
pub fn checkpoint_payload(epoch: u64, generation: u64) -> String {
    format!("{epoch}:{generation}")
}

/// Parse a checkpoint payload back into `(epoch, generation)`.
#[must_use]
pub fn parse_checkpoint(payload: &str) -> Option<(u64, u64)> {
    let (e, g) = payload.split_once(':')?;
    Some((e.parse().ok()?, g.parse().ok()?))
}

/// Commit the writer with an atomic checkpoint payload `(epoch, generation)`.
///
/// # Errors
/// Returns an error if the commit fails.
pub fn commit_with_checkpoint(
    writer: &mut IndexWriter<TantivyDocument>,
    index: &Index,
    epoch: u64,
    generation: u64,
) -> Result<()> {
    let mut prepared = writer.prepare_commit()?;
    prepared.set_payload(&checkpoint_payload(epoch, generation));
    prepared.commit()?;
    // Sanity: the payload must be readable back atomically from the commit.
    let metas = index.load_metas()?;
    debug_assert_eq!(
        metas.payload.as_deref(),
        Some(checkpoint_payload(epoch, generation).as_str())
    );
    Ok(())
}

/// Read the durable search checkpoint `(epoch, generation)` from the last commit.
///
/// # Errors
/// Returns an error if the commit metadata cannot be read.
pub fn read_checkpoint(index: &Index) -> Result<Option<(u64, u64)>> {
    let metas = index.load_metas()?;
    Ok(metas.payload.as_deref().and_then(parse_checkpoint))
}

// ---------------------------------------------------------------------------
// Diagnostic SQLite mirror (§4.14: advisory only; updated AFTER the search
// commit succeeds; the checkpoint payload is the durability authority)
// ---------------------------------------------------------------------------

/// The diagnostic SQLite mirror: `meta` (epoch/generation/committed counter),
/// `search_docs` (cached exact indexed body), and `changes` (bounded invalidation
/// log). WAL mode; single connection (concurrency is C4's scope).
pub struct Mirror {
    conn: Connection,
    epoch: u64,
}

impl Mirror {
    /// Open (creating if needed) the mirror at `path`, minting an epoch if
    /// absent.
    ///
    /// # Errors
    /// Returns an error if the database cannot be opened or initialized.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)
            .with_context(|| format!("open mirror db: {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS search_docs(note_id INTEGER PRIMARY KEY, body TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS changes(
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 generation INTEGER NOT NULL,
                 note_id INTEGER NOT NULL,
                 deleted INTEGER NOT NULL);",
        )?;
        let epoch: Option<u64> = conn
            .query_row("SELECT value FROM meta WHERE key='epoch'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| s.parse().ok());
        let epoch = match epoch {
            Some(e) => e,
            None => {
                let e = std::process::id() as u64;
                conn.execute(
                    "INSERT INTO meta(key, value) VALUES('epoch', ?1)",
                    params![e.to_string()],
                )?;
                e
            }
        };
        Ok(Mirror { conn, epoch })
    }

    /// The mirror's epoch.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The authoritative structural generation (bumped on every applied change).
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.conn
            .query_row("SELECT value FROM meta WHERE key='generation'", [], |r| {
                r.get::<_, String>(0)
            })
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    fn set_meta(&self, key: &str, value: u64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value.to_string()],
        )?;
        Ok(())
    }

    /// Apply an upsert: bump the generation, replace the cached body, and record
    /// an invalidation. Returns the new generation.
    ///
    /// # Errors
    /// Returns an error if the transaction fails.
    pub fn apply_upsert(&mut self, note_id: u64, body: &str) -> Result<u64> {
        let next_gen = self.generation() + 1;
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO search_docs(note_id, body) VALUES(?1, ?2)
             ON CONFLICT(note_id) DO UPDATE SET body=excluded.body",
            params![sql_i64(note_id), body],
        )?;
        tx.execute(
            "INSERT INTO changes(generation, note_id, deleted) VALUES(?1, ?2, 0)",
            params![sql_i64(next_gen), sql_i64(note_id)],
        )?;
        tx.commit()?;
        self.set_meta("generation", next_gen)?;
        Ok(next_gen)
    }

    /// Apply a delete: bump the generation, remove the cached body, and record a
    /// tombstone invalidation. Returns the new generation.
    ///
    /// # Errors
    /// Returns an error if the transaction fails.
    pub fn apply_delete(&mut self, note_id: u64) -> Result<u64> {
        let next_gen = self.generation() + 1;
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM search_docs WHERE note_id=?1",
            params![sql_i64(note_id)],
        )?;
        tx.execute(
            "INSERT INTO changes(generation, note_id, deleted) VALUES(?1, ?2, 1)",
            params![sql_i64(next_gen), sql_i64(note_id)],
        )?;
        tx.commit()?;
        self.set_meta("generation", next_gen)?;
        Ok(next_gen)
    }

    /// Distinct invalidated `note_id`s with a generation greater than
    /// `committed_generation` (the replay feed).
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn invalidations_since(&self, committed_generation: u64) -> Result<Vec<u64>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT note_id FROM changes WHERE generation > ?1 ORDER BY note_id",
        )?;
        let ids = stmt
            .query_map(params![sql_i64(committed_generation)], |r| {
                r.get::<_, i64>(0)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
            .into_iter()
            .map(from_sql_i64)
            .collect::<Vec<u64>>();
        Ok(ids)
    }

    /// The cached searchable body for a note, if it still exists.
    ///
    /// # Errors
    /// Returns an error if the query fails.
    pub fn body(&self, note_id: u64) -> Result<Option<String>> {
        let body = self
            .conn
            .query_row(
                "SELECT body FROM search_docs WHERE note_id=?1",
                params![sql_i64(note_id)],
                |r| r.get::<_, String>(0),
            )
            .ok();
        Ok(body)
    }

    /// The advisory diagnostic counter for what tantivy has committed. NOT the
    /// durability authority (the checkpoint payload is).
    #[must_use]
    pub fn tantivy_committed(&self) -> u64 {
        self.conn
            .query_row(
                "SELECT value FROM meta WHERE key='tantivy_committed_generation'",
                [],
                |r| r.get::<_, String>(0),
            )
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    /// Update the advisory diagnostic counter (mirrors, in production, the step
    /// that runs AFTER a successful search commit).
    ///
    /// # Errors
    /// Returns an error if the write fails.
    pub fn set_tantivy_committed(&self, generation: u64) -> Result<()> {
        self.set_meta("tantivy_committed_generation", generation)
    }

    /// Force-set the structural generation (used to stage crash scenarios).
    ///
    /// # Errors
    /// Returns an error if the write fails.
    pub fn force_generation(&self, generation: u64) -> Result<()> {
        self.set_meta("generation", generation)
    }
}

// ---------------------------------------------------------------------------
// Scenario reports
// ---------------------------------------------------------------------------

/// Aggregated precision/recall/F1 for one pipeline over a query class.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PipelineAgg {
    /// Number of queries aggregated.
    pub queries: usize,
    /// Mean precision.
    pub mean_precision: f64,
    /// Mean recall.
    pub mean_recall: f64,
    /// Mean F1.
    pub mean_f1: f64,
    /// Queries with zero recall (missed every relevant doc).
    pub zero_recall: usize,
}

/// One curated query's measured quality, both pipelines.
#[derive(Debug, Clone, Serialize)]
pub struct QueryRow {
    /// The query text.
    pub query: String,
    /// Language label.
    pub lang: String,
    /// Whether it is a CJK query.
    pub cjk: bool,
    /// Ground-truth relevant documents (title/body contains the query).
    pub relevant: usize,
    /// Bigram hits retrieved.
    pub bigram_hits: usize,
    /// Plain hits retrieved.
    pub plain_hits: usize,
    /// Bigram precision.
    pub bigram_precision: f64,
    /// Bigram recall.
    pub bigram_recall: f64,
    /// Plain precision.
    pub plain_precision: f64,
    /// Plain recall.
    pub plain_recall: f64,
}

/// Scenario 1 report: CJK bigram quality vs the plain pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct QualityReport {
    /// Notes indexed.
    pub notes: usize,
    /// Bigram pipeline over CJK queries.
    pub bigram_cjk: PipelineAgg,
    /// Bigram pipeline over Latin control queries.
    pub bigram_latin: PipelineAgg,
    /// Plain pipeline over CJK queries.
    pub plain_cjk: PipelineAgg,
    /// Plain pipeline over Latin control queries.
    pub plain_latin: PipelineAgg,
    /// Per-query rows.
    pub rows: Vec<QueryRow>,
}

/// Scenario 2 report: deferred commits under autosave load.
#[derive(Debug, Clone, Serialize)]
pub struct AutosaveReport {
    /// Autosave writes simulated.
    pub saves: u32,
    /// Window length in seconds.
    pub window_secs: u64,
    /// Tantivy commits actually performed (deferred).
    pub commit_count: u32,
    /// Ratio commit_count / saves (should be ≪ 1).
    pub commit_ratio: f64,
    /// Commit-before-search added latency (the commit cost), µs.
    pub commit_before_search_us: LatencyStats,
    /// Baseline search latency with nothing dirty, µs.
    pub baseline_search_us: LatencyStats,
}

/// Scenario 3 report: crash + `committed_generation` replay.
#[derive(Debug, Clone, Serialize)]
pub struct ReplayReport {
    /// Checkpoint generation read on reopen (pre-replay).
    pub checkpoint_generation: u64,
    /// Mirror structural generation at crash time.
    pub mirror_generation: u64,
    /// Invalidated notes replayed.
    pub invalidated: usize,
    /// Replay wall time, µs.
    pub replay_us: u64,
    /// Whether the applied upsert became visible after replay.
    pub update_visible: bool,
    /// Whether the applied delete was honored after replay.
    pub deletion_honored: bool,
    /// Whether the checkpoint advanced to the mirror generation.
    pub checkpoint_advanced: bool,
}

/// Scenario 4 report: atomic checkpoint publication across crash boundaries.
#[derive(Debug, Clone, Serialize)]
pub struct PublicationReport {
    /// Checkpoint payload is the durability authority (not the SQLite counter).
    pub checkpoint_is_authority: bool,
    /// Crash after search commit, before mirror update: reopen is correct.
    pub crash_after_commit_before_mirror_ok: bool,
    /// Crash after mirror update: checkpoint and mirror agree.
    pub crash_after_mirror_ok: bool,
    /// A deletion applied before the search commit is gone after commit+reload.
    pub deletion_before_commit_gone: bool,
    /// Reader reload reflects a just-committed document.
    pub reader_reload_ok: bool,
    /// An ahead checkpoint (search generation > mirror generation) is detected.
    pub ahead_checkpoint_detected: bool,
    /// The checkpoint generation observed.
    pub checkpoint_generation: u64,
    /// The mirror's advisory committed counter observed.
    pub mirror_committed: u64,
}

/// A scenario report.
#[derive(Debug, Clone, Serialize)]
pub enum ScenarioReport {
    /// Scenario 1.
    Quality(QualityReport),
    /// Scenario 2.
    Autosave(AutosaveReport),
    /// Scenario 3.
    Replay(ReplayReport),
    /// Scenario 4.
    Publication(PublicationReport),
}

/// A named scenario report.
#[derive(Debug, Clone, Serialize)]
pub struct NamedReport {
    /// Scenario name (`1-quality`, …, `4-publication`).
    pub name: String,
    /// The report.
    pub report: ScenarioReport,
}

/// A full probe run.
#[derive(Debug, Clone, Serialize)]
pub struct RunSummary {
    /// Host OS.
    pub os: String,
    /// Host architecture.
    pub arch: String,
    /// Fixture size (notes).
    pub notes: usize,
    /// Scenario reports, in scenario order.
    pub scenarios: Vec<NamedReport>,
}

/// Which scenarios to run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScenarioSelection {
    /// Scenario 1: CJK bigram quality.
    pub quality: bool,
    /// Scenario 2: deferred commits under autosave.
    pub autosave: bool,
    /// Scenario 3: crash + replay.
    pub replay: bool,
    /// Scenario 4: atomic checkpoint publication.
    pub publication: bool,
}

/// Parse a scenario list: `all` or a comma-separated subset of `1..4`.
///
/// # Errors
/// Returns an error on an unknown or empty scenario selection.
pub fn parse_selection(input: &str) -> Result<ScenarioSelection> {
    if input == "all" {
        return Ok(ScenarioSelection {
            quality: true,
            autosave: true,
            replay: true,
            publication: true,
        });
    }
    let mut selection = ScenarioSelection::default();
    for part in input.split(',') {
        match part.trim() {
            "1" => selection.quality = true,
            "2" => selection.autosave = true,
            "3" => selection.replay = true,
            "4" => selection.publication = true,
            other => anyhow::bail!("unknown scenario '{other}' (expected 1..4 or all)"),
        }
    }
    if !(selection.quality || selection.autosave || selection.replay || selection.publication) {
        anyhow::bail!("empty scenario selection");
    }
    Ok(selection)
}

// ---------------------------------------------------------------------------
// Scenario runners
// ---------------------------------------------------------------------------

fn ground_truth(notes: &[Note], query: &str) -> Vec<u64> {
    notes
        .iter()
        .filter(|n| n.title.contains(query) || n.body.contains(query))
        .map(|n| n.note_id)
        .collect()
}

fn prf(relevant: &[u64], hits: &[u64]) -> (f64, f64, f64) {
    let truth: BTreeSet<u64> = relevant.iter().copied().collect();
    let got: BTreeSet<u64> = hits.iter().copied().collect();
    let tp = truth.intersection(&got).count();
    let precision = if got.is_empty() {
        0.0
    } else {
        tp as f64 / got.len() as f64
    };
    let recall = if truth.is_empty() {
        0.0
    } else {
        tp as f64 / truth.len() as f64
    };
    let f1 = if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    };
    (precision, recall, f1)
}

fn aggregate(rows: &[QueryRow], cjk: bool, bigram: bool) -> PipelineAgg {
    let sel: Vec<&QueryRow> = rows.iter().filter(|r| r.cjk == cjk).collect();
    let n = sel.len();
    if n == 0 {
        return PipelineAgg {
            queries: 0,
            mean_precision: 0.0,
            mean_recall: 0.0,
            mean_f1: 0.0,
            zero_recall: 0,
        };
    }
    let (mut p, mut rc, mut f1, mut zero) = (0.0, 0.0, 0.0, 0usize);
    for r in &sel {
        let (pr, re, fi) = if bigram {
            (
                r.bigram_precision,
                r.bigram_recall,
                f1_of(r.bigram_precision, r.bigram_recall),
            )
        } else {
            (
                r.plain_precision,
                r.plain_recall,
                f1_of(r.plain_precision, r.plain_recall),
            )
        };
        p += pr;
        rc += re;
        f1 += fi;
        if re == 0.0 {
            zero += 1;
        }
    }
    PipelineAgg {
        queries: n,
        mean_precision: p / n as f64,
        mean_recall: rc / n as f64,
        mean_f1: f1 / n as f64,
        zero_recall: zero,
    }
}

fn f1_of(precision: f64, recall: f64) -> f64 {
    if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

/// Scenario 1 runner: index the fixture with both pipelines and measure
/// precision/recall per curated query against substring ground truth.
///
/// # Errors
/// Returns an error if indexing or search fails.
pub fn run_quality(notes: &[Note]) -> Result<QualityReport> {
    let (index, fields) = create_index(None)?;
    let mut writer = index.writer_with_num_threads(2, 64_000_000)?;
    for note in notes {
        writer.add_document(index_doc(&fields, note))?;
    }
    writer.commit()?;
    let reader = index.reader()?;

    let mut rows = Vec::new();
    for q in QUERIES {
        let relevant = ground_truth(notes, q.text);
        // Retrieve enough hits to never truncate the relevant set — these
        // substring-truth queries have precision ≈ 1.0 (a bigram/word match is
        // itself a substring), so matches ≤ relevant; 2× + headroom captures
        // every match so recall is a true measurement, not a limit artifact.
        let limit = relevant.len().saturating_mul(2).max(100) + 100;
        let bigram_hits = search_note_ids(&index, &fields, &reader, q.text, true, limit)?;
        let plain_hits = search_note_ids(&index, &fields, &reader, q.text, false, limit)?;
        let (bp, br, _) = prf(&relevant, &bigram_hits);
        let (pp, pr, _) = prf(&relevant, &plain_hits);
        rows.push(QueryRow {
            query: q.text.to_string(),
            lang: q.lang.to_string(),
            cjk: q.cjk,
            relevant: relevant.len(),
            bigram_hits: bigram_hits.len(),
            plain_hits: plain_hits.len(),
            bigram_precision: bp,
            bigram_recall: br,
            plain_precision: pp,
            plain_recall: pr,
        });
    }

    Ok(QualityReport {
        notes: notes.len(),
        bigram_cjk: aggregate(&rows, true, true),
        bigram_latin: aggregate(&rows, false, true),
        plain_cjk: aggregate(&rows, true, false),
        plain_latin: aggregate(&rows, false, false),
        rows,
    })
}

/// Scenario 2 runner: apply autosave writes over the window, commit per the 5 s
/// deferred policy (simulated clock — no real sleeps), and measure the
/// commit-before-search added latency.
///
/// # Errors
/// Returns an error if indexing, search, or commit fails.
pub fn run_autosave(notes: &[Note]) -> Result<AutosaveReport> {
    let (index, fields) = create_index(None)?;
    let mut writer = index.writer_with_num_threads(2, 64_000_000)?;
    for note in notes {
        writer.add_document(index_doc(&fields, note))?;
    }
    writer.commit()?;
    let reader = index.reader()?;

    let saves = (AUTOSAVE_WINDOW_SECS / AUTOSAVE_INTERVAL_SECS) as u32;
    let mut commit_count = 0u32;
    let mut dirty = false;
    let mut last_commit_sim = 0u64;
    let mut rng = Rng::new(0xA5_70);
    for s in 0..saves {
        let sim_now = (s as u64 + 1) * AUTOSAVE_INTERVAL_SECS;
        let idx = rng.below(notes.len());
        let mut note = notes[idx].clone();
        note.body = format!("{} autosave{s}", note.body);
        writer.delete_term(Term::from_field_u64(fields.note_id, note.note_id));
        writer.add_document(index_doc(&fields, &note))?;
        dirty = true;
        if sim_now - last_commit_sim >= COMMIT_INTERVAL_SECS {
            writer.commit()?;
            commit_count += 1;
            last_commit_sim = sim_now;
            dirty = false;
        }
    }
    if dirty {
        writer.commit()?;
        commit_count += 1;
    }

    // Commit-before-search latency: for a bounded number of rounds, apply a
    // dirty edit and time the commit a query forces, vs a clean baseline query.
    let rounds = 100.min(notes.len());
    let mut commit_us = Vec::with_capacity(rounds);
    let mut baseline_us = Vec::with_capacity(rounds);
    for r in 0..rounds {
        let t0 = Instant::now();
        let _ = search_note_ids(&index, &fields, &reader, "東京", true, 50)?;
        baseline_us.push(u64::try_from(t0.elapsed().as_micros()).unwrap_or(0));

        let idx = rng.below(notes.len());
        let mut note = notes[idx].clone();
        note.body = format!("{} cbs{r}", note.body);
        writer.delete_term(Term::from_field_u64(fields.note_id, note.note_id));
        writer.add_document(index_doc(&fields, &note))?;
        let t1 = Instant::now();
        writer.commit()?; // commit-before-search
        commit_us.push(u64::try_from(t1.elapsed().as_micros()).unwrap_or(0));
        reader.reload()?;
    }

    #[allow(clippy::cast_precision_loss)]
    let commit_ratio = commit_count as f64 / saves as f64;
    Ok(AutosaveReport {
        saves,
        window_secs: AUTOSAVE_WINDOW_SECS,
        commit_count,
        commit_ratio,
        commit_before_search_us: latency_stats(&commit_us),
        baseline_search_us: latency_stats(&baseline_us),
    })
}

/// Scenario 3 runner: commit a base generation, apply an upsert + delete that
/// are committed to the mirror but left uncommitted in tantivy, "crash" (drop
/// the writer), reopen, and replay the delta from the mirror.
///
/// # Errors
/// Returns an error if indexing, the mirror, or replay fails.
pub fn run_replay(dir: &Path, notes: &[Note]) -> Result<ReplayReport> {
    let index_dir = dir.join("replay-idx");
    let mirror_path = dir.join("replay-mirror.db");
    let (index, fields) = create_index(Some(&index_dir))?;
    let mut mirror = Mirror::open(&mirror_path)?;
    let epoch = mirror.epoch();
    let mut writer = index.writer_with_num_threads(2, 64_000_000)?;

    // Base: index every note into both the mirror and tantivy, then commit with
    // a checkpoint at the base generation.
    let mut base_gen = 0u64;
    for note in notes {
        writer.add_document(index_doc(&fields, note))?;
        base_gen = mirror.apply_upsert(note.note_id, &note.body)?;
    }
    commit_with_checkpoint(&mut writer, &index, epoch, base_gen)?;
    mirror.set_tantivy_committed(base_gen)?;

    // Delta: upsert note 0 with a unique marker and delete note 1. Both commit
    // to the mirror (generation advances) and apply to the tantivy writer, but
    // tantivy is NOT committed (the crash boundary).
    let mut upd = notes[0].clone();
    upd.body = format!("{} replaymarker", upd.body);
    mirror.apply_upsert(upd.note_id, &upd.body)?;
    writer.delete_term(Term::from_field_u64(fields.note_id, upd.note_id));
    writer.add_document(index_doc(&fields, &upd))?;

    let deleted = notes[1].clone();
    mirror.apply_delete(deleted.note_id)?;
    writer.delete_term(Term::from_field_u64(fields.note_id, deleted.note_id));

    let mirror_gen = mirror.generation();
    // CRASH: drop the writer without committing. Tantivy loses the uncommitted
    // delta; the mirror (WAL, committed) keeps it.
    drop(writer);

    // Reopen and read the durable checkpoint.
    let (index2, fields2) = reopen_index(&index_dir)?;
    let reader2 = index2.reader()?;
    let (ckpt_epoch, ckpt_gen) = read_checkpoint(&index2)?.unwrap_or((epoch, 0));

    // Replay the delta up to the mirror generation (idempotent, deletion-safe).
    let mut writer2 = index2.writer_with_num_threads(2, 64_000_000)?;
    let t = Instant::now();
    let invalidated = mirror.invalidations_since(ckpt_gen)?;
    for nid in &invalidated {
        writer2.delete_term(Term::from_field_u64(fields2.note_id, *nid));
        if let Some(body) = mirror.body(*nid)? {
            writer2.add_document(index_body_only(&fields2, *nid, &body))?;
        }
    }
    let target_gen = mirror_gen.max(ckpt_gen);
    commit_with_checkpoint(&mut writer2, &index2, ckpt_epoch, target_gen)?;
    let replay_us = u64::try_from(t.elapsed().as_micros()).unwrap_or(0);
    reader2.reload()?;

    // Verify: the upserted marker is now visible; the deleted note is gone; the
    // checkpoint advanced to the mirror generation.
    let upd_hits = search_note_ids(&index2, &fields2, &reader2, "replaymarker", true, 100)?;
    let update_visible = upd_hits.contains(&upd.note_id);
    let del_hits = search_note_ids(
        &index2,
        &fields2,
        &reader2,
        &deleted_uniquetag(&deleted),
        true,
        100,
    )?;
    let deletion_honored = !del_hits.contains(&deleted.note_id);
    let (new_epoch, new_gen) = read_checkpoint(&index2)?.unwrap_or((ckpt_epoch, ckpt_gen));
    let checkpoint_advanced = new_epoch == ckpt_epoch && new_gen == target_gen;

    Ok(ReplayReport {
        checkpoint_generation: ckpt_gen,
        mirror_generation: mirror_gen,
        invalidated: invalidated.len(),
        replay_us,
        update_visible,
        deletion_honored,
        checkpoint_advanced,
    })
}

fn deleted_uniquetag(note: &Note) -> String {
    // The fixture embeds `uniquenote<i>`; note_id == i+1.
    format!("uniquenote{}", note.note_id - 1)
}

/// Scenario 4 runner: prove the checkpoint payload is the durability authority
/// across the §4.14 crash boundaries.
///
/// # Errors
/// Returns an error if indexing, the mirror, or the assertions' setup fails.
pub fn run_publication(dir: &Path, notes: &[Note]) -> Result<PublicationReport> {
    let index_dir = dir.join("pub-idx");
    let mirror_path = dir.join("pub-mirror.db");
    let (index, fields) = create_index(Some(&index_dir))?;
    let mut mirror = Mirror::open(&mirror_path)?;
    let epoch = mirror.epoch();

    // Stage a committed search at generation 5 whose durable checkpoint says 5,
    // but leave the mirror's advisory counter at 4 (the crash happened after the
    // search commit, before the mirror update).
    let mut writer = index.writer_with_num_threads(2, 64_000_000)?;
    for note in notes.iter().take(50) {
        writer.add_document(index_doc(&fields, note))?;
        mirror.apply_upsert(note.note_id, &note.body)?;
    }
    let search_gen = mirror.generation(); // 50 structural changes
    commit_with_checkpoint(&mut writer, &index, epoch, search_gen)?;
    // The mirror update that normally follows the commit is NOT done (crash).
    mirror.set_tantivy_committed(search_gen - 1)?;
    drop(writer);

    // Reopen: the checkpoint payload (authority) must say search_gen even though
    // the SQLite counter is behind.
    let (index2, fields2) = reopen_index(&index_dir)?;
    let reader2 = index2.reader()?;
    let (ckpt_epoch, ckpt_gen) = read_checkpoint(&index2)?.unwrap_or((epoch, 0));
    let mirror_committed = mirror.tantivy_committed();
    let checkpoint_is_authority =
        ckpt_epoch == epoch && ckpt_gen == search_gen && mirror_committed == search_gen - 1;
    // And the committed documents must be findable (durability came from the
    // commit, not the counter).
    let probe_note = &notes[0];
    let hits = search_note_ids(&index2, &fields2, &reader2, &probe_note.title, true, 10)?;
    let crash_after_commit_before_mirror_ok = hits.contains(&probe_note.note_id);

    // Crash AFTER the mirror update: counter catches up; both agree.
    mirror.set_tantivy_committed(search_gen)?;
    let mirror_now = mirror.tantivy_committed();
    let crash_after_mirror_ok = mirror_now == search_gen && ckpt_gen == search_gen;

    // Deletion applied before the search commit is gone after commit+reload.
    let mut writer3 = index2.writer_with_num_threads(2, 64_000_000)?;
    let victim = &notes[1];
    writer3.delete_term(Term::from_field_u64(fields2.note_id, victim.note_id));
    writer3.commit()?;
    reader2.reload()?;
    let victim_hits = search_note_ids(
        &index2,
        &fields2,
        &reader2,
        &deleted_uniquetag(victim),
        true,
        10,
    )?;
    let deletion_before_commit_gone = !victim_hits.contains(&victim.note_id);

    // Reader reload reflects a just-committed document.
    let fresh_id = 9_999_999u64;
    writer3.add_document(index_body_only(&fields2, fresh_id, "readerreload sentinel"))?;
    writer3.commit()?;
    reader2.reload()?;
    let fresh_hits = search_note_ids(&index2, &fields2, &reader2, "readerreload", true, 10)?;
    let reader_reload_ok = fresh_hits.contains(&fresh_id);

    // An ahead checkpoint (search generation ahead of the mirror's structural
    // generation) is detected as requiring a rebuild (§4.14). Stage the anomaly
    // by simulating SQLite restored to an older snapshot while tantivy kept its
    // newer commit, then assert the detector flags checkpoint > structural.
    mirror.force_generation(search_gen.saturating_sub(10))?;
    let mirror_structural = mirror.generation();
    let ahead_checkpoint_detected = ckpt_gen > mirror_structural;

    Ok(PublicationReport {
        checkpoint_is_authority,
        crash_after_commit_before_mirror_ok,
        crash_after_mirror_ok,
        deletion_before_commit_gone,
        reader_reload_ok,
        ahead_checkpoint_detected,
        checkpoint_generation: ckpt_gen,
        mirror_committed,
    })
}

/// Run the selected scenarios against a fresh fixture rooted at `dir`.
///
/// # Errors
/// Returns an error if any selected scenario fails.
pub fn run_all(dir: &Path, notes: &[Note], selection: &ScenarioSelection) -> Result<RunSummary> {
    let mut scenarios = Vec::new();
    if selection.quality {
        let report = run_quality(notes)?;
        scenarios.push(NamedReport {
            name: "1-quality".to_string(),
            report: ScenarioReport::Quality(report),
        });
    }
    if selection.autosave {
        let report = run_autosave(notes)?;
        scenarios.push(NamedReport {
            name: "2-autosave".to_string(),
            report: ScenarioReport::Autosave(report),
        });
    }
    if selection.replay {
        let report = run_replay(&dir.join("replay"), notes)?;
        scenarios.push(NamedReport {
            name: "3-replay".to_string(),
            report: ScenarioReport::Replay(report),
        });
    }
    if selection.publication {
        let report = run_publication(&dir.join("publication"), notes)?;
        scenarios.push(NamedReport {
            name: "4-publication".to_string(),
            report: ScenarioReport::Publication(report),
        });
    }
    Ok(RunSummary {
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        notes: notes.len(),
        scenarios,
    })
}

// ---------------------------------------------------------------------------
// Self-checks (the properties the production wrappers rely on)
// ---------------------------------------------------------------------------

/// Self-check scenario 1: the bigram pipeline materially outperforms the plain
/// pipeline on CJK queries and is usable on its own; both handle Latin.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_quality(report: &QualityReport) -> Result<()> {
    if report.bigram_cjk.mean_recall <= report.plain_cjk.mean_recall {
        anyhow::bail!(
            "bigram CJK recall ({:.3}) did not beat plain ({:.3})",
            report.bigram_cjk.mean_recall,
            report.plain_cjk.mean_recall
        );
    }
    if report.bigram_cjk.mean_recall < 0.5 {
        anyhow::bail!(
            "bigram CJK recall ({:.3}) below the usable floor of 0.5",
            report.bigram_cjk.mean_recall
        );
    }
    if report.bigram_latin.mean_recall < 0.9 {
        anyhow::bail!(
            "bigram Latin control recall ({:.3}) below 0.9",
            report.bigram_latin.mean_recall
        );
    }
    if report.plain_latin.mean_recall < 0.9 {
        anyhow::bail!(
            "plain Latin control recall ({:.3}) below 0.9",
            report.plain_latin.mean_recall
        );
    }
    Ok(())
}

/// Self-check scenario 2: deferred commits keep the commit count far below the
/// save count (the R4 perf-cliff fix), and the commit-before-search latency is
/// recorded.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_autosave(report: &AutosaveReport) -> Result<()> {
    if report.commit_count >= report.saves {
        anyhow::bail!(
            "commit count ({}) was not deferred below save count ({})",
            report.commit_count,
            report.saves
        );
    }
    if report.commit_count as f64 > report.saves as f64 / 4.0 {
        anyhow::bail!(
            "commit count ({}) exceeded a quarter of saves ({}) — deferral ineffective",
            report.commit_count,
            report.saves
        );
    }
    if report.commit_before_search_us.count == 0 {
        anyhow::bail!("commit-before-search latency was not recorded");
    }
    Ok(())
}

/// Self-check scenario 3: replay applied the delta correctly (upsert visible,
/// delete honored) and advanced the checkpoint.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_replay(report: &ReplayReport) -> Result<()> {
    if !report.update_visible {
        anyhow::bail!("replay did not make the applied upsert visible");
    }
    if !report.deletion_honored {
        anyhow::bail!("replay did not honor the applied deletion");
    }
    if !report.checkpoint_advanced {
        anyhow::bail!(
            "checkpoint did not advance to the mirror generation ({} -> {})",
            report.checkpoint_generation,
            report.mirror_generation
        );
    }
    Ok(())
}

/// Self-check scenario 4: the checkpoint payload is the durability authority and
/// every §4.14 publication crash boundary behaves.
///
/// # Errors
/// Returns an error describing any property that failed.
pub fn self_check_publication(report: &PublicationReport) -> Result<()> {
    if !report.checkpoint_is_authority {
        anyhow::bail!(
            "checkpoint payload was not the durability authority over the SQLite counter"
        );
    }
    if !report.crash_after_commit_before_mirror_ok {
        anyhow::bail!("crash after search commit but before mirror update lost the commit");
    }
    if !report.crash_after_mirror_ok {
        anyhow::bail!("crash after mirror update left checkpoint/mirror inconsistent");
    }
    if !report.deletion_before_commit_gone {
        anyhow::bail!("a deletion applied before the search commit survived commit+reload");
    }
    if !report.reader_reload_ok {
        anyhow::bail!("reader reload did not reflect a just-committed document");
    }
    if !report.ahead_checkpoint_detected {
        anyhow::bail!("an ahead checkpoint was not detected as requiring a rebuild");
    }
    Ok(())
}

/// Run every self-check for the selected scenarios; returns failure messages.
#[must_use]
pub fn self_check(summary: &RunSummary) -> Vec<String> {
    let mut failures = Vec::new();
    for named in &summary.scenarios {
        let result = match &named.report {
            ScenarioReport::Quality(r) => self_check_quality(r),
            ScenarioReport::Autosave(r) => self_check_autosave(r),
            ScenarioReport::Replay(r) => self_check_replay(r),
            ScenarioReport::Publication(r) => self_check_publication(r),
        };
        if let Err(e) = result {
            failures.push(format!("{}: {e}", named.name));
        }
    }
    failures
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// A one-line-per-scenario terminal summary.
#[must_use]
pub fn render_summary(summary: &RunSummary) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "s4_search_probe: os={} arch={} notes={}\n",
        summary.os, summary.arch, summary.notes
    ));
    for named in &summary.scenarios {
        match &named.report {
            ScenarioReport::Quality(r) => out.push_str(&format!(
                "scenario 1-quality: bigram CJK recall {:.3} (plain {:.3}); bigram Latin {:.3} (plain {:.3})\n",
                r.bigram_cjk.mean_recall,
                r.plain_cjk.mean_recall,
                r.bigram_latin.mean_recall,
                r.plain_latin.mean_recall
            )),
            ScenarioReport::Autosave(r) => out.push_str(&format!(
                "scenario 2-autosave: {} saves -> {} commits (ratio {:.3}); commit-before-search p95={}us\n",
                r.saves, r.commit_count, r.commit_ratio, r.commit_before_search_us.p95
            )),
            ScenarioReport::Replay(r) => out.push_str(&format!(
                "scenario 3-replay: checkpoint {} -> mirror {} ({} invalidated, {}us); update={} deletion={} advanced={}\n",
                r.checkpoint_generation, r.mirror_generation, r.invalidated, r.replay_us,
                r.update_visible, r.deletion_honored, r.checkpoint_advanced
            )),
            ScenarioReport::Publication(r) => out.push_str(&format!(
                "scenario 4-publication: authority={} after-commit={} after-mirror={} deletion-gone={} reload={} ahead={}\n",
                r.checkpoint_is_authority, r.crash_after_commit_before_mirror_ok,
                r.crash_after_mirror_ok, r.deletion_before_commit_gone, r.reader_reload_ok,
                r.ahead_checkpoint_detected
            )),
        }
    }
    out
}

impl RunSummary {
    /// Render the full markdown report (the spike doc's measured tables).
    #[must_use]
    pub fn render_markdown(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "\n## Measured — {} ({})\n\n| scenario | result |\n| --- | --- |\n",
            self.os, self.arch
        ));
        for named in &self.scenarios {
            let cell = match &named.report {
                ScenarioReport::Quality(r) => format!(
                    "bigram CJK recall {:.3} (plain {:.3}); bigram Latin {:.3} (plain {:.3}); {} notes",
                    r.bigram_cjk.mean_recall,
                    r.plain_cjk.mean_recall,
                    r.bigram_latin.mean_recall,
                    r.plain_latin.mean_recall,
                    r.notes
                ),
                ScenarioReport::Autosave(r) => format!(
                    "{} saves → {} commits (ratio {:.3}); commit-before-search p95 {} µs; baseline p95 {} µs",
                    r.saves,
                    r.commit_count,
                    r.commit_ratio,
                    r.commit_before_search_us.p95,
                    r.baseline_search_us.p95
                ),
                ScenarioReport::Replay(r) => format!(
                    "checkpoint {} → mirror {} ({} invalidated, {} µs); update {} / deletion {} / advanced {}",
                    r.checkpoint_generation,
                    r.mirror_generation,
                    r.invalidated,
                    r.replay_us,
                    r.update_visible,
                    r.deletion_honored,
                    r.checkpoint_advanced
                ),
                ScenarioReport::Publication(r) => format!(
                    "authority {}; after-commit {}; after-mirror {}; deletion-gone {}; reload {}; ahead {}",
                    r.checkpoint_is_authority,
                    r.crash_after_commit_before_mirror_ok,
                    r.crash_after_mirror_ok,
                    r.deletion_before_commit_gone,
                    r.reader_reload_ok,
                    r.ahead_checkpoint_detected
                ),
            };
            out.push_str(&format!("| {} | {} |\n", named.name, cell));
        }
        // Per-query quality rows for scenario 1, if present.
        for named in &self.scenarios {
            if let ScenarioReport::Quality(q) = &named.report {
                out.push_str(
                    "\n### Scenario 1 — per-query precision/recall (bigram vs plain)\n\n| query | lang | relevant | bigram P/R | plain P/R |\n| --- | --- | --- | --- | --- |\n",
                );
                for row in &q.rows {
                    out.push_str(&format!(
                        "| {} | {} | {} | {:.2}/{:.2} | {:.2}/{:.2} |\n",
                        row.query,
                        row.lang,
                        row.relevant,
                        row.bigram_precision,
                        row.bigram_recall,
                        row.plain_precision,
                        row.plain_recall
                    ));
                }
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_detection_ranges() {
        assert!(is_cjk_script('東')); // Han
        assert!(is_cjk_script('あ')); // Hiragana
        assert!(is_cjk_script('ア')); // Katakana
        assert!(is_cjk_script('한')); // Hangul
        assert!(!is_cjk_script('a'));
        assert!(!is_cjk_script('。')); // CJK punctuation → separator
        assert!(!is_cjk_script(' '));
    }

    #[test]
    fn bigram_tokenizes_cjk_run() {
        let tokens = tokenize_cjk_bigram("東京都");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["東京", "京都"]);
    }

    #[test]
    fn bigram_single_char_run_is_unigram() {
        let tokens = tokenize_cjk_bigram("京");
        assert_eq!(tokens.len(), 1);
        assert_eq!(tokens[0].text, "京");
    }

    #[test]
    fn bigram_mixed_cjk_and_latin() {
        let tokens = tokenize_cjk_bigram("北京 AI");
        let texts: Vec<&str> = tokens.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(texts, ["北京", "AI"]);
    }

    #[test]
    fn bigram_token_stream_walks_tokens() {
        let mut tk = CjkBigramTokenizer;
        let mut stream = tk.token_stream("東京タワー");
        let mut texts = Vec::new();
        while let Some(tok) = stream.next() {
            texts.push(tok.text.clone());
        }
        // 東京タワー → 東京,京タ,タワ,ワー (all bigrams of the run).
        assert_eq!(texts, ["東京", "京タ", "タワ", "ワー"]);
    }

    #[test]
    fn plain_tokenizer_keeps_cjk_run_whole() {
        let mut analyzer = TextAnalyzer::from(SimpleTokenizer::default());
        let mut stream = analyzer.token_stream("東京都");
        let tok = stream.next().expect("one token");
        // SimpleTokenizer treats an unbroken CJK run as a single token — the
        // reason the plain pipeline under-recalls on CJK substrings.
        assert_eq!(tok.text, "東京都");
    }

    #[test]
    fn fixture_is_deterministic_and_cjk_heavy() {
        let a = build_fixture(200);
        let b = build_fixture(200);
        assert_eq!(a.len(), 200);
        assert_eq!(a[0].body, b[0].body);
        assert_eq!(a[199].title, b[199].title);
        // Unique per-note sentinel present.
        assert!(a[42].body.contains("uniquenote42"));
        // At least one note has CJK in the title.
        assert!(a.iter().any(|n| n.title.chars().any(is_cjk_script)));
    }

    #[test]
    fn checkpoint_payload_roundtrip() {
        let payload = checkpoint_payload(7, 42);
        assert_eq!(parse_checkpoint(&payload), Some((7, 42)));
        assert_eq!(parse_checkpoint("garbage"), None);
        assert_eq!(parse_checkpoint("7:xx"), None);
    }

    #[test]
    fn parse_selection_all_and_subsets() {
        let all = parse_selection("all").expect("all parses");
        assert!(all.quality && all.autosave && all.replay && all.publication);
        let subset = parse_selection("1,3").expect("subset parses");
        assert!(subset.quality);
        assert!(!subset.autosave);
        assert!(subset.replay);
        assert!(!subset.publication);
        assert!(parse_selection("5").is_err());
        assert!(parse_selection("").is_err());
    }

    #[test]
    fn prf_math() {
        let relevant = vec![1, 2, 3, 4];
        let hits = vec![1, 2, 5];
        let (p, r, f) = prf(&relevant, &hits);
        assert!((p - 2.0 / 3.0).abs() < 1e-9);
        assert!((r - 0.5).abs() < 1e-9);
        assert!(f > 0.0 && f < 1.0);
    }

    #[test]
    fn live_quality_bigram_beats_plain_on_cjk() {
        let notes = build_fixture(500);
        let report = run_quality(&notes).expect("quality runs");
        // The property the tokenizer design rests on: bigram materially beats
        // plain on CJK, and both handle Latin.
        assert!(
            report.bigram_cjk.mean_recall > report.plain_cjk.mean_recall,
            "bigram CJK recall {:.3} must beat plain {:.3}",
            report.bigram_cjk.mean_recall,
            report.plain_cjk.mean_recall
        );
        assert!(report.bigram_latin.mean_recall >= 0.9);
        assert!(report.plain_latin.mean_recall >= 0.9);
    }

    #[test]
    fn live_autosave_defers_commits() {
        let notes = build_fixture(300);
        let report = run_autosave(&notes).expect("autosave runs");
        // 600 saves over 600 s with a 5 s commit timer → ~120 commits, far
        // below one-per-save.
        assert!(report.commit_count < report.saves);
        assert!(report.commit_count as f64 <= report.saves as f64 / 4.0);
        assert!(report.commit_before_search_us.count > 0);
    }

    #[test]
    fn live_replay_applies_delta() {
        let notes = build_fixture(200);
        let vault = crate::spike_s3::TempVault::new("s4-replay-test");
        let report = run_replay(vault.path(), &notes).expect("replay runs");
        assert!(report.update_visible, "upsert not visible after replay");
        assert!(report.deletion_honored, "deletion not honored after replay");
        assert!(report.checkpoint_advanced, "checkpoint did not advance");
    }

    #[test]
    fn live_publication_proves_checkpoint_authority() {
        let notes = build_fixture(200);
        let vault = crate::spike_s3::TempVault::new("s4-pub-test");
        let report = run_publication(vault.path(), &notes).expect("publication runs");
        assert!(report.checkpoint_is_authority);
        assert!(report.crash_after_commit_before_mirror_ok);
        assert!(report.crash_after_mirror_ok);
        assert!(report.deletion_before_commit_gone);
        assert!(report.reader_reload_ok);
        assert!(report.ahead_checkpoint_detected);
    }

    #[test]
    fn run_all_selected_self_checks_pass() {
        let notes = build_fixture(400);
        let vault = crate::spike_s3::TempVault::new("s4-all-test");
        let selection = ScenarioSelection {
            quality: true,
            autosave: true,
            replay: true,
            publication: true,
        };
        let summary = run_all(vault.path(), &notes, &selection).expect("run_all runs");
        let failures = self_check(&summary);
        assert!(failures.is_empty(), "self-check failures: {failures:?}");
        assert_eq!(summary.scenarios.len(), 4);
    }

    #[test]
    fn mirror_generation_and_invalidations() {
        let vault = crate::spike_s3::TempVault::new("s4-mirror-test");
        let mut mirror = Mirror::open(&vault.path().join("m.db")).expect("mirror opens");
        let g1 = mirror.apply_upsert(1, "alpha").expect("upsert");
        let g2 = mirror.apply_upsert(2, "beta").expect("upsert");
        let g3 = mirror.apply_delete(1).expect("delete");
        assert_eq!(mirror.generation(), g3);
        assert!(g1 < g2 && g2 < g3);
        let since0 = mirror.invalidations_since(0).expect("invalidations");
        assert_eq!(since0, vec![1, 2]);
        let since_g2 = mirror.invalidations_since(g2).expect("invalidations");
        assert_eq!(since_g2, vec![1]);
        assert_eq!(mirror.body(2).expect("body").as_deref(), Some("beta"));
        assert!(mirror.body(1).expect("body").is_none());
    }
}
