//! Context controls use real Session edits, guarded view state, and shared undo.
#[path = "support/mod.rs"]
mod support;

use flavite_edit_context::{ContextCommand as Command, ContextMenu, ContextMode};
use flavite_editor::{
    context::{native_code, native_modifiers},
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
            preferred_x: Vec::new(),
        },
    });
}

#[test]
fn implicit_preview_and_copy_preserve_committed_state_and_cut_uses_the_same_range() {
    use flavite_ui::host::context_mode::{effect_caption, preview_command};
    let (mut f, tab) = fixture(3120);
    selection(&mut f.app, tab, 4, 4);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::InsertStart);
    let source = EditorTarget::capture(&f.app, tab).unwrap();
    let before = f.app.editor_interaction_snapshot(tab).unwrap();
    let text = f.app.note_editor_view(tab).unwrap();
    let history = f.app.context_mode.history.page.clone();
    let _ = f.app.update(Message::ContextMode(ContextMessage::Preview(
        source,
        Command::Copy,
        true,
    )));
    assert_eq!(preview_command(&f.app, tab), Some(Command::Copy));
    assert_eq!(
        effect_caption(&f.app, tab).as_deref(),
        Some("Copy outlined line")
    );
    assert_eq!(f.app.editor_interaction_snapshot(tab).unwrap(), before);
    assert_eq!(f.app.context_mode.history.page, history);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
    run(&mut f.app, tab, Command::Copy);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (4, 4),
        "implicit line Copy must retain the caret"
    );
    assert_eq!(f.app.note_editor_view(tab).unwrap().revision, text.revision);
    assert_eq!(
        f.app
            .context_mode
            .history
            .page
            .as_ref()
            .unwrap()
            .entries
            .len(),
        history.unwrap().entries.len()
    );
    let effect = flavite_edit_context::effect::plan(&text.text, (4, 4), Command::Cut).unwrap();
    run(&mut f.app, tab, Command::Cut);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        &format!(
            "{}{}",
            &text.text[..effect.range.start],
            &text.text[effect.range.end..]
        )
    );
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, text.text);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (4, 4)
    );
}

