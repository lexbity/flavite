use std::fs;
use std::path::Path;
use std::process::Command;

fn run_gate(root: &Path, profile: &str) -> std::process::Output {
    Command::new("bash")
        .arg(format!(
            "{}/../scripts/check-workspace-contracts.sh",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(profile)
        .arg(root)
        .output()
        .expect("run contract gate")
}

fn seed_w01_inputs(root: &Path) {
    for relative in [
        "crates/flavite-ui/src/accessibility/program.rs",
        "crates/flavite-ui/src/accessibility/tree.rs",
        "vendor/iced-accessibility/PATCHES.md",
        "vendor/iced-accessibility/iced_winit/src/lib.rs",
    ] {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).expect("required parent");
        fs::write(path, "implementation\n").expect("required file");
    }
}

fn seed_w02_inputs(root: &Path) {
    seed_w01_inputs(root);
    for relative in [
        "crates/flavite-workspace/src/reducer.rs",
        "crates/flavite-workspace/src/geometry.rs",
        "crates/flavite-ui/src/workspace_shell.rs",
        "crates/flavite-ui/src/workspace/pane_grid.rs",
        "crates/flavite-ui/src/host/commands.rs",
        "scripts/workspace-visual-linux.sh",
    ] {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).expect("required parent");
        fs::write(path, "implementation\n").expect("required file");
    }
    fs::write(
        root.join("crates/flavite-workspace/Cargo.toml"),
        "[dependencies]\n",
    )
    .expect("clean workspace Cargo.toml");
}

#[test]
fn workspace_gate_passes_clean_tree_and_rejects_forbidden_text() {
    let temp = tempfile::tempdir().expect("temp dir");
    let accessibility = temp.path().join("crates/flavite-ui/src/accessibility");
    fs::create_dir_all(&accessibility).expect("accessibility dir");
    fs::write(accessibility.join("mod.rs"), "// runtime boundary\n").expect("clean file");
    seed_w01_inputs(temp.path());
    assert!(run_gate(temp.path(), "W01").status.success());

    fs::write(accessibility.join("mod.rs"), "// fake_accessible_text\n").expect("seed");
    let rejected = run_gate(temp.path(), "W01");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("forbidden production pattern"));
}

#[test]
fn workspace_gate_rejects_missing_search_inputs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let rejected = run_gate(temp.path(), "W01");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("missing or empty"));
}

#[test]
fn w02_gate_passes_clean_tree_and_rejects_destination_shell() {
    let temp = tempfile::tempdir().expect("temp dir");
    seed_w02_inputs(temp.path());
    let ui_src = temp.path().join("crates/flavite-ui/src");
    fs::write(ui_src.join("app.rs"), "// clean shell\n").expect("clean app.rs");
    fs::write(ui_src.join("workspace_shell.rs"), "// clean shell\n").expect("clean shell file");
    fs::write(
        temp.path().join("crates/flavite-workspace/Cargo.toml"),
        "[dependencies]\n",
    )
    .expect("clean workspace manifest");
    assert!(run_gate(temp.path(), "W02").status.success());

    // A surviving destination enum anywhere in the UI crate fails the gate.
    fs::write(ui_src.join("pane.rs"), "pub enum Pane { Editor }\n").expect("seed pane");
    let rejected = run_gate(temp.path(), "W02");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("enum Pane"));
    fs::remove_file(ui_src.join("pane.rs")).expect("remove pane seed");

    // A demo target or legacy wordmark in the shell files fails the gate.
    fs::write(ui_src.join("app.rs"), "// LATTICE branding\n").expect("seed LATTICE");
    let rejected = run_gate(temp.path(), "W02");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("demo\\.md|LATTICE"));

    // A toolkit dependency in the IO-free workspace model fails the gate.
    fs::write(
        temp.path().join("crates/flavite-workspace/Cargo.toml"),
        "[dependencies]\niced = \"0.14\"\n",
    )
    .expect("seed iced dependency");
    let rejected = run_gate(temp.path(), "W02");
    assert!(!rejected.status.success());
}

#[test]
fn w02_gate_requires_the_visual_runner() {
    let temp = tempfile::tempdir().expect("temp dir");
    seed_w02_inputs(temp.path());
    fs::remove_file(temp.path().join("scripts/workspace-visual-linux.sh")).expect("remove runner");
    let rejected = run_gate(temp.path(), "W02");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("missing or empty"));
}

#[test]
fn w08_gate_rejects_forbidden_graph_io_and_missing_projection_inputs() {
    let temp = tempfile::tempdir().unwrap();
    seed_current_contract_inputs(temp.path());
    let clean = run_gate(temp.path(), "W08");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    let filters = temp.path().join("crates/flavite-ui/src/graph/filters.rs");
    fs::write(&filters, "use rusqlite::Connection;\n").unwrap();
    assert!(!run_gate(temp.path(), "W08").status.success());
    fs::write(filters, "implementation\n").unwrap();
    fs::remove_file(
        temp.path()
            .join("crates/flavite-api/src/graph_projection.rs"),
    )
    .unwrap();
    let missing = run_gate(temp.path(), "W08");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing or empty"));
}

