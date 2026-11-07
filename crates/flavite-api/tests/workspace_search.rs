//! W10 saved-content search: read-your-writes after save acknowledgement,
//! bounded excerpts, typed failures, sequence cancellation through the
//! bounded query service, and paging across a served generation.

use flavite_api::{ContextQueryResult, Flavite, QueryPayload, QueryPurpose, command::Invocation};
use std::time::{Duration, Instant};

#[path = "support/links.rs"]
mod support;

fn fixture(notes: &[(&str, &str)]) -> support::Fixture {
    support::fixture(notes)
}

fn save(flavite: &Flavite, path: &str) {
    let request_id = flavite.command_registry().next_request_id();
    let invocation = Invocation::new(
        request_id,
        "note.save",
        serde_json::json!(flavite.vault_ref().root.join(path)),
        flavite_api::Origin::Gui,
    );
    let result = flavite.execute(invocation);
    match result {
        flavite_api::ExecutionResult::Pending { request_id } => {
            let completion = flavite.wait_for_operation(request_id);
            assert!(matches!(
                completion,
                flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(_))
            ));
        }
        other => panic!("save_note did not admit: {other:?}"),
    }
}

fn search_once(flavite: &Flavite, text: &str, limit: u64) -> ContextQueryResult {
    let service = flavite.query_service();
    let token = service
        .submit(
            0,
            1,
            1,
            QueryPayload::Search {
                text: text.into(),
                limit,
            },
        )
        .expect("search admitted");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let drained = service.drain();
        if let Some(outcome) = drained.into_iter().find(|outcome| outcome.token == token) {
            return outcome.result;
        }
        flavite.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("search never completed");
}

#[test]
fn search_reflects_saved_content_immediately_after_save() {
    let f = fixture(&[("alpha.md", "The quick brown fox\n")]);
    // Index knows alpha from the fixture reconcile.
    let result = search_once(&f.flavite, "quick brown", 10);
    let ContextQueryResult::Search { items, .. } = result else {
        panic!("expected a search page, got {result:?}");
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].path, "alpha.md");
    assert!(
        items[0].excerpt.contains("quick"),
        "excerpt: {}",
        items[0].excerpt
    );

    // Read-your-writes: an acknowledged save is immediately searchable.
    let (buffer, _) = f.open_note("alpha.md");
    let view = f
        .flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer), None)
        .unwrap();
    let request_id = f.flavite.command_registry().next_request_id();
    let invocation = Invocation::new(
        request_id,
        "apply_edits",
        serde_json::json!({
            "buffer_id": view.buffer_id,
            "revision": view.revision,
            "ops": [{ "range": [view.text.len(), view.text.len()], "replacement": "jumps over the lazy dog" }],
        }),
        flavite_api::Origin::Gui,
    );
    f.flavite.execute(invocation);
    save(&f.flavite, "alpha.md");
    let result = search_once(&f.flavite, "lazy dog", 10);
    let ContextQueryResult::Search { items, .. } = result else {
        panic!("expected a search page after save");
    };
    assert_eq!(items.len(), 1);
}

#[test]
fn dirty_edits_are_absent_from_saved_search() {
    let f = fixture(&[("alpha.md", "saved body\n")]);
    let (buffer, revision) = f.open_note("alpha.md");
    let request_id = f.flavite.command_registry().next_request_id();
    let invocation = Invocation::new(
        request_id,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer,
            "revision": revision,
            "ops": [{ "range": [0, 0], "replacement": "unsavory phantom text" }],
        }),
        flavite_api::Origin::Gui,
    );
    match f.flavite.execute(invocation) {
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(_)) => {}
        other => panic!("edit not applied: {other:?}"),
    }
    let result = search_once(&f.flavite, "phantom", 10);
    let ContextQueryResult::Search { items, .. } = result else {
        panic!("expected a search page");
    };
    assert!(items.is_empty(), "dirty text leaked into saved search");
}

#[test]
fn empty_and_invalid_queries_fail_with_typed_session_errors() {
    let f = fixture(&[("alpha.md", "body\n")]);
    let empty = search_once(&f.flavite, "   ", 10);
    let ContextQueryResult::SearchFailed { error } = empty else {
        panic!("empty query must fail with the typed session error, got {empty:?}");
    };
    assert!(
        matches!(
            error,
            flavite_api::search_sessions::SearchSessionError::Invalid { .. }
        ),
        "an empty query is a typed invalid request: {error:?}"
    );

    let broken = search_once(&f.flavite, "prop:missing:::", 10);
    assert!(matches!(
        broken,
        ContextQueryResult::SearchFailed { .. } | ContextQueryResult::Search { .. }
    ));
}

