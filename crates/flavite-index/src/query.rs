//! Typed query AST and executor (§4.3, D13).
//!
//! This module provides the typed query API that is the only way to query the
//! index. Plugins MUST NOT receive SQL access to prevent injection, locking, and
//! resource exhaustion issues.

use crate::schema::{MetaKey, get_meta_u64};
use crate::types::{CmpOp, Cursor, IndexError, NoteRef, Page, PropertyValue};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Maximum query AST depth (NFR-8). Deeper trees are rejected before any
/// allocation or effect.
pub const MAX_QUERY_DEPTH: usize = 32;
/// Maximum query AST node count (NFR-8).
pub const MAX_QUERY_NODES: usize = 256;
/// Maximum distinct text leaves per query (NFR-8); the evaluator assigns one
/// 64-bit contributing-mask bit per distinct positive leaf.
pub const MAX_QUERY_TEXT_LEAVES: usize = 64;
/// Maximum byte length of one text term/pattern (NFR-8).
pub const MAX_PATTERN_BYTES: usize = 8 * 1024;

/// Indexed text fields addressable by a text leaf (§6.9 explicit field
/// filters). `All` is the default scope: title, aliases, tags and body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextField {
    Title,
    Aliases,
    Tags,
    Body,
}

/// A typed full-text leaf (§6.9): explicit field scope plus term or phrase
/// input. There is no hidden field-query grammar inside the term: it is
/// token/phrase input for the pinned analyzer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TextLeaf {
    /// Fields this leaf searches. Empty means all indexed text fields.
    pub fields: Vec<TextField>,
    /// When true the term must match as one phrase (position-preserving);
    /// when false it is a term/conjunction input for the analyzer.
    pub phrase: bool,
    /// The raw term text (≤ [`MAX_PATTERN_BYTES`] UTF-8 bytes).
    pub term: String,
}

impl TextLeaf {
    #[must_use]
    pub fn all_fields(term: impl Into<String>) -> Self {
        Self {
            fields: Vec::new(),
            phrase: false,
            term: term.into(),
        }
    }

    #[must_use]
    pub fn phrase(term: impl Into<String>) -> Self {
        Self {
            fields: Vec::new(),
            phrase: true,
            term: term.into(),
        }
    }

    /// Stable evaluation digest input: identical leaves share one searcher
    /// evaluation and one contributing-mask bit (§6.3).
    #[must_use]
    pub fn digest_key(&self) -> (Vec<TextField>, bool, &str) {
        (self.fields.clone(), self.phrase, self.term.as_str())
    }
}

/// Query input mode (D-07). Plain full-text mode treats whitespace as AND
/// and punctuation as text (supported field filters excepted); expression
/// mode requires explicit operators and supports quotes/parentheses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum QueryMode {
    /// Whitespace-separated terms AND together; quotes mark phrases.
    Plain,
    /// Explicit `AND`/`OR`/`NOT` with parentheses and quotes.
    Expression,
}

/// Typed parse failure (§6.1 deterministic machine-readable errors).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueryParseError {
    Empty,
    UnterminatedQuote,
    /// A `key:value` form the expression grammar does not implement.
    UnsupportedSyntax {
        token: String,
    },
    /// Nested engine field syntax (`text:title:rocket`) is rejected with a
    /// concrete repair (§6.9): use the explicit field filter.
    UnsupportedLegacyTextSyntax {
        term: String,
        repair: String,
    },
    TooLong {
        observed: usize,
        limit: usize,
    },
    TooComplex {
        reason: &'static str,
    },
    MissingClosingParen,
    UnexpectedToken {
        token: String,
    },
}

impl fmt::Display for QueryParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => write!(f, "query is empty"),
            Self::UnterminatedQuote => write!(f, "unterminated quoted query value"),
            Self::UnsupportedSyntax { token } => {
                write!(f, "unsupported query syntax: {token}")
            }
            Self::UnsupportedLegacyTextSyntax { term, repair } => write!(
                f,
                "unsupported legacy text syntax: {term}; repair: {repair}"
            ),
            Self::TooLong { observed, limit } => {
                write!(f, "query term too long: {observed} bytes exceeds {limit}")
            }
            Self::TooComplex { reason } => write!(f, "query too complex: {reason}"),
            Self::MissingClosingParen => write!(f, "missing closing parenthesis"),
            Self::UnexpectedToken { token } => {
                write!(f, "unexpected token {token:?}")
            }
        }
    }
}

/// The field-filter prefix set interpreted by both modes (§6.9). Arbitrary
/// `key:value` forms are never field filters.
const FIELD_PREFIXES: [(&str, TextField); 4] = [
    ("title:", TextField::Title),
    ("aliases:", TextField::Aliases),
    ("tags:", TextField::Tags),
    ("body:", TextField::Body),
];

