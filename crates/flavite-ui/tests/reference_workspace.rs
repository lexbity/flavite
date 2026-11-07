//! Workspace behavior behind the reference-based shell presentation.
#[path = "../../flavite-api/tests/support/links.rs"]
mod support;
use flavite_ui::{
    accessibility::NodeTarget,
    app::{App, Message},
    host::commands,
};
use flavite_workspace::{LayoutAction, ViewKind};
use std::time::{Duration, Instant};

fn fixture() -> (App, tempfile::TempDir) {
    let f = support::fixture(&[("alpha.md", "# Alpha\n"), ("beta.md", "# Beta\n")]);
    let mut app = App::from_flavite(f.flavite);
    let deadline = Instant::now() + Duration::from_secs(10);
    while app.catalog().len() != 2 {
        assert!(Instant::now() < deadline, "catalog: {}", app.status_text());
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
    app.use_writing_layout();
    (app, f.cache)
}

#[test]
fn default_workbench_keeps_document_focus_and_graph_in_a_separate_right_group() {
    let (mut app, _cache) = fixture();
    let groups = app.workspace.group_order();
    assert_eq!(groups.len(), 2);
    let left = app.workspace.group(&groups[0]).unwrap();
    let right = app.workspace.group(&groups[1]).unwrap();
    assert!(matches!(
        app.workspace.tab(&left.active()).unwrap().kind(),
        ViewKind::Empty
    ));
    assert!(matches!(
        app.workspace.tab(&right.active()).unwrap().kind(),
        ViewKind::Graph(_)
    ));
    assert_eq!(
        right.tabs().len(),
        1,
        "no empty placeholder beside the graph"
    );
    assert_eq!(app.focused_group(), Some(groups[0]));
    app.use_writing_layout();
    assert_eq!(
        app.workspace.group_order(),
        groups,
        "opening the baseline twice does not create more groups"
    );
}

#[test]
fn sidebar_filter_and_tab_placement_preserve_the_graph_and_expose_native_actions() {
    let (mut app, _cache) = fixture();
    let groups = app.workspace.group_order();
    let graph = app.workspace.group(&groups[1]).unwrap().active();
    let _ = app.update(Message::ToggleSidebar);
    let _ = app.update(Message::SidebarFilter("beta".into()));
    assert_eq!(app.note_candidates().matches(&app.sidebar_filter).len(), 1);
    assert!(
        app.a11y_nodes
            .values()
            .any(|target| matches!(target, NodeTarget::SidebarFilter))
    );
    let notes: Vec<_> = app
        .a11y_nodes
        .values()
        .filter_map(|target| match target {
            NodeTarget::OpenNote(path) => Some(path),
            _ => None,
        })
        .collect();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].ends_with("beta.md"));
    app.dispatch_layout(|expected_revision| LayoutAction::FocusGroup {
        expected_revision,
        group: groups[1],
    });
    let _ = app.update(Message::SidebarToTab);
    assert!(!app.sidebar_open());
    assert_eq!(app.workspace.group(&groups[1]).unwrap().active(), graph);
    assert!(matches!(
        app.workspace
            .tab(&app.workspace.group(&groups[0]).unwrap().active())
            .unwrap()
            .kind(),
        ViewKind::Notes
    ));
    let notes_tab = app.workspace.group(&groups[0]).unwrap().active();
    assert_eq!(
        app.notes_filter(notes_tab),
        "beta",
        "placement retains the filter"
    );
    let _ = app.update(Message::NotesToSidebar(notes_tab));
    assert!(app.sidebar_open());
    assert_eq!(app.sidebar_filter, "beta");
    assert_eq!(app.workspace.group(&groups[1]).unwrap().active(), graph);
    assert_eq!(
        app.workspace.groups().count(),
        2,
        "moving Notes back preserves the document area"
    );
}

#[test]
fn sidebar_chord_and_application_menu_are_real_shell_actions() {
    use iced::keyboard::{self, Key, Modifiers};
    let (mut app, _cache) = fixture();
    let key = Key::Character("b".into());
    let message = commands::keyboard_message(
        &app,
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: Modifiers::COMMAND,
            text: None,
            repeat: false,
        },
    )
    .expect("sidebar command resolves");
    let _ = app.update(message);
    assert!(app.sidebar_open());
    let before = app.workspace.clone();
    let _ = app.update(Message::AppMenu);
    assert!(app.menu_bar_visible);
    assert!(
        app.strip_menu.is_none(),
        "revealing categories does not open an unrelated command menu"
    );
    assert_eq!(app.workspace, before);
}

#[test]
fn back_forward_take_explicit_tabs_and_stay_gated_to_notes() {
    let (mut app, _cache) = fixture();
    let groups = app.workspace.group_order();
    let empty = app.workspace.group(&groups[0]).unwrap().active();
    let before = app.workspace.clone();
    let _ = app.update(Message::NavigateBack(empty));
    assert_eq!(app.status_text(), "Back/forward applies to a note tab");
    assert_eq!(app.workspace, before, "a gated step changes nothing");
}
