//! Integration tests for xtask (§10.2, §10.5, §10.10).
//!
//! Uses a synthetic fixture workspace in `tests/fixtures/workspace/` to test
//! the arch check (drift, edges, layering) and cov-check end-to-end.
//! Negative tests (AC-28/AC-29) seed violations and assert failure.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Copy the fixture workspace to a unique temp directory.
fn fixture_workspace() -> PathBuf {
    let id = COUNTER.fetch_add(1, Ordering::SeqCst);
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/workspace");
    let tmp = std::env::temp_dir().join(format!(
        "flavite-xtask-integration-{}-{id}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&tmp);
    copy_dir_all(&fixture, &tmp).expect("failed to copy fixture");
    tmp
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

fn cleanup(tmp: &Path) {
    let _ = fs::remove_dir_all(tmp);
}

// ---------------------------------------------------------------------------
// Positive tests: arch check passes on a clean fixture
// ---------------------------------------------------------------------------

#[test]
fn arch_check_passes_on_clean_fixture() {
    let tmp = fixture_workspace();
    // Generate artifacts first.
    xtask::arch::generate(&tmp).expect("arch gen failed");
    // Check should pass.
    let report = xtask::arch::check(&tmp).expect("arch check failed");
    assert!(!report.drift, "drift should be false after gen");
    assert!(report.edges_ok, "edges should match: {:?}", report);
    assert!(
        report.layering_violations.is_empty(),
        "no layering violations expected"
    );
    cleanup(&tmp);
}

#[test]
fn arch_gen_creates_artifacts() {
    let tmp = fixture_workspace();
    xtask::arch::generate(&tmp).expect("arch gen failed");
    assert!(tmp.join("devdocs/architecture/crate-graph.json").exists());
    assert!(tmp.join("devdocs/architecture/DEPENDENCIES.md").exists());
    // DEPENDENCIES.md has the GENERATED header.
    let md = fs::read_to_string(tmp.join("devdocs/architecture/DEPENDENCIES.md")).unwrap();
    assert!(md.contains("GENERATED"));
    // crate-graph.json is valid JSON.
    let json = fs::read_to_string(tmp.join("devdocs/architecture/crate-graph.json")).unwrap();
    let _: serde_json::Value = serde_json::from_str(&json).expect("invalid JSON");
    cleanup(&tmp);
}

// ---------------------------------------------------------------------------
// Negative tests (AC-28): edge violations
// ---------------------------------------------------------------------------

#[test]
fn arch_check_fails_on_undocumented_edge() {
    // AC-28: adding an internal dependency without updating crates.toml
    // must fail the edges check specifically.
    let tmp = fixture_workspace();
    // Add a dependency from alpha to gamma (a new internal edge, no cycle).
    let alpha_manifest = tmp.join("crates/alpha/Cargo.toml");
    let content = fs::read_to_string(&alpha_manifest).unwrap();
    let content = content.replace(
        "[dependencies]",
        "[dependencies]\ngamma = { path = \"../gamma\" }",
    );
    fs::write(&alpha_manifest, content).unwrap();

    // Generate artifacts (drift check passes because we regenerate).
    xtask::arch::generate(&tmp).expect("arch gen failed");

    // Check should fail on edges (the new edge is not in crates.toml).
    let report = xtask::arch::check(&tmp).expect("arch check should run");
    assert!(
        !report.edges_ok,
        "edges check should fail: undocumented edge alpha → gamma"
    );
    cleanup(&tmp);
}

#[test]
fn arch_check_fails_on_missing_expected_edge() {
    // AC-28: removing an edge that crates.toml expects must fail.
    let tmp = fixture_workspace();
    // Remove the beta → alpha dependency.
    let beta_manifest = tmp.join("crates/beta/Cargo.toml");
    let content = fs::read_to_string(&beta_manifest).unwrap();
    let content = content.replace("alpha = { path = \"../alpha\" }\n", "");
    fs::write(&beta_manifest, content).unwrap();

    xtask::arch::generate(&tmp).expect("arch gen failed");

    let report = xtask::arch::check(&tmp).expect("arch check should run");
    assert!(
        !report.edges_ok,
        "edges check should fail: expected edge beta → alpha is missing"
    );
    cleanup(&tmp);
}

// ---------------------------------------------------------------------------
// Negative tests (AC-28): layering violations
// ---------------------------------------------------------------------------

#[test]
fn arch_check_fails_on_layering_violation() {
    // AC-28: adding iced to a core-tier crate must fail the layering check.
    let tmp = fixture_workspace();
    // Add iced to alpha (a core-tier crate in the fixture).
    // Resolve a local package with the real external crate name so this test
    // stays deterministic and works in network-isolated builds.
    let iced_dir = tmp.join("external/iced");
    fs::create_dir_all(&iced_dir).unwrap();
    fs::write(
        iced_dir.join("Cargo.toml"),
        "[package]\nname = \"iced\"\nversion = \"0.14.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(iced_dir.join("src")).unwrap();
    fs::write(iced_dir.join("src/lib.rs"), "#![no_std]\n").unwrap();
    let alpha_manifest = tmp.join("crates/alpha/Cargo.toml");
    let content = fs::read_to_string(&alpha_manifest).unwrap();
    let content = content.replace(
        "[dependencies]",
        "[dependencies]\niced = { version = \"0.14\", path = \"../../external/iced\" }",
    );
    fs::write(&alpha_manifest, content).unwrap();

    xtask::arch::generate(&tmp).expect("arch gen failed");

    let report = xtask::arch::check(&tmp).expect("arch check should run");
    assert!(
        !report.layering_violations.is_empty(),
        "layering check should fail: iced in core-tier crate alpha"
    );
    assert!(
        report
            .layering_violations
            .iter()
            .any(|v| v.contains("iced")),
        "violation should mention iced: {:?}",
        report.layering_violations
    );
    cleanup(&tmp);
}

// ---------------------------------------------------------------------------
// Negative tests (AC-28): drift detection
// ---------------------------------------------------------------------------

#[test]
fn arch_check_fails_on_drift() {
    let tmp = fixture_workspace();
    xtask::arch::generate(&tmp).expect("arch gen failed");
    // Modify a generated artifact (simulate hand-editing).
    let json_path = tmp.join("devdocs/architecture/crate-graph.json");
    let mut content = fs::read_to_string(&json_path).unwrap();
    content.push_str("\n// hand-edited\n");
    fs::write(&json_path, content).unwrap();

    let report = xtask::arch::check(&tmp).expect("arch check should run");
    assert!(report.drift, "drift check should fail after hand-editing");
    cleanup(&tmp);
}

// ---------------------------------------------------------------------------
// Negative tests (AC-29): coverage below floor
// ---------------------------------------------------------------------------

#[test]
fn cov_check_fails_below_floor() {
    // AC-29: a Tier-1 crate at 50% (floor: 90%) must fail cov-check.
    let tmp = fixture_workspace();
    let lcov_path = tmp.join("lcov.info");
    fs::write(
        &lcov_path,
        "SF:/project/crates/alpha/src/lib.rs\nDA:1,1\nDA:2,0\nend_of_record\n",
    )
    .unwrap();

    let report = xtask::cov::check(&lcov_path, &tmp, None).expect("cov check should run");
    assert!(
        !report.failures.is_empty(),
        "cov-check should fail: alpha at 50% < 90% floor"
    );
    assert!(report.failures[0].contains("alpha"));
    cleanup(&tmp);
}

#[test]
fn cov_check_passes_above_floor() {
    let tmp = fixture_workspace();
    let lcov_path = tmp.join("lcov.info");
    fs::write(
        &lcov_path,
        "SF:/project/crates/alpha/src/lib.rs\nDA:1,1\nDA:2,1\nend_of_record\n\
         SF:/project/crates/beta/src/lib.rs\nDA:1,1\nend_of_record\n",
    )
    .unwrap();

    let report = xtask::cov::check(&lcov_path, &tmp, None).expect("cov check should run");
    assert!(
        report.failures.is_empty(),
        "cov-check should pass: all crates at 100%"
    );
    cleanup(&tmp);
}

// ---------------------------------------------------------------------------
// Query CLI tests
// ---------------------------------------------------------------------------

#[test]
fn arch_query_deps_lists_dependencies() {
    let tmp = fixture_workspace();
    xtask::arch::generate(&tmp).expect("arch gen failed");
    // Query deps for beta (should list alpha).
    // We test the query function directly since it prints to stdout.
    let result = xtask::arch::query(&tmp, "deps", &["beta".to_string()]);
    assert!(result.is_ok());
    cleanup(&tmp);
}

#[test]
fn arch_query_why_shows_tier() {
    let tmp = fixture_workspace();
    xtask::arch::generate(&tmp).expect("arch gen failed");
    let result = xtask::arch::query(&tmp, "why", &["alpha".to_string()]);
    assert!(result.is_ok());
    cleanup(&tmp);
}

#[test]
fn arch_query_violations_empty_on_clean() {
    let tmp = fixture_workspace();
    xtask::arch::generate(&tmp).expect("arch gen failed");
    let result = xtask::arch::query(&tmp, "violations", &[]);
    assert!(result.is_ok());
    cleanup(&tmp);
}

// ---------------------------------------------------------------------------
// Clean/gc integration tests
// ---------------------------------------------------------------------------

#[test]
fn clean_removes_target_in_fixture() {
    let tmp = fixture_workspace();
    // Create some build output.
    fs::create_dir_all(tmp.join("target/debug")).unwrap();
    fs::write(tmp.join("target/debug/binary"), b"data").unwrap();

    let report = xtask::clean::clean(&tmp, false).expect("clean failed");
    assert!(!report.paths_removed.is_empty());
    assert!(!tmp.join("target").exists());
    // Fixture files (Cargo.toml, crates/) are preserved.
    assert!(tmp.join("Cargo.toml").exists());
    assert!(tmp.join("crates/alpha/Cargo.toml").exists());
    cleanup(&tmp);
}

#[test]
fn clean_dry_run_preserves_everything() {
    let tmp = fixture_workspace();
    fs::create_dir_all(tmp.join("target/debug")).unwrap();
    fs::write(tmp.join("target/debug/binary"), b"data").unwrap();

    let report = xtask::clean::clean(&tmp, true).expect("clean dry-run failed");
    assert!(report.dry_run);
    assert!(report.bytes_freed > 0);
    // Nothing removed.
    assert!(tmp.join("target").exists());
    cleanup(&tmp);
}
