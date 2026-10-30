use std::fs;
use std::path::Path;
use std::process::Command;

fn run_gate(root: &Path) -> std::process::Output {
    Command::new("bash")
        .arg(format!(
            "{}/../scripts/check-workspace-contracts.sh",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg("W01")
        .arg(root)
        .output()
        .expect("run contract gate")
}

fn seed_required_inputs(root: &Path) {
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

#[test]
fn workspace_gate_passes_clean_tree_and_rejects_forbidden_text() {
    let temp = tempfile::tempdir().expect("temp dir");
    let accessibility = temp.path().join("crates/flavite-ui/src/accessibility");
    fs::create_dir_all(&accessibility).expect("accessibility dir");
    fs::write(accessibility.join("mod.rs"), "// runtime boundary\n").expect("clean file");
    seed_required_inputs(temp.path());
    assert!(run_gate(temp.path()).status.success());

    fs::write(accessibility.join("mod.rs"), "// fake_accessible_text\n").expect("seed");
    let rejected = run_gate(temp.path());
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("forbidden production pattern"));
}

#[test]
fn workspace_gate_rejects_missing_search_inputs() {
    let temp = tempfile::tempdir().expect("temp dir");
    let rejected = run_gate(temp.path());
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("missing or empty"));
}
