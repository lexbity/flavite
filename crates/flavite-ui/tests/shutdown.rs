//! Quit schedule integration (RFC-0003 W12, D-10/D-12).
//!
//! The window close request runs through document decisions, accepted-
//! operation flushes, and the layout flush before the window may exit.
//! Every label is truthful: a timeout never turns pending work into saved
//! work, and quitting with unresolved work is a separate explicit choice.

mod support;

use flavite_editor::{
    EditorAction, EditorInteractionSnapshot, input::CapturedInput, keymap::KeyAction,
};
use flavite_fs::Fs;
use flavite_ui::app::{App, Message};
use flavite_ui::host::closing::CloseDecision;
use flavite_ui::host::dialogs::Dialog;
use flavite_ui::host::shutdown::{ShutdownMessage, ShutdownPlan, ShutdownStage};
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::time::{Duration, Instant};
use support::UiFixture;

fn fixture() -> UiFixture {
    let mut fixture = support::ui_fixture(
        17,
        "/test/shutdown",
        &[support::SeedNote {
            components: &["alpha.md"],
            body: "# Alpha\n\nalpha body\n",
        }],
    );
    // Finish startup restoration before injecting generation faults. A late
    // catalog reply would otherwise load the injected file as startup state.
    pump_until(&mut fixture.app, "startup catalog", |app| {
        matches!(
            app.catalog_status(),
            flavite_ui::app::CatalogStatus::Ready { .. }
        )
    });
    fixture
}

fn wait_ready(app: &mut App) {
    pump_until(app, "note ready", |app| !app.notes.any_pending());
}

fn pump_until(app: &mut App, what: &str, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate(app) {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {}",
            app.status_text()
        );
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Pump until the quit schedule completes; return the one exit task so the
/// caller can drop it without executing it (executing would close a
/// nonexistent window). The Pump handler forwards `shutdown_step`'s exit
/// task, so the task is taken either from an observed ReadyToExit plan or
/// from the pump that ended the schedule (the task is produced exactly
/// once by construction: the ReadyToExit branch clears the plan).
fn pump_to_exit(app: &mut App, what: &str) -> iced::Task<Message> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {}",
            app.status_text()
        );
        if let Some(plan) = app.shutdown.as_ref()
            && plan.stage == ShutdownStage::ReadyToExit
        {
            let exit = app.shutdown_step();
            assert!(app.shutdown.is_none(), "the exit consumes the plan");
            assert!(
                app.shutdown_step().is_none(),
                "the exit task is produced exactly once"
            );
            return exit.expect("ReadyToExit yields the exit task");
        }
        let was_active = app.shutdown.is_some();
        let task = app.update(Message::Pump);
        if was_active && app.shutdown.is_none() {
            assert!(
                app.shutdown_step().is_none(),
                "the exit task is produced exactly once"
            );
            return task;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn open_note(app: &mut App, name: &str) -> TabId {
    let group = app.focused_group().unwrap();
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator: NoteLocator::from_components([name]).unwrap(),
        group,
    });
    wait_ready(app);
    app.workspace.focused_tab()
}

/// Type into the focused editor through the exact capture path the widget
/// uses: one acknowledged interaction, then one captured insert.
fn type_into(app: &mut App, tab: TabId, text: &str) {
    let view = app.note_editor_view(tab).expect("the editor view exists");
    let incarnation = app.notes.incarnation(tab).expect("the view incarnation");
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(EditorInteractionSnapshot {
            sequence: 1,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret: view.text.len(),
            selection: (view.text.len(), view.text.len()),
            viewport_anchor: 0,
            focused: true,
            composing: false,
        }),
    });
    let _ = app.update(Message::EditorInput {
        tab,
        incarnation,
        input: CapturedInput {
            buffer: view.buffer_id,
            observed_revision: view.revision,
            sequence: 2,
            action: KeyAction::Insert,
            text: text.to_owned(),
            extend: false,
            document_boundary: false,
            vertical: None,
        },
    });
}

