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
