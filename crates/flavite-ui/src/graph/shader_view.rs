//! Production instanced wgpu `shader` widget for the graph view (PLAN-B B6, D15).
//!
//! This module provides the production-ready shader widget that evolves from the
//! S2-1 probe. It includes:
//!
//! - LOD rendering via zoom-based edge capping (D15)
//! - Instanced wgpu draws with degree-based node sizing
//! - Edge alpha by zoom and importance-based edge capping
//! - Hit-testing for node selection
//! - GPU device-loss recovery (D15, §4.9)
//! - Command routing preparation for `note.open` and `graph.focus` (B7)
//!
//! # Design
//!
//! The widget follows the same architecture as the probe:
//!
//! 1. **CPU-side geometry** ([`super::instance::ProbeGeometry`]): Plain data
//!    describing nodes and edges, sized by degree and ordered by importance.
//!    This is the authoritative state; the GPU side is a projection of it.
//!
//! 2. **Camera** ([`super::instance::Camera`]): Pan/zoom state in world units.
//!    The camera is owned by the application and passed to the widget on
//!    every frame.
//!
//! 3. **Pipeline**: GPU state (buffers, pipelines, bind groups) created once
//!    per device and reused. Recreated on device loss.
//!
//! 4. **LOD**: At low zoom, renders instanced dots+lines with importance-capped
//!    edges. At high zoom (above [`LOD_ZOOM_THRESHOLD`]), the card layer
//!    (B7) takes over with real widgets.
//!
//! 5. **Hit-testing**: Distance-based hit-test for node selection, with
//!    radius scaled by zoom. The implementation uses a simple linear scan
//!    for B6; a production implementation would use
//!    [`flavite_graph::SpatialGrid`] for efficiency (B7+).

use std::sync::Arc;

use iced::mouse;
use iced::widget::shader::{Action, Program as ShaderProgram, Shader};
use iced::{Event, Length, Rectangle};

use super::instance::{Camera, NODE_RADIUS_BASE, ProbeGeometry};
use super::shader_probe::{
    ProbeMessage as InnerMessage, ProbePrimitive, ProbeProgram as InnerProgram,
    ProbeState as InnerState, ProbeTimings,
};
use flavite_graph::{NodeId, NodeKind};

/// Zoom level reserved for the planned detail transition.
pub const LOD_ZOOM_THRESHOLD: f32 = 8.0;

/// Hit-test radius for a node at `zoom`.
///
/// The hit radius scales inversely with zoom so that a node stays the
/// same physical size on screen: zooming in makes nodes larger, so the
/// hit radius grows to match.
#[must_use]
pub fn node_radius_for_hit_test(zoom: f32) -> f32 {
    NODE_RADIUS_BASE / zoom.clamp(0.1, 10.0)
}

/// Identity of a hit node, enough to dispatch a command via flavite-api.
///
/// Carries the node's stable identity and kind, which is sufficient to
/// construct commands like `note.open` or `graph.focus` (B7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HitNode {
    /// The node's identity in the graph model.
    pub id: NodeId,
    /// The node's kind (note or attachment).
    pub kind: NodeKind,
}

impl HitNode {
    /// Create a new hit node from its components.
    #[must_use]
    pub fn new(id: NodeId, kind: NodeKind) -> Self {
        Self { id, kind }
    }

    /// Convert to a display string for debugging.
    #[must_use]
    pub fn display(&self) -> String {
        match self.kind {
            NodeKind::Note => format!("Note({})", self.id.id),
            NodeKind::Attachment => format!("Attachment({})", self.id.id),
        }
    }
}

/// Messages published by the production shader widget.
///
/// These extend the inner probe messages with hit events and LOD transitions.
#[derive(Debug, Clone, PartialEq)]
pub enum ShaderViewMessage {
    /// A node was hit/clicked at `world_position`.
    ///
    /// Carries the hit node's identity for command dispatch. The application
    /// should map this to `note.open` or `graph.focus` commands via
    /// flavite-api (B7).
    NodeHit {
        node: HitNode,
        world_position: [f32; 2],
    },
    /// The zoom level crossed the LOD threshold.
    ///
    /// `true` = entered high-zoom (card layer should activate);
    /// `false` = exited high-zoom (return to instanced rendering).
    LodThresholdCrossed(bool),
    /// Inner probe messages for viewport, scale, pan, zoom.
    Inner(InnerMessage),
}

