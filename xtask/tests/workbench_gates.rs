//! RFC-0006 §11: negative fixtures for the workbench source gates. The tests
//! seed a hermetic temp-tree repository (every presence file, with tiny
//! placeholder content) and drive the real checker against it via the
//! repository-root argument, so they pass independently of how much of S00/S01
//! has landed in the real tree. The gate library itself is copied from the
//! repo so the checker sources the real functions.

use std::fs;
use std::path::Path;
use std::process::Command;

fn run_gate(root: &Path, slice: &str) -> std::process::Output {
    Command::new("bash")
        .arg(format!(
            "{}/../scripts/check-workbench-contracts.sh",
            env!("CARGO_MANIFEST_DIR")
        ))
        .arg(slice)
        .arg(root)
        .output()
        .expect("run workbench gate")
}

fn seed_file(root: &Path, relative: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).expect("required parent");
    fs::write(path, "implementation\n").expect("required file");
}

fn seed_s00_inputs(root: &Path) {
    // The checker sources the gate library relative to the repository root,
    // so the seeded tree carries a copy of the real library.
    let lib = root.join("scripts/lib/source-gates.sh");
    fs::create_dir_all(lib.parent().unwrap()).expect("lib parent");
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    fs::copy(repo.join("scripts/lib/source-gates.sh"), &lib).expect("copy gate library");

    for relative in [
        "crates/flavite-encoding/src/lib.rs",
        "crates/flavite-encoding/src/format.rs",
        "crates/flavite-encoding/src/decode.rs",
        "crates/flavite-encoding/src/encode.rs",
        "crates/flavite-encoding/src/map.rs",
        "crates/flavite-encoding/src/stream.rs",
        "crates/flavite-encoding/tests/roundtrip.rs",
        "crates/flavite-encoding/tests/mapping_properties.rs",
        "crates/flavite-syntax/tests/encoding_maps.rs",
        "crates/flavite-buffer/tests/encoding_roundtrip.rs",
        "crates/flavite-vault/tests/encoding_workflow.rs",
        "crates/flavite-vault/tests/encoding_recovery.rs",
        "crates/flavite-index/tests/encoding_ingest.rs",
        "crates/flavite-api/tests/note_encoding.rs",
        "crates/flavite-ui/tests/file_encoding.rs",
        "crates/flavite-cli/tests/file_encoding.rs",
        "crates/flavite-vault/src/encoding.rs",
        "xtask/tests/workbench_gates.rs",
        "xtask/tests/workbench_ownership.rs",
        // rg targets of the S00 absence gates.
        "crates/flavite-buffer/src/eol.rs",
        "crates/flavite-buffer/src/manager.rs",
        "crates/flavite-syntax/src/text.rs",
    ] {
        seed_file(root, relative);
    }
    // The dependency gates parse these manifests. The codec manifest carries
    // its own package name, which the dependency-table parse must NOT
    // falsely reject (§11 S00 note).
    fs::create_dir_all(root.join("crates/flavite-editor")).expect("editor crate dir");
    fs::write(
        root.join("crates/flavite-encoding/Cargo.toml"),
        "[package]\nname = \"flavite-encoding\"\n\n[dependencies]\n",
    )
    .expect("codec manifest");
    fs::write(
        root.join("crates/flavite-editor/Cargo.toml"),
        "[package]\nname = \"flavite-editor\"\n\n[dependencies]\n",
    )
    .expect("editor manifest");
}

