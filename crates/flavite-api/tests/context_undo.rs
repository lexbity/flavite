//! Explicit typing runs use the public API, real Session, and normal history.
#[path = "support/links.rs"]
mod support;

use flavite_api::{ExecutionResult, Flavite, Invocation, LocalResult, Origin};
use serde_json::{Value, json};

fn execute(api: &Flavite, command: &'static str, args: Value) -> ExecutionResult {
    api.execute(editor_operations_fixture::admit(
        &api,
        Invocation::new(support::next_id(api), command, args, Origin::Gui),
    ))
}

fn success(result: ExecutionResult) -> Value {
    let ExecutionResult::Local(LocalResult::Success(value)) = result else {
        panic!("expected success: {result:?}")
    };
    value.data
}

fn revision(api: &Flavite, buffer: u64) -> u64 {
    api.get_buffer_view(buffer.into(), None).unwrap().revision
}

fn append(api: &Flavite, buffer: u64, text: &str, policy: Value) -> u64 {
    let view = api.get_buffer_view(buffer.into(), None).unwrap();
    success(execute(
        api,
        "apply_edits",
        json!({
            "buffer_id":buffer,"revision":view.revision,
            "ops":[{"range":[view.text.len(),view.text.len()],"replacement":text}],
            "undo_policy":policy
        }),
    ));
    revision(api, buffer)
}

#[test]
fn explicit_run_seal_is_idempotent_revision_guarded_and_not_a_document_edit() {
    let f = support::fixture(&[("note.md", "café\n")]);
    let api = &f.flavite;
    let (buffer, initial) = f.open_note("note.md");
    let policy = json!({"context":{"origin":"a".repeat(32)}});
    append(api, buffer, "A", policy.clone());
    let before = append(api, buffer, "雪", policy.clone());
    let args = json!({"buffer_id":buffer,"revision":before});
    for _ in 0..2 {
        let data = success(execute(api, "buffer.seal_undo_group", args.clone()));
        assert_eq!(data["revision"], before);
        assert_eq!(revision(api, buffer), before);
        assert_eq!(f.text(buffer), "café\nA雪");
    }
    assert_eq!(f.saved_bytes("note.md"), "café\n".as_bytes());
    let stale = execute(
        api,
        "buffer.seal_undo_group",
        json!({"buffer_id":buffer,"revision":initial}),
    );
    assert!(!stale.is_success(), "stale revision must refuse: {stale:?}");
    append(api, buffer, "B", policy);
    success(execute(
        api,
        "buffer.undo",
        json!({"buffer_id":buffer,"revision":revision(api,buffer)}),
    ));
    assert_eq!(f.text(buffer), "café\nA雪");
    success(execute(
        api,
        "buffer.undo",
        json!({"buffer_id":buffer,"revision":revision(api,buffer)}),
    ));
    assert_eq!(f.text(buffer), "café\n");
    success(execute(
        api,
        "buffer.redo",
        json!({"buffer_id":buffer,"revision":revision(api,buffer)}),
    ));
    assert_eq!(f.text(buffer), "café\nA雪");
}

#[test]
fn other_editor_or_ordinary_api_edit_cannot_join_the_typing_run() {
    let f = support::fixture(&[("note.md", "")]);
    let api = &f.flavite;
    let (buffer, _) = f.open_note("note.md");
    let p = |origin| json!({"context":{"origin":format!("{origin:032x}")}});
    append(api, buffer, "A", p(1));
    append(api, buffer, "B", p(2));
    append(api, buffer, "C", json!("typing"));
    append(api, buffer, "D", p(2));
    for expected in ["ABC", "AB", "A", ""] {
        success(execute(
            api,
            "buffer.undo",
            json!({"buffer_id":buffer,"revision":revision(api,buffer)}),
        ));
        assert_eq!(f.text(buffer), expected);
    }
}

#[test]
fn malformed_origin_stale_edits_and_read_only_seals_have_no_effect() {
    let mut f = support::fixture(&[("note.md", "source")]);
    let (buffer, original) = f.open_note("note.md");
    for origin in ["abc".into(), "A".repeat(32), "z".repeat(32)] {
        let result = execute(
            &f.flavite,
            "apply_edits",
            json!({
                "buffer_id":buffer,"revision":original,
                "ops":[{"range":[0,0],"replacement":"bad"}],
                "undo_policy":{"context":{"origin":origin}}
            }),
        );
        assert!(!result.is_success(), "invalid origin accepted: {result:?}");
        assert_eq!(revision(&f.flavite, buffer), original);
        assert_eq!(f.text(buffer), "source");
    }
    let current = append(
        &f.flavite,
        buffer,
        "ok",
        json!({"context":{"origin":"0".repeat(32)}}),
    );
    let stale = execute(
        &f.flavite,
        "apply_edits",
        json!({
            "buffer_id":buffer,"revision":original,
            "ops":[{"range":[0,0],"replacement":"bad"}],
            "undo_policy":{"context":{"origin":"0".repeat(32)}}
        }),
    );
    assert!(!stale.is_success());
    assert_eq!(f.text(buffer), "sourceok");
    f.flavite.set_read_only(true);
    let refused = execute(
        &f.flavite,
        "buffer.seal_undo_group",
        json!({"buffer_id":buffer,"revision":current}),
    );
    assert!(!refused.is_success());
    assert_eq!(revision(&f.flavite, buffer), current);
}

#[path = "support/editor_operations.rs"]
mod editor_operations_fixture;