/// Parse one user-facing term into a text leaf or structural predicate.
/// Shared by both modes; `expression` controls whether an unknown
/// `key:value` form is a typed error (expression) or literal text (plain).
fn parse_term_token(raw: &str, expression: bool) -> Result<Query, QueryParseError> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(QueryParseError::Empty);
    }
    if token.len() > MAX_PATTERN_BYTES {
        return Err(QueryParseError::TooLong {
            observed: token.len(),
            limit: MAX_PATTERN_BYTES,
        });
    }
    if let Some(term) = token.strip_prefix("text:") {
        if term.is_empty() {
            return Err(QueryParseError::Empty);
        }
        if let Some((prefix, _)) = FIELD_PREFIXES
            .iter()
            .find(|(prefix, _)| term.starts_with(*prefix))
        {
            let field = &prefix[..prefix.len() - 1];
            return Err(QueryParseError::UnsupportedLegacyTextSyntax {
                term: term.to_owned(),
                repair: format!(
                    "use the explicit field filter (--field {field} or the search UI Title filter) instead of nested text field syntax"
                ),
            });
        }
        if term.contains(':') && expression {
            return Err(QueryParseError::UnsupportedSyntax {
                token: token.to_owned(),
            });
        }
        return Ok(Query::Text(strip_quoted_phrase(term)?));
    }
    for (prefix, field) in FIELD_PREFIXES {
        if let Some(value) = token.strip_prefix(prefix).filter(|v| !v.is_empty()) {
            return Ok(Query::Text(field_leaf(field, strip_quoted_phrase(value)?)));
        }
    }
    if let Some(value) = token.strip_prefix("all:").filter(|v| !v.is_empty()) {
        return Ok(Query::Text(strip_quoted_phrase(value)?));
    }
    if let Some(value) = token.strip_prefix("tag:").filter(|v| !v.is_empty()) {
        return Ok(Query::Tag {
            name: flavite_syntax::wellknown::normalize_tag(value),
            hierarchical: false,
        });
    }
    if token.eq_ignore_ascii_case("unresolved") {
        return Ok(Query::Unresolved);
    }
    if token.contains(':') {
        if expression {
            return Err(QueryParseError::UnsupportedSyntax {
                token: token.to_owned(),
            });
        }
        // Plain text mode treats punctuation as text (§6.9): an arbitrary
        // `key:value` form is literal token/phrase input, never a field.
        return Ok(Query::Text(TextLeaf::all_fields(token)));
    }
    Ok(Query::Text(TextLeaf::all_fields(token)))
}

/// One field-filtered text leaf.
fn field_leaf(field: TextField, leaf: TextLeaf) -> TextLeaf {
    let mut leaf = leaf;
    if leaf.fields.is_empty() {
        leaf.fields = vec![field];
    }
    leaf
}

/// A token wrapped in matching quotes becomes a phrase leaf; the quotes are
/// stripped. Everything else stays a term input.
fn strip_quoted_phrase(term: &str) -> Result<TextLeaf, QueryParseError> {
    let bytes = term.as_bytes();
    if bytes.len() >= 2
        && (bytes[0] == b'"' || bytes[0] == b'\'')
        && bytes[0] == bytes[term.len() - 1]
    {
        let inner = &term[1..term.len() - 1];
        if inner.contains('"') || inner.contains('\'') {
            return Err(QueryParseError::UnterminatedQuote);
        }
        return Ok(TextLeaf::phrase(inner));
    }
    Ok(TextLeaf::all_fields(term))
}

/// The typed query AST (§4.3).
///
/// This is the only way to query the index. Internal query API uses this
/// typed AST, not SQL strings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Query {
    /// Query by tag name (hierarchical if true: `a/b` matches `a/b` and `a/b/c`).
    Tag { name: String, hierarchical: bool },
    /// Query by property: `key OP value`.
    Prop {
        key: String,
        op: CmpOp,
        value: PropertyValue,
    },
    /// Notes that link to the given note.
    LinksTo(NoteRef),
    /// Notes that are linked from the given note.
    LinkedFrom(NoteRef),
    /// Notes with unresolved links.
    Unresolved,
    /// Full-text search over the leaf's field scope (default: all indexed
    /// text fields). Evaluated by the captured Tantivy searcher, never by
    /// SQL (§6.3); the structural endpoint rejects this variant explicitly.
    Text(TextLeaf),
    /// Query attachments by kind and/or name.
    Attachment {
        kind: Option<crate::schema::AttachmentKind>,
        name: Option<String>,
    },
    /// AND of multiple queries.
    And(Vec<Query>),
    /// OR of multiple queries.
    Or(Vec<Query>),
    /// NOT of a query.
    Not(Box<Query>),
}

impl Query {
    /// Parse the expression-mode query syntax into the typed query AST.
    /// Supports `tag:`, `text:`, explicit field filters (`title:`,
    /// `aliases:`, `tags:`, `body:`, `all:`), `unresolved`, quoted phrases,
    /// parentheses, and boolean `AND`/`OR`/`NOT` operators (`AND` binds
    /// tighter than `OR`). Unsupported `key:value` expressions are rejected
    /// with a typed error instead of being treated as a literal search.
    ///
    /// # Errors
    /// Returns a displayable error for malformed or unsupported syntax.
    pub fn parse(input: &str) -> Result<Self, String> {
        Self::parse_typed(input, QueryMode::Expression).map_err(|error| error.to_string())
    }