fn seed_current_contract_inputs(root: &Path) {
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let script = fs::read_to_string(repo.join("scripts/check-workspace-contracts.sh")).unwrap();
    // These are inert search fixtures, not compiled substitute algorithms.
    // Derive the input paths from the gate so missing-path behavior is tested
    // for its actual additive W01–W08 profile.
    for token in script.split_whitespace() {
        let token = token.trim_matches(|c| c == '"' || c == ';');
        let relative = token.strip_prefix("$root/").unwrap_or(token);
        if !["crates/", "vendor/", "fuzz/", "scripts/"]
            .iter()
            .any(|prefix| relative.starts_with(prefix))
        {
            continue;
        }
        let source = repo.join(relative);
        let target = root.join(relative);
        if source.is_file() {
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, "implementation\n").unwrap();
        } else if source.is_dir() {
            fs::create_dir_all(&target).unwrap();
            fs::write(target.join("search_fixture.rs"), "implementation\n").unwrap();
        }
    }
}

#[test]
fn w09_gate_rejects_unsafe_renderer_and_missing_property_authority() {
    let temp = tempfile::tempdir().unwrap();
    seed_current_contract_inputs(temp.path());
    assert!(run_gate(temp.path(), "W09").status.success());
    let renderer = temp.path().join("crates/flavite-ui/src/read/renderer.rs");
    fs::write(&renderer, "use reqwest::Client;\n").unwrap();
    let forbidden = run_gate(temp.path(), "W09");
    assert!(!forbidden.status.success());
    assert!(String::from_utf8_lossy(&forbidden.stderr).contains("forbidden production pattern"));
    fs::write(renderer, "implementation\n").unwrap();
    fs::remove_file(temp.path().join("crates/flavite-vault/src/properties.rs")).unwrap();
    let missing = run_gate(temp.path(), "W09");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing or empty"));
}

#[test]
fn w11_gate_rejects_raw_view_colors_and_missing_appearance_inputs() {
    let temp = tempfile::tempdir().unwrap();
    seed_current_contract_inputs(temp.path());
    assert!(run_gate(temp.path(), "W11").status.success());
    let view = temp.path().join("crates/flavite-ui/src/views/note.rs");
    fs::write(&view, "let c = Color::from_rgb(0.1, 0.2, 0.3);\n").unwrap();
    let forbidden = run_gate(temp.path(), "W11");
    assert!(!forbidden.status.success());
    assert!(String::from_utf8_lossy(&forbidden.stderr).contains("forbidden production pattern"));
    fs::write(view, "implementation\n").unwrap();
    fs::remove_file(temp.path().join("crates/flavite-ui/src/preferences.rs")).unwrap();
    let missing = run_gate(temp.path(), "W11");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing or empty"));
}

#[test]
fn final_gate_rejects_demo_assets_and_placeholders() {
    let temp = tempfile::tempdir().unwrap();
    seed_current_contract_inputs(temp.path());
    // Ensure files checked by FINAL exist in seed
    fs::create_dir_all(temp.path().join("crates/flavite-ui/src")).unwrap();
    fs::create_dir_all(temp.path().join("crates/flavite-ui/examples")).unwrap();
    fs::write(
        temp.path().join("crates/flavite-ui/src/main.rs"),
        "fn main(){}\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-ui/src/boot.rs"),
        "fn boot(){}\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-ui/examples/app-demo.rs"),
        "// ok\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-ui/src/app.rs"),
        "// clean\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-ui/src/workspace/mod.rs"),
        "// clean\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-ui/src/views/note.rs"),
        "// clean\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-ui/src/host/commands.rs"),
        "// clean\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-editor/src/widget.rs"),
        "// clean\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-workspace/src/reducer.rs"),
        "// clean\n",
    )
    .unwrap();
    assert!(run_gate(temp.path(), "FINAL").status.success());

    fs::write(
        temp.path().join("crates/flavite-ui/examples/app-demo.rs"),
        "prepare_demo_vault();\n",
    )
    .unwrap();
    assert!(!run_gate(temp.path(), "FINAL").status.success());

    fs::write(
        temp.path().join("crates/flavite-ui/examples/app-demo.rs"),
        "// ok\n",
    )
    .unwrap();
    fs::write(
        temp.path().join("crates/flavite-ui/src/host/commands.rs"),
        "// TODO: fix later\n",
    )
    .unwrap();
    assert!(!run_gate(temp.path(), "FINAL").status.success());
}
