//! The incremental graph bridge (RFC-0004 W-B, S6, §6.5).
//!
//! [`GraphMirror`] owns the canonical saved graph view ([`SavedGraph`]) and
//! advances it by reading **only changed entities** from the typed change
//! feed ([`flavite_index::changelog`]). Every pump still sees a *complete*
//! view — the publisher's `apply_current` consumes the same full
//! [`GraphBatch`] it always did, so the dense node-record alignment
//! invariant (`feed.rs` misalignment refusal) is untouched (D7):
//! incremental *reads and splices*, complete *publications*.
//!
//! # Why the view is the state (AC-4 / NFR-2)
//!
//! `SavedGraph` fields are canonically ordered (nodes by id, resolved
//! occurrences in `(src, ord)` runs followed by the unresolved occurrence
//! section, edges sorted and deduplicated), so one source's rows occupy a
//! CONTIGUOUS region of the canonical vectors. A delta therefore replaces
//! those regions in place — element moves, not vault-wide record clones —
//! keeping synchronize + assembly at O(change + affected-run memmove +
//! unresolved-section rebuild) instead of O(vault) allocations; the
//! rebuild's dedup is linear (O(U log U)) since RFC-0005 F2 — before that
//! it re-scanned the growing node list per occurrence, an O(U²) tax on
//! every delta. The equivalence gate
//! (`tests/graph_mirror_equivalence.rs`) asserts the spliced view
//! byte-equals a fresh full read after every change, so the incremental
//! path can never silently diverge from `read_current` semantics.
//!
//! # Reset policy (§6.5.2, FR-5)
//!
//! [`Synchronize::Reset`] fires on: epoch change, `cursor.generation <
//! log_floor` ([`ResetReason::BelowLogFloor`]), parity failure, service
//! start, and DB recreate. The reset path is the full consistent read
//! ([`full_saved_graph`], which reuses
//! [`read_current`](crate::graph_service::read_current), the occurrence
//! scans, and the metadata scan verbatim) — correct-by-construction,
//! battle-tested, and the recovery for every drift class. A mirror whose
//! delta machinery fails mid-apply is *poisoned*, not half-trusted: the
//! next synchronize forces a full rebuild. SQLite remains the single truth
//! (§4.7): the mirror advances its cursor only after a fully successful
//! synchronize; the view is a disposable, rebuildable cache, checksummed
//! by the parity guard — a change-log consumer exactly like tantivy.
//!
//! # Parity guard (D8, §6.5.4)
//!
//! Every [`GRAPH_PARITY_GENERATIONS`] delta generations or
//! [`GRAPH_PARITY_INTERVAL`] of activity, the service re-reads a full
//! consistent snapshot and compares it against the view (nodes, edges,
//! occurrences per source, metadata per note). A mismatch is counted,
//! surfaced to the doctor, and forces a reset — detection for the entire
//! class of mirror-drift bugs, which is what makes the hot path safe to
//! splice locally instead of re-deriving globally.
//!
//! # Trust boundaries (§6.5 "How")
//!
//! An edge whose target node the view does not hold, or a stored unresolved
//! identity collision, is a change-log contract violation, not a
//! topological fact. Newly spliced rows are validated as they enter; the
//! parity guard catches anything the log lied about (inbound flips are the
//! log's job — FR-1), poisons the mirror, and the next synchronize rebuilds
//! from the full truth.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flavite_graph::scope::NoteMetadata;
use flavite_graph::{EdgeKind, GraphBatch, GraphEdgeRecord, GraphNodeRecord, NodeId, NodeKind};
use flavite_index::changelog::{
    ChangeKind, ChangeLogRead, ChangeLogReader, ChangeRecord, ResetReason,
};
use flavite_index::types::IndexVersion;
use flavite_vault::session::Session;
use rusqlite::{Connection, OptionalExtension};

use crate::graph_projection::{LinkOccurrence, OccurrenceSource};

// ---------------------------------------------------------------------------
// Drift-guard policy (D8; locked decisions — the constructor seam tightens
// them for tests and benches, production never does).
// ---------------------------------------------------------------------------

/// Delta generations between parity checks (§6.5.4).
pub const GRAPH_PARITY_GENERATIONS: u64 = 512;

/// Maximum gap between parity checks while the mirror is active (§6.5.4:
/// "512 delta generations or 600 s of activity").
pub const GRAPH_PARITY_INTERVAL: Duration = Duration::from_secs(600);

/// Drift-guard policy for one mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParityPolicy {
    /// Delta generations before the guard must run.
    pub generations: u64,
    /// Wall-clock gap before the guard must run while the mirror is active.
    pub interval: Duration,
}

impl Default for ParityPolicy {
    fn default() -> Self {
        Self {
            generations: GRAPH_PARITY_GENERATIONS,
            interval: GRAPH_PARITY_INTERVAL,
        }
    }
}

// ---------------------------------------------------------------------------
// SavedGraph — the canonical complete saved view at one index version
// ---------------------------------------------------------------------------

/// The complete saved graph state at one exact `(epoch, generation)`, in a
/// canonical layout both producers maintain:
///
/// * the full-read pipeline ([`full_saved_graph`]) — `read_current` plus the
///   occurrence, unresolved, and metadata scans, reused verbatim; and
/// * the delta path ([`GraphMirror::synchronize`]) — per-entity point reads
///   spliced into the contiguous regions of the same canonical vectors.
///
/// So `SavedGraph` equality is a *byte-comparable canonical* comparison
/// (AC-3): notes before attachments by id (the derived [`NodeId`] order),
/// edges sorted and deduplicated, occurrences in `(src, ord)` order with
/// unresolved occurrences after resolved ones, metadata per note with
/// property values in `(key, ord)` order, unresolved nodes unique by id and
/// sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedGraph {
    /// Exact index version every field was read at.
    pub version: IndexVersion,
    /// Saved notes + attachments + resolved edges — the exact
    /// [`read_current`](crate::graph_service::read_current) shape, native
    /// paths decoded.
    pub saved: GraphBatch,
    /// Unresolved target nodes derived from saved unresolved links, sorted
    /// by [`NodeId`]. Never part of [`Self::saved`] (same invariant as the
    /// pre-S6 bridge).
    pub unresolved_nodes: Vec<GraphNodeRecord>,
    /// Edges into unresolved nodes, sorted and deduplicated.
    pub unresolved_edges: Vec<GraphEdgeRecord>,
    /// Saved occurrences: the RESOLVED section first (contiguous
    /// `(src, ord)` runs, ordered by source), then the UNRESOLVED section
    /// (contiguous `(src, ord)` runs, ordered by source).
    pub occurrences: Vec<LinkOccurrence>,
    /// Tags + properties per note node.
    pub metadata: BTreeMap<NodeId, NoteMetadata>,
    /// Unresolved identity guard (`blake3`-prefix id → folded key) covering
    /// the unresolved nodes of this view.
    pub unresolved_keys: BTreeMap<u64, String>,
}

impl SavedGraph {
    /// The complete batch as it enters the publisher *before* buffer
    /// overlays: saved records plus unresolved nodes, saved edges plus
    /// unresolved edges. `build` clones this and applies overlays on top —
    /// exactly where the pre-S6 bridge did.
    #[must_use]
    pub fn effective_pre_overlay(&self) -> GraphBatch {
        let mut batch = self.saved.clone();
        batch.nodes.extend(self.unresolved_nodes.iter().cloned());
        batch.edges.extend(self.unresolved_edges.iter().copied());
        batch
    }

    /// The sorted position of one node record, if present. Node ids are the
    /// [`NodeId`] order (notes by id, then attachments by id), so a binary
    /// search locates any record.
    fn node_position(&self, id: NodeId) -> Option<usize> {
        let start = self.saved.nodes.partition_point(|node| node.id < id);
        self.saved
            .nodes
            .get(start)
            .filter(|node| node.id == id)
            .map(|_| start)
    }

    /// Replace or insert one node's canonical record.
    fn upsert_node(&mut self, record: GraphNodeRecord) {
        match self.node_position(record.id) {
            Some(position) => self.saved.nodes[position] = record,
            None => {
                let start = self.saved.nodes.partition_point(|node| node.id < record.id);
                self.saved.nodes.insert(start, record);
            }
        }
    }

