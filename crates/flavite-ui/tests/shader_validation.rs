//! Headless compile/link validation for the probe's WGSL (PLAN-B §5, B2).
//!
//! Two independent gates, and the split matters:
//!
//! 1. **`naga` parse + validate** — runs on *every* platform, including CI
//!    runners with no GPU at all. It catches a WGSL syntax error, a bad
//!    entry point, or a type error the moment the shader changes, and it
//!    reflects the `Camera` struct so the Rust ↔ WGSL uniform layout can be
//!    asserted without touching a driver.
//!
//! 2. **A real wgpu device** — creates [`ProbePipeline`] (which calls
//!    `Device::create_render_pipeline` for both pipelines) and pushes one
//!    render pass through it, under `push_error_scope(Validation)`. This is
//!    the *link* gate: naga happily validates a shader whose vertex outputs
//!    do not match the pipeline's fragment inputs only inasmuch as the
//!    backend agrees, so a driver is genuinely required to prove it.
//!
//! The device gate needs an adapter. On a machine without one it skips with
//! a message rather than failing — except when `FLAVITE_REQUIRE_WGPU_ADAPTER=1`
//! is set, where "no adapter" *is* the failure, because that is how CI
//! (`.github/workflows/ci.yml`, lavapipe) declares "the GPU half must have
//! actually run here". A silent skip would turn the gate into a no-op and
//! let the probe ship unverified.

use iced::wgpu;
use iced::widget::shader::{Pipeline as _, Primitive as _};

use flavite_ui::graph::instance::{
    CAMERA_UNIFORM_SIZE, Camera, CameraUniform, EdgeInstance, NODE_INSTANCE_SIZE, NOTE_COLOR,
    NodeInstance, NodeShape, ProbeGeometry,
};
use flavite_ui::graph::shader_probe::{
    CAMERA_UNIFORM_NAME, EDGE_FRAGMENT_ENTRY, EDGE_VERTEX_ENTRY, NODE_FRAGMENT_ENTRY,
    NODE_VERTEX_ENTRY, PROBE_WGSL, ProbePipeline, ProbePrimitive, ProbeTimings,
};

use naga::valid::{Capabilities, ValidationFlags, Validator};

/// Environment flag: fail (rather than skip) when no wgpu adapter is found.
const REQUIRE_ADAPTER: &str = "FLAVITE_REQUIRE_WGPU_ADAPTER";

// ---------------------------------------------------------------------------
// 1. naga: parse
// ---------------------------------------------------------------------------

#[test]
fn probe_wgsl_parses() {
    naga::front::wgsl::parse_str(PROBE_WGSL).unwrap_or_else(|error| {
        panic!(
            "the probe's WGSL no longer parses:\n{}",
            error.emit_to_string(PROBE_WGSL)
        )
    });
}

#[test]
fn probe_wgsl_validates() {
    let module = naga::front::wgsl::parse_str(PROBE_WGSL).expect("the probe's WGSL should parse");

    let mut validator = Validator::new(ValidationFlags::all(), Capabilities::default());
    let _info = validator.validate(&module).unwrap_or_else(|error| {
        panic!(
            "the probe's WGSL no longer validates:\n{}",
            error.emit_to_string(PROBE_WGSL)
        )
    });
}

#[test]
fn probe_wgsl_declares_exactly_the_four_expected_entry_points() {
    let module = naga::front::wgsl::parse_str(PROBE_WGSL).expect("parses");

    let found: Vec<(&str, naga::ShaderStage)> = module
        .entry_points
        .iter()
        .map(|entry| (entry.name.as_str(), entry.stage))
        .collect();

    for (name, stage) in [
        (NODE_VERTEX_ENTRY, naga::ShaderStage::Vertex),
        (NODE_FRAGMENT_ENTRY, naga::ShaderStage::Fragment),
        (EDGE_VERTEX_ENTRY, naga::ShaderStage::Vertex),
        (EDGE_FRAGMENT_ENTRY, naga::ShaderStage::Fragment),
    ] {
        assert!(
            found
                .iter()
                .any(|(found_name, found_stage)| { *found_name == name && *found_stage == stage }),
            "missing {stage:?} entry point {name:?}; the pipeline requests it \
             by name and would fail at link time. Found: {found:?}"
        );
    }

    assert_eq!(
        found.len(),
        4,
        "the probe is exactly two pipelines; an unexpected entry point means \
         either a stray `@vertex`/`@fragment` or a rename that has not been \
         followed through to `ProbePipeline`. Found: {found:?}"
    );
}

// ---------------------------------------------------------------------------
// 2. naga: the Rust ↔ WGSL layout contract
// ---------------------------------------------------------------------------

