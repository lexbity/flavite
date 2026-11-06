//! RFC-0006 S04 workbench loop (AC-6, D-02/D-01): edit → graph → open →
//! edit against the REAL graph publication pipeline — never a fake graph
//! batch. Link, title, and frontmatter changes made through the editor
//! publish through the projection with exact source identity while a
//! user-held camera, the scope, and the Preview tool stay stable.

#[path = "support/mod.rs"]
mod support;

#[path = "support/graph_fixture.rs"]
mod graph_fixture;

use flavite_editor::{EditorAction, EditorInteractionSnapshot};
use flavite_fs::Fs;
use flavite_graph::NodeKind;
use flavite_ui::app::{App, Message};
use flavite_ui::graph::geometry::HitNode;
use flavite_ui::graph::renderer::{GraphMessage, ReleaseKind};
use flavite_ui::host::context::{ToolContent, ToolState};
use flavite_ui::host::graph::GraphCommand;
use flavite_ui::host::open_intents::OpenIntent;
use flavite_workspace::{ContextViewKind, GraphCamera, GraphView, NoteLocator, TabId, ViewKind};
use graph_fixture::wait_for;

fn locator(name: &str) -> NoteLocator {
    NoteLocator::from_components(name.split('/')).expect("locator")
}

fn loop_fixture() -> (App, tempfile::TempDir) {
    let (mut app, _fs, cache) = graph_fixture::fixture_with(&[
        ("a.md", "# A\n\nsee [[b]] here.\n"),
        ("b.md", "---\ntitle: Bee\n---\n# B\n\nb body\n"),
        ("c.md", "# C\n\nc body\n"),
    ]);
    // The graph needs a real publication pipeline: reconcile, then mount a
    // graph view in a second group (the graph stays visible, AC-5).
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "vault.reconcile", |app| {
        app.pending_operations() == 0
    });
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    let _ = app.update(Message::NewViewInFocused(ViewKind::Graph(GraphView::new(
        GraphCamera::centered(),
    ))));
    (app, cache)
}

fn graph_tab(app: &App) -> TabId {
    app.workspace
        .tabs()
        .find_map(|(tab, session)| matches!(session.kind(), ViewKind::Graph(_)).then_some(*tab))
        .expect("a graph view exists")
}

fn wait_graph_mounted(app: &mut App) -> TabId {
    let tab = graph_tab(app);
    wait_for(app, "mounted graph", |app| {
        app.graph
            .state(tab)
            .is_some_and(|s| s.geometry.is_some() && !s.updating && !s.stale)
    });
    tab
}

fn append_text(app: &mut App, tab: TabId, suffix: &str) -> u64 {
    let incarnation = app.notes.incarnation(tab).expect("incarnation");
    let view = app.note_editor_view(tab).expect("editor view");
    let revision = view.revision;
    let position = view.text.len();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::replace(&view, position, position, suffix.to_owned()),
    });
    revision
}

/// The dense record for a note path in the current base publication.
fn record_for_path(app: &App, path: &str) -> Option<flavite_graph::GraphNodeRecord> {
    let frame = app.flavite.as_ref()?.graph_frame()?;
    let publication = frame.publication();
    publication
        .records
        .iter()
        .find(|record| record.path == path && record.id.kind == NodeKind::Note)
        .cloned()
}

