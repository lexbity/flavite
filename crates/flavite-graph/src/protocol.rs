//! IO-free graph protocol DTOs (RFC-0003 TD-12, W07).
//!
//! The index bridge lives in `flavite-api::graph_service`: it reads the
//! cache database and produces these plain records. The CPU graph crate
//! consumes them without any database, filesystem, or toolkit dependency
//! (§5.1: "Index bridge in API service, CPU graph consumes plain DTOs").
//!
//! - [`GraphBatch`] — one complete current view of the saved index: every
//!   live node (with its native identity and display data) and every live
//!   edge, stamped with the exact `(epoch, generation)` it was read at.
//!   A batch is never partial (§4.14).
//! - [`RenderKey`] — the version key that pairs a base frame with camera,
//!   geometry, positions, hit tests, and selections (§10.3). A worker or
//!   view result naming a mismatched key is discarded with a counter;
//!   dense indices are never interpreted through another frame.

use crate::delta::{Delta, EdgeKind, NodeId};

/// One graph node with its display data and durable identity inputs.
///
/// `path_native` is the exact index-native path bytes (authoritative
/// identity); `path` is the vault-relative display path the bridge read
/// alongside it. Pin keys derive from `stable_id`/`path_native` exactly as
/// the index established them — display strings are not identity (§7.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphNodeRecord {
    /// Graph node identity (kind plus cache entity id).
    pub id: NodeId,
    /// Vault-relative display path, `/`-separated.
    pub path: String,
    /// Exact native path bytes from the index.
    pub path_native: Vec<u8>,
    /// Native relative path decoded by the API storage authority.
    pub native_path: std::path::PathBuf,
    /// A replacement file shares this path with an orphaned open buffer.
    pub binding_blocked: bool,
    /// Current open source has been deleted externally.
    pub missing_on_disk: bool,
    /// Indexed title (notes) or final path component (attachments).
    pub title: String,
    /// Frontmatter stable ID when the note carries one.
    pub stable_id: Option<String>,
    /// Whether the stable ID is duplicated in this vault (conflicted IDs
    /// must never be used as durable identity, §4.12).
    pub id_conflict: bool,
}

impl GraphNodeRecord {
    /// A note node record.
    #[must_use]
    pub fn note(
        id: u64,
        path: impl Into<String>,
        path_native: Vec<u8>,
        title: impl Into<String>,
        stable_id: Option<String>,
        id_conflict: bool,
    ) -> Self {
        let path = path.into();
        Self {
            native_path: std::path::PathBuf::from(&path),
            binding_blocked: false,
            missing_on_disk: false,
            id: NodeId::note(id),
            path,
            path_native,
            title: title.into(),
            stable_id,
            id_conflict,
        }
    }

    /// An attachment node record.
    #[must_use]
    pub fn attachment(id: u64, path: impl Into<String>, path_native: Vec<u8>) -> Self {
        let path = path.into();
        let title = path
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .map_or_else(|| path.clone(), str::to_owned);
        Self {
            native_path: std::path::PathBuf::from(&path),
            binding_blocked: false,
            missing_on_disk: false,
            id: NodeId::attachment(id),
            stable_id: None,
            id_conflict: false,
            path,
            path_native,
            title,
        }
    }
}

/// One directed graph edge between live nodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphEdgeRecord {
    /// Source node.
    pub from: NodeId,
    /// Destination node.
    pub to: NodeId,
    /// Link or embed.
    pub kind: EdgeKind,
}

/// A complete current view of the saved index at one exact version.
///
/// The producer reads all live nodes and edges under one consistent index
/// transaction; the consumer diffs consecutive batches into exact deltas
/// (see [`crate::feed::GraphPublisher`]). Deletions survive in the batch
/// they disappear from — reconstruction from "what is still there" is what
/// makes removal correct (§4.14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphBatch {
    /// Index epoch the batch was read under.
    pub epoch: u64,
    /// Index generation the batch was read at.
    pub generation: u64,
    /// Every live node, sorted in [`NodeId`] order.
    pub nodes: Vec<GraphNodeRecord>,
    /// Every live edge, deduplicated by `(from, to, kind)`.
    pub edges: Vec<GraphEdgeRecord>,
}

impl GraphBatch {
    /// A new empty batch at one version stamp.
    #[must_use]
    pub fn empty(epoch: u64, generation: u64) -> Self {
        Self {
            epoch,
            generation,
            nodes: Vec::new(),
            edges: Vec::new(),
        }
    }
}

/// The version key that pairs every piece of one rendered graph frame.
///
/// Geometry, positions, hit-test identity tables, camera state, and gesture
/// candidates all carry the same key or they do not belong to one another
/// (§10.3). W08 extends this with the overlay digest and scope sequence;
/// the base frame of W07 is the saved index alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RenderKey {
    /// Base index epoch.
    pub epoch: u64,
    /// Base index generation.
    pub generation: u64,
    /// Topology version of the applied model frame.
    pub topology_version: u64,
    /// Current source revisions, independent of the saved index.
    pub overlay_digest: u64,
    /// Per-view scope request sequence.
    pub scope_sequence: u64,
}

impl RenderKey {
    /// A new key for one base publication.
    #[must_use]
    pub const fn new(epoch: u64, generation: u64, topology_version: u64) -> Self {
        Self {
            epoch,
            generation,
            topology_version,
            overlay_digest: 0,
            scope_sequence: 0,
        }
    }

    /// Stamp current source and per-view versions onto this base key.
    #[must_use]
    pub const fn with_view(mut self, overlay_digest: u64, scope_sequence: u64) -> Self {
        self.overlay_digest = overlay_digest;
        self.scope_sequence = scope_sequence;
        self
    }

    /// Whether this key names the same *topology* as a positions frame.
    /// A base generation advance without a topology change keeps positions
    /// valid; an epoch or topology change never does.
    #[must_use]
    pub fn topology_matches(&self, other: &Self) -> bool {
        self.epoch == other.epoch && self.topology_version == other.topology_version
    }
}

/// The exact deltas one batch transitions the previous model by, produced
/// by [`crate::feed::GraphPublisher`] (idempotent apply, D5).
pub type PublicationDeltas = Vec<Delta>;
