//! RFC-0006 S07 UI: vault source Find and replacement preview (D-08/
//! D-09) through the full App pump — source find with typed exclusions,
//! sealed preview with exact before/after hashes, and selection.

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
        if Instant::now() >= until {
            let dump: Vec<String> = app
                .replace
                .tabs
                .values()
                .map(|state| {
                    format!(
                        "updating={} preview={:?} finder={:?} error={:?} mode={:?}",
                        state.updating, state.preview_id, state.finder, state.error, state.mode
                    )
                })
                .collect();
            panic!("replace view never settled: {dump:?}");
        }
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

fn fixture() -> PageFixture {
    let f = support::fixture(&[
        ("notes/a.md", "alpha beta alpha\n"),
        ("notes/b.md", "alpha\n"),
        ("notes/dirty.md", "dirty\n"),
    ]);
    // Dirty the dirty note through the real session so exclusions show.
    let (buffer_id, revision) = f.open_note("notes/dirty.md");
    let _ = f.flavite.execute(editor_operations_fixture::admit(
        &f.flavite,
        flavite_api::command::Invocation::new(
            support::next_id(&f.flavite),
            "apply_edits",
            serde_json::json!({
                "buffer_id": buffer_id,
                "revision": revision,
                "ops": [{"range": [0, 0], "replacement": "prefix "}],
                "undo_policy": "typing",
            }),
            flavite_api::Origin::Gui,
        ),
    ));
    PageFixture {
        app: App::from_flavite(f.flavite),
        cache: f.cache,
        fs: f.fs,
    }
}

fn open_replace(app: &mut App) -> TabId {
    let group = app.focused_group().unwrap();
    let before = app.workspace.group(&group).unwrap().tabs().len();
    let _ = app.update(Message::RunHostCommand("workspace.open_search_view"));
    wait(app, |app| {
        app.workspace
            .group(&group)
            .is_some_and(|group| group.tabs().len() > before)
    });
    app.workspace.group(&group).unwrap().tabs()[before]
}

#[test]
fn vault_source_find_runs_through_the_app_pump() {
    let mut f = fixture();
    let tab = open_replace(&mut f.app);
    let _ = f.app.update(Message::ReplaceEdit {
        tab,
        field: flavite_ui::host::replace::ReplaceField::Query,
        value: "alpha".into(),
    });
    let _ = f.app.update(Message::ReplaceSubmitNow { tab });
    wait(&mut f.app, |app| {
        app.replace
            .tabs
            .get(&tab)
            .is_some_and(|state| !state.updating && state.total_matches.is_some())
    });
    let state = &f.app.replace.tabs[&tab];
    assert_eq!(state.total_matches, Some(3));
    assert_eq!(state.rows.len(), 2, "a.md + b.md rows");
    assert!(
        state
            .exclusions
            .iter()
            .any(|(path, reason)| { path == "notes/dirty.md" && reason == "dirty_buffer" })
    );
}

#[test]
fn preview_seals_and_selection_records_a_revision() {
    let mut f = fixture();
    let tab = open_replace(&mut f.app);
    let _ = f.app.update(Message::ReplaceSetMode {
        tab,
        mode: flavite_ui::host::replace::ReplacePanelMode::Preview,
    });
    let _ = f.app.update(Message::ReplaceEdit {
        tab,
        field: flavite_ui::host::replace::ReplaceField::Query,
        value: "alpha".into(),
    });
    let _ = f.app.update(Message::ReplaceEdit {
        tab,
        field: flavite_ui::host::replace::ReplaceField::Replacement,
        value: "X".into(),
    });
    let _ = f.app.update(Message::ReplaceRunPreview { tab });
    wait(&mut f.app, |app| {
        app.replace
            .tabs
            .get(&tab)
            .is_some_and(|state| !state.updating && state.preview_id.is_some())
    });
    let state = &f.app.replace.tabs[&tab];
    let plan = state.plan.as_ref().expect("plan");
    assert_eq!(plan.matches, 3, "a.md (2) + b.md (1); dirty draft excluded");
    assert_eq!(plan.matched_notes, 2);
    assert_eq!(state.candidates.len(), 2);
    assert!(
        state
            .candidates
            .iter()
            .all(|candidate| !candidate.no_change)
    );
    // Select the candidate by native key.
    let key = state.candidates[0].path_native.clone();
    let _ = f.app.update(Message::ReplaceSelect {
        target: f.app.replace.view_state(tab).preview_target().unwrap(),
        tab,
        selection: flavite_ui::host::replace::SelectAction::Keys(vec![key]),
    });
    wait(&mut f.app, |app| {
        app.replace
            .tabs
            .get(&tab)
            .is_some_and(|state| !state.updating && state.selection_ready)
    });
    let state = &f.app.replace.tabs[&tab];
    assert_eq!(state.plan.as_ref().unwrap().selection_revision, Some(1));
    assert_eq!(state.selected_count(), 1);
}

#[path = "../../flavite-api/tests/support/editor_operations.rs"]
mod editor_operations_fixture;
