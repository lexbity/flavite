//! Editor kernel command tests (FEUX-0002 X03): ticket lifecycle (AC-5),
//! planner-backed delete end-to-end with undo identity (AC-6/AC-8), zero-
//! effect no-ops, selection-only set commands (AC-7/AC-9), and the payload
//! ceiling at 10,000 members.

#[path = "support/links.rs"]
mod support;
use flavite_api::{ExecutionResult, Flavite, Invocation, LocalResult, Origin};
use serde_json::{Value, json};

fn execute(api: &Flavite, command: &'static str, args: Value) -> ExecutionResult {
    api.execute(Invocation::new(
        support::next_id(api),
        command,
        args,
        Origin::Gui,
    ))
}

fn data(result: ExecutionResult) -> Value {
    let ExecutionResult::Local(LocalResult::Success(value)) = result else {
        panic!("expected success: {result:?}");
    };
    value.data
}

fn revision(api: &Flavite, buffer: u64) -> u64 {
    api.get_buffer_view(buffer.into(), None).unwrap().revision
}

fn open_note(f: &support::Fixture, name: &str) -> u64 {
    f.open_note(name).0
}

fn issue_operation(api: &Flavite, buffer: u64) -> Value {
    let result = execute(api, "editor.issue_operation", json!({"buffer_id": buffer}));
    let value = data(result);
    value["operation"].clone()
}

fn items(pairs: &[(usize, usize)]) -> Vec<[usize; 3]> {
    pairs
        .iter()
        .enumerate()
        .map(|(i, &(a, h))| [i, a, h])
        .collect()
}

fn message_of(result: &ExecutionResult) -> Option<String> {
    match result {
        ExecutionResult::Local(LocalResult::Error(error)) => Some(error.message.clone()),
        ExecutionResult::Rejected(rejected) => rejected.message.clone(),
        _ => None,
    }
}

#[test]
fn ticket_lifecycle_issue_admit_dedup_cancel_and_retirement() {
    let f = support::fixture(&[("note.md", "abc\ndef\n")]);
    let buffer = open_note(&f, "note.md");
    let op = issue_operation(&f.flavite, buffer);
    assert_eq!(op["buffer_incarnation"].as_u64(), Some(1));
    assert!(op["sequence"].as_u64().is_some());
    assert_eq!(op["ticket"].as_str().unwrap().len(), 32);

    // Duplicate delivery after completion replays the applied outcome.
    let args = json!({
        "buffer_id": buffer,
        "revision": revision(&f.flavite, buffer),
        "operation": op,
        "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
        "selections": {"primary": 0, "items": items(&[(0, 3)])},
    });
    let first = execute(&f.flavite, "editor.delete", args.clone());
    assert!(first.is_success(), "delete failed: {first:?}");
    let second = execute(&f.flavite, "editor.delete", args.clone());
    assert_eq!(message_of(&second).as_deref(), Some("AlreadyApplied"));

    // Cancel before install yields Cancelled on a later delivery.
    let op2 = issue_operation(&f.flavite, buffer);
    let cancel = execute(
        &f.flavite,
        "editor.cancel_operation",
        json!({"buffer_id": buffer, "operation": op2}),
    );
    assert!(cancel.is_success());
    let args2 = json!({
        "buffer_id": buffer,
        "revision": revision(&f.flavite, buffer),
        "operation": op2,
        "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
        "selections": {"primary": 0, "items": items(&[(0, 3)])},
    });
    let refused = execute(&f.flavite, "editor.delete", args2);
    assert_eq!(message_of(&refused).as_deref(), Some("Cancelled"));

    // Retired sequences (after many completions) are refused as expired.
    for _ in 0..300 {
        let op_n = issue_operation(&f.flavite, buffer);
        let args_n = json!({
            "buffer_id": buffer,
            "revision": revision(&f.flavite, buffer),
            "operation": op_n,
            "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
            "selections": {"primary": 0, "items": items(&[(0, 0)])},
        });
        let r = execute(&f.flavite, "editor.delete", args_n);
        assert!(r.is_success(), "delete {r:?}");
    }
    let stale = execute(&f.flavite, "editor.delete", args);
    assert_eq!(message_of(&stale).as_deref(), Some("ReceiptExpired"));
}

