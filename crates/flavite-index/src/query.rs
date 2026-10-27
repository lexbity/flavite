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
    /// Full-text search in note bodies.
    Text(String),
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
    /// Execute this query against the SQLite index.
    ///
    /// Returns a page of note IDs matching the query, along with the index version.
    pub fn execute(
        &self,
        conn: &Connection,
        page_size: usize,
        cursor: Option<Cursor>,
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
        let (where_clause, base_params) = self.to_sql();

        // Build the full query with cursor support
        let mut final_sql = format!(
            "SELECT note_id, path_native FROM notes WHERE {}",
            where_clause
        );
        let mut all_params: Vec<Box<dyn rusqlite::ToSql>> = base_params;

        if let Some(cursor) = &cursor {
            final_sql.push_str(" AND (note_id > ? OR (note_id = ? AND generation > ?))");
            all_params.push(Box::new(cursor.note_id.unwrap_or(0)));
            all_params.push(Box::new(cursor.note_id.unwrap_or(0)));
            all_params.push(Box::new(cursor.generation as i64));
        }

        final_sql.push_str(" ORDER BY note_id ASC, generation ASC LIMIT ?");
        all_params.push(Box::new(page_size as i64));

        // Create a parameter list for the query
        let param_refs: Vec<&dyn rusqlite::ToSql> = all_params.iter().map(|p| p.as_ref()).collect();

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

    /// Convert the query to SQL fragments.
    /// Returns (WHERE_clause, params) where WHERE_clause is the condition without the WHERE keyword
    fn to_sql(&self) -> (String, Vec<Box<dyn rusqlite::ToSql>>) {
        match self {
            Self::Tag { name, hierarchical } => {
                if *hierarchical {
                    // For hierarchical: `a/b` matches `a/b` and `a/b/c`
                    // Use range scan: [ 'a/', 'a0' )
                    let prefix = if name.ends_with('/') {
                        name.to_string()
                    } else {
                        format!("{}/", name)
                    };
                    let next_prefix = next_hierarchical_key(name);
                    (
                        format!("note_id IN (SELECT note_id FROM tags WHERE tag >= ? AND tag < ?)",),
                        vec![
                            Box::new(prefix) as Box<dyn rusqlite::ToSql>,
                            Box::new(next_prefix) as Box<dyn rusqlite::ToSql>,
                        ],
                    )
                } else {
                    (
                        "note_id IN (SELECT note_id FROM tags WHERE tag = ?)".to_string(),
                        vec![Box::new(name.clone()) as Box<dyn rusqlite::ToSql>],
                    )
                }
            }
            Self::Prop { key, op, value } => {
                let (_sql_op, val_param) = match (op, value) {
                    (CmpOp::Eq, PropertyValue::Text(t)) => ("=", Some(t.clone())),
                    (CmpOp::Eq, PropertyValue::Number(n)) => ("=", Some(n.to_string())),
                    (CmpOp::Eq, PropertyValue::Bool(b)) => {
                        ("=", Some(if *b { "1" } else { "0" }.to_string()))
                    }
                    (CmpOp::Eq, PropertyValue::Date(d)) => ("=", Some(d.clone())),
                    (CmpOp::Eq, PropertyValue::Datetime(dt)) => ("=", Some(dt.clone())),
                    (CmpOp::Eq, PropertyValue::Null) => ("IS", None),
                    (CmpOp::Neq, PropertyValue::Text(t)) => ("!=", Some(t.clone())),
                    (CmpOp::Neq, PropertyValue::Number(n)) => ("!=", Some(n.to_string())),
                    (CmpOp::Neq, PropertyValue::Bool(b)) => {
                        ("!=", Some(if *b { "1" } else { "0" }.to_string()))
                    }
                    (CmpOp::Neq, PropertyValue::Date(d)) => ("!=", Some(d.clone())),
                    (CmpOp::Neq, PropertyValue::Datetime(dt)) => ("!=", Some(dt.clone())),
                    (CmpOp::Neq, PropertyValue::Null) => ("IS NOT", None),
                    (CmpOp::Lt, PropertyValue::Number(n)) => ("<", Some(n.to_string())),
                    (CmpOp::Lte, PropertyValue::Number(n)) => ("<=", Some(n.to_string())),
                    (CmpOp::Gt, PropertyValue::Number(n)) => (">", Some(n.to_string())),
                    (CmpOp::Gte, PropertyValue::Number(n)) => (">=", Some(n.to_string())),
                    (CmpOp::Contains, PropertyValue::Text(t)) => ("LIKE", Some(format!("%{}%", t))),
                    (CmpOp::StartsWith, PropertyValue::Text(t)) => {
                        ("LIKE", Some(format!("{}%", t)))
                    }
                    (CmpOp::EndsWith, PropertyValue::Text(t)) => ("LIKE", Some(format!("%{}", t))),
                    _ => ("=", Some("NULL".to_string())), // fallback
                };

                let op_sql = match op {
                    CmpOp::Eq => {
                        if val_param.is_none() {
                            // IS NULL case
                            "IS NULL"
                        } else {
                            "="
                        }
                    }
                    CmpOp::Neq => {
                        if val_param.is_none() {
                            // IS NOT NULL case
                            "IS NOT NULL"
                        } else {
                            "!="
                        }
                    }
                    CmpOp::Lt => "<",
                    CmpOp::Lte => "<=",
                    CmpOp::Gt => ">",
                    CmpOp::Gte => ">=",
                    CmpOp::Contains | CmpOp::StartsWith | CmpOp::EndsWith => "LIKE",
                };

                // Handle NULL cases differently
                if val_param.is_none() {
                    (
                        format!(
                            "note_id IN (SELECT note_id FROM properties WHERE key = ? AND vtype = ? AND v_text {})",
                            op_sql
                        ),
                        vec![
                            Box::new(key.clone()) as Box<dyn rusqlite::ToSql>,
                            Box::new("text".to_string()) as Box<dyn rusqlite::ToSql>,
                        ],
                    )
                } else {
                    (
                        format!(
                            "note_id IN (SELECT note_id FROM properties WHERE key = ? AND vtype = ? AND v_text {} ?)",
                            op_sql
                        ),
                        vec![
                            Box::new(key.clone()) as Box<dyn rusqlite::ToSql>,
                            Box::new("text".to_string()) as Box<dyn rusqlite::ToSql>,
                            Box::new(val_param.unwrap()) as Box<dyn rusqlite::ToSql>,
                        ],
                    )
                }
            }
            Self::LinksTo(note_ref) => (
                format!("note_id IN (SELECT dst_note FROM links WHERE dst_note = ?)",),
                vec![Box::new(note_ref.note_id) as Box<dyn rusqlite::ToSql>],
            ),
            Self::LinkedFrom(note_ref) => (
                format!("note_id IN (SELECT src FROM links WHERE src = ?)",),
                vec![Box::new(note_ref.note_id) as Box<dyn rusqlite::ToSql>],
            ),
            Self::Unresolved => (
                "note_id IN (SELECT src FROM links WHERE res_state <> 'resolved')".to_string(),
                Vec::new(),
            ),
            Self::Text(_query) => {
                // This would be handled by tantivy in the search backend
                // For SQLite-only queries, we don't support full-text search
                // So we return an empty result for now
                ("1 = 0".to_string(), Vec::new())
            }
            Self::Attachment { kind, name } => {
                let mut conditions = Vec::new();
                let mut params = Vec::new();

                if let Some(k) = kind {
                    conditions.push(format!("kind = ?"));
                    params.push(Box::new(k.as_str().to_string()) as Box<dyn rusqlite::ToSql>);
                }

                if let Some(n) = name {
                    conditions.push(format!("stem_key = ?"));
                    params.push(Box::new(n.clone()) as Box<dyn rusqlite::ToSql>);
                }

                let where_clause = if conditions.is_empty() {
                    "1 = 1".to_string()
                } else {
                    conditions.join(" AND ")
                };

                (
                    format!(
                        "note_id IN (SELECT note_id FROM links WHERE dst_att IN (SELECT att_id FROM attachments WHERE {}))",
                        where_clause
                    ),
                    params,
                )
            }
            Self::And(queries) => {
                let mut conditions = Vec::new();
                let mut params = Vec::new();

                for query in queries {
                    let (sql, p) = query.to_sql();
                    conditions.push(format!("({})", sql));
                    params.extend(p);
                }

                (conditions.join(" AND "), params)
            }
            Self::Or(queries) => {
                let mut conditions = Vec::new();
                let mut params = Vec::new();

                for query in queries {
                    let (sql, p) = query.to_sql();
                    conditions.push(format!("({})", sql));
                    params.extend(p);
                }

                (conditions.join(" OR "), params)
            }
            Self::Not(query) => {
                let (sql, params) = query.to_sql();
                (format!("NOT ({})", sql), params)
            }
        }
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
        format!("{}0", key)
    }
}

