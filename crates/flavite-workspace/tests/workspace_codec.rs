//! Workspace codec conformance (RFC-0003 §22, W06).
//!
//! Golden round-trips, identity/reference/ratio/depth/checksum violations,
//! vault mismatch, giant inputs, future majors, and non-UTF-8 native names:
//! the codec refuses without mutating anything and never guesses.

use flavite_workspace::{
    CURRENT_SCHEMA, CodecError, Direction, GraphScope, MAX_WORKSPACE_FILE_BYTES,
    MIN_SUPPORTED_SCHEMA, NamedLayout, Neighborhood, PersistedBinding, PersistedGroup,
    PersistedKind, PersistedNode, PersistedTab, PersistedWorkspace, decode_workspace,
    encode_workspace, validate_workspace,
};
use std::collections::BTreeSet;

const VAULT: &str = "01234567-89ab-cdef-0123-456789abcdef";

fn sample() -> PersistedWorkspace {
    PersistedWorkspace {
        schema: CURRENT_SCHEMA,
        vault_uuid: VAULT.into(),
        tree: PersistedNode::Split {
            id: "21".into(),
            arrangement: "side_by_side".into(),
            ratio_ppm: 180_000,
            first: Box::new(PersistedNode::Group { id: "11".into() }),
            second: Box::new(PersistedNode::Group { id: "12".into() }),
        },
        groups: vec![
            PersistedGroup {
                id: "11".into(),
                tabs: vec!["1".into(), "2".into()],
                active: "1".into(),
            },
            PersistedGroup {
                id: "12".into(),
                tabs: vec!["3".into()],
                active: "3".into(),
            },
        ],
        tabs: vec![
            PersistedTab {
                id: "1".into(),
                kind: PersistedKind::Note {
                    mode: flavite_workspace::NoteMode::Edit,
                    locator: vec!["Systems".into(), "Feedback loops.md".into()],
                },
            },
            PersistedTab {
                id: "2".into(),
                kind: PersistedKind::Backlinks {
                    binding: PersistedBinding::Pinned {
                        path: vec!["alpha.md".into()],
                    },
                },
            },
            PersistedTab {
                id: "3".into(),
                kind: PersistedKind::Empty,
            },
        ],
        focus: "2".into(),
        maximized: None,
        recently_closed: vec![vec!["beta.md".into()]],
        named_layouts: Vec::new(),
    }
}

#[test]
fn golden_round_trip_preserves_every_durable_field() {
    let workspace = sample();
    let bytes = encode_workspace(&workspace).expect("encodes");
    let decoded = decode_workspace(&bytes, VAULT).expect("decodes");
    assert_eq!(decoded, workspace);
    // Non-canonical envelope bytes still decode: the checksum is over the
    // canonical payload, not the file formatting.
    let payload_value = serde_json::to_value(&workspace).unwrap();
    let canonical = serde_json::to_vec(&payload_value).unwrap();
    let shuffled = serde_json::to_vec(&serde_json::json!({
        "payload": payload_value,
        "checksum": blake3::hash(&canonical).to_hex().to_string(),
    }))
    .unwrap();
    assert_eq!(decode_workspace(&shuffled, VAULT).unwrap(), workspace);
}

#[test]
fn graph_scope_round_trips_and_invalid_filter_bounds_cannot_be_restored() {
    use flavite_workspace::{Direction, GraphScope, Neighborhood, NotePredicate};
    let mut workspace = sample();
    workspace.tabs[2].kind = PersistedKind::Graph {
        camera: None,
        scope: GraphScope {
            predicate: NotePredicate::And {
                clauses: vec![
                    NotePredicate::Tag {
                        name: "systems".into(),
                    },
                    NotePredicate::SavedQuery {
                        query: "feedback".into(),
                    },
                ],
            },
            neighborhood: Some(Neighborhood {
                root_native: b"Systems/Root.md".to_vec(),
                depth: 2,
                direction: Direction::Both,
            }),
            ..Default::default()
        },
    };
    let bytes = encode_workspace(&workspace).unwrap();
    assert_eq!(decode_workspace(&bytes, VAULT).unwrap(), workspace);
    if let PersistedKind::Graph { scope, .. } = &mut workspace.tabs[2].kind {
        scope.neighborhood.as_mut().unwrap().depth = 7;
    }
    let bytes = encode_workspace(&workspace).unwrap();
    assert!(decode_workspace(&bytes, VAULT).is_err());
}

