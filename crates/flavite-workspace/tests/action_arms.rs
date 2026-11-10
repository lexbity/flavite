//! Action-arm and authority-API edge paths (RFC-0003 §7.4, §8.1/§8.2).
//!
//! Every typed rejection leaves the state byte-identical; the success paths
//! exercise the arms the property campaigns do not reach: navigation on
//! non-note tabs, relocation of every referring kind, graph camera/scope
//! validation, sink-follow refusals, move/merge/split limits, cycle no-ops,
//! restore limits, and the acknowledged-interaction guards.

use flavite_workspace::layout::{DEFAULT_RATIO_PPM, MAX_DEPTH, MAX_REOPEN};
use flavite_workspace::*;
use std::collections::BTreeSet;

fn apply(state: &mut WorkspaceState, action: LayoutAction) {
    let Outcome::Committed(commit) = reduce(state, action) else {
        panic!("action rejected")
    };
    *state = commit.state;
}

fn rejected(state: &WorkspaceState, action: LayoutAction) -> WorkspaceError {
    let before = state.clone();
    match reduce(state, action) {
        Outcome::Rejected(error) => {
            assert_eq!(*state, before, "rejections leave state byte-identical");
            error
        }
        Outcome::Committed(_) => panic!("expected rejection"),
    }
}

fn locator(name: &str) -> NoteLocator {
    NoteLocator::from_components([name]).expect("locator")
}

fn restore_action(
    revision: u64,
    tabs: Vec<(TabId, ViewKind)>,
    groups: Vec<(GroupId, Vec<TabId>, TabId)>,
    tree: LayoutNode,
    focus: TabId,
    maximized: Option<GroupId>,
) -> LayoutAction {
    LayoutAction::RestoreWorkspace {
        expected_revision: revision,
        tree,
        tabs,
        groups,
        focus,
        maximized,
        closed_history: Vec::new(),
    }
}

fn open_note(state: &mut WorkspaceState, path: &str) -> TabId {
    let group = state.first_group().expect("group");
    let revision = state.revision();
    apply(
        state,
        LayoutAction::OpenNote {
            expected_revision: revision,
            locator: locator(path),
            group,
        },
    );
    state.focused_tab()
}

fn open_settings(state: &mut WorkspaceState) -> TabId {
    let group = state.first_group().expect("group");
    let revision = state.revision();
    apply(
        state,
        LayoutAction::OpenSettings {
            expected_revision: revision,
            group,
        },
    );
    state.focused_tab()
}

fn editable_ack(state: &mut WorkspaceState, tab: TabId, sequence: u64, text: &str) {
    let mut interaction = state.tab(&tab).expect("tab").interaction().clone();
    interaction.buffer_id = Some(7);
    interaction.revision = 1000;
    interaction.sequence = sequence;
    interaction.focused = true;
    interaction.composition = CompositionState::Idle;
    interaction.selections = vec![Selection::caret(1)];
    state
        .acknowledge_interaction(interaction, text)
        .expect("ack");
}

#[test]
fn navigate_tab_rejects_non_notes_same_locator_and_active_composition() {
    let mut state = WorkspaceState::new();
    let settings = open_settings(&mut state);
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::NavigateTab {
                expected_revision: revision,
                tab: settings,
                locator: locator("x.md"),
            }
        ),
        WorkspaceError::InvariantViolation(_)
    ));

    let note = open_note(&mut state, "a.md");
    let revision = state.revision();
    assert_eq!(
        rejected(
            &state,
            LayoutAction::NavigateTab {
                expected_revision: revision,
                tab: note,
                locator: locator("a.md"),
            }
        ),
        WorkspaceError::NoChange
    );

    // An active composition blocks navigation until committed or cancelled.
    let mut interaction = state.tab(&note).unwrap().interaction().clone();
    interaction.sequence = 1;
    interaction.composition = CompositionState::Active;
    state.acknowledge_interaction(interaction, "abc").unwrap();
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::NavigateTab {
                expected_revision: revision,
                tab: note,
                locator: locator("b.md"),
            }
        ),
        WorkspaceError::InvariantViolation(_)
    ));

    // A clean navigation keeps the tab identity and drops the caret state.
    editable_ack(&mut state, note, 2, "abc");
    assert!(state.writing_target().is_some());
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::NavigateTab {
            expected_revision: revision,
            tab: note,
            locator: locator("b.md"),
        },
    );
    let view = match state.tab(&note).unwrap().kind() {
        ViewKind::Note(note) => note.clone(),
        other => panic!("expected note, got {other:?}"),
    };
    assert_eq!(view.locator(), &locator("b.md"));
    assert_eq!(view.load(), NoteLoad::Reserved);
    assert!(
        state.writing_target().is_none(),
        "navigation invalidates authority"
    );
}