    /// Remove one node record.
    fn remove_node(&mut self, id: NodeId) {
        if let Some(position) = self.node_position(id) {
            self.saved.nodes.remove(position);
        }
    }

    /// Replace the contiguous run of resolved edges leaving `from` (edges
    /// are globally sorted by `(from, to, kind)`, so one source's edges are
    /// one region).
    fn splice_edge_run(&mut self, from: NodeId, run: impl IntoIterator<Item = GraphEdgeRecord>) {
        let start = self.saved.edges.partition_point(|edge| edge.from < from);
        let end = self.saved.edges.partition_point(|edge| edge.from <= from);
        self.saved.edges.splice(start..end, run);
    }
}

/// The verdict of one parity comparison (§6.5.4): the version both sides
/// were read at, and bounded, human-readable findings (empty means the
/// mirror equals the full consistent read).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityOutcome {
    /// Index version the comparison ran at (mirror cursor == full-read
    /// version by construction).
    pub version: IndexVersion,
    /// Bounded mismatch samples — a drift detector, never a data pipe.
    pub findings: Vec<String>,
}

impl ParityOutcome {
    #[must_use]
    pub fn matched(&self) -> bool {
        self.findings.is_empty()
    }
}

/// Fixed-cardinality mirror counters (§6.5.5; surfaced via
/// `GraphServiceMetrics`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MirrorStats {
    /// Successful delta synchronizations.
    pub deltas_total: u64,
    /// Full rebuilds (epoch change, floor overrun, parity failure, start).
    pub resets_total: u64,
    /// Parity comparisons executed.
    pub parity_checks_total: u64,
    /// Parity mismatches (each one forced — or will force — a rebuild).
    pub mismatches_total: u64,
    /// Deduplicated entities touched by the last delta.
    pub last_delta_entities: u64,
    /// Distinct unresolved guard nodes in the view after the last
    /// synchronize/reset — the scale `rebuild_unresolved` works over on
    /// every delta (RFC-0005 F2 telemetry: the count whose scan went from
    /// O(U²) to O(U log U)). Printed by the scale bench; never a behavior
    /// input.
    pub unresolved_targets: u64,
}

/// The outcome of one [`GraphMirror::synchronize`] step. The updated view
/// is read through [`GraphMirror::view`] while the mirror is borrowed — a
/// pump that needs the complete saved state gets it without the mirror
/// cloning the whole vault first.
#[derive(Debug)]
pub enum Synchronize {
    /// The mirror cursor already equals the current index version; the
    /// saved view is unchanged (the pump publishes nothing and continues
    /// the layout drive — exactly today's `Ok(None)` path).
    UpToDate,
    /// The mirror advanced by the feed. `reset` says the publisher must
    /// rebuild the model from the complete view (§6.5.2); `entities` is
    /// the deduplicated count of changed entities spliced.
    Delta {
        version: IndexVersion,
        entities: u64,
        reset: bool,
    },
    /// The cursor cannot be advanced by deltas; the caller must
    /// [`GraphMirror::force_reset`] — never "no changes" (§4.14).
    Reset { reason: ResetReason },
}

/// One saved unresolved link of a source note. The node identity and edge
/// are derived from `raw` by the same [`unresolved_node`] functions the full
/// scan uses, so both producers emit byte-identical records.
#[derive(Debug, Clone, PartialEq, Eq)]
struct UnresolvedOcc {
    raw: String,
    start: usize,
    end: usize,
    kind: EdgeKind,
}

// ---------------------------------------------------------------------------
// GraphMirror
// ---------------------------------------------------------------------------

/// The in-memory mirror of the saved graph, advanced by the change feed.
/// Owned by the graph service thread behind a mutex (the service pump and
/// the on-demand doctor parity are the only drivers); delta machinery is
/// single-threaded by construction.
#[derive(Debug)]
pub struct GraphMirror {
    session: Option<Arc<Session>>,
    policy: ParityPolicy,
    cursor: IndexVersion,
    state: Option<MirrorState>,
    poisoned: bool,
    delta_generations_since_parity: u64,
    last_parity: Instant,
    stats: MirrorStats,
}

/// The complete canonical saved state plus the auxiliary per-source inputs
/// the splices need. Fields are mutated together inside one `&mut`;
/// drift (a contract-violating splice) sets [`MirrorState::drift`] and the
/// mirror poisons toward the full-truth rebuild.
#[derive(Debug)]
struct MirrorState {
    view: SavedGraph,
    /// End index of the RESOLVED occurrence section (the boundary the
    /// two-section occurrence vector is spliced against).
    resolved_occurrences: usize,
    /// Unresolved occurrences per source note, in `ord` order — the live
    /// inputs of the canonical unresolved sections.
    unresolved_by_src: BTreeMap<i64, Vec<UnresolvedOcc>>,
    /// `notes.content_hash` per source with link rows — the occurrence
    /// provenance the resolved/unresolved scans carry.
    source_hashes: BTreeMap<i64, Vec<u8>>,
    /// Monotonic unresolved identity guard (§6.5.1): entries are never
    /// removed, because it is a consistency guard, not a membership set.
    collision_ids: BTreeMap<u64, String>,
    drift: bool,
}

impl MirrorState {
    /// Build the full auxiliary state from one canonical view (reset path).
    fn from_view(view: SavedGraph) -> Self {
        let resolved_occurrences = view
            .occurrences
            .iter()
            .position(|occurrence| occurrence.edge.to.kind == NodeKind::Unresolved)
            .unwrap_or(view.occurrences.len());
        Self {
            unresolved_by_src: unresolved_by_source(&view.occurrences[resolved_occurrences..]),
            resolved_occurrences,
            source_hashes: source_hashes(&view.occurrences),
            collision_ids: view.unresolved_keys.clone(),
            view,
            drift: false,
        }
    }

