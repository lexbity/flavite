//! Graph shader probe example — production widget (PLAN-B B6, D15).
//!
//! This example demonstrates the production-ready instanced wgpu shader widget.
//!
//! # Usage
//!
//! ```bash
//! cargo run -p flavite-ui --example graph_shader_probe
//! ```

use std::sync::Arc;

use iced::widget::shader::Shader;
use iced::{Element, Length};

use flavite_ui::graph::{
    Camera, ProbeGeometry, ProbeMessage, ProbeProgram, ProbeTimings, SyntheticOptions, generate,
};

/// The probe state.
pub struct ProbeState {
    /// CPU-side geometry: nodes and edges with identities.
    geometry: Arc<ProbeGeometry>,
    /// Shared instrumentation for frame-time measurement.
    timings: Arc<ProbeTimings>,
    /// The current camera (pan/zoom in world units).
    camera: Camera,
}

impl ProbeState {
    /// Initialize the state.
    pub fn new() -> Self {
        let synthetic = generate(&SyntheticOptions::probe_default());
        let positions = synthetic.positions.clone();

        let geometry = ProbeGeometry::from_snapshot(&synthetic.snapshot, &positions, 0)
            .expect("fixture snapshot and positions must match");

        let geometry = Arc::new(geometry);
        let timings = Arc::new(ProbeTimings::default());
        let mut camera = Camera::centered();

        // Fit the camera to the synthetic graph's extent
        let extent = synthetic.world_extent();
        camera.fit_extent(extent, [0.0, 0.0], [800.0, 600.0]);

        Self {
            geometry,
            timings,
            camera,
        }
    }

    /// Update the state with a message.
    pub fn update(&mut self, message: ProbeMessage) -> iced::Task<ProbeMessage> {
        // Handle probe messages: update camera
        match message {
            ProbeMessage::Viewport { size } => {
                // Window size changed - refit camera
                let synthetic = generate(&SyntheticOptions::probe_default());
                self.camera
                    .fit_extent(synthetic.world_extent(), [0.0, 0.0], size);
            }
            ProbeMessage::PanByPixels { dx, dy } => {
                // Apply pan (world units = logical pixels / zoom)
                let zoom = self.camera.zoom;
                self.camera.pan_by(-dx / zoom, -dy / zoom);
            }
            ProbeMessage::ZoomAt { anchor, factor } => {
                // Apply zoom centered at anchor
                self.camera
                    .zoom_at(anchor, [0.0, 0.0], [800.0, 600.0], factor);
            }
            ProbeMessage::ScaleFactor { .. } => {
                // Scale factor changed — handled by camera in draw
            }
        }
        iced::Task::none()
    }

    /// Build the view.
    pub fn view(&self) -> Element<'_, ProbeMessage> {
        let program = ProbeProgram::new(
            Arc::clone(&self.geometry),
            self.camera,
            Arc::clone(&self.timings),
        );

        // Use the shader widget directly
        Shader::new(program)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }
}

/// View function that can be used with the application builder.
fn view_probe(state: &ProbeState) -> Element<'_, ProbeMessage> {
    state.view()
}

fn main() -> iced::Result {
    iced::application(
        || (ProbeState::new(), iced::Task::none()),
        |state: &mut ProbeState, message: ProbeMessage| state.update(message),
        view_probe,
    )
    .run()
}