#[test]
fn edit_publishes_to_graph_and_loop_keeps_identity_camera_scope_preview() {
    let (mut app, _cache) = loop_fixture();
    let graph = wait_graph_mounted(&mut app);

    // The writing note: an acknowledged caret makes a.md the document
    // anchor and the writing target (D-01).
    let a_tab = {
        let _ = app.update(Message::OpenIntent(OpenIntent::reuse(
            locator("a.md"),
            None,
        )));
        wait_for(&mut app, "a.md open", |app| !app.notes.any_pending());
        let tab = app.workspace.focused_tab();
        let incarnation = app.notes.incarnation(tab).expect("incarnation");
        let view = app.note_editor_view(tab).expect("editor view");
        let _ = app.update(Message::EditorAction {
            tab,
            incarnation,
            action: EditorAction::Interaction(EditorInteractionSnapshot {
                sequence: 1,
                buffer_id: view.buffer_id,
                revision: view.revision,
                caret: view.text.len(),
                selection: (view.text.len(), view.text.len()),
                viewport_anchor: 0,
                focused: true,
                composing: false,
            }),
        });
        tab
    };
    assert!(
        app.valid_writing_target()
            .is_some_and(|(tab, ..)| tab == a_tab),
        "the acknowledged caret is the writing target"
    );
    // Let the open's overlay publication settle before the camera gesture:
    // camera pan/zoom keeps the exact current-frame pairing while the frame
    // is `Updating` (only navigation is exempt during the hold, D-02), so
    // the gesture starts from a known settled camera.
    wait_for(&mut app, "graph settles after the open", |app| {
        app.graph
            .state(graph)
            .is_some_and(|s| !s.updating && !s.stale && s.geometry.is_some())
    });

    // The user takes the camera with a real pan: auto-follow must stop.
    {
        let state = app.graph.state(graph).expect("graph state");
        let incarnation = app
            .workspace
            .tab(&graph)
            .map_or(0, |s| s.interaction().incarnation);
        let key = state.geometry_key.expect("mounted geometry has a key");
        let _ = app.update(Message::GraphAction {
            tab: graph,
            incarnation,
            message: GraphMessage::PanByPixels { dx: 60.0, dy: 0.0 },
            key,
        });
    }
    let user_camera = app.graph.state(graph).expect("state").camera;
    assert!(app.graph.state(graph).expect("state").user_camera_moved);

    // --- EDIT: append a link to c.md through the real editor path.
    let a_buffer = app.notes.buffer(&a_tab).expect("a buffer");
    let revision_before = append_text(&mut app, a_tab, "\nsee [[c]] too\n");
    assert_eq!(
        app.note_buffer_state(a_tab),
        Some(flavite_buffer::state::BufferState::Dirty),
        "the appended link made the buffer dirty"
    );

    // The Preview tool follows the writing note by default (D-04); it is
    // opened after the edit and stays the group's visible tab (an
    // invisible tool is honestly detached, not served stale content).
    let _ = app.update(Message::OpenContextView {
        view: ContextViewKind::Preview,
        group: None,
    });
    let preview = app
        .workspace
        .tabs()
        .find_map(|(tab, session)| matches!(session.kind(), ViewKind::Preview(_)).then_some(*tab))
        .expect("preview opened");

    // The graph publishes the unsaved edge from the exact buffer: wait on
    // the REAL projection (no injected batch), then assert source identity.
    wait_for(&mut app, "a→c unsaved edge publication", |app| {
        let Some(state) = app.graph.state(graph) else {
            return false;
        };
        let Some(target_record) = record_for_path(app, "c.md") else {
            return false;
        };
        state.occurrences.iter().any(|occurrence| {
            occurrence.edge.to == target_record.id
                && matches!(
                    occurrence.source,
                    flavite_api::OccurrenceSource::Buffer { buffer_id, .. }
                        if buffer_id == a_buffer
                )
        })
    });
    let from = app
        .graph
        .state(graph)
        .expect("state")
        .occurrences
        .iter()
        .find(|occurrence| {
            record_for_path(&app, "c.md").is_some_and(|record| occurrence.edge.to == record.id)
        })
        .map(|occurrence| occurrence.edge.from)
        .expect("the a→c edge was observed");
    let Some(a_record) = record_for_path(&app, "a.md") else {
        panic!("a.md vanished from the publication");
    };
    assert_eq!(from, a_record.id, "the edge leaves a.md's node");

    // --- loop stability: camera, scope, epoch and preview content.
    let state = app.graph.state(graph).expect("state");
    assert_eq!(state.camera, user_camera, "the user-held camera moved");
    assert!(state.user_camera_moved, "auto-follow claimed the camera");
    assert_ne!(
        state.geometry_key.expect("geometry key").overlay_digest,
        0,
        "the published overlay is present"
    );
    let scope_nodes = state
        .scope_result
        .as_ref()
        .map(|scope| scope.nodes.len())
        .unwrap_or_default();
    assert_eq!(scope_nodes, 3, "the saved scope still lists every note");
    // The tool may serve either content kind for the bound source; both
    // must show the exact edited text of the live buffer revision.
    let preview_ready = |app: &App, preview: TabId| match app.contexts.state(&preview) {
        Some(ToolState::Ready(ToolContent::BufferPreview { text, revision, .. }))
            if text.contains("[[c]]") =>
        {
            Some((Some(*revision), true))
        }
        Some(ToolState::Ready(ToolContent::Document(doc)))
            if doc.text.contains("[[c]]") && doc.unsaved =>
        {
            Some((doc.buffer.map(|(_, revision)| revision), true))
        }
        Some(ToolState::Ready(ToolContent::Document(doc))) if doc.text.contains("[[c]]") => {
            Some((doc.buffer.map(|(_, revision)| revision), doc.unsaved))
        }
        _ => None,
    };
    wait_for(&mut app, "preview reflects the edit", |app| {
        preview_ready(app, preview).is_some()
    });
    let (preview_revision, preview_unsaved) = preview_ready(&app, preview).expect("content");
    assert_eq!(
        preview_revision,
        Some(revision_before + 1),
        "preview revision is exact"
    );
    assert!(preview_unsaved, "the preview marks the unsaved draft");

    // --- TITLE/FRONTMATTER: open b.md through the shared destination
    // policy and rename its title through the buffer.
    let _ = app.update(Message::OpenIntent(OpenIntent::reuse(
        locator("b.md"),
        None,
    )));
    wait_for(&mut app, "b.md open", |app| !app.notes.any_pending());
    let b_tab = app.workspace.focused_tab();
    let b_revision = append_text(&mut app, b_tab, ""); // no-op edit for revision parity
    let _ = b_revision;
    {
        let incarnation = app.notes.incarnation(b_tab).expect("incarnation");
        let view = app.note_editor_view(b_tab).expect("editor view");
        let at = view.text.find("title: Bee").expect("frontmatter title");
        let _ = app.update(Message::EditorAction {
            tab: b_tab,
            incarnation,
            action: EditorAction::replace(
                &view,
                at,
                at + "title: Bee".len(),
                "title: Bee 2".into(),
            ),
        });
    }
    wait_for(&mut app, "b.md title publication", |app| {
        record_for_path(app, "b.md").is_some_and(|record| record.title.contains("Bee 2"))
    });
    let state = app.graph.state(graph).expect("state");
    assert_eq!(
        state.camera, user_camera,
        "title publication moved the user-held camera"
    );

    // --- ACTIVATION: activating c.md through the typed intent opens it in
    // the preferred document group; the graph and the loop stay intact.
    let c_node = record_for_path(&app, "c.md").expect("c record").id;
    let _ = app.update(Message::GraphCommand {
        tab: graph,
        command: flavite_ui::host::graph::GraphCommand::Open(c_node),
    });
    wait_for(&mut app, "c.md open through activation", |app| {
        !app.notes.any_pending()
            && app
                .workspace
                .tabs()
                .any(|(_, session)| {
                    matches!(session.kind(), ViewKind::Note(note) if note.locator() == &locator("c.md"))
                })
    });
    // a.md's dirty draft survived the loop untouched.
    assert_eq!(
        app.note_buffer_state(a_tab),
        Some(flavite_buffer::state::BufferState::Dirty),
    );
    assert_eq!(app.notes.buffer(&a_tab), Some(a_buffer));
    assert_eq!(
        app.graph.state(graph).expect("state").camera,
        user_camera,
        "the full loop moved the user-held camera"
    );
    assert!(
        app.is_tab_visible(graph),
        "the graph stays visible when geometry permits (AC-5)"
    );
}

