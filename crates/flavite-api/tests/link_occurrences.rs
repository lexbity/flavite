//! Exact directed source-span removal, stale chooser refusal and standalone Undo.
#[path = "support/links.rs"]
mod links_fixture;
use flavite_api::{
    ContextQueryResult, ExecutionResult, Invocation, LocalResult, Origin, QueryPayload,
};
use flavite_fs::Fs;
use links_fixture::*;
use std::time::{Duration, Instant};
fn wait_graph(f: &Fixture) -> std::sync::Arc<flavite_api::GraphFrame> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        f.flavite.pump();
        f.flavite.graph_notify_changed();
        if let Some(frame) = f
            .flavite
            .graph_frame()
            .filter(|_| f.flavite.graph_is_current())
        {
            return frame;
        }
        assert!(Instant::now() < deadline, "graph unavailable");
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn detail(
    f: &Fixture,
    occurrence: flavite_api::LinkOccurrence,
    epoch: u64,
) -> flavite_api::OccurrenceDetail {
    let token = f
        .flavite
        .query_service()
        .submit(71, 1, 1, QueryPayload::Occurrence { epoch, occurrence })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for outcome in f.flavite.query_service().drain() {
            if outcome.token == token {
                let ContextQueryResult::Occurrence(detail) = outcome.result else {
                    panic!("inspection failed: {:?}", outcome.result);
                };
                return detail;
            }
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn prepare(
    f: &Fixture,
    id: u64,
    revision: u64,
    detail: &flavite_api::OccurrenceDetail,
    sequence: u64,
) -> u128 {
    let result=f.flavite.execute(Invocation::new(next_id(&f.flavite),"link.prepare_remove",serde_json::json!({"intent":{"client":f.client,"sequence":sequence},"source":{"buffer_id":id,"revision":revision,"caret":detail.start},"target_path":"b.md","epoch":detail.epoch,"target_entity_id":detail.occurrence.edge.to.id,"target_is_attachment":detail.occurrence.edge.to.kind==flavite_graph::NodeKind::Attachment,"target_is_unresolved":detail.occurrence.edge.to.kind==flavite_graph::NodeKind::Unresolved,"start":detail.start,"end":detail.end,"expected_source":detail.exact_source}),Origin::Gui));
    let ExecutionResult::Pending { request_id } = result else {
        panic!("prepare not admitted: {result:?}");
    };
    let done = f.flavite.wait_for_operation(request_id);
    let ExecutionResult::Local(LocalResult::Success(result)) = done else {
        panic!("prepare failed: {done:?}");
    };
    result.data["plan_id"].as_str().unwrap().parse().unwrap()
}
fn commit(f: &Fixture, plan: u128, sequence: u64) -> ExecutionResult {
    f.flavite.execute(Invocation::new(
        next_id(&f.flavite),
        "link.remove",
        serde_json::json!({"intent":{"client":f.client,"sequence":sequence},"plan_id":plan}),
        Origin::Gui,
    ))
}
#[test]
fn removes_one_saved_crlf_occurrence_keeps_other_occurrences_prose_and_reciprocal_link() {
    let f = fixture(&[
        (
            "a.md",
            "Intro\r\nSee [[b|first]] and [[b#Heading|second]].\r\nTail\r\n",
        ),
        ("b.md", "[[a]] untouched"),
    ]);
    let frame = wait_graph(&f);
    let selected = frame
        .projection()
        .occurrences
        .iter()
        .filter(|o| o.source_path == std::path::Path::new("a.md"))
        .nth(1)
        .unwrap()
        .clone();
    let detail = detail(&f, selected, frame.render_key().epoch);
    assert_eq!(detail.exact_source, "[[b#Heading|second]]");
    let (id, revision) = f.open_note("a.md");
    let before = f.text(id);
    let target = f.saved_bytes("b.md");
    let plan = prepare(&f, id, revision, &detail, 1);
    assert!(commit(&f, plan, 1).is_success());
    assert_eq!(f.text(id), "Intro\nSee [[b|first]] and .\nTail\n");
    assert_eq!(f.saved_bytes("b.md"), target);
    assert!(
        commit(&f, plan, 1).is_success(),
        "duplicate commit is cached, never deletes twice"
    );
    let frame = wait_graph(&f);
    assert_eq!(
        frame.projection().effective.edges.len(),
        2,
        "parallel occurrence removal retains edge and reciprocal edge"
    );
    let view = f.flavite.get_buffer_view(id.into(), None).unwrap();
    assert!(
        f.flavite
            .execute(Invocation::new(
                next_id(&f.flavite),
                "undo",
                serde_json::json!({"buffer_id":id,"revision":view.revision}),
                Origin::Gui
            ))
            .is_success()
    );
    assert_eq!(f.text(id), before);
}
#[test]
fn unresolved_connection_removal_uses_the_exact_source_occurrence() {
    let f = fixture(&[("a.md", "Before [[missing]] after [[missing]].")]);
    let frame = wait_graph(&f);
    let selected = frame.projection().occurrences.iter().next().expect("occurrence").clone();
    assert_eq!(selected.edge.to.kind, flavite_graph::NodeKind::Unresolved);
    let detail = detail(&f, selected, frame.render_key().epoch);
    let (id, revision) = f.open_note("a.md");
    let plan = prepare(&f, id, revision, &detail, 1);
    assert!(commit(&f, plan, 1).is_success());
    assert_eq!(f.text(id), "Before  after [[missing]].");
}

#[test]
fn an_unresolved_target_resolved_after_prepare_requires_a_new_choice() {
    let f = fixture(&[("a.md", "Before [[missing]] after.")]);
    let frame = wait_graph(&f);
    let detail = detail(
        &f,
        frame.projection().occurrences.iter().next().expect("occurrence").clone(),
        frame.render_key().epoch,
    );
    let (id, revision) = f.open_note("a.md");
    let plan = prepare(&f, id, revision, &detail, 1);
    f.fs.write_new(&f.root.join("missing.md"), b"New target")
        .unwrap();
    reconcile(&f.flavite);
    assert!(!commit(&f, plan, 1).is_success());
    assert_eq!(f.text(id), "Before [[missing]] after.");
    assert_eq!(f.saved_bytes("missing.md"), b"New target");
}

#[test]
fn later_typing_invalidates_prepared_removal_without_deleting_another_span() {
    let f = fixture(&[("a.md", "[[b]] [[b]]"), ("b.md", "B")]);
    let frame = wait_graph(&f);
    let selected = frame.projection().occurrences.iter().nth(1).expect("occurrence").clone();
    let detail = detail(&f, selected, frame.render_key().epoch);
    let (id, revision) = f.open_note("a.md");
    let plan = prepare(&f, id, revision, &detail, 1);
    assert!(f.flavite.execute(Invocation::new(next_id(&f.flavite),"apply_edits",serde_json::json!({"buffer_id":id,"revision":revision,"ops":[{"range":[0,0],"replacement":"shift "}]}),Origin::Gui)).is_success());
    let before = f.text(id);
    assert_eq!(error_code(&commit(&f, plan, 1)), "link.stale_source");
    assert_eq!(f.text(id), before);
}
#[test]
fn insertion_and_removal_are_independent_undo_units_amid_adjacent_typing() {
    let f = fixture(&[("a.md", "Hello"), ("b.md", "B")]);
    let (id, revision) = f.open_note("a.md");
    assert!(f.flavite.execute(Invocation::new(next_id(&f.flavite),"apply_edits",serde_json::json!({"buffer_id":id,"revision":revision,"ops":[{"range":[5,5],"replacement":"?"}]}),Origin::Gui)).is_success());
    let revision = f.flavite.get_buffer_view(id.into(), None).unwrap().revision;
    let (sequence, plan, _) = f.prepare(1, id, revision, 6, "b.md");
    assert!(f.commit(sequence, plan).is_success());
    let revision = f.flavite.get_buffer_view(id.into(), None).unwrap().revision;
    assert!(
        f.flavite
            .execute(Invocation::new(
                next_id(&f.flavite),
                "undo",
                serde_json::json!({"buffer_id":id,"revision":revision}),
                Origin::Gui
            ))
            .is_success()
    );
    assert_eq!(
        f.text(id),
        "Hello?",
        "one undo removes only the semantic insertion"
    );
}
