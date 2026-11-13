//! FEUX-0002 §11: negative fixtures for the cumulative FEUX source gates.
//! The tests seed a hermetic temp-tree repository (every presence file, with
//! tiny content carrying the required patterns) and drive the real checker
//! against it via the repository-root argument. `rg` exit 2 (missing search
//! target) must fail, never pass.

use std::fs;
use std::path::Path;
use std::process::Command;

fn run_gate(root: &Path, slice: &str) -> std::process::Output {
    Command::new("bash")
        .arg(format!(
            "{}/../scripts/check-feux-contracts.sh",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(slice)
        .arg(root)
        .output()
        .expect("run feux gate")
}

fn seed_file(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).expect("required parent");
    fs::write(path, content).expect("required file");
}

fn seed_x01_inputs(root: &Path) {
    // The checker sources the gate library relative to the repository root.
    let lib = root.join("scripts/lib/source-gates.sh");
    fs::create_dir_all(lib.parent().unwrap()).expect("lib parent");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    fs::copy(repo.join("scripts/lib/source-gates.sh"), &lib).expect("copy gate library");

    seed_file(
        root,
        "scripts/check-feux-contracts.sh",
        "#!/usr/bin/env bash\n",
    );
    seed_file(root, "xtask/tests/feux_gates.rs", "// gate fixtures\n");
    seed_file(
        root,
        "scripts/tests/workbench/context_mode_budget.py",
        "import common\n",
    );
    seed_file(
        root,
        "scripts/workbench-visual-linux.sh",
        "#!/usr/bin/env bash\n",
    );
    // Absence-gate targets: budget.rs must contain no TODO; undo.rs must
    // carry the reservation/eviction/truncation plumbing; session.rs must
    // carry the app-wide ceiling; model.rs the typed refusal.
    seed_file(
        root,
        "crates/flavite-buffer/src/budget.rs",
        "pub const MAX: u64 = 1;\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/src/undo.rs",
        "fn reserve() {}\nfn evict_to_fit() {}\nfn history_truncated() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/src/history.rs",
        "pub enum HistoryError { HistoryChanged }\npub fn history_truncated() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/src/model.rs",
        "pub enum LimitExceeded {}\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/tests/history_budget.rs",
        "// budget tests\n",
    );
    seed_file(
        root,
        "crates/flavite-api/tests/editing_history.rs",
        "// api history tests\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/views/editor_history.rs",
        "// chooser\n",
    );
    seed_file(
        root,
        "crates/flavite-vault/src/session.rs",
        "pub const APP_WIDE_MAX_HISTORY_BYTES: u64 = 1;\n",
    );
}

fn seed_x02_inputs(root: &Path) {
    seed_x01_inputs(root);
    for file in [
        "crates/flavite-edit-context/Cargo.toml",
        "crates/flavite-edit-context/src/lib.rs",
        "crates/flavite-edit-context/src/key.rs",
        "crates/flavite-edit-context/src/vocabulary.rs",
        "crates/flavite-edit-context/src/grammar.rs",
        "crates/flavite-edit-context/src/effect.rs",
        "crates/flavite-edit-context/tests/transitions.rs",
    ] {
        seed_file(root, file, "// x02 crate file\n");
    }
    seed_file(
        root,
        "crates/flavite-edit-context/src/session.rs",
        "pub fn reduce() {}\npub enum Event {}\npub enum Effect {}\n",
    );
    seed_file(
        root,
        "crates/flavite-editor/src/context.rs",
        "pub mod layout;\npub struct ContextRequest;\n",
    );
}

fn seed_x03_inputs(root: &Path) {
    seed_x02_inputs(root);
    // The per-file selection-version gates assert the workspace data and the
    // GUI target that validates it (FEUX-001 §5.4's split).
    seed_file(
        root,
        "crates/flavite-workspace/src/input.rs",
        "pub selection_version: u64,\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/host/context_mode.rs",
        "pub selection_version: u64,\n",
    );
    // The protocol-absence gate scans the workspace root artifacts.
    seed_file(root, "Cargo.toml", "[workspace]\n");
    seed_file(root, "Cargo.lock", "# locked\n");
    seed_file(root, "docs/readme.md", "# docs\n");
    seed_file(
        root,
        "crates/flavite-buffer/src/selection.rs",
        "pub const MAX_SELECTIONS: usize = 10000;\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/src/commands.rs",
        "pub fn plan() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/tests/selection_properties.rs",
        "// selection properties\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/tests/movement_deltas.rs",
        "// movement deltas\n",
    );
    seed_file(
        root,
        "crates/flavite-vault/src/editor_receipts.rs",
        "pub struct OperationId {}\n",
    );
    seed_file(
        root,
        "crates/flavite-vault/tests/editor_receipts.rs",
        "// ticket integration\n",
    );
    seed_file(
        root,
        "crates/flavite-api/src/editor_commands.rs",
        "pub fn handle_editor_delete() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-api/tests/editor_commands.rs",
        "// api editor commands\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/tests/editor_spine.rs",
        "// editor spine\n",
    );
    // The workspace has no `Selection` type and no 256 cap.
    seed_file(
        root,
        "crates/flavite-workspace/src/input.rs",
        "pub struct EditorInteraction { pub selection_version: u64 }\n",
    );
    // The history model carries movement spans.
    seed_file(
        root,
        "crates/flavite-buffer/src/history.rs",
        "pub enum HistoryError { HistoryChanged }\npub struct MovementSpan {}\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/src/undo.rs",
        "fn reserve() {}\nfn evict_to_fit() {}\nfn history_truncated() {}\n",
    );
}

#[test]
fn feux_gate_passes_clean_tree_and_rejects_forbidden_patterns() {
    let temp = tempfile::tempdir().unwrap();
    seed_x01_inputs(temp.path());
    let clean = run_gate(temp.path(), "X01");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(String::from_utf8_lossy(&clean.stdout).contains("gates"));

    // A forbidden placeholder in budget.rs fails the gate and names the
    // pattern.
    let budget = temp.path().join("crates/flavite-buffer/src/budget.rs");
    fs::write(&budget, "// TODO: implement\n").unwrap();
    let rejected = run_gate(temp.path(), "X01");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("forbidden production pattern"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    fs::write(&budget, "pub const MAX: u64 = 1;\n").unwrap();

    // A sleep in the history-critical path fails.
    let session = temp.path().join("crates/flavite-vault/src/session.rs");
    fs::write(&session, "use std::thread::sleep;\n").unwrap();
    assert!(!run_gate(temp.path(), "X01").status.success());
    fs::write(&session, "pub const APP_WIDE_MAX_HISTORY_BYTES: u64 = 1;\n").unwrap();

    // Removing a required presence pattern fails.
    let undo = temp.path().join("crates/flavite-buffer/src/undo.rs");
    fs::write(&undo, "fn reserve() {}\n").unwrap();
    let missing = run_gate(temp.path(), "X01");
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("required production pattern missing"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    fs::write(
        &undo,
        "fn reserve() {}\nfn evict_to_fit() {}\nfn history_truncated() {}\n",
    )
    .unwrap();

    // Presence is enforced: removing a completed X01 file fails.
    fs::remove_file(
        temp.path()
            .join("crates/flavite-buffer/tests/history_budget.rs"),
    )
    .unwrap();
    let missing = run_gate(temp.path(), "X01");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing/empty"));
}

#[test]
fn x02_gate_passes_clean_tree_and_rejects_second_state_machines() {
    let temp = tempfile::tempdir().unwrap();
    seed_x02_inputs(temp.path());
    let clean = run_gate(temp.path(), "X02");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // A second mode enum in the widget crate fails the single-state-machine
    // gate.
    let editor_context = temp.path().join("crates/flavite-editor/src/context.rs");
    fs::write(
        &editor_context,
        "pub mod layout;\npub enum ContextMode { Off }\n",
    )
    .unwrap();
    assert!(!run_gate(temp.path(), "X02").status.success());

    // A duplicated grammar function in the shell fails too.
    fs::write(&editor_context, "pub mod layout;\n").unwrap();
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/other.rs",
        "fn menu_key() {}\n",
    );
    assert!(!run_gate(temp.path(), "X02").status.success());
    fs::remove_file(temp.path().join("crates/flavite-ui/src/host/other.rs")).unwrap();

    // The temporary blanket re-export is forbidden: imports only.
    fs::write(
        &editor_context,
        "pub mod layout;\npub use flavite_edit_context::ContextCommand;\n",
    )
    .unwrap();
    assert!(!run_gate(temp.path(), "X02").status.success());

    // A toolkit reference inside the pure crate fails.
    seed_file(
        temp.path(),
        "crates/flavite-editor/src/context.rs",
        "pub mod layout;\n",
    );
    let key = temp.path().join("crates/flavite-edit-context/src/key.rs");
    fs::write(&key, "pub use iced::Modifiers;\n").unwrap();
    assert!(!run_gate(temp.path(), "X02").status.success());
    fs::write(&key, "// x02 crate file\n").unwrap();

    // Removing the statechart module fails presence.
    fs::remove_file(
        temp.path()
            .join("crates/flavite-edit-context/src/session.rs"),
    )
    .unwrap();
    let missing = run_gate(temp.path(), "X02");
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("missing/empty"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn feux_gate_fails_on_missing_search_targets_rg_exit_2() {
    let temp = tempfile::tempdir().unwrap();
    seed_x01_inputs(temp.path());
    // Delete the whole buffer src tree: every `rg` over it exits 2, which
    // must fail the gate (never pass as "clean").
    fs::remove_dir_all(temp.path().join("crates/flavite-buffer/src")).unwrap();
    let rejected = run_gate(temp.path(), "X01");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("search failed"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
}

#[test]
fn x03_gate_passes_clean_tree_and_rejects_kernel_forgery() {
    let temp = tempfile::tempdir().unwrap();
    seed_x03_inputs(temp.path());
    let clean = run_gate(temp.path(), "X03");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // The protocol crate must never exist.
    seed_file(
        temp.path(),
        "crates/flavite-edit-protocol/src/lib.rs",
        "// flavite-edit-protocol\n",
    );
    assert!(!run_gate(temp.path(), "X03").status.success());
    fs::remove_dir_all(temp.path().join("crates/flavite-edit-protocol")).unwrap();

    // A resurrected workspace `Selection` type fails.
    seed_file(
        temp.path(),
        "crates/flavite-workspace/src/input.rs",
        "pub struct Selection { anchor: usize, head: usize }\n",
    );
    assert!(!run_gate(temp.path(), "X03").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-workspace/src/input.rs",
        "pub struct EditorInteraction { pub selection_version: u64 }\n",
    );

    // The 256 cap is forbidden.
    seed_file(
        temp.path(),
        "crates/flavite-workspace/src/input.rs",
        "if selections.len() > 256 {}\n",
    );
    assert!(!run_gate(temp.path(), "X03").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-workspace/src/input.rs",
        "pub struct EditorInteraction { pub selection_version: u64 }\n",
    );

    // Toolkit deps inside the pure algebra are forbidden.
    seed_file(
        temp.path(),
        "crates/flavite-buffer/src/selection.rs",
        "use iced::widget;\npub const MAX_SELECTIONS: usize = 10000;\n",
    );
    assert!(!run_gate(temp.path(), "X03").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-buffer/src/selection.rs",
        "pub const MAX_SELECTIONS: usize = 10000;\n",
    );

    // The UI must not re-implement normalization.
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/other.rs",
        "fn normalize() {}\n",
    );
    assert!(!run_gate(temp.path(), "X03").status.success());
    fs::remove_file(temp.path().join("crates/flavite-ui/src/host/other.rs")).unwrap();

    // Removing a required kernel file fails presence.
    fs::remove_file(
        temp.path()
            .join("crates/flavite-vault/src/editor_receipts.rs"),
    )
    .unwrap();
    let missing = run_gate(temp.path(), "X03");
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("missing/empty"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

fn seed_x04_inputs(root: &Path) {
    seed_x03_inputs(root);
    // Presence targets for the X04 surface.
    seed_file(
        root,
        "crates/flavite-editor/src/selections.rs",
        "pub struct EditorSelections {}\npub fn escape_collapse() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-editor/tests/multi_selection.rs",
        "// editor multi-selection oracle\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/tests/multi_selection.rs",
        "// ui multi-selection tests\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/host/editor_clipboard.rs",
        "// clipboard seam\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/views/note.rs",
        "fn footer() { let member_count = 1; }\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/views/context_toolbar.rs",
        "fn caption() { let member_count = 1; }\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/accessibility/tree.rs",
        "fn describe() { let _ = \"{} selections, primary {} to {}\"; }\n",
    );
    // Widget live state: no single anchor/caret field access, but the
    // set-backed state and the viewport-window member loop exist.
    seed_file(
        root,
        "crates/flavite-editor/src/widget.rs",
        "pub struct EditorWidget { selections: EditorSelections }\npub fn draw() { let visible_members = 0; }\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/style/tokens.rs",
        "pub const selection_muted: u8 = 0;\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/app.rs",
        "const LINK_GUARD: &str = \"Choose one caret to insert a link\";\n",
    );
    // The X03 per-file selection-version gates still hold; workspace input
    // carries the canonical committed set.
    seed_file(
        root,
        "crates/flavite-workspace/src/input.rs",
        "pub struct EditorInteraction { pub selection_version: u64 }\n",
    );
}

#[test]
fn x04_gate_passes_clean_tree_and_rejects_single_selection_regressions() {
    let temp = tempfile::tempdir().unwrap();
    seed_x04_inputs(temp.path());
    let clean = run_gate(temp.path(), "X04");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // Resurrected single-selection live-state access in the widget fails.
    seed_file(
        temp.path(),
        "crates/flavite-editor/src/widget.rs",
        "pub struct EditorWidget { selections: EditorSelections }\npub fn draw() { let _ = state.caret; }\n",
    );
    assert!(!run_gate(temp.path(), "X04").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-editor/src/widget.rs",
        "pub struct EditorWidget { selections: EditorSelections }\npub fn draw() { let visible_members = 0; }\n",
    );

    // An inline clipboard effect outside the seam fails.
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/context_mode.rs",
        "fn run() { let _ = iced::clipboard::read(); }\n",
    );
    assert!(!run_gate(temp.path(), "X04").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/context_mode.rs",
        "pub selection_version: u64,\n",
    );

    // A second regex engine in the planner fails.
    seed_file(
        temp.path(),
        "crates/flavite-buffer/src/commands.rs",
        "fn plan() { let _ = regex::Regex::new(\"a\"); }\n",
    );
    assert!(!run_gate(temp.path(), "X04").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-buffer/src/commands.rs",
        "pub fn plan() {}\n",
    );

    // Removing the muted channel fails presence.
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/style/tokens.rs",
        "pub const accent: u8 = 0;\n",
    );
    assert!(!run_gate(temp.path(), "X04").status.success());
}

fn seed_x05_inputs(root: &Path) {
    seed_x04_inputs(root);
    seed_file(
        root,
        "crates/flavite-ui/tests/editor_catalog.rs",
        "fn seeded_missing_row_fails_the_enumeration() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-edit-context/tests/transitions.rs",
        "// transition matrix\n",
    );
    seed_file(
        root,
        "crates/flavite-buffer/tests/selection_ops.rs",
        "// selection ops\n",
    );
    seed_file(
        root,
        "scripts/tests/workbench/context_mode_setops.py",
        "import common\n",
    );
    // Session state carries AwaitChar.
    seed_file(
        root,
        "crates/flavite-edit-context/src/session.rs",
        "pub fn reduce() {}\npub enum Event {}\npub enum Effect {}\npub struct AwaitChar {}\n",
    );
}

#[test]
fn x05_gate_passes_clean_tree_and_rejects_a_second_regex_engine() {
    let temp = tempfile::tempdir().unwrap();
    seed_x05_inputs(temp.path());
    let clean = run_gate(temp.path(), "X05");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // A regex engine sneaking into the editor fails.
    seed_file(
        temp.path(),
        "crates/flavite-editor/src/widget.rs",
        "use regex::Regex;\npub struct EditorWidget { selections: EditorSelections }\npub fn draw() { let visible_members = 0; }\n",
    );
    assert!(!run_gate(temp.path(), "X05").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-editor/src/widget.rs",
        "pub struct EditorWidget { selections: EditorSelections }\npub fn draw() { let visible_members = 0; }\n",
    );

    // Losing AwaitChar fails presence.
    seed_file(
        temp.path(),
        "crates/flavite-edit-context/src/session.rs",
        "pub fn reduce() {}\npub enum Event {}\npub enum Effect {}\n",
    );
    assert!(!run_gate(temp.path(), "X05").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-edit-context/src/session.rs",
        "pub fn reduce() {}\npub enum Event {}\npub enum Effect {}\npub struct AwaitChar {}\n",
    );

    // Losing the enumeration gate fails presence.
    seed_file(
        temp.path(),
        "crates/flavite-ui/tests/editor_catalog.rs",
        "// no enumeration\n",
    );
    assert!(!run_gate(temp.path(), "X05").status.success());
}

fn seed_x06_inputs(root: &Path) {
    seed_x05_inputs(root);
    seed_file(
        root,
        "crates/flavite-vault/src/text_select.rs",
        "pub const MAX_SELECTION_SPANS: usize = 10001;\npub fn collect_matches() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-api/tests/selection_find.rs",
        "// selection_find\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/src/host/selection_search.rs",
        "pub struct PreviewKey {}\npub fn overlay_spans() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/tests/selection_search.rs",
        "// selection_search\n",
    );
    seed_file(
        root,
        "scripts/tests/workbench/context_mode_matches.py",
        "import common\n",
    );
    // Session carries the MatchesCollected outcome (the X01 app-wide budget
    // presence is preserved so the cumulative gates stay green); the edit-
    // context session keeps the AwaitChar state the X05 gates require.
    seed_file(
        root,
        "crates/flavite-vault/src/session.rs",
        "pub const APP_WIDE_MAX_HISTORY_BYTES: u64 = 1;\npub struct MatchesCollected {}\npub fn text_select_matches() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-editor/src/widget.rs",
        "pub struct EditorWidget { selections: EditorSelections }\npub fn with_match_preview() {}\npub fn draw() { let visible_members = 0; }\n",
    );
}

#[test]
fn x06_gate_passes_clean_tree_and_rejects_a_second_matcher() {
    let temp = tempfile::tempdir().unwrap();
    seed_x06_inputs(temp.path());
    let clean = run_gate(temp.path(), "X06");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // A second matcher sneaking into the selection-search host fails.
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/selection_search.rs",
        "use regex::Regex;\npub struct PreviewKey {}\npub fn overlay_spans() { let _ = Regex::new(\"\"); }\n",
    );
    assert!(!run_gate(temp.path(), "X06").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/selection_search.rs",
        "pub struct PreviewKey {}\npub fn overlay_spans() {}\n",
    );

    // Losing the preview-key presence fails.
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/selection_search.rs",
        "pub fn overlay_spans() {}\n",
    );
    assert!(!run_gate(temp.path(), "X06").status.success());

    // Losing the vault collection presence fails.
    seed_file(
        temp.path(),
        "crates/flavite-vault/src/text_select.rs",
        "// no collection\n",
    );
    assert!(!run_gate(temp.path(), "X06").status.success());
}

fn seed_x07_inputs(root: &Path) {
    seed_x06_inputs(root);
    seed_file(
        root,
        "crates/flavite-ui/src/host/editor_diagnostics.rs",
        "pub const MAX_DIAGNOSTIC_EVENTS: usize = 1000;\npub fn show_burst_hint() {}\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/tests/editor_diagnostics.rs",
        "// diagnostics integration\n",
    );
    seed_file(
        root,
        "crates/flavite-ui/tests/editor_failures.rs",
        "// failures integration\n",
    );
    seed_file(
        root,
        "crates/flavite-edit-context/tests/burst.rs",
        "// burst matrix\n",
    );
    seed_file(
        root,
        "scripts/tests/workbench/context_mode_failures.py",
        "import common\n",
    );
    // The pure session carries the burst thresholds and the effect; the
    // settings registry the typed disable; the host the ring bound.
    seed_file(
        root,
        "crates/flavite-edit-context/src/session.rs",
        "pub fn reduce() {}\npub enum Event {}\npub enum Effect { ShowBurstHint }\npub struct AwaitChar {}\npub const BURST_WINDOW_MS: u64 = 1500;\npub const BURST_SUPPRESS_MS: u64 = 30000;\n",
    );
    seed_file(
        root,
        "crates/flavite-settings/src/registry.rs",
        "pub fn burst_hint_enabled() {}\n",
    );
}

#[test]
fn x07_gate_passes_clean_tree_and_rejects_diagnostics_regressions() {
    let temp = tempfile::tempdir().unwrap();
    seed_x07_inputs(temp.path());
    let clean = run_gate(temp.path(), "X07");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // The ring host must exist (presence gate).
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/editor_diagnostics.rs",
        "// no ring\n",
    );
    assert!(!run_gate(temp.path(), "X07").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/editor_diagnostics.rs",
        "pub const MAX_DIAGNOSTIC_EVENTS: usize = 1000;\npub fn show_burst_hint() {}\n",
    );

    // A hidden undo action in the hint surface is forbidden (the hint may
    // only offer Exit / Open history).
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/views/context_toolbar.rs",
        "pub fn view() { let _ = \"Undo burst\"; }\n",
    );
    assert!(!run_gate(temp.path(), "X07").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/views/context_toolbar.rs",
        "pub fn view() {}\n",
    );

    // A sleep in the diagnostics host fails its absence gate.
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/editor_diagnostics.rs",
        "pub fn export() { std::thread::sleep(std::time::Duration::from_millis(1)); }\n",
    );
    assert!(!run_gate(temp.path(), "X07").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/editor_diagnostics.rs",
        "pub const MAX_DIAGNOSTIC_EVENTS: usize = 1000;\npub fn show_burst_hint() {}\n",
    );

    // Losing the burst threshold presence fails.
    seed_file(
        temp.path(),
        "crates/flavite-edit-context/src/session.rs",
        "pub fn reduce() {}\npub enum Event {}\npub enum Effect {}\npub struct AwaitChar {}\n",
    );
    assert!(!run_gate(temp.path(), "X07").status.success());
}

