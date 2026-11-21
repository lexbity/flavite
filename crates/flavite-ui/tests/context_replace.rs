//! FEUX-0006 X03 UI integration: the replace-regex draft computes an exact
//! live preview, Enter commits through the editor kernel with recorded
//! before/after sets, and a read-only vault keeps the preview but refuses
//! the commit with the draft open.

#[path = "support/mod.rs"]
mod support;

use flavite_edit_context::ContextCommand as Command;
use flavite_ui::app::{App, Message};
use flavite_ui::host::command_args::CommandArgsMessage;
use flavite_ui::host::context_mode::{ContextMessage, EditorTarget};
use flavite_workspace::{LayoutAction, NoteLocator, TabId};
use std::sync::Arc;
use std::time::{Duration, Instant};

const BODY: &str = "one 1\ntwo 2\nthree 3\n";

/// Install the real regex helper beside this test binary (the exact
/// Install the real regex helper beside this test binary (the exact
/// discovery rule: `current_exe().parent()`), building it into a scratch
/// target that cannot contend with the outer cargo's lock.
fn install_helper_silently() {
    use std::path::PathBuf;
    use std::process::Command;
    let exe = std::env::current_exe().expect("current_exe");
    let sibling = exe
        .parent()
        .expect("parent dir")
        .join("flavite-text-search-worker");
    if sibling.is_file() {
        return;
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf();
    let scratch =
        std::env::temp_dir().join(format!("flavite-ui-worker-build-{}", std::process::id()));
    std::fs::create_dir_all(&scratch).expect("scratch target dir");
    let output = Command::new(env!("CARGO"))
        .args([
            "build",
            "-p",
            "flavite-text-search-worker",
            "--bin",
            "flavite-text-search-worker",
        ])
        .env("CARGO_TARGET_DIR", &scratch)
        .current_dir(&root)
        .output()
        .expect("cargo build runs");
    assert!(
        output.status.success(),
        "helper build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let candidates = [
        scratch.join("debug/flavite-text-search-worker"),
        scratch.join("debug/deps/flavite-text-search-worker"),
    ];
    let built = candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .expect("helper binary not found after build");
    std::fs::copy(built, &sibling).expect("install helper beside the test binary");
}

/// Hold the helper installed for the whole test and restore the no-helper
/// baseline on drop (mirrors the vault `text_worker` integration harness;
/// the sibling path is shared by every integration-test executable in
/// target/debug/deps).
struct HelperGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Drop for HelperGuard {
    fn drop(&mut self) {
        let exe = std::env::current_exe().expect("current_exe");
        let sibling = exe
            .parent()
            .expect("parent dir")
            .join("flavite-text-search-worker");
        let _ = std::fs::remove_file(sibling);
    }
}

fn helper_guard() -> HelperGuard {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let guard = LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    install_helper_silently();
    HelperGuard { _guard: guard }
}

fn fixture(seed: u64) -> (support::UiFixture, TabId) {
    let mut f = support::ui_fixture(
        seed,
        &format!("/test/context-replace-{seed}"),
        &[support::SeedNote {
            components: &["note.md"],
            body: BODY,
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
    let _ = f.app.update(Message::AccessibleSelection {
        tab,
        incarnation: f.app.notes.incarnation(tab).unwrap(),
        revision: f.app.note_editor_view(tab).unwrap().revision,
        anchor: 0,
        head: 0,
    });
    (f, tab)
}

fn run(app: &mut App, tab: TabId, command: Command) {
    let target = EditorTarget::capture(app, tab).unwrap();
    let _ = app.update(Message::ContextMode(ContextMessage::Run(target, command)));
}

fn paste_field(app: &mut App, tab: TabId, text: &str) {
    let _ = app.update(Message::CommandArgs(CommandArgsMessage::DraftAction {
        tab,
        action: iced::widget::text_editor::Action::Edit(iced::widget::text_editor::Edit::Paste(
            Arc::new(text.to_string()),
        )),
    }));
}

fn pump_until(app: &mut App, predicate: impl Fn(&App) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(8);
    while !predicate(app) {
        assert!(
            Instant::now() < deadline,
            "pump deadline: {}",
            app.status_text()
        );
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(3));
    }
}

/// Pump until the replace preview satisfies `done`. The regex helper binary
/// is shared with the vault integration harness (same deps directory): when
/// the vault restores its no-helper baseline mid-run, the preview refuses
/// with `regex is unavailable`, and the app-wide helper permit pool can
/// transiently report `busy` — reinstall/re-fire so the test self-heals
/// instead of flaking.
fn pump_preview(
    app: &mut App,
    tab: TabId,
    done: impl Fn(&flavite_ui::host::command_args::ReplacePreview) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let parsed = app
            .command_args
            .tabs
            .get(&tab)
            .and_then(|state| state.preview.as_ref());
        if let Some(preview) = parsed
            && done(preview)
        {
            return;
        }
        let transient = app
            .command_args
            .tabs
            .get(&tab)
            .and_then(|state| state.state_error.as_ref())
            .is_some_and(|error| error.contains("regex is unavailable") || error.contains("busy"));
        assert!(
            Instant::now() < deadline,
            "preview pump deadline: {}",
            app.status_text()
        );
        if transient {
            install_helper_silently();
            let template = app
                .command_args
                .tabs
                .get(&tab)
                .and_then(|state| state.fields.get(1))
                .map(|field| field.text.clone())
                .unwrap_or_default();
            paste_field(app, tab, &template);
        }
        let _ = app.update(Message::Pump);
        std::thread::sleep(Duration::from_millis(3));
    }
}

#[test]
fn replace_regex_previews_exact_totals_and_commits_through_the_kernel() {
    let _helper = helper_guard();
    let (mut f, tab) = fixture(9901);
    let revision_before = f.app.note_editor_view(tab).unwrap().revision;
    run(&mut f.app, tab, Command::ReplaceRegex);

    // Pattern field (active) then template field.
    paste_field(&mut f.app, tab, r"\d+");
    let _ = f
        .app
        .update(Message::CommandArgs(CommandArgsMessage::NextField(tab)));
    paste_field(&mut f.app, tab, "[$0]");

    // Live preview: exact totals from replace_plan (compute-only). The
    // pattern paste previews with the (empty) template; the final preview
    // reflects the template field too.
    pump_preview(&mut f.app, tab, |preview| {
        preview.match_count == 3 && preview.output_len > BODY.len() as u64
    });
    let preview = f
        .app
        .command_args
        .tabs
        .get(&tab)
        .unwrap()
        .preview
        .clone()
        .expect("preview");
    assert_eq!(preview.match_count, 3);
    assert!(
        preview.output_len > BODY.len() as u64,
        "brackets widen the text"
    );
    assert_eq!(preview.spans.len(), 3);

    // The preview never mutates.
    assert_eq!(
        f.app
            .note_editor_view(tab)
            .unwrap()
            .text
            .materialize()
            .as_ref(),
        BODY
    );

    // Enter: fresh plan → kernel commit → draft closes, text replaces.
    let _ = f
        .app
        .update(Message::CommandArgs(CommandArgsMessage::Apply(tab)));
    pump_until(&mut f.app, |app| !app.command_args.tabs.contains_key(&tab));
    assert_eq!(
        f.app
            .note_editor_view(tab)
            .unwrap()
            .text
            .materialize()
            .as_ref(),
        "one [1]\ntwo [2]\nthree [3]\n"
    );
    let view = f.app.note_editor_view(tab).unwrap();
    assert!(view.revision > revision_before, "commit bumps the revision");
}

#[test]
fn replace_regex_preview_works_in_a_read_only_vault_and_commit_is_refused() {
    let _helper = helper_guard();
    let (mut f, tab) = fixture(9902);
    f.app.flavite.as_mut().unwrap().set_read_only(true);

    run(&mut f.app, tab, Command::ReplaceRegex);
    paste_field(&mut f.app, tab, r"\d+");
    let _ = f
        .app
        .update(Message::CommandArgs(CommandArgsMessage::NextField(tab)));
    paste_field(&mut f.app, tab, "[$0]");

    // The preview still computes (text.replace_plan is ReadNotes).
    pump_preview(&mut f.app, tab, |_| true);
    assert_eq!(
        f.app
            .note_editor_view(tab)
            .unwrap()
            .text
            .materialize()
            .as_ref(),
        BODY
    );

    // Enter refuses with the vault state; the draft stays open and the text
    // is untouched.
    let _ = f
        .app
        .update(Message::CommandArgs(CommandArgsMessage::Apply(tab)));
    pump_until(&mut f.app, |app| {
        app.command_args
            .tabs
            .get(&tab)
            .is_some_and(|state| state.state_error.is_some())
    });
    assert!(
        f.app.command_args.tabs.contains_key(&tab),
        "the draft stays open on refusal"
    );
    assert_eq!(
        f.app
            .note_editor_view(tab)
            .unwrap()
            .text
            .materialize()
            .as_ref(),
        BODY
    );
}
