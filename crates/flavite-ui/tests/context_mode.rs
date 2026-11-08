//! Context controls use real Session edits, guarded view state, and shared undo.
#[path = "support/mod.rs"]
mod support;

use flavite_editor::{
    context::{ContextCommand as Command, ContextMenu, ContextMode},
    input::CapturedInput,
    keymap::KeyAction,
};
use flavite_ui::{
    app::{App, Message},
    host::context_mode::{ContextMessage, EditorTarget},
};
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::time::{Duration, Instant};

fn fixture(seed: u64) -> (support::UiFixture, TabId) {
    let mut f = support::ui_fixture(
        seed,
        &format!("/test/context-{seed}"),
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

fn type_text(app: &mut App, tab: TabId, text: &str) {
    let view = app.note_editor_view(tab).unwrap();
    let sequence = app.workspace.tab(&tab).unwrap().interaction().sequence + 1;
    let _ = app.update(Message::EditorInput {
        tab,
        incarnation: app.notes.incarnation(tab).unwrap(),
        input: CapturedInput {
            buffer: view.buffer_id,
            observed_revision: view.revision,
            sequence,
            action: KeyAction::Insert,
            text: text.into(),
            extend: false,
            document_boundary: false,
            vertical: None,
        },
    });
}

#[test]
fn enter_delete_undo_exit_preserves_source_identity_and_selection() {
    let (mut f, tab) = fixture(3101);
    selection(&mut f.app, tab, 4, 9);
    let before = f.app.note_editor_view(tab).unwrap();
    run(&mut f.app, tab, Command::Toggle);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Command);
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        before.revision,
        "entry must not edit"
    );
    run(&mut f.app, tab, Command::Delete);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one \nsecond line\n"
    );
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, before.text);
    selection(&mut f.app, tab, 4, 9);
    run(&mut f.app, tab, Command::Exit);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Off);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (4, 9)
    );
}

#[test]
fn typing_survives_reposition_and_toolbar_actions_with_separate_undo_groups() {
    let (mut f, tab) = fixture(3102);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::InsertStart);
    type_text(&mut f.app, tab, "A");
    type_text(&mut f.app, tab, "B");
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
    selection(&mut f.app, tab, 3, 3);
    type_text(&mut f.app, tab, "C");
    run(&mut f.app, tab, Command::Copy);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
    run(&mut f.app, tab, Command::Escape);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Command);
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "ABone café\nsecond line\n"
    );
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one café\nsecond line\n"
    );
}

#[test]
fn change_and_open_line_include_subsequent_typing_in_one_undo() {
    let (mut f, tab) = fixture(3103);
    selection(&mut f.app, tab, 4, 9);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::Change);
    type_text(&mut f.app, tab, "tea");
    run(&mut f.app, tab, Command::Escape);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one tea\nsecond line\n"
    );
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one café\nsecond line\n"
    );
    selection(&mut f.app, tab, 0, 0);
    run(&mut f.app, tab, Command::OpenAbove);
    type_text(&mut f.app, tab, "new");
    run(&mut f.app, tab, Command::Escape);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "new\none café\nsecond line\n"
    );
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one café\nsecond line\n"
    );
}

#[test]
fn right_click_entry_and_escape_pop_one_level_without_exiting() {
    let (mut f, tab) = fixture(3104);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::ContextMode(ContextMessage::EnterAt(
        target,
        (4, 9),
    )));
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Command);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (4, 9)
    );
    run(&mut f.app, tab, Command::InsertEnd);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::ContextMode(ContextMessage::OpenMenu(
        target,
        ContextMenu::Select,
    )));
    run(&mut f.app, tab, Command::Escape);
    assert!(f.app.context_mode.session.as_ref().unwrap().menu.is_none());
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
    run(&mut f.app, tab, Command::Escape);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Command);
    run(&mut f.app, tab, Command::Escape);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Command);
}

