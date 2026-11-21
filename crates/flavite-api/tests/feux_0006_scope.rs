//! FEUX-0006 scope-engine API tests: `editor.keep_matching` (E-4), the
//! in-selections split (`S`) per-member semantics (E-2), the ReadNotes
//! posting of `text.replace_plan` / `editor.goto_line` (E-12), and the
//! top-level `text.replace_plan` completion payload (E-5).

#[path = "support/links.rs"]
mod support;

use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};

fn execute(
    api: &flavite_api::Flavite,
    command: &'static str,
    args: serde_json::Value,
) -> ExecutionResult {
    api.execute(Invocation::new(
        support::next_id(api),
        command,
        args,
        Origin::Gui,
    ))
}

fn run(
    api: &flavite_api::Flavite,
    command: &'static str,
    args: serde_json::Value,
) -> ExecutionResult {
    match execute(api, command, args) {
        ExecutionResult::Pending { request_id } => api.wait_for_operation(request_id),
        other => other,
    }
}

fn success_data(result: ExecutionResult) -> serde_json::Value {
    match result {
        ExecutionResult::Local(LocalResult::Success(success)) => success.data,
        other => panic!("expected success, got {other:?}"),
    }
}

fn message_of(result: &ExecutionResult) -> Option<String> {
    match result {
        ExecutionResult::Local(LocalResult::Error(error)) => Some(error.message.clone()),
        ExecutionResult::Rejected(rejected) => rejected.message.clone(),
        _ => None,
    }
}

fn revision(api: &flavite_api::Flavite, buffer: u64) -> u64 {
    api.get_buffer_view(buffer.into(), None).unwrap().revision
}

fn set(members: &[(usize, usize)]) -> serde_json::Value {
    serde_json::json!({
        "primary": 0,
        "items": members
            .iter()
            .enumerate()
            .map(|(id, &(anchor, head))| [id, anchor, head])
            .collect::<Vec<_>>(),
    })
}

#[test]
fn keep_matching_draft_collects_indices_and_apply_filters_the_set() {
    let fixture = support::fixture(&[("notes/a.md", "aa line\nbb line\ncc line\n")]);
    let (buffer_id, revision_before) = fixture.open_note("notes/a.md");
    let members = [(0, 8), (8, 16), (16, 24)];
    let draft = run(
        &fixture.flavite,
        "editor.keep_matching",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&members),
            "query": {"text": "aa", "regex": false, "case_sensitive": false},
            "keep": true,
            "apply": false,
        }),
    );
    let data = success_data(draft);
    assert_eq!(data["selection_indices"], serde_json::json!([0]));
    assert_eq!(data["match_count"].as_u64(), Some(1));

    // Apply filters the set to the kept member; selection-only (same
    // revision, same bytes, no history node).
    let depth_before = {
        let request_id = support::next_id(&fixture.flavite);
        let result = fixture.flavite.execute(Invocation::new(
            request_id,
            "buffer.history_page",
            serde_json::json!({
                "buffer_id": buffer_id,
                "revision": revision_before,
                "limit": 100,
            }),
            Origin::Gui,
        ));
        let ExecutionResult::Local(LocalResult::Success(success)) = result else {
            panic!("history page failed: {result:?}");
        };
        success.data["entries"].as_array().map_or(0, Vec::len)
    };
    let applied = execute(
        &fixture.flavite,
        "editor.keep_matching",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&members),
            "query": {"text": "aa", "regex": false, "case_sensitive": false},
            "keep": true,
            "apply": true,
            "selection_indices": [0],
            "match_count": 1,
        }),
    );
    let applied = success_data(applied);
    let selections = applied["selections"].as_array().expect("selections");
    assert_eq!(selections.len(), 1);
    assert_eq!(selections[0].as_array().unwrap()[1..], [0, 8]);
    assert_eq!(applied["revision"].as_u64(), Some(revision_before));
    assert_eq!(fixture.text(buffer_id), "aa line\nbb line\ncc line\n");
    let depth_after = {
        let request_id = support::next_id(&fixture.flavite);
        let result = fixture.flavite.execute(Invocation::new(
            request_id,
            "buffer.history_page",
            serde_json::json!({
                "buffer_id": buffer_id,
                "revision": revision_before,
                "limit": 100,
            }),
            Origin::Gui,
        ));
        let ExecutionResult::Local(LocalResult::Success(success)) = result else {
            panic!("history page failed: {result:?}");
        };
        success.data["entries"].as_array().map_or(0, Vec::len)
    };
    assert_eq!(
        depth_after, depth_before,
        "keep is selection-only: no history node"
    );
}

