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
use flavite_graph::NodeKind;
use flavite_ui::app::{App, Message};
use flavite_ui::graph::renderer::GraphMessage;
use flavite_ui::host::context::{ToolContent, ToolState};
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
    wait_for(&mut app, "reconcile", |app| app.pending_operations() == 0);
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
    // input against an Updating frame is discarded by contract (D-02).
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
        Some(flavite_api::BufferStatus::Dirty),
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
        Some(flavite_api::BufferStatus::Dirty),
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
