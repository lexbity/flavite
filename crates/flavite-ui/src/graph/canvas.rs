//! Software canvas graph backend (RFC-0003 §12.2, W07).
//!
//! When wgpu is unavailable (software rendering, device loss) the graph
//! still *draws*: nodes and edges are rasterized through `iced`'s canvas
//! geometry pipeline from the exact same [`RenderGeometry`] — same dense
//! identity table, same hit-test function, same published
//! [`GraphMessage`](super::renderer::GraphMessage) set — so both backends
//! accept the same input and no platform's evidence can be claimed by the
//! other. A failed renderer must leave the network operable; it is never
//! allowed to disable note editing (§12.2).

use std::sync::Arc;

use iced::widget::canvas::{self, Canvas, Path, Program};
use iced::widget::shader::Program as ShaderProgram;
use iced::{Color, Event, Point, Rectangle, Theme, mouse};

use super::geometry::RenderGeometry;
use super::instance::{Camera, edge_alpha_factor};
use super::renderer::{GraphMessage, GraphProgram, GraphProgramState};

/// A canvas program that mirrors the production shader backend: identical
/// geometry, identical camera, and event handling delegated to the *same*
/// [`GraphProgram`] implementation, so gesture messages cannot drift
/// between backends.
#[derive(Debug, Clone)]
pub struct GraphCanvas {
    program: GraphProgram,
}

impl GraphCanvas {
    /// The software twin of
    /// `GraphProgram::new(geometry, camera).widget()`.
    #[must_use]
    pub const fn new(geometry: Arc<RenderGeometry>, camera: Camera) -> Self {
        Self {
            program: GraphProgram::new(geometry, camera),
        }
    }

    /// Build the fillable canvas element.
    #[must_use]
    pub fn element(self) -> Canvas<Self, GraphMessage> {
        Canvas::new(self)
            .width(iced::Length::Fill)
            .height(iced::Length::Fill)
    }
}

impl Program<GraphMessage> for GraphCanvas {
    type State = GraphProgramState;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<canvas::Action<GraphMessage>> {
        // One gesture implementation, both backends (§9.3 honesty: the
        // software path accepts exactly the real input the GPU path does).
        self.program.update(state, event, bounds, cursor)
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &iced::Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let geometry = self.program.geometry();
        let camera = self.program.camera();
        let zoom = camera.zoom.max(0.001);
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        let to_screen = |world: [f32; 2]| {
            let screen = camera.world_to_screen(world, [0.0, 0.0], [bounds.width, bounds.height]);
            Point::new(screen[0], screen[1])
        };

        // Edges first, under the importance cap exactly as the GPU draws
        // the prefix, with the same alpha model (D15).
        let alpha_factor = edge_alpha_factor(zoom);
        let edge_count = geometry.visible_edge_count(zoom);
        for edge in geometry.edges().iter().take(edge_count) {
            let (a, b) = (to_screen(edge.from), to_screen(edge.to));
            if a.x.abs() > 1.0e7 || b.x.abs() > 1.0e7 {
                continue;
            }
            let alpha = (edge.alpha * alpha_factor).clamp(0.02, 1.0);
            let path = Path::line(a, b);
            frame.stroke(
                &path,
                canvas::Stroke::default()
                    .with_color(Color {
                        a: alpha,
                        ..EDGE_COLOR_CANVAS
                    })
                    .with_width(edge.width.max(0.5)),
            );
        }

        // Nodes: circles for notes, squares for attachments; selection and
        // writing-target accents are already baked in the instance colors.
        for node in geometry.nodes() {
            let center = to_screen(node.center);
            if !center.x.is_finite() || !center.y.is_finite() {
                continue;
            }
            let radius = node.size * zoom;
            if radius < 0.2 {
                continue; // sub-pixel: skip rather than smear
            }
            let color =
                Color::from_rgba(node.color[0], node.color[1], node.color[2], node.color[3]);
            if node.shape == super::instance::NodeShape::Circle {
                let path = Path::circle(center, radius.max(0.7));
                frame.fill(&path, color);
            } else {
                let path = Path::rectangle(
                    Point::new(center.x - radius, center.y - radius),
                    iced::Size::new((radius * 2.0).max(1.4), (radius * 2.0).max(1.4)),
                );
                frame.fill(&path, color);
            }
        }

        vec![frame.into_geometry()]
    }

    fn mouse_interaction(
        &self,
        state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        self.program.mouse_interaction(state, bounds, cursor)
    }
}

/// Canvas edge color — the constant the WGSL `EDGE_COLOR` uses, kept in
/// Rust for the software path so both backends match visually.
const EDGE_COLOR_CANVAS: Color = Color::from_rgb(0.62, 0.66, 0.74);