#[test]
fn relocate_note_rewrites_every_referring_kind_and_refuses_noops() {
    let mut state = WorkspaceState::new();
    let group = state.first_group().unwrap();
    let note = open_note(&mut state, "old.md");
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenHistory {
            expected_revision: revision,
            locator: locator("old.md"),
            group,
        },
    );
    let history = state.focused_tab();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenContext {
            expected_revision: revision,
            view: ContextViewKind::Preview,
            binding: ContextBinding::Pinned(EntityLocator::new(locator("old.md"))),
            group,
        },
    );
    let pinned = state.focused_tab();

    // No view refers to the unknown locator: a typed no-op, not a rewrite.
    let revision = state.revision();
    assert_eq!(
        rejected(
            &state,
            LayoutAction::RelocateNote {
                expected_revision: revision,
                from: locator("ghost.md"),
                to: locator("new.md"),
            }
        ),
        WorkspaceError::NoChange
    );

    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::RelocateNote {
            expected_revision: revision,
            from: locator("old.md"),
            to: locator("new.md"),
        },
    );
    let note_view = match state.tab(&note).unwrap().kind() {
        ViewKind::Note(view) => view.clone(),
        other => panic!("expected note, got {other:?}"),
    };
    assert_eq!(note_view.locator(), &locator("new.md"));
    assert!(
        matches!(state.tab(&history).unwrap().kind(), ViewKind::History(loc) if *loc == locator("new.md"))
    );
    assert_eq!(
        state.binding_of(&pinned),
        Some(&ContextBinding::Pinned(EntityLocator::new(locator(
            "new.md"
        ))))
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn graph_scope_and_camera_commit_only_through_validation() {
    let mut state = WorkspaceState::new();
    let group = state.first_group().unwrap();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenGraph {
            expected_revision: revision,
            group,
        },
    );
    let graph = state.focused_tab();

    // Scope/camera actions on a non-graph tab are typed rejections.
    let note = open_note(&mut state, "a.md");
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetGraphScope {
                expected_revision: revision,
                tab: note,
                scope: GraphScope::default(),
            }
        ),
        WorkspaceError::InvalidGraphScope(_)
    ));
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetGraphCamera {
                expected_revision: revision,
                tab: note,
                camera: GraphCamera::centered(),
            }
        ),
        WorkspaceError::UnknownTab(_)
    ));

    // An out-of-range scope or camera is refused, never clamped (§10.3).
    let bad_scope = GraphScope {
        neighborhood: Some(Neighborhood {
            root_native: b"a.md".to_vec(),
            depth: 7,
            direction: Direction::Both,
        }),
        ..GraphScope::default()
    };
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetGraphScope {
                expected_revision: revision,
                tab: graph,
                scope: bad_scope,
            }
        ),
        WorkspaceError::InvalidGraphScope(_)
    ));
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetGraphCamera {
                expected_revision: revision,
                tab: graph,
                camera: GraphCamera {
                    center: [f32::NAN, 0.0],
                    zoom: 1.0,
                },
            }
        ),
        WorkspaceError::InvalidGraphCamera(GraphCameraError::NonFinite)
    ));
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetGraphCamera {
                expected_revision: revision,
                tab: graph,
                camera: GraphCamera {
                    center: [2.0e9, 0.0],
                    zoom: 1.0,
                },
            }
        ),
        WorkspaceError::InvalidGraphCamera(GraphCameraError::OutOfWorldRange)
    ));
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetGraphCamera {
                expected_revision: revision,
                tab: graph,
                camera: GraphCamera {
                    center: [0.0, 0.0],
                    zoom: 100.0,
                },
            }
        ),
        WorkspaceError::InvalidGraphCamera(GraphCameraError::ZoomOutOfRange)
    ));

    // A valid scope and camera commit.
    let camera = GraphCamera::new([10.0, -5.0], 2.0).expect("camera");
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::SetGraphCamera {
            expected_revision: revision,
            tab: graph,
            camera,
        },
    );
    let view = match state.tab(&graph).unwrap().kind() {
        ViewKind::Graph(view) => view.clone(),
        other => panic!("expected graph, got {other:?}"),
    };
    assert_eq!(view.camera(), camera);
}

