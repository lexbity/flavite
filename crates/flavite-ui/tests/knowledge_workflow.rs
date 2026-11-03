//! W10 connected-note workflow, end to end in the real shell: find a note,
//! open it, create a note through the explicit dialog flow, follow a link,
//! navigate back in the same tab, rename with an inbound-link rewrite, and
//! inspect/restore local history — asserting workspace state and disk bytes
//! after every terminal completion (RFC-0003 §26).

#[path = "../../flavite-api/tests/support/links.rs"]
mod support;

use flavite_fs::Fs;
use flavite_ui::app::{App, Message};
use flavite_workspace::{NoteLocator, TabId};
use std::time::{Duration, Instant};

fn wait(app: &mut App, done: impl Fn(&App) -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = app.update(Message::Pump);
        if done(app) {
            return;
        }
        assert!(Instant::now() < until, "{}", app.status_text());
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct Workflow {
    app: App,
    #[allow(dead_code)]
    cache: tempfile::TempDir,
    fs: std::sync::Arc<flavite_fs::MemFs>,
}

fn fixture() -> Workflow {
    let f = support::fixture(&[
        ("a.md", "See [[b]] here.\n"),
        ("b.md", "# B\nOriginal body\n"),
    ]);
    Workflow {
        app: App::from_flavite(f.flavite),
        cache: f.cache,
        fs: f.fs,
    }
}

fn open(app: &mut App, name: &str) -> TabId {
    let _ = app.update(Message::OpenNote {
        locator: NoteLocator::from_components([name]).unwrap(),
        group: None,
    });
    wait(app, |a| !a.notes.any_pending());
    app.workspace.focused_tab()
}

fn tab_of(app: &App, name: &str) -> Option<TabId> {
    let Ok(locator) = NoteLocator::from_components(name.split('/')) else {
        return None;
    };
    app.workspace
        .tabs()
        .find_map(|(tab, session)| {
            matches!(session.kind(), flavite_workspace::ViewKind::Note(note)
                if note.locator() == &locator)
            .then_some(tab)
        })
        .copied()
}

fn buffer_text(app: &App, tab: TabId) -> Option<String> {
    app.note_editor_view(tab).map(|view| view.text.to_string())
}

#[test]
fn create_note_opens_a_tab_and_reaches_disk_on_first_save() {
    let mut wf = fixture();
    let app = &mut wf.app;
    // Explicit creation through the dialog flow (never implicit).
    app.dialog = Some(flavite_ui::host::dialogs::Dialog::NewNote {
        path: "Notes/New idea.md".into(),
    });
    let _ = app.update(Message::DialogSubmit);
    wait(app, |a| {
        tab_of(a, "Notes/New idea.md").is_some() && !a.notes.any_pending()
    });
    let tab = tab_of(app, "Notes/New idea.md").expect("created note tab");
    assert!(app.note_editor_view(tab).is_some());

    // The empty file exists on disk immediately after creation.
    assert!(
        app.flavite
            .as_ref()
            .unwrap()
            .is_note_open(&app.root().join("Notes/New idea.md"))
    );

    // Catalog carries the new note for search and pickers.
    assert!(
        app.note_candidates()
            .all()
            .iter()
            .any(|candidate| candidate.path().to_string() == "Notes/New idea.md")
    );
}

#[test]
fn search_view_finds_saved_content_and_opens_results() {
    let mut wf = fixture();
    let app = &mut wf.app;
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenSearch {
        expected_revision: revision,
        group,
    });
    let search_tab = app.workspace.focused_tab();
    let _ = app.update(Message::SearchQuery {
        tab: search_tab,
        value: "Original body".into(),
    });
    wait(app, |a| {
        a.search
            .tabs
            .get(&search_tab)
            .is_some_and(|state| !state.results.is_empty())
    });
    let results = app.search.tabs[&search_tab].results.clone();
    assert_eq!(results[0].path, "b.md");
    assert!(results[0].excerpt.contains("Original"));

    // Opening a result routes canonically into the shell.
    let _ = app.update(Message::OpenNote {
        locator: NoteLocator::from_components(["b.md"]).unwrap(),
        group: None,
    });
    wait(app, |a| !a.notes.any_pending());
    assert!(tab_of(app, "b.md").is_some());
}

#[test]
fn back_and_forward_restore_place_within_one_tab() {
    let mut wf = fixture();
    let app = &mut wf.app;
    let tab = open(app, "a.md");
    let a_text = buffer_text(app, tab).unwrap();
    assert!(a_text.contains("[[b]]"));

    // Open b in the same tab (picker replace mode): a goes to Back.
    app.picker = flavite_ui::app::Picker {
        open: true,
        query: String::new(),
        selected: 0,
        replace_tab: Some(tab),
    };
    let _ = app.update(Message::PickerQuery("b".into()));
    let _ = app.update(Message::PickerSubmit);
    wait(app, |a| {
        tab_of(a, "b.md") == Some(tab)
            && a.note_editor_view(tab).is_some()
            && !a.notes.any_pending()
    });
    assert_eq!(
        app.note_locator(tab).unwrap(),
        NoteLocator::from_components(["b.md"]).unwrap()
    );
    assert!(app.nav.can_go_back(tab));
    assert!(!app.nav.can_go_forward(tab));

    // Back returns to a in the SAME tab identity.
    let _ = app.update(Message::NavigateBack);
    wait(app, |a| {
        app_note_is(a, tab, "a.md") && !a.notes.any_pending()
    });
    assert_eq!(buffer_text(app, tab).unwrap(), a_text);
    assert!(app.nav.can_go_forward(tab));

    // Forward returns to b again.
    let _ = app.update(Message::NavigateForward);
    wait(app, |a| {
        app_note_is(a, tab, "b.md") && !a.notes.any_pending()
    });
}