    /// Splice the deduplicated change records at version `to` into the
    /// canonical view, inside the caller's read transaction.
    fn apply_delta(
        &mut self,
        tx: &rusqlite::Transaction<'_>,
        records: &[ChangeRecord],
        to: IndexVersion,
        session: Option<&Session>,
    ) -> Result<u64, String> {
        // §4.6: consumers MUST dedupe `(kind, entity_id)` before acting,
        // and MUST process whole generations atomically — every point read
        // below observes the final state at `to`, so the spliced view is
        // exactly the generation-consistent truth regardless of how many
        // generations the window spans (R10: one synchronize per pump).
        let mut note_ids: BTreeSet<i64> = BTreeSet::new();
        let mut attachment_ids: BTreeSet<i64> = BTreeSet::new();
        let mut outgoing_ids: BTreeSet<i64> = BTreeSet::new();
        for record in records {
            match record.kind {
                ChangeKind::Note => {
                    note_ids.insert(record.entity_id);
                }
                ChangeKind::Attachment => {
                    attachment_ids.insert(record.entity_id);
                }
                ChangeKind::Outgoing => {
                    outgoing_ids.insert(record.entity_id);
                }
            }
        }
        let mut note_stmt = tx
            .prepare(NOTE_POINT_READ)
            .map_err(|error| error.to_string())?;
        let mut link_stmt = tx
            .prepare(LINKS_BY_SRC)
            .map_err(|error| error.to_string())?;
        let mut tag_stmt = tx
            .prepare(TAGS_BY_NOTE)
            .map_err(|error| error.to_string())?;
        let mut property_stmt = tx
            .prepare(PROPERTIES_BY_NOTE)
            .map_err(|error| error.to_string())?;
        let mut attachment_stmt = tx
            .prepare(ATTACHMENT_POINT_READ)
            .map_err(|error| error.to_string())?;

        // Phase 1 — point reads inside this feed transaction: rows first,
        // mutations only after every native path is decoded. Decoding goes
        // through the session's path authority (one vault-actor hop), so
        // the whole delta shares ONE hop instead of one per entity.
        let mut note_rows: BTreeMap<i64, NoteRow> = BTreeMap::new();
        for id in &note_ids {
            if let Some(row) = note_stmt
                .query_row([*id], NoteRow::from_row)
                .optional()
                .map_err(|error| error.to_string())?
            {
                note_rows.insert(*id, row);
            }
        }
        let mut attachment_rows: BTreeMap<i64, AttachmentRow> = BTreeMap::new();
        for id in &attachment_ids {
            if let Some(row) = attachment_stmt
                .query_row([*id], AttachmentRow::from_row)
                .optional()
                .map_err(|error| error.to_string())?
            {
                attachment_rows.insert(*id, row);
            }
        }
        let mut link_rows: BTreeMap<i64, Vec<LinkRow>> = BTreeMap::new();
        for id in note_ids.iter().chain(&outgoing_ids) {
            link_rows.insert(*id, read_links(&mut link_stmt, *id)?);
        }
        // Outgoing-only sources whose node the view does not hold (a flip
        // was logged but the entity row never travelled — only possible if
        // the log lied): the link JOIN proves the row exists, so adopt the
        // record for the splice. Self-healing; the parity guard validates
        // it against the full truth regardless.
        let mut heal_rows: BTreeMap<i64, NoteRow> = BTreeMap::new();
        for id in &outgoing_ids {
            if note_ids.contains(id) {
                continue;
            }
            let node = node_id_of(*id);
            if self.view.node_position(node).is_none()
                && !link_rows[id].is_empty()
                && let Some(row) = note_stmt
                    .query_row([*id], NoteRow::from_row)
                    .optional()
                    .map_err(|error| error.to_string())?
            {
                heal_rows.insert(*id, row);
            }
        }
        // One session hop decodes every native path this delta touches.
        let mut decoded: Vec<GraphNodeRecord> = Vec::new();
        let mut decode_index: BTreeMap<(NodeKind, i64), usize> = BTreeMap::new();
        for (id, row) in &note_rows {
            decode_index.insert((NodeKind::Note, *id), decoded.len());
            decoded.push(row.record());
        }
        for (id, row) in &heal_rows {
            decode_index.insert((NodeKind::Note, *id), decoded.len());
            decoded.push(row.record());
        }
        for (id, row) in &attachment_rows {
            decode_index.insert((NodeKind::Attachment, *id), decoded.len());
            decoded.push(row.record());
        }
        decode_records(session, &mut decoded)?;

        // Nodes this window itself brings in — valid edge targets for every
        // splice below, regardless of splice order.
        let arriving: BTreeSet<NodeId> = decode_index
            .keys()
            .map(|(kind, id)| NodeId {
                kind: *kind,
                id: node_row_id(*id),
            })
            .collect();

        // Phase 2 — splice the canonical regions. Note entity rows carry
        // record + link set + metadata; the applied note always writes its
        // own `note` row (§6.1.2), so span/raw-only link rewrites and
        // tag/property edits land here.
        for id in &note_ids {
            let node = node_id_of(*id);
            let Some(position) = decode_index.get(&(NodeKind::Note, *id)) else {
                self.remove_note(node, *id);
                continue;
            };
            self.view.upsert_node(decoded[*position].clone());
            let rows = link_rows.remove(id).unwrap_or_default();
            self.splice_source_links(*id, rows, &arriving)?;
            self.view.metadata.remove(&node);
            let metadata = read_note_metadata(&mut tag_stmt, &mut property_stmt, *id)?;
            if !metadata.tags.is_empty() || !metadata.properties.is_empty() {
                self.view.metadata.insert(node, metadata);
            }
        }
        // Attachments: the graph shows connectivity, not multiplicity, so
        // the row is the whole payload; inbound flips arrive as `outgoing`
        // rows (FR-1) and splice the referencing sources.
        for id in &attachment_ids {
            match decode_index.get(&(NodeKind::Attachment, *id)) {
                None => self.view.remove_node(attachment_id_of(*id)),
                Some(position) => self.view.upsert_node(decoded[*position].clone()),
            }
        }
        // Outgoing-only sources: splice just the link set — the record and
        // metadata did not move.
        for id in &outgoing_ids {
            if note_ids.contains(id) {
                continue;
            }
            if let Some(position) = decode_index.get(&(NodeKind::Note, *id)) {
                self.view.upsert_node(decoded[*position].clone());
            }
            let rows = link_rows.remove(id).unwrap_or_default();
            self.splice_source_links(*id, rows, &arriving)?;
        }
        // The unresolved sections are derived views of
        // [`MirrorState::unresolved_by_src`], rebuilt once per delta after
        // every node splice has landed (sources and targets may both move
        // inside one window).
        self.rebuild_unresolved();
        self.view.version = to;
        self.view.saved.epoch = to.epoch;
        self.view.saved.generation = to.generation;
        Ok(u64::try_from(
            note_ids.len()
                + attachment_ids.len()
                + outgoing_ids
                    .iter()
                    .filter(|id| !note_ids.contains(id))
                    .count(),
        )
        .unwrap_or(u64::MAX))
    }

    /// Replace one source's whole link footprint (resolved edge/occurrence
    /// runs, unresolved rows, provenance hash) with freshly read rows.
    /// `arriving` are the nodes this delta window itself will splice in —
    /// a brand-new target created earlier in the same window is known, so
    /// only rows pointing at nodes neither present nor arriving are the
    /// change-log violations the §6.5 "How" note describes.
    fn splice_source_links(
        &mut self,
        src: i64,
        rows: Vec<LinkRow>,
        arriving: &BTreeSet<NodeId>,
    ) -> Result<(), String> {
        let from = node_id_of(src);
        let source_path = self
            .view
            .node_position(from)
            .map(|position| self.view.saved.nodes[position].native_path.clone())
            .unwrap_or_default();
        let mut new_edges: BTreeSet<GraphEdgeRecord> = BTreeSet::new();
        let mut new_resolved: Vec<LinkOccurrence> = Vec::new();
        let mut new_unresolved: Vec<UnresolvedOcc> = Vec::new();
        for row in &rows {
            let kind = if row.kind == "link" {
                EdgeKind::Link
            } else {
                EdgeKind::Embed
            };
            match (row.dst_note, row.dst_att) {
                (Some(_), _) | (None, Some(_)) => {
                    let target = match (row.dst_note, row.dst_att) {
                        (Some(note), _) => node_id_of(note),
                        (_, Some(att)) => attachment_id_of(att),
                        (None, None) => continue,
                    };
                    // §6.5 "How": an edge into a node the view neither
                    // holds nor this window is bringing in is a change-log
                    // contract violation — counted, never published, drift
                    // toward the full-truth rebuild.
                    if self.view.node_position(target).is_none() && !arriving.contains(&target) {
                        tracing::warn!(
                            target: "flavite_api::graph_mirror",
                            src,
                            "graph mirror spliced an edge into a missing node (change-log contract violation; the next synchronize rebuilds)"
                        );
                        self.drift = true;
                    }
                    let edge = GraphEdgeRecord {
                        from,
                        to: target,
                        kind,
                    };
                    new_edges.insert(edge);
                    new_resolved.push(LinkOccurrence {
                        edge,
                        source_path: source_path.clone(),
                        source: OccurrenceSource::Saved {
                            content_hash: row.content_hash.clone(),
                        },
                        start: row.start,
                        end: row.end,
                        target_raw: row.dst_raw.clone(),
                    });
                }
                (None, None) => {
                    // §6.5.1: the same collision guard as the full scan —
                    // a stored identity collision is corruption, surfaced,
                    // never guessed past.
                    let key = flavite_syntax::text::fold_key(&row.dst_raw);
                    let hash = unresolved_id_for(&row.dst_raw);
                    if let Some(previous) = self.collision_ids.get(&hash) {
                        if previous != &key {
                            return Err(
                                "Unresolved target identity collision; edit the source".into()
                            );
                        }
                    } else {
                        self.collision_ids.insert(hash, key);
                    }
                    new_unresolved.push(UnresolvedOcc {
                        raw: row.dst_raw.clone(),
                        start: row.start,
                        end: row.end,
                        kind,
                    });
                }
            }
        }
        if let Some(hash) = rows.first().map(|row| row.content_hash.clone()) {
            self.source_hashes.insert(src, hash);
        } else {
            self.source_hashes.remove(&src);
        }
        self.view.splice_edge_run(from, new_edges);
        self.splice_resolved_run(from, new_resolved);
        if new_unresolved.is_empty() {
            self.unresolved_by_src.remove(&src);
        } else {
            self.unresolved_by_src.insert(src, new_unresolved);
        }
        Ok(())
    }