#[test]
fn keep_matching_refuses_an_emptying_filter_with_the_count_and_zero_effects() {
    let fixture = support::fixture(&[("notes/a.md", "aa line\nbb line\ncc line\n")]);
    let (buffer_id, revision_before) = fixture.open_note("notes/a.md");
    let members = [(0, 8), (8, 16), (16, 24)];
    // No member contains "zz": every member would be dropped.
    let result = run(
        &fixture.flavite,
        "editor.keep_matching",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&members),
            "query": {"text": "zz", "regex": false, "case_sensitive": false},
            "keep": true,
            "apply": true,
            "selection_indices": [],
            "match_count": 0,
        }),
    );
    assert_eq!(
        message_of(&result).as_deref(),
        Some("All 3 selections would be dropped")
    );
    assert_eq!(fixture.text(buffer_id), "aa line\nbb line\ncc line\n");
    assert_eq!(revision(&fixture.flavite, buffer_id), revision_before);

    // The complement keeps the non-matching member.
    let applied = run(
        &fixture.flavite,
        "editor.keep_matching",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&members),
            "query": {"text": "aa", "regex": false, "case_sensitive": false},
            "keep": false,
            "apply": true,
            "selection_indices": [1, 2],
            "match_count": 1,
        }),
    );
    let applied_data = success_data(applied);
    let selections = applied_data["selections"].as_array().expect("selections");
    assert_eq!(selections.len(), 2);
    assert_eq!(selections[0].as_array().unwrap()[1..], [8, 16]);
    assert_eq!(selections[1].as_array().unwrap()[1..], [16, 24]);
}

#[test]
fn keep_matching_refuses_out_of_range_indices_and_zero_match_count() {
    let fixture = support::fixture(&[("notes/a.md", "aa line\nbb line\ncc line\n")]);
    let (buffer_id, revision_before) = fixture.open_note("notes/a.md");
    let members = [(0, 8), (8, 16), (16, 24)];

    // An index beyond the canonical set is a typed refusal with zero effects.
    let result = run(
        &fixture.flavite,
        "editor.keep_matching",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&members),
            "query": {"text": "aa", "regex": false, "case_sensitive": false},
            "keep": true,
            "apply": true,
            "selection_indices": [0, 3],
            "match_count": 1,
        }),
    );
    assert_eq!(
        message_of(&result).as_deref(),
        Some("selection index 3 is out of range")
    );
    assert_eq!(fixture.text(buffer_id), "aa line\nbb line\ncc line\n");

    // A zero match total refuses whole even when indices are supplied.
    let result = run(
        &fixture.flavite,
        "editor.keep_matching",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&members),
            "query": {"text": "aa", "regex": false, "case_sensitive": false},
            "keep": true,
            "apply": true,
            "selection_indices": [0],
            "match_count": 0,
        }),
    );
    assert_eq!(
        message_of(&result).as_deref(),
        Some("All 3 selections would be dropped")
    );
    assert_eq!(revision(&fixture.flavite, buffer_id), revision_before);
}

#[test]
fn keep_matching_primary_hint_falls_back_to_the_last_survivor() {
    let fixture = support::fixture(&[("notes/a.md", "aa line\nbb line\ncc line\n")]);
    let (buffer_id, revision_before) = fixture.open_note("notes/a.md");
    // Primary is member 0 (the "aa" line); keeping only member 2 must fall
    // back to the last survivor as primary (delivered normalize rule).
    let applied = run(
        &fixture.flavite,
        "editor.keep_matching",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&[(0, 8), (8, 16), (16, 24)]),
            "query": {"text": "cc", "regex": false, "case_sensitive": false},
            "keep": true,
            "apply": true,
            "selection_indices": [2],
            "match_count": 1,
        }),
    );
    let data = success_data(applied);
    let selections = data["selections"].as_array().expect("selections");
    assert_eq!(selections.len(), 1);
    assert_eq!(selections[0].as_array().unwrap()[1..], [16, 24]);
    assert_eq!(
        data["primary"].as_u64(),
        Some(0),
        "the sole survivor is primary"
    );
}