impl fmt::Display for Query {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tag { name, hierarchical } => {
                if *hierarchical {
                    write!(f, "tag:{}/...", name)
                } else {
                    write!(f, "tag:{}", name)
                }
            }
            Self::Prop { key, op, value } => {
                write!(f, "{}:{:?}{:?}", key, op, value)
            }
            Self::LinksTo(note_ref) => {
                write!(f, "links-to:{}", note_ref)
            }
            Self::LinkedFrom(note_ref) => {
                write!(f, "linked-from:{}", note_ref)
            }
            Self::Unresolved => {
                write!(f, "unresolved")
            }
            Self::Text(query) => {
                write!(f, "text:\"{}\"", query)
            }
            Self::Attachment { kind, name } => {
                write!(f, "attachment:{:?}/{:?}", kind, name)
            }
            Self::And(queries) => {
                write!(f, "AND(")?;
                for (i, q) in queries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", q)?;
                }
                write!(f, ")")
            }
            Self::Or(queries) => {
                write!(f, "OR(")?;
                for (i, q) in queries.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", q)?;
                }
                write!(f, ")")
            }
            Self::Not(query) => {
                write!(f, "NOT({})", query)
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
    pub fn new() -> Self {
        Self::default()
    }

    pub fn tag(mut self, name: &str) -> Self {
        self.conditions.push(Query::Tag {
            name: name.to_string(),
            hierarchical: false,
        });
        self
    }

    pub fn tag_hierarchical(mut self, name: &str) -> Self {
        self.conditions.push(Query::Tag {
            name: name.to_string(),
            hierarchical: true,
        });
        self
    }

    pub fn prop(mut self, key: &str, op: CmpOp, value: PropertyValue) -> Self {
        self.conditions.push(Query::Prop {
            key: key.to_string(),
            op,
            value,
        });
        self
    }

    pub fn links_to(mut self, note_ref: NoteRef) -> Self {
        self.conditions.push(Query::LinksTo(note_ref));
        self
    }

    pub fn linked_from(mut self, note_ref: NoteRef) -> Self {
        self.conditions.push(Query::LinkedFrom(note_ref));
        self
    }

    pub fn unresolved(mut self) -> Self {
        self.conditions.push(Query::Unresolved);
        self
    }

    pub fn text(mut self, query: &str) -> Self {
        self.conditions.push(Query::Text(query.to_string()));
        self
    }

    pub fn attachment(
        mut self,
        kind: Option<crate::schema::AttachmentKind>,
        name: Option<&str>,
    ) -> Self {
        self.conditions.push(Query::Attachment {
            kind,
            name: name.map(|s| s.to_string()),
        });
        self
    }

    pub fn and(mut self) -> Self {
        let mut new = Self::new();
        let current_and = std::mem::take(&mut self.conditions);
        if !current_and.is_empty() {
            new.conditions.push(Query::And(current_and));
        }
        new
    }

    pub fn or(mut self, query: Query) -> Self {
        let last = self.conditions.pop().unwrap_or(Query::Unresolved);
        self.conditions.push(Query::Or(vec![last, query]));
        self
    }

    pub fn not(mut self, query: Query) -> Self {
        self.conditions.push(Query::Not(Box::new(query)));
        self
    }

    pub fn build(self) -> Query {
        if self.conditions.is_empty() {
            Query::Unresolved
        } else if self.conditions.len() == 1 {
            self.conditions.into_iter().next().unwrap()
        } else {
            Query::And(self.conditions)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::create_schema;
    use tempfile::NamedTempFile;

    #[test]
    fn test_query_display() {
        let query = Query::Tag {
            name: "rust".to_string(),
            hierarchical: false,
        };
        assert_eq!(format!("{}", query), "tag:rust");

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
            format!("{}", query),
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
}