fn seed_s01_inputs(root: &Path) {
    seed_s00_inputs(root);
    for relative in [
        "crates/flavite-text-search/src/lib.rs",
        "crates/flavite-text-search/src/matcher.rs",
        "crates/flavite-text-search/src/template.rs",
        "crates/flavite-text-search/tests/literal_properties.rs",
        "crates/flavite-vault/src/text_edit.rs",
        "crates/flavite-api/src/text_edit.rs",
        "crates/flavite-ui/src/host/note_search.rs",
        "crates/flavite-buffer/tests/prepared_edits.rs",
        "crates/flavite-api/tests/note_replace.rs",
        "crates/flavite-ui/tests/note_search.rs",
        // rg targets of the S01 absence gates.
        "crates/flavite-ui/src/host/reading.rs",
        "crates/flavite-ui/src/views/note.rs",
    ] {
        seed_file(root, relative);
    }
    for (manifest, name) in [
        (
            "crates/flavite-text-search/Cargo.toml",
            "flavite-text-search",
        ),
        ("crates/flavite-buffer/Cargo.toml", "flavite-buffer"),
        ("crates/flavite-syntax/Cargo.toml", "flavite-syntax"),
        ("crates/flavite-index/Cargo.toml", "flavite-index"),
    ] {
        fs::write(
            root.join(manifest),
            format!("[package]\nname = \"{name}\"\n\n[dependencies]\n"),
        )
        .expect("S01 manifest");
    }
}

fn seed_s04_inputs(root: &Path) {
    seed_s01_inputs(root);
    for relative in [
        "crates/flavite-ui/src/host/open_intents.rs",
        "crates/flavite-ui/tests/open_intents.rs",
        "crates/flavite-ui/tests/workbench_loop.rs",
        "scripts/workbench-visual-linux.sh",
        "scripts/tests/workbench/common.py",
        "scripts/tests/workbench/graph_editor_loop.py",
        // rg targets of the S04 absence gates.
        "crates/flavite-ui/src/graph/events.rs",
        "crates/flavite-ui/src/host/graph.rs",
        "crates/flavite-ui/src/app.rs",
    ] {
        seed_file(root, relative);
    }
}

#[test]
fn workbench_gate_rejects_unknown_slice() {
    let temp = tempfile::tempdir().unwrap();
    seed_s00_inputs(temp.path());
    let rejected = run_gate(temp.path(), "S99");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("unknown slice"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
}

#[test]
fn s00_gate_passes_clean_tree_and_rejects_forbidden_import() {
    let temp = tempfile::tempdir().unwrap();
    seed_s00_inputs(temp.path());
    let clean = run_gate(temp.path(), "S00");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
    assert!(String::from_utf8_lossy(&clean.stdout).contains("gates"));

    // A forbidden production import anywhere in the codec fails the gate and
    // names the pattern.
    let lib = temp.path().join("crates/flavite-encoding/src/lib.rs");
    fs::write(&lib, "use iced::foo;\n").unwrap();
    let rejected = run_gate(temp.path(), "S00");
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(
        stderr.contains("forbidden production pattern"),
        "{}",
        stderr
    );
    assert!(stderr.contains("iced"), "{}", stderr);
    fs::write(&lib, "implementation\n").unwrap();

    // An absent required file is a failure (error-on-missing-source), and so
    // is a present-but-empty one (presence gates demand nonempty files).
    fs::remove_file(temp.path().join("crates/flavite-vault/src/encoding.rs")).unwrap();
    let missing = run_gate(temp.path(), "S00");
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("missing/empty"),
        "{}",
        String::from_utf8_lossy(&missing.stderr)
    );
    fs::write(temp.path().join("crates/flavite-vault/src/encoding.rs"), "").unwrap();
    let empty = run_gate(temp.path(), "S00");
    assert!(!empty.status.success());
    assert!(
        String::from_utf8_lossy(&empty.stderr).contains("missing/empty"),
        "{}",
        String::from_utf8_lossy(&empty.stderr)
    );
}

