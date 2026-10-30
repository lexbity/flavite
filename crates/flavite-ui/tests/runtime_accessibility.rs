//! Native semantic-tree integration (RFC-0003 W01/W02).
//!
//! The running shell publishes the real Session-backed workspace: note
//! text, tab structure, and action routing. Native actions route through
//! the ordinary message path; removed nodes are rejected.

use flavite_api::{Flavite, VaultConfig};
use flavite_buffer::manager::BufferManager;
use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_ui::accessibility::program::FlaviteProgram;
use flavite_ui::accessibility::{route_action, tree};
use flavite_ui::app::{App, Message};
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use iced_winit::program::Program;

#[test]
fn running_shell_program_publishes_the_workspace_and_routes_actions() {
    // Real temporary vault with one note.
    let fs = MemFs::new();
    let root = std::path::PathBuf::from("/test/a11y-vault");
    fs.create_dir_all(&root).unwrap();
    fs.write_new(&root.join("note.md"), b"# Native bridge note\n\nbody\n")
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let db = temp.path().join("a11y.sqlite3");
    drop(flavite_index::open_or_init(&db).unwrap());
    let identity =
        flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![9, 1, 1]);
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
    let mut app = App::from_flavite(flavite);

    // Open the note through the shell and settle its open flight.
    let group = app.workspace.first_group().unwrap();
    let locator = flavite_workspace::NoteLocator::from_components(["note.md"]).unwrap();
    let _ = app.update(Message::OpenNote {
        locator,
        group: Some(group),
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let _ = app.update(Message::Pump);
        let ready = app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|session| match session.kind() {
                flavite_workspace::ViewKind::Note(note) => {
                    note.load() == flavite_workspace::NoteLoad::Ready
                }
                _ => false,
            });
        if ready {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    // The program publishes the shell tree with the real note text.
    let update = FlaviteProgram
        .accessibility_tree(&app, iced_winit::program::core::window::Id::unique())
        .expect("the shell publishes a semantic tree");
    let text_node = update
        .nodes
        .iter()
        .find(|(id, _)| *id == tree::exposed_text_node())
        .map(|(_, node)| node)
        .expect("the active note text is exposed");
    assert_eq!(text_node.value(), Some("# Native bridge note\n\nbody\n"));

    // A Click on a tab node routes through the ordinary message path.
    let tab = app.workspace.focused_tab();
    let tab_node = tree::node_for_tab(tab);
    let request = accesskit::ActionRequest {
        action: accesskit::Action::Click,
        target_tree: accesskit::TreeId::ROOT,
        target_node: tab_node,
        data: None,
    };
    match route_action(&app, request) {
        Some(Message::Layout(flavite_workspace::LayoutAction::ActivateTab {
            tab: routed, ..
        })) => {
            assert_eq!(routed, tab);
        }
        other => panic!("a tab click must route to ActivateTab, got {other:?}"),
    }

    // Actions referencing removed nodes are rejected.
    let ghost = accesskit::NodeId(u64::MAX);
    let request = accesskit::ActionRequest {
        action: accesskit::Action::Click,
        target_tree: accesskit::TreeId::ROOT,
        target_node: ghost,
        data: None,
    };
    assert!(
        route_action(&app, request).is_none(),
        "unknown nodes are rejected"
    );

    // Closing the tab through the routed action removes it from the tree.
    let _ = app.update(Message::Layout(flavite_workspace::LayoutAction::CloseTab {
        expected_revision: app.workspace.revision(),
        tab,
    }));
    assert!(
        app.workspace.tab(&tab).is_none(),
        "the closed tab no longer exists"
    );
}
