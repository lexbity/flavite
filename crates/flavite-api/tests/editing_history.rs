//! Public history contracts over real Session state, not a UI-side journal.
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
fn page(api: &Flavite, buffer: u64) -> Value {
    data(execute(
        api,
        "buffer.history_page",
        json!({"buffer_id":buffer,"revision":revision(api,buffer)}),
    ))["page"]
        .clone()
}
fn insert(api: &Flavite, buffer: u64, at: usize, value: &str, recorded: bool) {
    let mut args = json!({"buffer_id":buffer,"revision":revision(api,buffer),"ops":[{"range":[at,at],"replacement":value}],"undo_policy":"isolated"});
    if recorded {
        args["undo_policy"] = json!({"context":{"origin":format!("{:032x}",1)}});
        args["editor"] = json!({"origin":format!("{:032x}",1),"kind":"typing","before":[[at,at]],"after":[[at+value.len(),at+value.len()]]});
    }
    data(execute(api, "apply_edits", args));
}

#[test]
fn state_restoration_is_one_head_move_with_selection_receipts_and_no_disk_edit() {
    let mut f = support::fixture(&[("note.md", "base\r\n")]);
    let (buffer, _) = f.open_note("note.md");
    let root = page(&f.flavite, buffer)["entries"][0]["target"].clone();
    insert(&f.flavite, buffer, 0, "A", true);
    insert(&f.flavite, buffer, 1, "雪", true);
    let target = page(&f.flavite, buffer)["entries"][0]["target"].clone();
    assert_eq!(
        page(&f.flavite, buffer)["entries"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    insert(&f.flavite, buffer, 0, "B", false);
    let before = revision(&f.flavite, buffer);
    let args = json!({"buffer_id":buffer,"revision":before,"target":target});
    let receipt = data(execute(&f.flavite, "buffer.restore_state", args.clone()));
    assert_eq!(receipt["revision"], before + 1);
    assert_eq!(receipt["movement"]["selections"], json!([[4, 4]]));
    assert_eq!(f.text(buffer), "A雪base\n");
    assert_eq!(
        page(&f.flavite, buffer)["entries"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    assert!(
        !execute(&f.flavite, "buffer.restore_state", args).is_success(),
        "duplicate old activation must not install twice"
    );
    let undone = data(execute(
        &f.flavite,
        "buffer.undo",
        json!({"buffer_id":buffer,"revision":revision(&f.flavite,buffer)}),
    ));
    assert_eq!(undone["movement"]["selections"], json!([[0, 0]]));
    assert_eq!(f.text(buffer), "base\n");
    let redone = data(execute(
        &f.flavite,
        "buffer.redo",
        json!({"buffer_id":buffer,"revision":revision(&f.flavite,buffer)}),
    ));
    assert_eq!(redone["movement"]["selections"], json!([[4, 4]]));
    assert_eq!(f.text(buffer), "A雪base\n");
    data(execute(
        &f.flavite,
        "buffer.restore_state",
        json!({"buffer_id":buffer,"revision":revision(&f.flavite,buffer),"target":root}),
    ));
    assert_eq!(f.saved_bytes("note.md"), b"base\r\n");
    f.flavite.set_read_only(true);
    assert_eq!(
        page(&f.flavite, buffer)["current"],
        0,
        "reading history remains allowed"
    );
    assert!(
        !execute(
            &f.flavite,
            "buffer.restore_state",
            json!({"buffer_id":buffer,"revision":revision(&f.flavite,buffer),"target":target})
        )
        .is_success()
    );
}

#[test]
fn pages_are_bounded_and_cursors_cannot_mix_generations() {
    let f = support::fixture(&[("note.md", "")]);
    let (buffer, _) = f.open_note("note.md");
    for _ in 0..105 {
        insert(&f.flavite, buffer, 0, "A", false);
    }
    let first = page(&f.flavite, buffer);
    assert_eq!(first["entries"].as_array().unwrap().len(), 100);
    assert!(first["trail"].as_array().unwrap().len() <= 12);
    let old_cursor = first["next"].clone();
    let second = data(execute(
        &f.flavite,
        "buffer.history_page",
        json!({"buffer_id":buffer,"revision":revision(&f.flavite,buffer),"cursor":old_cursor}),
    ));
    assert_eq!(second["page"]["entries"].as_array().unwrap().len(), 6);
    insert(&f.flavite, buffer, 0, "B", false);
    let current = revision(&f.flavite, buffer);
    assert!(
        !execute(
            &f.flavite,
            "buffer.history_page",
            json!({"buffer_id":buffer,"revision":current,"cursor":old_cursor})
        )
        .is_success()
    );
    assert_eq!(revision(&f.flavite, buffer), current);
    assert!(
        !execute(
            &f.flavite,
            "buffer.history_page",
            json!({"buffer_id":buffer,"revision":current,"limit":101})
        )
        .is_success()
    );
}

#[test]
fn invalid_editor_records_do_not_add_text_or_history() {
    let f = support::fixture(&[("note.md", "e\u{301}x")]);
    let (buffer, before) = f.open_note("note.md");
    let original = page(&f.flavite, buffer);
    for record in [
        json!({"origin":"A".repeat(32),"kind":"typing","before":[[0,0]],"after":[[1,1]]}),
        json!({"origin":"0".repeat(32),"kind":"typing","before":[[1,1]],"after":[[1,1]]}),
        json!({"origin":"0".repeat(32),"kind":"typing","before":[[0,0],[3,3]],"after":[[1,1],[4,4]]}),
    ] {
        assert!(!execute(&f.flavite,"apply_edits",json!({"buffer_id":buffer,"revision":before,"ops":[{"range":[0,0],"replacement":"A"}],"editor":record})).is_success());
        assert_eq!(revision(&f.flavite, buffer), before);
        assert_eq!(f.text(buffer), "e\u{301}x");
        assert_eq!(page(&f.flavite, buffer), original);
    }
}

#[test]
fn app_wide_history_budget_refuses_with_typed_code_and_zero_effects() {
    // App-wide 512 MiB ledger, overridden to a tiny ceiling for the test.
    let f = support::fixture_with_session(&[("a.md", ""), ("b.md", "")], |session| {
        session.set_app_history_budget(2_600)
    });
    let (a, _) = f.open_note("a.md");
    let (b, _) = f.open_note("b.md");

    // First edit fits (root + projected <= ceiling).
    insert(&f.flavite, a, 0, &"x".repeat(400), false);
    assert_eq!(f.text(a), "x".repeat(400));

    // Second edit across a second buffer would push the app-wide ledger over
    // the ceiling: refused with the typed code before any mutation.
    let b_before = revision(&f.flavite, b);
    let b_page_before = page(&f.flavite, b);
    let refused = execute(
        &f.flavite,
        "apply_edits",
        json!({"buffer_id":b,"revision":b_before,
               "ops":[{"range":[0,0],"replacement":"x".repeat(400)}],
               "undo_policy":"isolated"}),
    );
    assert!(!refused.is_success());
    assert_eq!(support::error_code(&refused), "history-limit-exceeded");
    assert_eq!(f.text(b), "", "refused edit must not change text");
    assert_eq!(revision(&f.flavite, b), b_before);
    assert_eq!(page(&f.flavite, b), b_page_before);

    // The refusing buffer's own state is untouched too.
    assert_eq!(f.text(a), "x".repeat(400));
    assert_eq!(revision(&f.flavite, a), 1);
}