#[test]
fn preview_ownership_rejects_stale_sources_and_old_hover_exits() {
    use flavite_ui::host::context_mode::{effect_caption, preview_command};
    let (mut f, tab) = fixture(3121);
    run(&mut f.app, tab, Command::Toggle);
    let old = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::ContextMode(ContextMessage::Preview(
        old.clone(),
        Command::Copy,
        true,
    )));
    let _ = f.app.update(Message::ContextMode(ContextMessage::Preview(
        old.clone(),
        Command::Cut,
        true,
    )));
    let _ = f.app.update(Message::ContextMode(ContextMessage::Preview(
        old.clone(),
        Command::Copy,
        false,
    )));
    assert_eq!(preview_command(&f.app, tab), Some(Command::Cut));
    selection(&mut f.app, tab, 9, 9);
    let _ = f.app.update(Message::ContextMode(ContextMessage::Preview(
        old.clone(),
        Command::Copy,
        true,
    )));
    assert_eq!(preview_command(&f.app, tab), Some(Command::Delete));
    assert!(effect_caption(&f.app, tab).is_none());
    let view = f.app.note_editor_view(tab).unwrap();
    let effect = flavite_edit_context::effect::plan(&view.text, (9, 9), Command::Delete).unwrap();
    assert_eq!(effect.range, 7..9);
    run(&mut f.app, tab, Command::Delete);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one caf\nsecond line\n"
    );
    run(&mut f.app, tab, Command::History);
    let source = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::ContextMode(ContextMessage::Preview(
        source,
        Command::Cut,
        true,
    )));
    assert!(preview_command(&f.app, tab).is_none());
    run(&mut f.app, tab, Command::Exit);
    assert!(preview_command(&f.app, tab).is_none());
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
        vec![(4, 9)],
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
    assert!(
        f.app
            .context_mode
            .session
            .as_ref()
            .unwrap()
            .state
            .menu()
            .is_none()
    );
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
        .update(Message::EditorClipboard(
            flavite_ui::host::editor_clipboard::ClipboardMessage::ContextPaste {
                target,
                generation: 1,
                text: Some("stale".into()),
            },
        ));
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, original.text);
    run(&mut f.app, tab, Command::Exit);
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f
        .app
        .update(Message::EditorClipboard(
            flavite_ui::host::editor_clipboard::ClipboardMessage::ContextPaste {
                target,
                generation: 1,
                text: Some("stale".into()),
            },
        ));
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
        selections: vec![source.selection],
        sequence: sequence + index,
        event,
    };
    let key = |index, code, text: &str| {
        request(
            index,
            ContextEvent::Key(ContextKey {
                code: native_code(code),
                modifiers: native_modifiers(Modifiers::empty()),
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
                    preferred_x: Vec::new(),
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
        target.clone(),
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
            action: flavite_editor::EditorAction::Interaction(preedit.clone()),
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
                selections: vec![target.selection],
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
                    preferred_x: Vec::new(),
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
            selections: vec![target.selection],
            sequence,
            event: ContextEvent::Key(ContextKey {
                code: native_code(iced::keyboard::key::Code::KeyD),
                modifiers: native_modifiers(iced::keyboard::Modifiers::empty()),
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
                    preferred_x: Vec::new(),
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

#[test]
fn history_controls_restore_selection_and_branch_without_adding_an_edit() {
    use flavite_ui::host::editor_history::{HistoryAction, HistoryMessage};
    let (mut f, tab) = fixture(3114);
    selection(&mut f.app, tab, 4, 9);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::Change);
    type_text(&mut f.app, tab, "tea");
    run(&mut f.app, tab, Command::Escape);
    let changed = f.app.note_editor_view(tab).unwrap();
    let page = f.app.context_mode.history.page.clone().unwrap();
    let changed_state = page.trail.last().unwrap().target;
    assert_eq!(
        page.trail.last().unwrap().kind,
        flavite_buffer::history::EditKind::Change
    );
    run(&mut f.app, tab, Command::Undo);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (4, 9)
    );
    run(&mut f.app, tab, Command::Redo);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (7, 7)
    );
    selection(&mut f.app, tab, 0, 0);
    run(&mut f.app, tab, Command::Delete);
    let before = f.app.note_editor_view(tab).unwrap();
    let count = f
        .app
        .context_mode
        .history
        .page
        .as_ref()
        .unwrap()
        .entries
        .len();
    let source = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::EditorHistory(HistoryMessage::Act(
        source,
        HistoryAction::Restore(changed_state),
    )));
    assert_eq!(f.app.note_editor_view(tab).unwrap().text, changed.text);
    assert!(f.app.note_editor_view(tab).unwrap().revision > before.revision);
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (7, 7)
    );
    assert_eq!(
        f.app
            .context_mode
            .history
            .page
            .as_ref()
            .unwrap()
            .entries
            .len(),
        count
    );
    run(&mut f.app, tab, Command::InsertEnd);
    type_text(&mut f.app, tab, " branch");
    assert_eq!(
        f.app
            .context_mode
            .history
            .page
            .as_ref()
            .unwrap()
            .entries
            .len(),
        count + 1
    );
}

#[test]
fn history_popup_suspends_typing_escape_closes_it_and_exit_stays_available() {
    use flavite_ui::host::editor_history::{HistoryAction, HistoryMessage};
    let (mut f, tab) = fixture(3115);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::InsertStart);
    type_text(&mut f.app, tab, "A");
    let original = f.app.note_editor_view(tab).unwrap();
    let pending_paste = EditorTarget::capture(&f.app, tab).unwrap();
    let paste_generation = f.app.context_mode.generation();
    run(&mut f.app, tab, Command::History);
    assert!(f.app.context_mode.history.open);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
    type_text(&mut f.app, tab, "not editor input");
    run(&mut f.app, tab, Command::Delete);
    let _ = f
        .app
        .update(Message::EditorClipboard(
            flavite_ui::host::editor_clipboard::ClipboardMessage::ContextPaste {
                target: pending_paste.clone(),
                generation: paste_generation,
                text: Some("queued paste".into()),
            },
        ));
    let _ = f.app.update(Message::ContextMode(ContextMessage::OpenMenu(
        pending_paste.clone(),
        ContextMenu::Select,
    )));
    assert!(
        f.app
            .context_mode
            .session
            .as_ref()
            .unwrap()
            .state
            .menu()
            .is_none()
    );
    assert!(
        f.app.context_mode.history.open,
        "queued background commands cannot dismiss an owned chooser"
    );
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        original.revision
    );
    let source = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::EditorHistory(HistoryMessage::Act(
        source,
        HistoryAction::Close,
    )));
    assert!(!f.app.context_mode.history.open);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
    run(&mut f.app, tab, Command::History);
    run(&mut f.app, tab, Command::Exit);
    assert!(!f.app.context_mode.history.open);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Off);
}

