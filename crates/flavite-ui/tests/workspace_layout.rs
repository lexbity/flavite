//! Real-shell layout integration (RFC-0003 AC-W02, layout portion).
//!
//! Opens two real fixture notes, splits and merges through the host
//! command path, moves a tab between groups, and asserts identity, text,
//! adapter rectangles, and semantic ids through every step.

use flavite_api::{Flavite, VaultConfig};
use flavite_buffer::manager::BufferManager;
use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_ui::app::{App, Message};
use flavite_ui::host::commands as host_commands;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use flavite_workspace::{GroupId, LayoutAction, NoteLoad, TabId, ViewKind};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

struct Fixture {
    app: App,
    #[allow(dead_code)]
    cache: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/workspace-layout");
    fs.create_dir_all(&root).unwrap();
    fs.write_new(&root.join("alpha.md"), b"# Alpha\n\nalpha body\n")
        .unwrap();
    fs.write_new(&root.join("beta.md"), b"# Beta\n\nbeta body\n")
        .unwrap();
    let cache = tempfile::tempdir().unwrap();
    let db = cache.path().join("layout-index.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let identity =
        flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![5, 5, 5]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&db, 1).unwrap())
        .build()
        .unwrap()
        .start()
        .unwrap();
    let channels = ChannelSet::new();
    let session = Session::new(
        identity.clone(),
        actor,
        BufferManager::new(),
        channels.pulse,
        channels.notice,
        VaultConfig::default(),
    );
    let flavite = Flavite::open(identity, session, VaultConfig::default()).unwrap();
    Fixture {
        app: App::from_flavite(flavite),
        cache,
    }
}

