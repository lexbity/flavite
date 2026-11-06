//! Final-view close safety (RFC-0003 §28, W12): lease accounting, explicit
//! decisions (save/retain/discard), revision-bound discard, group close
//! queues, decision survival across layout moves, and the never-lose-work
//! rule when a save fails (AC-W13).

use flavite_api::{BufferStatus, VaultConfig};
use flavite_buffer::manager::BufferManager;
use flavite_editor::EditorAction;
use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_ui::app::{App, Message};
use flavite_ui::host::closing::{CloseDecision, CloseOrigin};
use flavite_ui::host::dialogs::Dialog;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use flavite_workspace::{LayoutAction, NoteLocator, TabDropTarget, TabId};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

mod support;

use support::SharedFs;

/// The recovery snapshot store lives in the OS data directory (process
/// global), so the tests here serialize and redirect it to a per-fixture
/// tempdir while any session that captured the directory is alive.
static DATA_DIR_LOCK: Mutex<()> = Mutex::new(());

const ALPHA_ORIGINAL: &[u8] = b"# Alpha\n\nalpha body\n";
const BETA_ORIGINAL: &[u8] = b"# Beta\n\nbeta body\n";

struct Fixture {
    app: App,
    fs: Arc<MemFs>,
    /// Keeps the redirected OS data directory alive for the session.
    #[allow(dead_code)]
    data: tempfile::TempDir,
    #[allow(dead_code)]
    cache: tempfile::TempDir,
    /// Held for the whole fixture lifetime: guards the process-global
    /// explicit data-dir redirection.
    _guard: MutexGuard<'static, ()>,
}

fn fixture() -> Fixture {
    // SAFETY: every test holds `DATA_DIR_LOCK` from before this call until
    // the fixture (and the session that captured the directory) drops, so
    // no concurrent reader or writer of the variable exists.
    // (S8/M3) The recovery store is now rooted by the explicit
    // `with_data_dir` builder argument below, not by process-global
    // environment state; the lock exists to keep the redirected-tempdir
    // pattern of this file honest if any ambient default is ever consulted.
    let guard = DATA_DIR_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let data = tempfile::tempdir().expect("data tempdir");
    let fs = Arc::new(MemFs::new());
    let root = PathBuf::from("/test/close-safety");
    fs.create_dir_all(&root).expect("vault root");
    fs.write_new(&root.join("alpha.md"), ALPHA_ORIGINAL)
        .expect("alpha");
    fs.write_new(&root.join("beta.md"), BETA_ORIGINAL)
        .expect("beta");
    let cache = tempfile::tempdir().expect("cache tempdir");
    let db = cache.path().join("close-safety-index.sqlite3");
    drop(flavite_index::open_or_init(&db).expect("index"));
    let identity =
        flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![12, 7, 3]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(SharedFs(fs.clone())) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).expect("index actor"))
        .build()
        .expect("actor")
        .start()
        .expect("actor start");
    let channels = ChannelSet::new();
    // Close-safety scenarios require the close decisions to be the only
    // thing that can move bytes: autosave must never race the dialog.
    let config = VaultConfig {
        autosave_idle_interval: Duration::from_secs(3600),
        ..VaultConfig::default()
    };
    let session = Session::new_with_data_dir(
        identity.clone(),
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        config.clone(),
        Some(data.path().to_path_buf()),
    );
    let flavite = flavite_api::Flavite::open(identity, session, config).expect("facade");
    Fixture {
        app: App::from_flavite(flavite),
        fs,
        data,
        cache,
        _guard: guard,
    }
}

fn wait_ready(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = app.update(Message::Pump);
        let pending = app.notes.any_pending();
        if !pending || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!app.notes.any_pending(), "every open completed");
}

fn pump_until(app: &mut App, what: &str, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_note(app: &mut App, name: &str) -> TabId {
    let group = app.focused_group().expect("a focused group");
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator: NoteLocator::from_components([name]).expect("locator"),
        group,
    });
    wait_ready(app);
    let tab = app.workspace.focused_tab();
    assert!(
        app.notes
            .state(&tab)
            .is_some_and(|state| state.buffer.is_some()),
        "{name} opened with a buffer"
    );
    tab
}

