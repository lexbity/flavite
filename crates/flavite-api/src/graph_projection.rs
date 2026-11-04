//! Revisioned current relationships built on a consistent saved index transaction.
//! Dirty sources replace their outgoing occurrences; failures never become empty
//! successful parses. Saved full-text membership always uses the canonical query.
//!
//! RFC-0004 S6: the saved base arrives as a [`SavedGraph`] — assembled by the
//! [`crate::graph_mirror`] from change-feed deltas or from the full consistent
//! read on a reset. This module no longer reads the saved tables; the overlay
//! and scope machinery, the retirement rules, `OccurrenceSource`, and the
//! `buffer_stamp`/digest semantics are exactly as before.
use flavite_graph::scope::{NoteMetadata, ScopeResult};
use flavite_graph::{
    EdgeKind, GraphBatch, GraphEdgeRecord, GraphNodeRecord, GraphScope, NodeId, NotePredicate,
    PropertyValue,
};
use flavite_index::resolver::Resolver;
use flavite_syntax::pulldown_glue::parse_normalized;
use flavite_vault::session::Session;
use rusqlite::{Connection, OptionalExtension};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::graph_mirror::{SavedGraph, unresolved_node};

/// Exact source version, including the buffer instance (same path is insufficient).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OccurrenceSource {
    Saved { content_hash: Vec<u8> },
    Buffer { buffer_id: u64, revision: u64 },
}

/// One specific directed connection. Saved spans use original source bytes;
/// overlay spans use normalized UTF-8 text bytes. Neither is a display offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkOccurrence {
    pub edge: GraphEdgeRecord,
    pub source_path: PathBuf,
    pub source: OccurrenceSource,
    pub start: usize,
    pub end: usize,
    pub target_raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OverlayState {
    Ready {
        buffer_id: u64,
        revision: u64,
    },
    Failed {
        buffer_id: u64,
        revision: u64,
        reason: &'static str,
    },
}

/// View request identity is supplied by the host, rechecked on publication.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct GraphViewToken {
    pub tab: u128,
    pub incarnation: u64,
    pub sequence: u64,
}

#[derive(Debug, Clone)]
pub struct ScopedGraph {
    pub token: GraphViewToken,
    pub result: Result<Arc<ScopeResult>, String>,
}

#[derive(Debug)]
pub struct GraphProjection {
    pub saved: GraphBatch,
    pub effective: GraphBatch,
    pub occurrences: Arc<[LinkOccurrence]>,
    pub overlays: BTreeMap<NodeId, OverlayState>,
    pub metadata: BTreeMap<NodeId, NoteMetadata>,
    pub scopes: BTreeMap<u128, ScopedGraph>,
    pub buffer_stamp: Vec<flavite_vault::session::GraphBufferStamp>,
    pub digest: u64,
    pub(crate) requests: BTreeMap<u128, ScopeRequest>,
}

/// Worker input. A scope is small and bounded before it can enter this map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRequest {
    pub token: GraphViewToken,
    pub scope: GraphScope,
}

