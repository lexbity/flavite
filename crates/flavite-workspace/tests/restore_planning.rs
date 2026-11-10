//! Restore-plan edge paths (RFC-0003 §22, W06): every durable kind, every
//! binding, dead-tree retirement, orphan re-attachment, and the deep-tree
//! refusal. Plans are built from crafted payloads, never from live resources.

use flavite_workspace::codec::PersistedGraphCamera;
use flavite_workspace::*;
use std::collections::BTreeSet;

const VAULT: &str = "01234567-89ab-cdef-0123-456789abcdef";

fn tab(id: u128, kind: PersistedKind) -> PersistedTab {
    PersistedTab {
        id: id.to_string(),
        kind,
    }
}

fn group(id: u128, tabs: &[u128], active: u128) -> PersistedGroup {
    PersistedGroup {
        id: id.to_string(),
        tabs: tabs.iter().map(ToString::to_string).collect(),
        active: active.to_string(),
    }
}

fn workspace(
    tabs: Vec<PersistedTab>,
    groups: Vec<PersistedGroup>,
    tree: PersistedNode,
) -> PersistedWorkspace {
    PersistedWorkspace {
        schema: CURRENT_SCHEMA,
        vault_uuid: VAULT.into(),
        tree,
        groups,
        tabs,
        focus: "1".into(),
        maximized: None,
        recently_closed: Vec::new(),
        named_layouts: Vec::new(),
    }
}

fn leaf(id: u128) -> PersistedNode {
    PersistedNode::Group { id: id.to_string() }
}

fn split(
    id: u128,
    arrangement: &str,
    ratio_ppm: u32,
    first: PersistedNode,
    second: PersistedNode,
) -> PersistedNode {
    PersistedNode::Split {
        id: id.to_string(),
        arrangement: arrangement.into(),
        ratio_ppm,
        first: Box::new(first),
        second: Box::new(second),
    }
}

#[test]
fn every_simple_kind_restores_and_invalid_locators_are_dropped() {
    let ws = workspace(
        vec![
            tab(1, PersistedKind::Empty),
            tab(2, PersistedKind::Notes),
            tab(3, PersistedKind::Search),
            tab(4, PersistedKind::Replace),
            tab(5, PersistedKind::Settings),
            tab(
                6,
                PersistedKind::History {
                    locator: vec!["a.md".into()],
                },
            ),
            // A history locator that cannot resolve is dropped, not guessed.
            tab(
                7,
                PersistedKind::History {
                    locator: vec!["..".into()],
                },
            ),
            // A note locator with a separator is equally unrepresentable.
            tab(
                8,
                PersistedKind::Note {
                    mode: NoteMode::Read,
                    locator: vec!["a/b.md".into()],
                },
            ),
        ],
        vec![group(10, &[1, 2, 3, 4, 5, 6, 7, 8], 1)],
        leaf(10),
    );
    let plan = plan_restore(&ws, &BTreeSet::new()).expect("plan builds");
    let restored: BTreeSet<u128> = plan.tabs.iter().map(|tab| tab.id.raw()).collect();
    assert_eq!(
        restored,
        BTreeSet::from([1, 2, 3, 4, 5, 6]),
        "invalid locators drop their tabs"
    );
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("was dropped"))
    );
    assert!(matches!(
        plan.tabs
            .iter()
            .find(|tab| tab.id.raw() == 6)
            .map(|tab| &tab.kind),
        Some(ViewKind::History(_))
    ));
}