// ---------------------------------------------------------------------------
// D-02 activation contract beyond the settled frame: the frozen hit-geometry
// hold, the stale/removed-record refusal, and the per-kind explicit intents.
// ---------------------------------------------------------------------------

/// One pointer release routed exactly as the mounted renderer publishes it.
fn click_release(
    app: &App,
    graph: TabId,
    node: flavite_graph::NodeId,
    center: [f32; 2],
) -> Message {
    let incarnation = app
        .workspace
        .tab(&graph)
        .expect("mounted graph")
        .interaction()
        .incarnation;
    let key = app
        .graph
        .state(graph)
        .expect("graph state")
        .geometry_key
        .expect("mounted geometry has a key");
    Message::GraphAction {
        tab: graph,
        incarnation,
        message: GraphMessage::Released {
            node: Some(HitNode::new(node, node.kind)),
            world: center,
            kind: ReleaseKind::Click,
        },
        key,
    }
}

fn node_center(app: &App, graph: TabId, node: flavite_graph::NodeId) -> [f32; 2] {
    let state = app.graph.state(graph).expect("graph state");
    let geometry = state.geometry.as_ref().expect("mounted geometry");
    (0..geometry.node_count())
        .find(|index| geometry.node_id(*index) == Some(node))
        .map(|index| geometry.nodes()[index].center)
        .expect("the node is mounted in the frozen geometry")
}

