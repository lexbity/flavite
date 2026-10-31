//! Graph view model rules (RFC-0003 W07, D-04, D-09, §7.3, §10.3).
//!
//! Graph sources publish exploration selections (including deliberately
//! empty ones) without moving the writing target; context sinks bound to a
//! graph source resolve it or show a typed unbound state; graph cameras
//! commit through validated transactions only; duplicates preserve the
//! durable camera and create a new identity.

use flavite_workspace::{
    ContextBinding, ContextRouter, ContextSource, ExplorationSelection, GraphCamera,
    GraphCameraError, GraphView, GroupId, Ids, LayoutAction, NoteLocator, Outcome, TabId,
    UnboundReason, ViewKind, WorkspaceState, is_source_kind, reduce,
};

fn locator(name: &str) -> NoteLocator {
    NoteLocator::from_components([name]).expect("locator")
}

#[allow(clippy::needless_pass_by_value)]
fn open_tab(state: &mut WorkspaceState, kind: ViewKind) -> (TabId, GroupId) {
    let group = state.first_group().expect("default group");
    let action = match &kind {
        ViewKind::Graph(_) => LayoutAction::OpenGraph {
            expected_revision: state.revision(),
            group,
        },
        ViewKind::Note(note) => LayoutAction::OpenNote {
            expected_revision: state.revision(),
            locator: note.locator().clone(),
            group,
        },
        ViewKind::Preview(_) | ViewKind::Backlinks(_) => {
            unreachable!("context tools open through OpenContext")
        }
        _ => LayoutAction::OpenEmpty {
            expected_revision: state.revision(),
            group,
        },
    };
    let Outcome::Committed(commit) = reduce(&state.clone(), action) else {
        panic!("open must commit");
    };
    let tab = commit
        .effects
        .iter()
        .find_map(|effect| match effect {
            flavite_workspace::Effect::ViewAttached { tab, .. } => Some(*tab),
            _ => None,
        })
        .expect("attached effect");
    *state = commit.state;
    let _ = kind;
    (tab, group)
}

#[test]
fn graph_is_a_source_kind_and_sinks_are_not() {
    assert!(is_source_kind(&ViewKind::Graph(GraphView::new(
        GraphCamera::centered()
    ))));
    assert!(!is_source_kind(&ViewKind::Preview(
        ContextBinding::FollowDocument
    )));
}

#[test]
fn graph_publishes_selection_and_deliberately_empty_state() {
    let mut state = WorkspaceState::new();
    let (graph, _) = open_tab(&mut state, graph_kind());
    let (preview, _) = {
        let group = state.first_group().unwrap();
        let outcome = reduce(
            &state,
            LayoutAction::OpenContext {
                expected_revision: state.revision(),
                view: flavite_workspace::ContextViewKind::Preview,
                binding: ContextBinding::FollowExplorationOrDocument,
                group,
            },
        );
        let Outcome::Committed(commit) = outcome else {
            panic!("preview opens");
        };
        let tab = commit
            .effects
            .iter()
            .find_map(|effect| match effect {
                flavite_workspace::Effect::ViewAttached { tab, .. } => Some(*tab),
                _ => None,
            })
            .expect("attached");
        state = commit.state;
        (tab, group)
    };

    // Before any publication: FollowExplorationOrDocument follows the
    // document anchor (absent here) and shows NoDocumentAnchor.
    assert_eq!(
        ContextRouter::resolve(&state, state.binding_of(&preview).unwrap()),
        ContextSource::Unbound {
            reason: UnboundReason::NoDocumentAnchor
        }
    );

    // The graph selects a node.
    state
        .publish_exploration(graph, 1, Some(locator("Beta.md")))
        .expect("graph is a source");
    let preview_binding = state.binding_of(&preview).cloned().expect("binding");
    assert_eq!(
        ContextRouter::resolve(&state, &preview_binding),
        ContextSource::Exploration {
            source_tab: graph,
            locator: locator("Beta.md"),
            sequence: 1,
        }
    );
    // Selection never moved or created a writing target (D-04).
    assert_eq!(state.writing_target(), None);

    // A deliberately published empty selection shows "No selection", not
    // a silent fallback to another note (D-04).
    state
        .publish_exploration(graph, 1, None)
        .expect("empty publication");
    assert_eq!(
        ContextRouter::resolve(&state, &preview_binding),
        ContextSource::Unbound {
            reason: UnboundReason::EmptySelection
        }
    );

    // Closing the source removes the selection; the default binding then
    // uses the document anchor again.
    let outcome = reduce(
        &state,
        LayoutAction::CloseTab {
            expected_revision: state.revision(),
            tab: graph,
        },
    );
    let Outcome::Committed(commit) = outcome else {
        panic!("close commits");
    };
    state = commit.state;
    assert_eq!(state.exploration(), None);
    assert_eq!(
        ContextRouter::resolve(&state, &preview_binding),
        ContextSource::Unbound {
            reason: UnboundReason::NoDocumentAnchor
        }
    );
}