#[test]
fn stale_controls_cannot_edit_or_exit_a_different_editor() {
    let (mut f, tab) = fixture(3105);
    run(&mut f.app, tab, Command::Toggle);
    let old = EditorTarget::capture(&f.app, tab).unwrap();
    run(&mut f.app, tab, Command::Delete);
    let before = f.app.note_editor_view(tab).unwrap();
    let _ = f.app.update(Message::ContextMode(ContextMessage::Run(
        old,
        Command::Delete,
    )));
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        before.revision
    );
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, before.text);
    let group = f.app.focused_group().unwrap();
    let _ = f.app.update(Message::Layout(LayoutAction::OpenGraph {
        expected_revision: f.app.workspace.revision(),
        group,
    }));
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Off);
    assert!(f.app.context_mode.session.is_none());
}

#[test]
fn window_blur_retains_mode_but_reading_and_tab_activation_exit() {
    let (mut f, tab) = fixture(3106);
    run(&mut f.app, tab, Command::Toggle);
    let _ = f
        .app
        .update(Message::WindowEvent(iced::window::Event::Unfocused));
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Command);
    let _ = f.app.update(Message::ToggleNoteMode(tab));
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Off);
}

#[test]
fn read_only_controls_refuse_mutation_but_keep_exit_and_selection_available() {
    let (mut f, tab) = fixture(3107);
    run(&mut f.app, tab, Command::Toggle);
    f.app.flavite.as_mut().unwrap().set_read_only(true);
    let before = f.app.note_editor_view(tab).unwrap();
    run(&mut f.app, tab, Command::Delete);
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        before.revision
    );
    run(&mut f.app, tab, Command::SelectAll);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (0, before.text.len())
    );
    run(&mut f.app, tab, Command::Exit);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Off);
}

#[test]
fn all_published_commands_have_unique_canonical_ids() {
    let specs = flavite_ui::host::commands::ui_specs(&Default::default());
    let unique: std::collections::BTreeSet<_> = specs.iter().map(|s| s.id).collect();
    assert_eq!(unique.len(), specs.len());
    for command in Command::ALL {
        if command.id().starts_with("editor.") {
            assert!(unique.contains(command.id()), "{}", command.id());
        }
    }
}

#[test]
fn native_accessibility_controls_use_real_targets_and_exit_through_owned_overlays() {
    use flavite_ui::accessibility::{route_action, tree};
    let (mut f, tab) = fixture(3108);
    run(&mut f.app, tab, Command::Toggle);
    // Native renderer supplies these observed rectangles. This test exercises
    // semantic routing; the Xvfb scenario separately proves their real paint.
    for (name, x) in [(Command::Exit.id(), 0.0), (Command::Undo.id(), 40.0)] {
        let _ = f.app.update(Message::ContextMode(ContextMessage::Bounds(
            tab,
            name,
            iced::Rectangle::new(iced::Point::new(x, 80.0), iced::Size::new(32.0, 32.0)),
        )));
    }
    let semantic = tree::shell_tree(&f.app);
    let exit = semantic
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some("Exit Context Mode"))
        .unwrap();
    assert!(exit.1.supports_action(accesskit::Action::Click));
    assert_eq!(exit.1.bounds().unwrap().width(), 32.0);
    assert!(
        semantic
            .nodes
            .iter()
            .any(|(_, n)| n.role() == accesskit::Role::Toolbar
                && n.label() == Some("Context Mode · COMMAND"))
    );
    let id = exit.0;
    let click = || accesskit::ActionRequest {
        action: accesskit::Action::Click,
        target_tree: accesskit::TreeId::ROOT,
        target_node: id,
        data: None,
    };
    let _ = f.app.update(Message::PaletteOpen);
    let message =
        route_action(&f.app, click()).expect("Exit remains reachable through the owned palette");
    let _ = f.app.update(message);
    assert!(!f.app.palette_open);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Off);
    assert!(
        route_action(&f.app, click()).is_none(),
        "removed toolbar nodes cannot act"
    );
}