#[test]
fn note_mode_switch_guards_composition_kind_and_noop() {
    let mut state = WorkspaceState::new();
    let settings = open_settings(&mut state);
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetNoteMode {
                expected_revision: revision,
                tab: settings,
                mode: NoteMode::Read,
            }
        ),
        WorkspaceError::InvariantViolation(_)
    ));

    let note = open_note(&mut state, "a.md");
    let revision = state.revision();
    assert_eq!(
        rejected(
            &state,
            LayoutAction::SetNoteMode {
                expected_revision: revision,
                tab: note,
                mode: NoteMode::Edit,
            }
        ),
        WorkspaceError::NoChange
    );

    let mut interaction = state.tab(&note).unwrap().interaction().clone();
    interaction.sequence = 1;
    interaction.composition = CompositionState::Active;
    state.acknowledge_interaction(interaction, "abc").unwrap();
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetNoteMode {
                expected_revision: revision,
                tab: note,
                mode: NoteMode::Read,
            }
        ),
        WorkspaceError::InvariantViolation(_)
    ));
}

#[test]
fn context_tools_refuse_to_follow_other_context_tools() {
    let mut state = WorkspaceState::new();
    let group = state.first_group().unwrap();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenContext {
            expected_revision: revision,
            view: ContextViewKind::Backlinks,
            binding: ContextBinding::FollowDocument,
            group,
        },
    );
    let sink = state.focused_tab();
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::OpenContext {
                expected_revision: revision,
                view: ContextViewKind::Outline,
                binding: ContextBinding::FollowSource { tab: sink },
                group,
            }
        ),
        WorkspaceError::InvariantViolation(_)
    ));
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::SetContextBinding {
                expected_revision: revision,
                tab: sink,
                binding: ContextBinding::FollowSource { tab: sink },
            }
        ),
        WorkspaceError::InvariantViolation(_)
    ));
    // Rebinding to a source kind is fine.
    let note = open_note(&mut state, "a.md");
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::SetContextBinding {
            expected_revision: revision,
            tab: sink,
            binding: ContextBinding::FollowSource { tab: note },
        },
    );
    assert_eq!(
        state.binding_of(&sink),
        Some(&ContextBinding::FollowSource { tab: note })
    );
    assert_eq!(state.binding_of(&note), None, "notes carry no binding");
}