#[test]
fn graph_tabs_restore_camera_scope_and_report_invalid_values() {
    let valid_camera = PersistedGraphCamera {
        center: [10.0, -5.0],
        zoom: 2.0,
    };
    let bad_camera = PersistedGraphCamera {
        center: [f32::NAN, 0.0],
        zoom: 1.0,
    };
    let bad_scope = GraphScope {
        neighborhood: Some(Neighborhood {
            root_native: b"a.md".to_vec(),
            depth: 7,
            direction: Direction::Both,
        }),
        ..GraphScope::default()
    };
    let ws = workspace(
        vec![
            tab(
                1,
                PersistedKind::Graph {
                    camera: Some(valid_camera),
                    scope: GraphScope::default(),
                },
            ),
            tab(
                2,
                PersistedKind::Graph {
                    camera: None,
                    scope: GraphScope::default(),
                },
            ),
            tab(
                3,
                PersistedKind::Graph {
                    camera: Some(bad_camera),
                    scope: GraphScope::default(),
                },
            ),
            tab(
                4,
                PersistedKind::Graph {
                    camera: None,
                    scope: bad_scope,
                },
            ),
        ],
        vec![group(10, &[1, 2, 3, 4], 1)],
        leaf(10),
    );
    let plan = plan_restore(&ws, &BTreeSet::new()).expect("plan builds");
    let camera_of = |raw: u128| match plan
        .tabs
        .iter()
        .find(|tab| tab.id.raw() == raw)
        .map(|tab| &tab.kind)
    {
        Some(ViewKind::Graph(view)) => view.camera(),
        other => panic!("expected graph, got {other:?}"),
    };
    assert_eq!(camera_of(1), GraphCamera::new([10.0, -5.0], 2.0).unwrap());
    assert_eq!(
        camera_of(2),
        GraphCamera::centered(),
        "no camera restores centered"
    );
    assert_eq!(
        camera_of(3),
        GraphCamera::centered(),
        "an invalid camera falls back, never clamps"
    );
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("invalid graph camera"))
    );
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("neighborhood")
                || entry.contains("scope")
                || entry.contains("depth")),
        "the invalid scope is reported: {:?}",
        plan.report
    );
    // The invalid scope leaves the default scope in place.
    match plan
        .tabs
        .iter()
        .find(|tab| tab.id.raw() == 4)
        .map(|tab| &tab.kind)
    {
        Some(ViewKind::Graph(view)) => assert_eq!(view.scope(), &GraphScope::default()),
        other => panic!("expected graph, got {other:?}"),
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn context_tools_restore_every_binding_shape() {
    let ws = workspace(
        vec![
            tab(
                1,
                PersistedKind::Preview {
                    binding: PersistedBinding::FollowDocument,
                },
            ),
            tab(
                2,
                PersistedKind::Backlinks {
                    binding: PersistedBinding::FollowExplorationOrDocument,
                },
            ),
            tab(
                3,
                PersistedKind::Outline {
                    binding: PersistedBinding::FollowSource { tab: "1".into() },
                },
            ),
            // A source id that is not a number drops the tab honestly.
            tab(
                4,
                PersistedKind::Properties {
                    binding: PersistedBinding::FollowSource { tab: "junk".into() },
                },
            ),
            tab(
                5,
                PersistedKind::Preview {
                    binding: PersistedBinding::Pinned {
                        path: vec!["p.md".into()],
                    },
                },
            ),
            // An unresolvable pinned locator falls back to the document.
            tab(
                6,
                PersistedKind::Backlinks {
                    binding: PersistedBinding::Pinned {
                        path: vec!["..".into()],
                    },
                },
            ),
            // A kind written by a newer build is inert and dropped.
            tab(
                7,
                PersistedKind::Other {
                    name: "graph".into(),
                    data: serde_json::json!({}),
                },
            ),
        ],
        vec![group(10, &[1, 2, 3, 4, 5, 6, 7], 1)],
        leaf(10),
    );
    let plan = plan_restore(&ws, &BTreeSet::new()).expect("plan builds");
    let kind_of = |raw: u128| {
        plan.tabs
            .iter()
            .find(|tab| tab.id.raw() == raw)
            .map(|tab| tab.kind.clone())
    };
    assert_eq!(
        kind_of(1),
        Some(ViewKind::Preview(ContextBinding::FollowDocument))
    );
    assert_eq!(
        kind_of(2),
        Some(ViewKind::Backlinks(
            ContextBinding::FollowExplorationOrDocument
        ))
    );
    assert_eq!(
        kind_of(3),
        Some(ViewKind::Outline(ContextBinding::FollowSource {
            tab: TabId::from_raw(1)
        }))
    );
    assert_eq!(kind_of(4), None, "an invalid source identity drops the tab");
    assert_eq!(
        kind_of(5),
        Some(ViewKind::Preview(ContextBinding::Pinned(
            EntityLocator::new(NoteLocator::from_components(["p.md"]).unwrap())
        )))
    );
    assert_eq!(
        kind_of(6),
        Some(ViewKind::Backlinks(ContextBinding::FollowDocument)),
        "an invalid pinned locator restores following the document"
    );
    assert_eq!(kind_of(7), None, "unknown kinds are dropped, never guessed");
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("unsupported kind"))
    );
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("pinned locator was invalid"))
    );
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("was dropped"))
    );
}

#[test]
fn dead_groups_and_tabs_are_reported_and_the_tree_retires_with_them() {
    // Group 11's only member tab has an invalid identity, so the tab drops,
    // the group loses every member and dies, and the split beside the
    // surviving group 12 retires with it.
    let ws = workspace(
        vec![
            tab(1, PersistedKind::Empty),
            tab(2, PersistedKind::Empty),
            PersistedTab {
                id: "junk".into(),
                kind: PersistedKind::Empty,
            },
        ],
        vec![
            PersistedGroup {
                id: "11".into(),
                tabs: vec!["junk".into()],
                active: "junk".into(),
            },
            group(12, &[1, 2], 1),
        ],
        split(20, "side_by_side", 500_000, leaf(11), leaf(12)),
    );
    let plan = plan_restore(&ws, &BTreeSet::new()).expect("plan builds");
    assert_eq!(plan.groups, vec![GroupId::from_raw(12)]);
    assert_eq!(
        plan.tree,
        LayoutNode::Group(GroupId::from_raw(12)),
        "the dead side retires the split"
    );
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("invalid identity"))
    );
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("lost every tab"))
    );
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("was dropped with its group"))
    );
}

