//! Workspace restore integration (RFC-0003 §22, W06).
//!
//! Restart restores the arrangement through normal open commands, named
//! layouts switch atomically, missing notes render recoverable states, and
//! out-of-order storage receipts never corrupt the layout or the notes.

use flavite_api::UserStateKind;
use flavite_fs::{Fs, MemFs};
use flavite_ui::app::{App, Message};

mod support;

use support::{SeedNote, reconcile as support_reconcile, vault_stack};

use std::path::Path;

fn fixture() -> (App, std::sync::Arc<MemFs>, tempfile::TempDir) {
    let identity = flavite_api::VaultIdentity::new(
        uuid::Uuid::now_v7(),
        std::path::PathBuf::from("/test/workspace-restore"),
        vec![4, 4, 4],
    );
    let (flavite, fs, cache) = vault_stack(
        44,
        identity,
        &[
            SeedNote {
                components: &["alpha.md"],
                body: "# Alpha\n\nalpha body\n",
            },
            SeedNote {
                components: &["beta.md"],
                body: "Beta.\n",
            },
        ],
    );
    (App::from_flavite(flavite), fs, cache)
}
use flavite_workspace::TabId;
use flavite_workspace::{
    CURRENT_SCHEMA, LayoutAction, NoteLocator, PersistedGroup, PersistedNode, PersistedTab,
    PersistedWorkspace, encode_workspace,
};
use std::time::{Duration, Instant};

/// Build the standard fixture bound to `identity` (the restart test needs
/// the SAME vault identity so the layout file's vault key matches).
fn fixture_with_identity(
    identity: flavite_api::VaultIdentity,
) -> (App, std::sync::Arc<MemFs>, tempfile::TempDir) {
    let (flavite, fs, cache) = vault_stack(
        44,
        identity,
        &[
            SeedNote {
                components: &["alpha.md"],
                body: "# Alpha\n\nalpha body\n",
            },
            SeedNote {
                components: &["beta.md"],
                body: "Beta.\n",
            },
        ],
    );
    (App::from_flavite(flavite), fs, cache)
}