#[test]
#[allow(clippy::too_many_lines)]
fn move_merge_split_and_cycle_edges_are_typed_rejections_or_noops() {
    let mut state = WorkspaceState::new();
    let g0 = state.first_group().unwrap();
    let a = open_note(&mut state, "a.md");
    let b = open_note(&mut state, "b.md");

    // MoveTab: unknown tab / unknown group.
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::MoveTab {
                expected_revision: revision,
                tab: TabId::from_raw(999_999),
                target: TabDropTarget::Index {
                    group: g0,
                    index: 0,
                },
            }
        ),
        WorkspaceError::UnknownTab(_)
    ));
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::MoveTab {
                expected_revision: revision,
                tab: a,
                target: TabDropTarget::Index {
                    group: GroupId::from_raw(424_242),
                    index: 0,
                },
            }
        ),
        WorkspaceError::UnknownGroup(_)
    ));

    // Split the group, then move a tab by index and by split.
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::SplitGroup {
            expected_revision: revision,
            group: g0,
            arrangement: Arrangement::Stacked,
        },
    );
    let g1 = state.focused_group().unwrap();
    assert_ne!(g1, g0);
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::MoveTab {
            expected_revision: revision,
            tab: b,
            target: TabDropTarget::Index {
                group: g1,
                index: 0,
            },
        },
    );
    assert_eq!(state.group_of(b), Some(g1));
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::MoveTab {
            expected_revision: revision,
            tab: b,
            target: TabDropTarget::SplitFrom {
                group: g0,
                arrangement: Arrangement::SideBySide,
                side: Side::Second,
            },
        },
    );
    assert_ne!(state.group_of(b), Some(g1));

    // MergeGroups: same group is a no-op; unknown groups are typed errors.
    let revision = state.revision();
    assert_eq!(
        rejected(
            &state,
            LayoutAction::MergeGroups {
                expected_revision: revision,
                source: g0,
                destination: g0,
            }
        ),
        WorkspaceError::NoChange
    );
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::MergeGroups {
                expected_revision: revision,
                source: g0,
                destination: GroupId::from_raw(424_242),
            }
        ),
        WorkspaceError::UnknownGroup(_)
    ));

    // MoveGroupSplit / MoveGroupEdge: no-op and unknown-group edges.
    let revision = state.revision();
    assert_eq!(
        rejected(
            &state,
            LayoutAction::MoveGroupSplit {
                expected_revision: revision,
                source: g0,
                destination: g0,
                arrangement: Arrangement::SideBySide,
                side: Side::First,
            }
        ),
        WorkspaceError::NoChange
    );
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::MoveGroupEdge {
                expected_revision: revision,
                group: GroupId::from_raw(424_243),
                edge: Edge::Left,
            }
        ),
        WorkspaceError::UnknownGroup(_)
    ));

    // ResizeSplit: ratio bounds and unknown splits are typed.
    let split = match state.tree() {
        LayoutNode::Split { id, .. } => *id,
        other @ LayoutNode::Group(_) => panic!("expected split, got {other:?}"),
    };
    let revision = state.revision();
    assert_eq!(
        rejected(
            &state,
            LayoutAction::ResizeSplit {
                expected_revision: revision,
                split,
                desired_ratio_ppm: 1,
            }
        ),
        WorkspaceError::InvalidRatio(1)
    );
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::ResizeSplit {
                expected_revision: revision,
                split: SplitId::from_raw(777),
                desired_ratio_ppm: 500_000,
            }
        ),
        WorkspaceError::UnknownSplit(_)
    ));

    // ToggleMaximize toggles and rejects unknown groups.
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::ToggleMaximize {
                expected_revision: revision,
                group: GroupId::from_raw(424_244),
            }
        ),
        WorkspaceError::UnknownGroup(_)
    ));
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::ToggleMaximize {
            expected_revision: revision,
            group: g0,
        },
    );
    assert_eq!(state.maximized_group(), Some(g0));
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::ToggleMaximize {
            expected_revision: revision,
            group: g0,
        },
    );
    assert_eq!(state.maximized_group(), None);

    // CycleTab on a single-tab group and CycleGroup with one group are no-ops.
    let lone = WorkspaceState::new();
    let revision = lone.revision();
    assert_eq!(
        rejected(
            &lone,
            LayoutAction::CycleTab {
                expected_revision: revision,
                forward: true,
            }
        ),
        WorkspaceError::NoChange
    );
    let revision = lone.revision();
    assert_eq!(
        rejected(
            &lone,
            LayoutAction::CycleGroup {
                expected_revision: revision,
                forward: true,
            }
        ),
        WorkspaceError::NoChange
    );

    // DuplicateTab on an Empty tab is refused; ReopenClosed with empty history too.
    let empty = state
        .tabs()
        .find(|(_, session)| matches!(session.kind(), ViewKind::Empty))
        .map(|(tab, _)| *tab)
        .expect("an empty tab exists");
    let revision = state.revision();
    assert!(matches!(
        rejected(
            &state,
            LayoutAction::DuplicateTab {
                expected_revision: revision,
                tab: empty,
            }
        ),
        WorkspaceError::NotDuplicateable(_)
    ));
    let fresh = WorkspaceState::new();
    let revision = fresh.revision();
    assert_eq!(
        rejected(
            &fresh,
            LayoutAction::ReopenClosed {
                expected_revision: revision,
            }
        ),
        WorkspaceError::NothingToReopen
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn restore_workspace_enforces_limits_and_invariants_atomically() {
    let state = WorkspaceState::new();

    // Too many tabs for the bound.
    let too_many_tabs: Vec<(TabId, ViewKind)> = (1..=MAX_TABS as u128 + 1)
        .map(|raw| (TabId::from_raw(raw), ViewKind::Empty))
        .collect();
    let too_many_groups: Vec<(GroupId, Vec<TabId>, TabId)> = (1..=MAX_TABS as u128 + 1)
        .map(|raw| {
            (
                GroupId::from_raw(raw),
                vec![TabId::from_raw(raw)],
                TabId::from_raw(raw),
            )
        })
        .collect();
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                too_many_tabs.clone(),
                too_many_groups,
                LayoutNode::Group(GroupId::from_raw(1)),
                TabId::from_raw(1),
                None,
            )
        ),
        WorkspaceError::TabLimitReached
    ));

    // Too many groups.
    let many_groups: Vec<(GroupId, Vec<TabId>, TabId)> = (1..=MAX_GROUPS as u128 + 1)
        .map(|raw| {
            (
                GroupId::from_raw(raw),
                vec![TabId::from_raw(raw)],
                TabId::from_raw(raw),
            )
        })
        .collect();
    let many_tabs: Vec<(TabId, ViewKind)> = (1..=MAX_GROUPS as u128 + 1)
        .map(|raw| (TabId::from_raw(raw), ViewKind::Empty))
        .collect();
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                many_tabs,
                many_groups,
                LayoutNode::Group(GroupId::from_raw(1)),
                TabId::from_raw(1),
                None,
            )
        ),
        WorkspaceError::GroupLimitReached
    ));

    // Too deep a tree.
    let mut deep = LayoutNode::Group(GroupId::from_raw(1));
    for raw in 2..=MAX_DEPTH as u128 + 2 {
        deep = LayoutNode::Split {
            id: SplitId::from_raw(raw),
            arrangement: Arrangement::Stacked,
            desired_ratio_ppm: DEFAULT_RATIO_PPM,
            first: Box::new(deep),
            second: Box::new(LayoutNode::Group(GroupId::from_raw(1))),
        };
    }
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                vec![(TabId::from_raw(1), ViewKind::Empty)],
                vec![(
                    GroupId::from_raw(1),
                    vec![TabId::from_raw(1)],
                    TabId::from_raw(1)
                )],
                deep,
                TabId::from_raw(1),
                None,
            )
        ),
        WorkspaceError::DepthLimitReached
    ));

    // Invariant violations: empty group, foreign active tab, tab in two
    // groups, tab in no group, unknown focus, unknown maximized.
    let tree = LayoutNode::Split {
        id: SplitId::from_raw(90),
        arrangement: Arrangement::SideBySide,
        desired_ratio_ppm: DEFAULT_RATIO_PPM,
        first: Box::new(LayoutNode::Group(GroupId::from_raw(1))),
        second: Box::new(LayoutNode::Group(GroupId::from_raw(2))),
    };
    let base_tabs = vec![
        (TabId::from_raw(1), ViewKind::Empty),
        (TabId::from_raw(2), ViewKind::Empty),
    ];
    let good_groups = vec![
        (
            GroupId::from_raw(1),
            vec![TabId::from_raw(1)],
            TabId::from_raw(1),
        ),
        (
            GroupId::from_raw(2),
            vec![TabId::from_raw(2)],
            TabId::from_raw(2),
        ),
    ];
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                base_tabs.clone(),
                vec![
                    (GroupId::from_raw(1), Vec::new(), TabId::from_raw(1)),
                    good_groups[1].clone()
                ],
                tree.clone(),
                TabId::from_raw(1),
                None
            )
        ),
        WorkspaceError::InvariantViolation(_)
    ));
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                base_tabs.clone(),
                vec![
                    (
                        GroupId::from_raw(1),
                        vec![TabId::from_raw(1)],
                        TabId::from_raw(2)
                    ),
                    good_groups[1].clone()
                ],
                tree.clone(),
                TabId::from_raw(1),
                None
            )
        ),
        WorkspaceError::InvariantViolation(_)
    ));
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                base_tabs.clone(),
                vec![
                    (
                        GroupId::from_raw(1),
                        vec![TabId::from_raw(1), TabId::from_raw(2)],
                        TabId::from_raw(1)
                    ),
                    good_groups[1].clone()
                ],
                tree.clone(),
                TabId::from_raw(1),
                None
            )
        ),
        WorkspaceError::InvariantViolation(_)
    ));
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                vec![
                    (TabId::from_raw(9), ViewKind::Empty),
                    (TabId::from_raw(1), ViewKind::Empty),
                    (TabId::from_raw(2), ViewKind::Empty)
                ],
                good_groups.clone(),
                tree.clone(),
                TabId::from_raw(1),
                None
            )
        ),
        WorkspaceError::InvariantViolation(_)
    ));
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                base_tabs.clone(),
                good_groups.clone(),
                tree.clone(),
                TabId::from_raw(9),
                None
            )
        ),
        WorkspaceError::InvariantViolation(_)
    ));
    assert!(matches!(
        rejected(
            &state,
            restore_action(
                state.revision(),
                base_tabs.clone(),
                good_groups.clone(),
                tree.clone(),
                TabId::from_raw(1),
                Some(GroupId::from_raw(9))
            )
        ),
        WorkspaceError::InvariantViolation(_)
    ));

    // A valid restore replaces the layout atomically and observes identities.
    let mut state = state;
    let action = restore_action(
        state.revision(),
        base_tabs,
        good_groups,
        tree,
        TabId::from_raw(2),
        Some(GroupId::from_raw(2)),
    );
    apply(&mut state, action);
    assert_eq!(state.focused_tab(), TabId::from_raw(2));
    assert_eq!(state.maximized_group(), Some(GroupId::from_raw(2)));
    assert_eq!(
        state.tabs().count(),
        2,
        "restored tabs replace the fresh empty tab"
    );
}