    /// Remove a note wholesale: node record, edge run, occurrence runs,
    /// unresolved rows, and metadata.
    fn remove_note(&mut self, node: NodeId, id: i64) {
        self.view.remove_node(node);
        self.view.splice_edge_run(node, []);
        self.splice_resolved_run(node, Vec::new());
        self.view.metadata.remove(&node);
        self.unresolved_by_src.remove(&id);
        self.source_hashes.remove(&id);
    }

    /// Replace one source's RESOLVED occurrence run inside the resolved
    /// section, keeping the section boundary exact.
    fn splice_resolved_run(&mut self, from: NodeId, run: Vec<LinkOccurrence>) {
        let boundary = self.resolved_occurrences;
        let start = self.view.occurrences[..boundary].partition_point(|o| o.edge.from < from);
        let end = self.view.occurrences[..boundary].partition_point(|o| o.edge.from <= from);
        let removed = end - start;
        let inserted = run.len();
        self.view.occurrences.splice(start..end, run);
        self.resolved_occurrences = boundary + inserted - removed;
    }

    /// Rebuild the canonical unresolved structures from
    /// [`Self::unresolved_by_src`]. Unresolved rows are, by graph-domain
    /// convention, the minority (ghost targets); the resolved structures —
    /// which dominate at the design bound — are spliced in place. The
    /// section rebuild is O(U log U) since RFC-0005 F2 (first-visit
    /// membership rides a seen-set; the scan it replaced re-walked
    /// `nodes` per occurrence, which was O(U²) for every delta).
    fn rebuild_unresolved(&mut self) {
        let mut nodes: Vec<GraphNodeRecord> = Vec::new();
        let mut edges: Vec<GraphEdgeRecord> = Vec::new();
        let mut keys: BTreeMap<u64, String> = BTreeMap::new();
        // F2: target ids already pushed into `nodes`. Membership via
        // `insert` (true = first visit) replaces the former
        // `!nodes.iter().any(|node| node.id.id == id)` re-scan. Push
        // order is unchanged — statement order, first spelling of a
        // target wins — and the section is re-sorted canonically at the
        // end of this function regardless, which is what makes the
        // set rewrite output-identical (FR-10; the
        // `graph_mirror_equivalence` proptest is the byte-equality
        // oracle over exactly this path).
        let mut pushed: BTreeSet<u64> = BTreeSet::new();
        let boundary = self.resolved_occurrences;
        self.view.occurrences.truncate(boundary);
        let unresolved = std::mem::take(&mut self.unresolved_by_src);
        for (src, list) in &unresolved {
            let Some(entry) = self.view.node_position(node_id_of(*src)) else {
                continue;
            };
            let from = node_id_of(*src);
            let source_path = self.view.saved.nodes[entry].native_path.clone();
            let content_hash = self.source_hashes.get(src).cloned().unwrap_or_default();
            for occurrence in list {
                let id = unresolved_id_for(&occurrence.raw);
                let key = flavite_syntax::text::fold_key(&occurrence.raw);
                keys.insert(id, key);
                if pushed.insert(id) {
                    nodes.push(unresolved_record(id, &occurrence.raw));
                }
                let edge = GraphEdgeRecord {
                    from,
                    to: NodeId::unresolved(id),
                    kind: occurrence.kind,
                };
                edges.push(edge);
                self.view.occurrences.push(LinkOccurrence {
                    edge,
                    source_path: source_path.clone(),
                    source: OccurrenceSource::Saved {
                        content_hash: content_hash.clone(),
                    },
                    start: occurrence.start,
                    end: occurrence.end,
                    target_raw: occurrence.raw.clone(),
                });
            }
        }
        self.unresolved_by_src = unresolved;
        // F2 honesty: this canonical sort is why first-visit push order
        // (statement order) is not an observable — the sections' final
        // shape depends only on membership, and membership is exactly
        // the set of targets seen in the scan.
        nodes.sort_by_key(|record| record.id);
        edges.sort();
        edges.dedup();
        self.view.unresolved_nodes = nodes;
        self.view.unresolved_edges = edges;
        self.view.unresolved_keys = keys;
    }
}

impl GraphMirror {
    /// A cold mirror: the first [`Self::synchronize`] reports
    /// [`Synchronize::Reset`] — service start is a reset (§6.5.2).
    #[must_use]
    pub fn new(session: Option<Arc<Session>>) -> Self {
        Self::with_parity_policy(session, ParityPolicy::default())
    }

    /// Measurement seam: the same mirror with a tightened drift guard
    /// (tests and benches only — production uses [`Self::new`], the same
    /// pattern as `IndexActor::with_resolver_backstop`).
    #[must_use]
    pub fn with_parity_policy(session: Option<Arc<Session>>, policy: ParityPolicy) -> Self {
        Self {
            session,
            policy,
            // Epoch 0 matches no real database (INITIAL_EPOCH is 1), so a
            // cold mirror can only ever synchronize into a Reset.
            cursor: IndexVersion::new(0, 0),
            state: None,
            poisoned: false,
            delta_generations_since_parity: 0,
            last_parity: Instant::now(),
            stats: MirrorStats::default(),
        }
    }

    #[must_use]
    pub const fn stats(&self) -> MirrorStats {
        self.stats
    }

    #[must_use]
    pub const fn cursor(&self) -> IndexVersion {
        self.cursor
    }

    /// The canonical complete saved view at the cursor, if the mirror holds
    /// a position. `UpToDate` pumps with changed buffers/scopes rebuild the
    /// projection from this without reading the saved tables at all.
    #[must_use]
    pub fn view(&self) -> Option<&SavedGraph> {
        self.state.as_ref().map(|state| &state.view)
    }

    /// Whether the delta machinery failed mid-apply (or a freshly spliced
    /// row violated the change-log contract): the next synchronize forces a
    /// full rebuild. The service checks this right after a delta, before
    /// publishing, so suspect output is never displayed.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Whether the drift guard must run now (§6.5.4). Queried once per pump
    /// inside the same service loop — no dedicated thread.
    #[must_use]
    pub fn parity_due(&self, now: Instant) -> bool {
        if self.state.is_none() || self.poisoned {
            return false;
        }
        self.delta_generations_since_parity >= self.policy.generations
            || (self.delta_generations_since_parity > 0
                && now.duration_since(self.last_parity) >= self.policy.interval)
    }

    /// Advance the mirror by the change feed in ONE read transaction: meta
    /// → `Reset`/`UpToDate` fast paths, else [`ChangeLogReader`] rows, the
    /// per-entity point reads, and the splices, all on the same snapshot
    /// (§4.6 — a consumer that read the log in one transaction and the
    /// entity rows in the next could splice a torn generation).
    ///
    /// State mutates in place; any failure poisons the mirror so the next
    /// synchronize forces a full rebuild (§4.7: drop, never invert).
    ///
    /// # Errors
    /// Returns the failure text when the feed or a point read fails.
    pub fn synchronize(&mut self, conn: &Connection) -> Result<Synchronize, String> {
        if self.poisoned {
            self.force_reset(conn)?;
            return Ok(Synchronize::Delta {
                version: self.cursor,
                entities: 0,
                reset: true,
            });
        }
        let tx = conn
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let current = ChangeLogReader::current_cursor(&tx)?;
        if self.state.is_none() || self.cursor.epoch != current.epoch {
            drop(tx);
            return Ok(Synchronize::Reset {
                reason: ResetReason::EpochChanged,
            });
        }
        if self.cursor.generation == current.generation {
            drop(tx);
            return Ok(Synchronize::UpToDate);
        }
        let ChangeLogRead::Delta { records, to } = ChangeLogReader::read_since(&tx, self.cursor)?
        else {
            drop(tx);
            return Ok(Synchronize::Reset {
                reason: ResetReason::BelowLogFloor,
            });
        };
        let state = self.state.as_mut().ok_or("mirror state missing")?;
        let outcome = state.apply_delta(&tx, &records, to, self.session.as_deref());
        match outcome {
            Ok(entities) => {
                tx.commit().map_err(|error| error.to_string())?;
                let drift = state.drift;
                let unresolved_targets = state.view.unresolved_nodes.len();
                let previous_generation = self.cursor.generation;
                self.cursor = to;
                self.delta_generations_since_parity = self
                    .delta_generations_since_parity
                    .saturating_add(to.generation.saturating_sub(previous_generation));
                self.stats.deltas_total += 1;
                self.stats.last_delta_entities = entities;
                self.stats.unresolved_targets =
                    u64::try_from(unresolved_targets).unwrap_or(u64::MAX);
                if drift {
                    self.stats.mismatches_total += 1;
                    self.poisoned = true;
                }
                Ok(Synchronize::Delta {
                    version: to,
                    entities,
                    reset: false,
                })
            }
            Err(error) => {
                drop(tx);
                self.poisoned = true;
                Err(error)
            }
        }
    }

