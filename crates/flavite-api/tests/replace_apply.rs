//! RFC-0006 S08 facade commands: `replace.apply/cancel/resume/job_status`
//! over a real Session/MemFs — durable apply, idempotent replay, stale
//! selection guard, read-only refusal and job status projection.

#[path = "support/links.rs"]
mod support;

use flavite_api::{ExecutionResult, Flavite, LocalResult, Origin, command::Invocation};

fn execute(flavite: &Flavite, id: &'static str, args: serde_json::Value) -> ExecutionResult {
    let request_id = flavite.command_registry().next_request_id();
    let result = flavite.execute(Invocation::new(request_id, id, args, Origin::Gui));
    match result {
        ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
        other => other,
    }
}

fn success_data(result: ExecutionResult) -> serde_json::Value {
    let ExecutionResult::Local(LocalResult::Success(success)) = result else {
        panic!("expected success, got {result:?}");
    };
    success.data
}

fn preview_all(flavite: &Flavite) -> (String, u64) {
    let preview = success_data(execute(
        flavite,
        "replace.preview",
        serde_json::json!({
            "query": "alpha",
            "replacement": "omega",
            "mode": "literal",
        }),
    ));
    let preview_id = preview["preview_id"].as_str().unwrap().to_owned();
    let selected = success_data(execute(
        flavite,
        "replace.select",
        serde_json::json!({ "preview_id": preview_id, "select": "all" }),
    ));
    let revision = selected["selection_revision"].as_u64().unwrap();
    (preview_id, revision)
}

#[test]
fn apply_seals_and_replaces_notes_durably_over_the_facade() {
    let fixture = support::fixture(&[("notes/a.md", "alpha one\n"), ("notes/b.md", "alpha two\n")]);
    let flavite = &fixture.flavite;
    let (preview_id, revision) = preview_all(flavite);
    let applied = success_data(execute(
        flavite,
        "replace.apply",
        serde_json::json!({
            "preview_id": preview_id,
            "selection_revision": revision,
            "idempotency_key": "0".repeat(32),
        }),
    ));
    let job_id = applied["job_id"].as_str().unwrap().to_owned();
    assert_eq!(applied["status"]["phase"], "completed");
    assert_eq!(applied["status"]["totals"]["saved"], 2);
    assert_eq!(applied["status"]["totals"]["conflicts"], 0);
    // Disk holds the replaced content.
    assert_eq!(fixture.saved_bytes("notes/a.md"), b"omega one\n");
    assert_eq!(fixture.saved_bytes("notes/b.md"), b"omega two\n");
    // Idempotent replay returns the same completed receipt.
    let replay = success_data(execute(
        flavite,
        "replace.apply",
        serde_json::json!({
            "preview_id": preview_id,
            "selection_revision": revision,
            "idempotency_key": "0".repeat(32),
        }),
    ));
    assert_eq!(replay["job_id"].as_str().unwrap(), job_id);
    assert_eq!(replay["status"]["phase"], "completed");
    // Job status agrees.
    let status = success_data(execute(
        flavite,
        "replace.job_status",
        serde_json::json!({ "job_id": job_id }),
    ));
    assert_eq!(status["status"]["phase"], "completed");
    assert_eq!(status["selected_items"], 2);
}

#[test]
fn stale_selection_revision_is_refused_before_any_effect() {
    let fixture = support::fixture(&[("notes/a.md", "alpha\n")]);
    let flavite = &fixture.flavite;
    let (preview_id, revision) = preview_all(flavite);
    let result = execute(
        flavite,
        "replace.apply",
        serde_json::json!({
            "preview_id": preview_id,
            "selection_revision": revision + 99,
            "idempotency_key": "1".repeat(32),
        }),
    );
    let ExecutionResult::Local(LocalResult::Error(error)) = result else {
        panic!("expected error, got {result:?}");
    };
    assert!(
        error.message.contains("selection locked"),
        "{}",
        error.message
    );
    assert_eq!(fixture.saved_bytes("notes/a.md"), b"alpha\n");
}

