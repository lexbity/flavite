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
    seed_s03_inputs(root);
    for relative in [
        "crates/flavite-ui/src/host/open_intents.rs",
        "crates/flavite-ui/tests/open_intents.rs",
        "crates/flavite-ui/tests/workbench_loop.rs",
        "scripts/workbench-visual-linux.sh",
        "scripts/tests/workbench/common.py",
        "scripts/tests/workbench/graph_editor_loop.py",
        "scripts/tests/workbench/reference_workspace.py",
        // rg targets of the S04 absence gates.
        "crates/flavite-ui/src/graph/events.rs",
        "crates/flavite-ui/src/host/graph.rs",
        "crates/flavite-ui/src/app.rs",
        "crates/flavite-ui/src/views/graph.rs",
    ] {
        seed_file(root, relative);
    }
}

fn seed_s05_inputs(root: &Path) {
    seed_s04_inputs(root);
    for relative in [
        "crates/flavite-index/src/pool.rs",
        "crates/flavite-index/src/types.rs",
        "crates/flavite-api/src/queries.rs",
        "crates/flavite-api/src/query_service.rs",
        "crates/flavite-api/src/flavite.rs",
        "crates/flavite-api/src/lib.rs",
        "crates/flavite-vault/src/actor.rs",
        "crates/flavite-vault/src/session.rs",
        "crates/flavite-api/tests/catalog_tickets.rs",
        "crates/flavite-editor/src/links.rs",
        "crates/flavite-editor/src/widget.rs",
        "crates/flavite-ui/src/app.rs",
        "crates/flavite-ui/src/views/note.rs",
        "crates/flavite-ui/tests/catalog_acquisition.rs",
        "crates/flavite-api/src/device_preferences.rs",
    ] {
        seed_file(root, relative);
    }
}

fn seed_s06_inputs(root: &Path) {
    seed_s05_inputs(root);
    for relative in [
        "crates/flavite-index/src/search_eval.rs",
        "crates/flavite-index/src/search_session.rs",
        "crates/flavite-index/src/search_writer.rs",
        "crates/flavite-api/src/search_sessions.rs",
        "crates/flavite-ui/src/host/search.rs",
        "crates/flavite-index/tests/search_boolean.rs",
        "crates/flavite-index/tests/search_writer.rs",
        "crates/flavite-api/tests/search_sessions.rs",
        "crates/flavite-ui/tests/search_paging.rs",
        "crates/flavite-cli/tests/search_modes.rs",
        // rg targets of the S06 absence gates.
        "crates/flavite-index/src/query.rs",
        "crates/flavite-index/src/search.rs",
        "crates/flavite-ui/src/views/search.rs",
    ] {
        seed_file(root, relative);
    }
}

fn seed_s07_inputs(root: &Path) {
    seed_s06_inputs(root);
    for relative in [
        "crates/flavite-vault/src/replace/mod.rs",
        "crates/flavite-vault/src/replace/plan.rs",
        "crates/flavite-vault/src/replace/preview.rs",
        "crates/flavite-vault/src/replace/store.rs",
        "crates/flavite-vault/tests/replace_preview.rs",
        "crates/flavite-api/src/replace.rs",
        "crates/flavite-api/tests/source_find.rs",
        "crates/flavite-ui/src/host/replace.rs",
        "crates/flavite-ui/src/views/replace.rs",
        "crates/flavite-ui/tests/vault_replace_preview.rs",
        "scripts/tests/workbench/vault_source_preview.py",
    ] {
        seed_file(root, relative);
    }
}

fn seed_s08_inputs(root: &Path) {
    seed_s07_inputs(root);
    for relative in [
        // rg targets of the S08 absence gates (must exist in the tree).
        "crates/flavite-vault/src/save.rs",
        "crates/flavite-fs/tests/retained_replace.rs",
        "crates/flavite-vault/src/source_leases.rs",
        "crates/flavite-vault/src/replace/coordinator.rs",
        "crates/flavite-vault/src/replace/log.rs",
        "crates/flavite-vault/src/replace/auth.rs",
        "crates/flavite-vault/tests/replace_fault.rs",
        "crates/flavite-vault/tests/replace_leases.rs",
        "crates/flavite-api/tests/replace_apply.rs",
        "crates/flavite-ui/src/host/operations.rs",
        "crates/flavite-ui/src/views/operations.rs",
        "crates/flavite-ui/tests/replace_operations.rs",
        "crates/flavite-cli/tests/operations_cli.rs",
    ] {
        seed_file(root, relative);
    }
}