/// Advance the store's layout generation behind the app's receipt (the
/// honest deterministic stand-in for a refusing store: the next app write
/// is a typed generation mismatch, never a silent overwrite).
/// White-box staging: put the quit schedule into the operation-drain stage
/// with a deadline `ago` in the past (there is no injectable clock).
fn stage_awaiting_operations(app: &mut App, ago: Duration) {
    app.shutdown = Some(ShutdownPlan {
        stage: ShutdownStage::AwaitingOperations {
            started: Instant::now() - ago,
        },
    });
}

fn poison_layout_generation(app: &mut App) {
    let uuid = app.flavite.as_ref().unwrap().vault_ref().uuid.to_string();
    let persisted = flavite_workspace::persist_workspace(&app.workspace, &uuid);
    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_workspace_layout()
        .unwrap();
    let generation = stored.as_ref().map_or(0, |stored| stored.generation);
    app.flavite
        .as_mut()
        .unwrap()
        .write_workspace_layout(generation, &persisted)
        .expect("external layout write");
}

#[test]
fn clean_quit_flushes_layout_and_reports_no_pending_work() {
    let UiFixture {
        mut app,
        fs,
        cache: _cache,
    } = fixture();
    let alpha = open_note(&mut app, "alpha.md");
    // A layout change the quit must flush durably.
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    pump_until(&mut app, "the split", |app| {
        app.workspace.groups().count() == 2
    });

    let _ = app.update(Message::WindowEvent(iced::window::Event::CloseRequested));
    let plan = app.shutdown.as_ref().expect("the quit schedule begins");
    assert_eq!(
        plan.stage,
        ShutdownStage::ReadyToExit,
        "with no dirty notes and no pending operations the schedule is immediate"
    );

    let exit = app.shutdown_step();
    assert!(
        exit.is_some(),
        "ReadyToExit yields the exit task exactly once"
    );
    assert!(app.shutdown.is_none());
    assert!(
        app.shutdown_step().is_none(),
        "no second exit task is produced"
    );

    // The layout flush was durable and reported no failure.
    assert!(app.persistence.last_error().is_none());
    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_workspace_layout()
        .unwrap()
        .expect("the layout file was written");
    assert!(stored.generation >= 1);
    let uuid = app.flavite.as_ref().unwrap().vault_ref().uuid.to_string();
    // stored.workspace is already the PersistedWorkspace
    flavite_workspace::decode_workspace(
        &flavite_workspace::encode_workspace(&stored.workspace).unwrap(),
        &uuid,
    )
        .expect("the persisted layout decodes");

    // Clean note tabs closed; the note bytes on the device are untouched.
    assert!(app.workspace.tab(&alpha).is_none(), "the note tab closed");
    let bytes = fs
        .read_if_exists(&app.root().join("alpha.md"))
        .unwrap()
        .expect("alpha.md exists");
    assert_eq!(bytes, b"# Alpha\n\nalpha body\n");
}

#[test]
fn quit_with_dirty_note_requires_a_decision_first() {
    let UiFixture {
        mut app,
        cache: _cache,
        ..
    } = fixture();
    let alpha = open_note(&mut app, "alpha.md");
    type_into(&mut app, alpha, " unsaved edit");
    assert_eq!(
        app.note_buffer_state(alpha),
        Some(flavite_buffer::state::BufferState::Dirty),
        "the edit dirtied the buffer"
    );
    let revision = app.workspace.revision();

    let _ = app.update(Message::WindowEvent(iced::window::Event::CloseRequested));
    assert_eq!(
        app.shutdown.as_ref().expect("the quit begins").stage,
        ShutdownStage::Documents
    );
    match &app.dialog {
        Some(Dialog::Close { tab }) => {
            assert_eq!(*tab, alpha, "the decision dialog names the dirty note")
        }
        other => panic!("expected the close decision dialog, got {other:?}"),
    }
    assert!(
        app.workspace.tab(&alpha).is_some(),
        "no exit happens while a decision is open"
    );

    // Keep the note open: the quit is cancelled and the workspace returns
    // to its actual current state.
    let _ = app.update(Message::CloseKeepOpen);
    assert!(app.shutdown.is_none(), "keeping the note cancels the quit");
    assert!(app.dialog.is_none());
    assert!(app.workspace.tab(&alpha).is_some(), "the tab is intact");
    assert_eq!(
        app.workspace.revision(),
        revision,
        "the workspace is unchanged"
    );
    assert_eq!(
        app.note_buffer_state(alpha),
        Some(flavite_buffer::state::BufferState::Dirty),
        "the unsaved edit is intact"
    );
}