/// Version-paired labels shared by GPU and canvas; the overlay takes no input.
pub fn label_overlay<'a>(
    camera: Camera,
    labels: Vec<([f32; 2], f32, String)>,
) -> iced::Element<'a, crate::app::Message> {
    struct Labels(Camera, Vec<([f32; 2], f32, String)>);
    impl canvas::Program<crate::app::Message> for Labels {
        type State = ();
        fn draw(
            &self,
            _state: &(),
            renderer: &iced::Renderer,
            _theme: &Theme,
            bounds: Rectangle,
            _cursor: mouse::Cursor,
        ) -> Vec<canvas::Geometry> {
            let mut frame = canvas::Frame::new(renderer, bounds.size());
            for (world, radius, title) in &self.1 {
                let p = self
                    .0
                    .world_to_screen(*world, [0.0, 0.0], [bounds.width, bounds.height]);
                if p[0] < 0.0 || p[1] < 0.0 || p[0] > bounds.width || p[1] > bounds.height {
                    continue;
                }
                frame.fill_text(canvas::Text {
                    content: title.clone(),
                    position: Point::new(p[0] + radius * self.0.zoom + 5.0, p[1] - 7.0),
                    color: Color::from_rgb(0.88, 0.9, 0.94),
                    size: iced::Pixels(12.0),
                    ..canvas::Text::default()
                });
            }
            vec![frame.into_geometry()]
        }
    }
    Canvas::new(Labels(camera, labels))
        .width(iced::Length::Fill)
        .height(iced::Length::Fill)
        .into()
}

/// Transparent, input-pass-through rectangle used by both graph backends.
pub fn selection_overlay<'a>(
    camera: Camera,
    first: [f32; 2],
    second: [f32; 2],
) -> iced::Element<'a, crate::app::Message> {
    struct BoxOverlay(Camera, [f32; 2], [f32; 2]);
    impl canvas::Program<crate::app::Message> for BoxOverlay {
        type State = ();
        fn draw(
            &self,
            _state: &(),
            renderer: &iced::Renderer,
            _theme: &Theme,
            bounds: Rectangle,
            _cursor: mouse::Cursor,
        ) -> Vec<canvas::Geometry> {
            let a = self
                .0
                .world_to_screen(self.1, [0.0, 0.0], [bounds.width, bounds.height]);
            let b = self
                .0
                .world_to_screen(self.2, [0.0, 0.0], [bounds.width, bounds.height]);
            let mut frame = canvas::Frame::new(renderer, bounds.size());
            let path = Path::rectangle(
                Point::new(a[0].min(b[0]), a[1].min(b[1])),
                iced::Size::new((a[0] - b[0]).abs(), (a[1] - b[1]).abs()),
            );
            frame.stroke(
                &path,
                canvas::Stroke::default()
                    .with_color(Color::from_rgb(0.16, 0.66, 0.62))
                    .with_width(2.0),
            );
            vec![frame.into_geometry()]
        }
    }
    canvas::Canvas::new(BoxOverlay(camera, first, second))
        .width(iced::Length::Fill)
        .height(iced::Length::Fill)
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::instance::{EdgeInstance, NOTE_COLOR, NodeInstance, NodeShape};
    use crate::graph::renderer::{GraphMessage, ReleaseKind};
    use flavite_graph::{NodeId, NodeKind};

    fn geometry() -> Arc<RenderGeometry> {
        Arc::new(RenderGeometry::new(
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
                    color: NOTE_COLOR,
                },
            ],
            vec![EdgeInstance {
                from: [0.0, 0.0],
                to: [10.0, 0.0],
                alpha: 0.5,
                width: 1.0,
                pad: [0.0; 2],
            }],
            vec![NodeId::note(1), NodeId::attachment(1)],
        ))
    }

    #[test]
    fn canvas_emits_the_same_click_messages_as_the_shader_backend() {
        let canvas = GraphCanvas::new(geometry(), Camera::centered());
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut state = GraphProgramState::default();
        let _ = canvas.update(
            &mut state,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            bounds,
            mouse::Cursor::Available(Point::new(400.0, 300.0)),
        );
        let action = canvas
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                bounds,
                mouse::Cursor::Available(Point::new(400.0, 300.0)),
            )
            .expect("release publishes");
        let (message, _, _) = action.into_inner();
        assert_eq!(
            message,
            Some(GraphMessage::Released {
                node: Some(crate::graph::geometry::HitNode::new(
                    NodeId::note(1),
                    NodeKind::Note
                )),
                world: [0.0, 0.0],
                kind: ReleaseKind::Click,
            })
        );
    }

    #[test]
    fn canvas_hit_and_shader_hit_share_one_identity_function() {
        // Same geometry object means same `RenderGeometry::hit` — the
        // backend choice cannot change which note a click names (§9.3).
        let geometry = geometry();
        let hit = geometry.hit([10.0, 0.0], 1.0).expect("attachment node");
        assert_eq!(hit.id, NodeId::attachment(1));
        assert_eq!(hit.kind, NodeKind::Attachment);
    }
}