/// One consistent DB transaction supplies overlay provenance, resolver, saved text
/// predicates and occurrence hashes; the saved base itself comes complete from
/// the [`GraphMirror`](crate::graph_mirror::GraphMirror). Only immutable Session
/// snapshots cross in.
#[allow(
    clippy::too_many_lines,
    reason = "overlay + scope assembly is one pass over the mirrored base; splitting would only relocate it"
)]
pub(crate) fn build(
    connection: &mut Connection,
    graph: &SavedGraph,
    session: Option<&Arc<Session>>,
    requests: &BTreeMap<u128, ScopeRequest>,
    previous: Option<&GraphProjection>,
) -> Result<Option<GraphProjection>, String> {
    let stamp = session.map_or_else(Vec::new, |s| s.graph_buffer_stamp());
    if previous.is_some_and(|p| {
        p.saved.epoch == graph.version.epoch
            && p.saved.generation == graph.version.generation
            && p.buffer_stamp == stamp
            && &p.requests == requests
    }) {
        return Ok(None);
    }
    let transaction = connection
        .transaction()
        .map_err(|_| "cannot begin graph snapshot")?;
    // The mirrored saved view: complete, decoded, canonical — identical to
    // what a full consistent read produces at `graph.version` (AC-3 gate).
    let saved = graph.saved.clone();
    let mut effective = graph.effective_pre_overlay();
    let mut occurrences = graph.occurrences.clone();
    let mut unresolved_ids = graph.unresolved_keys.clone();
    let mut metadata = graph.metadata.clone();
    let snapshots = session.map_or_else(Vec::new, |s| s.graph_buffer_snapshots());
    let buffer_stamp = snapshots
        .iter()
        .map(|s| {
            (
                s.buffer_id,
                s.revision,
                s.path.clone(),
                s.state,
                s.base_hash,
            )
        })
        .collect();
    let mut overlays = BTreeMap::new();
    let mut resolver = Resolver::new(&transaction);
    resolver
        .refresh_cache()
        .map_err(|_| "cannot load graph resolver")?;
    let mut digest = blake3::Hasher::new();
    for snapshot in snapshots {
        digest.update(&snapshot.buffer_id.as_u64().to_le_bytes());
        digest.update(&snapshot.revision.as_u64().to_le_bytes());
        digest.update(format!("{:?}{:?}", snapshot.state, snapshot.path).as_bytes());
        if let Some(hash) = snapshot.base_hash {
            digest.update(&hash);
        }
        let path = snapshot
            .path
            .strip_prefix(session.expect("snapshot needs session").root())
            .unwrap_or(&snapshot.path)
            .to_path_buf();
        let indexed = saved
            .nodes
            .iter()
            .find(|n| n.native_path == path && snapshot.state != crate::BufferStatus::Orphaned);
        let id = indexed.map_or_else(
            || NodeId::note(u64::MAX - snapshot.buffer_id.as_u64()),
            |n| n.id,
        );
        let saved_content: Option<(Vec<u8>, String)> = if let Some(record) = indexed {
            transaction
                .query_row(
                    "SELECT content_hash,body_text FROM search_docs WHERE note_id=?1",
                    [i64::try_from(record.id.id).map_err(|_| "invalid saved identity")?],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|_| "cannot read source provenance")?
        } else {
            None
        };
        // Retire only when the saved publication covers the exact base hash and
        // current normalized text; a later dirty edit cannot be covered by n.
        if saved_content.as_ref().is_some_and(|(hash, text)| {
            snapshot
                .base_hash
                .as_ref()
                .is_some_and(|h| h.as_slice() == hash)
                && snapshot.text.as_ref() == text
                && snapshot.state == crate::BufferStatus::Clean
        }) {
            continue;
        }
        if snapshot.state == crate::BufferStatus::Orphaned {
            for record in effective
                .nodes
                .iter_mut()
                .filter(|record| record.native_path == path)
            {
                record.binding_blocked = true;
                record.title.push_str(" (resolve replaced path)");
            }
        }
        if indexed.is_none() {
            // Display loss is allowed only in the label; exact native identity
            // remains available even when the UI cannot serialize a wikilink.
            let display = path.to_string_lossy().into_owned();
            let mut record = GraphNodeRecord::note(
                id.id,
                display.clone(),
                native_path(&path),
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unsaved note"),
                None,
                false,
            );
            record.native_path = path.clone();
            effective.nodes.push(record);
        }
        effective.edges.retain(|edge| edge.from != id);
        occurrences.retain(|occurrence| occurrence.edge.from != id);
        let parsed = match parse_normalized(&snapshot.text) {
            Ok(parsed) => parsed,
            Err(_) => {
                overlays.insert(
                    id,
                    OverlayState::Failed {
                        buffer_id: snapshot.buffer_id.as_u64(),
                        revision: snapshot.revision.as_u64(),
                        reason: "Current relationships unavailable; fix the source parse error",
                    },
                );
                metadata.remove(&id);
                continue;
            }
        };
        let current_metadata = parse_metadata(&parsed);
        metadata.insert(id, current_metadata);
        if let Some(record) = effective.nodes.iter_mut().find(|r| r.id == id) {
            let frontmatter = flavite_syntax::frontmatter::parse_frontmatter(&parsed.snapshot);
            record.missing_on_disk = snapshot.state == crate::BufferStatus::Orphaned;
            record.title = flavite_syntax::wellknown::derive_title(
                frontmatter.as_ref().and_then(|f| f.title.as_deref()),
                parsed.headings.first().map(|(_, text)| text.as_str()),
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unsaved note"),
            );
        }
        if snapshot.state == crate::BufferStatus::Orphaned
            && let Some(record) = effective.nodes.iter_mut().find(|r| r.id == id)
        {
            record.title.push_str(" (missing on disk)");
        }
        for link in &parsed.links {
            let resolution = resolver.resolve_at_path(
                &flavite_syntax::text::fold_key(&link.target_raw),
                &link.target_raw,
                link.is_embed,
                &path,
            );
            let to = if let Some(note) = resolution.note {
                Some(NodeId::note(note.note_id as u64))
            } else {
                resolution
                    .attachment
                    .map(|att| NodeId::attachment(att.att_id as u64))
            };
            let to = match to {
                Some(to) => to,
                None => unresolved_node(&mut effective, &mut unresolved_ids, &link.target_raw)?,
            };
            let edge = GraphEdgeRecord {
                from: id,
                to,
                kind: if link.is_embed {
                    EdgeKind::Embed
                } else {
                    EdgeKind::Link
                },
            };
            effective.edges.push(edge);
            occurrences.push(LinkOccurrence {
                edge,
                source_path: path.clone(),
                source: OccurrenceSource::Buffer {
                    buffer_id: snapshot.buffer_id.as_u64(),
                    revision: snapshot.revision.as_u64(),
                },
                start: link.span.start.0,
                end: link.span.end.0,
                target_raw: link.target_raw.clone(),
            });
        }
        overlays.insert(
            id,
            OverlayState::Ready {
                buffer_id: snapshot.buffer_id.as_u64(),
                revision: snapshot.revision.as_u64(),
            },
        );
    }
    let linked_unresolved: BTreeSet<_> = effective.edges.iter().map(|e| e.to).collect();
    effective.nodes.retain(|n| {
        n.id.kind != flavite_graph::NodeKind::Unresolved || linked_unresolved.contains(&n.id)
    });
    effective.nodes.sort_by_key(|record| record.id);
    effective.edges.sort();
    effective.edges.dedup();
    let mut scopes = BTreeMap::new();
    for (tab, request) in requests {
        let result = (|| {
            let mut queries = BTreeMap::new();
            collect_queries(
                &request.scope.predicate,
                &transaction,
                session,
                &mut queries,
            )?;
            let mut result =
                flavite_graph::scope::evaluate(&effective, &metadata, &queries, &request.scope)
                    .map_err(str::to_owned)?;
            result.total_base_notes = saved
                .nodes
                .iter()
                .filter(|n| n.id.kind == flavite_graph::NodeKind::Note)
                .count();
            if overlays
                .values()
                .any(|s| matches!(s, OverlayState::Failed { .. }))
                && !matches!(request.scope.predicate, NotePredicate::All)
            {
                return Err(
                    "Current metadata unavailable for a source; fix its parse error".into(),
                );
            }
            Ok(Arc::new(result))
        })();
        scopes.insert(
            *tab,
            ScopedGraph {
                token: request.token,
                result,
            },
        );
    }
    transaction
        .commit()
        .map_err(|_| "cannot finish graph snapshot")?;
    Ok(Some(GraphProjection {
        saved,
        effective,
        occurrences: occurrences.into(),
        overlays,
        metadata,
        scopes,
        buffer_stamp,
        digest: u64::from_le_bytes(
            digest.finalize().as_bytes()[..8]
                .try_into()
                .expect("hash prefix"),
        ),
        requests: requests.clone(),
    }))
}