/// Opens a note through the shared destination policy and acknowledges a
/// collapsed caret, which alone establishes the writing target (D-01).
fn open_with_caret(app: &mut App, name: &str) -> TabId {
    let _ = app.update(Message::OpenIntent(OpenIntent::reuse(locator(name), None)));
    wait_for(app, "note open", |app| !app.notes.any_pending());
    let tab = app.workspace.focused_tab();
    let incarnation = app.notes.incarnation(tab).expect("incarnation");
    let view = app.note_editor_view(tab).expect("editor view");
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(EditorInteractionSnapshot {
            sequence: 1,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret: view.text.len(),
            selection: (view.text.len(), view.text.len()),
            viewport_anchor: 0,
            focused: true,
            composing: false,
        }),
    });
    tab
}

fn graph_record_where(
    app: &App,
    kind: NodeKind,
    contains: &str,
) -> Option<flavite_graph::GraphNodeRecord> {
    let frame = app.flavite.as_ref()?;
    let frame = frame.graph_frame()?;
    frame
        .publication()
        .records
        .iter()
        .find(|record| {
            record.id.kind == kind
                && (record.path.contains(contains) || record.title.contains(contains))
        })
        .cloned()
}

fn wait_record(app: &mut App, kind: NodeKind, contains: &str) -> flavite_graph::GraphNodeRecord {
    wait_for(app, "graph node record", |app| {
        graph_record_where(app, kind, contains).is_some()
    });
    graph_record_where(app, kind, contains).expect("record")
}