    /// Typed variant of [`Query::parse`] (§6.1 deterministic errors).
    ///
    /// # Errors
    /// Returns [`QueryParseError`] for malformed or unsupported syntax.
    pub fn parse_typed(input: &str, mode: QueryMode) -> Result<Self, QueryParseError> {
        match mode {
            QueryMode::Expression => QueryParser::new(input)?.parse(),
            QueryMode::Plain => parse_plain(input),
        }
    }

    /// Whether this AST contains any text predicate. The structural SQL
    /// endpoint rejects text explicitly (§6.1: `UnsupportedTextPredicate`,
    /// never an empty success).
    #[must_use]
    pub fn contains_text(&self) -> bool {
        match self {
            Self::Text(_) => true,
            Self::Tag { .. }
            | Self::Prop { .. }
            | Self::LinksTo(_)
            | Self::LinkedFrom(_)
            | Self::Unresolved
            | Self::Attachment { .. } => false,
            Self::And(parts) | Self::Or(parts) => parts.iter().any(Self::contains_text),
            Self::Not(inner) => inner.contains_text(),
        }
    }

    /// Execute this query against the SQLite index.
    ///
    /// Returns a page of note IDs matching the query, along with the index version.
    ///
    /// # Errors
    ///
    /// Returns [`IndexError::ResetRequired`] when the cursor predates the
    /// invalidation log floor, [`IndexError::StaleCursor`] when the cursor is
    /// ahead of the current generation, and [`IndexError::Sqlite`] when the
    /// underlying query fails.
    pub fn execute(
        &self,
        conn: &Connection,
        page_size: usize,
        cursor: Option<&Cursor>,
    ) -> Result<Page<NoteRef>, IndexError> {
        let epoch = get_meta_u64(conn, MetaKey::Epoch)?.unwrap_or(1);
        let generation = get_meta_u64(conn, MetaKey::Generation)?.unwrap_or(0);

        // Check cursor validity
        if let Some(cursor) = &cursor {
            if cursor.epoch != epoch {
                return Ok(Page::new(Vec::new(), epoch, generation, None));
            }

            let log_floor: u64 = get_meta_u64(conn, MetaKey::LogFloor)?.unwrap_or(0);
            if cursor.generation < log_floor {
                return Err(IndexError::ResetRequired);
            }

            if cursor.generation > generation {
                return Err(IndexError::StaleCursor);
            }
        }

        // Build the query with all parameters using params! macro directly
        let (where_clause, base_params) = self.to_sql()?;

        // Build the full query with cursor support
        let mut final_sql = format!("SELECT note_id, path_native FROM notes WHERE {where_clause}");
        let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = base_params;

        if let Some(cursor) = &cursor {
            final_sql.push_str(" AND (note_id > ? OR (note_id = ? AND generation > ?))");
            all_params.push(Box::new(cursor.note_id.unwrap_or(0)));
            all_params.push(Box::new(cursor.note_id.unwrap_or(0)));
            all_params.push(Box::new(saturating_to_i64(cursor.generation)));
        }

        final_sql.push_str(" ORDER BY note_id ASC, generation ASC LIMIT ?");
        all_params.push(Box::new(saturating_to_i64(page_size)));

        // Create a parameter list for the query
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            all_params.iter().map(std::convert::AsRef::as_ref).collect();

        let mut stmt = conn.prepare(&final_sql)?;
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            Ok(NoteRef::new(
                row.get::<_, i64>(0)?,
                row.get::<_, Vec<u8>>(1)?,
            ))
        })?;

        let mut items = Vec::new();
        for row in rows {
            items.push(row?);
        }

        // Check if there are more results
        let next = if items.len() >= page_size {
            let last = &items[items.len() - 1];
            Some(Cursor {
                epoch,
                generation,
                note_id: Some(last.note_id),
                ord: None,
            })
        } else {
            None
        };

        Ok(Page::new(items, epoch, generation, next))
    }

    /// Convert a text-free query to SQL fragments. Text predicates are
    /// rejected explicitly: the structural endpoint never translates them
    /// to SQL and never returns a silent empty success (§6.1, §6.3).
    ///
    /// Returns (`WHERE_clause`, params) where `WHERE_clause` is the condition without the WHERE keyword
    ///
    /// # Errors
    /// Returns [`IndexError::UnsupportedTextPredicate`] when the AST
    /// contains a text predicate.
    pub(crate) fn to_sql(&self) -> Result<(String, Vec<Box<dyn rusqlite::ToSql>>), IndexError> {
        match self {
            Self::Tag { name, hierarchical } => {
                if *hierarchical {
                    // For hierarchical: `a/b` matches `a/b` and `a/b/c`
                    // Use range scan: [ 'a/', 'a0' )
                    let prefix = if name.ends_with('/') {
                        name.clone()
                    } else {
                        format!("{name}/")
                    };
                    let next_prefix = next_hierarchical_key(name);
                    Ok((
                        "note_id IN (SELECT note_id FROM tags WHERE tag >= ? AND tag < ?)"
                            .to_string(),
                        vec![
                            Box::new(prefix) as Box<dyn rusqlite::ToSql>,
                            Box::new(next_prefix) as Box<dyn rusqlite::ToSql>,
                        ],
                    ))
                } else {
                    Ok((
                        "note_id IN (SELECT note_id FROM tags WHERE tag = ?)".to_string(),
                        vec![Box::new(name.clone()) as Box<dyn rusqlite::ToSql>],
                    ))
                }
            }
            Self::Prop { key, op, value } => Ok(prop_to_sql(key, *op, value)),
            // Notes that link TO the given note: the link's source row.
            Self::LinksTo(note_ref) => Ok((
                "note_id IN (SELECT src FROM links WHERE dst_note = ?)".to_string(),
                vec![Box::new(note_ref.note_id) as Box<dyn rusqlite::ToSql>],
            )),
            // Notes linked FROM the given note: the link's destination row.
            Self::LinkedFrom(note_ref) => Ok((
                "note_id IN (SELECT dst_note FROM links WHERE src = ?)".to_string(),
                vec![Box::new(note_ref.note_id) as Box<dyn rusqlite::ToSql>],
            )),
            Self::Unresolved => Ok((
                "note_id IN (SELECT src FROM links WHERE res_state <> 'resolved')".to_string(),
                Vec::new(),
            )),
            Self::Text(_) => Err(IndexError::UnsupportedTextPredicate),
            Self::Attachment { kind, name } => {
                let mut conditions = Vec::new();
                let mut params = Vec::new();

                if let Some(k) = kind {
                    conditions.push("kind = ?".to_string());
                    params.push(Box::new(k.as_str().to_string()) as Box<dyn rusqlite::ToSql>);
                }

                if let Some(n) = name {
                    conditions.push("stem_key = ?".to_string());
                    params.push(Box::new(n.clone()) as Box<dyn rusqlite::ToSql>);
                }

                let where_clause = if conditions.is_empty() {
                    "1 = 1".to_string()
                } else {
                    conditions.join(" AND ")
                };

                Ok((
                    format!(
                        "note_id IN (SELECT note_id FROM links WHERE dst_att IN (SELECT att_id FROM attachments WHERE {where_clause}))"
                    ),
                    params,
                ))
            }
            Self::And(queries) => {
                let mut conditions = Vec::new();
                let mut params = Vec::new();

                for query in queries {
                    let (sql, p) = query.to_sql()?;
                    conditions.push(format!("({sql})"));
                    params.extend(p);
                }

                Ok((conditions.join(" AND "), params))
            }
            Self::Or(queries) => {
                let mut conditions = Vec::new();
                let mut params = Vec::new();

                for query in queries {
                    let (sql, p) = query.to_sql()?;
                    conditions.push(format!("({sql})"));
                    params.extend(p);
                }

                Ok((conditions.join(" OR "), params))
            }
            Self::Not(query) => {
                let (sql, params) = query.to_sql()?;
                Ok((format!("NOT ({sql})"), params))
            }
        }
    }
}