#[test]
fn s08_gate_passes_clean_tree_and_rejects_s08_isolation_violations() {
    let temp = tempfile::tempdir().unwrap();
    seed_s08_inputs(temp.path());
    let clean = run_gate(temp.path(), "S08");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // The shared save primitive never regresses to a bare rename_replace.
    let save = temp.path().join("crates/flavite-vault/src/save.rs");
    fs::write(
        &save,
        "fn save_inner() { fs.rename_replace(&temp, &real); }\n",
    )
    .unwrap();
    let rejected = run_gate(temp.path(), "S08");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("rename_replace"));
    fs::write(&save, "implementation\n").unwrap();

    // The operations host/view never touch the filesystem or block.
    let ops = temp.path().join("crates/flavite-ui/src/host/operations.rs");
    fs::write(&ops, "use std::fs::File;\n").unwrap();
    let rejected = run_gate(temp.path(), "S08");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("std::fs"));
    fs::write(&ops, "implementation\n").unwrap();

    // The authenticated log never stores free-form string payloads.
    let log = temp.path().join("crates/flavite-vault/src/replace/log.rs");
    fs::write(&log, "let m: HashMap<String, String> = HashMap::new();\n").unwrap();
    let rejected = run_gate(temp.path(), "S08");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("HashMap<String, String>"));
    fs::write(&log, "implementation\n").unwrap();

    // Presence is enforced: removing a completed S08 file fails.
    fs::remove_file(
        temp.path()
            .join("crates/flavite-vault/src/replace/coordinator.rs"),
    )
    .unwrap();
    let missing = run_gate(temp.path(), "S08");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing/empty"));
}

#[test]
fn s07_gate_passes_clean_tree_and_rejects_s07_isolation_violations() {
    let temp = tempfile::tempdir().unwrap();
    seed_s07_inputs(temp.path());
    let clean = run_gate(temp.path(), "S07");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // The UI host never touches the filesystem or the index/vault crates.
    let host = temp.path().join("crates/flavite-ui/src/host/replace.rs");
    fs::write(&host, "use std::fs::File;\n").unwrap();
    let rejected = run_gate(temp.path(), "S07");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("std::fs"));
    fs::write(&host, "implementation\n").unwrap();

    // The vault preview never consults the full-text index for offsets.
    let preview = temp
        .path()
        .join("crates/flavite-vault/src/replace/preview.rs");
    fs::write(&preview, "fn f() { search_notes(\"q\"); }\n").unwrap();
    let rejected = run_gate(temp.path(), "S07");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("search_notes"));
    fs::write(&preview, "implementation\n").unwrap();

    // The S07 facade registers no destructive Apply path.
    let api = temp.path().join("crates/flavite-api/src/replace.rs");
    fs::write(&api, "fn apply_replace() {}\n").unwrap();
    let rejected = run_gate(temp.path(), "S07");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("apply_replace"));

    // Presence is enforced: removing a completed S07 file fails.
    fs::remove_file(temp.path().join("crates/flavite-vault/src/replace/plan.rs")).unwrap();
    let missing = run_gate(temp.path(), "S07");
    assert!(!missing.status.success());
}

