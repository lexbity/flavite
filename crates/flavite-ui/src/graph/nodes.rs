//! Kind-aware visual data for graph nodes (D15, B8).
//!
//! The renderer consumes this projection of a `GraphSnapshot`: note nodes
//! are circular dots, while attachments are square nodes. Size follows total
//! degree for both kinds, matching the instanced renderer's rule.

use flavite_graph::{GraphSnapshot, NodeId};

use super::instance::{NodeShape, color_of, node_radius};
use crate::style::tokens::Scheme;

/// Renderer-ready appearance for one node in a snapshot.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphNodeVisual {
    /// Stable within the snapshot's epoch.
    pub id: NodeId,
    /// Shape selected by entity kind.
    pub shape: NodeShape,
    /// World-space radius/half-width, scaled by degree.
    pub size: f32,
    /// RGBA color in linear space.
    pub color: [f32; 4],
    /// Total in-degree plus out-degree.
    pub degree: u32,
}

/// Build visuals in the snapshot's dense-node order. Colors come from the
/// active scheme's semantic tokens (W11/D-01), so a theme change recolors
/// the graph by rebuilding geometry — never by re-running the layout.
#[must_use]
pub fn node_visuals(snapshot: &GraphSnapshot, scheme: Scheme) -> Vec<GraphNodeVisual> {
    snapshot
        .node_ids
        .iter()
        .copied()
        .zip(snapshot.node_kinds.iter().copied())
        .zip(snapshot.degrees.iter().copied())
        .map(|((id, kind), degree)| GraphNodeVisual {
            id,
            shape: NodeShape::from(kind),
            size: node_radius(degree),
            color: color_of(kind, scheme),
            degree,
        })
        .collect()
}

/// Display a native relative attachment path as a user-facing filename.
/// Invalid UTF-8 is lossily rendered for display only; identity remains the
/// native path bytes held by the index and pin store.
#[must_use]
pub fn attachment_filename(display_path: &str) -> String {
    std::path::Path::new(display_path).file_name().map_or_else(
        || display_path.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_graph::{Delta, EdgeDelta, EdgeKind, GraphModel, NodeDelta};

    #[test]
    fn attachment_nodes_are_squares_and_degree_sized() {
        let note = NodeId::note(1);
        let attachment = NodeId::attachment(2);
        let mut graph = GraphModel::new();
        graph.apply(&[
            Delta::Node(NodeDelta::Added { id: note }),
            Delta::Node(NodeDelta::Added { id: attachment }),
            Delta::Edge(EdgeDelta::Added {
                from: note,
                to: attachment,
                kind: EdgeKind::Embed,
            }),
        ]);
        let snapshot = graph.snapshot();
        let visuals = node_visuals(&snapshot, Scheme::Dark);
        let attachment_visual = visuals
            .iter()
            .find(|visual| visual.id == attachment)
            .unwrap();
        assert_eq!(attachment_visual.shape, NodeShape::Square);
        assert_eq!(attachment_visual.degree, 1);
        assert_eq!(
            attachment_visual.color,
            crate::style::tokens::graph_attachment_color(Scheme::Dark)
        );
        assert_eq!(attachment_visual.size, node_radius(1));
    }

    #[test]
    fn attachment_filename_uses_only_the_final_component() {
        assert_eq!(attachment_filename("images/diagram.svg"), "diagram.svg");
        assert_eq!(attachment_filename("diagram.svg"), "diagram.svg");
    }
}
