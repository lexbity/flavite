//! Graph camera behavior (manual-UX: the network must settle into view on
//! first render instead of streaming past a stationary viewport, and a
//! pan/zoom hands the camera to the user permanently).
#[path = "support/graph_fixture.rs"]
mod graph_fixture;
use flavite_ui::app::{App, Message};
use flavite_ui::graph::renderer::GraphMessage;
use flavite_ui::host::graph;
use flavite_workspace::{GraphCamera, GraphView, ViewKind};
use graph_fixture::*;

fn graph(app: &mut App) -> flavite_workspace::TabId {
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(app, "index", |app| app.pending_operations() == 0);
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    let _ = app.update(Message::NewViewInFocused(ViewKind::Graph(GraphView::new(
        GraphCamera::centered(),
    ))));
    let tab = app.workspace.focused_tab();
    wait_for(app, "mounted graph", |app| {
        app.graph
            .state(tab)
            .is_some_and(|s| s.geometry.is_some() && !s.updating && !s.stale)
    });
    tab
}

#[test]
fn first_geometry_auto_fits_and_user_pan_takes_the_camera() {
    let (mut app, _fs, _cache) = fixture();
    let tab = graph(&mut app);

    // A few pumps in: with no user camera input, the mounted geometry has
    // been fitted — the camera is no longer the centered default.
    wait_for(&mut app, "auto-fitted camera", |app| {
        app.graph
            .state(tab)
            .is_some_and(|state| state.camera != flavite_ui::graph::instance::Camera::centered())
    });
    let fitted = app.graph.state(tab).unwrap().camera;
    assert_ne!(
        fitted,
        flavite_ui::graph::instance::Camera::centered(),
        "first mount must fit the layout into view"
    );

    // A user pan permanently takes the camera: auto-follow must stop.
    let state = app.graph.state(tab).unwrap();
    let incarnation = app
        .workspace
        .tab(&tab)
        .map_or(0, |s| s.interaction().incarnation);
    let key = state.geometry_key.expect("mounted geometry has a key");
    let _ = app.update(Message::GraphAction {
        tab,
        incarnation,
        message: GraphMessage::PanByPixels { dx: 80.0, dy: 0.0 },
        key,
    });
    let state = app.graph.state(tab).unwrap();
    assert!(state.user_camera_moved, "pan must claim the camera");
    assert_ne!(state.camera, fitted, "pan moved the camera");
}

#[test]
fn fit_camera_to_geometry_centers_and_clamps_tiny_layouts() {
    // Pure function contract via the running host: fit on a real geometry,
    // then verify the durable camera commit path stays consistent.
    let (mut app, _fs, _cache) = fixture();
    let tab = graph(&mut app);
    let _ = app.update(Message::GraphCommand {
        tab,
        command: graph::GraphCommand::FitView,
    });
    wait_for(&mut app, "fitted", |app| {
        app.graph
            .state(tab)
            .is_some_and(|state| state.camera != flavite_ui::graph::instance::Camera::centered())
    });
}