    /// The §6.5.2 reset path: the full consistent read (existing functions,
    /// reused verbatim) installed as the canonical view. Afterwards the
    /// complete view is readable through [`Self::view`].
    ///
    /// # Errors
    /// Returns the failure text when the full read fails.
    pub fn force_reset(&mut self, conn: &Connection) -> Result<IndexVersion, String> {
        let graph = full_saved_graph(conn, self.session.as_deref())?;
        let version = graph.version;
        let unresolved_targets = graph.unresolved_nodes.len();
        self.state = Some(MirrorState::from_view(graph));
        self.cursor = version;
        self.poisoned = false;
        self.delta_generations_since_parity = 0;
        self.last_parity = Instant::now();
        self.stats.resets_total += 1;
        self.stats.unresolved_targets = u64::try_from(unresolved_targets).unwrap_or(u64::MAX);
        Ok(version)
    }

    /// §6.5.4 parity comparison: re-read the full consistent snapshot and
    /// compare it against the mirror view. The writer never blocks for a
    /// read-only consumer, so the guard catches up a bounded number of
    /// rounds and renders a verdict only at a version both sides share.
    /// A cold or poisoned mirror rebuilds first (the rebuild IS a consistent
    /// full read, so its verdict is matched by construction).
    ///
    /// # Errors
    /// Returns the failure text when the full read or a catch-up fails.
    pub fn parity_check(&mut self, conn: &Connection) -> Result<ParityOutcome, String> {
        if self.state.is_none() || self.poisoned {
            self.force_reset(conn)?;
        }
        for _ in 0..4 {
            let mirror_version = self.cursor;
            let full = full_saved_graph(conn, self.session.as_deref())?;
            if full.version == mirror_version {
                let findings = parity_findings(
                    &full,
                    self.view()
                        .ok_or("mirror has no assembled view to compare against")?,
                );
                self.stats.parity_checks_total += 1;
                if findings.is_empty() {
                    self.delta_generations_since_parity = 0;
                    self.last_parity = Instant::now();
                } else {
                    self.stats.mismatches_total += 1;
                }
                return Ok(ParityOutcome {
                    version: full.version,
                    findings,
                });
            }
            // The writer moved ahead of the mirror: advance, then compare
            // again at the new shared version.
            let _ = self.synchronize(conn)?;
        }
        Err("graph mirror parity could not settle against the live index".into())
    }
}

// ---------------------------------------------------------------------------
// Point-read SQL and row shapes
// ---------------------------------------------------------------------------

/// The graph bridge's note row shape — `read_current`'s exact columns.
const NOTE_POINT_READ: &str =
    "SELECT note_id, path, path_native, title, stable_id, id_conflict FROM notes WHERE note_id=?1";
const ATTACHMENT_POINT_READ: &str =
    "SELECT att_id, path, path_native FROM attachments WHERE att_id=?1";
const LINKS_BY_SRC: &str = "SELECT l.src, l.kind, l.dst_note, l.dst_att, l.dst_raw, l.span_start, l.span_end, n.content_hash FROM links l JOIN notes n ON n.note_id = l.src WHERE l.src = ?1 ORDER BY l.ord";
const TAGS_BY_NOTE: &str = "SELECT tag FROM tags WHERE note_id=?1";
const PROPERTIES_BY_NOTE: &str =
    "SELECT key, vtype, v_text, v_num FROM properties WHERE note_id=?1 ORDER BY key, ord";

struct NoteRow {
    id: i64,
    path: String,
    path_native: Vec<u8>,
    title: String,
    stable_id: Option<String>,
    id_conflict: bool,
}

impl NoteRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            path: row.get(1)?,
            path_native: row.get(2)?,
            title: row.get(3)?,
            stable_id: row.get(4)?,
            id_conflict: row.get::<_, i64>(5)? != 0,
        })
    }

    /// The exact `read_current` note record construction.
    fn record(&self) -> GraphNodeRecord {
        GraphNodeRecord::note(
            node_row_id(self.id),
            self.path.clone(),
            self.path_native.clone(),
            self.title.clone(),
            self.stable_id.clone(),
            self.id_conflict,
        )
    }
}

struct AttachmentRow {
    id: i64,
    path: String,
    path_native: Vec<u8>,
}

impl AttachmentRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            path: row.get(1)?,
            path_native: row.get(2)?,
        })
    }

    /// The exact `read_current` attachment record construction.
    fn record(&self) -> GraphNodeRecord {
        GraphNodeRecord::attachment(
            node_row_id(self.id),
            self.path.clone(),
            self.path_native.clone(),
        )
    }
}

struct LinkRow {
    kind: String,
    dst_note: Option<i64>,
    dst_att: Option<i64>,
    dst_raw: String,
    start: usize,
    end: usize,
    content_hash: Vec<u8>,
}

fn read_links(link_stmt: &mut rusqlite::Statement<'_>, id: i64) -> Result<Vec<LinkRow>, String> {
    link_stmt
        .query_map([id], |row| {
            Ok(LinkRow {
                kind: row.get(1)?,
                dst_note: row.get(2)?,
                dst_att: row.get(3)?,
                dst_raw: row.get(4)?,
                start: row.get::<_, i64>(5)? as usize,
                end: row.get::<_, i64>(6)? as usize,
                content_hash: row.get(7)?,
            })
        })
        .map_err(|error| error.to_string())?
        .collect::<Result<Vec<LinkRow>, _>>()
        .map_err(|error| error.to_string())
}