#[test]
fn delete_via_kernel_installs_atomically_and_undo_restores_exactly() {
    let f = support::fixture(&[("note.md", "one café\nsecond line\n")]);
    let buffer = open_note(&f, "note.md");
    let before = revision(&f.flavite, buffer);
    let op = issue_operation(&f.flavite, buffer);
    let args = json!({
        "buffer_id": buffer,
        "revision": before,
        "operation": op,
        "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
        "selections": {"primary": 0, "items": items(&[(4, 9)])},
    });
    let receipt = data(execute(&f.flavite, "editor.delete", args));
    assert_eq!(receipt["revision"].as_u64(), Some(before + 1));
    let movement = &receipt["movement"];
    assert_eq!(movement["truncated"], json!(false));
    let spans = movement["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0]["start"], json!(4));
    assert_eq!(spans[0]["end"], json!(9));
    assert_eq!(spans[0]["replacement_len"], json!(0));
    assert_eq!(
        receipt["after_selections"].as_array().unwrap()[0],
        json!([0, 4, 4])
    );
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    assert_eq!(view.text, "one \nsecond line\n");

    // Undo restores the exact text and the recorded selection.
    let undone = data(execute(
        &f.flavite,
        "buffer.undo",
        json!({"buffer_id": buffer, "revision": revision(&f.flavite, buffer)}),
    ));
    assert_eq!(
        undone["movement"]["selections"].as_array().unwrap()[0],
        json!([0, 4, 9])
    );
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    assert_eq!(view.text, "one café\nsecond line\n");
}

#[test]
fn kernel_delete_empty_line_is_a_zero_effect_command() {
    let f = support::fixture(&[("note.md", "a\n\nb")]);
    let buffer = open_note(&f, "note.md");
    let before = revision(&f.flavite, buffer);
    let op = issue_operation(&f.flavite, buffer);
    // Caret on the empty line (index 2) is a no-op.
    let args = json!({
        "buffer_id": buffer,
        "revision": before,
        "operation": op,
        "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
        "selections": {"primary": 0, "items": items(&[(2, 2)])},
    });
    let receipt = data(execute(&f.flavite, "editor.delete", args));
    assert_eq!(
        receipt["revision"].as_u64(),
        Some(before),
        "no revision bump"
    );
    assert_eq!(receipt["movement"]["spans"].as_array().unwrap().len(), 0);
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    assert_eq!(view.text, "a\n\nb");
    // And no history node was created: undo has nothing to do.
    let undo = execute(
        &f.flavite,
        "buffer.undo",
        json!({"buffer_id": buffer, "revision": revision(&f.flavite, buffer)}),
    );
    assert!(!undo.is_success());
}

#[test]
fn stale_revision_delete_leaves_everything_byte_identical() {
    let f = support::fixture(&[("note.md", "abc\n")]);
    let buffer = open_note(&f, "note.md");
    let op = issue_operation(&f.flavite, buffer);
    // Advance the buffer first so the captured revision is stale.
    data(execute(
        &f.flavite,
        "apply_edits",
        json!({
            "buffer_id": buffer,
            "revision": revision(&f.flavite, buffer),
            "ops": [{"range": [0, 0], "replacement": "X"}],
            "undo_policy": "isolated",
        }),
    ));
    let before_text = f.flavite.get_buffer_view(buffer.into(), None).unwrap().text;
    let args = json!({
        "buffer_id": buffer,
        "revision": revision(&f.flavite, buffer) - 1,
        "operation": op,
        "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
        "selections": {"primary": 0, "items": items(&[(1, 2)])},
    });
    let result = execute(&f.flavite, "editor.delete", args);
    assert!(matches!(
        result,
        ExecutionResult::Rejected(_) | ExecutionResult::Local(LocalResult::Error(_))
    ));
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    assert_eq!(view.text, before_text, "stale delete must not mutate");
}

#[test]
fn selection_only_commands_differ_exactly_as_specified_at_n_gt_1() {
    let f = support::fixture(&[("note.md", "abcdefghij")]);
    let buffer = open_note(&f, "note.md");
    let base = revision(&f.flavite, buffer);
    let set = json!({
        "primary": 1,
        "items": [[1, 0, 2], [2, 4, 6], [3, 8, 8]],
    });
    let args = json!({
        "buffer_id": buffer,
        "revision": base,
        "selections": set,
    });

    // keep_primary: only the primary member survives, at the same revision.
    let result = data(execute(&f.flavite, "editor.keep_primary", args.clone()));
    assert_eq!(result["revision"].as_u64(), Some(base));
    assert_eq!(
        result["selections"].as_array().unwrap().to_vec(),
        vec![json!([1, 0, 2])]
    );

    // collapse: every member becomes a caret at its head.
    let result = data(execute(&f.flavite, "editor.collapse", args.clone()));
    assert_eq!(
        result["selections"].as_array().unwrap().to_vec(),
        vec![json!([1, 2, 2]), json!([2, 6, 6]), json!([3, 8, 8])]
    );

    // clear_selection: one caret at the primary head.
    let result = data(execute(&f.flavite, "editor.clear_selection", args.clone()));
    assert_eq!(
        result["selections"].as_array().unwrap().to_vec(),
        vec![json!([1, 2, 2])]
    );

    // The buffer text and revision are untouched by all three.
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    assert_eq!(view.text, "abcdefghij");
    assert_eq!(revision(&f.flavite, buffer), base);
}

#[test]
fn worst_case_10000_member_request_fits_the_payload_ceiling() {
    let f = support::fixture(&[("note.md", "a")]);
    let buffer = open_note(&f, "note.md");
    let items: Vec<[usize; 3]> = (0..10_000).map(|i| [i, 0, 0]).collect();
    let args = json!({
        "buffer_id": buffer,
        "revision": revision(&f.flavite, buffer),
        "selections": {"primary": 0, "items": items},
    });
    let encoded = serde_json::to_vec(&args).unwrap();
    assert!(
        encoded.len() < 512 * 1024,
        "worst-case set request must fit the 512 KiB ceiling ({} bytes)",
        encoded.len()
    );
}

#[test]
fn payload_ceiling_is_checked_before_dispatch_for_giant_requests() {
    let f = support::fixture(&[("note.md", "a")]);
    let buffer = open_note(&f, "note.md");
    // A 100k-member request must be refused without touching the buffer.
    let items: Vec<[usize; 3]> = (0..100_000).map(|i| [i, 0, 0]).collect();
    let args = json!({
        "buffer_id": buffer,
        "revision": revision(&f.flavite, buffer),
        "selections": {"primary": 0, "items": items},
    });
    let result = execute(&f.flavite, "editor.keep_primary", args);
    let ExecutionResult::Rejected(rejected) = result else {
        panic!("giant request must be rejected, got {result:?}");
    };
    assert!(rejected.message.as_deref().unwrap_or("").contains("KiB"));
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    assert_eq!(view.text, "a");
}

#[test]
fn multi_span_delete_publishes_one_span_per_member_and_undo_restores_exactly() {
    let f = support::fixture(&[("note.md", "one café\nsecond line\n")]);
    let buffer = open_note(&f, "note.md");
    let before = revision(&f.flavite, buffer);
    let op = issue_operation(&f.flavite, buffer);
    // Three disjoint word members: café (4..9), second (10..16), line (17..21).
    let args = json!({
        "buffer_id": buffer,
        "revision": before,
        "operation": op,
        "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
        "selections": {"primary": 0, "items": items(&[(4, 9), (10, 16), (17, 21)])},
    });
    let receipt = data(execute(&f.flavite, "editor.delete", args));
    assert_eq!(
        receipt["revision"].as_u64(),
        Some(before + 1),
        "one revision for the whole batch"
    );
    let movement = &receipt["movement"];
    assert_eq!(movement["truncated"], json!(false));
    let spans = movement["spans"].as_array().unwrap();
    assert_eq!(spans.len(), 3, "one span per member, in document order");
    assert_eq!(spans[0]["start"], json!(4));
    assert_eq!(spans[1]["start"], json!(10));
    assert_eq!(spans[2]["start"], json!(17));
    // The requesting view receives its exact mapped after-set (carets).
    let after = receipt["after_selections"].as_array().unwrap();
    assert_eq!(after.len(), 3);
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    assert_eq!(view.text, "one \n \n");

    // Undo restores the exact pre-edit text and the recorded before-set.
    let undone = data(execute(
        &f.flavite,
        "buffer.undo",
        json!({"buffer_id": buffer, "revision": revision(&f.flavite, buffer)}),
    ));
    let restored = undone["movement"]["selections"].as_array().unwrap();
    assert_eq!(restored.len(), 3);
    assert_eq!(restored[0], json!([0, 4, 9]));
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    assert_eq!(view.text, "one café\nsecond line\n");
}

#[test]
fn transitions_beyond_the_span_limit_publish_truncated_movement() {
    // 300 disjoint words: the passive-view delta would need 300 spans, past
    // the 256 ceiling (E-13) — the receipt marks the movement truncated (the
    // requesting view still gets its exact after-set) and nothing else
    // changes: one revision, exact text.
    let words: Vec<String> = (0..300).map(|i| format!("x{i:03}")).collect();
    let text = words.join(" ");
    let f = support::fixture(&[("note.md", text.as_str())]);
    let buffer = open_note(&f, "note.md");
    let before = revision(&f.flavite, buffer);
    let op = issue_operation(&f.flavite, buffer);
    // Word i occupies [i*5, i*5+4); single spaces between tokens.
    let pairs: Vec<(usize, usize)> = (0..300).map(|i| (i * 5, i * 5 + 4)).collect();
    let args = json!({
        "buffer_id": buffer,
        "revision": before,
        "operation": op,
        "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
        "selections": {"primary": 0, "items": items(&pairs)},
    });
    let receipt = data(execute(&f.flavite, "editor.delete", args));
    assert_eq!(
        receipt["revision"].as_u64(),
        Some(before + 1),
        "truncation affects passive mapping, never install"
    );
    let movement = &receipt["movement"];
    assert_eq!(movement["truncated"], json!(true));
    assert!(
        movement["spans"].as_array().unwrap().len() <= 256,
        "spans never exceed the ceiling"
    );
    assert_eq!(
        receipt["after_selections"].as_array().unwrap().len(),
        300,
        "the requesting view still receives its exact mapped set"
    );
    let view = f.flavite.get_buffer_view(buffer.into(), None).unwrap();
    let expected: Vec<String> = (0..299).map(|_| " ".to_string()).collect();
    assert_eq!(view.text, expected.join(""));
}

#[test]
fn history_budget_refusals_do_not_exhaust_operation_tickets() {
    // FEUX-0002 R5: every admitted activation reaches a terminal outcome on
    // every return path, so refusals never leak pending tickets into the
    // per-buffer cap (E-5/E-16, NFR-13).
    let f = support::fixture_with_session(&[("note.md", "one\ntwo\n")], |session| {
        session.set_app_history_budget(0);
    });
    let buffer = open_note(&f, "note.md");
    let before = revision(&f.flavite, buffer);
    let args = |operation: Value| {
        json!({
            "buffer_id": buffer,
            "revision": before,
            "operation": operation,
            "origin": {"view": format!("{:032x}", 1), "incarnation": 1},
            "selections": {"primary": 0, "items": items(&[(0, 3)])},
        })
    };
    for _ in 0..32 {
        let op = issue_operation(&f.flavite, buffer);
        let refused = execute(&f.flavite, "editor.delete", args(op));
        assert!(!refused.is_success(), "the shrunken budget must refuse");
    }
    // Tickets never piled up: issuing still works after restoring the
    // ordinary budget, and the refused edits left text/revision untouched.
    f.flavite
        .set_history_budgets_for_test(512 * 1024 * 1024, 128 * 1024 * 1024, 10_000);
    let op = issue_operation(&f.flavite, buffer);
    let applied = execute(&f.flavite, "editor.delete", args(op));
    assert!(
        applied.is_success(),
        "ticketed editing must work again: {applied:?}"
    );
    assert_eq!(f.text(buffer), "\ntwo\n");
    assert_eq!(revision(&f.flavite, buffer), before + 1);
}

#[test]
fn rotate_then_keep_primary_keeps_the_rotated_member() {
    // FEUX-0002 R7: primary identity survives the UI/API handoff — the
    // rotated primary rides `SelectionsChanged` and the next command's
    // hint resolves to it (E-4, FR-4/FR-5/FR-7).
    let f = support::fixture(&[("note.md", "one\ntwo\nthird\n")]);
    let buffer = open_note(&f, "note.md");
    // Members at 0 and 10; the primary is the first member.
    let selections = json!({"primary": 0, "items": items(&[(0, 0), (10, 10)])});
    let rotated = data(execute(
        &f.flavite,
        "editor.rotate_primary",
        json!({"buffer_id": buffer, "revision": revision(&f.flavite, buffer), "selections": selections, "direction": "next"}),
    ));
    // The response names the new primary (the second member's id).
    assert_eq!(rotated["primary"], json!(1));
    let triples = rotated["selections"].as_array().unwrap().clone();
    assert_eq!(triples.len(), 2);

    // keep_primary with the rotated primary retains the second member.
    let kept = data(execute(
        &f.flavite,
        "editor.keep_primary",
        json!({
            "buffer_id": buffer,
            "revision": revision(&f.flavite, buffer),
            "selections": {"primary": rotated["primary"], "items": triples},
        }),
    ));
    let kept_items = kept["selections"].as_array().unwrap();
    assert_eq!(kept_items.len(), 1);
    assert_eq!(kept_items[0][1], json!(10), "the second member is kept");
    assert_eq!(kept["primary"], json!(0));
}
