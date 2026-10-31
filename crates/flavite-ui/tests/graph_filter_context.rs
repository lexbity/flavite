//! Actual host/Session graph filters, source routing and stale pointer guards.
#[path = "support/graph_fixture.rs"]
mod graph_fixture;
use flavite_ui::app::{App, Message};
use flavite_ui::graph::filters::FilterField;
use flavite_ui::host::graph::GraphCommand;
use flavite_workspace::{
    ContextBinding, ContextRouter, ContextSource, GraphCamera, GraphView, LayoutAction, ViewKind,
};
use graph_fixture::*;
fn graph(app: &mut App) -> flavite_workspace::TabId {
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(app, "index", |app| app.pending_operations() == 0);
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    let _ = app.update(Message::NewViewInFocused(ViewKind::Graph(GraphView::new(
        GraphCamera::centered(),
    ))));
    let tab = app.workspace.focused_tab();
    wait_for(app, "current graph", |app| {
        app.graph
            .state(tab)
            .is_some_and(|s| s.geometry.is_some() && !s.updating && !s.stale)
    });
    tab
}
#[test]
fn selection_filters_outside_scope_and_stale_clicks_never_redirect_the_writing_target() {
    let (mut app, _fs, _cache) = fixture();
    let note = open_note(&mut app, "source.md");
    let target_before = app.workspace.writing_target();
    let tab = graph(&mut app);
    let frame = app.flavite.as_ref().unwrap().graph_frame().unwrap();
    let target = frame
        .publication()
        .records
        .iter()
        .find(|n| n.path == "target.md")
        .unwrap()
        .id;
    let _ = app.update(Message::GraphCommand {
        tab,
        command: GraphCommand::ToggleFreeze,
    });
    let before_positions = app.graph.state(tab).unwrap().frozen_nodes.clone();
    let _ = app.update(Message::GraphCommand {
        tab,
        command: GraphCommand::Select(target),
    });
    assert!(
        matches!(ContextRouter::resolve(&app.workspace,&ContextBinding::FollowExplorationOrDocument),ContextSource::Exploration{locator,..} if locator.title()=="target")
    );
    assert_eq!(app.workspace.writing_target(), target_before);
    let key = app.graph.state(tab).unwrap().geometry_key.unwrap();
    let incarnation = app.workspace.tab(&tab).unwrap().interaction().incarnation;
    let _ = app.update(Message::GraphFilterInput {
        tab,
        incarnation,
        field: FilterField::Path,
        value: "missing-folder".into(),
    });
    let _ = app.update(Message::GraphCommand {
        tab,
        command: GraphCommand::ApplyFilters,
    });
    wait_for(&mut app, "zero matches", |app| {
        app.graph.state(tab).is_some_and(|s| {
            !s.updating && s.geometry.as_ref().is_some_and(|g| g.node_count() == 0)
        })
    });
    assert_eq!(app.graph.state(tab).unwrap().selection.unwrap().id, target);
    assert_eq!(app.workspace.writing_target(), target_before);
    let before = app.note_state_text(note).unwrap();
    let discarded = app.graph.stale_discards_total;
    let _ = app.update(Message::GraphNodeCommand {
        tab,
        incarnation,
        key,
        command: GraphCommand::Insert(target),
    });
    assert_eq!(app.note_state_text(note).unwrap(), before);
    assert!(app.graph.stale_discards_total > discarded);
    let _ = app.update(Message::GraphCommand {
        tab,
        command: GraphCommand::ClearFilters,
    });
    wait_for(&mut app, "clear filters", |app| {
        app.graph.state(tab).is_some_and(|s| {
            !s.updating && s.geometry.as_ref().is_some_and(|g| g.node_count() == 3)
        })
    });
    let geometry = app.graph.state(tab).unwrap().geometry.as_ref().unwrap();
    for (index, node) in geometry.nodes().iter().enumerate() {
        assert_eq!(
            Some(&node.center),
            before_positions.get(&geometry.node_id(index).unwrap()),
            "freezing preserves positions through filtering and selection"
        );
    }
    // A stale scope completion cannot replace the newest sequence.
    let previous_sequence = app.graph.state(tab).unwrap().scope_sequence;
    let moved_group = app.workspace.group_of(tab).unwrap();
    let _ = app.update(Message::Layout(LayoutAction::SetGraphScope {
        expected_revision: app.workspace.revision(),
        tab,
        scope: flavite_workspace::GraphScope {
            predicate: flavite_workspace::NotePredicate::Tag {
                name: "absent".into(),
            },
            ..Default::default()
        },
    }));
    wait_for(&mut app, "new scope after reducer change", |app| {
        app.graph
            .state(tab)
            .is_some_and(|s| !s.updating && s.scope_sequence > previous_sequence)
    });
    assert_eq!(app.workspace.group_of(tab), Some(moved_group));
    assert_eq!(app.workspace.writing_target(), target_before);
}
#[test]
fn occurrence_chooser_opens_the_source_removes_one_span_and_never_edits_the_target() {
    let (mut app, _fs, _cache) = fixture();
    let source = open_note(&mut app, "source.md");
    let tab = graph(&mut app);
    let state = app.graph.state(tab).unwrap();
    let index = state
        .occurrences
        .iter()
        .position(|o| o.source_path == std::path::Path::new("source.md"))
        .unwrap();
    let key = state.geometry_key.unwrap();
    let incarnation = app.workspace.tab(&tab).unwrap().interaction().incarnation;
    let target_before = app
        .flavite
        .as_ref()
        .unwrap()
        .graph_frame()
        .unwrap()
        .projection()
        .saved
        .clone();
    let _ = app.update(Message::InspectGraphOccurrence {
        tab,
        incarnation,
        key,
        index,
    });
    wait_for(&mut app, "occurrence choice", |app| {
        matches!(
            app.graph.removal.as_ref().map(|w| &w.stage),
            Some(flavite_ui::host::occurrences::RemovalStage::Choosing(_))
        )
    });
    let sequence = app.graph.removal.as_ref().unwrap().sequence;
    let _ = app.update(Message::GraphCommand {
        tab,
        command: GraphCommand::ConfirmRemoval(sequence),
    });
    wait_for(&mut app, "removal commit", |app| {
        app.graph.removal.is_none()
            && app.note_state_text(source).as_deref() == Some("See  here.\n")
    });
    assert!(
        app.workspace.writing_target().is_none(),
        "different-origin removal invalidates the old cursor authority"
    );
    let frame = app.flavite.as_ref().unwrap().graph_frame().unwrap();
    assert_eq!(
        frame.projection().saved,
        target_before,
        "removal is an unsaved source edit"
    );
}
