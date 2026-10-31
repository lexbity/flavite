//! The S2-1 frame-cost probe: a measurement **consumer** of the production
//! renderer (PLAN-B §5, D15; TD-10 cleanup in W07).
//!
//! The probe used to *own* the WGSL, pipeline, primitive, and program that
//! production later wrapped — an inverted dependency (`shader_view` imported
//! probe types) that made probe instrumentation a production requirement.
//! W07 extracted the shared core into [`super::renderer`]; this module is
//! now the thin instrumentation layer the examples and benchmarks use:
//!
//! - [`ProbeTimings`] / [`TimingsSnapshot`] — atomic per-frame CPU cost of
//!   `prepare`/`draw`, measured around the real production primitive.
//! - [`ProbeProgram`] / [`ProbePrimitive`] / [`ProbeState`] / [`ProbeMessage`]
//!   — wrappers that delegate every real behavior to the production
//!   program and add nothing to it.
//!
//! Production code never imports this module (§5.1; the W07 contract gates
//! forbid `shader_probe::` references in `shader_view.rs`, `renderer.rs`,
//! and `views/graph.rs`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

use iced::mouse;
use iced::wgpu;
use iced::widget::shader::{
    Action, Primitive as ShaderPrimitive, Program as ShaderProgram, Shader, Viewport,
};
use iced::{Event, Length, Rectangle};

use super::geometry::RenderGeometry;
use super::instance::Camera;
use super::renderer::{GRAPH_WGSL, GraphPrimitive, GraphProgram as ProbeCore};

/// The probe's message type is the production message (no fork).
pub use super::renderer::GraphMessage as ProbeMessage;
/// The probe's GPU pipeline is the production pipeline.
pub use super::renderer::GraphPipeline as ProbePipeline;
/// The probe's widget state is the production program state.
pub use super::renderer::GraphProgramState as ProbeState;

/// The probe's WGSL — now simply the production shader, exported under its
/// historical name so existing measurement consumers keep compiling.
pub const PROBE_WGSL: &str = GRAPH_WGSL;

/// Per-frame CPU cost of the widget, in nanoseconds.
///
/// The shader primitive's `prepare`/`draw` methods take `&self`, so these
/// are the *only* mutation points on the frame path. Atomics (not a mutex)
/// keep the "no locks on the frame path" rule literally true — the
/// increment itself is the whole cost.
#[derive(Debug, Default)]
pub struct ProbeTimings {
    prepare_ns: AtomicU64,
    draw_ns: AtomicU64,
    prepare_calls: AtomicU64,
    draw_calls: AtomicU64,
}

impl ProbeTimings {
    /// Record the duration of one shader primitive `prepare` call.
    pub fn record_prepare(&self, elapsed: Duration) {
        self.prepare_ns
            .fetch_add(elapsed.as_nanos() as u64, AtomicOrdering::Relaxed);
        self.prepare_calls.fetch_add(1, AtomicOrdering::Relaxed);
    }

    /// Record the duration of one shader primitive `draw` call.
    pub fn record_draw(&self, elapsed: Duration) {
        self.draw_ns
            .fetch_add(elapsed.as_nanos() as u64, AtomicOrdering::Relaxed);
        self.draw_calls.fetch_add(1, AtomicOrdering::Relaxed);
    }

    /// Read the accumulated totals.
    #[must_use]
    pub fn snapshot(&self) -> TimingsSnapshot {
        TimingsSnapshot {
            prepare_total: Duration::from_nanos(self.prepare_ns.load(AtomicOrdering::Relaxed)),
            draw_total: Duration::from_nanos(self.draw_ns.load(AtomicOrdering::Relaxed)),
            prepare_calls: self.prepare_calls.load(AtomicOrdering::Relaxed),
            draw_calls: self.draw_calls.load(AtomicOrdering::Relaxed),
        }
    }

    /// Zero every counter (the example does this between measured runs).
    pub fn reset(&self) {
        self.prepare_ns.store(0, AtomicOrdering::Relaxed);
        self.draw_ns.store(0, AtomicOrdering::Relaxed);
        self.prepare_calls.store(0, AtomicOrdering::Relaxed);
        self.draw_calls.store(0, AtomicOrdering::Relaxed);
    }
}

