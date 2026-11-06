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
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use crate::graph_mirror::{SavedGraph, node_id_entity, unresolved_node};

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

/// The occurrence collection of one projection (RFC-0005 F3, §4.7/§5.4).
///
/// The saved occurrence runs are shared with the mirror snapshot — an
/// `Arc`, never copied per publish. Dirty sources' saved runs are replaced
/// by fresh overlay runs; [`Self::iter`] yields the saved runs (dirty
/// sources omitted) followed by the overlay runs, byte-identical to the
/// retired `retain` + `extend` construction so `(src, ord)` run adjacency
/// for every untouched source is preserved (the backlinks grouping relies
/// on it — pinned by the order-equivalence property test).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OccurrenceSet {
    /// Canonical saved occurrence runs (`(src, ord)` order; resolved then
    /// unresolved sections), shared with the snapshot.
    saved: Arc<[LinkOccurrence]>,
    /// Sources whose saved runs are replaced by `overlay` (row-id keying,
    /// the mirror's `node_id_entity` policy).
    dirty_srcs: BTreeSet<i64>,
    /// Fresh runs for the dirty sources, in canonical `(src, ord)` order.
    overlay: Vec<LinkOccurrence>,
}

impl OccurrenceSet {
    /// Assemble an explicit set (tests, future consumers). The invariant
    /// [`Self::iter`] guarantees is up to the caller in this constructor;
    /// the projection builder maintains it.
    #[must_use]
    pub fn from_parts(
        saved: Arc<[LinkOccurrence]>,
        dirty_srcs: BTreeSet<i64>,
        overlay: Vec<LinkOccurrence>,
    ) -> Self {
        Self {
            saved,
            dirty_srcs,
            overlay,
        }
    }

    /// Saved runs (dirty sources omitted) then overlay runs — the exact
    /// order of the pre-F3 construction.
    pub fn iter(&self) -> impl Iterator<Item = &LinkOccurrence> {
        self.saved
            .iter()
            .filter(move |occurrence| {
                !self
                    .dirty_srcs
                    .contains(&node_id_entity(occurrence.edge.from))
            })
            .chain(self.overlay.iter())
    }

    /// Total occurrence count; `O(saved + overlay)` (metrics/tests only).
    #[must_use]
    pub fn len(&self) -> usize {
        let removed = self
            .saved
            .iter()
            .filter(|occurrence| {
                self.dirty_srcs
                    .contains(&node_id_entity(occurrence.edge.from))
            })
            .count();
        self.saved.len() - removed + self.overlay.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Membership over the merged view (element equality, like a slice).
    #[must_use]
    pub fn contains(&self, occurrence: &LinkOccurrence) -> bool {
        self.iter().any(|candidate| candidate == occurrence)
    }

    /// The `index`-th occurrence of [`Self::iter`] (the chooser's indexed
    /// access).
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&LinkOccurrence> {
        self.iter().nth(index)
    }
}

impl Default for OccurrenceSet {
    fn default() -> Self {
        Self {
            saved: Arc::from([]),
            dirty_srcs: BTreeSet::new(),
            overlay: Vec::new(),
        }
    }
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
    /// Saved base, shared with the mirror snapshot (never copied per
    /// publish).
    pub saved: Arc<GraphBatch>,
    /// The one owned `O(V)` construction per publish: the snapshot's
    /// pre-overlay batch, mutated in place by the overlay.
    pub effective: GraphBatch,
    /// Occurrences: shared saved runs + dirty-source overlay (F3).
    pub occurrences: OccurrenceSet,
    pub overlays: BTreeMap<NodeId, OverlayState>,
    /// Saved metadata, shared with the mirror snapshot; dirty-buffer
    /// overrides live in [`Self::overlay_metadata`].
    pub metadata: Arc<BTreeMap<NodeId, NoteMetadata>>,
    /// Dirty-buffer metadata overrides (`Some` replaces the saved entry,
    /// `None` removes it — a failed parse). Consumed by scope evaluation;
    /// the saved map itself is never mutated.
    pub overlay_metadata: BTreeMap<NodeId, Option<NoteMetadata>>,
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
///
/// F3 sharing (RFC-0005 S4): the saved base, metadata, and occurrence runs
/// are borrowed from the snapshot's `Arc`s — the only owned `O(V)`
/// construction is the `effective` batch. Dirty-buffer metadata overrides
/// land in [`GraphProjection::overlay_metadata`] and are merged into the
/// evaluation map only when a scope request exists (the saved map is never
/// mutated).
#[allow(
    clippy::too_many_lines,
    reason = "overlay + scope assembly is one pass over the mirrored base; splitting would only relocate it"
)]
/// Internal projection builder (`pub` for the scale/allocation gates and
/// the service pump; not part of the public facade surface).
pub fn build(
    connection: &mut Connection,
    graph: &Arc<SavedGraph>,
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
    let mut resolver = Resolver::new(&transaction);
    resolver
        .refresh_cache()
        .map_err(|_| "cannot load graph resolver")?;
    let projection = assemble(&transaction, &mut resolver, graph, session, requests)?;
    transaction
        .commit()
        .map_err(|_| "cannot finish graph snapshot")?;
    Ok(Some(projection))
}