#[test]
fn tampered_payload_fails_the_checksum() {
    let workspace = sample();
    let bytes = encode_workspace(&workspace).expect("encodes");
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    // Flip one durable field inside the payload.
    value["payload"]["focus"] = serde_json::json!("t3");
    let tampered = serde_json::to_vec(&value).unwrap();
    assert_eq!(
        decode_workspace(&tampered, VAULT),
        Err(CodecError::ChecksumMismatch),
        "the canonical payload no longer matches its checksum"
    );
}

#[test]
fn vault_mismatch_is_refused() {
    let workspace = sample();
    let bytes = encode_workspace(&workspace).expect("encodes");
    assert!(matches!(
        decode_workspace(&bytes, "99999999-9999-9999-9999-999999999999"),
        Err(CodecError::VaultMismatch { .. })
    ));
}

#[test]
fn structural_violations_are_refused() {
    let mut duplicate_tabs = sample();
    duplicate_tabs.tabs[1].id = "1".into();
    assert!(matches!(
        validate_workspace(&duplicate_tabs),
        Err(CodecError::InvalidStructure(message)) if message.contains("duplicate tab")
    ));

    let mut foreign_active = sample();
    foreign_active.groups[0].active = "3".into();
    assert!(matches!(
        validate_workspace(&foreign_active),
        Err(CodecError::InvalidStructure(message)) if message.contains("active tab")
    ));

    let mut unknown_tab = sample();
    unknown_tab.groups[0].tabs.push("999".into());
    assert!(matches!(
        validate_workspace(&unknown_tab),
        Err(CodecError::InvalidStructure(message)) if message.contains("unknown tab")
    ));

    let mut bad_ratio = sample();
    if let PersistedNode::Split { ratio_ppm, .. } = &mut bad_ratio.tree {
        *ratio_ppm = 5;
    }
    assert!(matches!(
        validate_workspace(&bad_ratio),
        Err(CodecError::InvalidStructure(message)) if message.contains("ratio")
    ));

    let mut unknown_group_leaf = sample();
    unknown_group_leaf.tree = PersistedNode::Group { id: "ghost".into() };
    assert!(matches!(
        validate_workspace(&unknown_group_leaf),
        Err(CodecError::InvalidStructure(message)) if message.contains("unknown group")
    ));

    let mut orphan_tab = sample();
    orphan_tab.tabs.push(PersistedTab {
        id: "9".into(),
        kind: PersistedKind::Empty,
    });
    assert!(matches!(
        validate_workspace(&orphan_tab),
        Err(CodecError::InvalidStructure(message)) if message.contains("no group")
    ));
}

#[test]
fn future_major_is_refused_and_never_overwritten_by_the_caller_contract() {
    let mut workspace = sample();
    workspace.schema = CURRENT_SCHEMA + 1;
    let bytes = encode_workspace(&workspace).expect("encodes");
    assert!(matches!(
        decode_workspace(&bytes, VAULT),
        Err(CodecError::FutureSchema { .. })
    ));
}

#[test]
fn giant_input_is_refused_before_parsing() {
    let giant = vec![b'a'; MAX_WORKSPACE_FILE_BYTES + 1];
    assert!(matches!(
        decode_workspace(&giant, VAULT),
        Err(CodecError::TooLarge { .. })
    ));
    // Deep nesting is rejected by the preflight before recursive parse.
    let deep = format!("{}{}", "[".repeat(200), "]".repeat(200));
    assert!(matches!(
        decode_workspace(deep.as_bytes(), VAULT),
        Err(CodecError::TooDeep { .. })
    ));
}

#[test]
fn unknown_tab_kinds_round_trip_inert() {
    let mut workspace = sample();
    workspace.tabs.push(PersistedTab {
        id: "7".into(),
        kind: PersistedKind::Other {
            name: "graph".into(),
            data: serde_json::json!({ "epoch": 4, "nodes": [1, 2, 3] }),
        },
    });
    workspace.groups[1].tabs.push("7".into());
    let bytes = encode_workspace(&workspace).expect("encodes");
    let decoded = decode_workspace(&bytes, VAULT).expect("decodes");
    match &decoded.tabs[3].kind {
        PersistedKind::Other { name, data } => {
            assert_eq!(name, "graph");
            assert_eq!(data["epoch"], 4);
        }
        other => panic!("unknown kind was not preserved: {other:?}"),
    }
}

