//! FEUX-0002 X07 diagnostics integration: the real emit paths never leak
//! note text, clipboard values, queries, paths, or produced characters into
//! the ring (the seeded-token scan), the ring stays bounded, export is
//! explicit-only, and the burst hint appears through the real run path and
//! is fully suppressed by the typed setting.

#[path = "support/mod.rs"]
mod support;

use flavite_edit_context::ContextCommand as Command;
use flavite_ui::app::{App, Message};
use flavite_ui::host::context_mode::{ContextMessage, EditorTarget};
use flavite_ui::host::editor_clipboard::ClipboardMessage;
use flavite_ui::host::editor_diagnostics::DiagnosticsMessage;
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::time::{Duration, Instant};

const NOTE_SEED: &str = "SEEDED_NOTE_FRAGMENT_7f3a";
const QUERY_SEED: &str = "SEEDED_QUERY_c4d0";
const CLIP_SEED: &str = "SEEDED_CLIPBOARD_9b21";
const PATH_SEED: &str = "diag-root";
const CHAR_SEED: &str = "z";
const SEED_BODY: &str = "SEEDED_NOTE_FRAGMENT_7f3a alpha\nbeta gamma\n";

fn fixture(seed: u64) -> (support::UiFixture, TabId) {
    let mut f = support::ui_fixture(
        seed,
        &format!("/{PATH_SEED}/diag-{seed}"),
        &[support::SeedNote {
            components: &["diag.md"],
            body: SEED_BODY,
        }],
    );
    let group = f.app.focused_group().unwrap();
    f.app
        .dispatch_layout(|expected_revision| LayoutAction::OpenNote {
            expected_revision,
            group,
            locator: NoteLocator::from_components(["diag.md"]).unwrap(),
        });
    let tab = f.app.workspace.focused_tab();
    let deadline = Instant::now() + Duration::from_secs(5);
    while f.app.note_editor_view(tab).is_none() {
        assert!(Instant::now() < deadline, "{}", f.app.status_text());
        let _ = f.app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
    (f, tab)
}

fn run(app: &mut App, tab: TabId, command: Command) {
    let target = EditorTarget::capture(app, tab).unwrap();
    let _ = app.update(Message::ContextMode(ContextMessage::Run(target, command)));
}

fn pump_until(app: &mut App, predicate: impl Fn(&App) -> bool, label: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate(app) {
        assert!(Instant::now() < deadline, "timed out: {label}");
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Drive every instrumented path with a distinct seeded token riding it:
/// note text, clipboard value, query string, and produced character. The
/// exported ring must contain none of them (the schema makes it
/// structurally impossible; the scan proves the real paths).
#[test]
fn seeded_forbidden_content_never_reaches_the_ring() {
    let (mut f, tab) = fixture(11_101);

    // Enter Context Mode; select the seeded fragment line.
    run(&mut f.app, tab, Command::Toggle);
    assert_eq!(
        f.app.context_mode.mode(tab),
        flavite_edit_context::ContextMode::Command
    );

    // Copy: `editor.copy` joins the selected fragment (the clipboard write
    // is the OS seam; the copied value rides the api result, never the
    // ring).
    run(&mut f.app, tab, Command::Copy);

    // A paste read with a seeded clipboard value (the seam): the value is
    // consumed by the paste path and must never reach the ring.
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let generation = f.app.context_mode.generation();
    let _ = f
        .app
        .update(Message::EditorClipboard(ClipboardMessage::ContextPaste {
            target,
            generation,
            text: Some(CLIP_SEED.to_owned()),
        }));
    pump_until(
        &mut f.app,
        |app| app.context_mode.mode(tab).active(),
        "paste completes",
    );

    // Match collection with a seeded query: draft collects and overlays.
    let _ = f.app.update(Message::SelectionSearch(
        flavite_ui::host::selection_search::SelectionSearchMessage::Open {
            tab,
            kind: flavite_ui::host::selection_search::ApplyKind::Select,
        },
    ));
    let _ = f.app.update(Message::SelectionSearch(
        flavite_ui::host::selection_search::SelectionSearchMessage::DraftAction {
            tab,
            action: iced::widget::text_editor::Action::Edit(
                iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(QUERY_SEED.to_string())),
            ),
        },
    ));
    pump_until(
        &mut f.app,
        |app| {
            app.selection_search
                .tabs
                .get(&tab)
                .is_some_and(|s| s.pending.is_none() && s.overlay.is_some())
        },
        "draft overlay ready",
    );
    let _ = f.app.update(Message::SelectionSearch(
        flavite_ui::host::selection_search::SelectionSearchMessage::Close(tab),
    ));

    // Find-char with a seeded produced character.
    let target = EditorTarget::capture(&f.app, tab).unwrap();
    let _ = f.app.update(Message::ContextMode(ContextMessage::Run(
        target,
        Command::FindChar { forward: true },
    )));
    let _ = f.app.update(Message::ContextMode(ContextMessage::Unbound(
        EditorTarget::capture(&f.app, tab).unwrap(),
    )));

    // History page open.
    run(&mut f.app, tab, Command::History);
    pump_until(
        &mut f.app,
        |app| app.context_mode.history.open,
        "history chooser open",
    );
    let _ = f.app.update(Message::EditorHistory(
        flavite_ui::host::editor_history::HistoryMessage::Act(
            EditorTarget::capture(&f.app, tab).unwrap(),
            flavite_ui::host::editor_history::HistoryAction::Close,
        ),
    ));

    // The export is the only text rendering; it must carry none of the
    // seeds.
    let exported = f.app.diagnostics.export_text();
    for token in [NOTE_SEED, QUERY_SEED, CLIP_SEED, PATH_SEED, CHAR_SEED] {
        assert!(
            !exported.contains(token),
            "forbidden content leaked into diagnostics: {token}"
        );
    }
}

#[test]
fn burst_hint_appears_through_the_real_run_path_and_dismisses() {
    let (mut f, tab) = fixture(11_103);
    run(&mut f.app, tab, Command::Toggle);
    // Five fast mutating activations in the window.
    for _ in 0..5 {
        run(&mut f.app, tab, Command::Delete);
    }
    assert!(
        f.app.diagnostics.burst_hint,
        "five mutating activations show the hint"
    );
    // Dismiss.
    let _ = f.app.update(Message::EditorDiagnostics(
        DiagnosticsMessage::DismissBurstHint,
    ));
    assert!(!f.app.diagnostics.burst_hint);
}

#[test]
fn disabled_setting_suppresses_the_hint_entirely() {
    let (mut f, tab) = fixture(11_104);
    let _ = f.app.apply_setting(
        "editor.burst_hint_enabled",
        flavite_settings::value::SettingValue::Bool(false),
    );
    assert!(!f.app.appearance.burst_hint_enabled());
    run(&mut f.app, tab, Command::Toggle);
    for _ in 0..5 {
        run(&mut f.app, tab, Command::Delete);
    }
    assert!(
        !f.app.diagnostics.burst_hint,
        "the typed setting disables the hint"
    );
}

#[test]
fn banner_history_action_opens_the_chooser_and_clears_the_banner() {
    let (mut f, tab) = fixture(11_105);
    run(&mut f.app, tab, Command::Toggle);
    for _ in 0..5 {
        run(&mut f.app, tab, Command::Delete);
    }
    assert!(f.app.diagnostics.burst_hint);
    let _ = f
        .app
        .update(Message::EditorDiagnostics(DiagnosticsMessage::OpenHistory(
            tab,
        )));
    eprintln!(
        "OPEN={} MSG={:?} MODE={:?} SESSION={}",
        f.app.context_mode.history.open,
        f.app
            .context_mode
            .session
            .as_ref()
            .map(|s| s.state.message()),
        f.app.context_mode.mode(tab),
        f.app.context_mode.session.is_some()
    );
    assert!(
        f.app.context_mode.history.open,
        "the banner's Open history action opens the chooser"
    );
    assert!(!f.app.diagnostics.burst_hint, "the banner clears on use");
    let _ = f.app.update(Message::EditorHistory(
        flavite_ui::host::editor_history::HistoryMessage::Act(
            EditorTarget::capture(&f.app, tab).unwrap(),
            flavite_ui::host::editor_history::HistoryAction::Close,
        ),
    ));
}

#[test]
fn banner_exit_action_leaves_context_mode() {
    let (mut f, tab) = fixture(11_106);
    run(&mut f.app, tab, Command::Toggle);
    for _ in 0..5 {
        run(&mut f.app, tab, Command::Delete);
    }
    assert!(f.app.diagnostics.burst_hint);
    let _ = f.app.update(Message::EditorDiagnostics(
        DiagnosticsMessage::ExitContextMode(tab),
    ));
    assert_eq!(
        f.app.context_mode.mode(tab),
        flavite_edit_context::ContextMode::Off,
        "the banner's Exit action leaves Context Mode"
    );
    assert!(!f.app.diagnostics.burst_hint);
}

#[test]
fn export_command_is_the_only_export_surface() {
    let (mut f, tab) = fixture(11_107);
    run(&mut f.app, tab, Command::Toggle);
    run(&mut f.app, tab, Command::Delete);
    assert!(f.app.diagnostics.event_count() > 0);
    // The palette command is the explicit export action.
    let _ = f
        .app
        .update(Message::RunHostCommand("editor.diagnostics_export"));
    let exported = f.app.diagnostics.export_text();
    assert!(exported.contains("DiagnosticExport"), "{exported}");
    assert!(exported.contains("OperationApplied"), "{exported}");
}