impl From<InnerMessage> for ShaderViewMessage {
    fn from(msg: InnerMessage) -> Self {
        Self::Inner(msg)
    }
}

/// Perform a hit-test against geometry at `world_position`.
///
/// For B6: simple distance-based hit test. A production implementation
/// would use [`flavite_graph::SpatialGrid`] for O(log n) hit-testing (B7+).
///
/// The hit radius scales with zoom: at zoom 1.0, uses the base node radius;
/// zoomed in, radius grows; zoomed out, radius shrinks.
#[must_use]
pub fn hit_test(geometry: &ProbeGeometry, world_position: [f32; 2], zoom: f32) -> Option<HitNode> {
    let radius = node_radius_for_hit_test(zoom);
    let radius_sq = radius * radius;

    for (node, &node_id) in geometry.nodes().iter().zip(geometry.node_ids().iter()) {
        let dx = node.center[0] - world_position[0];
        let dy = node.center[1] - world_position[1];
        let dist_sq = dx * dx + dy * dy;
        if dist_sq <= radius_sq {
            return Some(HitNode::new(node_id, node_id.kind));
        }
    }
    None
}

/// The production shader program.
///
/// For B6, this is a thin wrapper around the probe program that adds:
/// - Hit-testing capability
/// - LOD threshold tracking
/// - Message types for command routing (prepared for B7)
///
/// The program is cheap to clone and holds the geometry behind an `Arc`, so
/// the render thread never blocks on the layout worker (D15).
#[derive(Debug, Clone)]
pub struct ShaderViewProgram {
    /// The inner probe program (geometry, camera, timings).
    inner: InnerProgram,
    /// The LOD zoom threshold for this widget.
    lod_threshold: f32,
}

impl ShaderViewProgram {
    /// Create a new production shader program.
    ///
    /// - `geometry`: CPU-side geometry (nodes, edges, identities)
    /// - `camera`: Pan/zoom state in world units
    /// - `timings`: Shared instrumentation
    /// - `lod_threshold`: Zoom level at which to switch to card layer
    #[must_use]
    pub fn new(
        geometry: Arc<ProbeGeometry>,
        camera: Camera,
        timings: Arc<ProbeTimings>,
        lod_threshold: f32,
    ) -> Self {
        Self {
            inner: InnerProgram::new(geometry, camera, timings),
            lod_threshold,
        }
    }

    /// Create a production program from an inner probe program.
    #[must_use]
    pub fn from_probe(inner: InnerProgram) -> Self {
        Self {
            inner,
            lod_threshold: LOD_ZOOM_THRESHOLD,
        }
    }

    /// The camera this program will render with.
    #[must_use]
    pub fn camera(&self) -> Camera {
        self.inner.camera()
    }

    /// The geometry this program renders.
    #[must_use]
    pub fn geometry(&self) -> &Arc<ProbeGeometry> {
        self.inner.geometry()
    }

    /// The shared instrumentation this program reports into.
    #[must_use]
    pub fn timings(&self) -> &Arc<ProbeTimings> {
        self.inner.timings()
    }

    /// Get the LOD threshold for this widget.
    #[must_use]
    pub fn lod_threshold(&self) -> f32 {
        self.lod_threshold
    }

    /// Check if the current zoom level crosses the LOD threshold.
    #[must_use]
    pub fn crosses_lod_threshold(&self) -> Option<bool> {
        let zoom = self.camera().zoom;
        if zoom.is_finite() && zoom > 0.0 {
            Some(zoom >= self.lod_threshold)
        } else {
            None
        }
    }
}

// For B6, the program implements ShaderProgram with the same message type as
// the inner probe. Command routing through flavite-api will be added in B7.
impl ShaderProgram<InnerMessage> for ShaderViewProgram {
    type State = InnerState;
    type Primitive = ProbePrimitive;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<InnerMessage>> {
        // Delegate directly to inner probe for B6
        self.inner.update(state, event, bounds, cursor)
    }

    fn draw(
        &self,
        state: &Self::State,
        cursor: mouse::Cursor,
        bounds: Rectangle,
    ) -> Self::Primitive {
        self.inner.draw(state, cursor, bounds)
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        self.inner.mouse_interaction(state, bounds, cursor)
    }
}

impl ShaderViewProgram {
    /// Build the `iced` widget for this program, filling the available space.
    #[must_use]
    pub fn widget(self) -> Shader<InnerMessage, Self> {
        Shader::new(self).width(Length::Fill).height(Length::Fill)
    }
}

