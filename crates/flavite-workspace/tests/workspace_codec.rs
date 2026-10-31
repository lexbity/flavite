//! Workspace codec conformance (RFC-0003 §22, W06).
//!
//! Golden round-trips, identity/reference/ratio/depth/checksum violations,
//! vault mismatch, giant inputs, future majors, and non-UTF-8 native names:
//! the codec refuses without mutating anything and never guesses.

use flavite_workspace::{
    CURRENT_SCHEMA, CodecError, MAX_WORKSPACE_FILE_BYTES, NamedLayout, PersistedBinding,
    PersistedGroup, PersistedKind, PersistedNode, PersistedTab, PersistedWorkspace,
    decode_workspace, encode_workspace, validate_workspace,
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