/// The post-resolution projection assembly: everything [`build`] does after
/// the transaction and resolver catalog are ready — the F3 sharing, the
/// one owned `effective` clone, the dirty-buffer overlay, canonicalization,
/// scope evaluation, and the projection record.
///
/// (`pub` for the scale and allocation gates, which time exactly this
/// phase: the RFC-0005 S6 25 ms materialization baseline decomposes into
/// these passes alone — never the resolver/DB leg, which is pre-existing
/// and out of S4's blast radius.)
pub fn assemble(
    transaction: &rusqlite::Transaction<'_>,
    resolver: &mut Resolver<'_>,
    graph: &Arc<SavedGraph>,
    session: Option<&Arc<Session>>,
    requests: &BTreeMap<u128, ScopeRequest>,
) -> Result<GraphProjection, String> {
    // The mirrored saved view: complete, decoded, canonical — identical to
    // what a full consistent read produces at `graph.version` (AC-3 gate).
    // F3: shared `Arc`s — the only owned O(V) construction below is the
    // `effective` batch (FR-9, NFR-6).
    let saved = Arc::clone(&graph.saved);
    let mut effective = graph.effective_pre_overlay();
    let mut occurrences = OccurrenceSet {
        saved: Arc::clone(&graph.occurrences),
        dirty_srcs: BTreeSet::new(),
        overlay: Vec::new(),
    };
    let mut unresolved_ids = graph.unresolved_keys.clone();
    let metadata = Arc::clone(&graph.metadata);
    let mut overlay_metadata: BTreeMap<NodeId, Option<NoteMetadata>> = BTreeMap::new();
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
        let indexed = saved.nodes.iter().find(|n| {
            n.native_path == path && snapshot.state != flavite_buffer::state::BufferState::Orphaned
        });
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
                && snapshot.state == flavite_buffer::state::BufferState::Clean
        }) {
            continue;
        }
        if snapshot.state == flavite_buffer::state::BufferState::Orphaned {
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
        // F3: instead of `occurrences.retain(...)` (an O(V) scan+copy of
        // the whole run), mark the source dirty; `iter()` skips its saved
        // runs and emits the overlay runs below. Byte-identical order.
        occurrences.dirty_srcs.insert(node_id_entity(id));
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
                overlay_metadata.insert(id, None);
                continue;
            }
        };
        let current_metadata = parse_metadata(&parsed);
        overlay_metadata.insert(id, Some(current_metadata));
        if let Some(record) = effective.nodes.iter_mut().find(|r| r.id == id) {
            let frontmatter = flavite_syntax::frontmatter::parse_frontmatter(&parsed.snapshot);
            record.missing_on_disk = snapshot.state == flavite_buffer::state::BufferState::Orphaned;
            record.title = flavite_syntax::wellknown::derive_title(
                frontmatter.as_ref().and_then(|f| f.title.as_deref()),
                parsed.headings.first().map(|(_, text)| text.as_str()),
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("Unsaved note"),
            );
        }
        if snapshot.state == flavite_buffer::state::BufferState::Orphaned
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
            occurrences.overlay.push(LinkOccurrence {
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
    // F3/AC-6: the orphan-unresolved pruning set is a `HashSet` over the
    // live `NodeId`s of the batch (bounded by the node count — O(U)
    // auxiliary memory, never a per-edge allocation; `NodeId` derives
    // `Hash`). `with_capacity(edges.len())` was measured to preallocate
    // the full edge count (8.9 MB at 300k edges for ~5k distinct targets);
    // the default grow-on-insert keeps the table O(distinct).
    let mut linked_unresolved: HashSet<NodeId> = HashSet::new();
    for edge in &effective.edges {
        linked_unresolved.insert(edge.to);
    }
    effective.nodes.retain(|n| {
        n.id.kind != flavite_graph::NodeKind::Unresolved || linked_unresolved.contains(&n.id)
    });
    // Canonicalization is identity-preserving, so the allocation-free
    // unstable sorts are used: node ids are unique (equal keys cannot
    // exist), and duplicate edges are byte-identical — dedup output is
    // unchanged (NFR-6: the stable sorts' O(E) scratch would be a second
    // O(vault) allocation per publish). The mirror maintains canonical
    // order, so the common publish is already sorted and the O(n log n)
    // pass is skipped (AC-6 at the design bound).
    if !effective.nodes.is_sorted_by_key(|record| record.id) {
        effective.nodes.sort_unstable_by_key(|record| record.id);
    }
    if !effective.edges.is_sorted() {
        effective.edges.sort_unstable();
    }
    effective.edges.dedup();
    // F3: the saved metadata map is shared and never mutated; dirty-buffer
    // overrides are merged only when a scope request exists — the common
    // keystroke publish (no filters) never pays the merge. The
    // scope-evaluation caching follow-on (RFC-0005 §11 triggers) owns
    // optimizing the filtered case further.
    let effective_metadata = if requests.is_empty() {
        None
    } else {
        Some(merge_metadata(&metadata, &overlay_metadata))
    };
    let mut scopes = BTreeMap::new();
    for (tab, request) in requests {
        let result = (|| {
            let mut queries = BTreeMap::new();
            collect_queries(&request.scope.predicate, transaction, session, &mut queries)?;
            let metadata_for_eval = effective_metadata.as_ref().unwrap_or(&metadata);
            let mut result = flavite_graph::scope::evaluate(
                &effective,
                metadata_for_eval,
                &queries,
                &request.scope,
            )
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
    Ok(GraphProjection {
        saved,
        effective,
        occurrences,
        overlays,
        metadata,
        overlay_metadata,
        scopes,
        buffer_stamp,
        digest: u64::from_le_bytes(
            digest.finalize().as_bytes()[..8]
                .try_into()
                .expect("hash prefix"),
        ),
        requests: requests.clone(),
    })
}

/// Merge dirty-buffer metadata overrides over the shared saved map —
/// `Some` replaces a saved entry, `None` removes it (a failed parse hides
/// the saved metadata from scope predicates).
fn merge_metadata(
    base: &BTreeMap<NodeId, NoteMetadata>,
    overlay: &BTreeMap<NodeId, Option<NoteMetadata>>,
) -> BTreeMap<NodeId, NoteMetadata> {
    let mut merged = base.clone();
    for (id, meta) in overlay {
        match meta {
            Some(meta) => {
                merged.insert(*id, meta.clone());
            }
            None => {
                merged.remove(id);
            }
        }
    }
    merged
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