#[test]
fn follow_source_to_graph_reports_typed_publications() {
    let mut state = WorkspaceState::new();
    let (graph, _) = open_tab(&mut state, graph_kind());
    let binding = ContextBinding::FollowSource { tab: graph };
    // No publication yet: typed unbound, not a substituted source.
    assert_eq!(
        ContextRouter::resolve(&state, &binding),
        ContextSource::Unbound {
            reason: UnboundReason::NoExploration
        }
    );
    state
        .publish_exploration(graph, 1, Some(locator("Gamma.md")))
        .expect("source");
    assert_eq!(
        ContextRouter::resolve(&state, &binding),
        ContextSource::Exploration {
            source_tab: graph,
            locator: locator("Gamma.md"),
            sequence: 1,
        }
    );
    // The *explicit empty* publication is distinct from absence (§D-04).
    state.publish_exploration(graph, 1, None).expect("source");
    assert_eq!(
        ContextRouter::resolve(&state, &binding),
        ContextSource::Unbound {
            reason: UnboundReason::EmptySelection
        }
    );
}

#[test]
fn exploration_selection_carries_source_identity() {
    let mut state = WorkspaceState::new();
    let (graph, _) = open_tab(&mut state, graph_kind());
    state
        .publish_exploration(graph, 7, Some(locator("Beta.md")))
        .expect("publish");
    let selection: &ExplorationSelection = state.exploration().expect("selection");
    assert_eq!(selection.source_tab, graph);
    assert_eq!(selection.incarnation, 7);
    assert_eq!(selection.sequence, 1);
}

#[test]
fn graph_camera_commits_only_within_the_validated_range() {
    let mut state = WorkspaceState::new();
    let (graph, _) = open_tab(&mut state, graph_kind());
    // Valid pan/zoom release commits one transaction.
    let camera = GraphCamera::new([12.5, -4.0], 3.5).expect("valid");
    let outcome = reduce(
        &state,
        LayoutAction::SetGraphCamera {
            expected_revision: state.revision(),
            tab: graph,
            camera,
        },
    );
    let Outcome::Committed(commit) = outcome else {
        panic!("valid camera commits");
    };
    state = commit.state;
    let ViewKind::Graph(view) = state.tab(&graph).unwrap().kind() else {
        panic!("graph kind");
    };
    assert_eq!(view.camera(), camera);

    // An out-of-range camera is refused with the state untouched (§10.3).
    let before = state.clone();
    let outcome = reduce(
        &state,
        LayoutAction::SetGraphCamera {
            expected_revision: state.revision(),
            tab: graph,
            camera: GraphCamera {
                center: [0.0, 0.0],
                zoom: 128.0,
            },
        },
    );
    assert!(matches!(
        outcome,
        Outcome::Rejected(flavite_workspace::WorkspaceError::InvalidGraphCamera(
            GraphCameraError::ZoomOutOfRange
        ))
    ));
    assert_eq!(state, before, "rejection is a byte-identical no-op");

    // A camera commit never touches a note tab.
    let (note_tab, _) = {
        let group = state.first_group().unwrap();
        let outcome = reduce(
            &state,
            LayoutAction::OpenNote {
                expected_revision: state.revision(),
                locator: locator("alpha.md"),
                group,
            },
        );
        let Outcome::Committed(commit) = outcome else {
            panic!("note opens");
        };
        let tab = commit
            .effects
            .iter()
            .find_map(|e| match e {
                flavite_workspace::Effect::ViewAttached { tab, .. } => Some(*tab),
                _ => None,
            })
            .expect("tab");
        state = commit.state;
        (tab, group)
    };
    let outcome = reduce(
        &state,
        LayoutAction::SetGraphCamera {
            expected_revision: state.revision(),
            tab: note_tab,
            camera,
        },
    );
    assert!(matches!(
        outcome,
        Outcome::Rejected(flavite_workspace::WorkspaceError::UnknownTab(_))
    ));
}

