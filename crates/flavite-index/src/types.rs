//! Core types for the Lattice index (§4.3).
//!
//! This module contains the type definitions for notes, attachments, links, tags,
//! and other index entities.

use crate::schema::{AttachmentKind, ResState, ValueType};
use serde::{Deserialize, Serialize};
use std::fmt;

/// A reference to a note in the index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct NoteRef {
    pub note_id: i64,
    pub path_native: Vec<u8>,
}

impl NoteRef {
    pub fn new(note_id: i64, path_native: Vec<u8>) -> Self {
        Self {
            note_id,
            path_native,
        }
    }
}

impl fmt::Display for NoteRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "NoteRef({}, {})",
            self.note_id,
            String::from_utf8_lossy(&self.path_native)
        )
    }
}

/// A reference to an attachment in the index.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AttRef {
    pub att_id: i64,
    pub path_native: Vec<u8>,
}

impl AttRef {
    pub fn new(att_id: i64, path_native: Vec<u8>) -> Self {
        Self {
            att_id,
            path_native,
        }
    }
}

/// A note entity in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub note_id: i64,
    pub path: String,
    pub path_native: Vec<u8>,
    pub path_key: String,
    pub stem_key: String,
    pub title: String,
    pub stable_id: Option<String>,
    pub id_conflict: bool,
    pub content_hash: Vec<u8>,
    pub size: i64,
    pub mtime_ns: i64,
    pub scan_ns: i64,
    pub frontmatter: Option<String>,
    pub preview: Option<String>,
    pub parse_ok: bool,
    pub generation: i64,
}

impl Note {
    pub fn note_ref(&self) -> NoteRef {
        NoteRef::new(self.note_id, self.path_native.clone())
    }
}

/// An attachment entity in the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub att_id: i64,
    pub path: String,
    pub path_native: Vec<u8>,
    pub path_key: String,
    pub stem_key: String,
    pub kind: AttachmentKind,
    pub mime: Option<String>,
    pub size: i64,
    pub content_hash: Option<Vec<u8>>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub mtime_ns: i64,
    pub scan_ns: i64,
    pub hydrated: bool,
    pub generation: i64,
}

impl Attachment {
    pub fn att_ref(&self) -> AttRef {
        AttRef::new(self.att_id, self.path_native.clone())
    }
}

/// A link from one note to another note or attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub src: i64,
    pub ord: i32,
    pub kind: LinkKind,
    pub dst_raw: String,
    pub target_key: String,
    pub anchor: Option<String>,
    pub alias: Option<String>,
    pub by_id: bool,
    pub span_start: i32,
    pub span_end: i32,
    pub dst_note: Option<i64>,
    pub dst_att: Option<i64>,
    pub res_state: ResState,
}

/// Kind of link: regular link or embed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum LinkKind {
    Link,
    Embed,
}

impl LinkKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Link => "link",
            Self::Embed => "embed",
        }
    }
}

/// A tag on a note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tag {
    pub note_id: i64,
    pub tag: String,
    pub source: TagSource,
}

/// Source of a tag (inline in text or from frontmatter).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TagSource {
    Inline,
    Frontmatter,
}

impl TagSource {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Frontmatter => "frontmatter",
        }
    }
}

/// A property on a note.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub note_id: i64,
    pub key: String,
    pub ord: i32,
    pub vtype: ValueType,
    pub v_text: Option<String>,
    pub v_num: Option<f64>,
}

/// An extension fact (for first-party extensions or future plugins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtFact {
    pub note_id: i64,
    pub ns: String,
    pub key: String,
    pub value_json: String,
}

/// Cached searchable document body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchDoc {
    pub note_id: i64,
    pub content_hash: Vec<u8>,
    pub body_text: String,
}

/// A change record in the invalidation log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeRecord {
    pub generation: i64,
    pub kind: ChangeKind,
    pub entity_id: i64,
}

/// Kind of change: note, attachment, or outgoing (links from a note).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    Note,
    Attachment,
    Outgoing,
}

impl ChangeKind {
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Attachment => "attachment",
            Self::Outgoing => "outgoing",
        }
    }
}

/// An epoch and generation pair - the index version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct IndexVersion {
    pub epoch: u64,
    pub generation: u64,
}

impl IndexVersion {
    pub const fn new(epoch: u64, generation: u64) -> Self {
        Self { epoch, generation }
    }
}

/// A cursor for pagination.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub epoch: u64,
    pub generation: u64,
    pub note_id: Option<i64>,
    pub ord: Option<i32>,
}

/// A page of results.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub epoch: u64,
    pub generation: u64,
    pub next: Option<Cursor>,
}

impl<T> Page<T> {
    pub fn new(items: Vec<T>, epoch: u64, generation: u64, next: Option<Cursor>) -> Self {
        Self {
            items,
            epoch,
            generation,
            next,
        }
    }
}

/// Error type for index operations.
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Tantivy error: {0}")]
    Tantivy(String),

    #[error("Note not found: {0}")]
    NoteNotFound(i64),

    #[error("Attachment not found: {0}")]
    AttachmentNotFound(i64),

    #[error("Invalid query: {0}")]
    InvalidQuery(String),

    #[error("Generation mismatch: expected {0}, got {1}")]
    GenerationMismatch(u64, u64),

    #[error("Epoch mismatch: expected {0}, got {1}")]
    EpochMismatch(u64, u64),

    #[error("Stale cursor: cursor is ahead of current generation")]
    StaleCursor,

    #[error("Reset required: epoch changed or cursor too old")]
    ResetRequired,
}

/// Query comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CmpOp {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    Contains,
    StartsWith,
    EndsWith,
}

/// Property value for queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PropertyValue {
    Text(String),
    Number(f64),
    Bool(bool),
    Date(String),
    Datetime(String),
    Null,
}

/// Maximum note bytes beyond which file is listed but not parsed or indexed.
pub const MAX_NOTE_BYTES: usize = 32 * 1024 * 1024; // 32 MB

/// Maximum attachment size to hash (larger files are listed but not hashed).
pub const ATTACHMENT_HASH_MAX: usize = 64 * 1024 * 1024; // 64 MB

/// Default invalidation log storage budget (64 MiB).
pub const DEFAULT_LOG_BUDGET: usize = 64 * 1024 * 1024;
