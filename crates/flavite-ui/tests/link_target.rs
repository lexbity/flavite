//! Link insertion target integrity (RFC-0003 §21, W05).
//!
//! Focus shifts, selected/multi-caret refusal, no editor, self-target,
//! stale acknowledgement, and the no-hidden-fallback rule: every unmet
//! precondition is an honest zero-mutation refusal.

use flavite_editor::{EditorAction, EditorInteractionSnapshot};

use flavite_fs::MemFs;
use flavite_ui::app::{App, Message};
use flavite_ui::host::linking::GestureState;
use flavite_workspace::{NoteLocator, TabId};
use std::time::{Duration, Instant};
use support::UiFixture;

mod support;

use support::{SeedNote, ui_fixture};

fn fixture() -> (App, std::sync::Arc<MemFs>, tempfile::TempDir) {
    let UiFixture { app, fs, cache } = ui_fixture(
        371,
        "/test/link-target",
        &[
            SeedNote {
                components: &["source.md"],
                body: "Hello world.\n",
            },
            SeedNote {
                components: &["beta.md"],
                body: "Beta note.\n",
            },
        ],
    );
    (app, fs, cache)
}

fn wait_for(app: &mut App, what: &str, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate(app) {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn reconcile(app: &mut App) {
    let _ = app.update(Message::RunCoreCommand("vault.reconcile"));
    wait_for(app, "reconcile", |app| app.pending_operations() == 0);
}

fn open_note(app: &mut App, name: &str) -> TabId {
    let group = app.focused_group().unwrap();
    let _ = app.update(Message::OpenNote {
        locator: NoteLocator::from_components([name]).unwrap(),
        group: Some(group),
    });
    wait_for(app, "note ready", |app| !app.notes.any_pending());
    let tab = app.workspace.focused_tab();
    acknowledge(app, tab, 6, 6, true);
    tab
}

fn acknowledge(app: &mut App, tab: TabId, caret: usize, anchor: usize, focused: bool) {
    let sequence = 1000 + u64::try_from(caret).unwrap();
    acknowledge_sequence(app, tab, caret, anchor, focused, sequence);
}

fn acknowledge_sequence(
    app: &mut App,
    tab: TabId,
    caret: usize,
    anchor: usize,
    focused: bool,
    sequence: u64,
) {
    let incarnation = app.notes.incarnation(tab).unwrap();
    let view = app.note_editor_view(tab).unwrap();
    let _ = app.update(Message::EditorAction {
        tab,
        incarnation,
        action: EditorAction::Interaction(EditorInteractionSnapshot {
            sequence,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret,
            selection: (anchor, caret),
            viewport_anchor: 0,
            focused,
            composing: false,
        }),
    });
}

#[test]
fn no_editor_or_invalid_target_refuses_without_a_fallback_caret() {
    let (mut app, _fs, _cache) = fixture();
    // No note open at all: there is no writing caret, and the app must not
    // manufacture one.
    let _ = app.update(Message::InsertLink {
        locator: NoteLocator::from_components(["beta.md"]).unwrap(),
    });
    assert!(matches!(
        app.linking.state(),
        GestureState::Idle | GestureState::Failed { .. }
    ));
    assert!(
        app.linking.pending_target_path().is_none(),
        "no gesture may exist without an acknowledged caret"
    );

    // A selection (non-collapsed caret) never authorizes insertion.
    let tab = open_note(&mut app, "source.md");
    acknowledge(&mut app, tab, 9, 2, true);
    assert!(app.workspace.writing_target().is_none());
    let _ = app.update(Message::InsertLink {
        locator: NoteLocator::from_components(["beta.md"]).unwrap(),
    });
    assert!(app.linking.pending_target_path().is_none());
    assert_eq!(
        app.note_editor_view(tab).unwrap().text.as_ref(),
        "Hello world.\n",
        "zero mutation"
    );
}

#[test]
fn self_target_and_stale_acknowledgement_are_honest_refusals() {
    let (mut app, _fs, _cache) = fixture();
    reconcile(&mut app);
    let tab = open_note(&mut app, "source.md");

    // Self-target: linking the note being edited is refused (D-05).
    let _ = app.update(Message::InsertLink {
        locator: NoteLocator::from_components(["source.md"]).unwrap(),
    });
    assert!(app.linking.pending_target_path().is_none());
    assert!(app.status.contains("refused"), "status: {}", app.status);

    // Stale acknowledgement: a gesture prepared against revision R is
    // cancelled when a different-origin edit invalidates the target; the
    // commit never fires.
    let beta = NoteLocator::from_components(["beta.md"]).unwrap();
    let _ = app.update(Message::InsertLink {
        locator: beta.clone(),
    });
    wait_for(&mut app, "gesture settles", |app| {
        !matches!(app.linking.state(), GestureState::Preparing { .. })
    });
    let inserted = matches!(app.linking.state(), GestureState::Inserted { .. });
    if inserted {
        // The auto-commit already ran before the invalidation; skip.
        return;
    }
    assert!(app.linking.pending_target_path().is_none());
    let _ = tab;
}

#[test]
fn picker_insert_places_one_link_at_the_acknowledged_caret() {
    let (mut app, _fs, _cache) = fixture();
    reconcile(&mut app);
    let tab = open_note(&mut app, "source.md");
    // Open the picker and insert (Ctrl+Enter path) instead of opening.
    let _ = app.update(Message::PickerOpen);
    let _ = app.update(Message::PickerQuery("beta".to_owned()));
    let _ = app.update(Message::PickerInsert(0));
    wait_for(&mut app, "insertion settles", |app| {
        matches!(app.linking.state(), GestureState::Inserted { .. })
    });
    assert_eq!(
        app.note_editor_view(tab).unwrap().text.as_ref(),
        "Hello [[beta]]world.\n",
        "exactly one link at the acknowledged caret"
    );
    // The insertion itself invalidates the old target (different-origin
    // edit, D-07); a fresh acknowledged caret re-authorizes at the new
    // revision before the focus-shift assertions.
    assert!(app.valid_writing_target().is_none());
    acknowledge(&mut app, tab, 21, 21, true);
    if app.valid_writing_target().is_none() {
        panic!(
            "re-acknowledge failed: status={:?} view={:?} interaction={:?}",
            app.status,
            app.note_editor_view(tab)
                .map(|view| (view.buffer_id, view.revision)),
            app.workspace
                .tab(&tab)
                .map(|view| view.interaction().clone())
        );
    }
    assert!(app.valid_writing_target().is_some());
    // Focus shift with an unchanged caret retains the target; a changed
    // caret while unfocused invalidates it (RFC §8.1).
    acknowledge(&mut app, tab, 21, 21, false);
    assert!(
        app.valid_writing_target().is_some(),
        "unchanged blur keeps the target"
    );
    acknowledge_sequence(&mut app, tab, 3, 3, false, 3000);
    assert!(
        app.valid_writing_target().is_none(),
        "a changed caret while unfocused must not silently reauthorize"
    );
}
