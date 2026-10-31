//! Context routing integration (RFC-0003 §20, W04).
//!
//! Real notes A→B through the real Session/API/query-service stack: the
//! Backlinks tool follows the document anchor, survives pane moves, pins to
//! a locator, and late superseded queries never repaint a moved or rebound
//! tool. Scheduling parity is additionally covered by the registry unit
//! tests; here we prove the end-to-end wiring including async workers.

use flavite_ui::app::{App, Message};
use flavite_ui::host::context::{ToolContent, ToolState};
use flavite_workspace::{
    ContextBinding, ContextViewKind, EntityLocator, LayoutAction, NoteLocator, TabId, ViewKind,
};

#[path = "support/graph_fixture.rs"]
mod graph_fixture;
use graph_fixture::*;

fn context_tool(app: &App) -> TabId {
    app.workspace
        .tabs()
        .find_map(|(tab, session)| matches!(session.kind(), ViewKind::Backlinks(_)).then_some(*tab))
        .expect("a backlinks tool exists")
}

fn ready_backlinks(app: &App, tab: TabId) -> Option<&Vec<flavite_api::BacklinkRow>> {
    match app.contexts.state(&tab) {
        Some(ToolState::Ready(ToolContent::Backlinks { rows, .. })) => Some(rows),
        _ => None,
    }
}

#[test]
fn backlinks_follow_move_pin_and_never_repaint_from_stale_queries() {
    let (mut app, _fs, _cache) = fixture();
    // Index needs the link graph; reconcile then refresh the catalog.
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut app, "reconcile", |app| app.pending_operations() == 0);
    let _ = app.update(Message::RefreshCatalog);

    // Anchor on target.md, then open Backlinks following the document:
    // the rows are the notes that link TO the anchor.
    let _source = open_note(&mut app, "target.md");
    let _ = app.update(Message::OpenContextView {
        view: ContextViewKind::Backlinks,
        group: None,
    });
    let tool = context_tool(&app);
    wait_for(&mut app, "source backlinks", |app| {
        ready_backlinks(app, tool).is_some()
    });
    #[cfg(feature = "ui-test-support")]
    eprintln!(
        "DEBUG tool={tool:?} state={:?} binding={:?} observed={:?}",
        app.contexts.state(&tool),
        app.workspace.binding_of(&tool),
        app.contexts
            .observed(&tool)
            .map(|s| s.locator.components().join("/")),
    );
    let _ = &tool;
    let rows = ready_backlinks(&app, tool).expect("rows").clone();
    assert!(
        rows.iter()
            .any(|row| row.source_path.ends_with("source.md")),
        "source.md links to target: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.source_path.ends_with("other.md")),
        "other.md links to target too: {rows:?}"
    );
    assert!(
        rows.iter().any(|row| row
            .excerpt
            .as_deref()
            .is_some_and(|e| e.contains("[[target]]"))),
        "the excerpt contains the exact link line: {rows:?}"
    );

    // Moving the tool to another group preserves binding and content.
    let first_group = app.focused_group().unwrap();
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    let second_group = app.focused_group().unwrap();
    assert_ne!(first_group, second_group);
    let position = app
        .workspace
        .group(&second_group)
        .map(|group| group.tabs().len())
        .unwrap_or(0);
    let _ = app.update(Message::Layout(LayoutAction::MoveTab {
        expected_revision: app.workspace.revision(),
        tab: tool,
        target: flavite_workspace::TabDropTarget::Index {
            group: second_group,
            index: position,
        },
    }));
    let _ = app.update(Message::Pump);
    assert_eq!(
        app.workspace.binding_of(&tool).cloned(),
        Some(ContextBinding::FollowDocument),
        "the binding survives a pane move"
    );
    let moved_rows = ready_backlinks(&app, tool).expect("content survives the move");
    assert_eq!(moved_rows.len(), rows.len());

    // Re-anchor on a note with no inbound links: the tool follows and
    // reports the honest state.
    let _ = open_note(&mut app, "source.md");
    wait_for(&mut app, "no backlinks", |app| {
        matches!(app.contexts.state(&tool), Some(ToolState::NoBacklinks))
    });

    // Pin the source locator: later anchor changes no longer repaint.
    let _ = app.update(Message::SetContextBinding {
        tab: tool,
        binding: ContextBinding::Pinned(EntityLocator::new(
            NoteLocator::from_components(["target.md"]).unwrap(),
        )),
    });
    wait_for(&mut app, "pinned backlinks", |app| {
        ready_backlinks(app, tool).is_some()
    });
    let pinned_rows = ready_backlinks(&app, tool).expect("pinned rows").clone();
    let _ = open_note(&mut app, "other.md");
    wait_for(&mut app, "pinned stays", |app| {
        app.workspace
            .writing_target()
            .is_some_and(|target| target.tab == app.workspace.focused_tab())
    });
    let _ = app.update(Message::Pump);
    match app.contexts.state(&tool) {
        Some(ToolState::Ready(ToolContent::Backlinks { rows, .. })) => {
            assert_eq!(
                rows, &pinned_rows,
                "pinned content is anchored to the locator"
            );
        }
        other => panic!("pinned tool keeps its content, got {other:?}"),
    }

    // Rebind to follow again, then rapid re-anchoring: the final content
    // corresponds to the LAST source, never to a superseded query (n+1
    // wins even when n lands later).
    let _ = app.update(Message::SetContextBinding {
        tab: tool,
        binding: ContextBinding::FollowDocument,
    });
    let _ = open_note(&mut app, "source.md");
    let _ = open_note(&mut app, "other.md");
    wait_for(&mut app, "last-source settles", |app| {
        let observed = app
            .contexts
            .observed(&context_tool(app))
            .map(|source| source.locator.components().join("/"));
        observed.as_deref() == Some("other.md")
            && matches!(
                app.contexts.state(&context_tool(app)),
                Some(ToolState::NoBacklinks)
            )
    });
}