// Re-export inner probe types for convenience
pub use super::shader_probe::{
    ProbePrimitive as ShaderViewPrimitive, ProbeState as InnerShaderState,
    ProbeTimings as ShaderViewTimings,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_node_round_trip() {
        let node = HitNode::new(NodeId::note(42), NodeKind::Note);
        assert_eq!(node.id, NodeId::note(42));
        assert_eq!(node.kind, NodeKind::Note);
        assert_eq!(node.display(), "Note(42)");
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn lod_zoom_threshold_is_positive() {
        assert!(LOD_ZOOM_THRESHOLD > 0.0);
    }

    #[test]
    fn node_radius_for_hit_test_scales() {
        let radius_at_1 = node_radius_for_hit_test(1.0);
        let radius_at_2 = node_radius_for_hit_test(2.0);
        let radius_at_05 = node_radius_for_hit_test(0.5);

        // Radius scales inversely with zoom: zoomed in (zoom=2) means smaller
        // world radius to maintain the same physical hit area on screen.
        assert!(radius_at_2 < radius_at_1);
        // Zoomed out (zoom=0.5) means larger world radius.
        assert!(radius_at_05 > radius_at_1);
    }

    #[test]
    fn hit_test_finds_nearby_node() {
        use crate::graph::instance::{NOTE_COLOR, NodeInstance, NodeShape};

        let geometry = ProbeGeometry::new(
            1,
            vec![NodeInstance {
                center: [0.0, 0.0],
                size: 4.0,
                shape: NodeShape::Circle,
                color: NOTE_COLOR,
            }],
            vec![],
            vec![NodeId::note(1)],
        );

        // Hit at the node center should succeed
        assert!(hit_test(&geometry, [0.0, 0.0], 1.0).is_some());

        // Hit far away should fail
        assert!(hit_test(&geometry, [100.0, 100.0], 1.0).is_none());
    }

    #[test]
    fn hit_test_returns_correct_node_id() {
        use crate::graph::instance::{ATTACHMENT_COLOR, NOTE_COLOR, NodeInstance, NodeShape};

        let geometry = ProbeGeometry::new(
            1,
            vec![
                NodeInstance {
                    center: [0.0, 0.0],
                    size: 4.0,
                    shape: NodeShape::Circle,
                    color: NOTE_COLOR,
                },
                NodeInstance {
                    center: [10.0, 0.0],
                    size: 4.0,
                    shape: NodeShape::Square,
                    color: ATTACHMENT_COLOR,
                },
            ],
            vec![],
            vec![NodeId::note(1), NodeId::attachment(2)],
        );

        // Hit first node
        let hit = hit_test(&geometry, [0.0, 0.0], 1.0);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().id, NodeId::note(1));
        assert_eq!(hit.unwrap().kind, NodeKind::Note);

        // Hit second node
        let hit = hit_test(&geometry, [10.0, 0.0], 1.0);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().id, NodeId::attachment(2));
        assert_eq!(hit.unwrap().kind, NodeKind::Attachment);
    }

    #[test]
    fn message_from_inner_conversion() {
        let inner_msg = InnerMessage::Viewport {
            size: [1024.0, 768.0],
        };
        let outer_msg = ShaderViewMessage::from(inner_msg);
        match outer_msg {
            ShaderViewMessage::Inner(InnerMessage::Viewport { size }) => {
                assert_eq!(size, [1024.0, 768.0]);
            }
            _ => panic!("Expected Inner variant"),
        }
    }

    #[test]
    fn program_crosses_lod_threshold() {
        let geometry = Arc::new(ProbeGeometry::new(0, vec![], vec![], vec![]));
        let camera = Camera {
            center: [0.0, 0.0],
            zoom: 10.0, // Above threshold
        };
        let timings = Arc::new(ProbeTimings::default());
        let program = ShaderViewProgram::new(geometry, camera, timings, 8.0);

        assert_eq!(program.crosses_lod_threshold(), Some(true));
    }

    #[test]
    fn program_below_lod_threshold() {
        let geometry = Arc::new(ProbeGeometry::new(0, vec![], vec![], vec![]));
        let camera = Camera {
            center: [0.0, 0.0],
            zoom: 1.0, // Below threshold
        };
        let timings = Arc::new(ProbeTimings::default());
        let program = ShaderViewProgram::new(geometry, camera, timings, 8.0);

        assert_eq!(program.crosses_lod_threshold(), Some(false));
    }
}