struct QueryParser {
    tokens: Vec<String>,
    cursor: usize,
}

impl QueryParser {
    fn new(input: &str) -> Result<Self, QueryParseError> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut quote = None;
        for ch in input.chars() {
            match (quote, ch) {
                (Some(q), c) if q == c => {
                    quote = None;
                }
                (None, '\'' | '"') => quote = Some(ch),
                (None, '(' | ')') => {
                    if !current.trim().is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                    tokens.push(ch.to_string());
                }
                (None, c) if c.is_whitespace() => {
                    if !current.trim().is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                (Some(_) | None, c) => current.push(c),
            }
        }
        if quote.is_some() {
            return Err(QueryParseError::UnterminatedQuote);
        }
        if !current.trim().is_empty() {
            tokens.push(current);
        }
        if tokens.is_empty() {
            return Err(QueryParseError::Empty);
        }
        Ok(Self { tokens, cursor: 0 })
    }

    fn parse(mut self) -> Result<Query, QueryParseError> {
        let query = self.parse_or()?;
        if self.cursor != self.tokens.len() {
            return Err(QueryParseError::UnexpectedToken {
                token: self.tokens[self.cursor].clone(),
            });
        }
        check_query_limits(&query)?;
        Ok(query)
    }

    fn parse_or(&mut self) -> Result<Query, QueryParseError> {
        let mut parts = vec![self.parse_and()?];
        while self.take_word("OR") {
            parts.push(self.parse_and()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Query::Or(parts)
        })
    }

    fn parse_and(&mut self) -> Result<Query, QueryParseError> {
        let mut parts = vec![self.parse_unary()?];
        while self.take_word("AND") {
            parts.push(self.parse_unary()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Query::And(parts)
        })
    }