#[test]
fn one_sided_death_keeps_the_surviving_subtree_and_clamps_ratios() {
    let ws = workspace(
        vec![tab(1, PersistedKind::Empty), tab(2, PersistedKind::Empty)],
        vec![group(11, &[1], 1), group(12, &[2], 2)],
        // Left leaf dies (group 99 not in groups), right survives; the
        // out-of-range ratio is clamped into the persisted range.
        split(
            20,
            "stacked",
            5,
            leaf(99),
            split(21, "weird", 999_999, leaf(11), leaf(12)),
        ),
    );
    let plan = plan_restore(&ws, &BTreeSet::new()).expect("plan builds");
    match &plan.tree {
        LayoutNode::Split {
            arrangement,
            desired_ratio_ppm,
            first,
            second,
            ..
        } => {
            assert_eq!(
                *arrangement,
                Arrangement::Stacked,
                "unknown arrangement strings fall back to stacked"
            );
            assert_eq!(*desired_ratio_ppm, MAX_RATIO_PPM, "ratios clamp into range");
            assert!(matches!(**first, LayoutNode::Group(id) if id.raw() == 11));
            assert!(matches!(**second, LayoutNode::Group(id) if id.raw() == 12));
        }
        other @ LayoutNode::Group(_) => panic!("expected split, got {other:?}"),
    }
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("was dropped with its group"))
    );
}

#[test]
fn orphan_groups_reattach_and_unknown_focus_falls_back() {
    let ws = workspace(
        vec![tab(1, PersistedKind::Empty), tab(2, PersistedKind::Empty)],
        vec![group(11, &[1], 1), group(12, &[2], 2)],
        leaf(11), // group 12 is not in the tree
    );
    let mut ws = ws;
    ws.focus = "777".into(); // unknown focus falls back to the first group's active
    ws.maximized = Some("junk".into()); // unparsable maximized is ignored
    let plan = plan_restore(&ws, &BTreeSet::new()).expect("plan builds");
    assert_eq!(plan.focus, TabId::from_raw(1));
    assert_eq!(plan.maximized, None);
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("re-attached"))
    );
    // The orphan group is re-attached beside the tree with a synthetic split.
    match &plan.tree {
        LayoutNode::Split { first, second, .. } => {
            assert!(matches!(**first, LayoutNode::Group(id) if id.raw() == 11));
            assert!(matches!(**second, LayoutNode::Group(id) if id.raw() == 12));
        }
        other @ LayoutNode::Group(_) => panic!("expected split, got {other:?}"),
    }
}

#[test]
fn all_groups_dying_returns_the_default_empty_plan() {
    // The only group's identity is unparsable, so every group dies and the
    // plan falls back to the default empty workspace.
    let ws = workspace(
        vec![tab(1, PersistedKind::Empty)],
        vec![PersistedGroup {
            id: "not-a-number".into(),
            tabs: vec!["1".into()],
            active: "1".into(),
        }],
        leaf(11),
    );
    let plan = plan_restore(&ws, &BTreeSet::new()).expect("plan builds");
    assert!(plan.tabs.is_empty());
    assert!(plan.groups.is_empty());
    assert_eq!(plan.tree, LayoutNode::Group(GroupId::from_raw(0)));
    assert_eq!(plan.focus, TabId::from_raw(0));
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("invalid identity"))
    );
}

#[test]
fn a_tree_deeper_than_sixteen_is_refused() {
    let mut node = leaf(11);
    for raw in 0..20 {
        node = split(100 + raw, "side_by_side", 500_000, node, leaf(12));
    }
    let ws = workspace(
        vec![tab(1, PersistedKind::Empty), tab(2, PersistedKind::Empty)],
        vec![group(11, &[1], 1), group(12, &[2], 2)],
        node,
    );
    assert_eq!(
        plan_restore(&ws, &BTreeSet::new()),
        Err(RestoreError::TreeTooDeep)
    );
}

#[test]
fn closed_history_and_missing_notes_are_classified_against_the_catalog() {
    let mut ws = workspace(
        vec![
            tab(
                1,
                PersistedKind::Note {
                    mode: NoteMode::Edit,
                    locator: vec!["there.md".into()],
                },
            ),
            tab(
                2,
                PersistedKind::Note {
                    mode: NoteMode::Edit,
                    locator: vec!["gone.md".into()],
                },
            ),
        ],
        vec![group(11, &[1, 2], 1)],
        leaf(11),
    );
    ws.recently_closed = vec![vec!["there.md".into()], vec!["..".into()]];
    let catalog: BTreeSet<String> = ["there.md".to_owned()].into();
    let plan = plan_restore(&ws, &catalog).expect("plan builds");
    assert_eq!(
        plan.closed_history.len(),
        1,
        "only resolvable locators reopen"
    );
    assert_eq!(plan.closed_history[0].components(), ["there.md"]);
    assert_eq!(plan.missing_notes.len(), 1);
    assert_eq!(plan.missing_notes[0].components(), ["gone.md"]);
}