#[test]
fn s06_gate_passes_clean_tree_and_rejects_retired_search_paths() {
    let temp = tempfile::tempdir().unwrap();
    seed_s06_inputs(temp.path());
    let clean = run_gate(temp.path(), "S06");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    // The legacy whitespace-split search adapter stays dead.
    let query_service = temp.path().join("crates/flavite-api/src/query_service.rs");
    fs::write(&query_service, "let terms = trimmed.split_whitespace();\n").unwrap();
    let rejected = run_gate(temp.path(), "S06");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("split_whitespace"));
    fs::write(&query_service, "implementation\n").unwrap();

    // The UI never grows a page-limit-growth Load-more path again.
    let host = temp.path().join("crates/flavite-ui/src/host/search.rs");
    fs::write(&host, "const MAX_PAGE_LIMIT: u64 = 200;\n").unwrap();
    let rejected = run_gate(temp.path(), "S06");
    assert!(!rejected.status.success());
    fs::write(&host, "implementation\n").unwrap();

    // The structural SQL translator never regains the text placeholder.
    let query = temp.path().join("crates/flavite-index/src/query.rs");
    fs::write(
        &query,
        "Self::Text(_query) => (\"1 = 0\".to_string(), Vec::new()),\n",
    )
    .unwrap();
    let rejected = run_gate(temp.path(), "S06");
    assert!(!rejected.status.success());
    fs::write(&query, "implementation\n").unwrap();

    // The view owns no search state (it lives in the host).
    let view = temp.path().join("crates/flavite-ui/src/views/search.rs");
    fs::write(&view, "pub struct SearchState;\n").unwrap();
    let rejected = run_gate(temp.path(), "S06");
    assert!(!rejected.status.success());
    fs::write(&view, "implementation\n").unwrap();

    // The retired private hybrid executor stays dead.
    let pool = temp.path().join("crates/flavite-index/src/pool.rs");
    fs::write(&pool, "fn search_notes_with_filter_limit() {}\n").unwrap();
    let rejected = run_gate(temp.path(), "S06");
    assert!(!rejected.status.success());
    fs::write(&pool, "implementation\n").unwrap();

    // Presence is enforced: removing a completed S06 file fails.
    fs::remove_file(
        temp.path()
            .join("crates/flavite-index/src/search_writer.rs"),
    )
    .unwrap();
    let missing = run_gate(temp.path(), "S06");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing/empty"));
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

    // The retired implicit-insert gesture copy stays dead in the chrome,
    // and deferred non-note activation cannot come back as a hint.
    let views = temp.path().join("crates/flavite-ui/src/views/graph.rs");
    fs::write(
        &views,
        "text(\"— double-click a note node to insert its link\")\n",
    )
    .unwrap();
    let rejected = run_gate(temp.path(), "S04");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("double-click a note node to insert"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    fs::write(&views, "text(\"Attachment open arrives with W09\")\n").unwrap();
    let rejected = run_gate(temp.path(), "S04");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("arrives with W09"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    fs::write(&views, "implementation\n").unwrap();
    assert!(
        run_gate(temp.path(), "S04").status.success(),
        "the restored fixture must pass again"
    );

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

#[test]
fn s05_gate_requires_the_rich_ticketed_catalog_path() {
    let temp = tempfile::tempdir().unwrap();
    seed_s05_inputs(temp.path());
    let clean = run_gate(temp.path(), "S05");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );

    let query_service = temp.path().join("crates/flavite-api/src/query_service.rs");
    fs::write(
        &query_service,
        "pub use crate::queries::NoteCatalogEntry as CatalogEntry;\n",
    )
    .unwrap();
    let alias = run_gate(temp.path(), "S05");
    assert!(!alias.status.success());
    assert!(
        String::from_utf8_lossy(&alias.stderr).contains("forbidden production pattern"),
        "{}",
        String::from_utf8_lossy(&alias.stderr)
    );
    fs::write(&query_service, "implementation\n").unwrap();

    let app_source = temp.path().join("crates/flavite-ui/src/app.rs");
    fs::write(&app_source, "Message::FindOpen(tab)\n").unwrap();
    let shim = run_gate(temp.path(), "S05");
    assert!(!shim.status.success());
    assert!(
        String::from_utf8_lossy(&shim.stderr).contains("forbidden production pattern"),
        "{}",
        String::from_utf8_lossy(&shim.stderr)
    );
    fs::write(&app_source, "implementation\n").unwrap();

    let ui = temp.path().join("crates/flavite-ui/src/app.rs");
    fs::write(&ui, "flavite.note_catalog(50_000);\n").unwrap();
    let rejected = run_gate(temp.path(), "S05");
    assert!(!rejected.status.success());
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("\\.note_catalog"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );

    fs::write(&ui, "implementation\n").unwrap();
    fs::remove_file(temp.path().join("crates/flavite-api/src/query_service.rs")).unwrap();
    let missing = run_gate(temp.path(), "S05");
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("missing/empty"));
}

