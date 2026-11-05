//! `text.prepare_replace` / `text.apply_replace` (RFC-0006 S01): real
//! Session, plan replay/expiry, revision changes across the worker barrier,
//! unauthorized buffers, and exactly zero effect on every refusal.

#[path = "support/links.rs"]
mod support;

use flavite_api::{ExecutionResult, Flavite, LocalResult, Origin, command::Invocation};
use flavite_buffer::revision::BufferId;

fn prepare(
    flavite: &Flavite,
    buffer_id: u64,
    revision: u64,
    query: &str,
    replacement: &str,
    _scope: Option<(usize, usize)>,
) -> ExecutionResult {
    let request_id = flavite.command_registry().next_request_id();
    let result = flavite.execute(Invocation::new(
        request_id,
        "text.prepare_replace",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "query": query,
            "replacement": replacement,
        }),
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
        other => other,
    }
}

fn prepared_plan(result: &ExecutionResult) -> (String, u64) {
    let ExecutionResult::Local(LocalResult::Success(success)) = result else {
        panic!("prepare should succeed, got {result:?}");
    };
    let plan_id = success
        .data
        .get("plan_id")
        .and_then(serde_json::Value::as_str)
        .expect("plan_id")
        .to_owned();
    let count = success
        .data
        .get("match_count")
        .and_then(serde_json::Value::as_u64)
        .expect("match_count");
    (plan_id, count)
}

fn apply(flavite: &Flavite, buffer_id: u64, revision: u64, plan_id: &str) -> ExecutionResult {
    let request_id = flavite.command_registry().next_request_id();
    let result = flavite.execute(Invocation::new(
        request_id,
        "text.apply_replace",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "plan_id": plan_id,
        }),
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
        other => other,
    }
}

fn view(flavite: &Flavite, buffer_id: u64) -> (String, u64) {
    let view = flavite
        .get_buffer_view(BufferId::from(buffer_id), None)
        .expect("buffer view");
    (view.text, view.revision)
}

#[test]
fn prepare_and_apply_replace_all_in_one_isolated_batch() {
    let fixture = support::fixture(&[("notes/a.md", "alpha beta alpha\n")]);
    let flavite = &fixture.flavite;
    let buffer_id = fixture.open_note("notes/a.md").0;
    let (text, revision) = view(flavite, buffer_id);
    assert_eq!(text, "alpha beta alpha\n");
    let result = prepare(flavite, buffer_id, revision, "alpha", "X", None);
    let (plan_id, count) = prepared_plan(&result);
    assert_eq!(count, 2, "exact match count, no silent cap");
    let applied = apply(flavite, buffer_id, revision, &plan_id);
    let ExecutionResult::Local(LocalResult::Success(success)) = applied else {
        panic!("apply should succeed, got {applied:?}");
    };
    let new_revision = success
        .data
        .get("revision")
        .and_then(serde_json::Value::as_u64)
        .expect("revision");
    assert!(new_revision > revision);
    let (text, _) = view(flavite, buffer_id);
    assert_eq!(text, "X beta X\n");
}

#[test]
fn duplicate_apply_replays_the_same_receipt() {
    let fixture = support::fixture(&[("notes/a.md", "one one\n")]);
    let flavite = &fixture.flavite;
    let buffer_id = fixture.open_note("notes/a.md").0;
    let (_text, revision) = view(flavite, buffer_id);
    let (plan_id, _count) = prepared_plan(&prepare(flavite, buffer_id, revision, "one", "1", None));
    let first = apply(flavite, buffer_id, revision, &plan_id);
    assert!(first.is_success(), "first apply succeeds");
    // §6.9: the same request replays the same receipt — never a second
    // effect against the already-replaced text.
    let replay = apply(flavite, buffer_id, revision, &plan_id);
    assert_eq!(
        first.into_success().map(|success| success.data),
        replay.clone().into_success().map(|success| success.data)
    );
    assert!(
        replay.is_success(),
        "replay succeeds with the stored receipt"
    );
    let (replayed_text, _) = view(flavite, buffer_id);
    assert_eq!(replayed_text, "1 1\n", "no double replacement");
}