    fn parse_unary(&mut self) -> Result<Query, QueryParseError> {
        if self.take_word("NOT") {
            return Ok(Query::Not(Box::new(self.parse_unary()?)));
        }
        if self
            .tokens
            .get(self.cursor)
            .is_some_and(|token| token == "(")
        {
            self.cursor += 1;
            let query = self.parse_or()?;
            if self
                .tokens
                .get(self.cursor)
                .is_none_or(|token| token != ")")
            {
                return Err(QueryParseError::MissingClosingParen);
            }
            self.cursor += 1;
            return Ok(query);
        }
        let token = self
            .tokens
            .get(self.cursor)
            .ok_or(QueryParseError::Empty)?
            .clone();
        self.cursor += 1;
        parse_term_token(&token, true)
    }

    fn take_word(&mut self, word: &str) -> bool {
        if self
            .tokens
            .get(self.cursor)
            .is_some_and(|token| token.eq_ignore_ascii_case(word))
        {
            self.cursor += 1;
            true
        } else {
            false
        }
    }
}

/// Plain full-text mode (D-07): whitespace separates AND-combined terms;
/// quoted runs become one phrase; supported field-filter prefixes are
/// honored; everything else — punctuation included — is literal text
/// (§6.9). There are no implicit operators in this mode.
fn parse_plain(input: &str) -> Result<Query, QueryParseError> {
    let mut leaves: Vec<Query> = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for ch in input.chars() {
        match (quote, ch) {
            (Some(q), c) if q == c => {
                quote = None;
                if !current.trim().is_empty() {
                    leaves.push(parse_term_token(&current, false)?);
                }
                current.clear();
            }
            (None, '\'' | '"') => {
                if !current.trim().is_empty() {
                    leaves.push(parse_term_token(&current, false)?);
                    current.clear();
                }
                quote = Some(ch);
            }
            (None, c) if c.is_whitespace() => {
                if !current.trim().is_empty() {
                    leaves.push(parse_term_token(&current, false)?);
                    current.clear();
                }
            }
            (Some(_) | None, c) => current.push(c),
        }
    }
    if quote.is_some() {
        return Err(QueryParseError::UnterminatedQuote);
    }
    if !current.trim().is_empty() {
        leaves.push(parse_term_token(&current, false)?);
    }
    if leaves.is_empty() {
        return Err(QueryParseError::Empty);
    }
    let query = if leaves.len() == 1 {
        leaves.remove(0)
    } else {
        Query::And(leaves)
    };
    check_query_limits(&query)?;
    Ok(query)
}

/// Enforce the NFR-8 AST complexity limits before any evaluation effect.
fn check_query_limits(query: &Query) -> Result<(), QueryParseError> {
    let (depth, nodes, text_leaves) = measure(query);
    if depth > MAX_QUERY_DEPTH {
        return Err(QueryParseError::TooComplex {
            reason: "ast depth limit exceeded",
        });
    }
    if nodes > MAX_QUERY_NODES {
        return Err(QueryParseError::TooComplex {
            reason: "ast node limit exceeded",
        });
    }
    if text_leaves > MAX_QUERY_TEXT_LEAVES {
        return Err(QueryParseError::TooComplex {
            reason: "text leaf limit exceeded",
        });
    }
    Ok(())
}

/// Post-order measurement of depth, node count and distinct-capable text
/// leaf count for one AST.
fn measure(query: &Query) -> (usize, usize, usize) {
    match query {
        Query::Text(leaf) => {
            let _ = leaf;
            (1, 1, 1)
        }
        Query::Tag { .. }
        | Query::Prop { .. }
        | Query::LinksTo(_)
        | Query::LinkedFrom(_)
        | Query::Unresolved
        | Query::Attachment { .. } => (1, 1, 0),
        Query::And(parts) | Query::Or(parts) => {
            let mut depth = 1;
            let mut nodes = 1;
            let mut leaves = 0;
            for part in parts {
                let (d, n, l) = measure(part);
                depth = depth.max(d + 1);
                nodes += n;
                leaves += l;
            }
            (depth, nodes, leaves)
        }
        Query::Not(inner) => {
            let (d, n, l) = measure(inner);
            (d + 1, n + 1, l)
        }
    }
}

/// Saturate an unsigned count into the `i64` range for SQLite binding.
///
/// Every value bound here is a page size or a generation counter, far below
/// `i64::MAX`; the saturation only exists so a hostile value cannot wrap.
fn saturating_to_i64<T: TryInto<i64>>(value: T) -> i64 {
    value.try_into().unwrap_or(i64::MAX)
}