/// An immutable read of [`ProbeTimings`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TimingsSnapshot {
    /// Total time spent inside `prepare`, across every frame so far.
    pub prepare_total: Duration,
    /// Total time spent inside `draw`, across every frame so far.
    pub draw_total: Duration,
    /// Frames whose `prepare` ran.
    pub prepare_calls: u64,
    /// Frames whose `draw` ran.
    pub draw_calls: u64,
}

impl TimingsSnapshot {
    /// Widget CPU time for the whole run.
    #[must_use]
    pub fn widget_total(&self) -> Duration {
        self.prepare_total + self.draw_total
    }

    /// Mean `prepare` cost per call, in microseconds.
    #[must_use]
    pub fn mean_prepare_us(&self) -> f64 {
        Self::mean_us(self.prepare_total, self.prepare_calls)
    }

    /// Mean `draw` cost per call, in microseconds.
    #[must_use]
    pub fn mean_draw_us(&self) -> f64 {
        Self::mean_us(self.draw_total, self.draw_calls)
    }

    /// Mean widget cost per frame, in microseconds.
    #[must_use]
    pub fn mean_widget_us(&self) -> f64 {
        self.mean_prepare_us() + self.mean_draw_us()
    }

    fn mean_us(total: Duration, calls: u64) -> f64 {
        if calls == 0 {
            return 0.0;
        }
        total.as_micros() as f64 / calls as f64
    }
}

/// The probe's per-frame primitive: the production primitive plus timing
/// instrumentation around its real `prepare`/`draw` bodies.
#[derive(Debug, Clone)]
pub struct ProbePrimitive {
    inner: GraphPrimitive,
    timings: Arc<ProbeTimings>,
}

impl ProbePrimitive {
    /// Build the primitive for `camera` over `geometry`, reporting into
    /// `timings`.
    #[must_use]
    pub fn new(geometry: Arc<RenderGeometry>, camera: Camera, timings: Arc<ProbeTimings>) -> Self {
        Self {
            inner: GraphPrimitive::new(geometry, camera),
            timings,
        }
    }

    /// The camera this frame renders with.
    #[must_use]
    pub fn camera(&self) -> Camera {
        self.inner.camera()
    }

    /// Node instances drawn.
    #[must_use]
    pub fn node_count(&self) -> u32 {
        self.inner.node_count()
    }

    /// Edge instances drawn (importance-capped).
    #[must_use]
    pub fn edge_count(&self) -> u32 {
        self.inner.edge_count()
    }

    /// The geometry generation backing this frame.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.inner.generation()
    }
}

impl ShaderPrimitive for ProbePrimitive {
    type Pipeline = ProbePipeline;

    fn prepare(
        &self,
        pipeline: &mut Self::Pipeline,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bounds: &Rectangle,
        viewport: &Viewport,
    ) {
        let started = Instant::now();
        self.inner
            .prepare(pipeline, device, queue, bounds, viewport);
        self.timings.record_prepare(started.elapsed());
    }

    fn draw(&self, pipeline: &Self::Pipeline, render_pass: &mut wgpu::RenderPass<'_>) -> bool {
        let started = Instant::now();
        let used_pass = self.inner.draw(pipeline, render_pass);
        self.timings.record_draw(started.elapsed());
        used_pass
    }
}

/// The probe program: delegates every behavior to the production
/// [`GraphProgram`](super::renderer::GraphProgram) and adds only frame-cost
/// reporting. Examples and the S2 harness consume this; production does not.
#[derive(Debug, Clone)]
pub struct ProbeProgram {
    inner: ProbeCore,
    timings: Arc<ProbeTimings>,
}

impl ProbeProgram {
    /// A program that renders `geometry` through `camera`, reporting into
    /// `timings`.
    #[must_use]
    pub fn new(geometry: Arc<RenderGeometry>, camera: Camera, timings: Arc<ProbeTimings>) -> Self {
        Self {
            inner: ProbeCore::new(geometry, camera),
            timings,
        }
    }

    /// The camera this program will render with.
    #[must_use]
    pub fn camera(&self) -> Camera {
        self.inner.camera()
    }

    /// The geometry this program renders.
    #[must_use]
    pub fn geometry(&self) -> &Arc<RenderGeometry> {
        self.inner.geometry()
    }

    /// The shared instrumentation this program reports into.
    #[must_use]
    pub fn timings(&self) -> &Arc<ProbeTimings> {
        &self.timings
    }