#[test]
fn non_utf8_native_names_are_refused_not_percent_encoded() {
    // D-06: the app never invents encodings. A locator component that only
    // exists through lossy conversion is not representable and the codec
    // keeps raw bytes out of the JSON payload entirely.
    let mut workspace = sample();
    workspace.tabs[0].kind = PersistedKind::Note {
        mode: flavite_workspace::NoteMode::Edit,
        locator: vec!["a\0b".into()],
    };
    let bytes = encode_workspace(&workspace).expect("encodes");
    let decoded = decode_workspace(&bytes, VAULT).expect("decodes");
    // The codec stores the value verbatim; the LOCATOR validation happens
    // at restore time, where NUL components are rejected, never guessed.
    let restore = flavite_workspace::plan_restore(&decoded, &BTreeSet::new()).expect("plan builds");
    assert!(
        restore
            .report
            .iter()
            .any(|entry| entry.contains("was dropped")),
        "the unrepresentable tab is reported, not repaired: {:?}",
        restore.report
    );
}

#[test]
fn named_layouts_round_trip_and_are_independently_validated() {
    let mut workspace = sample();
    workspace.named_layouts = vec![NamedLayout {
        name: "Writing".into(),
        workspace: sample(),
    }];
    let bytes = encode_workspace(&workspace).expect("encodes");
    let decoded = decode_workspace(&bytes, VAULT).expect("decodes");
    assert_eq!(decoded.named_layouts.len(), 1);
    assert_eq!(decoded.named_layouts[0].name, "Writing");
    assert_eq!(decoded.named_layouts[0].workspace, sample());

    // A named layout referencing a foreign vault is refused (D-11: imports
    // validate all locators against the destination vault).
    let mut foreign = sample();
    foreign.vault_uuid = "99999999-9999-9999-9999-999999999999".into();
    let mut workspace = sample();
    workspace.named_layouts = vec![NamedLayout {
        name: "Foreign".into(),
        workspace: foreign,
    }];
    let bytes = encode_workspace(&workspace).expect("encodes");
    assert!(matches!(
        decode_workspace(&bytes, VAULT),
        Err(CodecError::VaultMismatch { .. })
    ));
}

#[test]
fn restore_plan_reports_missing_notes_and_drops_invalid_identities() {
    let mut workspace = sample();
    workspace.tabs.push(PersistedTab {
        id: "not-a-number".into(),
        kind: PersistedKind::Empty,
    });
    workspace.groups[0].tabs.push("not-a-number".into());
    workspace.tabs.push(PersistedTab {
        id: "8".into(),
        kind: PersistedKind::Note {
            mode: flavite_workspace::NoteMode::Edit,
            locator: vec!["gone.md".into()],
        },
    });
    workspace.groups[1].tabs.push("8".into());
    let catalog: BTreeSet<String> = ["Systems/Feedback loops.md".to_owned()].into();
    let plan = flavite_workspace::plan_restore(&workspace, &catalog).expect("plan builds");
    assert!(
        plan.report
            .iter()
            .any(|entry| entry.contains("invalid identity")),
        "invalid identities are reported: {:?}",
        plan.report
    );
    assert!(
        plan.missing_notes
            .iter()
            .any(|locator| locator.components().join("/") == "gone.md"),
        "missing notes are reported for recovery: {:?}",
        plan.missing_notes
    );
    assert!(
        !plan
            .missing_notes
            .iter()
            .any(|locator| locator.components().join("/") == "Systems/Feedback loops.md"),
        "catalogued notes are not reported missing"
    );
}

#[test]
fn settings_tab_round_trips_through_the_codec() {
    // RFC-0007 S6: the Settings tab is a stateless durable kind.
    let mut workspace = sample();
    workspace.tabs.push(PersistedTab {
        id: "4".into(),
        kind: PersistedKind::Settings,
    });
    workspace.groups[1].tabs.push("4".into());
    workspace.groups[1].active = "4".into();
    let bytes = encode_workspace(&workspace).expect("encodes");
    let decoded = decode_workspace(&bytes, VAULT).expect("decodes");
    assert_eq!(decoded, workspace);

    // Restore rehydrates the Settings tab like every other kind.
    let catalog = BTreeSet::new();
    let plan = flavite_workspace::plan_restore(&decoded, &catalog).expect("plan");
    assert!(
        plan.report.is_empty(),
        "no drops expected: {:?}",
        plan.report
    );
    let restored = plan
        .tabs
        .iter()
        .find(|tab| tab.id.raw() == 4)
        .expect("settings tab restored");
    assert_eq!(restored.kind, flavite_workspace::ViewKind::Settings);
}