/// Append `suffix` through the acknowledged editor action path, making the
/// shared Session buffer Dirty.
fn edit_note(app: &mut App, tab: TabId, suffix: &str) {
    let incarnation = app.notes.incarnation(tab).expect("incarnation");
    let view = app.note_editor_view(tab).expect("editor view");
    let position = view.text.len();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::replace(&view, position, position, suffix.to_owned()),
    });
    assert_eq!(
        app.note_buffer_state(tab),
        Some(BufferStatus::Dirty),
        "the edit made the buffer dirty"
    );
}

fn run_host(app: &mut App, id: &'static str) {
    let _ = app.update(Message::RunHostCommand(id));
}

fn disk_bytes(fs: &MemFs, app: &App, name: &str) -> Vec<u8> {
    fs.read_if_exists(&app.root().join(name))
        .expect("disk read")
        .unwrap_or_default()
}

#[test]
fn two_view_close_releases_only_the_lease() {
    let Fixture { mut app, .. } = fixture();
    let (alpha, alpha_buffer) = {
        let tab = open_note(&mut app, "alpha.md");
        let buffer = app.notes.state(&tab).unwrap().buffer.unwrap();
        (tab, buffer)
    };
    run_host(&mut app, "workspace.split_right");
    wait_ready(&mut app);
    let duplicate = app.workspace.focused_tab();
    assert_ne!(duplicate, alpha, "the split mints a fresh tab identity");
    assert_eq!(
        app.notes.state(&duplicate).unwrap().buffer,
        Some(alpha_buffer),
        "the duplicate shares the Session buffer"
    );

    let _ = app.update(Message::CloseTab(duplicate));
    assert!(
        app.workspace.tab(&duplicate).is_none(),
        "a shared view closes immediately: no decision is required"
    );
    assert!(app.dialog.is_none(), "no close dialog for a shared view");
    assert!(app.closing.is_none(), "no close plan for a shared view");

    // The sibling view keeps its buffer identity; the shared buffer stays.
    assert_eq!(
        app.notes.state(&alpha).unwrap().buffer,
        Some(alpha_buffer),
        "the sibling keeps its buffer binding"
    );
    assert_eq!(
        app.note_state_text(alpha).as_deref(),
        Some(std::str::from_utf8(ALPHA_ORIGINAL).unwrap()),
        "the shared buffer still serves its text"
    );
}

#[test]
fn final_dirty_close_requires_a_decision_and_save_close_persists() {
    let Fixture { mut app, fs, .. } = fixture();
    let tab = open_note(&mut app, "alpha.md");
    let tree_before = app.workspace.clone();
    edit_note(&mut app, tab, " MORE");

    let _ = app.update(Message::CloseTab(tab));
    assert!(
        app.workspace.tab(&tab).is_some(),
        "the tab stays open until a decision is made"
    );
    assert!(
        matches!(app.dialog, Some(Dialog::Close { .. })),
        "the close decision dialog is open: {:?}",
        app.dialog
    );
    assert!(app.closing.is_some(), "a close plan is active");
    assert_eq!(
        app.workspace, tree_before,
        "the workspace tree is unchanged while the dialog is open"
    );

    let _ = app.update(Message::CloseDecision(CloseDecision::SaveClose));
    pump_until(&mut app, "save-then-close to finish", |app| {
        app.workspace.tab(&tab).is_none()
    });
    assert_eq!(
        disk_bytes(&fs, &app, "alpha.md"),
        b"# Alpha\n\nalpha body\n MORE",
        "the edit reached MemFs disk through the save"
    );
    assert!(
        app.notes.state(&tab).is_none(),
        "the document entry is fully closed"
    );
    assert!(app.closing.is_none() && app.dialog.is_none());
}