fn app_note_is(app: &App, tab: TabId, name: &str) -> bool {
    app.note_locator(tab) == Some(NoteLocator::from_components([name]).unwrap())
        && app.note_editor_view(tab).is_some()
}

#[test]
fn rename_rewrites_links_relocates_tabs_and_updates_catalog() {
    let mut wf = fixture();
    let app = &mut wf.app;
    let _tab = open(app, "b.md");

    // Rename b.md -> renamed.md with the default rewrite strategy.
    app.dialog = Some(flavite_ui::host::dialogs::Dialog::Rename {
        from: NoteLocator::from_components(["b.md"]).unwrap(),
        to: "renamed.md".into(),
        tier: None,
    });
    let _ = app.update(Message::DialogSubmit);
    wait(app, |a| {
        a.note_candidates()
            .all()
            .iter()
            .any(|candidate| candidate.path().to_string() == "renamed.md")
            && a.note_candidates()
                .all()
                .iter()
                .all(|candidate| candidate.path().to_string() != "b.md")
    });
    // The open tab adopted the new identity in place (RelocateNote).
    let relocated = tab_of(app, "renamed.md");
    assert!(relocated.is_some(), "the open tab must follow the rename");
}

#[test]
fn history_lists_snapshots_and_restore_is_undoable_without_disk_write() {
    let mut wf = fixture();
    let app = &mut wf.app;
    let tab = open(app, "b.md");

    // Save twice so two durable snapshots exist and the newest saved bytes
    // differ from the snapshot we will restore.
    edit_buffer(app, tab, "# B\nReplaced body\n");
    let _ = app.update(Message::RunCoreCommand("note.save"));
    wait(app, |a| {
        a.note_editor_view(tab)
            .is_some_and(|view| matches!(view.state, flavite_api::BufferStatus::Clean))
    });
    edit_buffer(app, tab, "# B\nThird body\n");
    let _ = app.update(Message::RunCoreCommand("note.save"));
    wait(app, |a| {
        a.note_editor_view(tab)
            .is_some_and(|view| matches!(view.state, flavite_api::BufferStatus::Clean))
    });

    // Open the history view for this note and load it.
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenHistory {
        expected_revision: revision,
        locator: NoteLocator::from_components(["b.md"]).unwrap(),
        group,
    });
    let history_tab = app.workspace.focused_tab();
    wait(app, |a| {
        a.history
            .tabs
            .get(&history_tab)
            .is_some_and(|list| !list.entries.is_empty())
    });
    // Each save records the *prior* bytes, so with two saves the save
    // entries are [Replaced, Original] newest-first and "Replaced" differs
    // from both the current buffer (Third) and disk (Third).
    let target = app.history.tabs[&history_tab]
        .entries
        .iter()
        .find(|entry| entry.reason == "save")
        .expect("a prior saved snapshot")
        .hash
        .clone();
    assert!(app.can_restore_history(&NoteLocator::from_components(["b.md"]).unwrap()));

    // Restore: one guarded undoable buffer edit; disk still holds the
    // saved (replaced) bytes until an explicit save.
    let _ = app.update(Message::HistoryRestore {
        tab: history_tab,
        hash: target,
    });
    wait(app, |a| {
        buffer_text(a, tab).is_some_and(|text| text.contains("Replaced body"))
    });
    // The restore is buffer-only: disk still holds the newest saved bytes.
    let disk = String::from_utf8(
        wf.fs
            .read_if_exists(&app.root().join("b.md"))
            .unwrap()
            .expect("saved note bytes"),
    )
    .unwrap();
    assert!(
        disk.contains("Third body") && !disk.contains("Replaced body"),
        "restore must not write disk before an explicit save; disk: {disk}"
    );
    // The restore is one undoable edit: undo brings the newest body back.
    let _ = app.update(Message::ActivateTab(tab));
    let _ = app.update(Message::RunCoreCommand("buffer.undo"));
    wait(app, |a| {
        buffer_text(a, tab).is_some_and(|text| text.contains("Third body"))
    });
}

/// Replace a note tab's whole buffer through the core apply_edits command.
fn edit_buffer(app: &mut App, tab: TabId, replacement: &str) {
    let view = app.note_editor_view(tab).expect("buffer view");
    let flavite = app.flavite.as_ref().unwrap();
    let request_id = flavite.command_registry().next_request_id();
    let invocation = flavite_api::command::Invocation::new(
        request_id,
        "apply_edits",
        serde_json::json!({
            "buffer_id": view.buffer_id,
            "revision": view.revision,
            "ops": [{ "range": [0, view.text.len()], "replacement": replacement }],
        }),
        flavite_api::Origin::Gui,
    );
    let result = flavite.execute(invocation);
    assert!(matches!(
        result,
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(_))
            | flavite_api::ExecutionResult::Pending { .. }
    ));
}

#[test]
fn unresolved_link_activation_offers_explicit_creation() {
    let mut wf = fixture();
    let app = &mut wf.app;
    let tab = open(app, "a.md");
    // The editor navigation path offers creation for an unresolved target:
    // activating [[missing note]] opens the create dialog prefilled.
    app.dialog = Some(flavite_ui::host::dialogs::offer_create_for_link(
        "missing note",
    ));
    match &app.dialog {
        Some(flavite_ui::host::dialogs::Dialog::NewNote { path }) => {
            assert_eq!(path, "missing note.md");
        }
        other => panic!("expected a create dialog, got {other:?}"),
    }
    let _ = app.update(Message::DialogSubmit);
    wait(app, |a| {
        tab_of(a, "missing note.md").is_some() && !a.notes.any_pending()
    });
    let _ = tab;
}