#[test]
fn quit_after_save_decision_saves_and_exits() {
    let UiFixture {
        mut app,
        fs,
        cache: _cache,
    } = fixture();
    let alpha = open_note(&mut app, "alpha.md");
    type_into(&mut app, alpha, " unsaved edit");

    let _ = app.update(Message::WindowEvent(iced::window::Event::CloseRequested));
    let _ = app.update(Message::CloseDecision(CloseDecision::SaveClose));
    assert!(
        app.pending_operations() >= 1,
        "the save before close is an accepted operation"
    );

    let exit = pump_to_exit(&mut app, "save-then-exit");
    drop(exit);
    assert!(app.shutdown.is_none());
    assert_eq!(
        app.pending_operations(),
        0,
        "the save reached terminal state"
    );
    let bytes = fs
        .read_if_exists(&app.root().join("alpha.md"))
        .unwrap()
        .expect("alpha.md exists");
    assert_eq!(
        bytes, b"# Alpha\n\nalpha body\n unsaved edit",
        "the decision saved the edit to the device"
    );
    assert!(
        app.workspace.tab(&alpha).is_none(),
        "the note closed after saving"
    );
}

#[test]
fn quit_stops_layout_mutations_while_active() {
    let UiFixture {
        mut app,
        cache: _cache,
        ..
    } = fixture();
    let alpha = open_note(&mut app, "alpha.md");
    type_into(&mut app, alpha, " dirty");
    let _ = app.update(Message::WindowEvent(iced::window::Event::CloseRequested));
    assert!(app.shutdown.is_some(), "the quit is active");
    let before = app.workspace.clone();
    let revision = app.workspace.revision();

    // A typed layout action is refused while the quit is being resolved.
    let _ = app.update(Message::Layout(LayoutAction::SplitGroup {
        expected_revision: revision,
        group: app.focused_group().unwrap(),
        arrangement: flavite_workspace::Arrangement::SideBySide,
    }));
    assert!(
        app.status_text().contains("Quitting"),
        "refused with the quitting status: {}",
        app.status_text()
    );
    assert_eq!(app.workspace, before, "the tree is unchanged");
    assert_eq!(app.workspace.revision(), revision);

    // The host command path is refused too.
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    assert!(
        app.status_text().contains("Quitting"),
        "refused with the quitting status: {}",
        app.status_text()
    );
    assert_eq!(app.workspace.revision(), revision);
    let _ = alpha;
}

#[test]
fn layout_flush_failure_offers_quit_with_unsaved_layout() {
    let UiFixture {
        mut app,
        fs,
        cache: _cache,
    } = fixture();
    let alpha = open_note(&mut app, "alpha.md");
    poison_layout_generation(&mut app);

    // Zero pending operations; only the layout flush stands between the
    // schedule and the exit.
    stage_awaiting_operations(&mut app, Duration::ZERO);
    assert_eq!(app.pending_operations(), 0);
    let _ = app.update(Message::Pump);
    let concern = match &app.dialog {
        Some(Dialog::ShutdownTimeout { concern }) => concern.clone(),
        other => panic!("expected the quit-consequence dialog, got {other:?}"),
    };
    assert!(concern.contains("layout"), "{concern}");
    assert!(
        app.shutdown.is_some(),
        "the dialog never exits the window by itself"
    );

    // The notes are unaffected (D-12: independent durability).
    let bytes = fs
        .read_if_exists(&app.root().join("alpha.md"))
        .unwrap()
        .expect("alpha.md exists");
    assert_eq!(bytes, b"# Alpha\n\nalpha body\n");
    assert!(app.workspace.tab(&alpha).is_some());

    // The explicit consequential choice exits with the unsaved layout.
    let _ = app.update(Message::Shutdown(ShutdownMessage::QuitAnyway));
    assert_eq!(
        app.shutdown.as_ref().unwrap().stage,
        ShutdownStage::ReadyToExit
    );
    let exit = pump_to_exit(&mut app, "quit with unsaved layout");
    drop(exit);
    assert!(app.shutdown.is_none());
    assert_eq!(app.pending_operations(), 0, "no note work was pending");
}

