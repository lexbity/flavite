//! RFC-0006 S09 facade commands: `replace.preview` with `include_drafts`,
//! `replace.apply` for draft candidates (AppliedDraft, never a saved
//! file), and `replace.revert` with its typed outcome projection — all
//! through the real facade over a Session/MemFs vault.

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

fn open_note(flavite: &Flavite, path: &str) -> (u64, u64) {
    let data = success_data(execute(flavite, "note.open", serde_json::json!(path)));
    (
        data["buffer_id"].as_u64().unwrap(),
        data["revision"].as_u64().unwrap(),
    )
}

fn dirty_note(flavite: &Flavite, buffer_id: u64, revision: u64, text: &str) -> u64 {
    let data = success_data(execute(
        flavite,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "ops": [
                { "range": [5, 5], "replacement": text }
            ],
            "undo_policy": "typing",
        }),
    ));
    data["revision"].as_u64().unwrap()
}

fn draft_preview_all(flavite: &Flavite) -> (String, u64) {
    let preview = success_data(execute(
        flavite,
        "replace.preview",
        serde_json::json!({
            "query": "alpha",
            "replacement": "omega",
            "mode": "literal",
            "include_drafts": true,
        }),
    ));
    let preview_id = preview["preview_id"].as_str().unwrap().to_owned();
    assert!(preview["plan"]["totals"]["drafts"].as_u64().is_some());
    let selected = success_data(execute(
        flavite,
        "replace.select",
        serde_json::json!({ "preview_id": preview_id, "select": "all" }),
    ));
    let revision = selected["selection_revision"].as_u64().unwrap();
    (preview_id, revision)
}

#[test]
fn draft_preview_and_apply_flow_through_the_facade_without_saving() {
    let fixture = support::fixture(&[("notes/a.md", "alpha one\n"), ("notes/b.md", "alpha two\n")]);
    let flavite = &fixture.flavite;
    let (buffer_id, rev0) = open_note(flavite, "/test/semantic-links/notes/a.md");
    let rev1 = dirty_note(flavite, buffer_id, rev0, " more");

    let (preview_id, revision) = draft_preview_all(flavite);

    // The candidate row is typed as a draft and carries its lineage.
    let status = success_data(execute(
        flavite,
        "replace.status",
        serde_json::json!({ "preview_id": preview_id }),
    ));
    let rows = status["page"]["rows"].as_array().unwrap();
    let draft_row = rows
        .iter()
        .find(|row| row["path"].as_str() == Some("notes/a.md"))
        .expect("a.md candidate");
    assert_eq!(draft_row["source_kind"], "draft");
    assert_eq!(
        draft_row["draft_lineage"]["preview_revision"]
            .as_u64()
            .unwrap(),
        rev1
    );
    let saved_row = rows
        .iter()
        .find(|row| row["path"].as_str() == Some("notes/b.md"))
        .expect("b.md candidate");
    assert_eq!(saved_row["source_kind"], "saved");

    let applied = success_data(execute(
        flavite,
        "replace.apply",
        serde_json::json!({
            "preview_id": preview_id,
            "selection_revision": revision,
            "idempotency_key": "a".repeat(32),
        }),
    ));
    let job_id = applied["job_id"].as_str().unwrap().to_owned();
    assert_eq!(applied["status"]["phase"], "completed");
    assert_eq!(applied["status"]["totals"]["applied_drafts"], 1);
    assert_eq!(applied["status"]["totals"]["saved"], 1);

    // The draft was not force-saved: its disk bytes are untouched, while
    // the saved note was replaced.
    assert_eq!(fixture.saved_bytes("notes/a.md"), b"alpha one\n");
    assert_eq!(fixture.saved_bytes("notes/b.md"), b"omega two\n");

    // Revert restores both: the saved note on disk, the draft in the
    // buffer (via a Draft revert).
    let reverted = success_data(execute(
        flavite,
        "replace.revert",
        serde_json::json!({
            "job_id": job_id,
            "idempotency_key": "b".repeat(32),
        }),
    ));
    assert_eq!(reverted["status"]["phase"], "completed");
    assert_eq!(reverted["status"]["totals"]["reverted"], 1);
    assert_eq!(reverted["status"]["totals"]["reverted_drafts"], 1);
    assert_eq!(fixture.saved_bytes("notes/a.md"), b"alpha one\n");
    assert_eq!(fixture.saved_bytes("notes/b.md"), b"alpha two\n");
}

#[test]
fn draft_stale_guard_is_reportable_over_the_facade() {
    let fixture = support::fixture(&[("notes/a.md", "alpha one\n")]);
    let flavite = &fixture.flavite;
    let (buffer_id, rev0) = open_note(flavite, "/test/semantic-links/notes/a.md");
    let rev1 = dirty_note(flavite, buffer_id, rev0, " more");
    let (preview_id, revision) = draft_preview_all(flavite);
    // The user edits after the preview was sealed.
    dirty_note(flavite, buffer_id, rev1, " two");
    let applied = success_data(execute(
        flavite,
        "replace.apply",
        serde_json::json!({
            "preview_id": preview_id,
            "selection_revision": revision,
            "idempotency_key": "c".repeat(32),
        }),
    ));
    assert_eq!(applied["status"]["phase"], "partial");
    assert_eq!(applied["status"]["totals"]["stale"], 1);
    assert_eq!(applied["status"]["totals"]["applied_drafts"], 0);
    assert_eq!(fixture.saved_bytes("notes/a.md"), b"alpha one\n");
}
