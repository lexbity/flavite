//! FEUX-0002 X06 (E-10/E-11) API tests: `editor.select_matches` and
//! `editor.split_selection` — drafts through the existing `text.find`
//! engine, atomic applies, typed refusals, and zero-effect guarantees.

#[path = "support/links.rs"]
mod support;

use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};
use flavite_buffer::revision::BufferId;

fn draft(
    fixture: &support::Fixture,
    command: &'static str,
    buffer_id: u64,
    revision: u64,
    query: &str,
    regex: bool,
) -> ExecutionResult {
    let request_id = support::next_id(&fixture.flavite);
    let result = fixture.flavite.execute(Invocation::new(
        request_id,
        command,
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "selections": {"primary": 0, "items": [[0, 0, 0]]},
            "query": {"text": query, "regex": regex, "case_sensitive": false},
        }),
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => fixture.flavite.wait_for_operation(request_id),
        other => other,
    }
}

fn apply(
    fixture: &support::Fixture,
    command: &'static str,
    buffer_id: u64,
    revision: u64,
    spans: &[(usize, usize)],
) -> ExecutionResult {
    let request_id = support::next_id(&fixture.flavite);
    fixture.flavite.execute(Invocation::new(
        request_id,
        command,
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "selections": {"primary": 0, "items": [[0, 0, 0]]},
            "query": {"text": "TODO", "regex": false, "case_sensitive": false},
            "apply": true,
            "spans": spans,
        }),
        Origin::Gui,
    ))
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

fn history_depth(fixture: &support::Fixture, buffer_id: u64, revision: u64) -> usize {
    let request_id = support::next_id(&fixture.flavite);
    let result = fixture.flavite.execute(Invocation::new(
        request_id,
        "buffer.history_page",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "limit": 100,
        }),
        Origin::Gui,
    ));
    let data = success_data(result);
    data["entries"].as_array().map_or(0, Vec::len)
}