#[test]
fn split_selection_in_selections_computes_per_member_gaps() {
    let fixture = support::fixture(&[("notes/a.md", "aa--bb--cc")]);
    let (buffer_id, revision_before) = fixture.open_note("notes/a.md");
    // Members: [0,6) ("aa--bb") and [6,10) ("--cc"). Match "--" at 2..4
    // is fully inside member 0 → gaps (0,2) and (4,6); member 1 has no
    // contained match → contributes itself (6,10).
    let result = execute(
        &fixture.flavite,
        "editor.split_selection",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&[(0, 6), (6, 10)]),
            "query": {"text": "--", "regex": false, "case_sensitive": false},
            "in_selections": true,
            "apply": true,
            "spans": [[2, 4]],
            "total_matches": 1,
            "zero_width_count": 0,
        }),
    );
    let data = success_data(result);
    let selections = data["selections"].as_array().expect("selections");
    assert_eq!(selections.len(), 3);
    assert_eq!(selections[0].as_array().unwrap()[1..], [0, 2]);
    assert_eq!(selections[1].as_array().unwrap()[1..], [4, 6]);
    assert_eq!(selections[2].as_array().unwrap()[1..], [6, 10]);
    assert_eq!(fixture.text(buffer_id), "aa--bb--cc");
    assert_eq!(revision(&fixture.flavite, buffer_id), revision_before);

    // A match straddling the member edge belongs to no member: both members
    // are unmatched and contribute themselves.
    let result = execute(
        &fixture.flavite,
        "editor.split_selection",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&[(0, 6), (6, 10)]),
            "query": {"text": "--", "regex": false, "case_sensitive": false},
            "in_selections": true,
            "apply": true,
            "spans": [[4, 8]],
            "total_matches": 1,
            "zero_width_count": 0,
        }),
    );
    let data = success_data(result);
    let selections = data["selections"].as_array().expect("selections");
    assert_eq!(selections.len(), 2);
    assert_eq!(selections[0].as_array().unwrap()[1..], [0, 6]);
    assert_eq!(selections[1].as_array().unwrap()[1..], [6, 10]);
}

#[test]
fn replace_plan_completes_with_a_top_level_ops_payload() {
    let fixture = support::fixture(&[("notes/a.md", "xaxbx")]);
    let (buffer_id, revision_before) = fixture.open_note("notes/a.md");
    let result = run(
        &fixture.flavite,
        "text.replace_plan",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "query": "a",
            "mode": "literal",
            "replacement": "A",
            "case_sensitive": false,
            "whole_word": false,
            "scope_spans": [],
        }),
    );
    let data = success_data(result);
    assert_eq!(data["match_count"].as_u64(), Some(1));
    assert!(data["output_len"].as_u64().is_some());
    let ops = data["ops"].as_array().expect("top-level ops");
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0][0].as_u64(), Some(1));
    assert_eq!(ops[0][1].as_u64(), Some(2));
    assert_eq!(ops[0][2].as_str(), Some("A"));
    // Compute-only: nothing changed.
    assert_eq!(fixture.text(buffer_id), "xaxbx");
    assert_eq!(revision(&fixture.flavite, buffer_id), revision_before);
}

#[test]
fn read_only_vault_keeps_readnotes_replace_plan_and_goto_line() {
    let mut fixture = support::fixture(&[("notes/a.md", "line one\nline two\n")]);
    let (buffer_id, revision_before) = fixture.open_note("notes/a.md");
    fixture.flavite.set_read_only(true);

    // editor.goto_line is selection-only (class-S): admitted read-only with
    // the ticketed selection-only shape.
    let operation = fixture
        .flavite
        .issue_editor_operation(buffer_id, revision_before, None)
        .unwrap();
    let goto = execute(
        &fixture.flavite,
        "editor.goto_line",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "operation": operation,
            "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
            "selections": set(&[(0, 4)]),
            "line": 2,
        }),
    );
    assert!(goto.is_success(), "goto_line must be read-only: {goto:?}");
    assert_eq!(fixture.text(buffer_id), "line one\nline two\n");

    // text.replace_plan is ReadNotes compute: admitted (pending worker job).
    let plan = execute(
        &fixture.flavite,
        "text.replace_plan",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "query": "one",
            "mode": "literal",
            "replacement": "1",
            "case_sensitive": false,
            "whole_word": false,
            "scope_spans": [],
        }),
    );
    let data = match plan {
        ExecutionResult::Pending { request_id } => {
            success_data(fixture.flavite.wait_for_operation(request_id))
        }
        ExecutionResult::Local(LocalResult::Success(success)) => success.data,
        other => panic!("replace_plan must be read-only compute: {other:?}"),
    };
    assert_eq!(data["match_count"].as_u64(), Some(1));
    assert_eq!(fixture.text(buffer_id), "line one\nline two\n");

    // A mutating command stays refused in the read-only vault.
    let refused = execute(
        &fixture.flavite,
        "editor.delete",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision_before,
            "selections": set(&[(0, 4)]),
        }),
    );
    assert!(matches!(refused, ExecutionResult::Rejected(_)));
}