/// The AC-6 regime at real speed: edits flow, the newer semantic frame is
/// accepted and stays pending presentation as `Updating`, and the click
/// pair still completes on the frozen geometry (D-02's declared 500 ms
/// exception). Explicit Insert keeps requiring the current paired frame.
#[test]
fn pair_activates_while_newer_frame_pending_and_insert_refuses() {
    let (mut app, _fs, _cache) = graph_fixture::fixture_with(&[
        ("a.md", "# A\n\nsee [[b]] here.\n"),
        ("b.md", "# B\n\nb body\n"),
    ]);
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "vault.reconcile", |app| {
        app.pending_operations() == 0
    });
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    let _ = app.update(Message::NewViewInFocused(ViewKind::Graph(GraphView::new(
        GraphCamera::centered(),
    ))));
    let graph = wait_graph_mounted(&mut app);

    // The user takes the camera: selection and activation must not move it.
    let a_tab = open_with_caret(&mut app, "a.md");
    wait_for(&mut app, "graph settles after the open", |app| {
        app.graph
            .state(graph)
            .is_some_and(|s| !s.updating && !s.stale && s.geometry.is_some())
    });
    let _ = app.update(Message::GraphAction {
        tab: graph,
        incarnation: app
            .workspace
            .tab(&graph)
            .expect("graph")
            .interaction()
            .incarnation,
        message: GraphMessage::PanByPixels { dx: 40.0, dy: 0.0 },
        key: app
            .graph
            .state(graph)
            .expect("state")
            .geometry_key
            .expect("key"),
    });
    let user_camera = app.graph.state(graph).expect("state").camera;
    assert!(app.graph.state(graph).expect("state").user_camera_moved);

    let b_id = record_for_path(&app, "b.md").expect("b node").id;
    let center = node_center(&app, graph, b_id);
    let discards_before = app.graph.stale_discards_total;

    // A real buffer edit makes the projection honestly pending.
    let mut expected_a = String::from("# A\n\nsee [[b]] here.\n");
    append_text(&mut app, a_tab, "\nmore [[b]] prose\n");
    expected_a.push_str("\nmore [[b]] prose\n");
    let _ = app.update(Message::Pump);
    assert!(
        !app.flavite.as_ref().expect("api").graph_is_current(),
        "the accepted newer frame is pending publication"
    );
    assert!(
        app.graph.state(graph).expect("state").updating,
        "the pending frame is labelled Updating; the hit geometry stays frozen"
    );

    // First release: selects on the frozen geometry.
    let _ = app.update(click_release(&app, graph, b_id, center));
    assert_eq!(
        app.graph
            .state(graph)
            .expect("state")
            .selection
            .map(|h| h.id),
        Some(b_id),
        "the first click selects"
    );

    // A newer edit lands inside the hold, then the second release arrives:
    // the pair is keyed by identity epoch, not by frame currency (D-02).
    append_text(&mut app, a_tab, "\n");
    expected_a.push('\n');
    let _ = app.update(Message::Pump);
    assert!(
        app.graph.state(graph).expect("state").updating,
        "the frame is still pending inside the gesture hold"
    );
    let _ = app.update(click_release(&app, graph, b_id, center));
    wait_for(
        &mut app,
        "activation opens b.md through the held pair",
        |app| {
            !app.notes.any_pending()
                && app.workspace.tabs().any(|(_, session)| {
                    matches!(session.kind(), ViewKind::Note(note) if note.locator() == &locator("b.md"))
                })
        },
    );
    assert_eq!(
        app.graph.stale_discards_total, discards_before,
        "a pending frame must not eat the held pair's clicks"
    );

    // Activation navigates, it never mutates: both buffers hold their
    // exact edited content and the user camera stayed.
    let b_tab = app
        .workspace
        .tabs()
        .find(|(_, session)| {
            matches!(session.kind(), ViewKind::Note(note) if note.locator() == &locator("b.md"))
        })
        .map(|(tab, _)| *tab)
        .expect("b tab");
    assert_eq!(
        &*app.note_editor_view(b_tab).expect("b view").text,
        "# B\n\nb body\n",
        "the double-click inserted nothing into the activated note"
    );
    assert_eq!(
        &*app.note_editor_view(a_tab).expect("a view").text,
        expected_a,
        "the writing note kept exactly its own edits"
    );
    assert_eq!(
        app.graph.state(graph).expect("state").camera,
        user_camera,
        "selection and activation moved the user-held camera (AC-4/AC-6)"
    );

    // Explicit Insert still requires the current paired frame (D-02):
    // refused while pending, with zero document effect, and the palette/chip
    // availability tracks the very predicate that gates execution (FR-15).
    let _ = app.update(Message::Layout(
        flavite_workspace::LayoutAction::ActivateTab {
            expected_revision: app.workspace.revision(),
            tab: graph,
        },
    ));
    append_text(&mut app, a_tab, " ");
    let _ = app.update(Message::Pump);
    assert!(app.graph.state(graph).expect("state").updating);
    let a_before_insert = app.note_editor_view(a_tab).expect("view").text.clone();
    let _ = app.update(Message::GraphCommand {
        tab: graph,
        command: GraphCommand::InsertSelected,
    });
    assert!(
        app.status.contains("stale") || app.status.contains("disabled"),
        "the insert refusal must explain itself: {}",
        app.status
    );
    assert_eq!(
        app.note_editor_view(a_tab).expect("view").text,
        a_before_insert,
        "a refused insert has zero note effect"
    );
    assert!(
        !flavite_ui::host::commands::available(&app, "graph.insert_selected"),
        "insert availability follows the executing predicate while pending (FR-15)"
    );

    // A removed entity fails activation against the current records even
    // while its old dot may still be painted in the frozen hold.
    let _ = app.update(Message::GraphCommand {
        tab: graph,
        command: GraphCommand::Open(flavite_graph::NodeId::note(999_999)),
    });
    assert!(
        app.status.contains("no longer in the current frame"),
        "activation of a removed node refuses honestly: {}",
        app.status
    );
    assert_eq!(
        app.note_editor_view(a_tab).expect("view").text,
        a_before_insert,
        "the refused activation changed no document"
    );
}

