//! Production graph widget entry point (RFC-0003 W07, TD-10).
//!
//! The B6-era wrapper imported the probe's types and delegated its event
//! handling to them — production built on measurement scaffolding. W07
//! inverts that relationship correctly: everything real (WGSL, pipeline,
//! primitive, gesture program, identity-safe hit-testing) lives in
//! [`super::renderer`] and [`super::geometry`], and this module is only a
//! stable widget constructor for the graph tab plus the message alias the
//! view/host consume. The probe is now a consumer of the production core,
//! never the other way around.

use std::sync::Arc;

use iced::widget::shader::Shader;

use super::geometry::{HitNode, RenderGeometry, keys_pairable};
use super::instance::Camera;
use super::renderer::{GraphMessage, GraphProgram};

/// The typed message a mounted graph widget publishes to its host.
pub type ShaderViewMessage = GraphMessage;

/// Build the production `iced` shader widget for one graph view.
///
/// The `geometry` must have been built from a base publication and a
/// positions frame that agree on [`RenderKey`](flavite_graph::RenderKey)
/// — the host refuses to mount otherwise (§10.3); this function never
/// re-validates (single authority), it only projects.
#[must_use]
pub fn production_widget(
    geometry: Arc<RenderGeometry>,
    camera: Camera,
    scheme: crate::style::tokens::Scheme,
) -> Shader<GraphMessage, GraphProgram> {
    GraphProgram::new(geometry, camera, scheme).widget()
}

/// Whether two version keys may share one mounted frame. Hosts call this
/// when a worker result arrives late (discard-with-counter, §10.3).
#[must_use]
pub fn frame_keys_pairable(
    base: &flavite_graph::RenderKey,
    frame: &flavite_graph::RenderKey,
) -> bool {
    keys_pairable(base, frame)
}

/// Re-export so view code imports its display identity type from the
/// widget boundary, matching how note views import editor types.
pub type WidgetHitNode = HitNode;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::instance::{NodeInstance, NodeShape, note_color};
    use crate::graph::renderer::ReleaseKind;
    use crate::style::tokens::Scheme;
    use flavite_graph::{NodeId, NodeKind};

    #[test]
    fn production_widget_mounts_without_probe_dependencies() {
        // The widget path accepts a real geometry/camera pair and publishes
        // the same typed messages the renderer defines. (GPU construction
        // itself is validated by tests/shader_validation.rs; this proves
        // the wiring has no probe types in it.)
        let geometry = Arc::new(RenderGeometry::new(
            3,
            vec![NodeInstance {
                center: [1.0, 2.0],
                size: 4.0,
                shape: NodeShape::Circle,
                color: note_color(Scheme::Dark),
            }],
            vec![],
            vec![NodeId::note(9)],
        ));
        let _widget = production_widget(Arc::clone(&geometry), Camera::centered(), Scheme::Dark);
        assert_eq!(geometry.generation(), 3);
    }

    #[test]
    fn keys_pairable_agrees_with_topology_rules() {
        let base = flavite_graph::RenderKey::new(1, 7, 4);
        let same_topology_newer_base = flavite_graph::RenderKey::new(1, 8, 4);
        assert!(
            frame_keys_pairable(&base, &same_topology_newer_base),
            "a base generation advance without topology change keeps frames pairable"
        );
        assert!(
            !frame_keys_pairable(&base, &flavite_graph::RenderKey::new(2, 8, 4)),
            "a different epoch is never pairable"
        );
        assert!(
            !frame_keys_pairable(&base, &flavite_graph::RenderKey::new(1, 8, 5)),
            "a different topology version is never pairable"
        );
    }

    #[test]
    fn message_alias_carries_gesture_kinds() {
        let message = ShaderViewMessage::Released {
            node: Some(HitNode::new(NodeId::note(1), NodeKind::Note)),
            world: [0.0, 0.0],
            kind: ReleaseKind::Click,
        };
        match message {
            ShaderViewMessage::Released { node, kind, .. } => {
                assert_eq!(node.map(|hit| hit.id), Some(NodeId::note(1)));
                assert_eq!(kind, ReleaseKind::Click);
            }
            _ => panic!("release message"),
        }
    }
}