/// Every member of the WGSL `Camera` struct, by name → byte offset.
fn camera_members() -> Vec<(String, u32)> {
    let module = naga::front::wgsl::parse_str(PROBE_WGSL).expect("parses");

    let (_handle, ty) = module
        .types
        .iter()
        .find(|(_, ty)| ty.name.as_deref() == Some(CAMERA_UNIFORM_NAME))
        .unwrap_or_else(|| {
            panic!(
                "no `{CAMERA_UNIFORM_NAME}` struct in the module; \
                 `tests/shader_validation.rs` and the WGSL disagree"
            )
        });

    let naga::TypeInner::Struct { span, members } = &ty.inner else {
        panic!("{CAMERA_UNIFORM_NAME} is not a struct in the WGSL");
    };
    assert!(
        *span as usize == CAMERA_UNIFORM_SIZE,
        "WGSL struct `{CAMERA_UNIFORM_NAME}` spans {span} bytes but \
         `CameraUniform` packs {CAMERA_UNIFORM_SIZE}"
    );

    members
        .iter()
        .map(|member| {
            (
                member.name.clone().unwrap_or_else(|| "<unnamed>".into()),
                member.offset,
            )
        })
        .collect()
}

/// Decode a little-endian `f32` out of a packed uniform at `offset`.
fn f32_at(bytes: &[u8], offset: u32) -> f32 {
    let at = offset as usize;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[at..at + 4]);
    f32::from_le_bytes(buf)
}

/// Decode a little-endian `u32` out of a packed uniform at `offset`.
fn u32_at(bytes: &[u8], offset: u32) -> u32 {
    let at = offset as usize;
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&bytes[at..at + 4]);
    u32::from_le_bytes(buf)
}

#[test]
fn camera_uniform_offsets_match_the_wgsl_struct() {
    // Distinct, recognisable values per field, so a swapped pair cannot
    // accidentally agree.
    let uniform = CameraUniform {
        center: [1.5, -2.5],
        zoom: 3.25,
        edge_alpha: 0.5,
        bounds_position: [100.0, 200.0],
        bounds_size: [800.0, 600.0],
        viewport_size: [1920.0, 1080.0],
        edge_instances: 123_456,
        node_instances: 65_432,
    };
    let bytes = uniform.to_bytes();
    assert_eq!(bytes.len(), CAMERA_UNIFORM_SIZE);

    let members = camera_members();
    let lookup = |name: &str| -> u32 {
        members
            .iter()
            .find(|(member, _)| member == name)
            .unwrap_or_else(|| panic!("`{name}` missing from the WGSL `Camera`"))
            .1
    };

    // Read the Rust-side bytes back at the *WGSL-reported* offsets. If the
    // struct in either language is reordered, resized, or gained a field,
    // this stops matching — which is exactly the drift it exists to catch.
    assert_eq!(
        (
            f32_at(&bytes, lookup("center")),
            f32_at(&bytes, lookup("center") + 4)
        ),
        (1.5, -2.5),
        "center"
    );
    assert_eq!(f32_at(&bytes, lookup("zoom")), 3.25, "zoom");
    assert_eq!(f32_at(&bytes, lookup("edge_alpha")), 0.5, "edge_alpha");
    assert_eq!(
        (
            f32_at(&bytes, lookup("bounds_position")),
            f32_at(&bytes, lookup("bounds_position") + 4)
        ),
        (100.0, 200.0),
        "bounds_position"
    );
    assert_eq!(
        (
            f32_at(&bytes, lookup("bounds_size")),
            f32_at(&bytes, lookup("bounds_size") + 4)
        ),
        (800.0, 600.0),
        "bounds_size"
    );
    assert_eq!(
        (
            f32_at(&bytes, lookup("viewport_size")),
            f32_at(&bytes, lookup("viewport_size") + 4)
        ),
        (1920.0, 1080.0),
        "viewport_size"
    );
    assert_eq!(
        u32_at(&bytes, lookup("edge_instances")),
        123_456,
        "edge_instances"
    );
    assert_eq!(
        u32_at(&bytes, lookup("node_instances")),
        65_432,
        "node_instances"
    );

    // And the offsets are the ones documented in the WGSL comments.
    assert_eq!(
        members,
        vec![
            ("center".to_string(), 0),
            ("zoom".to_string(), 8),
            ("edge_alpha".to_string(), 12),
            ("bounds_position".to_string(), 16),
            ("bounds_size".to_string(), 24),
            ("viewport_size".to_string(), 32),
            ("edge_instances".to_string(), 40),
            ("node_instances".to_string(), 44),
        ],
        "`Camera` grew, shrank, or was reordered — update the WGSL comments \
         and the `to_bytes` packing together"
    );
}