/// Build the SQL condition and parameters for one property comparison.
///
/// Null comparisons bind no value and read `IS NULL` / `IS NOT NULL`; every
/// other comparison binds its rendered value. Values compare as text
/// (`v_text`), matching the property table's layout.
fn prop_to_sql(
    key: &str,
    op: CmpOp,
    value: &PropertyValue,
) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
    // The rendered bound value; `None` marks a NULL comparison.
    let val_param: Option<String> = match (op, value) {
        (_, PropertyValue::Null) => None,
        (CmpOp::Eq | CmpOp::Neq, PropertyValue::Text(t)) => Some(t.clone()),
        (CmpOp::Eq | CmpOp::Neq, PropertyValue::Number(n)) => Some(n.to_string()),
        (CmpOp::Eq | CmpOp::Neq, PropertyValue::Bool(b)) => {
            Some(if *b { "1" } else { "0" }.to_string())
        }
        (CmpOp::Eq | CmpOp::Neq, PropertyValue::Date(d)) => Some(d.clone()),
        (CmpOp::Eq | CmpOp::Neq, PropertyValue::Datetime(dt)) => Some(dt.clone()),
        (CmpOp::Lt | CmpOp::Lte | CmpOp::Gt | CmpOp::Gte, PropertyValue::Number(n)) => {
            Some(n.to_string())
        }
        (CmpOp::Contains, PropertyValue::Text(t)) => Some(format!("%{t}%")),
        (CmpOp::StartsWith, PropertyValue::Text(t)) => Some(format!("{t}%")),
        (CmpOp::EndsWith, PropertyValue::Text(t)) => Some(format!("%{t}")),
        // Fallback for an operator/value pair this condition does not
        // express: bind the literal "NULL" string, as this query always has.
        _ => Some("NULL".to_string()),
    };

    let condition = |op_sql: &str| {
        format!(
            "note_id IN (SELECT note_id FROM properties WHERE key = ? AND vtype = ? AND v_text {op_sql})"
        )
    };

    match val_param {
        Some(bound) => {
            let op_sql: &str = match op {
                CmpOp::Eq => "=",
                CmpOp::Neq => "!=",
                CmpOp::Lt => "<",
                CmpOp::Lte => "<=",
                CmpOp::Gt => ">",
                CmpOp::Gte => ">=",
                CmpOp::Contains | CmpOp::StartsWith | CmpOp::EndsWith => "LIKE",
            };
            (
                format!("{} ?", condition(op_sql)),
                vec![
                    Box::new(key.to_string()) as Box<dyn rusqlite::ToSql>,
                    Box::new("text".to_string()) as Box<dyn rusqlite::ToSql>,
                    Box::new(bound) as Box<dyn rusqlite::ToSql>,
                ],
            )
        }
        None => (
            condition(match op {
                CmpOp::Eq => "IS NULL",
                // Only `Neq` reaches here with a NULL value.
                _ => "IS NOT NULL",
            }),
            vec![
                Box::new(key.to_string()) as Box<dyn rusqlite::ToSql>,
                Box::new("text".to_string()) as Box<dyn rusqlite::ToSql>,
            ],
        ),
    }
}

/// Compute the next hierarchical key for range queries.
fn next_hierarchical_key(key: &str) -> String {
    // For a key like "a/b", the next prefix is "a/b0"
    // For "a/b/c", next is "a/b/c0"
    // For "a", next is "a0"
    // For "", next is "0"
    // This is used for range queries: tag >= "prefix" AND tag < "next_prefix"
    if key.is_empty() {
        return "0".to_string();
    }

    // Append "0" to the last path component
    if let Some(pos) = key.rfind('/') {
        format!("{}/{}0", &key[..pos], &key[pos + 1..])
    } else {
        format!("{key}0")
    }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag { name, hierarchical } => {
                if *hierarchical {
                    write!(f, "tag:{name}/...")
                } else {
                    write!(f, "tag:{name}")
                }
            }
            Self::Prop { key, op, value } => {
                write!(f, "{key}:{op:?}{value:?}")
            }
            Self::LinksTo(note_ref) => {
                write!(f, "links-to:{note_ref}")
            }
            Self::LinkedFrom(note_ref) => {
                write!(f, "linked-from:{note_ref}")
            }
            Self::Unresolved => {
                write!(f, "unresolved")
            }
            Self::Text(leaf) => {
                if leaf.phrase {
                    write!(f, "text:\"{}\"", leaf.term)
                } else {
                    write!(f, "text:{}", leaf.term)
                }
            }
            Self::Attachment { kind, name } => {
                write!(f, "attachment:{kind:?}/{name:?}")
            }
            Self::And(queries) => {
                write!(f, "AND(")?;
                for (i, q) in queries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{q}")?;
                }
                write!(f, ")")
            }
            Self::Or(queries) => {
                write!(f, "OR(")?;
                for (i, q) in queries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{q}")?;
                }
                write!(f, ")")
            }
            Self::Not(query) => {
                write!(f, "NOT({query})")
            }
        }
    }
}

/// Query builder for convenience.
#[derive(Debug, Default)]
pub struct QueryBuilder {
    conditions: Vec<Query>,
    #[allow(dead_code)]
    and_stack: Vec<Vec<Query>>,
}

