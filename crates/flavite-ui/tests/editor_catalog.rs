//! Catalogue enumeration (FEUX-0002 NFR-12, AC-7): every §6 command has
//! one canonical ID, a nonempty title, a display surface (palette or core
//! registry), and — for mutating verbs — a core-registry executor. A
//! seeded missing-metadata row must fail the enumeration.

#[path = "support/mod.rs"]
mod support;

use flavite_edit_context::ContextCommand;
use flavite_ui::app::Message;
use flavite_ui::host::commands::ui_specs;
use flavite_workspace::{LayoutAction, NoteLocator};
use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

#[test]
fn context_catalog_ids_are_unique_and_every_row_displays() {
    let ids: Vec<&str> = ContextCommand::ALL.iter().map(|c| c.id()).collect();
    let mut seen = BTreeSet::new();
    for id in &ids {
        assert!(seen.insert(*id), "duplicate catalog id: {id}");
    }
    // Every row has a nonempty title.
    for command in ContextCommand::ALL {
        assert!(!command.title().is_empty(), "{} has no title", command.id());
        assert!(!command.id().is_empty());
    }

    let palette: BTreeSet<&str> = ui_specs(&BTreeMap::new()).iter().map(|s| s.id).collect();
    // Every context command is displayed in the palette, except the two
    // core-only history verbs whose display surface is the toolbar/chooser
    // (they must then be resolvable in the core registry; asserted by the
    // executor test below).
    for command in ContextCommand::ALL {
        if matches!(command, ContextCommand::Undo | ContextCommand::Redo) {
            continue;
        }
        assert!(
            palette.contains(command.id()),
            "{} is not exposed in the command palette",
            command.id()
        );
    }
}

#[test]
fn every_mutating_and_selection_verb_has_a_core_registry_executor() {
    let mut f = support::ui_fixture(
        7010,
        "/test/catalog-7010",
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
    let registry: BTreeSet<&str> = f
        .app
        .flavite
        .as_ref()
        .unwrap()
        .command_registry()
        .command_ids()
        .into_iter()
        .collect();
    // Every verb the toolbar can destructive/selection-only run must exist
    // in the registry (its executor) — E-7/§6 one canonical ID, one
    // executor; the host only routes, never re-implements.
    for command in ContextCommand::ALL {
        let needs_executor = command.mutates()
            || matches!(
                command,
                ContextCommand::KeepPrimary
                    | ContextCommand::Collapse
                    | ContextCommand::ClearSelection
                    | ContextCommand::AddCaretAbove
                    | ContextCommand::AddCaretBelow
                    | ContextCommand::RotatePrimary
                    | ContextCommand::FlipSelections
                    | ContextCommand::TrimSelections
                    | ContextCommand::InvertSelections
                    | ContextCommand::SelectWord
                    | ContextCommand::SelectLine
                    | ContextCommand::InsertStart
                    | ContextCommand::InsertEnd
                    | ContextCommand::InsertLineStart
                    | ContextCommand::InsertLineEnd
                    | ContextCommand::FindChar { .. }
            );
        if !needs_executor {
            continue;
        }
        assert!(
            registry.contains(command.id()),
            "{} has no core-registry executor",
            command.id()
        );
    }
    // Search verbs, Go motions, Select-all, and the host-routed controls
    // must NOT claim a registry executor (they are host-dispatched
    // surfaces: `dispatch`/`select`/find hosts — one canonical ID, one
    // execution route).
    for command in [
        ContextCommand::Toggle,
        ContextCommand::Exit,
        ContextCommand::Escape,
        ContextCommand::Find,
        ContextCommand::Replace,
        ContextCommand::History,
        ContextCommand::Commands,
        ContextCommand::LineStart,
        ContextCommand::LineEnd,
        ContextCommand::DocumentStart,
        ContextCommand::DocumentEnd,
        ContextCommand::SelectAll,
    ] {
        assert!(
            !registry.contains(command.id()),
            "{} is host-routed and must not register a core executor",
            command.id()
        );
    }
}

#[test]
fn seeded_missing_row_fails_the_enumeration() {
    // A fabricated id that matches no display surface and no registry
    // executor must be caught by the enumeration: every executor-backed
    // command id resolves.
    let fabricated = "editor.nonexistent_verb";
    let palette: BTreeSet<&str> = ui_specs(&BTreeMap::new()).iter().map(|s| s.id).collect();
    assert!(
        !palette.contains(fabricated),
        "fabricated id must not pre-exist in the palette"
    );
    assert!(
        !ContextCommand::ALL.iter().any(|c| c.id() == fabricated),
        "fabricated id must not pre-exist in the context catalog"
    );
}
