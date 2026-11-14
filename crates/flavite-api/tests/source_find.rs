//! RFC-0006 S07 facade commands (D-08/D-09): `source_find.*` and
//! `replace.preview/select/status` over a real Session/MemFs, including
//! typed exclusions, read-only non-applicable reports, regex-helper
//! refusal, and zero mutation from any preview/selection.

#[path = "support/links.rs"]
mod support;

use flavite_api::{ExecutionResult, Flavite, LocalResult, Origin, command::Invocation};
use flavite_fs::Fs;

fn execute(flavite: &Flavite, id: &'static str, args: serde_json::Value) -> ExecutionResult {
    let request_id = flavite.command_registry().next_request_id();
    let result = flavite.execute(editor_operations_fixture::admit(
        &flavite,
        Invocation::new(request_id, id, args, Origin::Gui),
    ));
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

#[test]
fn source_find_start_reports_exact_counts_and_exclusions() {
    let fixture = support::fixture(&[
        ("notes/a.md", "alpha beta alpha\n"),
        ("notes/b.md", "alpha\n"),
        ("notes/dirty.md", "dirty\n"),
    ]);
    let flavite = &fixture.flavite;
    // Dirty one note through the real session.
    let buffer_id = fixture.open_note("notes/dirty.md").0;
    let view = flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
        .unwrap();
    let _ = execute(
        flavite,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": view.revision,
            "ops": [{"range": [0, 0], "replacement": "prefix "}],
            "undo_policy": "typing",
        }),
    );

    let data = success_data(execute(
        flavite,
        "source_find.start",
        serde_json::json!({ "query": "alpha", "mode": "literal" }),
    ));
    // The frozen scope contains all three indexed notes; the dirty buffer is
    // excluded as a typed row, never silently dropped from counts.
    assert_eq!(data["total_notes"], 3);
    assert_eq!(data["total_matches"], 3);
    let rows = data["page"]["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["path"], "notes/a.md");
    assert_eq!(rows[0]["note_matches"], 2);
    let exclusions = data["exclusions"].as_array().expect("exclusions");
    assert!(exclusions.iter().any(|row| row["path"] == "notes/dirty.md"));
    assert_eq!(
        exclusions
            .iter()
            .find(|row| row["path"] == "notes/dirty.md")
            .unwrap()["reason"],
        "dirty_buffer"
    );
}

#[test]
fn source_find_continuation_pages_all_matches() {
    let body = "x ".repeat(120) + "\n";
    let fixture = support::fixture(&[("notes/one.md", Box::leak(body.into_boxed_str()))]);
    let flavite = &fixture.flavite;
    let start = success_data(execute(
        flavite,
        "source_find.start",
        serde_json::json!({ "query": "x", "mode": "literal" }),
    ));
    let finder = start["finder_id"].as_str().unwrap().to_owned();
    assert_eq!(
        start["page"]["rows"][0]["matches"]
            .as_array()
            .unwrap()
            .len(),
        50
    );
    let mut note_index = start["page"]["rows"][0]["ordinal"].as_u64().unwrap();
    let mut match_offset = start["page"]["next_match_offset"].as_u64();
    let mut total = start["page"]["rows"][0]["matches"]
        .as_array()
        .unwrap()
        .len();
    while let Some(offset) = match_offset {
        let page = success_data(execute(
            flavite,
            "source_find.page",
            serde_json::json!({
                "finder_id": finder,
                "note_index": note_index,
                "match_offset": offset,
                "page_len": 50,
            }),
        ));
        let rows = page["page"]["rows"].as_array().unwrap();
        total += rows[0]["matches"].as_array().unwrap().len();
        match_offset = page["page"]["next_match_offset"].as_u64();
        note_index = rows[0]["ordinal"].as_u64().unwrap();
    }
    assert_eq!(total, 120, "exact match enumeration, no silent stop");
    let _ = execute(
        flavite,
        "source_find.close",
        serde_json::json!({ "finder_id": finder }),
    );
}