#[test]
fn pending_save_timeout_offers_truthful_choices() {
    let UiFixture {
        mut app,
        cache: _cache,
        ..
    } = fixture();
    // One accepted operation that will never reach terminal state in this
    // process (white-box staging: no real operation can be stalled
    // deterministically headlessly).
    app.operations.accept(9999, "fake stalled save");
    assert_eq!(app.pending_operations(), 1);
    stage_awaiting_operations(&mut app, Duration::from_secs(6));

    // The pump offers the typed consequential choice, never an exit.
    let _ = app.update(Message::Pump);
    let concern = match &app.dialog {
        Some(Dialog::ShutdownTimeout { concern }) => concern.clone(),
        other => panic!("expected the timeout dialog, got {other:?}"),
    };
    assert!(
        concern.contains("1 accepted operation(s)"),
        "the concern names the pending count: {concern}"
    );
    assert!(app.shutdown.is_some(), "the timeout never exits by itself");

    // Keep waiting: the deadline resets, the dialog closes.
    let _ = app.update(Message::Shutdown(ShutdownMessage::KeepWaiting));
    assert!(app.dialog.is_none());
    let ShutdownStage::AwaitingOperations { started } =
        app.shutdown.as_ref().expect("still quitting").stage
    else {
        panic!("keep waiting stays in the drain stage");
    };
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the deadline was reset"
    );
    let _ = app.update(Message::Pump);
    assert!(
        app.dialog.is_none(),
        "inside the fresh deadline nothing is offered"
    );

    // Re-trigger the timeout, then take the explicit consequential choice.
    stage_awaiting_operations(&mut app, Duration::from_secs(6));
    let _ = app.update(Message::Pump);
    assert!(matches!(app.dialog, Some(Dialog::ShutdownTimeout { .. })));
    let _ = app.update(Message::Shutdown(ShutdownMessage::QuitAnyway));
    assert_eq!(
        app.shutdown.as_ref().expect("ready to exit").stage,
        ShutdownStage::ReadyToExit
    );
    let exit = pump_to_exit(&mut app, "quit-anyway exit");
    drop(exit);
    assert!(app.shutdown.is_none());
}

#[test]
fn pending_save_timeout_can_be_cancelled() {
    let UiFixture {
        mut app,
        cache: _cache,
        ..
    } = fixture();
    app.operations.accept(9999, "fake stalled save");
    stage_awaiting_operations(&mut app, Duration::from_secs(6));
    let _ = app.update(Message::Pump);
    assert!(matches!(app.dialog, Some(Dialog::ShutdownTimeout { .. })));

    let _ = app.update(Message::Shutdown(ShutdownMessage::Cancel));
    assert!(app.shutdown.is_none(), "the quit is cancelled");
    assert!(app.dialog.is_none());
    assert!(
        app.shutdown_step().is_none(),
        "no exit task after cancellation"
    );
    assert_eq!(
        app.pending_operations(),
        1,
        "cancellation does not fabricate terminal state"
    );
}

#[test]
fn composition_blocks_quit() {
    let UiFixture {
        mut app,
        cache: _cache,
        ..
    } = fixture();
    let alpha = open_note(&mut app, "alpha.md");
    let view = app.note_editor_view(alpha).expect("the editor view");
    let incarnation = app.notes.incarnation(alpha).unwrap();
    let _ = app.update(Message::EditorAction {
        tab: alpha,
        incarnation,
        action: EditorAction::Interaction(EditorInteractionSnapshot {
            sequence: 1,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret: 0,
            selection: (0, 0),
            viewport_anchor: 0,
            focused: true,
            composing: true,
        }),
    });
    let _ = app.update(Message::WindowEvent(iced::window::Event::CloseRequested));
    assert!(
        app.shutdown.is_none(),
        "an active composition refuses to begin the quit"
    );
    assert_eq!(
        app.status_text(),
        "Finish or cancel text composition before quitting"
    );
    assert!(app.workspace.tab(&alpha).is_some());
    assert!(app.dialog.is_none());
}