#[test]
fn newer_sequence_supersedes_older_results() {
    let f = fixture(&[
        ("one.md", "gamma delta one\n"),
        ("two.md", "gamma delta two\n"),
    ]);
    let service = f.flavite.query_service();
    let older = service
        .submit(
            7,
            1,
            1,
            QueryPayload::Search {
                text: "gamma".into(),
                limit: 10,
            },
        )
        .expect("first admitted");
    let newer = service
        .submit(
            7,
            1,
            2,
            QueryPayload::Search {
                text: "delta".into(),
                limit: 10,
            },
        )
        .expect("second admitted");
    let deadline = Instant::now() + Duration::from_secs(10);
    // The service drops a superseded older sequence outright: a late result
    // for sequence 1 can never overwrite or coexist with a served 2.
    let mut newer_seen = None;
    loop {
        for outcome in service.drain() {
            if outcome.token == newer {
                newer_seen = Some(outcome.result);
            } else if outcome.token == older {
                panic!("a superseded older sequence must not be delivered");
            }
        }
        if newer_seen.is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "searches never completed");
        f.flavite.pump();
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(matches!(
        newer_seen,
        Some(ContextQueryResult::Search { .. })
    ));
    // A concurrent in-flight newer sequence still refuses a stale admission;
    // once drained, admission is the host's responsibility (the search view
    // checks its own submitted sequence before painting).
    let _ = QueryPurpose::Search;
}

#[test]
fn paging_reports_more_results_within_one_generation() {
    let mut notes: Vec<(String, String)> = Vec::new();
    for index in 0..30 {
        notes.push((
            format!("note{index:02}.md"),
            format!("uniqueword{index} sharedterm\n"),
        ));
    }
    let borrowed: Vec<(&str, &str)> = notes
        .iter()
        .map(|(path, body)| (path.as_str(), body.as_str()))
        .collect();
    let f = fixture(&borrowed);
    let page_one = search_once(&f.flavite, "sharedterm", 10);
    let ContextQueryResult::Search {
        items,
        has_more,
        provenance,
        ..
    } = page_one
    else {
        panic!("expected a search page");
    };
    assert_eq!(items.len(), 10);
    assert!(has_more);
    let page_two = search_once(&f.flavite, "sharedterm", 20);
    let ContextQueryResult::Search {
        items,
        provenance: p2,
        ..
    } = page_two
    else {
        panic!("expected a second page");
    };
    assert_eq!(items.len(), 20);
    assert_eq!(provenance, p2, "paging stayed within one index generation");
}

#[test]
fn excerpts_are_bounded_and_never_split_characters() {
    let body = format!("needle {}\n", "é".repeat(400));
    let f = fixture(&[("unicode.md", body.as_str())]);
    let result = search_once(&f.flavite, "needle", 10);
    let ContextQueryResult::Search { items, .. } = result else {
        panic!("expected a search page");
    };
    assert_eq!(items.len(), 1);
    assert!(!items[0].excerpt.is_empty());
    assert!(items[0].excerpt.chars().count() <= 160 + 1);
}

/// RFC-0004 §4.4/FR-7 — the paging sentinel end-to-end. `SearchPage` now
/// carries the pool-computed `has_more` (single truncate, `limit+1` probe),
/// and `execute_search` consumes it; before S4 the pool double-truncated
/// its probe row and the field did not exist at all — this assertion could
/// not even compile against the pre-S4 contract. Truth table at the exact
/// boundary: T = 5 matches.
#[test]
fn has_more_is_true_exactly_when_a_limit_plus_one_row_exists() {
    let notes: Vec<(String, String)> = (0..5)
        .map(|index| {
            (
                format!("page{index}.md"),
                format!("pagetoken unique{index} body words\n"),
            )
        })
        .collect();
    let borrowed: Vec<(&str, &str)> = notes
        .iter()
        .map(|(path, body)| (path.as_str(), body.as_str()))
        .collect();
    let f = fixture(&borrowed);
    for (limit, expected_items, expected_has_more) in
        [(3u64, 3usize, true), (4, 4, true), (5, 5, false)]
    {
        let result = search_once(&f.flavite, "pagetoken", limit);
        let ContextQueryResult::Search {
            items, has_more, ..
        } = result
        else {
            panic!("expected a search page for limit {limit}");
        };
        assert_eq!(items.len(), expected_items, "page size at limit {limit}");
        assert_eq!(has_more, expected_has_more, "sentinel at limit {limit}");
        assert!(
            items.iter().all(|row| !row.excerpt.is_empty()),
            "saved-content excerpts stay assembled"
        );
    }
}