#[test]
fn stale_revision_across_the_worker_barrier_has_zero_effect() {
    let fixture = support::fixture(&[("notes/a.md", "target target\n")]);
    let flavite = &fixture.flavite;
    let buffer_id = fixture.open_note("notes/a.md").0;
    let (_, revision) = view(flavite, buffer_id);
    let (plan_id, _count) = prepared_plan(&prepare(
        flavite, buffer_id, revision, "target", "hit", None,
    ));
    // Change the revision while the plan exists (the worker barrier ran).
    let request_id = flavite.command_registry().next_request_id();
    let _ = flavite.execute(Invocation::new(
        request_id,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "ops": [{"range": [0, 0], "replacement": "prefix "}],
            "undo_policy": "typing",
        }),
        Origin::Gui,
    ));
    let applied = apply(flavite, buffer_id, revision, &plan_id);
    let ExecutionResult::Rejected(rejected) = applied else {
        panic!("stale revision must be rejected, got {applied:?}");
    };
    assert_eq!(
        rejected.reason,
        flavite_api::RejectionReason::StaleRevision,
        "the plan is bound to its exact revision"
    );
    let (text, _) = view(flavite, buffer_id);
    assert_eq!(text, "prefix target target\n", "zero replacement effect");
}

#[test]
fn unknown_or_foreign_plan_ids_have_zero_effect() {
    let fixture = support::fixture(&[("notes/a.md", "x y\n"), ("notes/b.md", "x\n")]);
    let flavite = &fixture.flavite;
    let a = fixture.open_note("notes/a.md").0;
    let b = fixture.open_note("notes/b.md").0;
    let (_, revision) = view(flavite, a);
    let (plan_id, _count) = prepared_plan(&prepare(flavite, a, revision, "x", "y", None));
    // A plan applied against another buffer is refused outright.
    let foreign = apply(flavite, b, revision, &plan_id);
    assert!(
        !foreign.is_success(),
        "foreign buffer is refused: {foreign:?}"
    );
    // A fabricated id is Gone, never "match nothing".
    let unknown = apply(flavite, a, revision, &"0".repeat(32));
    let ExecutionResult::ActionRequired { code, .. } = unknown else {
        panic!("unknown plan is a typed gone/expire, got {unknown:?}");
    };
    assert_eq!(code, "text-plan-expired");
    let (text_a, _) = view(flavite, a);
    let (text_b, _) = view(flavite, b);
    assert_eq!(text_a, "x y\n");
    assert_eq!(text_b, "x\n");
}

#[test]
fn empty_query_is_idle_and_invalid_scopes_are_refused_with_zero_effect() {
    let fixture = support::fixture(&[("notes/a.md", "abc\n")]);
    let flavite = &fixture.flavite;
    let buffer_id = fixture.open_note("notes/a.md").0;
    let (_, revision) = view(flavite, buffer_id);
    let result = prepare(flavite, buffer_id, revision, "", "X", None);
    let ExecutionResult::Local(LocalResult::Error(error)) = result else {
        panic!("empty query is Idle, refused typed, got {result:?}");
    };
    assert!(
        error.message.contains("idle"),
        "the typed message says find is idle: {}",
        error.message
    );
    // Out-of-bounds scope: typed refusal, zero effects.
    let request_id = flavite.command_registry().next_request_id();
    let result = flavite.execute(Invocation::new(
        request_id,
        "text.prepare_replace",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "query": "abc",
            "replacement": "X",
            "scope": [0, 9999],
        }),
        Origin::Gui,
    ));
    let result = match result {
        ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
        other => other,
    };
    assert!(!result.is_success(), "invalid scope refused: {result:?}");
    let (text, _) = view(flavite, buffer_id);
    assert_eq!(text, "abc\n", "zero note effects");
}