#[test]
fn delayed_clipboard_cannot_mutate_after_selection_change_or_mode_exit() {
    let (mut f, tab) = fixture(3109);
    run(&mut f.app, tab, Command::Toggle);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let original = f.app.note_editor_view(tab).unwrap();
    selection(&mut f.app, tab, 4, 9);
    let _ = f
        .app
        .update(Message::ContextMode(ContextMessage::PasteReady(
            target,
            1,
            Some("stale".into()),
        )));
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, original.text);
    run(&mut f.app, tab, Command::Exit);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f
        .app
        .update(Message::ContextMode(ContextMessage::PasteReady(
            target,
            1,
            Some("stale".into()),
        )));
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        original.revision
    );
}

#[test]
fn queued_keys_resolve_current_mode_and_selection_without_waiting_for_redraw() {
    use flavite_editor::context::{ContextEvent, ContextKey, ContextRequest};
    use iced::keyboard::{Modifiers, key::Code};
    let (mut f, tab) = fixture(3110);
    let source = EditorTarget::capture(&f.app, tab).unwrap();
    let sequence = f.app.workspace.tab(&tab).unwrap().interaction().sequence;
    let request = |index, event| ContextRequest {
        buffer: source.buffer,
        revision: source.revision,
        selection: source.selection,
        sequence: sequence + index,
        event,
    };
    let key = |index, code, text: &str| {
        request(
            index,
            ContextEvent::Key(ContextKey {
                code: Some(code),
                modifiers: Modifiers::empty(),
                repeat: false,
                input: CapturedInput {
                    buffer: source.buffer,
                    observed_revision: source.revision,
                    sequence: sequence + index,
                    action: KeyAction::Insert,
                    text: text.into(),
                    extend: false,
                    document_boundary: false,
                    vertical: None,
                },
            }),
        )
    };
    let send = |app: &mut App, request| {
        let _ = app.update(Message::ContextNative {
            tab,
            incarnation: source.incarnation,
            request,
        });
    };
    send(
        &mut f.app,
        request(1, ContextEvent::Command(Command::Toggle)),
    );
    send(&mut f.app, key(2, Code::KeyI, "i"));
    // The key is captured from the COMMAND frame but is text in current TYPING.
    send(&mut f.app, key(3, Code::KeyA, "Z"));
    assert!(
        f.app
            .note_editor_view(tab)
            .unwrap()
            .text
            .starts_with("Zone")
    );
    send(
        &mut f.app,
        request(4, ContextEvent::Command(Command::Escape)),
    );
    // Conversely, a key captured from TYPING is a command after Escape.
    send(&mut f.app, key(5, Code::KeyD, "d"));
    assert!(f.app.note_editor_view(tab).unwrap().text.starts_with("Zne"));
    send(&mut f.app, key(6, Code::KeyU, "u"));
    assert!(
        f.app
            .note_editor_view(tab)
            .unwrap()
            .text
            .starts_with("Zone")
    );
    let before = f.app.note_editor_view(tab).unwrap();
    send(&mut f.app, key(5, Code::KeyD, "d"));
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        before.revision,
        "replayed native event is refused"
    );
    send(
        &mut f.app,
        request(7, ContextEvent::Command(Command::Toggle)),
    );
    send(&mut f.app, key(8, Code::KeyD, "d"));
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Off);
    assert!(
        f.app
            .note_editor_view(tab)
            .unwrap()
            .text
            .starts_with("Zdone"),
        "after Exit a stale command frame must type normally"
    );
}

#[test]
fn undersized_pane_suspends_edits_without_losing_mode_or_selection() {
    let (mut f, tab) = fixture(3111);
    selection(&mut f.app, tab, 4, 9);
    run(&mut f.app, tab, Command::Toggle);
    let before = f.app.note_editor_view(tab).unwrap();
    let _ = f.app.update(Message::EditorBounds(
        tab,
        iced::Rectangle::with_size(iced::Size::new(300.0, 500.0)),
    ));
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    assert!(flavite_ui::host::context_mode::too_narrow(&f.app, tab));
    assert!(!flavite_ui::host::context_mode::available(
        &f.app,
        target,
        Command::Delete
    ));
    assert!(flavite_ui::host::context_mode::available(
        &f.app,
        target,
        Command::Exit
    ));
    run(&mut f.app, tab, Command::Delete);
    run(&mut f.app, tab, Command::InsertStart);
    type_text(&mut f.app, tab, "refused");
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Command);
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        before.revision
    );
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (4, 9)
    );
    let _ = f.app.update(Message::EditorBounds(
        tab,
        iced::Rectangle::with_size(iced::Size::new(500.0, 500.0)),
    ));
    assert!(!flavite_ui::host::context_mode::too_narrow(&f.app, tab));
    run(&mut f.app, tab, Command::Delete);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one \nsecond line\n"
    );
}