#[test]
fn retain_recovery_closes_and_retains_a_durable_copy() {
    let Fixture { mut app, fs, .. } = fixture();
    let tab = open_note(&mut app, "alpha.md");
    edit_note(&mut app, tab, " UNSAVED");

    let _ = app.update(Message::CloseTab(tab));
    assert!(matches!(app.dialog, Some(Dialog::Close { .. })));
    let _ = app.update(Message::CloseDecision(CloseDecision::RetainRecovery));
    pump_until(&mut app, "retain-recovery close to finish", |app| {
        app.workspace.tab(&tab).is_none()
    });
    assert!(app.notes.state(&tab).is_none());
    assert_eq!(
        disk_bytes(&fs, &app, "alpha.md"),
        ALPHA_ORIGINAL,
        "disk never received the unsaved edits"
    );

    // Reopening offers the retained copy: the durable recovery snapshot
    // differs from disk, and the core surfaces it as a RecoveryOffer.
    run_host(&mut app, "workspace.reopen_tab");
    wait_ready(&mut app);
    pump_until(&mut app, "the recovery offer dialog", |app| {
        matches!(app.dialog, Some(Dialog::RecoveryOffer { .. }))
    });
    match &app.dialog {
        Some(Dialog::RecoveryOffer { path }) => {
            assert!(
                path.ends_with("alpha.md"),
                "the offer names the reopened note: {path}"
            );
        }
        other => panic!("expected a recovery offer, got {other:?}"),
    }
}

#[test]
fn discard_is_revision_bound_and_refuses_after_a_later_edit() {
    let Fixture { mut app, fs, .. } = fixture();
    let tab = open_note(&mut app, "alpha.md");
    edit_note(&mut app, tab, " FIRST");

    let _ = app.update(Message::CloseTab(tab));
    let _ = app.update(Message::CloseDiscardArm(true));
    let armed = app
        .closing
        .as_ref()
        .and_then(|plan| plan.discard_armed)
        .expect("the discard confirmation armed with a revision");

    // A later edit changes the revision; the armed confirmation is stale.
    edit_note(&mut app, tab, " SECOND");
    let current = app.note_editor_view(tab).unwrap().revision;
    assert_ne!(armed, current, "the edit moved the revision past the arm");
    let tree_before = app.workspace.clone();

    let _ = app.update(Message::CloseDiscardConfirm);
    assert!(
        app.workspace.tab(&tab).is_some(),
        "a stale discard confirmation must not close the tab"
    );
    assert_eq!(app.workspace, tree_before, "no layout change happened");
    let status = app.status_text().to_owned();
    assert!(
        status.contains("changed") && status.contains("choose again"),
        "the status explains the refusal: {status}"
    );
    assert!(
        matches!(app.dialog, Some(Dialog::Close { .. })),
        "the decision dialog stays open"
    );
    assert_eq!(
        disk_bytes(&fs, &app, "alpha.md"),
        ALPHA_ORIGINAL,
        "disk is untouched by the refused discard"
    );

    // Re-arm against the revision the user actually sees, then confirm.
    let _ = app.update(Message::CloseDiscardArm(true));
    let _ = app.update(Message::CloseDiscardConfirm);
    pump_until(&mut app, "the confirmed discard to close", |app| {
        app.workspace.tab(&tab).is_none()
    });
    assert_eq!(
        disk_bytes(&fs, &app, "alpha.md"),
        ALPHA_ORIGINAL,
        "the discarded edits never reached disk"
    );
    assert!(app.notes.state(&tab).is_none());

    // The discarded bytes are retrievable through the local history API.
    let flavite = app.flavite.as_ref().expect("facade");
    let request_id = flavite.command_registry().next_request_id();
    let invocation = flavite_api::Invocation::new(
        request_id,
        "note.history",
        serde_json::json!({ "path": app.root().join("alpha.md"), "limit": 100 }),
        flavite_api::Origin::Gui,
    );
    match flavite.execute(invocation) {
        flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) => {
            let entries = success
                .data
                .get("entries")
                .and_then(serde_json::Value::as_array)
                .cloned()
                .unwrap_or_default();
            assert!(
                entries.iter().any(|entry| {
                    entry.get("reason").and_then(serde_json::Value::as_str)
                        == Some("conflict_resolution")
                }),
                "the discarded edits were snapshotted into history: {entries:?}"
            );
        }
        other => panic!("note.history did not succeed: {other:?}"),
    }
}