/// Tags + properties for exactly one note, shaped identically to the
/// `saved_metadata` scan.
fn read_note_metadata(
    tag_stmt: &mut rusqlite::Statement<'_>,
    property_stmt: &mut rusqlite::Statement<'_>,
    id: i64,
) -> Result<NoteMetadata, String> {
    let tags = tag_stmt
        .query_map([id], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<Result<BTreeSet<String>, _>>()
        .map_err(|error| error.to_string())?;
    let mut properties: BTreeMap<String, Vec<flavite_graph::PropertyValue>> = BTreeMap::new();
    {
        let rows = property_stmt
            .query_map([id], |row| {
                let kind: String = row.get(1)?;
                let text: Option<String> = row.get(2)?;
                let number: Option<f64> = row.get(3)?;
                Ok((
                    row.get::<_, String>(0)?,
                    property_value(&kind, text, number),
                ))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            let (key, value) = row.map_err(|error| error.to_string())?;
            properties.entry(key).or_default().push(value);
        }
    }
    Ok(NoteMetadata { tags, properties })
}

fn property_value(
    kind: &str,
    text: Option<String>,
    number: Option<f64>,
) -> flavite_graph::PropertyValue {
    use flavite_graph::PropertyValue;
    match kind {
        "num" => PropertyValue::Number(number.map(|n| n.to_string()).unwrap_or_default()),
        "bool" => PropertyValue::Bool(number == Some(1.0) || text.as_deref() == Some("true")),
        "date" => PropertyValue::Date(text.unwrap_or_default()),
        "datetime" => PropertyValue::DateTime(text.unwrap_or_default()),
        "null" => PropertyValue::Null,
        _ => PropertyValue::Text(text.unwrap_or_default()),
    }
}

fn node_id_entity(id: NodeId) -> i64 {
    i64::try_from(id.id).unwrap_or(-1)
}

/// RFC-0005 §7 identity helper — one sentinel policy for every row id
/// entering the mirror view.
///
/// Note and attachment ids are ROWID-allocated positive integers
/// (`note_id`/`att_id` are `INTEGER PRIMARY KEY`); a negative id cannot
/// exist in a database this writer created. The 0 sentinel is therefore
/// unreachable and exists only to keep the conversion total: a corrupt
/// negative row id collapses to node 0 rather than panicking the pump,
/// and the parity guard still surfaces the drift against the full truth.
/// One policy, written once — the former scattered
/// `u64::try_from(..).unwrap_or(0)` expressions were this same rule
/// repeated per call site.
#[inline]
fn node_row_id(id: i64) -> u64 {
    u64::try_from(id).unwrap_or(0)
}

/// The [`NodeId`] of a note row (first call shape; sentinel policy in
/// [`node_row_id`]).
#[inline]
fn node_id_of(id: i64) -> NodeId {
    NodeId::note(node_row_id(id))
}

/// The [`NodeId`] of an attachment row (second call shape over the same
/// policy, RFC-0005 §7).
#[inline]
fn attachment_id_of(id: i64) -> NodeId {
    NodeId::attachment(node_row_id(id))
}

/// Derive the per-source unresolved inputs from the canonical unresolved
/// occurrence section.
fn unresolved_by_source(section: &[LinkOccurrence]) -> BTreeMap<i64, Vec<UnresolvedOcc>> {
    let mut map: BTreeMap<i64, Vec<UnresolvedOcc>> = BTreeMap::new();
    for occurrence in section {
        map.entry(node_id_entity(occurrence.edge.from))
            .or_default()
            .push(UnresolvedOcc {
                raw: occurrence.target_raw.clone(),
                start: occurrence.start,
                end: occurrence.end,
                kind: occurrence.edge.kind,
            });
    }
    map
}

/// Derive the per-source provenance hash from the canonical occurrence
/// vector (every section shares the source row's `content_hash`).
fn source_hashes(occurrences: &[LinkOccurrence]) -> BTreeMap<i64, Vec<u8>> {
    let mut map: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
    for occurrence in occurrences {
        if let OccurrenceSource::Saved { content_hash } = &occurrence.source {
            map.entry(node_id_entity(occurrence.edge.from))
                .or_insert_with(|| content_hash.clone());
        }
    }
    map
}

/// The bridge's native-path decoding, verbatim from the pre-S6 `build`
/// step: session-backed keys only, exact vault-relative components, and
/// the same typed refusals. Applied per-entity on the delta path and to
/// the complete batch on the full path, so both producers' records are
/// decoded identically.
fn decode_records(
    session: Option<&Session>,
    records: &mut [GraphNodeRecord],
) -> Result<(), String> {
    let Some(session) = session else {
        return Ok(());
    };
    if records.is_empty() {
        // Never pay a vault-actor round-trip for nothing.
        return Ok(());
    }
    let decoded =
        session.decode_graph_paths(records.iter().map(|n| n.path_native.clone()).collect())?;
    if decoded.len() != records.len() {
        return Err("graph native path catalog is incomplete".into());
    }
    for (record, native) in records.iter_mut().zip(decoded) {
        if native.is_absolute()
            || native
                .components()
                .any(|c| !matches!(c, Component::Normal(_)))
        {
            return Err("graph native path is outside the vault".into());
        }
        record.native_path = native;
    }
    Ok(())
}

/// The unresolved identity derivation, shared by the mirror and the
/// projection overlay so both produce the same ids and records.
fn unresolved_id_for(raw: &str) -> u64 {
    let key = flavite_syntax::text::fold_key(raw);
    let hash = blake3::hash(key.as_bytes());
    u64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("hash prefix"))
}

fn unresolved_record(id: u64, raw: &str) -> GraphNodeRecord {
    let mut record = GraphNodeRecord::note(
        id,
        "",
        Vec::new(),
        format!("[[{raw}]] (unresolved)"),
        None,
        false,
    );
    record.id = NodeId::unresolved(id);
    record
}

/// The unresolved identity guard the projection overlay uses for buffer
/// targets (`graph_projection::build`, moved here so the mirror splices,
/// the full pipeline, and the overlay share one derivation): the same
/// `blake3`-prefix ids and the same collision refusal, pre-S6 verbatim.
pub(crate) fn unresolved_node(
    batch: &mut GraphBatch,
    ids: &mut BTreeMap<u64, String>,
    raw: &str,
) -> Result<NodeId, String> {
    let key = flavite_syntax::text::fold_key(raw);
    let id = unresolved_id_for(raw);
    if let Some(previous) = ids.get(&id) {
        if previous != &key {
            return Err("Unresolved target identity collision; edit the source".into());
        }
        return Ok(NodeId::unresolved(id));
    }
    ids.insert(id, key);
    batch.nodes.push(unresolved_record(id, raw));
    Ok(NodeId::unresolved(id))
}

// ---------------------------------------------------------------------------
// Full pipeline — the AC-3 oracle and the §6.5.2 reset path
// ---------------------------------------------------------------------------

/// The complete saved view at the current index version: `read_current`
/// (reused verbatim), the same native-path decoding the pre-S6 bridge
/// applied, the same occurrence scans, and the same metadata read. This is
/// the equivalence oracle and the rebuild path in one function — which is
/// the parity guarantee made structural, not just tested.
///
/// # Errors
/// Returns the failure text when the transaction or any scan fails.
pub fn full_saved_graph(
    conn: &Connection,
    session: Option<&Session>,
) -> Result<SavedGraph, String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let version = ChangeLogReader::current_cursor(&tx)?;
    let mut saved = crate::graph_service::read_current(&tx, version.epoch, version.generation)
        .map_err(|_| "cannot read graph base")?;
    decode_records(session, &mut saved.nodes)?;
    let mut effective = saved.clone();
    let mut occurrences = saved_occurrences(&tx, &saved)?;
    let mut unresolved_ids = BTreeMap::new();
    add_saved_unresolved(&tx, &mut effective, &mut occurrences, &mut unresolved_ids)?;
    let metadata = saved_metadata(&tx)?;
    tx.commit().map_err(|error| error.to_string())?;
    let mut unresolved_nodes: Vec<GraphNodeRecord> = effective
        .nodes
        .iter()
        .filter(|node| node.id.kind == NodeKind::Unresolved)
        .cloned()
        .collect();
    unresolved_nodes.sort_by_key(|record| record.id);
    let mut unresolved_edges: Vec<GraphEdgeRecord> = effective
        .edges
        .iter()
        .filter(|edge| edge.to.kind == NodeKind::Unresolved)
        .copied()
        .collect();
    unresolved_edges.sort();
    unresolved_edges.dedup();
    Ok(SavedGraph {
        version,
        saved,
        unresolved_nodes,
        unresolved_edges,
        occurrences,
        metadata,
        unresolved_keys: unresolved_ids,
    })
}

/// Saved resolved occurrences — the pre-S6 `graph_projection` scan with one
/// deliberate improvement: the per-row source lookup uses an id → path
/// index instead of the original linear `batch.nodes.iter().find(...)`
/// (O(occurrences) not O(occurrences × vault); node ids are unique and
/// every source is a note, so the located value is identical). Reused
/// verbatim otherwise so the reset path and the parity oracle stay exact.
fn saved_occurrences(conn: &Connection, batch: &GraphBatch) -> Result<Vec<LinkOccurrence>, String> {
    let source_paths: BTreeMap<i64, PathBuf> = batch
        .nodes
        .iter()
        .filter(|node| node.id.kind == NodeKind::Note)
        .map(|node| (node_id_entity(node.id), node.native_path.clone()))
        .collect();
    let mut stmt = conn.prepare("SELECT l.src,l.kind,l.dst_note,l.dst_att,l.span_start,l.span_end,l.dst_raw,n.content_hash FROM links l JOIN notes n ON n.note_id=l.src WHERE l.dst_note IS NOT NULL OR l.dst_att IS NOT NULL ORDER BY l.src,l.ord").map_err(|_| "cannot read occurrences")?;
    stmt.query_map([], |r| {
        let from = NodeId::note(r.get::<_, i64>(0)? as u64);
        let note: Option<i64> = r.get(2)?;
        let att: Option<i64> = r.get(3)?;
        Ok(LinkOccurrence {
            edge: GraphEdgeRecord {
                from,
                to: note.map_or_else(
                    || NodeId::attachment(att.expect("resolved occurrence") as u64),
                    |id| NodeId::note(id as u64),
                ),
                kind: if r.get::<_, String>(1)? == "link" {
                    EdgeKind::Link
                } else {
                    EdgeKind::Embed
                },
            },
            source_path: source_paths
                .get(&node_id_entity(from))
                .cloned()
                .unwrap_or_default(),
            source: OccurrenceSource::Saved {
                content_hash: r.get(7)?,
            },
            start: r.get::<_, i64>(4)? as usize,
            end: r.get::<_, i64>(5)? as usize,
            target_raw: r.get(6)?,
        })
    })
    .map_err(|_| "cannot read occurrences")?
    .collect::<Result<_, _>>()
    .map_err(|_| "invalid occurrence record".into())
}

