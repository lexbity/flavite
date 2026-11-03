//! W10 navigation identity: same titles across folders, missing-link
//! create dialogs, rename collisions, foreign-vault paths, authoritative
//! rename relocation, per-view history bounds, and stale in-tab activation
//! (RFC-0003 §26: external ambiguity is never guessed).

#[path = "../../flavite-api/tests/support/links.rs"]
mod support;

use flavite_ui::app::{App, Message, Picker};
use flavite_ui::host::dialogs::{self, Dialog};
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::time::{Duration, Instant};

fn wait(app: &mut App, done: impl Fn(&App) -> bool) {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let _ = app.update(Message::Pump);
        if done(app) {
            return;
        }
        assert!(Instant::now() < until, "{}", app.status_text());
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn fixture(notes: &[(&str, &str)]) -> App {
    let f = support::fixture(notes);
    App::from_flavite(f.flavite)
}

fn open(app: &mut App, path: &str) -> TabId {
    let locator = NoteLocator::from_components(path.split('/')).unwrap();
    let _ = app.update(Message::OpenNote {
        locator,
        group: None,
    });
    wait(app, |a| !a.notes.any_pending());
    app.workspace.focused_tab()
}

fn note_tab(app: &App, path: &str) -> Option<TabId> {
    let Ok(locator) = NoteLocator::from_components(path.split('/')) else {
        return None;
    };
    app.workspace
        .tabs()
        .find_map(|(tab, session)| {
            matches!(session.kind(), flavite_workspace::ViewKind::Note(note)
                if note.locator() == &locator)
            .then_some(tab)
        })
        .copied()
}

#[test]
fn same_titled_notes_stay_distinct_by_native_path() {
    let mut app = fixture(&[
        ("2024/journal.md", "2024 entry\n"),
        ("2025/journal.md", "2025 entry\n"),
    ]);
    wait(&mut app, |a| a.note_candidates().all().len() == 2);
    // The matcher surfaces both; identity is the native path, never a stem.
    let matched = app.note_candidates().matches("journal");
    assert_eq!(matched.len(), 2);
    let mut paths: Vec<String> = matched
        .iter()
        .map(|candidate| candidate.path().to_string())
        .collect();
    paths.sort();
    assert_eq!(paths, ["2024/journal.md", "2025/journal.md"]);

    // Opening each opens distinct tabs with distinct content.
    let first = open(&mut app, "2024/journal.md");
    let second = open(&mut app, "2025/journal.md");
    assert_ne!(first, second);
    let text = |app: &App, tab: TabId| {
        app.note_editor_view(tab)
            .map(|view| view.text.to_string())
            .unwrap_or_default()
    };
    assert!(text(&app, first).contains("2024"));
    assert!(text(&app, second).contains("2025"));
}

#[test]
fn in_tab_navigation_to_same_note_is_rejected_without_state_change() {
    let mut app = fixture(&[("a.md", "body\n")]);
    let tab = open(&mut app, "a.md");
    let revision = app.workspace.revision();
    app.dispatch_layout(|revision| LayoutAction::NavigateTab {
        expected_revision: revision,
        tab,
        locator: NoteLocator::from_components(["a.md"]).unwrap(),
    });
    // NoChange: the transaction is rejected and the revision is untouched.
    assert_eq!(app.workspace.revision(), revision);
}

#[test]
fn per_view_history_is_bounded() {
    let mut app = fixture(&[("a.md", "a\n"), ("b.md", "b\n"), ("c.md", "c\n")]);
    let tab = open(&mut app, "a.md");
    let anchor = app
        .workspace
        .tab(&tab)
        .map(|session| session.interaction().viewport_anchor);
    // Walk more replacements than the bound in one tab.
    let notes = ["b.md", "c.md", "b.md", "c.md", "b.md"];
    for (index, name) in notes.iter().enumerate() {
        let previous = app.note_locator(tab).unwrap();
        let next = NoteLocator::from_components(name.split('/')).unwrap();
        let committed = app.dispatch_layout_result(|revision| LayoutAction::NavigateTab {
            expected_revision: revision,
            tab,
            locator: next.clone(),
        });
        assert!(committed);
        app.nav.record_replacement(tab, previous, anchor, next);
        wait(&mut app, |a| {
            a.note_locator(tab).is_some_and(|locator| {
                locator == NoteLocator::from_components(name.split('/')).unwrap()
            }) && a.note_editor_view(tab).is_some()
        });
        let _ = index;
    }
    // The bound holds; the deepest history still resolves to the first note.
    for _ in 0..(flavite_ui::host::navigation::MAX_NAV_ENTRIES + 10) {
        if !app.nav.can_go_back(tab) {
            break;
        }
        let entry = app.nav.go_back(tab, None).expect("back entry");
        let committed = app.dispatch_layout_result(|revision| LayoutAction::NavigateTab {
            expected_revision: revision,
            tab,
            locator: entry.locator.clone(),
        });
        assert!(committed);
        wait(&mut app, |a| !a.notes.any_pending());
    }
    assert!(!app.nav.can_go_back(tab), "history must be bounded");
    assert_eq!(
        app.note_locator(tab).unwrap(),
        NoteLocator::from_components(["a.md"]).unwrap(),
        "the deepest back entry is the original note"
    );
}

#[test]
fn foreign_vault_paths_are_refused_not_guessed() {
    let mut app = fixture(&[("a.md", "body\n")]);
    let foreign = std::path::PathBuf::from("/elsewhere/vault/a.md");
    app.open_picked_note(Some(foreign));
    assert!(
        app.status_text().contains("outside this vault"),
        "status: {}",
        app.status_text()
    );
    assert!(note_tab(&app, "a.md").is_none());
}

#[test]
fn rename_collision_is_refused_by_validation() {
    let mut app = fixture(&[("a.md", "a\n"), ("b.md", "b\n")]);
    let _tab = open(&mut app, "a.md");
    // Destination exists: the dialog validation refuses submit.
    let dialog = Dialog::Rename {
        from: NoteLocator::from_components(["a.md"]).unwrap(),
        to: "b.md".into(),
        tier: None,
    };
    app.dialog = Some(dialog);
    let revision = app.workspace.revision();
    let _ = app.update(Message::DialogSubmit);
    // The dialog stays open with the typed error; no rename was dispatched.
    assert!(app.dialog.is_some());
    assert_eq!(app.workspace.revision(), revision);
    assert!(note_tab(&app, "a.md").is_some());
    assert!(
        app.note_candidates()
            .all()
            .iter()
            .any(|candidate| candidate.path().to_string() == "b.md"),
        "the destination note is untouched"
    );
}

#[test]
fn authoritative_rename_relocates_views_bindings_and_history() {
    let mut app = fixture(&[("a.md", "a\n"), ("folder/b.md", "b\n")]);
    let tab = open(&mut app, "folder/b.md");
    // Build nav history in this tab, then rename the current note.
    app.picker = Picker {
        open: true,
        query: String::new(),
        selected: 0,
        replace_tab: Some(tab),
    };
    let _ = app.update(Message::PickerQuery("a".into()));
    let _ = app.update(Message::PickerSubmit);
    wait(&mut app, |a| !a.notes.any_pending());
    assert!(app.nav.can_go_back(tab));

    // Authoritative core rename (T0–T2): tabs, pinned bindings, nav entries,
    // and history views adopt the new identity.
    let flavite = app.flavite.as_ref().unwrap();
    let request_id = flavite.command_registry().next_request_id();
    let invocation = flavite_api::command::Invocation::new(
        request_id,
        "note.rename",
        serde_json::json!({
            "from": app.root().join("folder/b.md"),
            "to": app.root().join("folder/moved.md"),
            "strategy": "rewrite",
        }),
        flavite_api::Origin::Gui,
    );
    match flavite.execute(invocation) {
        flavite_api::ExecutionResult::Pending { request_id } => {
            let completion = flavite.wait_for_operation(request_id);
            if let flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) =
                &completion
            {
                app.route_completion(flavite_api::OperationCompletion {
                    request_id,
                    result: flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(
                        success.clone(),
                    )),
                });
            } else {
                panic!("rename did not succeed: {completion:?}");
            }
        }
        other => panic!("rename not admitted: {other:?}"),
    }
    wait(&mut app, |a| {
        a.note_candidates()
            .all()
            .iter()
            .any(|candidate| candidate.path().to_string() == "folder/moved.md")
    });
    // No tab shows the dead path; the recorded history adopts the rename
    // instead (this tab was replaced by an earlier in-tab navigation).
    assert!(note_tab(&app, "folder/b.md").is_none());
    // History entries recorded the old locator now resolve to the new one.
    if std::env::var("FLAVITE_DEBUG_RENAME").is_ok() {
        eprintln!("can_go_back={}", app.nav.can_go_back(tab));
        eprintln!("focused={:?}", app.focused_note_tab());
        eprintln!("status_before_back={}", app.status_text());
    }
    let _ = app.update(Message::NavigateBack);
    wait(&mut app, |a| !a.notes.any_pending());
    if std::env::var("FLAVITE_DEBUG_RENAME").is_ok() {
        eprintln!("status_after_back={}", app.status_text());
        eprintln!("locator={:?}", app.note_locator(tab));
    }
    assert_eq!(
        app.note_locator(tab).unwrap(),
        NoteLocator::from_components(["folder", "moved.md"]).unwrap(),
        "back navigation lands on the renamed identity, never the dead path"
    );
}

#[test]
fn create_dialog_offers_canonical_path_and_survives_collision() {
    let mut app = fixture(&[("a.md", "a\n")]);
    // Unresolved-link activation pre-fills the canonical .md path.
    let dialog = dialogs::offer_create_for_link("systems/feedback");
    match &dialog {
        Dialog::NewNote { path } => assert_eq!(path, "systems/feedback.md"),
        other => panic!("expected create dialog, got {other:?}"),
    }
    app.dialog = Some(dialog);
    let _ = app.update(Message::DialogSubmit);
    wait(&mut app, |a| {
        note_tab(a, "systems/feedback.md").is_some() && !a.notes.any_pending()
    });
    // A second submit of the same path collides and refuses honestly.
    app.dialog = Some(Dialog::NewNote {
        path: "systems/feedback.md".into(),
    });
    let before = note_tab(&app, "systems/feedback.md");
    let _ = app.update(Message::DialogSubmit);
    wait(&mut app, |a| a.status_text().contains("already exists"));
    assert_eq!(note_tab(&app, "systems/feedback.md"), before);
}