#[test]
fn interaction_acknowledgement_guards_are_total() {
    let mut state = WorkspaceState::new();
    let note = open_note(&mut state, "a.md");

    // Unknown tab.
    let mut interaction = EditorInteraction::new(TabId::from_raw(31337), 1);
    interaction.sequence = 1;
    assert_eq!(
        state.acknowledge_interaction(interaction, "abc"),
        Err("interaction tab is closed")
    );

    // Non-editable view.
    let settings = open_settings(&mut state);
    let mut interaction = EditorInteraction::new(settings, 1);
    interaction.sequence = 1;
    assert_eq!(
        state.acknowledge_interaction(interaction, "abc"),
        Err("View is not editable")
    );

    // Replaced-view incarnation, out-of-range position, stale sequence,
    // non-boundary position, stale revision, and a caret with no buffer.
    let mut base = state.tab(&note).unwrap().interaction().clone();
    base.sequence = 10;
    base.revision = 5;
    base.buffer_id = Some(3);
    base.focused = true;
    base.selections = vec![Selection::caret(1)];

    let mut wrong_incarnation = base.clone();
    wrong_incarnation.incarnation = 99;
    assert_eq!(
        state.acknowledge_interaction(wrong_incarnation, "abc"),
        Err("interaction belongs to a replaced view")
    );
    let mut too_far = base.clone();
    too_far.selections = vec![Selection::caret(99)];
    assert_eq!(
        state.acknowledge_interaction(too_far, "abc"),
        Err("interaction byte position is outside the current text")
    );
    let mut ok = base.clone();
    ok.sequence = 11;
    state.acknowledge_interaction(ok, "abc").unwrap();
    let mut stale_seq = base.clone();
    stale_seq.sequence = 10;
    assert_eq!(
        state.acknowledge_interaction(stale_seq, "abc"),
        Err("interaction sequence is stale")
    );
    let mut bad_boundary = base.clone();
    bad_boundary.sequence = 12;
    bad_boundary.viewport_anchor = 1; // middle of the two-byte "é"
    assert_eq!(
        state.acknowledge_interaction(bad_boundary, "é"),
        Err("interaction position is not a UTF-8 boundary")
    );
    let mut stale_rev = base.clone();
    stale_rev.sequence = 13;
    stale_rev.revision = 4;
    assert_eq!(
        state.acknowledge_interaction(stale_rev, "abc"),
        Err("interaction revision is stale")
    );
    let mut no_buffer = base.clone();
    no_buffer.sequence = 14;
    no_buffer.buffer_id = None;
    assert_eq!(
        state.acknowledge_interaction(no_buffer, "abc"),
        Err("writing target has no buffer")
    );

    // A focused caret acknowledgement with a buffer establishes the target.
    editable_ack(&mut state, note, 20, "abc");
    let target = state.writing_target().expect("target established");
    assert_eq!(target.tab, note);

    // acknowledge_target_edit guards: changed target, stale revision,
    // non-boundary caret; then a valid advance.
    assert_eq!(
        state.acknowledge_target_edit(target, target.revision, 1, "abc"),
        Err("writing target changed before acknowledgement")
    );
    let mut changed = target;
    changed.buffer_id = 999;
    assert_eq!(
        state.acknowledge_target_edit(changed, 6, 1, "abc"),
        Err("writing target changed before acknowledgement")
    );
    assert_eq!(
        state.acknowledge_target_edit(target, 1001, 1, "é"),
        Err("acknowledged caret is not a UTF-8 boundary")
    );
    state
        .acknowledge_target_edit(target, 1002, 2, "abc")
        .unwrap();
    assert_eq!(state.writing_target().unwrap().revision, 1002);
}