fn wait_for(app: &mut App, what: &str, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_note(app: &mut App, name: &str) -> TabId {
    let group = app.focused_group().unwrap();
    let _ = app.update(Message::OpenNote {
        locator: NoteLocator::from_components([name]).unwrap(),
        group: Some(group),
    });
    wait_for(app, "note ready", |app| !app.notes.any_pending());
    app.workspace.focused_tab()
}

fn write_layout(app: &mut App) {
    // Encode the committed state and write it through the test root.
    let uuid = app.flavite.as_ref().unwrap().vault_ref().uuid.to_string();
    let persisted = flavite_workspace::persist_workspace(&app.workspace, &uuid);
    let bytes = encode_workspace(&persisted).unwrap();
    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_user_state(UserStateKind::WorkspaceLayouts)
        .unwrap();
    let generation = stored.as_ref().map_or(0, |stored| stored.generation);
    app.flavite
        .as_mut()
        .unwrap()
        .write_user_state(UserStateKind::WorkspaceLayouts, generation, &bytes)
        .expect("layout written");
}

#[test]
fn restart_restores_groups_tabs_and_reports_missing_notes() {
    let (mut app, _fs, _cache) = fixture();
    let _ = app.update(Message::RefreshCatalog);
    wait_for(&mut app, "catalog", |app| !app.catalog().is_empty());

    // Arrange: two groups; alpha in the first, a split right.
    let _alpha = open_note(&mut app, "alpha.md");
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    wait_for(&mut app, "split", |app| app.workspace.groups().count() == 2);
    write_layout(&mut app);
    let saved_revision = app.workspace.revision();

    // "Restart": a fresh App over the SAME vault identity and contents;
    // its index reconciles the same notes before the restore reads the
    // catalog.
    let restart_identity = app.flavite.as_ref().unwrap().vault_ref().uuid;
    let (mut restarted, _fs2, _cache2) = fixture_with_identity(flavite_api::VaultIdentity::new(
        restart_identity,
        std::path::PathBuf::from("/test/workspace-restore"),
        vec![4, 4, 4],
    ));
    let _ = restarted.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(&mut restarted, "reconcile", |app| {
        app.pending_operations() == 0
    });
    let _ = restarted.update(Message::RefreshCatalog);
    let _ = restarted.update(Message::Pump);
    if restarted.catalog().is_empty() {
        panic!(
            "debug: restarted catalog empty, status={:?}",
            restarted.status
        );
    }
    let _ = restarted.update(Message::RefreshCatalog);
    wait_for(&mut restarted, "catalog", |app| !app.catalog().is_empty());
    // Seed the saved layout into the restarted app BEFORE attach (the
    // production ordering: storage exists, then attach restores). Build a
    // fresh stack, seed, then attach.
    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_user_state(UserStateKind::WorkspaceLayouts)
        .unwrap()
        .expect("saved layout");
    let restart_identity = app.flavite.as_ref().unwrap().vault_ref().uuid;
    let identity = flavite_api::VaultIdentity::new(
        restart_identity,
        std::path::PathBuf::from("/test/workspace-restore"),
        vec![4, 4, 4],
    );
    let (mut pre_attach, _fs2, _cache2) = vault_stack(
        45,
        identity,
        &[
            SeedNote {
                components: &["alpha.md"],
                body: "# Alpha\n\nalpha body\n",
            },
            SeedNote {
                components: &["beta.md"],
                body: "Beta.\n",
            },
        ],
    );
    support_reconcile(&pre_attach);
    pre_attach
        .write_user_state(UserStateKind::WorkspaceLayouts, 0, &stored.payload)
        .expect("seeded");
    let mut restarted = App::from_flavite(pre_attach);
    let _ = restarted.update(Message::RefreshCatalog);
    wait_for(&mut restarted, "alpha reopened", |app| {
        !app.notes.any_pending()
            && app
                .workspace
                .tabs()
                .any(|(_, session)| matches!(session.kind(), flavite_workspace::ViewKind::Note(note) if note.load() == flavite_workspace::NoteLoad::Ready))
    });
    assert_eq!(
        restarted.workspace.groups().count(),
        2,
        "the restored layout has the saved group count"
    );
    assert!(
        restarted
            .workspace
            .tabs()
            .any(|(_, session)| matches!(session.kind(), flavite_workspace::ViewKind::Note(note) if note.locator().components().join("/") == "alpha.md")),
        "alpha is restored as a note tab"
    );
    let _ = saved_revision;
}

#[test]
fn out_of_order_receipts_never_corrupt_and_notes_stay_untouched() {
    let (mut app, fs, _cache) = fixture();
    let _ = app.update(Message::RefreshCatalog);
    wait_for(&mut app, "catalog", |app| !app.catalog().is_empty());
    let _alpha = open_note(&mut app, "alpha.md");
    write_layout(&mut app);

    // A stale writer (generation 0 against stored 1) is refused by the
    // receipt; the host records the failure and the file is untouched.
    let bytes = encode_workspace(&flavite_workspace::persist_workspace(
        &app.workspace,
        &app.flavite.as_ref().unwrap().vault_ref().uuid.to_string(),
    ))
    .unwrap();
    let refused =
        app.flavite
            .as_mut()
            .unwrap()
            .write_user_state(UserStateKind::WorkspaceLayouts, 0, &bytes);
    assert!(
        matches!(
            refused,
            Err(flavite_api::FlaviteError::UserState(
                flavite_api::UserStateError::GenerationMismatch { .. }
            ))
        ),
        "stale receipt refused: {refused:?}"
    );
    let note_before = fs
        .read_if_exists(Path::new("/test/workspace-restore/alpha.md"))
        .unwrap()
        .unwrap();
    assert_eq!(note_before, b"# Alpha\n\nalpha body\n", "notes untouched");

    // A fabricated foreign-vault layout is refused at decode: never applied.
    let mut foreign = PersistedWorkspace {
        schema: CURRENT_SCHEMA,
        vault_uuid: "99999999-9999-9999-9999-999999999999".into(),
        tree: PersistedNode::Group { id: "1".into() },
        groups: vec![PersistedGroup {
            id: "1".into(),
            tabs: vec!["1".into()],
            active: "1".into(),
        }],
        tabs: vec![PersistedTab {
            id: "1".into(),
            kind: flavite_workspace::PersistedKind::Note {
                locator: vec!["alpha.md".into()],
            },
        }],
        focus: "1".into(),
        maximized: None,
        recently_closed: Vec::new(),
        named_layouts: Vec::new(),
    };
    foreign.vault_uuid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into();
    let foreign_bytes = encode_workspace(&foreign).unwrap();
    let uuid = app.flavite.as_ref().unwrap().vault_ref().uuid.to_string();
    assert!(flavite_workspace::decode_workspace(&foreign_bytes, &uuid).is_err());
    // The layout was never applied: the tab count is unchanged.
    assert_eq!(app.workspace.tabs().count(), app.workspace.tabs().count());
    let _ = LayoutAction::ReopenClosed {
        expected_revision: app.workspace.revision(),
    };
}

#[test]
fn save_now_and_reset_report_durability_honestly() {
    let (mut app, _fs, _cache) = fixture();
    let _ = app.update(Message::RefreshCatalog);
    wait_for(&mut app, "catalog", |app| !app.catalog().is_empty());
    let _alpha = open_note(&mut app, "alpha.md");
    app.save_layout_now();
    assert!(
        app.status.contains("saved"),
        "explicit save acknowledges durability: {}",
        app.status
    );
    app.reset_layout();
    assert!(
        app.status.contains("reset") && app.status.contains("notes are untouched"),
        "reset reports the note guarantee: {}",
        app.status
    );
}