#[test]
fn read_only_vault_refuses_apply_while_find_stays_available() {
    let mut fixture = support::fixture(&[("notes/a.md", "alpha\n")]);
    fixture.flavite.set_read_only(true);
    let flavite = &fixture.flavite;
    // Find remains available under the read budget.
    let start = success_data(execute(
        flavite,
        "source_find.start",
        serde_json::json!({ "query": "alpha", "mode": "literal" }),
    ));
    assert_eq!(start["total_matches"], 1);
    // Preview is refused on a read-only vault (device-local report), so the
    // write gate itself must reject apply before any preview is needed.
    let result = execute(
        flavite,
        "replace.apply",
        serde_json::json!({
            "preview_id": "a".repeat(32),
            "selection_revision": 1,
            "idempotency_key": "2".repeat(32),
        }),
    );
    assert!(
        matches!(result, ExecutionResult::Rejected(_)),
        "write-requiring apply must be rejected on a read-only vault: {result:?}"
    );
}

#[test]
fn unknown_job_status_is_a_typed_action_required() {
    let fixture = support::fixture(&[("notes/a.md", "alpha\n")]);
    let result = execute(
        &fixture.flavite,
        "replace.job_status",
        serde_json::json!({ "job_id": "f".repeat(32) }),
    );
    let ExecutionResult::ActionRequired { code, .. } = result else {
        panic!("expected action required, got {result:?}");
    };
    assert_eq!(code, "job-unknown");
}

#[test]
fn apply_without_selection_is_refused() {
    let fixture = support::fixture(&[("notes/a.md", "alpha\n")]);
    let flavite = &fixture.flavite;
    let preview = success_data(execute(
        flavite,
        "replace.preview",
        serde_json::json!({
            "query": "alpha",
            "replacement": "omega",
            "mode": "literal",
        }),
    ));
    let preview_id = preview["preview_id"].as_str().unwrap().to_owned();
    let result = execute(
        flavite,
        "replace.apply",
        serde_json::json!({
            "preview_id": preview_id,
            "selection_revision": 1,
            "idempotency_key": "3".repeat(32),
        }),
    );
    let ExecutionResult::Local(LocalResult::Error(error)) = result else {
        panic!("expected error, got {result:?}");
    };
    assert!(error.message.contains("select notes"), "{}", error.message);
}

#[test]
fn closing_preview_removes_unused_payloads_and_preserves_durable_revert() {
    use flavite_fs::Fs;
    let fixture = support::fixture(&[("notes/a.md", "alpha one\n")]);
    let api = &fixture.flavite;
    let store = flavite_vault::replace::ReplaceStore::new(&fixture.root);
    let (unused, _) = preview_all(api);
    let payloads = fixture.fs.read_dir(&store.payload_dir(&unused)).unwrap();
    assert!(!payloads.is_empty());
    success_data(execute(
        api,
        "replace.preview_close",
        serde_json::json!({"preview_id":unused}),
    ));
    assert!(
        fixture
            .fs
            .read_if_exists(&store.manifest_path(&unused))
            .unwrap()
            .is_none()
    );
    for payload in payloads {
        assert!(fixture.fs.read_if_exists(&payload.path).unwrap().is_none());
    }
    let (used, revision) = preview_all(api);
    let applied = success_data(execute(
        api,
        "replace.apply",
        serde_json::json!({"preview_id":used,"selection_revision":revision,"idempotency_key":"a".repeat(32)}),
    ));
    let job = applied["job_id"].as_str().unwrap();
    success_data(execute(
        api,
        "replace.preview_close",
        serde_json::json!({"preview_id":used}),
    ));
    assert!(
        fixture
            .fs
            .read_if_exists(&store.manifest_path(&used))
            .unwrap()
            .is_some()
    );
    let reverted = success_data(execute(
        api,
        "replace.revert",
        serde_json::json!({"job_id":job,"idempotency_key":"b".repeat(32)}),
    ));
    assert_eq!(reverted["status"]["totals"]["reverted"], 1);
    assert_eq!(fixture.saved_bytes("notes/a.md"), b"alpha one\n");
}