#[test]
fn codec_error_display_renders_every_variant_verbatim() {
    let cases: Vec<(CodecError, &str)> = vec![
        (CodecError::TooLarge { size: 5 }, "5 bytes"),
        (CodecError::TooDeep { depth: 99 }, "depth 99"),
        (
            CodecError::Malformed("bad json".into()),
            "malformed: bad json",
        ),
        (CodecError::ChecksumMismatch, "checksum does not match"),
        (CodecError::FutureSchema { found: 9 }, "schema 9 is newer"),
        (
            CodecError::UnsupportedSchema { found: 0 },
            "schema 0 is not supported",
        ),
        (
            CodecError::VaultMismatch {
                expected: "a".into(),
                found: "b".into(),
            },
            "vault b, expected a",
        ),
        (
            CodecError::InvalidStructure("dup".into()),
            "violates its invariants: dup",
        ),
    ];
    for (error, needle) in cases {
        let rendered = error.to_string();
        assert!(
            rendered.contains(needle),
            "{rendered} should contain {needle:?}"
        );
        // The error trait object is usable too.
        let boxed: Box<dyn std::error::Error> = Box::new(error);
        assert!(!boxed.to_string().is_empty());
    }
}

#[test]
fn schema_bounds_are_refused_before_any_structure_check() {
    let mut old = sample();
    old.schema = MIN_SUPPORTED_SCHEMA - 1;
    assert!(matches!(
        validate_workspace(&old),
        Err(CodecError::UnsupportedSchema { found: 0 })
    ));
    let mut future = sample();
    future.schema = CURRENT_SCHEMA + 1;
    assert!(matches!(
        validate_workspace(&future),
        Err(CodecError::FutureSchema { found: 2 })
    ));
}

#[test]
fn structural_identity_violations_are_refused() {
    // Empty tab id.
    let mut empty_id = sample();
    empty_id.tabs[0].id = String::new();
    assert!(matches!(
        validate_workspace(&empty_id),
        Err(CodecError::InvalidStructure(message)) if message.contains("empty tab id")
    ));

    // Duplicate group id.
    let mut dup_group = sample();
    dup_group.groups[1].id = "11".into();
    assert!(matches!(
        validate_workspace(&dup_group),
        Err(CodecError::InvalidStructure(message)) if message.contains("duplicate group id")
    ));

    // Empty group.
    let mut empty_group = sample();
    empty_group.groups[0].tabs.clear();
    assert!(matches!(
        validate_workspace(&empty_group),
        Err(CodecError::InvalidStructure(message)) if message.contains("is empty")
    ));

    // One tab listed by two groups.
    let mut two_groups = sample();
    two_groups.groups[1].tabs.push("1".into());
    assert!(matches!(
        validate_workspace(&two_groups),
        Err(CodecError::InvalidStructure(message)) if message.contains("more than one group")
    ));

    // Unknown focus and unknown maximized group.
    let mut bad_focus = sample();
    bad_focus.focus = "999".into();
    assert!(matches!(
        validate_workspace(&bad_focus),
        Err(CodecError::InvalidStructure(message)) if message.contains("focus 999")
    ));
    let mut bad_max = sample();
    bad_max.maximized = Some("999".into());
    assert!(matches!(
        validate_workspace(&bad_max),
        Err(CodecError::InvalidStructure(message)) if message.contains("maximized group 999")
    ));

    // Empty named-layout name.
    let mut blank_name = sample();
    blank_name.named_layouts = vec![NamedLayout {
        name: "   ".into(),
        workspace: sample(),
    }];
    assert!(matches!(
        validate_workspace(&blank_name),
        Err(CodecError::InvalidStructure(message)) if message.contains("empty layout name")
    ));
}

#[test]
fn graph_camera_and_scope_import_bounds_are_refused_not_clamped() {
    use flavite_workspace::codec::PersistedGraphCamera;

    // Non-finite camera component.
    let mut non_finite = sample();
    bad_finite_camera(&mut non_finite);
    assert!(matches!(
        validate_workspace(&non_finite),
        Err(CodecError::InvalidStructure(message)) if message.contains("not finite")
    ));

    // Camera outside the world/zoom range.
    let mut out_of_range = sample();
    out_of_range.tabs[2].kind = PersistedKind::Graph {
        camera: Some(PersistedGraphCamera {
            center: [2.0e9, 0.0],
            zoom: 1.0,
        }),
        scope: GraphScope::default(),
    };
    assert!(matches!(
        validate_workspace(&out_of_range),
        Err(CodecError::InvalidStructure(message)) if message.contains("outside the supported range")
    ));

    // Scope exceeding its clause/depth/value bounds.
    let mut bad_scope = sample();
    bad_scope.tabs[2].kind = PersistedKind::Graph {
        camera: None,
        scope: GraphScope {
            neighborhood: Some(Neighborhood {
                root_native: b"a.md".to_vec(),
                depth: 7,
                direction: Direction::Both,
            }),
            ..GraphScope::default()
        },
    };
    assert!(matches!(
        validate_workspace(&bad_scope),
        Err(CodecError::InvalidStructure(_))
    ));
}