#[test]
fn content_anchor_display_selection_and_focus_release_paths() {
    let mut state = WorkspaceState::new();
    let note = open_note(&mut state, "a.md");
    editable_ack(&mut state, note, 5, "a\nb\nc");
    let target = state.writing_target().expect("target established");
    assert_eq!(target.tab, note);

    assert_eq!(
        state.set_content_anchor(note, 1, "é"),
        Err("Invalid content anchor")
    );
    assert_eq!(
        state.set_content_anchor(TabId::from_raw(31338), 0, "abc"),
        Err("Missing note view")
    );
    // Active composition blocks the content anchor.
    let mut interaction = state.tab(&note).unwrap().interaction().clone();
    interaction.sequence = 6;
    interaction.composition = CompositionState::Active;
    state
        .acknowledge_interaction(interaction, "a\nb\nc")
        .unwrap();
    assert_eq!(
        state.set_content_anchor(note, 1, "a\nb\nc"),
        Err("Composition active")
    );

    // Clear the composition; the focused caret re-establishes the target.
    let mut idle = state.tab(&note).unwrap().interaction().clone();
    idle.sequence = 7;
    idle.composition = CompositionState::Idle;
    state.acknowledge_interaction(idle, "a\nb\nc").unwrap();
    assert!(state.writing_target().is_some());

    // Display selection: a reversed range is rejected; a real one invalidates
    // the writing target (find selections are not insertion authority).
    assert_eq!(
        state.set_display_selection(note, 3, 1, 1, "a\nb\nc"),
        Err("Invalid selection")
    );
    state
        .set_display_selection(note, 0, 2, 1, "a\nb\nc")
        .unwrap();
    assert!(state.writing_target().is_none());
    assert_eq!(
        state.tab(&note).unwrap().interaction().selections,
        [Selection::new(0, 2)]
    );

    // Scrolling keeps the retained target only while every guard matches.
    editable_ack(&mut state, note, 10, "a\nb\nc");
    let retained = state.writing_target().expect("target");
    state.set_content_anchor(note, 3, "a\nb\nc").unwrap();
    let after = state.writing_target().expect("scroll retains the target");
    assert_eq!(after.tab, retained.tab);
    assert_eq!(after.caret_text_byte, retained.caret_text_byte);
    assert_eq!(state.tab(&note).unwrap().interaction().viewport_anchor, 3);

    // release_editor_focus blurs every focused editor except one, and bumps
    // the blurred tab's target sequence without clearing the writing context.
    editable_ack(&mut state, note, 20, "a\nb\nc");
    let other = open_note(&mut state, "b.md");
    editable_ack(&mut state, other, 1, "xyz");
    let other_target = state.writing_target().expect("other holds the target");
    assert_eq!(other_target.tab, other);
    state.release_editor_focus(Some(note));
    assert!(state.tab(&note).unwrap().interaction().focused);
    assert!(!state.tab(&other).unwrap().interaction().focused);
    let after = state
        .writing_target()
        .expect("release keeps the writing context");
    assert_eq!(after.tab, other);
    assert_eq!(
        after.interaction_sequence,
        other_target.interaction_sequence + 1,
        "the blurred editor's sequence advance is observed by the target"
    );
}

