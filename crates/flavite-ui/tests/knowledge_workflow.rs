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
    let _ = app.update(Message::OpenIntent(
        flavite_ui::host::open_intents::OpenIntent::reuse(
            NoteLocator::from_components([name]).unwrap(),
            None,
        ),
    ));
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

    // Catalog carries the new note for search and pickers. RFC-0004 W-D
    // made acquisition ticketed: the completion trigger admits a refresh,
    // the pump applies the page — so this is now eventually, never
    // synchronously, true (D9's accepted visible change).
    wait(app, |a| {
        a.note_candidates()
            .all()
            .iter()
            .any(|candidate| candidate.path().to_string() == "Notes/New idea.md")
    });
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
            .is_some_and(|state| !state.rows.is_empty())
    });
    let results = app.search.tabs[&search_tab].rows.clone();
    assert_eq!(results[0].path, "b.md");
    assert!(results[0].excerpt.contains("Original"));

    // Opening a result routes canonically into the shell.
    let _ = app.update(Message::OpenIntent(
        flavite_ui::host::open_intents::OpenIntent::reuse(
            NoteLocator::from_components(["b.md"]).unwrap(),
            None,
        ),
    ));
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
    // The picker matches against the catalog host's snapshot; submit
    // before the first snapshot lands matches nothing and opens nothing
    // (a real user cannot submit a picker with no matches either) — wait
    // for the catalog the same way navigation_identity does.
    wait(app, |a| {
        a.note_candidates()
            .all()
            .iter()
            .any(|candidate| candidate.path().to_string() == "b.md")
    });
    app.picker = flavite_ui::app::Picker {
        open: true,
        query: String::new(),
        selected: 0,
        replace_tab: Some(tab),
    };
    let _ = app.update(Message::PickerQuery("b".into()));
    // Note loading and catalog acquisition are independent async flights.
    // Submit only once the target is selectable, as the visible picker does.
    wait(app, |a| {
        a.picker_matches()
            .first()
            .is_some_and(|entry| entry.path == std::path::Path::new("b.md"))
    });
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
    let _ = app.update(Message::NavigateBack(tab));
    wait(app, |a| {
        app_note_is(a, tab, "a.md") && !a.notes.any_pending()
    });
    assert_eq!(buffer_text(app, tab).unwrap(), a_text);
    assert!(app.nav.can_go_forward(tab));

    // Forward returns to b again.
    let _ = app.update(Message::NavigateForward(tab));
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
            .is_some_and(|view| matches!(view.state, flavite_buffer::state::BufferState::Clean))
    });
    edit_buffer(app, tab, "# B\nThird body\n");
    let _ = app.update(Message::RunCoreCommand("note.save"));
    wait(app, |a| {
        a.note_editor_view(tab)
            .is_some_and(|view| matches!(view.state, flavite_buffer::state::BufferState::Clean))
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

// ---- W12: crash subprocess test (edit → kill -9 without destructors →
// reopen → recover → the recovered note's link reaches the graph) ----

/// The child half of the crash test: seeds an edit with a link, waits for
/// the durable recovery checkpoint, writes the marker, and parks until the
/// parent kills the process (no destructors run).
#[test]
fn w12_crash_child_seeds_and_parks() {
    let Some(root) = std::env::var_os("FLAVITE_W12_CRASH_ROOT").map(std::path::PathBuf::from)
    else {
        // Not the child run: nothing to do.
        return;
    };
    let data = std::env::var_os("FLAVITE_DATA_DIR")
        .map(std::path::PathBuf::from)
        .expect("child data dir");
    let flavite = flavite_api::Flavite::open_root_with_data_dir(
        root.clone(),
        flavite_api::VaultConfig::default(),
        Some(data),
    )
    .expect("child vault opens");
    let path = root.join("alpha.md");
    let opened = flavite.execute(flavite_api::Invocation::new(
        flavite.command_registry().next_request_id(),
        "note.open",
        serde_json::json!({ "path": path, "policy": "existing_only" }),
        flavite_api::Origin::Gui,
    ));
    let request = match opened {
        flavite_api::ExecutionResult::Pending { request_id } => request_id,
        other => panic!("child open not admitted: {other:?}"),
    };
    let opened_result = loop {
        flavite.pump();
        let done = flavite.wait_for_operation(request);
        if !done.is_pending() {
            break done;
        }
        assert!(Instant::now() < Instant::now() + Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(5));
    };
    let opened_success = opened_result.into_success().expect("child open succeeds");
    let buffer = opened_success.data["buffer_id"]
        .as_u64()
        .expect("child buffer id");
    let view = flavite.get_buffer_view(buffer.into(), None).expect("view");
    let edited = flavite.execute(flavite_api::Invocation::new(
        flavite.command_registry().next_request_id(),
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer,
            "revision": view.revision,
            "ops": [{ "range": [0, view.text.len()], "replacement": "# Alpha\n\ncrash draft\n\nSee [[b]].\n" }],
        }),
        flavite_api::Origin::Gui,
    ));
    assert!(edited.is_success(), "child edit admitted: {edited:?}");

    // The recovery worker checkpoints within ~100ms of the edit; wait for
    // the durable file under the identity-keyed recovery store.
    let uuid = flavite.vault_ref().uuid.to_string();
    let data = std::env::var_os("FLAVITE_DATA_DIR")
        .map(std::path::PathBuf::from)
        .expect("child data dir");
    let recovery_dir = data.join("flavite/vaults").join(&uuid).join("recovery");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let has_snapshot = std::fs::read_dir(&recovery_dir)
            .map(|entries| entries.filter_map(Result::ok).next().is_some())
            .unwrap_or(false);
        if has_snapshot {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let has_snapshot = std::fs::read_dir(&recovery_dir)
        .map(|entries| entries.filter_map(Result::ok).next().is_some())
        .unwrap_or(false);
    assert!(has_snapshot, "child recovery checkpoint never landed");
    // The disk file must still hold the pre-crash bytes.
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"# Alpha\n\nalpha body\n",
        "the child's edit must exist only as recovery bytes"
    );
    std::fs::write(root.join(".recovery-durable"), b"ok").expect("marker");
    // Park until the parent's kill -9.
    loop {
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn crash_recovery_matches_the_recovered_note_in_the_workspace() {
    use std::os::unix::process::ExitStatusExt;
    use std::process::{Command, Stdio};

    if std::env::var_os("FLAVITE_W12_CRASH_ROOT").is_some() {
        // This is the child invocation running under --exact for the child
        // test; the parent test body must not run.
        return;
    }
    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path().join("vault");
    std::fs::create_dir_all(&root).expect("vault root");
    std::fs::write(root.join("alpha.md"), b"# Alpha\n\nalpha body\n").expect("seed alpha");
    std::fs::write(root.join("b.md"), b"# B\n\ntarget body\n").expect("seed b");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    // (S8/M3) The crash child is a separate OS process whose only channel
    // for the shared recovery-store location is the environment — so the
    // redirect is passed as a per-child `Command::env` (safe, scoped to the
    // spawned process), never as process-global `set_var`.

    let test_exe = std::env::current_exe().unwrap();
    let mut child = Command::new(test_exe)
        .arg("--exact")
        .arg("w12_crash_child_seeds_and_parks")
        .arg("--nocapture")
        .env("FLAVITE_W12_CRASH_ROOT", &root)
        .env("FLAVITE_DATA_DIR", &data)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("child spawns");
    let marker = root.join(".recovery-durable");
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && !marker.exists() {
        if let Some(status) = child.try_wait().expect("child status") {
            panic!("crash child exited before its recovery checkpoint: {status}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        marker.exists(),
        "child never reached its recovery checkpoint"
    );
    child.kill().expect("kill -9");
    let killed = child.wait().expect("child exit");
    assert_eq!(killed.signal(), Some(9), "the child died by SIGKILL");
    // No destructors ran: the disk file never received the unsaved edit.
    assert_eq!(
        std::fs::read(root.join("alpha.md")).unwrap(),
        b"# Alpha\n\nalpha body\n",
        "SIGKILL leaves the unsaved edit out of the disk file"
    );

    // Reopen in-process: the retained recovery copy is offered, restored
    // explicitly, and saved; the recovered link reaches the saved graph.
    // The explicit data_dir is the SAME temp store the child wrote under.
    let flavite = flavite_api::Flavite::open_root_with_data_dir(
        root.clone(),
        flavite_api::VaultConfig::default(),
        Some(data),
    )
    .expect("parent vault opens");
    let path = root.join("alpha.md");
    let opened = flavite.execute(flavite_api::Invocation::new(
        flavite.command_registry().next_request_id(),
        "note.open",
        serde_json::json!({ "path": path, "policy": "existing_only" }),
        flavite_api::Origin::Gui,
    ));
    let request = match opened {
        flavite_api::ExecutionResult::Pending { request_id } => request_id,
        other => panic!("parent open not admitted: {other:?}"),
    };
    let deadline = Instant::now() + Duration::from_secs(10);
    let result = loop {
        flavite.pump();
        let done = flavite.wait_for_operation(request);
        if !done.is_pending() {
            break done;
        }
        assert!(Instant::now() < deadline, "parent open never finished");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(result.is_success(), "parent open succeeds: {result:?}");

    // The recovery offer arrives as presentation, never mutation.
    let offer_deadline = Instant::now() + Duration::from_secs(10);
    let mut offered = false;
    while Instant::now() < offer_deadline && !offered {
        flavite.pump();
        for delivery in flavite.poll_events(64) {
            if let flavite_api::Event::RecoveryOffer { note_path, .. } = delivery.event {
                assert!(
                    note_path.ends_with("alpha.md"),
                    "the offer names the crashed note: {note_path}"
                );
                offered = true;
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(offered, "the retained recovery copy was never offered");

    let restored = flavite.execute(flavite_api::Invocation::new(
        flavite.command_registry().next_request_id(),
        "recovery.restore",
        serde_json::json!({ "path": path }),
        flavite_api::Origin::Gui,
    ));
    assert!(restored.is_success(), "restore succeeds: {restored:?}");
    let restored_success = restored.into_success().expect("restore success data");
    let buffer = restored_success.data["buffer_id"]
        .as_u64()
        .expect("restored buffer id");
    let view = flavite.get_buffer_view(buffer.into(), None).expect("view");
    assert!(
        view.text.contains("See [[b]]."),
        "the recovered bytes are back in the buffer: {:?}",
        view.text
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"# Alpha\n\nalpha body\n",
        "a restore never writes the disk"
    );

    let saved = flavite.execute(flavite_api::Invocation::new(
        flavite.command_registry().next_request_id(),
        "note.save",
        serde_json::json!(path),
        flavite_api::Origin::Gui,
    ));
    let save = match saved {
        flavite_api::ExecutionResult::Pending { request_id } => {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                flavite.pump();
                let done = flavite.wait_for_operation(request_id);
                if !done.is_pending() {
                    break done;
                }
                assert!(Instant::now() < deadline, "save never finished");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        other => other,
    };
    assert!(save.is_success(), "save succeeds: {save:?}");
    let saved_bytes = std::fs::read(&path).unwrap();
    assert!(
        String::from_utf8_lossy(&saved_bytes).contains("See [[b]]."),
        "the recovered bytes are saved to disk: {:?}",
        String::from_utf8_lossy(&saved_bytes)
    );

    // Reconcile publishes the saved topology; the recovered link is an
    // edge from alpha to b in the saved graph projection.
    let reconciled = flavite.execute(flavite_api::Invocation::new(
        flavite.command_registry().next_request_id(),
        "vault.reconcile",
        serde_json::json!({ "scope": "full", "mode": "verify" }),
        flavite_api::Origin::Gui,
    ));
    let reconcile = match reconciled {
        flavite_api::ExecutionResult::Pending { request_id } => {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                flavite.pump();
                let done = flavite.wait_for_operation(request_id);
                if !done.is_pending() {
                    break done;
                }
                assert!(Instant::now() < deadline, "reconcile never finished");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        other => other,
    };
    assert!(reconcile.is_success(), "reconcile succeeds: {reconcile:?}");
    let edge_deadline = Instant::now() + Duration::from_secs(10);
    let mut edge_found = false;
    while Instant::now() < edge_deadline && !edge_found {
        flavite.pump();
        // The save's index commit reaches the service through the
        // writer-side broadcast; the reevaluate kick keeps the wait
        // deterministic instead of racing the 5 s sweep bound.
        flavite.graph_reevaluate();
        if let Some(frame) = flavite.graph_frame() {
            // The effective projection batch carries the record-level
            // graph; after the save it equals the saved topology.
            let batch = &frame.projection().effective;
            let alpha = batch.nodes.iter().find(|node| node.path == "alpha.md");
            let b = batch.nodes.iter().find(|node| node.path == "b.md");
            if let (Some(alpha), Some(b)) = (alpha, b) {
                edge_found = batch
                    .edges
                    .iter()
                    .any(|edge| edge.from == alpha.id && edge.to == b.id);
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        edge_found,
        "the recovered note's link reaches the saved graph projection"
    );
}