fn bad_finite_camera(workspace: &mut PersistedWorkspace) {
    use flavite_workspace::codec::PersistedGraphCamera;
    workspace.tabs[2].kind = PersistedKind::Graph {
        camera: Some(PersistedGraphCamera {
            center: [f32::NAN, 0.0],
            zoom: 1.0,
        }),
        scope: GraphScope::default(),
    };
}

#[test]
fn tree_shape_violations_are_refused() {
    // Duplicate split id.
    let mut dup_split = sample();
    dup_split.tree = PersistedNode::Split {
        id: "21".into(),
        arrangement: "side_by_side".into(),
        ratio_ppm: 500_000,
        first: Box::new(PersistedNode::Split {
            id: "21".into(),
            arrangement: "stacked".into(),
            ratio_ppm: 500_000,
            first: Box::new(PersistedNode::Group { id: "11".into() }),
            second: Box::new(PersistedNode::Group { id: "12".into() }),
        }),
        second: Box::new(PersistedNode::Group { id: "12".into() }),
    };
    assert!(matches!(
        validate_workspace(&dup_split),
        Err(CodecError::InvalidStructure(message)) if message.contains("duplicate split id")
    ));

    // Unknown arrangement tag.
    let mut weird = sample();
    if let PersistedNode::Split { arrangement, .. } = &mut weird.tree {
        *arrangement = "diagonal".into();
    }
    assert!(matches!(
        validate_workspace(&weird),
        Err(CodecError::InvalidStructure(message)) if message.contains("unknown arrangement")
    ));

    // Tree deeper than the bound.
    let mut node = PersistedNode::Group { id: "11".into() };
    for raw in 0..20u32 {
        node = PersistedNode::Split {
            id: format!("9{raw}"),
            arrangement: "stacked".into(),
            ratio_ppm: 500_000,
            first: Box::new(node),
            second: Box::new(PersistedNode::Group { id: "12".into() }),
        };
    }
    let mut deep = sample();
    deep.tree = node;
    assert!(matches!(
        validate_workspace(&deep),
        Err(CodecError::InvalidStructure(message)) if message.contains("depth exceeds")
    ));
}

