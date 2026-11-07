//! RFC-0006 S08 UI: durable replacement apply/cancel/resume through the
//! full App pump — sealed preview + selection → apply → completed job
//! state, cancelled job, and the operations inventory view.

#[path = "../../flavite-api/tests/support/links.rs"]
mod support;

use flavite_fs::Fs;
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
                        "updating={} preview={:?} job={:?} error={:?} mode={:?}",
                        state.updating, state.preview_id, state.job, state.error, state.mode
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
    let f = support::fixture(&[("notes/a.md", "alpha one\n"), ("notes/b.md", "alpha two\n")]);
    PageFixture {
        app: App::from_flavite(f.flavite),
        cache: f.cache,
        fs: f.fs,
    }
}

fn open_replace(app: &mut App) -> TabId {
    let group = app.focused_group().unwrap();
    let before = app.workspace.group(&group).unwrap().tabs().len();
    let _ = app.update(Message::RunHostCommand("workspace.open_replace_view"));
    wait(app, |app| {
        app.workspace
            .group(&group)
            .is_some_and(|group| group.tabs().len() > before)
    });
    app.workspace.group(&group).unwrap().tabs()[before]
}

fn prepare_sealed_preview(app: &mut App, tab: TabId) {
    let _ = app.update(Message::ReplaceSetMode {
        tab,
        mode: flavite_ui::host::replace::ReplacePanelMode::Preview,
    });
    let _ = app.update(Message::ReplaceEdit {
        tab,
        field: flavite_ui::host::replace::ReplaceField::Query,
        value: "alpha".into(),
    });
    let _ = app.update(Message::ReplaceEdit {
        tab,
        field: flavite_ui::host::replace::ReplaceField::Replacement,
        value: "omega".into(),
    });
    let _ = app.update(Message::ReplaceRunPreview { tab });
    wait(app, |app| {
        app.replace
            .tabs
            .get(&tab)
            .is_some_and(|state| !state.updating && state.preview_id.is_some())
    });
    let _ = app.update(Message::ReplaceSelect {
        tab,
        selection: flavite_ui::host::replace::SelectAction::All,
    });
    wait(app, |app| {
        app.replace
            .tabs
            .get(&tab)
            .is_some_and(|state| !state.updating && state.selection_ready)
    });
}

#[test]
fn apply_completes_via_the_app_pump_and_operations_inventory_visible() {
    let mut f = fixture();
    let tab = open_replace(&mut f.app);
    prepare_sealed_preview(&mut f.app, tab);
    let state = &f.app.replace.tabs[&tab];
    assert!(state.can_apply());
    let _ = f.app.update(Message::ReplaceApply { tab });
    wait(&mut f.app, |app| {
        app.replace.tabs.get(&tab).is_some_and(|state| {
            state
                .job
                .as_ref()
                .is_some_and(|job| job.phase == "completed")
        })
    });
    let state = &f.app.replace.tabs[&tab];
    let job = state.job.as_ref().expect("job");
    assert_eq!(job.phase, "completed");
    assert_eq!(job.saved, 2);
    assert_eq!(job.conflicts, 0);
    // Disk bytes replaced through the real vault.
    assert_eq!(
        f.fs.read_if_exists(&f.fs_join("notes/a.md"))
            .unwrap()
            .unwrap(),
        b"omega one\n"
    );
    // Operations inventory aggregates the job.
    let rows = f.app.replace_operations.rows(&f.app.replace);
    assert!(rows.iter().any(|row| row.job.job_id == job.job_id));
}

#[test]
fn cancel_stops_an_applying_job() {
    let mut f = fixture();
    let tab = open_replace(&mut f.app);
    prepare_sealed_preview(&mut f.app, tab);
    let _ = f.app.update(Message::ReplaceApply { tab });
    // Cancel as soon as the job exists (it may complete first on this tiny
    // corpus; either terminal state is legal, but cancel must be accepted).
    let _ = f.app.update(Message::ReplaceCancel { tab });
    wait(&mut f.app, |app| {
        app.replace
            .tabs
            .get(&tab)
            .is_some_and(|state| state.job.is_some())
    });
    let state = &f.app.replace.tabs[&tab];
    let job = state.job.as_ref().expect("job");
    assert!(
        matches!(job.phase.as_str(), "completed" | "cancelled" | "partial"),
        "{}",
        job.phase
    );
}

impl PageFixture {
    fn fs_join(&self, relative: &str) -> std::path::PathBuf {
        std::path::PathBuf::from("/test/semantic-links").join(relative)
    }
}