#[test]
fn close_cancellation_keeps_the_workspace() {
    let Fixture { mut app, .. } = fixture();
    let tab = open_note(&mut app, "alpha.md");
    edit_note(&mut app, tab, " KEEP");
    let revision_before = app.workspace.revision();
    let tree_before = app.workspace.clone();

    let _ = app.update(Message::CloseTab(tab));
    assert!(matches!(app.dialog, Some(Dialog::Close { .. })));
    let _ = app.update(Message::CloseKeepOpen);
    assert!(
        app.workspace.tab(&tab).is_some(),
        "keep-open leaves the tab open"
    );
    assert!(app.dialog.is_none(), "the decision dialog closed");
    assert!(app.closing.is_none(), "the close plan was cancelled");
    assert_eq!(
        app.note_buffer_state(tab),
        Some(BufferStatus::Dirty),
        "the buffer state is unchanged"
    );
    assert_eq!(
        app.workspace, tree_before,
        "the workspace tree is unchanged"
    );
    assert_eq!(
        app.workspace.revision(),
        revision_before,
        "cancelling a close never mutates the layout"
    );
}

#[test]
fn group_close_closes_clean_tabs_and_queues_dirty_decisions() {
    let Fixture { mut app, fs, .. } = fixture();
    let group = app.focused_group().expect("the boot group");
    let clean = open_note(&mut app, "alpha.md");
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator: NoteLocator::from_components(["beta.md"]).expect("locator"),
        group,
    });
    wait_ready(&mut app);
    let dirty = app.workspace.focused_tab();
    assert_ne!(clean, dirty);
    edit_note(&mut app, dirty, " EXTRA");

    let _ = app.update(Message::RequestCloseGroup(group));
    assert!(
        app.workspace.tab(&clean).is_none(),
        "the clean tab closed immediately"
    );
    assert!(
        app.workspace.tab(&dirty).is_some(),
        "the dirty tab waits for a decision"
    );
    assert!(matches!(app.dialog, Some(Dialog::Close { tab }) if tab == dirty));
    let plan = app.closing.as_ref().expect("a group close plan");
    assert_eq!(plan.tab, dirty);
    assert_eq!(
        plan.origin,
        CloseOrigin::Group,
        "the plan records the group origin"
    );
    assert!(
        app.close_queue.is_empty(),
        "the dirty tab was the only decision left"
    );

    let _ = app.update(Message::CloseDecision(CloseDecision::SaveClose));
    pump_until(&mut app, "the group close to finish", |app| {
        app.workspace.tab(&dirty).is_none()
    });
    assert!(app.workspace.group(&group).is_none(), "the group collapsed");
    // The workspace model re-seeds a boot Empty tab when its last group
    // empties; no note tab and no other group may survive.
    let leftover_tabs: Vec<_> = app.workspace.tabs().map(|(tab, _)| tab).collect();
    assert_eq!(leftover_tabs.len(), 1, "only the boot tab remains");
    assert_eq!(
        app.workspace
            .tab(leftover_tabs[0])
            .map(|view| view.kind().clone()),
        Some(flavite_workspace::ViewKind::Empty),
        "the remaining tab is the boot Empty tab"
    );
    assert!(app.notes.state(&dirty).is_none());
    assert_eq!(
        disk_bytes(&fs, &app, "beta.md"),
        b"# Beta\n\nbeta body\n EXTRA",
        "the dirty note was saved before its close"
    );
}