/// D-02/FR-19: activating an unresolved node offers explicit Create or
/// Locate and never creates the note; the band and the palette share the
/// same predicate.
#[test]
fn unresolved_activation_offers_create_or_locate_and_never_creates() {
    let (mut app, fs, _cache) =
        graph_fixture::fixture_with(&[("a.md", "# A\n\nsee [[ghost]] here.\n")]);
    let root = std::path::PathBuf::from("/test/context-routing");
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "vault.reconcile", |app| {
        app.pending_operations() == 0
    });
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    let _ = app.update(Message::NewViewInFocused(ViewKind::Graph(GraphView::new(
        GraphCamera::centered(),
    ))));
    let graph = wait_graph_mounted(&mut app);
    let ghost = wait_record(&mut app, NodeKind::Unresolved, "ghost");

    // A real writing target exists (so a "no caret" excuse cannot explain
    // an insert offer); the graph tab keeps focus for the catalog check.
    let _ = open_with_caret(&mut app, "a.md");
    let _ = app.update(Message::Layout(
        flavite_workspace::LayoutAction::ActivateTab {
            expected_revision: app.workspace.revision(),
            tab: graph,
        },
    ));
    let tabs_before = app.workspace.tabs().count();

    let _ = app.update(Message::GraphCommand {
        tab: graph,
        command: GraphCommand::Open(ghost.id),
    });
    assert_eq!(
        app.workspace.tabs().count(),
        tabs_before,
        "navigation never opens or creates an unresolved note implicitly (FR-19)"
    );
    assert!(
        fs.read_if_exists(&root.join("ghost.md"))
            .expect("fs")
            .is_none(),
        "activation created no file"
    );
    assert!(
        app.status.contains("Create") && app.status.contains("Locate"),
        "the unresolved offer must be explicit: {}",
        app.status
    );

    let _ = app.update(Message::GraphCommand {
        tab: graph,
        command: GraphCommand::Select(ghost.id),
    });
    assert!(
        flavite_ui::host::commands::available(&app, "graph.create_from_node"),
        "Create is offered for the selected unresolved node (FR-15)"
    );
    assert!(
        flavite_ui::host::commands::available(&app, "graph.locate_node"),
        "Locate is offered for the selected unresolved node (FR-15)"
    );
    assert!(
        !flavite_ui::host::commands::available(&app, "graph.insert_selected"),
        "an unresolved target is never insertable"
    );

    let _ = app.update(Message::CreateFromNode(graph));
    let dialog_path = match &app.dialog {
        Some(flavite_ui::host::dialogs::Dialog::NewNote { path }) => path.clone(),
        other => panic!("Create must open the prefilled new-note dialog, got {other:?}"),
    };
    assert!(
        dialog_path.contains("ghost"),
        "the dialog prefills the link target: {dialog_path}"
    );
    assert!(
        fs.read_if_exists(&root.join("ghost.md"))
            .expect("fs")
            .is_none(),
        "the unconfirmed dialog created nothing"
    );
    app.dialog = None;

    let _ = app.update(Message::LocateFromNode(graph));
    assert!(
        app.picker.open && app.picker.query.contains("ghost"),
        "Locate prefills the picker with the link target: {:?}",
        app.picker.query
    );
    assert_eq!(
        app.workspace.tabs().count(),
        tabs_before,
        "offering Create/Locate changed no workspace state"
    );
}