#[test]
fn stale_history_activation_and_read_only_restore_leave_the_source_unchanged() {
    use flavite_ui::host::editor_history::{HistoryAction, HistoryMessage};
    let (mut f, tab) = fixture(3116);
    run(&mut f.app, tab, Command::Toggle);
    let root = f.app.context_mode.history.page.as_ref().unwrap().entries[0].target;
    run(&mut f.app, tab, Command::Delete);
    let old = EditorTarget::capture(&f.app, tab).unwrap();
    run(&mut f.app, tab, Command::Delete);
    let before = f.app.note_editor_view(tab).unwrap();
    let _ = f.app.update(Message::EditorHistory(HistoryMessage::Act(
        old,
        HistoryAction::Restore(root),
    )));
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        before.revision
    );
    f.app.flavite.as_mut().unwrap().set_read_only(true);
    let source = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::EditorHistory(HistoryMessage::Act(
        source,
        HistoryAction::Restore(root),
    )));
    assert_eq!(
        f.app.note_editor_view(tab).unwrap().revision,
        before.revision
    );
}

#[test]
fn history_maps_passive_split_selections_and_scroll_without_adopting_or_focusing_them() {
    use flavite_ui::host::editor_history::{HistoryAction, HistoryMessage};
    use flavite_workspace::CommittedSelection;
    let (mut f, passive) = fixture(3117);
    let _ = f
        .app
        .update(Message::RunHostCommand("workspace.split_right"));
    let source = f.app.workspace.focused_tab();
    assert_ne!(source, passive);
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(source).is_none() {
        assert!(Instant::now() < deadline);
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    let text = f.app.note_editor_view(passive).unwrap().text;
    let mut interaction = f.app.workspace.tab(&passive).unwrap().interaction().clone();
    interaction.selections = vec![CommittedSelection::new(15, 11)];
    interaction.viewport_anchor = 10;
    interaction.focused = false;
    interaction.sequence += 1;
    f.app
        .workspace
        .acknowledge_interaction(interaction, &text)
        .unwrap();
    selection(&mut f.app, source, 0, 0);
    run(&mut f.app, source, Command::Toggle);
    let root = f.app.context_mode.history.page.as_ref().unwrap().entries[0].target;
    run(&mut f.app, source, Command::InsertStart);
    type_text(&mut f.app, source, "雪");
    let passive_view = f.app.workspace.tab(&passive).unwrap().interaction();
    assert_eq!(
        passive_view.selections,
        vec![CommittedSelection::new(18, 14)]
    );
    assert_eq!(passive_view.viewport_anchor, 13);
    assert!(!passive_view.focused);
    let target = EditorTarget::capture(&f.app, source).unwrap();
    let _ = f.app.update(Message::EditorHistory(HistoryMessage::Act(
        target,
        HistoryAction::Restore(root),
    )));
    let passive_view = f.app.workspace.tab(&passive).unwrap().interaction();
    assert_eq!(
        passive_view.selections,
        vec![CommittedSelection::new(15, 11)]
    );
    assert_eq!(passive_view.viewport_anchor, 10);
    assert!(!passive_view.focused);
    assert_eq!(
        passive_view.revision,
        f.app.note_editor_view(source).unwrap().revision
    );
    assert_eq!(
        f.app.editor_interaction_snapshot(source).unwrap().selection,
        (0, 0)
    );
    assert_eq!(f.app.workspace.document_anchor(), Some(source));
}

#[test]
fn accessible_history_restores_through_the_real_handler_and_masks_background_actions() {
    use flavite_ui::{
        accessibility::{route_action, tree},
        host::editor_history::HistoryMessage,
    };
    let (mut f, tab) = fixture(3118);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::InsertStart);
    type_text(&mut f.app, tab, "A");
    run(&mut f.app, tab, Command::History);
    let source = EditorTarget::capture(&f.app, tab).unwrap();
    let node = f
        .app
        .context_mode
        .history
        .page
        .as_ref()
        .unwrap()
        .entries
        .last()
        .unwrap()
        .target
        .node;
    let _ = f.app.update(Message::EditorHistory(HistoryMessage::Bounds {
        source,
        generation: f.app.context_mode.generation(),
        node,
        popup: true,
        rect: iced::Rectangle::new(iced::Point::new(80.0, 180.0), iced::Size::new(400.0, 52.0)),
    }));
    let semantic = tree::shell_tree(&f.app);
    let id = semantic
        .nodes
        .iter()
        .find(|(_, n)| n.label() == Some("Initial state · Earlier state"))
        .unwrap()
        .0;
    assert!(
        tree::build_node_map(&f.app)
            .values()
            .all(|target| !matches!(target, tree::NodeTarget::Editor { .. }))
    );
    let message = route_action(
        &f.app,
        accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target_tree: accesskit::TreeId::ROOT,
            target_node: id,
            data: None,
        },
    )
    .expect("history button publishes a real restore target");
    let _ = f.app.update(message);
    assert_eq!(
        &*f.app.note_editor_view(tab).unwrap().text,
        "one café\nsecond line\n"
    );
    assert_eq!(
        f.app.editor_interaction_snapshot(tab).unwrap().selection,
        (0, 0)
    );
    assert!(!f.app.context_mode.history.open);
    assert_eq!(f.app.context_mode.mode(tab), ContextMode::Typing);
}