    /// Build the `iced` widget for this program, filling the space.
    #[must_use]
    pub fn widget(self) -> Shader<ProbeMessage, Self> {
        Shader::new(self).width(Length::Fill).height(Length::Fill)
    }
}

impl ShaderProgram<ProbeMessage> for ProbeProgram {
    type State = ProbeState;
    type Primitive = ProbePrimitive;

    fn update(
        &self,
        state: &mut Self::State,
        event: &Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> Option<Action<ProbeMessage>> {
        // The probe never forks behavior: whatever production does with a
        // real event, the probe does with the same event.
        self.inner.update(state, event, bounds, cursor)
    }

    fn draw(
        &self,
        state: &Self::State,
        cursor: mouse::Cursor,
        bounds: Rectangle,
    ) -> Self::Primitive {
        ProbePrimitive {
            inner: self.inner.draw(state, cursor, bounds),
            timings: Arc::clone(&self.timings),
        }
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

// The probe re-exports the one helper the historical harness API owned, so
// measurement code has a single import site; production code imports it
// from `renderer` directly.
pub use super::renderer::scroll_factor;

#[cfg(test)]
mod tests {
    use super::super::renderer::ReleaseKind;
    use super::*;
    use crate::graph::instance::{NOTE_COLOR, NodeInstance, NodeShape};
    use flavite_graph::NodeId;
    use iced::Point;

    fn tiny_geometry() -> Arc<RenderGeometry> {
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
            vec![
                crate::graph::instance::EdgeInstance {
                    from: [0.0, 0.0],
                    to: [10.0, 0.0],
                    alpha: 0.9,
                    width: 1.0,
                    pad: [0.0; 2],
                };
                10
            ],
            vec![NodeId::note(1), NodeId::attachment(2)],
        ))
    }

    #[test]
    fn timings_accumulate_and_snapshot() {
        let timings = ProbeTimings::default();
        assert_eq!(timings.snapshot(), TimingsSnapshot::default());
        timings.record_prepare(Duration::from_micros(100));
        timings.record_prepare(Duration::from_micros(300));
        timings.record_draw(Duration::from_micros(50));
        let snapshot = timings.snapshot();
        assert_eq!(snapshot.prepare_calls, 2);
        assert_eq!(snapshot.draw_calls, 1);
        assert_eq!(snapshot.prepare_total, Duration::from_micros(400));
        assert_eq!(snapshot.widget_total(), Duration::from_micros(450));
        assert_eq!(snapshot.mean_prepare_us(), 200.0);
        assert_eq!(snapshot.mean_draw_us(), 50.0);
        timings.reset();
        assert_eq!(timings.snapshot(), TimingsSnapshot::default());
    }

    #[test]
    fn probe_delegates_gestures_to_production() {
        // The probe's release path must produce the same typed message the
        // production program produces for the same event (no fork).
        let program = ProbeProgram::new(
            tiny_geometry(),
            Camera::centered(),
            Arc::new(ProbeTimings::default()),
        );
        let bounds = Rectangle {
            x: 0.0,
            y: 0.0,
            width: 800.0,
            height: 600.0,
        };
        let mut state = ProbeState::default();
        let _ = program.update(
            &mut state,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            bounds,
            mouse::Cursor::Available(Point { x: 400.0, y: 300.0 }),
        );
        let action = program
            .update(
                &mut state,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                bounds,
                mouse::Cursor::Available(Point { x: 400.0, y: 300.0 }),
            )
            .expect("release publishes through production");
        let (message, _, _) = action.into_inner();
        assert!(matches!(
            message,
            Some(ProbeMessage::Released {
                kind: ReleaseKind::Click,
                ..
            })
        ));
    }

    #[test]
    fn probe_primitive_counts_match_production_primitive() {
        let geometry = tiny_geometry();
        let timings = Arc::new(ProbeTimings::default());
        let primitive = ProbePrimitive::new(
            Arc::clone(&geometry),
            Camera::default(),
            Arc::clone(&timings),
        );
        assert_eq!(primitive.node_count(), 2);
        assert_eq!(primitive.edge_count(), 10);
        assert_eq!(primitive.generation(), 1);
    }

    #[test]
    fn probe_types_are_send_sync_across_the_render_handoff() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<ProbePrimitive>();
        assert_send_sync::<RenderGeometry>();
        assert_send_sync::<ProbeTimings>();
        assert_send_sync::<Arc<RenderGeometry>>();
    }
}
