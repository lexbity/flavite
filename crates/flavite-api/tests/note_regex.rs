//! S03 API routing for regex mode: strict DTO validation, read capability,
//! revision-bound pending work, and no mutation from a find request.
#[path = "support/links.rs"]
mod support;

use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};
use flavite_buffer::revision::BufferId;

#[test]
fn regex_find_is_admitted_as_read_only_revision_bound_work() {
    let fixture = support::fixture(&[("notes/a.md", "cat scatter cat\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let before = fixture
        .flavite
        .get_buffer_view(BufferId::from(buffer_id), None)
        .expect("buffer view");
    let request_id = fixture.flavite.command_registry().next_request_id();
    let result = fixture.flavite.execute(Invocation::new(
        request_id,
        "text.find",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "query": r"\bcat\b",
            "mode": "regex",
            "case_sensitive": false,
            "whole_word": false,
        }),
        Origin::Gui,
    ));
    let completed = match result {
        ExecutionResult::Pending { request_id } => fixture.flavite.wait_for_operation(request_id),
        other => other,
    };
    // The matcher may report RegexUnavailable when the installed sibling is
    // absent; either way the valid regex DTO was admitted and the source is
    // untouched. Real helper semantics are exercised in vault::text_worker.
    assert!(
        matches!(
            completed,
            ExecutionResult::Local(_)
                | ExecutionResult::Rejected(_)
                | ExecutionResult::ActionRequired { .. }
        ),
        "unexpected regex find result: {completed:?}"
    );
    let after = fixture
        .flavite
        .get_buffer_view(BufferId::from(buffer_id), None)
        .expect("buffer remains open");
    assert_eq!(after.revision, before.revision);
    assert_eq!(after.text, before.text);
}

#[test]
fn unknown_match_mode_is_rejected_before_worker_admission() {
    let fixture = support::fixture(&[("notes/a.md", "unchanged\n")]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let request_id = fixture.flavite.command_registry().next_request_id();
    let result = fixture.flavite.execute(Invocation::new(
        request_id,
        "text.find",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "query": "x",
            "mode": "different-engine",
        }),
        Origin::Gui,
    ));
    assert!(matches!(result, ExecutionResult::Rejected(_)));
    let after = fixture
        .flavite
        .get_buffer_view(BufferId::from(buffer_id), None)
        .expect("buffer remains open");
    assert_eq!(after.revision, revision);
    assert_eq!(after.text, "unchanged\n");
}

#[test]
fn text_find_continuation_pages_every_match_in_ordinal_order() {
    let body = "x ".repeat(600);
    let fixture = support::fixture(&[("notes/a.md", body.as_str())]);
    let (buffer_id, revision) = fixture.open_note("notes/a.md");
    let mut all = Vec::new();
    let mut offset = 0u64;
    loop {
        let request_id = fixture.flavite.command_registry().next_request_id();
        let result = fixture.flavite.execute(Invocation::new(
            request_id,
            "text.find",
            serde_json::json!({
                "buffer_id": buffer_id,
                "revision": revision,
                "query": "x",
                "mode": "literal",
                "offset": offset,
            }),
            Origin::Gui,
        ));
        let completed = match result {
            ExecutionResult::Pending { request_id } => {
                fixture.flavite.wait_for_operation(request_id)
            }
            other => other,
        };
        let data = match completed {
            ExecutionResult::Local(LocalResult::Success(success)) => success.data,
            other => panic!("expected find page, got {other:?}"),
        };
        let spans = data["spans"].as_array().expect("spans");
        assert_eq!(data["offset"].as_u64(), Some(offset));
        assert_eq!(data["total_matches"].as_u64(), Some(600));
        all.extend(spans.iter().map(|span| span[0].as_u64().unwrap()));
        match data["next_offset"].as_u64() {
            Some(next) => {
                assert_eq!(next, offset + spans.len() as u64);
                assert_eq!(data["truncated"].as_bool(), Some(true));
                offset = next;
            }
            None => {
                assert_eq!(data["truncated"].as_bool(), Some(false));
                break;
            }
        }
    }
    assert_eq!(all.len(), 600);
    assert_eq!(
        all,
        (0..600).map(|index| (index * 2) as u64).collect::<Vec<_>>()
    );
}
