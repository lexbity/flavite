//! Real edit/save/index/undo schedules and structural metadata provenance.
#[path = "support/links.rs"]
mod links_fixture;
use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};
use flavite_fs::Fs;
use flavite_graph::{GraphScope, NodeId, NotePredicate};
use links_fixture::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[track_caller]
fn wait(
    f: &Fixture,
    predicate: impl Fn(&flavite_api::GraphFrame) -> bool,
) -> Arc<flavite_api::GraphFrame> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        f.flavite.pump();
        // The reevaluate kick forces a pump (never skip-able) so the wait
        // stays deterministic instead of racing the 5 s sweep; the
        // catch-up head check keeps the retired relay's correctness — a
        // frame only counts when the mirror has CAUGHT UP to the committed
        // generation, not merely when a stamp-current frame exists.
        f.flavite.graph_reevaluate();
        let head = f.flavite.vault_info().expect("vault info").generation;
        if let Some(frame) = f.flavite.graph_frame().filter(|frame| {
            frame.publication().generation >= head
                && f.flavite.graph_is_current()
                && predicate(frame)
        }) {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "current graph never arrived: {:?}; frame {:?}",
            f.flavite.graph_status(),
            f.flavite.graph_frame().map(|fr| (
                fr.publication().edge_count(),
                fr.projection().overlays.clone(),
                fr.projection().buffer_stamp.clone(),
                f.flavite.graph_is_current()
            ))
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn edit(f: &Fixture, id: u64, body: &str) -> u64 {
    let view = f.flavite.get_buffer_view(id.into(), None).unwrap();
    let result = f.flavite.execute(editor_operations_fixture::admit(&f.flavite, Invocation::new(next_id(&f.flavite), "apply_edits", serde_json::json!({"buffer_id":id,"revision":view.revision,"ops":[{"range":[0,view.text.len()],"replacement":body}]}), Origin::Gui)));
    let ExecutionResult::Local(LocalResult::Success(result)) = result else {
        panic!("edit failed: {result:?}");
    };
    result.data["revision"].as_u64().unwrap()
}
fn edges(frame: &flavite_api::GraphFrame) -> Vec<(String, String)> {
    frame
        .projection()
        .effective
        .edges
        .iter()
        .map(|edge| {
            let name = |id: NodeId| {
                frame
                    .projection()
                    .effective
                    .nodes
                    .iter()
                    .find(|n| n.id == id)
                    .unwrap()
                    .path
                    .clone()
            };
            (name(edge.from), name(edge.to))
        })
        .collect()
}

#[test]
fn dirty_replacement_survives_unrelated_generation_save_later_edit_and_undo() {
    let f = fixture(&[("a.md", "[[b]]\n"), ("b.md", "B\n"), ("c.md", "C\n")]);
    wait(&f, |frame| frame.publication().edge_count() == 1);
    let (id, _) = f.open_note("a.md");
    let revision = edit(&f, id, "[[c]]\n");
    assert!(
        !f.flavite.graph_is_current(),
        "old edges immediately become updating"
    );
    let frame = wait(&f, |frame| frame.projection().overlays.len() == 1);
    assert_eq!(edges(&frame), vec![("a.md".into(), "c.md".into())]);
    assert_eq!(frame.projection().saved.edges.len(), 1);
    assert!(
        matches!(frame.projection().occurrences.iter().next().expect("occurrence").source, flavite_api::OccurrenceSource::Buffer { revision:r,.. } if r==revision)
    );
    f.fs.write_new(&f.root.join("unrelated.md"), b"unrelated")
        .unwrap();
    reconcile(&f.flavite);
    let frame = wait(&f, |frame| frame.projection().saved.nodes.len() == 4);
    assert_eq!(edges(&frame), vec![("a.md".into(), "c.md".into())]);
    let save = f.flavite.execute(editor_operations_fixture::admit(
        &f.flavite,
        Invocation::new(
            next_id(&f.flavite),
            "note.save",
            serde_json::json!(f.root.join("a.md")),
            Origin::Gui,
        ),
    ));
    let ExecutionResult::Pending { request_id } = save else {
        panic!("save: {save:?}");
    };
    assert!(f.flavite.wait_for_operation(request_id).is_success());
    edit(&f, id, "[[b]]\n[[c]]\n");
    let frame = wait(&f, |frame| frame.publication().edge_count() == 2);
    assert_eq!(
        frame.projection().overlays.len(),
        1,
        "save of n never covers n+1"
    );
    let view = f.flavite.get_buffer_view(id.into(), None).unwrap();
    assert!(
        f.flavite
            .execute(editor_operations_fixture::admit(
                &f.flavite,
                Invocation::new(
                    next_id(&f.flavite),
                    "buffer.undo",
                    serde_json::json!({"buffer_id":id,"revision":view.revision}),
                    Origin::Gui
                )
            ))
            .is_success()
    );
    let frame = wait(&f, |frame| frame.publication().edge_count() == 1);
    assert_eq!(edges(&frame), vec![("a.md".into(), "c.md".into())]);
    let save = f.flavite.execute(editor_operations_fixture::admit(
        &f.flavite,
        Invocation::new(
            next_id(&f.flavite),
            "note.save",
            serde_json::json!(f.root.join("a.md")),
            Origin::Gui,
        ),
    ));
    if let ExecutionResult::Pending { request_id } = save {
        assert!(f.flavite.wait_for_operation(request_id).is_success());
    }
    wait(&f, |frame| frame.projection().overlays.is_empty());
}

#[test]
fn saved_and_dirty_properties_tags_and_saved_query_membership_are_consistent() {
    let f = fixture(&[
        (
            "a.md",
            "---\ntags: [alpha]\nstatus: draft\npriority: 2\n---\nsavedneedle [[b]]",
        ),
        ("b.md", "B"),
    ]);
    wait(&f, |_| true);
    let token = flavite_api::GraphViewToken {
        tab: 1,
        incarnation: 1,
        sequence: 1,
    };
    let scope = GraphScope {
        predicate: NotePredicate::Property {
            key: "status".into(),
            value: flavite_graph::PropertyValue::Text("draft".into()),
        },
        ..GraphScope::default()
    };
    f.flavite.graph_request_scope(token, scope).unwrap();
    let frame = wait(&f, |frame| frame.projection().scopes.contains_key(&1));
    assert_eq!(
        frame.projection().scopes[&1]
            .result
            .as_ref()
            .unwrap()
            .matching_notes,
        1,
        "normal ingestion populates typed properties"
    );
    let (id, _) = f.open_note("a.md");
    edit(
        &f,
        id,
        "---\ntags: [beta]\nstatus: complete\npriority: 3\n---\ndirtyneedle [[b]]",
    );
    f.flavite
        .graph_request_scope(
            flavite_api::GraphViewToken {
                sequence: 2,
                ..token
            },
            GraphScope {
                predicate: NotePredicate::And {
                    clauses: vec![
                        NotePredicate::Tag {
                            name: "beta".into(),
                        },
                        NotePredicate::Property {
                            key: "status".into(),
                            value: flavite_graph::PropertyValue::Text("complete".into()),
                        },
                        NotePredicate::SavedQuery {
                            query: "text:savedneedle".into(),
                        },
                    ],
                },
                ..GraphScope::default()
            },
        )
        .unwrap();
    let frame = wait(&f, |frame| {
        frame
            .projection()
            .scopes
            .get(&1)
            .is_some_and(|s| s.token.sequence == 2)
    });
    assert_eq!(
        frame.projection().scopes[&1]
            .result
            .as_ref()
            .unwrap()
            .matching_notes,
        1
    );
    f.flavite
        .graph_request_scope(
            flavite_api::GraphViewToken {
                sequence: 3,
                ..token
            },
            GraphScope {
                predicate: NotePredicate::SavedQuery {
                    query: "text:dirtyneedle".into(),
                },
                ..GraphScope::default()
            },
        )
        .unwrap();
    let frame = wait(&f, |frame| {
        frame.projection().scopes[&1].token.sequence == 3
    });
    assert_eq!(
        frame.projection().scopes[&1]
            .result
            .as_ref()
            .unwrap()
            .matching_notes,
        0,
        "dirty-only text is never saved-search membership"
    );
}

#[test]
fn failed_parse_is_unavailable_and_new_unindexed_source_has_an_ephemeral_identity() {
    let f = fixture(&[("a.md", "[[b]]"), ("b.md", "B")]);
    let (id, _) = f.open_note("a.md");
    edit(&f, id, "\0 [[b]]");
    let frame = wait(&f, |frame| {
        frame
            .projection()
            .overlays
            .values()
            .any(|s| matches!(s, flavite_api::OverlayState::Failed { .. }))
    });
    assert_eq!(frame.publication().edge_count(), 0);
    assert_eq!(
        frame.projection().saved.edges.len(),
        1,
        "saved history is retained separately"
    );
    let open = f.flavite.execute(editor_operations_fixture::admit(
        &f.flavite,
        Invocation::new(
            next_id(&f.flavite),
            "note.open",
            serde_json::json!({"path":f.root.join("new.md"),"policy":"create_if_missing"}),
            Origin::Gui,
        ),
    ));
    if let ExecutionResult::Pending { request_id } = open {
        assert!(f.flavite.wait_for_operation(request_id).is_success());
    }
    let view = f
        .flavite
        .open_source_view(std::path::Path::new("new.md"))
        .unwrap();
    edit(&f, view.buffer_id, "[[b]]");
    let frame = wait(&f, |frame| frame.projection().effective.nodes.len() == 3);
    assert!(
        frame
            .projection()
            .effective
            .nodes
            .iter()
            .any(|n| n.path == "new.md" && n.id.id > i64::MAX as u64)
    );
    assert!(
        !f.fs.exists(&f.root.join("new.md")).unwrap(),
        "opening/overlay does not create a saved file"
    );
}

#[path = "support/editor_operations.rs"]
mod editor_operations_fixture;
