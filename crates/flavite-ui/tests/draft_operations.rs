//! RFC-0006 S09 UI: draft opt-in preview, AppliedDraft job state, and the
//! guarded Revert control through the full App pump — a draft candidate is
//! applied as an isolated buffer edit (never a forced save), then reverted
//! as a distinct action epoch.

#[path = "../../flavite-api/tests/support/links.rs"]
mod support;

use flavite_api::{Invocation, Origin};
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

fn api_execute(
    api: &flavite_api::Flavite,
    id: &'static str,
    args: serde_json::Value,
) -> serde_json::Value {
    let request_id = api.command_registry().next_request_id();
    let result = api.execute(Invocation::new(request_id, id, args, Origin::Cli));
    let result = match result {
        flavite_api::ExecutionResult::Pending { request_id } => api.wait_for_operation(request_id),
        other => other,
    };
    match result {
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) => {
            success.data
        }
        other => panic!("api {id} failed: {other:?}"),
    }
}

/// Open the note and make it dirty through the real facade.
fn dirty(api: &flavite_api::Flavite, path: &str, suffix: &str) {
    let opened = api_execute(api, "note.open", serde_json::json!(path));
    let buffer_id = opened["buffer_id"].as_u64().unwrap();
    let revision = opened["revision"].as_u64().unwrap();
    let _ = api_execute(
        api,
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": revision,
            "ops": [ { "range": [5, 5], "replacement": suffix } ],
            "undo_policy": "typing",
        }),
    );
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

fn prepare_draft_preview(app: &mut App, tab: TabId) {
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
    let _ = app.update(Message::ReplaceToggle {
        tab,
        flag: flavite_ui::host::replace::ReplaceFlag::IncludeDrafts,
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
fn draft_apply_and_revert_flow_through_app_pump() {
    let mut f = fixture();
    // Dirty a.md through the real facade (notes/a.md becomes "alpha one more\n").
    let api = f.app.flavite.as_ref().expect("flavite");
    dirty(api, "/test/semantic-links/notes/a.md", " more");
    let tab = open_replace(&mut f.app);
    prepare_draft_preview(&mut f.app, tab);

    let state = &f.app.replace.tabs[&tab];
    assert!(state.include_drafts, "draft opt-in is on");
    assert!(state.can_apply());

    // The candidate row for a.md is typed as a draft.
    assert!(
        state.candidates.iter().any(|candidate| {
            candidate.path == "notes/a.md" && candidate.source_kind == "draft"
        })
    );
    assert!(
        state.candidates.iter().any(|candidate| {
            candidate.path == "notes/b.md" && candidate.source_kind == "saved"
        })
    );

    let _ = f.app.update(Message::ReplaceApply { tab });
    wait(&mut f.app, |app| {
        app.replace.tabs.get(&tab).is_some_and(|state| {
            state
                .job
                .as_ref()
                .is_some_and(|job| job.phase == "completed")
        })
    });
    let job = f.app.replace.tabs[&tab].job.as_ref().expect("job").clone();
    assert_eq!(job.job_id.len(), 32);
    assert_eq!(job.applied_drafts, 1);
    assert_eq!(job.saved, 1);
    // The draft was not force-saved; the saved note was replaced.
    assert_eq!(
        f.fs.read_if_exists(&std::path::PathBuf::from("/test/semantic-links/notes/a.md"))
            .unwrap()
            .unwrap(),
        b"alpha one\n"
    );
    assert_eq!(
        f.fs.read_if_exists(&std::path::PathBuf::from("/test/semantic-links/notes/b.md"))
            .unwrap()
            .unwrap(),
        b"omega two\n"
    );

    // Revert through the app: both item classes restore.
    assert!(f.app.replace.tabs[&tab].can_revert());
    let _ = f.app.update(Message::ReplaceRevert { tab });
    wait(&mut f.app, |app| {
        app.replace.tabs.get(&tab).is_some_and(|state| {
            state.job.as_ref().is_some_and(|job| {
                matches!(job.phase.as_str(), "completed" | "partial")
                    && (job.reverted > 0 || job.reverted_drafts > 0)
            })
        })
    });
    let job = f.app.replace.tabs[&tab].job.as_ref().expect("job").clone();
    assert_eq!(job.phase, "completed");
    assert_eq!(job.reverted, 1);
    assert_eq!(job.reverted_drafts, 1);
    assert_eq!(
        f.fs.read_if_exists(&std::path::PathBuf::from("/test/semantic-links/notes/b.md"))
            .unwrap()
            .unwrap(),
        b"alpha two\n"
    );
    assert_eq!(
        f.fs.read_if_exists(&std::path::PathBuf::from("/test/semantic-links/notes/a.md"))
            .unwrap()
            .unwrap(),
        b"alpha one\n"
    );
}