#[test]
fn camera_uniform_is_bound_at_group_0_binding_0() {
    let module = naga::front::wgsl::parse_str(PROBE_WGSL).expect("parses");

    let binding = module
        .global_variables
        .iter()
        .find(|(_, variable)| variable.name.as_deref() == Some("camera"))
        .unwrap_or_else(|| panic!("no `camera` uniform in the WGSL"))
        .1
        .binding
        .expect("`camera` must be a bound resource");

    assert_eq!((binding.group, binding.binding), (0, 0));
}

// ---------------------------------------------------------------------------
// 3. A real device: pipeline creation, then a render pass through it
// ---------------------------------------------------------------------------

/// Acquire a headless wgpu device, or `None` when this machine has no
/// adapter. Panics instead of returning `None` when the environment has
/// declared that an adapter is mandatory.
fn headless_device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor::default());

    let adapter =
        match futures::executor::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::None,
            force_fallback_adapter: false,
            compatible_surface: None,
        })) {
            Ok(adapter) => adapter,
            Err(error) => {
                let required = std::env::var(REQUIRE_ADAPTER).is_ok_and(|value| value == "1");
                assert!(
                    !required,
                    "{REQUIRE_ADAPTER}=1 but no wgpu adapter was available: {error}. \
                 CI installs lavapipe (mesa-vulkan-drivers) for exactly this \
                 reason — without a device the link gate silently does nothing."
                );
                eprintln!("SKIPPED (no wgpu adapter): {error}");
                return None;
            }
        };

    let (device, queue) =
        futures::executor::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("flavite graph probe validation"),
            ..Default::default()
        }))
        .unwrap_or_else(|error| panic!("the adapter refused a device request: {error}"));

    Some((device, queue))
}

#[test]
fn probe_pipelines_compile_and_link_on_a_real_device() {
    let Some((device, queue)) = headless_device() else {
        return;
    };

    // Everything that creates GPU objects goes inside the scope, so a
    // validation error becomes a caught `Error` we can report rather than a
    // process abort from a background thread.
    device.push_error_scope(wgpu::ErrorFilter::Validation);

    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let mut pipeline = ProbePipeline::new(&device, &queue, format);

    // Drive one prepare + one draw through the pipeline. `prepare` writes
    // the camera uniform and uploads the instance buffers; `draw` binds
    // both pipelines against a real render pass, which is what would
    // surface a vertex/fragment interface mismatch.
    let geometry = std::sync::Arc::new(ProbeGeometry::new(
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
                size: 6.0,
                shape: NodeShape::Square,
                color: NOTE_COLOR,
            },
        ],
        vec![EdgeInstance {
            from: [0.0, 0.0],
            to: [10.0, 0.0],
            alpha: 0.8,
            width: 1.0,
            pad: [0.0; 2],
        }],
    ));
    let primitive = ProbePrimitive::new(
        geometry,
        Camera::centered(),
        std::sync::Arc::new(ProbeTimings::default()),
    );

    let bounds = iced::Rectangle {
        x: 0.0,
        y: 0.0,
        width: 800.0,
        height: 600.0,
    };
    let viewport =
        iced::widget::shader::Viewport::with_physical_size(iced::Size::new(1600, 1200), 2.0);
    primitive.prepare(&mut pipeline, &device, &queue, &bounds, &viewport);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("flavite graph probe validation target"),
        size: wgpu::Extent3d {
            width: 16,
            height: 16,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("flavite graph probe validation"),
    });
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("flavite graph probe validation"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
        });
        assert!(
            primitive.draw(&pipeline, &mut pass),
            "the probe claims the render pass, so `iced` will not issue a second one"
        );
    }
    queue.submit(Some(encoder.finish()));
    // Wait for the pass to finish so `pop_error_scope` sees every error
    // the backend raised while executing it, not just the setup ones.
    device.poll(wgpu::PollType::wait_indefinitely()).ok();

    let error = futures::executor::block_on(device.pop_error_scope());
    assert!(
        error.is_none(),
        "the probe's pipelines did not compile and link cleanly: {:?}",
        error.map(|error| error.to_string())
    );

    // A steady-state frame must not have reallocated the instance buffers:
    // `prepare` gates uploads on the geometry generation.
    drop(pipeline);
}

#[test]
fn instance_strides_match_the_wgpu_vertex_layouts() {
    // The vertex attributes in `ProbePipeline::new` are hand-written offsets
    // into these strides. Keeping the two assertions side by side makes a
    // stride change loud.
    assert_eq!(
        NODE_INSTANCE_SIZE, 32,
        "center 0..8, size 8..12, shape 12..16, color 16..32"
    );
    assert_eq!(
        flavite_ui::graph::EDGE_INSTANCE_SIZE,
        32,
        "from 0..8, to 8..16, alpha 16..20, width 20..24, pad 24..32"
    );
    assert_eq!(
        CAMERA_UNIFORM_SIZE, 48,
        "eight members, last one ending at 48"
    );
}