fn open_note(app: &mut App, name: &str) -> (TabId, u64) {
    let group = app.focused_group().unwrap();
    let locator = flavite_workspace::NoteLocator::from_components([name]).unwrap();
    let _ = app.update(Message::OpenNote {
        locator,
        group: Some(group),
    });
    wait_ready(app);
    let tab = app.workspace.focused_tab();
    let session = app.workspace.tab(&tab).unwrap();
    let ViewKind::Note(note) = session.kind() else {
        panic!("a note tab opened");
    };
    assert_eq!(note.load(), NoteLoad::Ready, "{name} is ready");
    let buffer = app
        .notes
        .state(&tab)
        .and_then(|state| state.buffer)
        .expect("the buffer binding exists");
    (tab, buffer)
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

fn run_host(app: &mut App, id: &'static str) {
    let _ = app.update(Message::RunHostCommand(id));
}

#[test]
fn split_move_merge_preserve_identity_text_and_semantics() {
    let Fixture { mut app, .. } = fixture();
    assert_eq!(app.workspace.groups().count(), 1, "one group at boot");
    assert_eq!(app.workspace.tabs().count(), 1, "one real Empty tab");

    let (alpha, alpha_buffer) = open_note(&mut app, "alpha.md");
    let (beta, beta_buffer) = open_note(&mut app, "beta.md");
    assert_ne!(alpha, beta, "separate tabs");
    assert_ne!(alpha_buffer, beta_buffer, "separate buffers");
    let buffers_before: BTreeMap<TabId, u64> = [(alpha, alpha_buffer), (beta, beta_buffer)]
        .into_iter()
        .collect();

    // Keyboard split through the host catalog. Splitting duplicates the
    // focused note view intentionally (RFC §7.4); its open coalesces onto
    // the same Session buffer.
    run_host(&mut app, "workspace.split_right");
    assert_eq!(
        app.workspace.groups().count(),
        2,
        "split right creates a group"
    );
    assert_eq!(app.workspace.tree().depth(), 2);
    assert_eq!(
        app.adapter.group_to_pane.len(),
        2,
        "the adapter projects both groups"
    );
    wait_ready(&mut app);
    let duplicate = app.workspace.focused_tab();
    assert_ne!(duplicate, alpha, "the duplicate is a fresh tab identity");
    // The split duplicates whichever view is focused (beta at this point)
    // and shares its Session buffer.
    let focused_before_split = beta;
    assert_eq!(
        app.notes.state(&duplicate).and_then(|state| state.buffer),
        app.notes
            .state(&focused_before_split)
            .and_then(|state| state.buffer),
        "the duplicate shares the focused note's Session buffer"
    );

    // Move `alpha` into the other group with a typed tab move.
    let target_group: GroupId = app
        .workspace
        .group_order()
        .into_iter()
        .find(|group| {
            app.workspace
                .group(group)
                .is_some_and(|state| !state.tabs().contains(&alpha))
        })
        .expect("the new group");
    let _ = app.update(Message::Layout(LayoutAction::MoveTab {
        expected_revision: app.workspace.revision(),
        tab: alpha,
        target: flavite_workspace::TabDropTarget::Index {
            group: target_group,
            index: 0,
        },
    }));
    assert_eq!(app.workspace.group_of(alpha), Some(target_group));
    assert_eq!(
        app.workspace.tab(&alpha).map(|session| session.tab()),
        Some(alpha),
        "TabId survives the move"
    );
    let buffers_after: BTreeMap<TabId, u64> = app
        .notes
        .tabs()
        .filter_map(|(tab, state)| state.buffer.map(|buffer| (tab, buffer)))
        .collect();
    // Every tab's buffer maps onto the pre-split buffers: layout moved a
    // tab but created no new buffer.
    for (tab, buffer) in &buffers_after {
        assert!(
            buffers_before.values().any(|known| known == buffer),
            "tab {tab:?} carries a pre-existing buffer"
        );
        let _ = tab;
    }
    assert_eq!(
        buffers_after.len(),
        buffers_before.len() + 1,
        "exactly one duplicate view was added by the split"
    );

    // Same text on the shared buffer after the move.
    let text = app.note_state_text(alpha).expect("alpha text");
    assert_eq!(text, "# Alpha\n\nalpha body\n");

    // Adapter rectangles match the authoritative solver and never overlap.
    let bounds = iced::Size::new(1440.0, 800.0);
    let adapter_rects = app
        .adapter
        .pane_rectangles(flavite_workspace::SPLITTER_WIDTH, bounds);
    assert_eq!(adapter_rects.len(), 2);
    let available = flavite_workspace::Size {
        width: bounds.width,
        height: bounds.height,
    };
    let solved = flavite_workspace::rects(&app.workspace, available, 1.0, &app.resize_preview);
    for (group, adapter_rect) in &adapter_rects {
        let expected = solved.get(group).expect("solver covers every group");
        assert!(
            (adapter_rect.x - expected.x).abs() < 1.0,
            "adapter x matches the solver"
        );
        assert!(
            (adapter_rect.width - expected.width).abs() < 1.0,
            "adapter width matches the solver"
        );
    }
    let rects: Vec<_> = adapter_rects.values().collect();
    let overlap_x = (rects[0].x.max(rects[1].x))
        - (rects[0].x + rects[0].width).min(rects[1].x + rects[1].width);
    assert!(
        overlap_x >= flavite_workspace::SPLITTER_WIDTH - 1.0,
        "panes do not overlap"
    );

    // Semantic ids are stable across the layout change and routing works.
    assert_eq!(
        app.a11y_nodes
            .get(&flavite_ui::accessibility::tree::node_for_tab(alpha)),
        Some(&flavite_ui::accessibility::NodeTarget::ActivateTab(alpha))
    );

    // Keyboard tab cycling moves focus inside the group.
    let active_before = app.workspace.focused_tab();
    run_host(&mut app, "workspace.next_tab");
    let active_after = app.workspace.focused_tab();
    assert_ne!(
        active_before, active_after,
        "cycling within a two-tab group changes focus"
    );
    run_host(&mut app, "workspace.previous_tab");
    assert_eq!(app.workspace.focused_tab(), active_before);

    // Merge the neighbor group into the focused one; the split collapses.
    let focused = app.focused_group().unwrap();
    let source = app.next_group_of(focused, true).expect("the other group");
    run_host(&mut app, "workspace.merge_next_group");
    assert_eq!(
        app.workspace.groups().count(),
        1,
        "merge collapses the split"
    );
    assert_eq!(app.workspace.tree().depth(), 1);
    assert_eq!(app.adapter.group_to_pane.len(), 1);
    assert!(app.workspace.tab(&alpha).is_some() && app.workspace.tab(&beta).is_some());
    let _ = source;

    // Invalid layout operations do not lose tabs.
    let revision = app.workspace.revision();
    let ghost = GroupId::from_raw(u128::MAX);
    let _ = app.update(Message::Layout(LayoutAction::MergeGroups {
        expected_revision: revision,
        source: app.workspace.first_group().unwrap(),
        destination: ghost,
    }));
    assert_eq!(
        app.workspace.tabs().count(),
        4,
        "an invalid merge loses no tabs (boot empty + 2 notes + split duplicate)"
    );
    assert_eq!(
        app.workspace.revision(),
        revision,
        "invalid actions are no-ops"
    );
}

#[test]
fn close_reopen_round_trip_restores_the_note() {
    let Fixture { mut app, .. } = fixture();
    let (alpha, alpha_buffer) = open_note(&mut app, "alpha.md");
    let _ = app.update(Message::Layout(LayoutAction::CloseTab {
        expected_revision: app.workspace.revision(),
        tab: alpha,
    }));
    assert!(app.workspace.tab(&alpha).is_none());
    assert!(
        app.notes.state(&alpha).is_none(),
        "the view lease is released"
    );
    run_host(&mut app, "workspace.reopen_tab");
    let reopened = app.workspace.focused_tab();
    assert_ne!(reopened, alpha, "reopen mints a fresh tab identity");
    wait_ready(&mut app);
    let reopened_buffer = app
        .notes
        .state(&reopened)
        .and_then(|state| state.buffer)
        .expect("the reopened note reopens its buffer");
    assert_eq!(
        reopened_buffer, alpha_buffer,
        "the Session buffer is reused by identity"
    );
    assert_eq!(
        app.note_state_text(reopened).as_deref(),
        Some("# Alpha\n\nalpha body\n")
    );
}

#[test]
fn missing_notes_render_an_actionable_failed_state() {
    let Fixture { mut app, .. } = fixture();
    let group = app.focused_group().unwrap();
    let locator = flavite_workspace::NoteLocator::from_components(["ghost.md"]).unwrap();
    let _ = app.update(Message::OpenNote {
        locator,
        group: Some(group),
    });
    wait_ready(&mut app);
    let tab = app.workspace.focused_tab();
    let session = app.workspace.tab(&tab).unwrap();
    let ViewKind::Note(note) = session.kind() else {
        panic!("a note tab opened");
    };
    assert_eq!(
        note.load(),
        NoteLoad::Failed,
        "a missing note reports failure"
    );
    assert!(app.notes.state(&tab).unwrap().buffer.is_none());
    // The tab remains operable: close it and the workspace stays valid.
    let _ = app.update(Message::Layout(LayoutAction::CloseTab {
        expected_revision: app.workspace.revision(),
        tab,
    }));
    assert_eq!(app.workspace.tabs().count(), 1);
}

#[test]
fn host_catalog_and_palette_resolve_the_same_commands() {
    let Fixture { mut app, .. } = fixture();
    let specs = host_commands::ui_specs();
    assert!(!specs.is_empty());
    let _ = app.update(Message::PaletteOpen);
    let _ = app.update(Message::PaletteQuery("quick".into()));
    let matches = app.palette_snapshot();
    assert!(
        matches
            .iter()
            .any(|spec| spec.id == "workspace.quick_switch"),
        "the palette finds host commands: {:?}",
        matches.iter().map(|spec| spec.id).collect::<Vec<_>>()
    );
    let _ = app.update(Message::PaletteQuery(String::new()));
    let matches = app.palette_snapshot();
    // Running the selected palette row dispatches the host command.
    let tabs_before = app.workspace.tabs().count();
    let index = matches
        .iter()
        .position(|spec| spec.id == "workspace.new_tab")
        .expect("new tab matches with an empty query");
    let _ = app.update(Message::PalettePick(index));
    assert_eq!(app.workspace.tabs().count(), tabs_before + 1);
}