/// D-02: activating an attachment node drives the existing local
/// attachment inspector — never a note open, and the core-validated
/// outcome is reported honestly on the owning graph tab.
#[test]
fn attachment_activation_uses_the_local_inspector_not_a_note_open() {
    let (mut app, _fs, _cache) = graph_fixture::fixture_with(&[
        ("a.md", "# A\n\nmedia ![[img/movie.mkv]] here.\n"),
        ("img/movie.mkv", "fake-matroska-bytes"),
    ]);
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "vault.reconcile", |app| {
        app.pending_operations() == 0
    });
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    let _ = app.update(Message::NewViewInFocused(ViewKind::Graph(GraphView::new(
        GraphCamera::centered(),
    ))));
    let graph = wait_graph_mounted(&mut app);
    let att = wait_record(&mut app, NodeKind::Attachment, "movie.mkv");
    let tabs_before = app.workspace.tabs().count();

    // Activation submits the existing validated-open primitive and tracks
    // it by the core-issued token; a second activation in the same
    // in-flight window refuses instead of stacking requests.
    let _ = app.update(Message::GraphCommand {
        tab: graph,
        command: GraphCommand::Open(att.id),
    });
    assert!(
        app.status.contains("Opening local attachment"),
        "attachment activation reached the inspector: {}",
        app.status
    );
    assert_eq!(
        app.graph.attachment_opens.len(),
        1,
        "the inspector request is owned by this graph tab"
    );
    let _ = app.update(Message::GraphCommand {
        tab: graph,
        command: GraphCommand::Open(att.id),
    });
    assert!(app.status.contains("already opening"));
    assert_eq!(app.graph.attachment_opens.len(), 1);
    assert_eq!(
        app.workspace.tabs().count(),
        tabs_before,
        "an attachment never opens as a note"
    );

    // The completion routes to the owning tab with its real result. The
    // indexed .mkv has no safe open action, so the outcome is the honest
    // refusal — no note tab, no document effect, no dead token.
    wait_for(&mut app, "attachment inspector result routed", |app| {
        app.graph.attachment_opens.is_empty()
    });
    assert!(
        app.status.contains("Attachment unavailable"),
        "the typed refusal is reported: {}",
        app.status
    );
    assert_eq!(app.workspace.tabs().count(), tabs_before);
}