#[test]
fn preview_seals_non_applying_plan_with_exact_payloads() {
    let fixture = support::fixture(&[("notes/a.md", "alpha beta alpha\n")]);
    let flavite = &fixture.flavite;
    let data = success_data(execute(
        flavite,
        "replace.preview",
        serde_json::json!({
            "query": "alpha",
            "replacement": "X",
            "mode": "literal",
        }),
    ));
    let plan = &data["plan"];
    assert_eq!(plan["state"], "ready");
    assert_eq!(plan["totals"]["matches"], 2);
    assert_eq!(plan["totals"]["matched"], 1);
    let rows = data["page"]["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["match_count"], 2);
    assert!(!rows[0]["no_change"].as_bool().unwrap());
    assert!(rows[0]["before"]["hash"].as_str().unwrap().len() == 64);
    // Selection produces a revision + digest.
    let preview = data["preview_id"].as_str().unwrap().to_owned();
    let selected = success_data(execute(
        flavite,
        "replace.select",
        serde_json::json!({ "preview_id": preview, "select": "all" }),
    ));
    assert_eq!(selected["selection_revision"], 1);
    assert!(!selected["selection_digest"].as_str().unwrap().is_empty());
    // Status reflects the sealed plan.
    let status = success_data(execute(
        flavite,
        "replace.status",
        serde_json::json!({ "preview_id": preview }),
    ));
    assert_eq!(status["plan"]["totals"]["notes"], 1);
}

#[test]
fn preview_is_non_applicable_on_read_only_vaults() {
    let mut fixture = support::fixture(&[("notes/a.md", "alpha\n")]);
    let flavite = &mut fixture.flavite;
    flavite.set_read_only(true);
    let result = execute(
        flavite,
        "replace.preview",
        serde_json::json!({
            "query": "alpha",
            "replacement": "X",
            "mode": "literal",
        }),
    );
    let ExecutionResult::Local(LocalResult::Error(error)) = result else {
        panic!("read-only preview must be refused typed, got {result:?}");
    };
    assert_eq!(error.code, "read_only_non_applicable");
    // Find still works under the read budget.
    let data = success_data(execute(
        flavite,
        "source_find.start",
        serde_json::json!({ "query": "alpha", "mode": "literal" }),
    ));
    assert_eq!(data["total_matches"], 1);
}

#[test]
fn empty_literal_query_is_idle_and_bad_mode_is_rejected() {
    let fixture = support::fixture(&[("notes/a.md", "alpha\n")]);
    let flavite = &fixture.flavite;
    let result = execute(
        flavite,
        "source_find.start",
        serde_json::json!({ "query": "", "mode": "literal" }),
    );
    let ExecutionResult::Local(LocalResult::Error(error)) = result else {
        panic!("empty query must be refused typed, got {result:?}");
    };
    assert!(error.message.contains("idle"), "{}", error.message);
    let result = execute(
        flavite,
        "source_find.start",
        serde_json::json!({ "query": "alpha", "mode": "fuzzy" }),
    );
    let ExecutionResult::Rejected(rejected) = result else {
        panic!("unknown mode must be rejected, got {result:?}");
    };
    assert_eq!(rejected.reason, flavite_api::RejectionReason::InvalidArgs);
}

#[test]
fn preview_never_mutates_notes() {
    let fixture = support::fixture(&[("notes/a.md", "alpha beta alpha\n")]);
    let flavite = &fixture.flavite;
    let _ = success_data(execute(
        flavite,
        "replace.preview",
        serde_json::json!({
            "query": "alpha",
            "replacement": "X",
            "mode": "literal",
        }),
    ));
    // Saved bytes are untouched; no buffer was opened.
    let saved = fixture
        .fs
        .read_if_exists(&fixture.root.join("notes/a.md"))
        .unwrap();
    assert_eq!(saved.as_deref(), Some(&b"alpha beta alpha\n"[..]));
}

#[test]
fn anonymous_unknown_args_are_rejected() {
    let fixture = support::fixture(&[("notes/a.md", "alpha\n")]);
    let flavite = &fixture.flavite;
    let result = execute(
        flavite,
        "source_find.start",
        serde_json::json!({ "query": "alpha", "sneaky": true }),
    );
    let ExecutionResult::Rejected(rejected) = result else {
        panic!("unknown fields must be rejected, got {result:?}");
    };
    assert_eq!(rejected.reason, flavite_api::RejectionReason::InvalidArgs);
}

#[path = "support/editor_operations.rs"]
mod editor_operations_fixture;