/// Saved unresolved targets — the pre-S6 `graph_projection` scan, moved
/// with the same source-path index improvement as [`saved_occurrences`]
/// (identical located values: every source is a note with a unique id);
/// identity derivation is [`unresolved_node`], shared with the mirror and
/// the overlay.
fn add_saved_unresolved(
    conn: &Connection,
    batch: &mut GraphBatch,
    occurrences: &mut Vec<LinkOccurrence>,
    ids: &mut BTreeMap<u64, String>,
) -> Result<(), String> {
    let source_paths: BTreeMap<i64, PathBuf> = batch
        .nodes
        .iter()
        .filter(|node| node.id.kind == NodeKind::Note)
        .map(|node| (node_id_entity(node.id), node.native_path.clone()))
        .collect();
    let mut stmt = conn.prepare("SELECT l.src,l.kind,l.dst_raw,l.span_start,l.span_end,n.content_hash FROM links l JOIN notes n ON n.note_id=l.src WHERE l.dst_note IS NULL AND l.dst_att IS NULL ORDER BY l.src,l.ord").map_err(|_| "Cannot read unresolved targets")?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Vec<u8>>(5)?,
            ))
        })
        .map_err(|_| "Cannot read unresolved targets")?;
    for row in rows {
        let (source, kind, raw, start, end, hash) = row.map_err(|_| "Invalid unresolved target")?;
        let from = NodeId::note(source as u64);
        let source_path = source_paths
            .get(&source)
            .ok_or("Missing unresolved source")?
            .clone();
        let to = unresolved_node(batch, ids, &raw)?;
        let edge = GraphEdgeRecord {
            from,
            to,
            kind: if kind == "embed" {
                EdgeKind::Embed
            } else {
                EdgeKind::Link
            },
        };
        batch.edges.push(edge);
        occurrences.push(LinkOccurrence {
            edge,
            source_path,
            source: OccurrenceSource::Saved { content_hash: hash },
            start: start as usize,
            end: end as usize,
            target_raw: raw,
        });
    }
    Ok(())
}

/// Saved note metadata — the pre-S6 `graph_projection` scan, moved verbatim.
fn saved_metadata(conn: &Connection) -> Result<BTreeMap<NodeId, NoteMetadata>, String> {
    let mut metadata: BTreeMap<NodeId, NoteMetadata> = BTreeMap::new();
    let mut stmt = conn
        .prepare("SELECT note_id,tag FROM tags")
        .map_err(|_| "cannot read tags")?;
    for row in stmt
        .query_map([], |r| {
            Ok((
                NodeId::note(r.get::<_, i64>(0)? as u64),
                r.get::<_, String>(1)?,
            ))
        })
        .map_err(|_| "cannot read tags")?
    {
        let (id, tag) = row.map_err(|_| "invalid tag record")?;
        metadata.entry(id).or_default().tags.insert(tag);
    }
    let mut stmt = conn
        .prepare("SELECT note_id,key,vtype,v_text,v_num FROM properties ORDER BY note_id,key,ord")
        .map_err(|_| "cannot read properties")?;
    for row in stmt
        .query_map([], |r| {
            let kind: String = r.get(2)?;
            let text: Option<String> = r.get(3)?;
            let number: Option<f64> = r.get(4)?;
            Ok((
                NodeId::note(r.get::<_, i64>(0)? as u64),
                r.get::<_, String>(1)?,
                property_value(&kind, text, number),
            ))
        })
        .map_err(|_| "cannot read properties")?
    {
        let (id, key, value) = row.map_err(|_| "invalid property record")?;
        metadata
            .entry(id)
            .or_default()
            .properties
            .entry(key)
            .or_default()
            .push(value);
    }
    Ok(metadata)
}

// ---------------------------------------------------------------------------
// Canonical comparison (D8, §6.5.4)
// ---------------------------------------------------------------------------

const PARITY_SAMPLE_LIMIT: usize = 4;

/// Compare two canonical views: nodes by id, edges as sets, occurrence
/// counts per source, metadata per note (plus exact record equality, which
/// the canonical order makes byte-comparable). Findings are bounded — this
/// is a drift detector, not a data pipe. Public because the equivalence
/// gate asserts through it, so test and guard share one comparison.
pub fn parity_findings(full: &SavedGraph, mirrored: &SavedGraph) -> Vec<String> {
    let mut findings = Vec::new();
    if full.version != mirrored.version {
        findings.push(format!(
            "version: full=({}, {}) mirrored=({}, {})",
            full.version.epoch,
            full.version.generation,
            mirrored.version.epoch,
            mirrored.version.generation
        ));
    }
    diff_node_list(
        "node",
        &full.saved.nodes,
        &mirrored.saved.nodes,
        &mut findings,
    );
    diff_node_list(
        "unresolved node",
        &full.unresolved_nodes,
        &mirrored.unresolved_nodes,
        &mut findings,
    );
    diff_edge_list(
        "edge",
        &full.saved.edges,
        &mirrored.saved.edges,
        &mut findings,
    );
    diff_edge_list(
        "unresolved edge",
        &full.unresolved_edges,
        &mirrored.unresolved_edges,
        &mut findings,
    );
    diff_occurrences(&full.occurrences, &mirrored.occurrences, &mut findings);
    diff_metadata(&full.metadata, &mirrored.metadata, &mut findings);
    findings
}

fn describe_node(record: &GraphNodeRecord) -> String {
    format!(
        "id={:?} path={:?} title={:?} stable_id={:?} conflict={}",
        record.id, record.path, record.title, record.stable_id, record.id_conflict
    )
}

fn diff_node_list(
    what: &str,
    full: &[GraphNodeRecord],
    mirrored: &[GraphNodeRecord],
    findings: &mut Vec<String>,
) {
    if full == mirrored {
        return;
    }
    let mut sample = format!(
        "{what}: full={} mirrored={} record(s) differ",
        full.len(),
        mirrored.len()
    );
    let mut shown = 0usize;
    for (index, (left, right)) in full.iter().zip(mirrored.iter()).enumerate() {
        if left != right {
            sample.push_str(&format!(
                " [#{index} full={} mirrored={}]",
                describe_node(left),
                describe_node(right)
            ));
            shown += 1;
            if shown >= PARITY_SAMPLE_LIMIT {
                break;
            }
        }
    }
    for extra in full.iter().skip(mirrored.len()).take(1) {
        sample.push_str(&format!(" full-only={}", describe_node(extra)));
    }
    for extra in mirrored.iter().skip(full.len()).take(1) {
        sample.push_str(&format!(" mirrored-only={}", describe_node(extra)));
    }
    findings.push(sample);
}

fn diff_edge_list(
    what: &str,
    full: &[GraphEdgeRecord],
    mirrored: &[GraphEdgeRecord],
    findings: &mut Vec<String>,
) {
    if full == mirrored {
        return;
    }
    let set_full: BTreeSet<GraphEdgeRecord> = full.iter().copied().collect();
    let set_mirrored: BTreeSet<GraphEdgeRecord> = mirrored.iter().copied().collect();
    let missing: Vec<String> = set_full
        .difference(&set_mirrored)
        .take(PARITY_SAMPLE_LIMIT)
        .map(|edge| format!("{:?}->{:?}", edge.from, edge.to))
        .collect();
    let extra: Vec<String> = set_mirrored
        .difference(&set_full)
        .take(PARITY_SAMPLE_LIMIT)
        .map(|edge| format!("{:?}->{:?}", edge.from, edge.to))
        .collect();
    findings.push(format!(
        "{what}: full={} mirrored={} missing={missing:?} mirrored-extra={extra:?}",
        full.len(),
        mirrored.len()
    ));
}

