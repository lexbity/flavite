use accesskit::{Action, ActionRequest, NodeId, TreeId};
use flavite_api::{Flavite, VaultConfig, VaultIdentity};
use flavite_buffer::manager::BufferManager;
use flavite_fs::{Fs, MemFs};
use flavite_index::IndexActor;
use flavite_ui::accessibility::{program::FlaviteProgram, tree};
use flavite_ui::app::{App, Message};
use flavite_ui::pane::Pane;
use flavite_vault::actor::VaultActorBuilder;
use flavite_vault::channels::ChannelSet;
use flavite_vault::session::Session;
use iced_winit::program::Program;
use std::path::PathBuf;

fn fixture(note_contents: &str) -> (Flavite, tempfile::TempDir) {
    let filesystem = MemFs::new();
    let root = PathBuf::from("/test/runtime-accessibility");
    filesystem.create_dir_all(&root).unwrap();
    filesystem
        .write_new(&root.join("demo.md"), note_contents.as_bytes())
        .unwrap();
    let temp = tempfile::tempdir().unwrap();
    let database = temp.path().join("index.sqlite3");
    drop(flavite_index::open_or_init(&database).unwrap());
    let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root, vec![4, 0, 4]);
    let actor = VaultActorBuilder::new()
        .with_fs(Box::new(filesystem) as Box<dyn Fs + Send + Sync>)
        .with_identity(identity.clone())
        .with_index(IndexActor::new(&database, 1).unwrap())
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
    (
        Flavite::open(identity, session, VaultConfig::default()).unwrap(),
        temp,
    )
}

#[test]
fn running_shell_program_publishes_the_open_session_note_and_routes_shell_actions() {
    let note = "# Accessible shell note\n\ncurrent Session text\n";
    let (flavite, _temp) = fixture(note);
    let mut app = App::from_flavite(flavite);
    let _ = app.update(Message::Navigate(Pane::Editor));

    let program = FlaviteProgram;
    let update = program
        .accessibility_tree(&app, iced_winit::program::core::window::Id::unique())
        .expect("shell semantic tree");
    let editor_text = update
        .nodes
        .iter()
        .find(|(id, _)| *id == tree::EDITOR_TEXT)
        .map(|(_, node)| node.value())
        .expect("editor text node")
        .expect("current note text");
    assert_eq!(editor_text, note);

    let focus_action = ActionRequest {
        action: Action::Focus,
        target_tree: TreeId::ROOT,
        target_node: tree::EDITOR,
        data: None,
    };
    assert!(matches!(
        program.accessibility_action(
            &app,
            iced_winit::program::core::window::Id::unique(),
            focus_action,
        ),
        Some(Message::EditorFocusRequested)
    ));

    let action = ActionRequest {
        action: Action::Click,
        target_tree: TreeId::ROOT,
        target_node: NodeId(13),
        data: None,
    };
    assert!(matches!(
        program.accessibility_action(
            &app,
            iced_winit::program::core::window::Id::unique(),
            action,
        ),
        Some(Message::Navigate(Pane::Graph))
    ));
}