#[test]
fn s00_dependency_gate_rejects_declared_tables_but_not_the_package_name() {
    let temp = tempfile::tempdir().unwrap();
    seed_s00_inputs(temp.path());
    assert!(run_gate(temp.path(), "S00").status.success());

    // A dependency-table declaration of the forbidden crate fails...
    fs::write(
        temp.path().join("crates/flavite-editor/Cargo.toml"),
        "[package]\nname = \"flavite-editor\"\n\n[dependencies]\nflavite-encoding = { path = \"../flavite-encoding\" }\n",
    )
    .unwrap();
    let rejected = run_gate(temp.path(), "S00");
    assert!(!rejected.status.success());
    let stderr = String::from_utf8_lossy(&rejected.stderr);
    assert!(stderr.contains("forbidden dependency"), "{}", stderr);

    // ...including the target-specific dev-dependencies form.
    fs::write(
        temp.path().join("crates/flavite-editor/Cargo.toml"),
        "[package]\nname = \"flavite-editor\"\n\n[target.'cfg(unix)'.dev-dependencies]\nflavite-encoding = \"0.1\"\n",
    )
    .unwrap();
    assert!(!run_gate(temp.path(), "S00").status.success());
}

#[test]
fn s01_gate_runs_s00_gates_and_its_own() {
    let temp = tempfile::tempdir().unwrap();
    seed_s01_inputs(temp.path());
    let clean = run_gate(temp.path(), "S01");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // Removing an S00-required file fails even under the S01 argument:
    // skipping completed earlier gates is forbidden.
    fs::remove_file(temp.path().join("crates/flavite-encoding/src/lib.rs")).unwrap();
    let cumulative = run_gate(temp.path(), "S01");
    assert!(!cumulative.status.success());
    assert!(
        String::from_utf8_lossy(&cumulative.stderr).contains("missing/empty"),
        "{}",
        String::from_utf8_lossy(&cumulative.stderr)
    );
}

#[test]
fn s01_gate_rejects_forbidden_search_pattern() {
    let temp = tempfile::tempdir().unwrap();
    seed_s01_inputs(temp.path());
    fs::write(
        temp.path()
            .join("crates/flavite-ui/src/host/note_search.rs"),
        "let at = text.match_indices(needle).next();\n",
    )
    .unwrap();
    let rejected = run_gate(temp.path(), "S01");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("forbidden production pattern"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
}

#[test]
fn s04_gate_passes_clean_tree_and_rejects_forbidden_patterns() {
    let temp = tempfile::tempdir().unwrap();
    seed_s04_inputs(temp.path());
    let clean = run_gate(temp.path(), "S04");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // The implicit-insert gesture outcome is dead in both gesture and host
    // code; resurfacing it fails the gate with the named pattern.
    let events = temp.path().join("crates/flavite-ui/src/graph/events.rs");
    fs::write(&events, "let outcome = GestureOutcome::InsertLink(hit);\n").unwrap();
    let rejected = run_gate(temp.path(), "S04");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("GestureOutcome::InsertLink"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    fs::write(&events, "implementation\n").unwrap();

    // The global first-map-match open must stay deleted from app.rs.
    let app = temp.path().join("crates/flavite-ui/src/app.rs");
    fs::write(&app, "fn request_open(&mut self) {}\n").unwrap();
    assert!(!run_gate(temp.path(), "S04").status.success());
    fs::write(&app, "implementation\n").unwrap();

    // Graph commands use an explicit source identity, never the focused
    // group as an accidental destination.
    let graph = temp.path().join("crates/flavite-ui/src/host/graph.rs");
    fs::write(&graph, "let group = app.focused_group();\n").unwrap();
    let rejected = run_gate(temp.path(), "S04");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("focused_group"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    fs::write(&graph, "implementation\n").unwrap();

    // S04 requires S00+S01 files too: removing one fails cumulatively.
    fs::remove_file(temp.path().join("crates/flavite-text-search/src/lib.rs")).unwrap();
    let cumulative = run_gate(temp.path(), "S04");
    assert!(!cumulative.status.success());
    assert!(
        String::from_utf8_lossy(&cumulative.stderr).contains("missing/empty"),
        "{}",
        String::from_utf8_lossy(&cumulative.stderr)
    );
}