#[test]
fn history_page_reports_truncation_after_budget_eviction() {
    // Real Session under a shrunk retained-history budget: enough isolated
    // edits exceed the budget, eviction fires, and the history page reports
    // truncation with the retained watermark (FEUX-0002 X01).
    let (mut f, tab) = fixture(4117);
    run(&mut f.app, tab, Command::Toggle);
    f.app
        .flavite
        .as_ref()
        .unwrap()
        .set_history_budgets_for_test(40_000, 40_000, 1_000);
    let api = f.app.flavite.as_ref().unwrap();
    for _ in 0..90 {
        let view = f.app.note_editor_view(tab).unwrap();
        let result = api.execute(flavite_api::Invocation::new(
            api.command_registry().next_request_id(),
            "apply_edits",
            serde_json::json!({
                "buffer_id": view.buffer_id,
                "revision": view.revision,
                "ops": [{"range": [0, 0], "replacement": "x"}],
                "undo_policy": "isolated",
            }),
            flavite_api::Origin::Gui,
        ));
        assert!(result.is_success(), "{result:?}");
    }
    flavite_ui::host::editor_history::reconcile(&mut f.app);
    let page = f
        .app
        .context_mode
        .history
        .page
        .clone()
        .expect("history page");
    assert!(
        page.history_truncated,
        "budget eviction must surface truncation (entries {})",
        page.entries.len()
    );
    assert!(page.retained_watermark > 0);
    for entry in &page.entries {
        assert!(entry.target.node >= page.retained_watermark);
    }
    assert!(page.entries.iter().any(|entry| entry.current));
}