#[test]
fn duplicated_graph_views_share_camera_start_but_not_identity() {
    let mut state = WorkspaceState::new();
    let (graph, _) = open_tab(&mut state, graph_kind());
    let camera = GraphCamera::new([4.0, 5.0], 2.0).expect("valid");
    let outcome = reduce(
        &state,
        LayoutAction::SetGraphCamera {
            expected_revision: state.revision(),
            tab: graph,
            camera,
        },
    );
    let Outcome::Committed(commit) = outcome else {
        panic!("commit");
    };
    state = commit.state;
    let outcome = reduce(
        &state,
        LayoutAction::DuplicateTab {
            expected_revision: state.revision(),
            tab: graph,
        },
    );
    let Outcome::Committed(commit) = outcome else {
        panic!("duplicate graph");
    };
    state = commit.state;
    let copies: Vec<TabId> = state
        .tabs()
        .filter(|(_, session)| matches!(session.kind(), ViewKind::Graph(_)))
        .map(|(tab, _)| *tab)
        .collect();
    assert_eq!(copies.len(), 2, "two independent graph views");
    assert_ne!(copies[0], copies[1]);
    for tab in copies {
        let ViewKind::Graph(view) = state.tab(&tab).unwrap().kind() else {
            panic!("graph");
        };
        assert_eq!(view.camera(), camera, "durable camera duplicated");
    }
}

#[test]
fn graph_camera_bounds_are_exact() {
    assert!(GraphCamera::new([0.0, 0.0], 0.049).is_err());
    assert!(GraphCamera::new([0.0, 0.0], 64.0001).is_err());
    assert!(GraphCamera::new([f32::NAN, 0.0], 1.0).is_err());
    assert!(
        GraphCamera::new([2.0e9, 0.0], 1.0)
            .err()
            .is_some_and(|error| error == GraphCameraError::OutOfWorldRange)
    );
    assert!(GraphCamera::new([1.0e9, -1.0e9], 64.0).is_ok());
    // Interaction clamps instead of refusing (only import/restore reject).
    let panned = GraphCamera::centered().panned(1.0e12, 0.0).zoomed(1.0e9);
    assert!(panned.center[0] <= GraphCamera::MAX_COORDINATE);
    assert!(panned.zoom <= GraphCamera::MAX_ZOOM);
}

#[test]
fn closing_a_graph_source_retires_its_selection() {
    let mut state = WorkspaceState::new();
    let (graph, _) = open_tab(&mut state, graph_kind());
    let (note, _) = {
        let group = state.first_group().unwrap();
        let outcome = reduce(
            &state,
            LayoutAction::OpenNote {
                expected_revision: state.revision(),
                locator: locator("keep.md"),
                group,
            },
        );
        let Outcome::Committed(commit) = outcome else {
            panic!("open");
        };
        let tab = commit
            .effects
            .iter()
            .find_map(|e| match e {
                flavite_workspace::Effect::ViewAttached { tab, .. } => Some(*tab),
                _ => None,
            })
            .expect("tab");
        state = commit.state;
        (tab, group)
    };
    state
        .publish_exploration(graph, 1, Some(locator("Beta.md")))
        .expect("publish");
    // A FollowSource binding to the graph becomes typed-unbound after close.
    let outcome = reduce(
        &state,
        LayoutAction::CloseTab {
            expected_revision: state.revision(),
            tab: graph,
        },
    );
    let Outcome::Committed(commit) = outcome else {
        panic!("close");
    };
    state = commit.state;
    assert_eq!(state.exploration(), None);
    assert_eq!(
        ContextRouter::resolve(&state, &ContextBinding::FollowSource { tab: graph }),
        ContextSource::Unbound {
            reason: UnboundReason::SourceClosed(graph)
        }
    );
    // The remaining note tab is untouched; publication identity is stable.
    assert_eq!(state.tab(&note).unwrap().title(), "keep");
}

#[test]
fn graph_publish_never_moves_document_anchor_or_writing_target() {
    let mut ids = Ids::new();
    let mut state = WorkspaceState::with_ids(&mut ids);
    let (graph, _) = open_tab(&mut state, graph_kind());
    let anchor_publication = state.document_publication();
    let writing_before = state.writing_target();
    state
        .publish_exploration(graph, 1, Some(locator("Beta.md")))
        .expect("publish");
    assert_eq!(state.writing_target(), writing_before);
    // The publication bumps the observable publication counter (sinks must
    // re-resolve) but never reassigns the anchor itself.
    assert!(state.document_publication() > anchor_publication);
    assert_eq!(state.document_anchor(), None);
}

fn graph_kind() -> ViewKind {
    ViewKind::Graph(GraphView::new(GraphCamera::centered()))
}