/// D-02: during a pending pair hold the hit geometry is frozen — an
/// entity-moving frame (a node drag committing a shared pin) waits out the
/// bounded hold while emphasis-only updates still apply, and the deferred
/// frame catches up once the gesture resolves. A drag also suppresses the
/// click pair instead of ever activating.
#[test]
fn entity_frames_freeze_for_the_pair_hold_while_emphasis_updates() {
    let (mut app, _fs, _cache) = graph_fixture::fixture_with(&[
        ("a.md", "# A\n\nsee [[b]] and [[c]].\n"),
        ("b.md", "# B\n\nb body\n"),
        ("c.md", "# C\n\nc body\n"),
    ]);
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "vault.reconcile", |app| {
        app.pending_operations() == 0
    });
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    let _ = app.update(Message::NewViewInFocused(ViewKind::Graph(GraphView::new(
        GraphCamera::centered(),
    ))));
    let graph = wait_graph_mounted(&mut app);

    let b_id = record_for_path(&app, "b.md").expect("b node").id;
    let c_id = record_for_path(&app, "c.md").expect("c node").id;
    let b_center = node_center(&app, graph, b_id);
    let c_center = node_center(&app, graph, c_id);
    let incarnation = app
        .workspace
        .tab(&graph)
        .expect("graph")
        .interaction()
        .incarnation;

    // First release of the pair on b: the bounded hold starts.
    let _ = app.update(click_release(&app, graph, b_id, b_center));
    assert!(
        app.graph
            .state(graph)
            .expect("state")
            .arbiter
            .has_candidate(),
        "the pair candidate is pending"
    );

    // Emphasis-only (palette switch) is allowed to update during the hold.
    let gen0 = app.graph.state(graph).expect("state").geometry_generation;
    let _ = app.update(Message::Preferences(
        flavite_ui::host::appearance::PreferenceMessage::SetTheme(
            flavite_ui::preferences::ThemeChoice::Light,
        ),
    ));
    let _ = app.update(Message::Pump);
    assert!(
        app.graph.state(graph).expect("state").geometry_generation > gen0,
        "selection emphasis/palette may update while the hit geometry is frozen"
    );
    let gen1 = app.graph.state(graph).expect("state").geometry_generation;

    // An entity-moving frame lands during the hold: the node-drag preview
    // of c. The mounted geometry (and b's hit position) stays frozen.
    let c_hit = HitNode::new(c_id, c_id.kind);
    let _ = app.update(Message::GraphAction {
        tab: graph,
        incarnation,
        message: GraphMessage::NodeDragged {
            node: c_hit,
            world: [c_center[0] + 25.0, c_center[1] + 25.0],
        },
        key: app
            .graph
            .state(graph)
            .expect("state")
            .geometry_key
            .expect("key"),
    });
    let _ = app.update(Message::Pump);
    assert_eq!(
        app.graph.state(graph).expect("state").geometry_generation,
        gen1,
        "the entity frame is deferred while the pair hold is pending"
    );
    assert_eq!(
        node_center(&app, graph, b_id),
        b_center,
        "the frozen hit geometry moved under the cursor"
    );

    // The completed drag cancels click recognition (§9.3: a drag never
    // activates) and commits the shared pin.
    let _ = app.update(Message::GraphAction {
        tab: graph,
        incarnation,
        message: GraphMessage::Released {
            node: Some(c_hit),
            world: [c_center[0] + 25.0, c_center[1] + 25.0],
            kind: ReleaseKind::NodeDragCompleted { node: c_hit },
        },
        key: app
            .graph
            .state(graph)
            .expect("state")
            .geometry_key
            .expect("key"),
    });
    assert!(
        !app.graph
            .state(graph)
            .expect("state")
            .arbiter
            .has_candidate(),
        "a completed drag suppresses the pending pair"
    );
    let tabs_before = app.workspace.tabs().count();

    // With the hold resolved, the deferred pin frame catches up: c moves
    // to its pinned position and b is untouched.
    wait_for(&mut app, "deferred pin frame applied", |app| {
        app.graph
            .state(graph)
            .is_some_and(|s| s.geometry_generation > gen1 && s.pins_in_signature())
    });
    assert_ne!(
        node_center(&app, graph, c_id),
        c_center,
        "the deferred pin never caught up"
    );
    assert_eq!(
        node_center(&app, graph, b_id),
        b_center,
        "the settled b node moved"
    );
    assert_eq!(
        app.workspace.tabs().count(),
        tabs_before,
        "the suppressed drag pair opened nothing"
    );

    // A fresh deliberate click after the hold selects normally.
    let _ = app.update(click_release(&app, graph, b_id, b_center));
    assert_eq!(
        app.graph
            .state(graph)
            .expect("state")
            .selection
            .map(|h| h.id),
        Some(b_id),
        "post-hold clicks recognize again"
    );
}