#[test]
fn incarnation_and_exploration_guards() {
    let mut state = WorkspaceState::new();
    let note = open_note(&mut state, "a.md");
    let group = state.first_group().unwrap();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenGraph {
            expected_revision: revision,
            group,
        },
    );
    let graph = state.focused_tab();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenContext {
            expected_revision: revision,
            view: ContextViewKind::Preview,
            binding: ContextBinding::FollowDocument,
            group,
        },
    );
    let sink = state.focused_tab();

    assert_eq!(
        state.replace_view_incarnation(TabId::from_raw(31339)),
        Err("view tab is closed")
    );
    let incarnation = state.replace_view_incarnation(note).unwrap();
    assert!(incarnation > 1);
    assert_eq!(
        state.install_view_incarnation(note, 0),
        Err("view incarnation must be nonzero")
    );
    assert_eq!(
        state.install_view_incarnation(TabId::from_raw(31340), 2),
        Err("view tab is closed")
    );
    state.install_view_incarnation(note, 5).unwrap();
    assert_eq!(state.tab(&note).unwrap().interaction().incarnation, 5);

    // Exploration: unknown tabs and sinks cannot publish; the owning source clears.
    assert_eq!(
        state.publish_exploration(TabId::from_raw(31341), 1, Some(locator("a.md"))),
        Err("exploration source tab is closed")
    );
    assert_eq!(
        state.publish_exploration(sink, 1, Some(locator("a.md"))),
        Err("sinks cannot publish exploration selections")
    );
    state
        .publish_exploration(note, 1, Some(locator("a.md")))
        .unwrap();
    state.clear_exploration(graph);
    assert!(state.exploration().is_some(), "a foreign tab cannot clear");
    state.clear_exploration(note);
    assert!(state.exploration().is_none());
    state.publish_exploration(graph, 1, None).unwrap();
    assert_eq!(state.exploration().and_then(|s| s.target.clone()), None);

    // invalidate_writing_target only clears the matching buffer.
    let mut state2 = WorkspaceState::new();
    let tab = open_note(&mut state2, "a.md");
    editable_ack(&mut state2, tab, 1, "abc");
    state2.invalidate_writing_target(12345);
    assert!(
        state2.writing_target().is_some(),
        "other buffers keep the target"
    );
    state2.invalidate_writing_target(7);
    assert!(state2.writing_target().is_none());
}