#[test]
fn composition_commit_acknowledges_caret_and_joins_the_explicit_typing_group() {
    use flavite_editor::context::{ContextEvent, ContextRequest};
    let (mut f, tab) = fixture(3112);
    run(&mut f.app, tab, Command::Toggle);
    let original = f.app.note_editor_view(tab).unwrap();
    let stale_commit = flavite_editor::EditorAction::replace(&original, 0, 0, "leak".into());
    let _ = f.app.update(Message::EditorAction {
        tab,
        incarnation: f.app.notes.incarnation(tab).unwrap(),
        action: stale_commit,
    });
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        original.revision
    );
    run(&mut f.app, tab, Command::InsertStart);
    for text in ["日本語", "雪"] {
        let mut preedit = f.app.editor_interaction_snapshot(tab).unwrap();
        preedit.sequence += 1;
        preedit.composing = true;
        let _ = f.app.update(Message::EditorAction {
            tab,
            incarnation: f.app.notes.incarnation(tab).unwrap(),
            action: flavite_editor::EditorAction::Interaction(preedit),
        });
        let target = EditorTarget::capture(&f.app, tab).unwrap();
        let sequence = preedit.sequence + 1;
        let _ = f.app.update(Message::ContextNative {
            tab,
            incarnation: target.incarnation,
            request: ContextRequest {
                buffer: target.buffer,
                revision: target.revision,
                selection: target.selection,
                sequence,
                event: ContextEvent::Commit(CapturedInput {
                    buffer: target.buffer,
                    observed_revision: target.revision,
                    sequence,
                    action: KeyAction::Insert,
                    text: text.into(),
                    extend: false,
                    document_boundary: false,
                    vertical: None,
                }),
            },
        });
        assert_eq!(
            f.app.workspace.tab(&tab).unwrap().interaction().composition,
            flavite_workspace::CompositionState::Idle,
        );
    }
    assert!(
        f.app
            .note_editor_view(tab)
            .unwrap()
            .text
            .starts_with("日本語雪one")
    );
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (12, 12)
    );
    run(&mut f.app, tab, Command::Escape);
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, original.text);
}

#[test]
fn keys_from_a_departed_editor_cannot_become_literal_text_in_a_visible_background_pane() {
    use flavite_editor::context::{ContextEvent, ContextKey, ContextRequest};
    let (mut f, tab) = fixture(3113);
    run(&mut f.app, tab, Command::Toggle);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let original = f.app.note_editor_view(tab).unwrap();
    let sequence = f.app.workspace.tab(&tab).unwrap().interaction().sequence + 1;
    let _ = f
        .app
        .update(Message::RunHostCommand("workspace.split_right"));
    assert_ne!(f.app.workspace.focused_tab(), tab);
    assert!(
        f.app.is_tab_visible(tab),
        "the test must cover a visible background source"
    );
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Off);
    let _ = f.app.update(Message::ContextNative {
        tab,
        incarnation: target.incarnation,
        request: ContextRequest {
            buffer: target.buffer,
            revision: target.revision,
            selection: target.selection,
            sequence,
            event: ContextEvent::Key(ContextKey {
                code: Some(iced::keyboard::key::Code::KeyD),
                modifiers: iced::keyboard::Modifiers::empty(),
                repeat: false,
                input: CapturedInput {
                    buffer: target.buffer,
                    observed_revision: target.revision,
                    sequence,
                    action: KeyAction::Insert,
                    text: "d".into(),
                    extend: false,
                    document_boundary: false,
                    vertical: None,
                },
            }),
        },
    });
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        original.revision
    );
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, original.text);
}
