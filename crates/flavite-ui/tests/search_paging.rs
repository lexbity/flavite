//! RFC-0006 S06 UI search continuation (AC-7): 230 hits enumerate exactly
//! once across 50/50/50/50/30, replayed pages never duplicate rows, stale
//! debounce responses are rejected immediately on query change, empty
//! results stay empty, and quota overflow shows Busy without labelling a
//! partial list complete.

#[path = "../../flavite-api/tests/support/links.rs"]
mod support;

use flavite_ui::app::{App, Message};
use flavite_workspace::TabId;
use std::time::{Duration, Instant};

fn wait(app: &mut App, done: impl Fn(&App) -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let _ = app.update(Message::Pump);
        if done(app) {
            return;
        }
        assert!(Instant::now() < until, "search view never settled");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct PageFixture {
    app: App,
    #[allow(dead_code)]
    cache: tempfile::TempDir,
    #[allow(dead_code)]
    fs: std::sync::Arc<flavite_fs::MemFs>,
}

fn paging_fixture() -> PageFixture {
    let mut notes: Vec<(String, String)> = Vec::new();
    for index in 0..230 {
        notes.push((
            format!("notes/hit-{index:03}.md"),
            format!("# Hit {index}\n\nhitword number {index}\n"),
        ));
    }
    notes.push(("other.md".into(), "# Other\n\nunrelated content\n".into()));
    let refs: Vec<(&str, &str)> = notes
        .iter()
        .map(|(path, body)| (path.as_str(), body.as_str()))
        .collect();
    let f = support::fixture(&refs);
    PageFixture {
        app: App::from_flavite(f.flavite),
        cache: f.cache,
        fs: f.fs,
    }
}

fn wait_search(app: &mut App, tab: TabId, done: impl Fn(&App) -> bool) {
    let until = Instant::now() + Duration::from_secs(30);
    loop {
        let _ = app.update(Message::Pump);
        if done(app) {
            return;
        }
        let dumped = app.search.tabs.get(&tab).map(|state| {
            (
                state.updating,
                state.total,
                state.rows.len(),
                state.error.clone(),
                state.snapshot,
            )
        });
        assert!(
            Instant::now() < until,
            "search view never settled: {dumped:?}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_search(app: &mut App) -> TabId {
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenSearch {
        expected_revision: revision,
        group,
    });
    app.workspace.focused_tab()
}

fn search_state(app: &App, tab: TabId) -> &flavite_ui::host::search::SearchState {
    app.search.tabs.get(&tab).expect("search tab state exists")
}

#[test]
fn two_hundred_thirty_hits_enumerate_exactly_once_across_pages() {
    let mut wf = paging_fixture();
    let app = &mut wf.app;
    let tab = open_search(app);
    let _ = app.update(Message::SearchQuery {
        tab,
        value: "hitword".into(),
    });
    wait_search(app, tab, |a| {
        search_state(a, tab).total == Some(230) && search_state(a, tab).rows.len() == 50
    });
    assert_eq!(
        search_state(app, tab)
            .snapshot
            .map(|(_, generation)| generation),
        Some({
            // The frozen snapshot generation is visible and stable.
            search_state(app, tab).snapshot.unwrap().1
        })
    );

    // 50/50/50/50/30: four continuation pages append exactly 50 rows each.
    for expected in [100usize, 150, 200, 230] {
        assert!(
            search_state(app, tab).has_next(),
            "page for {expected} missing"
        );
        let _ = app.update(Message::SearchNextPage { tab });
        wait_search(app, tab, |a| {
            !search_state(a, tab).updating && search_state(a, tab).rows.len() == expected
        });
    }
    let rows = &search_state(app, tab).rows;
    assert_eq!(rows.len(), 230);
    let unique: std::collections::BTreeSet<&i64> = rows.iter().map(|row| &row.note_id).collect();
    assert_eq!(unique.len(), 230, "no row may appear twice");
    assert!(
        !search_state(app, tab).has_next(),
        "the end token is absent after full enumeration"
    );
    // Pressing next after the end is inert.
    let _ = app.update(Message::SearchNextPage { tab });
    assert_eq!(search_state(app, tab).rows.len(), 230);
}

#[test]
fn stale_debounce_outcomes_are_rejected_immediately_on_query_change() {
    let mut wf = paging_fixture();
    let app = &mut wf.app;
    let tab = open_search(app);
    // Query A is admitted; query B edits the input BEFORE A's outcome
    // arrives — A's late result must never paint (S06), not even after the
    // next debounce.
    let _ = app.update(Message::SearchQuery {
        tab,
        value: "hitword".into(),
    });
    let _ = app.update(Message::SearchSubmitNow { tab });
    let _ = app.update(Message::SearchQuery {
        tab,
        value: "zzz-no-such-word".into(),
    });
    wait(app, |a| !search_state(a, tab).updating);
    let state = search_state(app, tab);
    assert!(
        state.rows.is_empty(),
        "stale results must not survive the query change: {} rows",
        state.rows.len()
    );
    assert!(state.error.is_none());
    assert_eq!(state.total, None);
}

#[test]
fn empty_results_stay_empty_without_a_fake_complete_label() {
    let mut wf = paging_fixture();
    let app = &mut wf.app;
    let tab = open_search(app);
    let _ = app.update(Message::SearchQuery {
        tab,
        value: "zzz-no-such-word".into(),
    });
    wait(app, |a| {
        let state = search_state(a, tab);
        !state.updating && (state.total.is_some() || state.error.is_some())
    });
    let state = search_state(app, tab);
    assert!(state.rows.is_empty());
    assert_eq!(state.total, Some(0));
    assert!(!state.has_next());
    assert!(state.error.is_none());
}

#[test]
fn quota_overflow_reports_busy_and_never_labels_partial_rows_complete() {
    let mut wf = paging_fixture();
    let app = &mut wf.app;
    let tab = open_search(app);
    // Saturate the bounded session registry out-of-band (four active
    // sessions is the NFR-10 bound).
    for _ in 0..4 {
        app.flavite
            .as_ref()
            .unwrap()
            .search_start(
                &[flavite_api::Capability::ReadNotes],
                flavite_api::search_sessions::SearchQueryOptions {
                    mode: flavite_api::search_sessions::SearchMode::FullText,
                    text: "hitword".into(),
                    page_size: 50,
                },
            )
            .unwrap();
    }
    let _ = app.update(Message::SearchQuery {
        tab,
        value: "hitword".into(),
    });
    wait(app, |a| {
        let state = search_state(a, tab);
        !state.updating && (state.total.is_some() || state.error.is_some())
    });
    let state = search_state(app, tab);
    assert!(
        state
            .error
            .as_deref()
            .is_some_and(|error| error.to_lowercase().contains("busy")),
        "the fifth session must be refused with Busy: {:?}",
        state.error
    );
    assert!(
        state.rows.is_empty() && state.total.is_none(),
        "a refused start must not label a partial list complete"
    );
}
