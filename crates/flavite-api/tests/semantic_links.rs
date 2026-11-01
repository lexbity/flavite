//! Semantic link insertion end-to-end (RFC-0003 §9/§21, W05).
//!
//! Real Session/temporary vault/parser/resolver: exact source bytes, target
//! unchanged, single Undo/Redo, same-title notes, native paths, duplicate
//! intents, stale sources, index changes, capability and read-only policy,
//! and the raw-shape negative test. AC-W04/05 zero-mutation outcomes are
//! asserted after every rejected transition.

use flavite_api::{
    API_VERSION, ExecutionResult, Invocation, LocalResult, Origin, Preconditions, RejectionReason,
};
use flavite_fs::Fs;

#[path = "support/links.rs"]
mod links_fixture;
use links_fixture::*;

#[test]
fn semantic_proof_keeps_session_coordinates_after_preserved_carriage_returns() {
    let f = fixture(&[
        ("source.md", "\r\r\nBefore after\n"),
        ("target.md", "Target\n"),
    ]);
    let (buffer, revision) = f.open_note("source.md");
    let original = f.text(buffer);
    let caret = original.find("after").unwrap();
    let (_, plan, _) = f.prepare(1, buffer, revision, caret, "target.md");
    assert!(f.commit(1, plan).is_success());
    assert_eq!(
        f.text(buffer),
        original.replacen("after", "[[target]]after", 1)
    );
    assert_eq!(f.saved_bytes("target.md"), b"Target\n");
}

#[test]
fn cancellation_releases_capacity_and_old_plan_cannot_mutate_a_new_gesture() {
    let fixture = fixture(&[("source.md", "Text."), ("target.md", "Target.")]);
    let (buffer, revision) = fixture.open_note("source.md");
    let (_, plan, _) = fixture.prepare(1, buffer, revision, 5, "target.md");
    fixture.flavite.cancel_link_intent(fixture.client, 1);
    let (_, new_plan, _) = fixture.prepare(2, buffer, revision, 5, "target.md");
    assert!(!fixture.commit(1, plan).is_success());
    assert_eq!(fixture.text(buffer), "Text.");
    assert!(fixture.commit(2, new_plan).is_success());
    assert_eq!(fixture.text(buffer), "Text.[[target]]");
}

#[test]
#[allow(clippy::too_many_lines)]
fn prepare_commit_undo_redo_and_target_immutability() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let target_before = fixture.saved_bytes("target.md");

    let (sequence, plan_id, serialized) = fixture.prepare(1, buffer_id, revision, 6, "target.md");
    assert_eq!(serialized, "[[target]]");

    let result = fixture.commit(sequence, plan_id);
    let ExecutionResult::Local(LocalResult::Success(success)) = &result else {
        panic!("commit must succeed: {result:?}");
    };
    let new_revision = success.data["revision"].as_u64().expect("new revision");
    assert_eq!(
        fixture.text(buffer_id),
        "Hello [[target]]world.\n",
        "exactly one occurrence at the caret"
    );
    assert_eq!(
        fixture.saved_bytes("target.md"),
        target_before,
        "the target note is unchanged by insertion"
    );

    // One undo removes it; redo restores it.
    let undo = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "undo",
        serde_json::json!({ "buffer_id": buffer_id, "revision": new_revision }),
        Origin::Gui,
    ));
    assert!(undo.is_success(), "undo: {undo:?}");
    assert_eq!(fixture.text(buffer_id), "Hello world.\n");
    let view = fixture
        .flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
        .expect("view");
    let redo = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "redo",
        serde_json::json!({ "buffer_id": buffer_id, "revision": view.revision }),
        Origin::Gui,
    ));
    assert!(redo.is_success(), "redo: {redo:?}");
    assert_eq!(fixture.text(buffer_id), "Hello [[target]]world.\n");
}

#[test]
fn same_title_notes_need_explicit_paths_and_stems_are_ambiguous() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("Systems/Emergence.md", "systems emergence\n"),
        ("Ideas/Emergence.md", "ideas emergence\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");

    // The exact path resolves unambiguously.
    let (sequence, plan_id, serialized) =
        fixture.prepare(1, buffer_id, revision, 6, "Systems/Emergence.md");
    assert_eq!(serialized, "[[Systems/Emergence]]");
    assert!(fixture.commit(sequence, plan_id).is_success());

    // The bare stem is refused: the app never guesses between same titles.
    let request_id = next_id(&fixture.flavite);
    let result = fixture.flavite.execute(Invocation::with_preconditions(
        request_id,
        "link.prepare",
        serde_json::json!({
            "intent": { "client": fixture.client, "sequence": 2 },
            "source": { "buffer_id": buffer_id, "revision": revision + 1, "caret": 6 },
            "target_path": "Emergence",
            "expected_target": {"epoch":1,"entity_id":0,"native_path":[]},
            "stable_link_policy": "disabled",
        }),
        Origin::Gui,
        Preconditions::new()
            .with_buffer_id(buffer_id)
            .with_revision(revision + 1)
            .requires_write(),
    ));
    let pending = match result {
        ExecutionResult::Pending { request_id } => request_id,
        other => panic!("prepare must be admitted: {other:?}"),
    };
    let completion = fixture.flavite.wait_for_operation(pending);
    assert_eq!(error_code(&completion), "link.ambiguous_target");
    assert_eq!(
        fixture.text(buffer_id),
        "Hello [[Systems/Emergence]]world.\n"
    );
}