/// Seed the S03 helper-crate surfaces. The S03 absence gates target the
/// helper sources and the UI search host, so the seeded placeholders are
/// clean and the negative tests inject each forbidden pattern.
fn seed_s03_inputs(root: &Path) {
    seed_s01_inputs(root);
    for relative in [
        "crates/flavite-text-search/src/protocol.rs",
        "crates/flavite-text-search-worker/src/main.rs",
        "crates/flavite-text-search/tests/regex_properties.rs",
        "crates/flavite-vault/src/text_worker.rs",
        "crates/flavite-vault/tests/text_worker.rs",
        "crates/flavite-ui/tests/note_regex.rs",
        "crates/flavite-api/tests/note_regex.rs",
        "fuzz/fuzz_targets/text_match.rs",
        "fuzz/fuzz_targets/text_worker_protocol.rs",
    ] {
        seed_file(root, relative);
    }
    fs::write(
        root.join("crates/flavite-text-search-worker/Cargo.toml"),
        "[package]\nname = \"flavite-text-search-worker\"\n\n[dependencies]\n",
    )
    .expect("S03 manifest");
}

#[test]
fn s03_gate_passes_clean_tree() {
    let temp = tempfile::tempdir().unwrap();
    seed_s03_inputs(temp.path());
    let clean = run_gate(temp.path(), "S03");
    assert!(
        clean.status.success(),
        "{}",
        String::from_utf8_lossy(&clean.stderr)
    );
}

#[test]
fn s03_gate_rejects_shell_and_worker_path_hijacks() {
    for (needle, description) in [
        (r#"Command::new("sh").arg("-c").spawn();"#, "shell spawn"),
        (
            r#"let p = env::var("FLAVITE_WORKER_PATH").unwrap();"#,
            "worker path override",
        ),
    ] {
        let temp = tempfile::tempdir().unwrap();
        seed_s03_inputs(temp.path());
        fs::write(
            temp.path().join("crates/flavite-vault/src/text_worker.rs"),
            needle,
        )
        .unwrap();
        let rejected = run_gate(temp.path(), "S03");
        assert!(!rejected.status.success(), "{description} must be rejected");
        assert!(
            String::from_utf8_lossy(&rejected.stderr).contains("forbidden production pattern"),
            "{}",
            String::from_utf8_lossy(&rejected.stderr)
        );
    }
}

#[test]
fn s03_gate_rejects_regex_compilation_and_helper_product_edges() {
    // UI-side regex compilation (TD discipline: matches come from the API).
    let temp = tempfile::tempdir().unwrap();
    seed_s03_inputs(temp.path());
    fs::write(
        temp.path()
            .join("crates/flavite-ui/src/host/note_search.rs"),
        "let compiled = Regex::new(pattern)?;
",
    )
    .unwrap();
    assert!(!run_gate(temp.path(), "S03").status.success());

    // The helper cannot declare a product-crate dependency.
    let temp = tempfile::tempdir().unwrap();
    seed_s03_inputs(temp.path());
    fs::write(
        temp.path()
            .join("crates/flavite-text-search-worker/Cargo.toml"),
        "[package]\nname = \"flavite-text-search-worker\"\n\n[dependencies]\nflavite-vault = { path = \"../flavite-vault\" }\n",
    )
    .unwrap();
    let rejected = run_gate(temp.path(), "S03");
    assert!(
        !rejected.status.success(),
        "helper → vault dependency must be rejected"
    );
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("forbidden dependency"),
        "{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
}

#[test]
fn later_slices_require_s03_helper_and_isolation_guards() {
    for slice in ["S04", "S05"] {
        let temp = tempfile::tempdir().unwrap();
        seed_s05_inputs(temp.path());
        assert!(run_gate(temp.path(), slice).status.success());
        let worker = temp.path().join("crates/flavite-vault/src/text_worker.rs");
        fs::write(&worker, r#"Command::new("sh").spawn();"#).unwrap();
        assert!(
            !run_gate(temp.path(), slice).status.success(),
            "{slice} must retain S03 isolation checks"
        );
        fs::write(
            &worker,
            "implementation
",
        )
        .unwrap();
        fs::remove_file(
            temp.path()
                .join("crates/flavite-text-search/src/protocol.rs"),
        )
        .unwrap();
        assert!(
            !run_gate(temp.path(), slice).status.success(),
            "{slice} must require S03 protocol"
        );
    }
}