impl QueryBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn tag(mut self, name: &str) -> Self {
        self.conditions.push(Query::Tag {
            name: name.to_string(),
            hierarchical: false,
        });
        self
    }

    #[must_use]
    pub fn tag_hierarchical(mut self, name: &str) -> Self {
        self.conditions.push(Query::Tag {
            name: name.to_string(),
            hierarchical: true,
        });
        self
    }

    #[must_use]
    pub fn prop(mut self, key: &str, op: CmpOp, value: PropertyValue) -> Self {
        self.conditions.push(Query::Prop {
            key: key.to_string(),
            op,
            value,
        });
        self
    }

    #[must_use]
    pub fn links_to(mut self, note_ref: NoteRef) -> Self {
        self.conditions.push(Query::LinksTo(note_ref));
        self
    }

    #[must_use]
    pub fn linked_from(mut self, note_ref: NoteRef) -> Self {
        self.conditions.push(Query::LinkedFrom(note_ref));
        self
    }

    #[must_use]
    pub fn unresolved(mut self) -> Self {
        self.conditions.push(Query::Unresolved);
        self
    }

    #[must_use]
    pub fn text(mut self, query: &str) -> Self {
        self.conditions
            .push(Query::Text(TextLeaf::all_fields(query)));
        self
    }

    #[must_use]
    pub fn attachment(
        mut self,
        kind: Option<crate::schema::AttachmentKind>,
        name: Option<&str>,
    ) -> Self {
        self.conditions.push(Query::Attachment {
            kind,
            name: name.map(std::string::ToString::to_string),
        });
        self
    }

    #[must_use]
    pub fn and(mut self) -> Self {
        let mut new = Self::new();
        let current_and = std::mem::take(&mut self.conditions);
        if !current_and.is_empty() {
            new.conditions.push(Query::And(current_and));
        }
        new
    }

    #[must_use]
    pub fn or(mut self, query: Query) -> Self {
        let last = self.conditions.pop().unwrap_or(Query::Unresolved);
        self.conditions.push(Query::Or(vec![last, query]));
        self
    }

    #[must_use]
    pub fn not(mut self, query: Query) -> Self {
        self.conditions.push(Query::Not(Box::new(query)));
        self
    }

    #[must_use]
    pub fn build(self) -> Query {
        let mut conditions = self.conditions;
        match conditions.len() {
            0 => Query::Unresolved,
            // The len == 1 guard makes the pop infallible; `unwrap_or`
            // keeps the function panic-free by construction.
            1 => conditions.pop().unwrap_or(Query::Unresolved),
            _ => Query::And(conditions),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_schema;
    use tempfile::NamedTempFile;

    #[test]
    fn parses_cli_boolean_tag_and_text_query() {
        assert_eq!(
            Query::parse("tag:foo AND text:'y'").unwrap(),
            Query::And(vec![
                Query::Tag {
                    name: "foo".into(),
                    hierarchical: false
                },
                Query::Text(TextLeaf::all_fields("y")),
            ])
        );
        assert!(Query::parse("tag:foo AND text:'unterminated").is_err());
        assert!(Query::parse("tag:").is_err());
    }

    #[test]
    fn test_query_display() {
        let query = Query::Tag {
            name: "rust".to_string(),
            hierarchical: false,
        };
        assert_eq!(format!("{query}"), "tag:rust");

        let query = Query::And(vec![
            Query::Tag {
                name: "rust".to_string(),
                hierarchical: false,
            },
            Query::Prop {
                key: "language".to_string(),
                op: CmpOp::Eq,
                value: PropertyValue::Text("rust".to_string()),
            },
        ]);
        assert_eq!(
            format!("{query}"),
            "AND(tag:rust, language:EqText(\"rust\"))"
        );
    }

    #[test]
    fn test_query_builder() {
        let query = QueryBuilder::new()
            .tag("rust")
            .and()
            .prop(
                "language",
                CmpOp::Eq,
                PropertyValue::Text("rust".to_string()),
            )
            .build();

        assert!(matches!(query, Query::And(_)));
    }

    #[test]
    fn test_next_hierarchical_key() {
        assert_eq!(next_hierarchical_key("a"), "a0");
        assert_eq!(next_hierarchical_key("a/b"), "a/b0");
        assert_eq!(next_hierarchical_key("a/b/c"), "a/b/c0");
        assert_eq!(next_hierarchical_key(""), "0");
    }

    #[test]
    fn test_tag_query() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open_with_flags(
            temp.path(),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE | rusqlite::OpenFlags::SQLITE_OPEN_CREATE,
        )
        .unwrap();

        create_schema(&mut conn).unwrap();

        // Insert test data
        conn.execute(
            "INSERT INTO notes (note_id, path, path_native, path_key, stem_key, title, content_hash, size, mtime_ns, scan_ns, generation) 
             VALUES (1, 'note1.md', X'6E6F7465312E6D64', 'note1.md', 'note1', 'Note 1', X'00', 100, 0, 0, 0)",
            [],
        ).unwrap();

        conn.execute(
            "INSERT INTO tags (note_id, tag, source) VALUES (1, 'rust', 'inline')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO tags (note_id, tag, source) VALUES (1, 'programming/rust', 'inline')",
            [],
        )
        .unwrap();

        let query = Query::Tag {
            name: "rust".to_string(),
            hierarchical: false,
        };

        let page = query.execute(&conn, 10, None).unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].note_id, 1);

        let query = Query::Tag {
            name: "programming".to_string(),
            hierarchical: true,
        };

        let page = query.execute(&conn, 10, None).unwrap();
        assert_eq!(page.items.len(), 1);
    }

    #[test]
    fn sql_translation_covers_composites_properties_and_attachment_filters() {
        let note = NoteRef::new(7, b"notes/seven.md".to_vec());
        let cases = [
            Query::Tag {
                name: "x/".into(),
                hierarchical: true,
            },
            Query::LinksTo(note.clone()),
            Query::LinkedFrom(note),
            Query::Unresolved,
            Query::Attachment {
                kind: None,
                name: None,
            },
            Query::Attachment {
                kind: Some(crate::schema::AttachmentKind::Image),
                name: None,
            },
            Query::Attachment {
                kind: None,
                name: Some("cover".into()),
            },
            Query::And(vec![Query::Unresolved, Query::Unresolved]),
            Query::Or(vec![Query::Unresolved, Query::Unresolved]),
            Query::Not(Box::new(Query::Unresolved)),
        ];
        for query in cases {
            let (sql, _) = query.to_sql().unwrap();
            assert!(!sql.is_empty());
        }
        // Text predicates are rejected by the structural translator — never
        // a silent empty success (§6.1/§6.3).
        assert!(matches!(
            Query::Text(TextLeaf::all_fields("a")).to_sql(),
            Err(IndexError::UnsupportedTextPredicate)
        ));
        assert!(
            Query::And(vec![
                Query::Text(TextLeaf::all_fields("a")),
                Query::Unresolved
            ])
            .to_sql()
            .is_err()
        );

        let properties = [
            (CmpOp::Eq, PropertyValue::Text("value".into())),
            (CmpOp::Neq, PropertyValue::Number(3.5)),
            (CmpOp::Lt, PropertyValue::Number(2.0)),
            (CmpOp::Lte, PropertyValue::Number(2.0)),
            (CmpOp::Gt, PropertyValue::Number(2.0)),
            (CmpOp::Gte, PropertyValue::Number(2.0)),
            (CmpOp::Contains, PropertyValue::Text("mid".into())),
            (CmpOp::StartsWith, PropertyValue::Text("pre".into())),
            (CmpOp::EndsWith, PropertyValue::Text("suf".into())),
            (CmpOp::Eq, PropertyValue::Bool(true)),
            (CmpOp::Eq, PropertyValue::Date("2026-01-01".into())),
            (
                CmpOp::Eq,
                PropertyValue::Datetime("2026-01-01T00:00:00Z".into()),
            ),
            (CmpOp::Eq, PropertyValue::Null),
            (CmpOp::Neq, PropertyValue::Null),
        ];
        for (op, value) in properties {
            let (sql, params) = Query::Prop {
                key: "k".into(),
                op,
                value,
            }
            .to_sql()
            .unwrap();
            assert!(sql.contains("properties"));
            assert!(params.len() >= 2);
        }
        assert!(Query::And(vec![]).to_sql().unwrap().0.is_empty());
        assert!(Query::Or(vec![]).to_sql().unwrap().0.is_empty());
    }

    #[test]
    fn execution_rejects_invalid_cursors_and_builds_next_page_cursor() {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        for id in 1..=2 {
            conn.execute(
                "INSERT INTO notes (note_id, path, path_native, path_key, stem_key, title, content_hash, size, mtime_ns, scan_ns, generation) VALUES (?1, '', ?2, '', '', '', X'00', 0, 0, 0, 0)",
                rusqlite::params![id, vec![u8::try_from(id).expect("fixture IDs fit in a byte")]],
            ).unwrap();
        }
        let query = Query::Not(Box::new(Query::Unresolved));
        let first = query.execute(&conn, 1, None).unwrap();
        assert_eq!(first.items.len(), 1);
        let cursor = first.next.unwrap();
        assert_eq!(cursor.note_id, Some(1));
        assert_eq!(
            query.execute(&conn, 1, Some(&cursor)).unwrap().items[0].note_id,
            2
        );

        let mut stale = cursor.clone();
        stale.epoch += 1;
        assert!(
            query
                .execute(&conn, 1, Some(&stale))
                .unwrap()
                .items
                .is_empty()
        );
        let mut ahead = cursor.clone();
        ahead.generation = u64::MAX;
        assert!(matches!(
            query.execute(&conn, 1, Some(&ahead)),
            Err(IndexError::StaleCursor)
        ));
        let mut expired = cursor;
        expired.generation = 0;
        conn.execute("UPDATE meta SET value = '2' WHERE key = 'log_floor'", [])
            .unwrap();
        assert!(matches!(
            query.execute(&conn, 1, Some(&expired)),
            Err(IndexError::ResetRequired)
        ));
    }
}