#[test]
fn draft_select_matches_returns_spans_and_leaves_the_buffer_unchanged() {
    let fixture = support::fixture(&[("notes/a.md", "TODO one\nTODO two\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let before = fixture.text(buffer_id);
    let result = draft(
        &fixture,
        "editor.select_matches",
        buffer_id,
        revision,
        "TODO",
        false,
    );
    let data = success_data(result);
    assert_eq!(data["buffer_id"].as_u64(), Some(buffer_id));
    assert_eq!(data["revision"].as_u64(), Some(revision));
    assert_eq!(data["spans"].as_array().map(|spans| spans.len()), Some(2));
    assert_eq!(data["total_matches"].as_u64(), Some(2));
    assert_eq!(data["zero_width_count"].as_u64(), Some(0));
    // A draft is read-only: same revision, same bytes.
    let after = fixture.text(buffer_id);
    assert_eq!(after, before);
    assert_eq!(
        fixture
            .flavite
            .get_buffer_view(BufferId::from(buffer_id), None)
            .expect("view")
            .revision,
        revision
    );
}

#[test]
fn draft_split_selection_returns_matches_as_gap_candidates() {
    let fixture = support::fixture(&[("notes/a.md", "xTODOxTODOx")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let data = success_data(draft(
        &fixture,
        "editor.split_selection",
        buffer_id,
        revision,
        "TODO",
        false,
    ));
    assert_eq!(data["total_matches"].as_u64(), Some(2));
    // The draft only collects; the gaps are built at apply time.
    assert_eq!(data["spans"].as_array().map(|spans| spans.len()), Some(2));
}

#[test]
fn apply_select_matches_replaces_the_set_without_mutating_text_or_history() {
    let fixture = support::fixture(&[("notes/a.md", "TODO one\nTODO two\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let depth_before = history_depth(&fixture, buffer_id, revision);
    let result = apply(
        &fixture,
        "editor.select_matches",
        buffer_id,
        revision,
        &[(0, 4), (6, 10)],
    );
    let data = success_data(result);
    let selections = data["selections"].as_array().expect("selections");
    assert_eq!(selections.len(), 2);
    // Canonical triples [id, anchor, head].
    assert_eq!(selections[0].as_array().unwrap()[1..], [0, 4]);
    assert_eq!(selections[1].as_array().unwrap()[1..], [6, 10]);
    // Selection-only: revision, bytes, and history depth are untouched.
    assert_eq!(fixture.text(buffer_id), "TODO one\nTODO two\n");
    assert_eq!(
        fixture
            .flavite
            .get_buffer_view(BufferId::from(buffer_id), None)
            .expect("view")
            .revision,
        revision
    );
    let depth_after = history_depth(&fixture, buffer_id, revision);
    assert_eq!(
        depth_after, depth_before,
        "selection-only creates no history node"
    );
}

#[test]
fn apply_split_selection_builds_the_gaps_and_refuses_no_gaps_atomically() {
    let fixture = support::fixture(&[("notes/a.md", "xTODOxTODOx")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    // Matches at 1..5 and 6..10; gaps are (0,1), (5,6), (10,11).
    let data = success_data(apply(
        &fixture,
        "editor.split_selection",
        buffer_id,
        revision,
        &[(1, 5), (6, 10)],
    ));
    let selections = data["selections"].as_array().expect("selections");
    assert_eq!(selections.len(), 3);
    assert_eq!(selections[0].as_array().unwrap()[1..], [0, 1]);
    assert_eq!(selections[1].as_array().unwrap()[1..], [5, 6]);
    assert_eq!(selections[2].as_array().unwrap()[1..], [10, 11]);

    // A match set covering the whole document leaves no gaps: typed refusal,
    // prior set kept, zero effects.
    let text = fixture.text(buffer_id);
    let before = fixture.text(buffer_id);
    let request_id = support::next_id(&fixture.flavite);
    let result = fixture.flavite.execute(Invocation::new(
        request_id,
        "editor.split_selection",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "selections": {"primary": 0, "items": [[0, 0, 0]]},
            "query": {"text": "xTODOxTODOx", "regex": false, "case_sensitive": false},
            "apply": true,
            "spans": [[0, 11]],
        }),
        Origin::Gui,
    ));
    assert_eq!(message_of(&result).as_deref(), Some("NoGaps"));
    assert_eq!(fixture.text(buffer_id), before);
    assert_eq!(fixture.text(buffer_id), text);
}

#[test]
fn select_apply_refuses_zero_width_spans_with_the_observed_count() {
    let fixture = support::fixture(&[("notes/a.md", "abcd\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let request_id = support::next_id(&fixture.flavite);
    let result = fixture.flavite.execute(Invocation::new(
        request_id,
        "editor.select_matches",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "selections": {"primary": 0, "items": [[0, 0, 0]]},
            "query": {"text": "a*", "regex": true, "case_sensitive": false},
            "apply": true,
            "spans": [[0, 0], [1, 1], [2, 2], [3, 3], [4, 4]],
        }),
        Origin::Gui,
    ));
    assert_eq!(message_of(&result).as_deref(), Some("ZeroWidthMatches 5"));
}

#[test]
fn select_apply_refuses_oversize_with_the_observed_count() {
    let fixture = support::fixture(&[("notes/a.md", "a".repeat(10_010).as_str())]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let spans: Vec<[usize; 2]> = (0..10_001usize).map(|i| [i, i + 1]).collect();
    let request_id = support::next_id(&fixture.flavite);
    let result = fixture.flavite.execute(Invocation::new(
        request_id,
        "editor.select_matches",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "selections": {"primary": 0, "items": [[0, 0, 0]]},
            "query": {"text": "a", "regex": false, "case_sensitive": false},
            "apply": true,
            "spans": spans,
        }),
        Origin::Gui,
    ));
    assert_eq!(
        message_of(&result).as_deref(),
        Some("TooManySelections 10001")
    );
    assert_eq!(fixture.text(buffer_id), "a".repeat(10_010));
}

#[test]
fn draft_and_apply_refuse_stale_revisions_with_zero_effects() {
    let fixture = support::fixture(&[("notes/a.md", "TODO\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let stale = revision + 40;
    let result = draft(
        &fixture,
        "editor.select_matches",
        buffer_id,
        stale,
        "TODO",
        false,
    );
    let message = message_of(&result).expect("stale-revision refusal");
    assert!(message.contains("StaleRevision"), "{message}");
    let apply_result = apply(
        &fixture,
        "editor.select_matches",
        buffer_id,
        stale,
        &[(0, 4)],
    );
    let message = message_of(&apply_result).expect("stale-revision refusal");
    assert!(message.contains("StaleRevision"), "{message}");
    assert_eq!(fixture.text(buffer_id), "TODO\n");
}

#[test]
fn apply_refuses_missing_spans_and_draft_refuses_supplied_spans() {
    let fixture = support::fixture(&[("notes/a.md", "TODO\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let request_id = support::next_id(&fixture.flavite);
    let missing = fixture.flavite.execute(Invocation::new(
        request_id,
        "editor.select_matches",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "selections": {"primary": 0, "items": [[0, 0, 0]]},
            "query": {"text": "TODO", "regex": false, "case_sensitive": false},
            "apply": true,
        }),
        Origin::Gui,
    ));
    assert_eq!(
        message_of(&missing).as_deref(),
        Some("apply needs the candidate spans")
    );

    let request_id = support::next_id(&fixture.flavite);
    let spans = fixture.flavite.execute(Invocation::new(
        request_id,
        "editor.select_matches",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "selections": {"primary": 0, "items": [[0, 0, 0]]},
            "query": {"text": "TODO", "regex": false, "case_sensitive": false},
            "spans": [[0, 4]],
        }),
        Origin::Gui,
    ));
    assert_eq!(
        message_of(&spans).as_deref(),
        Some("spans only apply to an apply")
    );
}

#[test]
fn draft_validation_checks_the_incoming_selections() {
    let fixture = support::fixture(&[("notes/a.md", "TODO\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let request_id = support::next_id(&fixture.flavite);
    let result = fixture.flavite.execute(Invocation::new(
        request_id,
        "editor.select_matches",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "selections": {"primary": 0, "items": [[0, 9999, 10000]]},
            "query": {"text": "TODO", "regex": false, "case_sensitive": false},
        }),
        Origin::Gui,
    ));
    let message = message_of(&result).expect("out-of-bounds selection refusal");
    assert!(message.contains("outside the text"), "{message}");
}

#[test]
fn empty_literal_query_is_idle_on_draft() {
    let fixture = support::fixture(&[("notes/a.md", "TODO\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let result = draft(
        &fixture,
        "editor.select_matches",
        buffer_id,
        revision,
        "",
        false,
    );
    let message = message_of(&result).expect("idle-query refusal");
    assert!(message.contains("idle"), "{message}");
}

#[test]
fn worst_case_10k_member_payload_fits_the_ceiling() {
    let spans: Vec<[usize; 2]> = (0..10_000usize).map(|i| [i, i + 1]).collect();
    let payload = serde_json::json!({
        "buffer_id": 1,
        "revision": 1,
        "selections": {"primary": 0, "items": [[0, 0, 0]]},
        "query": {"text": "a", "regex": false, "case_sensitive": false},
        "apply": true,
        "spans": spans,
    });
    let bytes = serde_json::to_vec(&payload).expect("serialize");
    assert!(
        bytes.len() < flavite_api::command::MAX_INVOCATION_PAYLOAD_BYTES,
        "payload {} exceeds ceiling {}",
        bytes.len(),
        flavite_api::command::MAX_INVOCATION_PAYLOAD_BYTES
    );
}
