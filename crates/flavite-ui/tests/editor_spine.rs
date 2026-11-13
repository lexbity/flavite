//! Editor spine integration (FEUX-0002 X03): the ticketed, planner-backed
//! delete through the real App/Session, undo identity, passive-view mapping
//! once, stale selection-version refusal, and zero-effect no-ops.

#[path = "support/mod.rs"]
mod support;

use flavite_edit_context::ContextCommand as Command;
use flavite_ui::{
    app::{App, Message},
    host::context_mode::{ContextMessage, EditorTarget},
};
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::time::{Duration, Instant};

fn fixture(seed: u64) -> (support::UiFixture, TabId) {
    let mut f = support::ui_fixture(
        seed,
        &format!("/test/spine-{seed}"),
        &[support::SeedNote {
            components: &["note.md"],
            body: "one café\nsecond line\n",
        }],
    );
    let group = f.app.focused_group().unwrap();
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["note.md"]).unwrap(),
        });
    let tab = f.app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    selection(&mut f.app, tab, 0, 0);
    (f, tab)
}

fn selection(app: &mut App, tab: TabId, anchor: usize, head: usize) {
    let view = app.note_editor_view(tab).unwrap();
    let _ = app.update(Message::AccessibleSelection {
        tab,
        incarnation: app.notes.incarnation(tab).unwrap(),
        revision: view.revision,
        anchor,
        head,
    });
}

fn run(app: &mut App, tab: TabId, command: Command) {
    let target = EditorTarget::capture(app, tab).unwrap();
    let _ = app.update(Message::ContextMode(ContextMessage::Run(target, command)));
}

#[test]
fn delete_through_the_kernel_installs_the_receipt_and_undo_restores_exactly() {
    let (mut f, tab) = fixture(4010);
    selection(&mut f.app, tab, 4, 9);
    let before_text = f.app.note_editor_view(tab).unwrap().text.clone();
    let before_revision = f.app.note_editor_view(tab).unwrap().revision;

    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::Delete);

    // The delete applied through the ticketed kernel path.
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one \nsecond line\n"
    );
    let after_revision = f.app.note_editor_view(tab).unwrap().revision;
    assert!(after_revision > before_revision);

    // Undo restores the exact text; the committed selection is the recorded
    // before-set (4..9).
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, before_text);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (4, 9)
    );
    assert_eq!(
        f.app.workspace.tab(&tab).unwrap().interaction().selections[0],
        flavite_workspace::CommittedSelection::new(4, 9)
    );
}

#[test]
fn passive_view_maps_once_and_stateale_selection_version_refuses() {
    let (mut f, tab_a) = fixture(4020);
    // Open a second view of the same note.
    let group = f.app.focused_group().unwrap();
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["note.md"]).unwrap(),
        });
    let tab_b = f.app.workspace.focused_tab();
    assert_ne!(tab_a, tab_b);
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab_b).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    // B is focused right after opening; select it here.
    selection(&mut f.app, tab_b, 0, 2);
    // Focus the first view before selecting and mutating.
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::ActivateTab {
            expected_revision,
            tab: tab_a,
        });
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.workspace.focused_tab() != tab_a {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    selection(&mut f.app, tab_a, 4, 9);
    run(&mut f.app, tab_a, Command::Toggle);
    run(&mut f.app, tab_a, Command::Delete);

    // The passive view mapped once to the after-text.
    assert_eq!(
        &*f.app.note_editor_view(tab_b).unwrap().text,
        "one \nsecond line\n"
    );
    let b_selection = f.app.editor_interaction_snapshot(tab_b).unwrap().selection;
    // The old selection 0..2 lies entirely before the deletion; it maps
    // unchanged (the deletion starts at 4).
    assert_eq!(b_selection, (0, 2));

    // A captured target with a stale selection version is refused.
    let fake = EditorTarget {
        tab: tab_a,
        incarnation: f.app.notes.incarnation(tab_a).unwrap(),
        buffer: f.app.note_editor_view(tab_a).unwrap().buffer_id,
        revision: f.app.note_editor_view(tab_a).unwrap().revision,
        selection: (0, 0),
        selections: vec![(0, 0)],
        primary: 0,
        selection_version: 999_999,
    };
    let text_before = f.app.note_editor_view(tab_a).unwrap().text.clone();
    let _ = f.app.update(Message::ContextMode(ContextMessage::Run(
        fake,
        Command::Delete,
    )));
    assert_eq!(
        f.app.note_editor_view(tab_a).unwrap().text,
        text_before,
        "stale selection version must refuse the mutation"
    );
}

#[test]
fn kernel_delete_empty_line_creates_no_history_node() {
    // Build a fixture with an empty line directly.
    let mut f = support::ui_fixture(
        4031,
        "/test/spine-4031",
        &[support::SeedNote {
            components: &["note.md"],
            body: "a\n\nb",
        }],
    );
    let group = f.app.focused_group().unwrap();
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["note.md"]).unwrap(),
        });
    let tab = f.app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    selection(&mut f.app, tab, 2, 2);
    let before_revision = f.app.note_editor_view(tab).unwrap().revision;
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::Delete);
    assert_eq!(&*f.app.note_editor_view(tab).unwrap().text, "a\n\nb");
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        before_revision,
        "a no-op delete must not bump the revision"
    );
}