#[test]
fn layout_switch_with_retained_dirty_document() {
    let Fixture { mut app, fs, .. } = fixture();
    // Group A gets alpha (edited) and beta; the split mints group B holding
    // a beta duplicate, so the move below has a real destination group.
    let group_a = app.focused_group().expect("the boot group");
    let alpha = open_note(&mut app, "alpha.md");
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator: NoteLocator::from_components(["beta.md"]).expect("locator"),
        group: group_a,
    });
    wait_ready(&mut app);
    run_host(&mut app, "workspace.split_right");
    wait_ready(&mut app);
    let group_b = app
        .workspace
        .group_order()
        .into_iter()
        .find(|group| *group != group_a)
        .expect("the split group");

    // Move alpha into group B, activate it there, and make it dirty: it is
    // the final view of its document.
    let _ = app.update(Message::Layout(LayoutAction::MoveTab {
        expected_revision: app.workspace.revision(),
        tab: alpha,
        target: TabDropTarget::Index {
            group: group_b,
            index: 0,
        },
    }));
    assert_eq!(app.workspace.group_of(alpha), Some(group_b));
    let _ = app.update(Message::ActivateTab(alpha));
    edit_note(&mut app, alpha, " MOVED");

    let _ = app.update(Message::CloseTab(alpha));
    assert!(matches!(app.dialog, Some(Dialog::Close { tab }) if tab == alpha));

    // The layout changes underneath the open decision: the plan survives,
    // still keyed by the moved tab identity (FR-15).
    let _ = app.update(Message::Layout(LayoutAction::MoveTab {
        expected_revision: app.workspace.revision(),
        tab: alpha,
        target: TabDropTarget::Index {
            group: group_a,
            index: 0,
        },
    }));
    assert_eq!(app.workspace.group_of(alpha), Some(group_a));
    let plan = app.closing.as_ref().expect("the plan survived the move");
    assert_eq!(plan.tab, alpha, "the plan still names the moved tab");
    assert!(matches!(app.dialog, Some(Dialog::Close { .. })));

    let _ = app.update(Message::CloseDecision(CloseDecision::SaveClose));
    pump_until(&mut app, "the moved tab to close", |app| {
        app.workspace.tab(&alpha).is_none()
    });
    assert!(
        app.notes.state(&alpha).is_none(),
        "the moved tab's document is closed"
    );
    assert_eq!(
        disk_bytes(&fs, &app, "alpha.md"),
        b"# Alpha\n\nalpha body\n MOVED",
        "the edit was saved before the moved tab closed"
    );
    assert!(
        app.workspace.group(&group_b).is_some(),
        "the unrelated split group survives"
    );
}

#[test]
fn failed_save_never_closes_the_final_view() {
    let Fixture { mut app, fs, .. } = fixture();
    let tab = open_note(&mut app, "alpha.md");
    edit_note(&mut app, tab, " MINE");

    let _ = app.update(Message::CloseTab(tab));
    assert!(matches!(app.dialog, Some(Dialog::Close { .. })));

    // The file changes on disk behind the buffer's back: the save's
    // precondition will fail its compare stage.
    let path = app.root().join("alpha.md");
    fs.remove(&path).expect("remove");
    fs.write_new(&path, b"external overwrite")
        .expect("overwrite");
    let _ = app.update(Message::Pump);

    let _ = app.update(Message::CloseDecision(CloseDecision::SaveClose));
    pump_until(&mut app, "the conflicted save to settle", |app| {
        app.pending_operations() == 0
    });
    assert_eq!(
        disk_bytes(&fs, &app, "alpha.md"),
        b"external overwrite",
        "the failing save never wrote the buffer bytes"
    );
    assert!(
        app.workspace.tab(&tab).is_some(),
        "AC-W13: a failed save never removes the final view"
    );
    assert!(
        app.notes.state(&tab).is_some(),
        "the document stays bound to the open view"
    );
    assert!(
        app.closing.is_some(),
        "the close plan is retained for another decision"
    );
    assert!(
        app.status_text().contains("Close cancelled"),
        "the status reports the typed failure: {}",
        app.status_text()
    );
    assert_eq!(
        app.note_buffer_state(tab),
        Some(BufferStatus::Conflicted),
        "the buffer reports the disk conflict"
    );
}