fn seed_x08_inputs(root: &Path) {
    seed_x07_inputs(root);
    seed_file(
        root,
        "scripts/tests/workbench/context_workbench_acceptance.py",
        "import common\n",
    );
    seed_file(root, "docs/APP-FEATURES.md", "# Flavite Features\n");
}

#[test]
fn x08_gate_passes_clean_tree_and_rejects_compat_surfaces() {
    let temp = tempfile::tempdir().unwrap();
    seed_x08_inputs(temp.path());
    let clean = run_gate(temp.path(), "X08");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // A compatibility alias surface fails the gate.
    seed_file(
        temp.path(),
        "crates/flavite-ui/src/host/aliases.rs",
        "pub const deprecatedIds: &str = \"editor.old\";\n",
    );
    assert!(!run_gate(temp.path(), "X08").status.success());
    fs::remove_file(temp.path().join("crates/flavite-ui/src/host/aliases.rs")).unwrap();

    // A legacy keymap shim fails.
    seed_file(
        temp.path(),
        "crates/flavite-settings/src/legacy.rs",
        "pub fn legacy_keymap() {}\n",
    );
    assert!(!run_gate(temp.path(), "X08").status.success());
    fs::remove_file(temp.path().join("crates/flavite-settings/src/legacy.rs")).unwrap();

    // A placeholder in the production selection path fails.
    seed_file(
        temp.path(),
        "crates/flavite-buffer/src/selection.rs",
        "pub const MAX_SELECTIONS: usize = 10000;\n// TODO: implement\n",
    );
    assert!(!run_gate(temp.path(), "X08").status.success());
    seed_file(
        temp.path(),
        "crates/flavite-buffer/src/selection.rs",
        "pub const MAX_SELECTIONS: usize = 10000;\n",
    );

    // The protocol crate is forbidden everywhere, including docs.
    seed_file(temp.path(), "docs/protocol.md", "flavite-edit-protocol\n");
    assert!(!run_gate(temp.path(), "X08").status.success());
    fs::remove_file(temp.path().join("docs/protocol.md")).unwrap();

    // Losing the acceptance scenario fails presence.
    fs::remove_file(
        temp.path()
            .join("scripts/tests/workbench/context_workbench_acceptance.py"),
    )
    .unwrap();
    let missing = run_gate(temp.path(), "X08");
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("missing/empty"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
}

#[test]
fn feux_gate_rejects_unknown_or_unimplemented_slices() {
    let temp = tempfile::tempdir().unwrap();
    seed_x01_inputs(temp.path());
    let unknown = run_gate(temp.path(), "X99");
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("unknown slice"));
    // X08 is implemented: on a seeded clean tree it passes.
    seed_x08_inputs(temp.path());
    let x08 = run_gate(temp.path(), "X08");
    assert!(
        x08.status.success(),
        "{}",
        String::from_utf8_lossy(&x08.stderr)
    );
}