/// Occurrences are compared exactly (canonical order makes element equality
/// byte-comparable); the per-source count is the human-readable locator.
fn diff_occurrences(
    full: &[LinkOccurrence],
    mirrored: &[LinkOccurrence],
    findings: &mut Vec<String>,
) {
    if full == mirrored {
        return;
    }
    let mut sample = format!(
        "occurrences: full={} mirrored={} — first differing entry:",
        full.len(),
        mirrored.len()
    );
    for (index, (left, right)) in full.iter().zip(mirrored.iter()).enumerate() {
        if left != right {
            sample.push_str(&format!(
                " #{index} src={:?} full(to={:?},raw={:?}) mirrored(to={:?},raw={:?})",
                left.edge.from, left.edge.to, left.target_raw, right.edge.to, right.target_raw,
            ));
            break;
        }
    }
    if full.len() != mirrored.len() {
        sample.push_str(&format!(
            " lengths differ at src {:?}",
            full.get(mirrored.len())
                .or(mirrored.get(full.len()))
                .map(|occurrence| occurrence.edge.from)
        ));
    }
    findings.push(sample);
}

fn diff_metadata(
    full: &BTreeMap<NodeId, NoteMetadata>,
    mirrored: &BTreeMap<NodeId, NoteMetadata>,
    findings: &mut Vec<String>,
) {
    if full == mirrored {
        return;
    }
    let mut sample = Vec::new();
    for (id, left) in full {
        match mirrored.get(id) {
            Some(right) if left != right => {
                sample.push(format!(
                    "{id:?}: full tags={:?} mirrored tags={:?}",
                    left.tags, right.tags
                ));
            }
            None => sample.push(format!("{id:?}: absent in mirror")),
            _ => {}
        }
        if sample.len() >= PARITY_SAMPLE_LIMIT {
            break;
        }
    }
    if sample.len() < PARITY_SAMPLE_LIMIT {
        for id in mirrored.keys() {
            if !full.contains_key(id) {
                sample.push(format!("{id:?}: absent in full"));
                if sample.len() >= PARITY_SAMPLE_LIMIT {
                    break;
                }
            }
        }
    }
    findings.push(format!(
        "metadata: full={} mirrored={} notes differ — {sample:?}",
        full.len(),
        mirrored.len()
    ));
}

// ---------------------------------------------------------------------------
// Unit tests (RFC-0005 S2)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    /// Seed source notes each carrying `ghosts` distinct unresolved links
    /// straight into the base tables, then advance generation once (the
    /// seeded-state convention of the scale/service fixtures). Returns the
    /// database connection positioned at generation 1.
    fn seed_ghost_matrix(conn: &Connection, sources: i64, ghosts: i64) {
        let tx = conn.unchecked_transaction().unwrap();
        {
            let mut note = tx
                .prepare(
                    "INSERT INTO notes(path, path_native, path_key, stem_key, title,
                                       id_conflict, content_hash, size, mtime_ns, scan_ns,
                                       parse_ok, generation)
                     VALUES(?1, ?2, ?3, ?4, ?5, 0, ?6, 64, 1, 1, 1, 1)",
                )
                .unwrap();
            let mut link = tx
                .prepare(
                    "INSERT INTO links(src, ord, kind, dst_raw, target_key, target_stem,
                                       span_start, span_end, res_state)
                     VALUES(?1, ?2, 'link', ?3, ?4, ?5, ?6, ?7, 'unresolved')",
                )
                .unwrap();
            for src in 1..=sources {
                let path = format!("src{src}.md");
                note.execute(params![
                    path,
                    path.as_bytes(),
                    flavite_syntax::text::fold_key(&path),
                    flavite_syntax::text::fold_key(&format!("src{src}")),
                    format!("Source {src}"),
                    blake3::hash(&src.to_le_bytes()).as_bytes().to_vec(),
                ])
                .unwrap();
            }
            for ghost in 0..sources * ghosts {
                // Spread deterministically: source k owns ghosts k, k+U, …
                let src = ghost % sources + 1;
                let ord = ghost / sources;
                let raw = format!("ghost{ghost}");
                let key = flavite_syntax::text::fold_key(&raw);
                link.execute(params![
                    src,
                    ord,
                    raw,
                    key,
                    key,
                    10 + ord * 100,
                    20 + ord * 100,
                ])
                .unwrap();
            }
        }
        tx.execute("UPDATE meta SET value='1' WHERE key='generation'", [])
            .unwrap();
        tx.commit().unwrap();
    }

    /// RFC-0005 F2 exit test: ≥1_000 distinct unresolved targets spread
    /// across sources, one delta splice through the real writer, and the
    /// rebuilt canonical sections must be complete, strictly sorted by
    /// [`NodeId`], unique, and byte-equal to a fresh full consistent read
    /// at the same version — the FR-10 oracle, same `parity_findings`
    /// comparison the drift guard and the equivalence proptest run. The
    /// O(U²) scan this replaced would also pass this test; the permanent
    /// regression teeth are the proptest (identity) plus the scale bench's
    /// printed `unresolved_targets` count (the measurement).
    #[test]
    fn delta_rebuild_over_a_thousand_unresolved_targets_is_exact() {
        const SOURCES: i64 = 4;
        const GHOSTS_PER_SOURCE: i64 = 256; // 1_024 distinct targets
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("dedup.sqlite");
        let reader = open_reader(&db);
        seed_ghost_matrix(&reader, SOURCES, GHOSTS_PER_SOURCE);

        let mut mirror = GraphMirror::new(None);
        assert!(
            matches!(
                mirror.synchronize(&reader).unwrap(),
                Synchronize::Reset { .. }
            ),
            "a cold mirror must take the reset path"
        );
        mirror.force_reset(&reader).unwrap();
        let baseline = mirror.view().expect("view").unresolved_nodes.len();
        assert_eq!(
            baseline,
            usize::try_from(SOURCES * GHOSTS_PER_SOURCE).unwrap(),
            "the reset rebuild must hold every seeded ghost"
        );
        assert!(baseline >= 1_000, "the exit gate needs ≥1_000 targets");
        assert_eq!(mirror.stats().unresolved_targets, baseline as u64);

        // One delta splice through the real apply spine: a brand-new
        // source note with a brand-new ghost target.
        let mut actor = flavite_index::IndexActor::new(&db, 1).unwrap();
        actor
            .apply_saved_note(
                b"src9.md",
                "src9.md",
                b"# Nine\nFresh ghost [[ghost-new]].\n",
                10,
                11,
            )
            .unwrap();
        let outcome = mirror.synchronize(&reader).unwrap();
        assert!(
            matches!(outcome, Synchronize::Delta { .. }),
            "the applied change must arrive as a delta, not a reset"
        );

        let view = mirror.view().expect("view");
        for pair in view.unresolved_nodes.windows(2) {
            assert!(
                pair[0].id < pair[1].id,
                "unresolved nodes must be strictly sorted by NodeId (unique + canonical)"
            );
        }
        assert_eq!(
            view.unresolved_nodes.len(),
            baseline + 1,
            "the new ghost lands and nothing else changed"
        );
        assert!(
            view.unresolved_keys
                .contains_key(&unresolved_id_for("ghost-new")),
            "the identity guard must cover the new target"
        );
        assert_eq!(
            mirror.stats().unresolved_targets,
            u64::try_from(baseline + 1).unwrap(),
            "the F2 telemetry counter must track the rebuild"
        );
        // Byte-equality oracle at the shared version (FR-10):
        let full = full_saved_graph(&reader, None).unwrap();
        assert_eq!(
            parity_findings(&full, view),
            Vec::<String>::new(),
            "the linear rebuild must equal the full truth byte-for-byte"
        );
    }

    /// A fresh schema at the path, WAL set, returned as the mirror's read
    /// handle (the actor and the seed share the file through separate
    /// connections, exactly like the service fixtures).
    fn open_reader(db: &std::path::Path) -> Connection {
        let conn = flavite_index::open_or_init(db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn
    }
}