#[test]
fn duplicate_intents_replay_cached_results_without_second_insert() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let (sequence, plan_id, _) = fixture.prepare(41, buffer_id, revision, 6, "target.md");

    assert!(fixture.commit(sequence, plan_id).is_success());
    let once = fixture.text(buffer_id);
    // The identical gesture replays its cached result: no second insert.
    let replay = fixture.commit(sequence, plan_id);
    assert!(replay.is_success(), "cached replay: {replay:?}");
    assert_eq!(fixture.text(buffer_id), once);

    // A different payload under a consumed gesture is a mismatch.
    assert_eq!(
        error_code(&fixture.commit(sequence, plan_id + 999)),
        "link.intent_mismatch"
    );
    assert_eq!(fixture.text(buffer_id), once);

    // An unconsumed sequence below the high-water mark is expired.
    assert_eq!(
        error_code(&fixture.commit(sequence - 1, plan_id)),
        "link.intent_expired"
    );
}

#[test]
fn stale_source_after_edit_rejects_with_zero_mutation() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let (sequence, plan_id, _) = fixture.prepare(1, buffer_id, revision, 6, "target.md");

    // The source moves while the plan is held (confirmation window).
    let edit = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "ops": [{ "range": [0, 0], "replacement": "PREFIX " }]
        }),
        Origin::Gui,
    ));
    assert!(edit.is_success());

    let result = fixture.commit(sequence, plan_id);
    assert_eq!(error_code(&result), "link.stale_source");
    assert_eq!(
        fixture.text(buffer_id),
        "PREFIX Hello world.\n",
        "zero mutation from the rejected commit"
    );
    assert_eq!(fixture.saved_bytes("target.md"), b"Target note.\n");
}

#[test]
fn index_change_between_prepare_and_commit_retargets_nothing() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let (sequence, plan_id, _) = fixture.prepare(1, buffer_id, revision, 6, "target.md");

    // The target disappears from the vault and the index reconciles.
    fixture
        .fs
        .remove(&fixture.root.join("target.md"))
        .expect("removed");
    reconcile(&fixture.flavite);

    let result = fixture.commit(sequence, plan_id);
    assert_eq!(error_code(&result), "link.target_retargeted");
    assert_eq!(fixture.text(buffer_id), "Hello world.\n");
}

#[test]
fn non_ascii_targets_insert_and_round_trip_through_the_scanner() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("Ideas/漢字.md", "kanji\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let (sequence, plan_id, serialized) =
        fixture.prepare(1, buffer_id, revision, 6, "Ideas/漢字.md");
    assert_eq!(serialized, "[[Ideas/漢字]]");
    assert!(fixture.commit(sequence, plan_id).is_success());
    assert_eq!(fixture.text(buffer_id), "Hello [[Ideas/漢字]]world.\n");
}

#[test]
fn plugin_origin_without_capabilities_is_denied_and_read_only_vaults_refuse() {
    let mut fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");

    // A plugin origin without the declared capability is denied up front.
    let denied = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "link.prepare",
        serde_json::json!({
            "intent": { "client": fixture.client, "sequence": 1 },
            "source": { "buffer_id": buffer_id, "revision": revision, "caret": 6 },
            "target_path": "target.md",
            "expected_target": fixture.flavite.link_target_identity("target.md").unwrap(),
            "stable_link_policy": "disabled",
        }),
        Origin::Plugin,
    ));
    assert!(matches!(
        &denied,
        ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::CapabilityDenied
    ));

    // Read-only enforcement is unconditional for both commands (TD-17).
    fixture.flavite.set_read_only(true);
    let prepare = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "link.prepare",
        serde_json::json!({
            "intent": { "client": fixture.client, "sequence": 1 },
            "source": { "buffer_id": buffer_id, "revision": revision, "caret": 6 },
            "target_path": "target.md",
            "expected_target": fixture.flavite.link_target_identity("target.md").unwrap(),
            "stable_link_policy": "disabled",
        }),
        Origin::Gui,
    ));
    assert!(
        matches!(
            &prepare,
            ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::ReadOnly
        ),
        "link.prepare must be read-only rejected: {prepare:?}"
    );
    let insert = fixture.flavite.execute(Invocation::new(
        next_id(&fixture.flavite),
        "link.insert",
        serde_json::json!({
            "intent": { "client": fixture.client, "sequence": 1 },
            "plan_id": 1,
        }),
        Origin::Gui,
    ));
    assert!(
        matches!(
            &insert,
            ExecutionResult::Rejected(rejected) if rejected.reason == RejectionReason::ReadOnly
        ),
        "link.insert must be read-only rejected: {insert:?}"
    );
    assert_eq!(fixture.text(buffer_id), "Hello world.\n");
}

#[test]
fn raw_edit_shape_is_rejected_with_an_explicit_pointer() {
    let fixture = fixture(&[
        ("source.md", "Hello world.\n"),
        ("target.md", "Target note.\n"),
    ]);
    let (buffer_id, revision) = fixture.open_note("source.md");
    let rejected = fixture.flavite.execute(Invocation::with_preconditions(
        next_id(&fixture.flavite),
        "link.insert",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "ops": [{ "range": [0, 0], "replacement": "[[target]]" }]
        }),
        Origin::Gui,
        Preconditions::new()
            .with_buffer_id(buffer_id)
            .with_revision(revision)
            .requires_write(),
    ));
    match &rejected {
        ExecutionResult::Rejected(rejected) => {
            assert_eq!(rejected.reason, RejectionReason::InvalidArgs);
            assert_eq!(
                rejected.message.as_deref(),
                Some("use apply_edits for raw replacements")
            );
        }
        other => panic!("raw shape must be rejected: {other:?}"),
    }
    assert_eq!(fixture.text(buffer_id), "Hello world.\n");
}

#[test]
fn api_version_is_0_2_0_for_the_semantic_surface() {
    assert_eq!(API_VERSION, "0.2.0");
}