fn meta(conn: &Connection, key: &str, default: u64) -> Result<u64, String> {
    conn.query_row("SELECT value FROM meta WHERE key=?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .optional()
    .map_err(|_| "cannot read index version")?
    .map_or(Ok(default), |s| {
        s.parse().map_err(|_| "invalid index version".into())
    })
}

fn parse_metadata(parsed: &flavite_syntax::pulldown_glue::ParsedNote) -> NoteMetadata {
    use flavite_syntax::frontmatter::PropertyValue as V;
    let mut metadata = NoteMetadata::default();
    for tag in &parsed.tags {
        if let Some(text) = parsed.snapshot.text().get(tag.start.0..tag.end.0) {
            metadata
                .tags
                .insert(flavite_syntax::wellknown::normalize_tag(
                    text.trim_start_matches('#'),
                ));
        }
    }
    fn scalar(value: V) -> Vec<PropertyValue> {
        match value {
            V::Text(s) | V::Raw(s) => vec![PropertyValue::Text(s)],
            V::Number(s) => vec![PropertyValue::Number(s)],
            V::Bool(v) => vec![PropertyValue::Bool(v)],
            V::Date(s) => vec![PropertyValue::Date(s)],
            V::DateTime(s) => vec![PropertyValue::DateTime(s)],
            V::Null => vec![PropertyValue::Null],
            V::List(values) => values.into_iter().flat_map(scalar).collect(),
        }
    }
    if let Some(frontmatter) = flavite_syntax::frontmatter::parse_frontmatter(&parsed.snapshot) {
        for (key, values) in frontmatter.well_known {
            if matches!(
                key,
                flavite_syntax::wellknown::WellKnownKey::Tags
                    | flavite_syntax::wellknown::WellKnownKey::Tag
            ) {
                metadata.tags.extend(
                    values
                        .iter()
                        .map(|s| flavite_syntax::wellknown::normalize_tag(s)),
                );
            }
        }
        for (key, values) in frontmatter.properties {
            metadata.properties.insert(
                key,
                values
                    .into_iter()
                    .flat_map(|(_, value)| scalar(value))
                    .collect(),
            );
        }
    }
    metadata
}

fn collect_queries(
    predicate: &NotePredicate,
    conn: &Connection,
    session: Option<&Arc<Session>>,
    queries: &mut BTreeMap<String, BTreeSet<NodeId>>,
) -> Result<(), String> {
    match predicate {
        NotePredicate::SavedQuery { query } => {
            if !queries.contains_key(query) {
                let parsed = crate::Query::parse(query)
                    .map_err(|_| "Invalid saved-content query; the last valid scope is retained")?;
                let matches = query_membership(&parsed, conn, session)?;
                queries.insert(query.clone(), matches);
            }
        }
        NotePredicate::And { clauses } | NotePredicate::Or { clauses } => {
            for clause in clauses {
                collect_queries(clause, conn, session, queries)?;
            }
        }
        NotePredicate::Not { clause } => collect_queries(clause, conn, session, queries)?,
        _ => {}
    }
    Ok(())
}

fn query_membership(
    query: &crate::Query,
    conn: &Connection,
    session: Option<&Arc<Session>>,
) -> Result<BTreeSet<NodeId>, String> {
    use crate::Query;
    match query {
        Query::Text(text) => {
            let session = session.ok_or("Saved text search unavailable")?;
            let page = session
                .search_notes(Query::Text(text.clone()), 50_001)
                .map_err(|_| "Saved text search failed")?;
            if page.items.len() > 50_000 {
                return Err("Saved search exceeds 50k note view limit".into());
            }
            if (page.epoch, page.generation)
                != (meta(conn, "epoch", 1)?, meta(conn, "generation", 0)?)
            {
                return Err("Saved search is updating to the graph generation".into());
            }
            Ok(page
                .items
                .into_iter()
                .map(|row| NodeId::note(row.note_id as u64))
                .collect())
        }
        Query::And(parts) | Query::Or(parts) => {
            let mut sets = parts
                .iter()
                .map(|part| query_membership(part, conn, session));
            let mut result = sets.next().transpose()?.unwrap_or_default();
            for set in sets {
                let set = set?;
                result = if matches!(query, Query::And(_)) {
                    result.intersection(&set).copied().collect()
                } else {
                    result.union(&set).copied().collect()
                };
            }
            Ok(result)
        }
        Query::Not(part) => {
            let excluded = query_membership(part, conn, session)?;
            let mut statement = conn
                .prepare("SELECT note_id FROM notes")
                .map_err(|_| "Saved query failed")?;
            let all = statement
                .query_map([], |row| Ok(NodeId::note(row.get::<_, i64>(0)? as u64)))
                .map_err(|_| "Saved query failed")?
                .collect::<Result<BTreeSet<_>, _>>()
                .map_err(|_| "Saved query failed")?;
            Ok(all.difference(&excluded).copied().collect())
        }
        _ => {
            let page = query
                .execute(conn, 50_001, None)
                .map_err(|_| "Saved query failed")?;
            if page.items.len() > 50_000 {
                return Err("Saved query exceeds 50k note view limit".into());
            }
            Ok(page
                .items
                .into_iter()
                .map(|n| NodeId::note(n.note_id as u64))
                .collect())
        }
    }
}

fn native_path(path: &std::path::Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        path.as_os_str().as_encoded_bytes().to_vec()
    }
}
