//! Real Session, parser and Undo; no substitute edit engine.
#[path = "support/links.rs"]
mod support;
use flavite_api::{ExecutionResult, Invocation, Origin, Preconditions};
use support::*;
fn edit(f: &Fixture, command: &'static str, args: serde_json::Value) -> ExecutionResult {
    let result = f.flavite.execute(editor_operations_fixture::admit(
        &f.flavite,
        Invocation::new(next_id(&f.flavite), command, args, Origin::Gui),
    ));
    match result {
        ExecutionResult::Pending { request_id } => f.flavite.wait_for_operation(request_id),
        other => other,
    }
}
#[test]
fn guarded_property_patch_preserves_crlf_bom_comments_quotes_and_standalone_undo() {
    let original =
        "\u{feff}---\r\n# keep\r\ntitle: 'Old'  # inline\r\ncount: 3\r\n---\r\nBody 雪\r\n";
    let f = fixture(&[("a.md", original), ("b.md", "unchanged")]);
    let (id, revision) = f.open_note("a.md");
    let result = edit(
        &f,
        "property.set",
        serde_json::json!({"buffer_id":id,"revision":revision,"key":"title","value":{"type":"text","value":"New's 雪"}}),
    );
    assert!(result.is_success(), "{result:?}");
    assert!(
        f.text(id)
            .contains("title: 'New''s 雪'  # inline\ncount: 3\n")
    );
    let saved = f.flavite.execute(editor_operations_fixture::admit(
        &f.flavite,
        Invocation::new(
            next_id(&f.flavite),
            "note.save",
            serde_json::json!(f.root.join("a.md")),
            Origin::Gui,
        ),
    ));
    let ExecutionResult::Pending { request_id } = saved else {
        panic!("save not admitted")
    };
    assert!(f.flavite.wait_for_operation(request_id).is_success());
    assert_eq!(
        f.saved_bytes("a.md"),
        original.replace("'Old'", "'New''s 雪'").as_bytes()
    );
    assert_eq!(f.saved_bytes("b.md"), b"unchanged");
    let revision = f.flavite.get_buffer_view(id.into(), None).unwrap().revision;
    assert!(
        edit(
            &f,
            "buffer.undo",
            serde_json::json!({"buffer_id":id,"revision":revision})
        )
        .is_success()
    );
    assert!(f.text(id).contains("title: 'Old'  # inline"));
    let revision = f.flavite.get_buffer_view(id.into(), None).unwrap().revision;
    assert!(
        edit(
            &f,
            "buffer.redo",
            serde_json::json!({"buffer_id":id,"revision":revision})
        )
        .is_success()
    );
    assert!(f.text(id).contains("New''s 雪"));
}
#[test]
fn invalid_yaml_stale_task_read_only_and_forged_source_never_mutate() {
    let mut f = fixture(&[("a.md", "---\nx: 1\nx: 2\n---\n- [ ] real\n\n`- [ ] fake`\n")]);
    let (id, revision) = f.open_note("a.md");
    let original = f.text(id);
    assert!(!edit(&f,"property.set",serde_json::json!({"buffer_id":id,"revision":revision,"key":"x","value":{"type":"number","value":"3"}})).is_success());
    let start = original.find("[ ] real").unwrap();
    assert!(!edit(&f,"task.toggle",serde_json::json!({"buffer_id":id,"revision":revision,"start":original.find("[ ] fake").unwrap(),"checked":false})).is_success());
    assert_eq!(f.text(id), original);
    assert!(
        edit(
            &f,
            "task.toggle",
            serde_json::json!({"buffer_id":id,"revision":revision,"start":start,"checked":false})
        )
        .is_success()
    );
    let current = f.text(id);
    assert!(current.contains("- [x] real"));
    assert!(
        !edit(
            &f,
            "task.toggle",
            serde_json::json!({"buffer_id":id,"revision":revision,"start":start,"checked":false})
        )
        .is_success()
    );
    assert_eq!(f.text(id), current);
    let revision = f.flavite.get_buffer_view(id.into(), None).unwrap().revision;
    let args = serde_json::json!({"buffer_id":id,"revision":revision,"start":start,"checked":true});
    let denied = f.flavite.execute(editor_operations_fixture::admit(
        &f.flavite,
        Invocation::new(
            next_id(&f.flavite),
            "task.toggle",
            args.clone(),
            Origin::Plugin,
        ),
    ));
    assert!(!denied.is_success());
    f.flavite.set_read_only(true);
    let bypass = f.flavite.execute(editor_operations_fixture::admit(
        &f.flavite,
        Invocation::with_preconditions(
            next_id(&f.flavite),
            "task.toggle",
            args,
            Origin::Gui,
            Preconditions::default(),
        ),
    ));
    assert!(!bypass.is_success());
    assert_eq!(f.text(id), current);
}

#[test]
fn task_patch_preserves_standalone_carriage_return_next_to_crlf_in_real_session() {
    let original = "\r\r\n- [ ] task\n";
    let f = fixture(&[("a.md", original)]);
    let (buffer, revision) = f.open_note("a.md");
    let normalized = f.text(buffer);
    let start = normalized.find("[ ]").unwrap();
    assert!(edit(&f,"task.toggle",serde_json::json!({"buffer_id":buffer,"revision":revision,"start":start,"checked":false})).is_success());
    assert_eq!(f.text(buffer), normalized.replace("[ ]", "[x]"));
    let result = edit(&f, "note.save", serde_json::json!(f.root.join("a.md")));
    assert!(result.is_success(), "{result:?}");
    assert_eq!(
        f.saved_bytes("a.md"),
        original.replace("[ ]", "[x]").as_bytes()
    );
}

#[test]
fn link_and_structure_completions_keep_distinct_public_request_identities() {
    let f = fixture(&[("a.md", "---\ncount: 1\n---\nBody\n"), ("b.md", "Target\n")]);
    let (buffer, revision) = f.open_note("a.md");
    let link_request = 2;
    let property_request = 10_000;
    let link = f.flavite.execute(editor_operations_fixture::admit(&f.flavite, Invocation::new(link_request,"link.prepare",serde_json::json!({"intent":{"client":f.client,"sequence":1},"source":{"buffer_id":buffer,"revision":revision,"caret":f.text(buffer).len()},"target_path":"b.md","expected_target":f.flavite.link_target_identity("b.md").unwrap(),"stable_link_policy":"disabled"}),Origin::Gui)));
    assert!(link.is_pending(), "{link:?}");
    let property = f.flavite.execute(editor_operations_fixture::admit(&f.flavite, Invocation::new(property_request,"property.set",serde_json::json!({"buffer_id":buffer,"revision":revision,"key":"count","value":{"type":"number","value":"2"}}),Origin::Gui)));
    assert!(property.is_pending(), "{property:?}");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut outcomes = std::collections::BTreeMap::new();
    while outcomes.len() < 2 {
        for completion in f.flavite.drain_operation_completions() {
            outcomes.insert(completion.request_id, completion.result);
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(
        matches!(&outcomes[&property_request],ExecutionResult::Local(flavite_api::LocalResult::Success(result)) if result.command_id == "apply_edits")
    );
    assert!(
        !matches!(&outcomes[&link_request],ExecutionResult::Local(flavite_api::LocalResult::Success(result)) if result.command_id == "apply_edits")
    );
    assert!(f.text(buffer).contains("count: 2"));
}

#[path = "support/editor_operations.rs"]
mod editor_operations_fixture;