#[test]
fn encode_refuses_payloads_over_the_byte_bound() {
    let mut huge = sample();
    huge.named_layouts = vec![NamedLayout {
        name: "x".repeat(MAX_WORKSPACE_FILE_BYTES),
        workspace: sample(),
    }];
    assert!(matches!(
        encode_workspace(&huge),
        Err(CodecError::TooLarge { .. })
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn every_live_view_kind_persists_and_round_trips() {
    use flavite_workspace::{
        Arrangement, ContextBinding, ContextViewKind, EntityLocator, GraphCamera, LayoutAction,
        NoteLocator, Outcome, ViewKind, WorkspaceState, reduce,
    };

    let mut state = WorkspaceState::new();
    let apply = |state: &mut WorkspaceState, action: LayoutAction| {
        let Outcome::Committed(commit) = reduce(state, action) else {
            panic!("action rejected")
        };
        *state = commit.state;
    };
    let group = state.first_group().unwrap();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::OpenNote {
            expected_revision: revision,
            locator: NoteLocator::from_components(["a.md"]).unwrap(),
            group,
        },
    );
    let note = state.focused_tab();
    for action in [
        LayoutAction::OpenNotes {
            expected_revision: 0,
            group,
        },
        LayoutAction::OpenSearch {
            expected_revision: 0,
            group,
        },
        LayoutAction::OpenReplace {
            expected_revision: 0,
            group,
        },
        LayoutAction::OpenSettings {
            expected_revision: 0,
            group,
        },
        LayoutAction::OpenHistory {
            expected_revision: 0,
            locator: NoteLocator::from_components(["a.md"]).unwrap(),
            group,
        },
        LayoutAction::OpenGraph {
            expected_revision: 0,
            group,
        },
        LayoutAction::OpenContext {
            expected_revision: 0,
            view: ContextViewKind::Preview,
            binding: ContextBinding::FollowDocument,
            group,
        },
        LayoutAction::OpenContext {
            expected_revision: 0,
            view: ContextViewKind::Backlinks,
            binding: ContextBinding::FollowSource { tab: note },
            group,
        },
        LayoutAction::OpenContext {
            expected_revision: 0,
            view: ContextViewKind::Outline,
            binding: ContextBinding::Pinned(EntityLocator::new(
                NoteLocator::from_components(["p.md"]).unwrap(),
            )),
            group,
        },
        LayoutAction::OpenContext {
            expected_revision: 0,
            view: ContextViewKind::Properties,
            binding: ContextBinding::FollowExplorationOrDocument,
            group,
        },
    ] {
        let revision = state.revision();
        let action = match action {
            LayoutAction::OpenNotes { group, .. } => LayoutAction::OpenNotes {
                expected_revision: revision,
                group,
            },
            LayoutAction::OpenSearch { group, .. } => LayoutAction::OpenSearch {
                expected_revision: revision,
                group,
            },
            LayoutAction::OpenReplace { group, .. } => LayoutAction::OpenReplace {
                expected_revision: revision,
                group,
            },
            LayoutAction::OpenSettings { group, .. } => LayoutAction::OpenSettings {
                expected_revision: revision,
                group,
            },
            LayoutAction::OpenHistory { locator, group, .. } => LayoutAction::OpenHistory {
                expected_revision: revision,
                locator,
                group,
            },
            LayoutAction::OpenGraph { group, .. } => LayoutAction::OpenGraph {
                expected_revision: revision,
                group,
            },
            LayoutAction::OpenContext {
                view,
                binding,
                group,
                ..
            } => LayoutAction::OpenContext {
                expected_revision: revision,
                view,
                binding,
                group,
            },
            other => other,
        };
        apply(&mut state, action);
    }
    // Move the graph camera off-center so the persisted camera carries a payload.
    let graph = state
        .tabs()
        .find(|(_, session)| matches!(session.kind(), ViewKind::Graph(_)))
        .map(|(tab, _)| *tab)
        .unwrap();
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::SetGraphCamera {
            expected_revision: revision,
            tab: graph,
            camera: GraphCamera::new([3.0, 4.0], 2.0).unwrap(),
        },
    );
    // A stacked split exercises the second arrangement tag.
    let revision = state.revision();
    apply(
        &mut state,
        LayoutAction::SplitGroup {
            expected_revision: revision,
            group,
            arrangement: Arrangement::Stacked,
        },
    );

    let persisted = flavite_workspace::persist_workspace(&state, VAULT);
    let has = |name: &str, pred: fn(&PersistedKind) -> bool| {
        assert!(
            persisted.tabs.iter().any(|tab| pred(&tab.kind)),
            "missing persisted kind {name}"
        );
    };
    has("note", |k| matches!(k, PersistedKind::Note { .. }));
    has("notes", |k| matches!(k, PersistedKind::Notes));
    has("search", |k| matches!(k, PersistedKind::Search));
    has("replace", |k| matches!(k, PersistedKind::Replace));
    has("settings", |k| matches!(k, PersistedKind::Settings));
    has("history", |k| matches!(k, PersistedKind::History { .. }));
    has("graph", |k| {
        matches!(
            k,
            PersistedKind::Graph {
                camera: Some(_),
                ..
            }
        )
    });
    has("preview", |k| {
        matches!(
            k,
            PersistedKind::Preview {
                binding: PersistedBinding::FollowDocument
            }
        )
    });
    has("backlinks", |k| {
        matches!(
            k,
            PersistedKind::Backlinks {
                binding: PersistedBinding::FollowSource { .. }
            }
        )
    });
    has("outline", |k| {
        matches!(
            k,
            PersistedKind::Outline {
                binding: PersistedBinding::Pinned { .. }
            }
        )
    });
    has("properties", |k| {
        matches!(
            k,
            PersistedKind::Properties {
                binding: PersistedBinding::FollowExplorationOrDocument
            }
        )
    });
    assert!(
        serde_json::to_value(&persisted.tree)
            .unwrap()
            .to_string()
            .contains("stacked"),
        "the stacked arrangement persists"
    );
    let bytes = encode_workspace(&persisted).unwrap();
    assert_eq!(decode_workspace(&bytes, VAULT).unwrap(), persisted);
}
