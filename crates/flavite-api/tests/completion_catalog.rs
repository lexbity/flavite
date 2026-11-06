//! Canonical completion uses the existing semantic prepare/consume-once path.
#[path = "support/links.rs"]
mod support;
use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};
use flavite_fs::Fs;

fn prepare(f: &support::Fixture, buffer: u64, revision: u64, client: u128) -> ExecutionResult {
    let identity = f.flavite.link_target_identity("beta.md").unwrap();
    let result = f.flavite.execute(Invocation::new(
        support::next_id(&f.flavite),
        "link.prepare",
        serde_json::json!({
            "intent": {"client": client, "sequence": 1},
            "source": {"buffer_id": buffer, "revision": revision, "caret": 4},
            "target_path": "beta.md", "expected_target": identity,
            "stable_link_policy": "disabled",
            "completion": {"start": 0, "end": 4, "prefix": "[[Be"}
        }),
        Origin::Gui,
    ));
    match result {
        ExecutionResult::Pending { request_id } => f.flavite.wait_for_operation(request_id),
        terminal => terminal,
    }
}

fn plan(result: &ExecutionResult) -> u128 {
    let ExecutionResult::Local(LocalResult::Success(success)) = result else {
        panic!("{result:?}")
    };
    success.data["plan_id"].as_str().unwrap().parse().unwrap()
}
fn commit(f: &support::Fixture, client: u128, plan_id: u128) -> ExecutionResult {
    f.flavite.execute(Invocation::new(
        support::next_id(&f.flavite),
        "link.insert",
        serde_json::json!({"intent":{"client":client,"sequence":1},"plan_id":plan_id}),
        Origin::Gui,
    ))
}

#[test]
fn completion_replaces_only_the_original_prefix_and_replays_one_receipt() {
    let f = support::fixture(&[("source.md", "[[Be"), ("beta.md", "# Beta")]);
    let (buffer, revision) = f.open_note("source.md");
    let client = f.flavite.new_link_client();
    let prepared = prepare(&f, buffer, revision, client);
    let id = plan(&prepared);
    let applied = commit(&f, client, id);
    assert!(applied.is_success(), "{applied:?}");
    assert_eq!(f.text(buffer), "[[beta|Beta]]");
    let first_revision = f
        .flavite
        .get_buffer_view(buffer.into(), None)
        .unwrap()
        .revision;
    assert!(commit(&f, client, id).is_success());
    assert_eq!(
        f.flavite
            .get_buffer_view(buffer.into(), None)
            .unwrap()
            .revision,
        first_revision
    );
}

#[test]
fn renamed_target_after_prepare_refuses_completion_without_a_source_effect() {
    let f = support::fixture(&[("source.md", "[[Be"), ("beta.md", "# Beta")]);
    let (buffer, revision) = f.open_note("source.md");
    let client = f.flavite.new_link_client();
    let id = plan(&prepare(&f, buffer, revision, client));
    f.fs.rename_no_replace(&f.root.join("beta.md"), &f.root.join("moved.md"))
        .unwrap();
    support::reconcile(&f.flavite);
    assert!(!commit(&f, client, id).is_success());
    assert_eq!(f.text(buffer), "[[Be");
    assert_eq!(
        f.flavite
            .get_buffer_view(buffer.into(), None)
            .unwrap()
            .revision,
        revision
    );
}

#[test]
fn cancelled_view_client_cannot_complete_a_prefix() {
    let f = support::fixture(&[("source.md", "[[Be"), ("beta.md", "# Beta")]);
    let (buffer, revision) = f.open_note("source.md");
    let client = f.flavite.new_link_client();
    let id = plan(&prepare(&f, buffer, revision, client));
    f.flavite.cancel_link_intent(client, 1);
    assert!(!commit(&f, client, id).is_success());
    assert_eq!(f.text(buffer), "[[Be");
}
