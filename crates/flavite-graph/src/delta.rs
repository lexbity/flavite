//! Plain-data graph deltas — the interface between the index feed and the
//! graph model (D15).
//!
//! Deltas are pure data with no IO. The index feed (`GraphFeed`, B5) derives
//! them from §4.14 invalidation batches; [`GraphModel::apply`] consumes them.
//! Application is idempotent (D5, reconcile by idempotence): re-applying an
//! `Added` for an existing node or edge, or a `Removed` for an absent one, is
//! a no-op.
//!
//! [`GraphModel::apply`]: crate::GraphModel::apply

/// What a graph node represents.
///
/// Attachments are first-class graph citizens (D15, v2/R3): an `![[embed]]`
/// edge targets an attachment node, which renders as a square sized by
/// degree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    /// A note (`notes.note_id` in the index, §4.3).
    Note,
    /// An attachment (`attachments.att_id` in the index, §4.3).
    Attachment,
}

/// Stable identity of a graph node.
///
/// The index's cache-internal entity id paired with its kind. `note_id` and
/// `att_id` are separate id spaces (§4.3), so the kind is part of the
/// identity: note `7` and attachment `7` are different nodes. Ids are never
/// reused within an index epoch (§4.14); an epoch reset clears them and the
/// feed rebuilds the model from a full snapshot.
///
/// The derived `Ord` (kind, then id) is a deterministic total order used for
/// dense indexing, snapshot ordering, and hit-test tie-breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId {
    /// What the node represents.
    pub kind: NodeKind,
    /// Cache-internal entity id (`notes.note_id` / `attachments.att_id`).
    pub id: u64,
}

impl NodeId {
    /// A note node.
    #[must_use]
    pub fn note(id: u64) -> Self {
        Self {
            kind: NodeKind::Note,
            id,
        }
    }

    /// An attachment node.
    #[must_use]
    pub fn attachment(id: u64) -> Self {
        Self {
            kind: NodeKind::Attachment,
            id,
        }
    }
}

/// The kind of a graph edge (D15: embed edges are included in the graph).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeKind {
    /// A `[[link]]` (`links.kind = 'link'`, §4.3).
    Link,
    /// An `![[embed]]` (`links.kind = 'embed'`, §4.3).
    Embed,
}

/// A node-set change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeDelta {
    /// Add a node. Idempotent: adding an existing node is a no-op.
    Added {
        /// The node to add (its kind is part of its identity).
        id: NodeId,
    },
    /// Remove a node. Idempotent. Removing a node also removes every
    /// incident edge in both directions — the index's FK semantics
    /// (`links.src` `ON DELETE CASCADE`; `links.dst` `ON DELETE SET NULL`
    /// leaves the link unresolved — either way the edge leaves the graph,
    /// §4.3).
    Removed {
        /// The node to remove.
        id: NodeId,
    },
}

/// An edge-set change.
///
/// Edge identity is the `(from, to, kind)` triple: a pair that is both linked
/// and embedded carries two edges (one per kind). Multiple index rows for
/// the same triple — e.g. two `[[B]]` spans in one note — collapse to one
/// edge: the graph shows connectivity, not multiplicity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EdgeDelta {
    /// Add an edge. Idempotent. An edge whose endpoints are not in the node
    /// set is a stale hint, not an error: it is ignored (D5; the feed
    /// re-derives edge sets from current index rows, §4.14).
    Added {
        /// Source node.
        from: NodeId,
        /// Target node.
        to: NodeId,
        /// Link or embed.
        kind: EdgeKind,
    },
    /// Remove an edge. Idempotent.
    Removed {
        /// Source node.
        from: NodeId,
        /// Target node.
        to: NodeId,
        /// Link or embed.
        kind: EdgeKind,
    },
}

/// One graph mutation: a node or an edge delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Delta {
    /// A node-set change.
    Node(NodeDelta),
    /// An edge-set change.
    Edge(EdgeDelta),
}