#[test]
fn reopen_history_is_bounded_and_lifo() {
    let mut state = WorkspaceState::new();
    for index in 0..MAX_REOPEN + 5 {
        state.remember_closed_for_restore(locator(&format!("note-{index}.md")));
    }
    assert_eq!(state.recently_closed_len(), MAX_REOPEN);
    let locators: Vec<String> = state
        .recently_closed()
        .map(|locator| locator.components().join("/"))
        .collect();
    assert_eq!(
        locators.first().unwrap(),
        "note-5.md",
        "oldest entries evict first"
    );
    assert_eq!(
        locators.last().unwrap(),
        &format!("note-{}.md", MAX_REOPEN + 4)
    );

    // ReopenClosed pops the most recent locator into the focused group.
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::ReopenClosed {
            expected_revision: revision,
        },
    );
    let reopened = state.focused_tab();
    assert!(
        matches!(state.tab(&reopened).unwrap().kind(), ViewKind::Note(view) if view.locator().components().join("/") == format!("note-{}.md", MAX_REOPEN + 4))
    );
    assert_eq!(state.recently_closed_len(), MAX_REOPEN - 1);
}

#[test]
fn cycle_tab_and_group_walk_stable_orders() {
    let mut state = WorkspaceState::new();
    let g0 = state.first_group().unwrap();
    let group = state.first_group().unwrap();
    let a = open_note(&mut state, "a.md");
    let b = open_note(&mut state, "b.md");
    assert_eq!(state.focused_tab(), b);
    let strip: Vec<TabId> = state.group(&group).unwrap().tabs().to_vec();

    // CycleTab walks the focused group's strip and wraps around.
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::CycleTab {
            expected_revision: revision,
            forward: true,
        },
    );
    assert_eq!(
        state.focused_tab(),
        strip[0],
        "forward from the last tab wraps to the first"
    );
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::CycleTab {
            expected_revision: revision,
            forward: false,
        },
    );
    assert_eq!(state.focused_tab(), b, "backward returns to the prior tab");
    let _ = a;

    // SplitGroup creates a second group focused on the duplicate.
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::SplitGroup {
            expected_revision: revision,
            group: g0,
            arrangement: Arrangement::SideBySide,
        },
    );
    let g1 = state.focused_group().unwrap();
    assert_ne!(g1, g0);

    // CycleGroup walks the stable traversal order in both directions.
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::CycleGroup {
            expected_revision: revision,
            forward: true,
        },
    );
    assert_eq!(state.focused_group(), Some(g0));
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::CycleGroup {
            expected_revision: revision,
            forward: false,
        },
    );
    assert_eq!(state.focused_group(), Some(g1));
}

#[test]
fn workspace_identity_and_default_are_consistent() {
    let mut ids = Ids::with_seed(42);
    let state = WorkspaceState::with_ids(&mut ids);
    let default = WorkspaceState::default();
    assert_ne!(state.id(), default.id(), "entropy-seeded identities differ");
    assert_eq!(state.revision(), 0);
    assert_eq!(default.revision(), 0);
    let catalog = BTreeSet::new();
    let persisted = persist_workspace(&state, "vault");
    let bytes = encode_workspace(&persisted).unwrap();
    assert_eq!(decode_workspace(&bytes, "vault").unwrap(), persisted);
    let plan = plan_restore(&persisted, &catalog).unwrap();
    assert!(plan.report.is_empty());
}
