//! RFC-0006 §4.1 workbench ownership tests (S00/S01).
//!
//! These tests check cargo metadata's **normal/build** graph as well as
//! exact direct edges, so a hidden GUI dependency in a codec crate fails
//! and dev-only fixture dependencies cannot make a production layer
//! violation look allowed. Negative fixtures prove the predicate rejects
//! every forbidden edge before it is trusted against the real workspace.

use std::collections::BTreeMap;
use std::process::Command;

/// One resolved workspace dependency edge (`from` → `to`).
type Edges = BTreeMap<String, Vec<String>>;

fn run_metadata(workspace_root: &std::path::Path) -> serde_json::Value {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--all-features"])
        .current_dir(workspace_root)
        .output()
        .expect("cargo metadata runs");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("metadata JSON parses")
}

/// Collect normal + build dependency edges among workspace members (the
/// production graph; dev-dependencies are excluded by construction).
fn workspace_edges(metadata: &serde_json::Value) -> Edges {
    let members: std::collections::HashSet<&str> = metadata["workspace_members"]
        .as_array()
        .expect("members")
        .iter()
        .map(|id| id.as_str().expect("id"))
        .collect();
    let mut edges = Edges::new();
    for package in metadata["packages"].as_array().expect("packages") {
        let id = package["id"].as_str().expect("package id");
        if !members.contains(id) {
            continue;
        }
        let name = package["name"].as_str().expect("package name").to_owned();
        for dep in package["dependencies"].as_array().expect("dependencies") {
            let kind = dep["kind"].as_str();
            if matches!(kind, Some("dev")) {
                continue;
            }
            edges
                .entry(name.clone())
                .or_default()
                .push(dep["name"].as_str().expect("dep name").to_owned());
        }
    }
    edges
}

/// RFC-0006 §4.1 ownership predicate. Returns every violating edge.
fn ownership_violations(edges: &Edges) -> Vec<String> {
    let mut violations = Vec::new();
    // Canonical consumers: encoding sits below syntax/buffer/vault/api/index;
    // Source-search is consumed by the vault/API facade and the isolated
    // matcher helper (§4 and §6.2).
    let encoding_consumers = [
        "flavite-syntax",
        "flavite-buffer",
        "flavite-vault",
        "flavite-api",
        "flavite-index",
    ];
    let search_consumers = ["flavite-vault", "flavite-api", "flavite-text-search-worker"];
    for (from, to) in edges {
        for target in to {
            match (target.as_str(), from.as_str()) {
                ("flavite-encoding", from) if !encoding_consumers.contains(&from) => {
                    violations.push(format!("{from} → flavite-encoding"));
                }
                ("flavite-text-search", from) if !search_consumers.contains(&from) => {
                    violations.push(format!("{from} → flavite-text-search"));
                }
                _ => {}
            }
        }
        // The codec crate and the matcher have no first-party outbound
        // edges beyond the declared ones.
        if from == "flavite-encoding" {
            violations.extend(
                to.iter()
                    .filter(|target| target.starts_with("flavite-"))
                    .map(|target| format!("flavite-encoding → {target}")),
            );
        }
        if from == "flavite-text-search" {
            violations.extend(
                to.iter()
                    .filter(|target| {
                        target.starts_with("flavite-") && target.as_str() != "flavite-syntax"
                    })
                    .map(|target| format!("flavite-text-search → {target}")),
            );
        }
    }
    violations
}

/// GUI/foreign crates that must never appear in the codec crate's
/// normal/build dependency list (§4 first gate).
const ENCODING_FORBIDDEN_EXTERNALS: [&str; 7] = [
    "iced",
    "cosmic-text",
    "ropey",
    "rusqlite",
    "regex",
    "encoding_rs",
    "chardet",
];

#[test]
fn encoding_crate_has_no_flavite_or_gui_dependencies() {
    let root = std::env::current_dir().expect("cwd");
    let root = root.ancestors().nth(1).expect("workspace root");
    let edges = workspace_edges(&run_metadata(root));
    let encoding = edges.get("flavite-encoding").cloned().unwrap_or_default();
    for dep in &encoding {
        assert!(
            !dep.starts_with("flavite-"),
            "flavite-encoding must not depend on any Flavite crate, found {dep}"
        );
        assert!(
            !ENCODING_FORBIDDEN_EXTERNALS.contains(&dep.as_str()),
            "flavite-encoding must not depend on {dep}"
        );
    }
    // The declared dependency set is exactly the optional serde.
    assert!(
        encoding.iter().all(|dep| dep == "serde"),
        "flavite-encoding declares only optional serde, found {encoding:?}"
    );
}

#[test]
fn source_search_crate_has_only_its_allowed_closure() {
    let root = std::env::current_dir().expect("cwd");
    let root = root.ancestors().nth(1).expect("workspace root");
    let edges = workspace_edges(&run_metadata(root));
    let search = edges
        .get("flavite-text-search")
        .cloned()
        .unwrap_or_default();
    let allowed = ["flavite-syntax", "regex", "unicode-segmentation", "serde"];
    for dep in &search {
        assert!(
            allowed.contains(&dep.as_str()),
            "flavite-text-search may depend only on {allowed:?}, found {dep}"
        );
        assert!(
            !dep.starts_with("flavite-") || dep == "flavite-syntax",
            "flavite-text-search depends only on flavite-syntax among first-party crates"
        );
    }
}

#[test]
fn no_other_crate_acquires_a_direct_codec_or_search_edge() {
    let root = std::env::current_dir().expect("cwd");
    let root = root.ancestors().nth(1).expect("workspace root");
    let edges = workspace_edges(&run_metadata(root));
    let violations = ownership_violations(&edges);
    assert!(
        violations.is_empty(),
        "ownership violations in the real workspace: {violations:?}"
    );
}

#[test]
fn negative_fixtures_introduce_each_forbidden_edge_and_are_rejected() {
    // Seed synthetic workspaces that introduce each forbidden edge and
    // prove the predicate fails for each — the checker is only trusted
    // once its rejections are demonstrated (§4.1 negative fixtures).
    let seed = |from: &str, to: &str| {
        let mut edges = Edges::new();
        edges.insert(
            "flavite-syntax".to_owned(),
            vec!["flavite-encoding".to_owned()],
        );
        edges.insert(
            "flavite-buffer".to_owned(),
            vec!["flavite-syntax".to_owned()],
        );
        edges.insert(
            "flavite-vault".to_owned(),
            vec!["flavite-buffer".to_owned()],
        );
        edges.insert(from.to_owned(), vec![to.to_owned()]);
        edges
    };
    for (from, to) in [
        ("flavite-editor", "flavite-encoding"),
        ("flavite-editor", "flavite-text-search"),
        ("flavite-ui", "flavite-encoding"),
        ("flavite-ui", "flavite-text-search"),
        ("flavite-buffer", "flavite-text-search"),
        ("flavite-syntax", "flavite-text-search"),
        ("flavite-index", "flavite-text-search"),
        ("flavite-workspace", "flavite-encoding"),
    ] {
        let edges = seed(from, to);
        let violations = ownership_violations(&edges);
        assert!(
            violations
                .iter()
                .any(|violation| violation.contains(from) && violation.contains(to)),
            "fixture {from} → {to} must be rejected, got {violations:?}"
        );
    }
    // An outbound first-party edge from either new crate is rejected.
    for (from, to) in [
        ("flavite-encoding", "flavite-buffer"),
        ("flavite-text-search", "flavite-buffer"),
        ("flavite-text-search", "flavite-vault"),
    ] {
        let mut edges = Edges::new();
        edges.insert(from.to_owned(), vec![to.to_owned()]);
        let violations = ownership_violations(&edges);
        assert!(
            violations.iter().any(|violation| violation.contains(from)),
            "fixture {from} → {to} must be rejected, got {violations:?}"
        );
    }
}

#[test]
fn search_crate_declares_no_duplicate_domain_types() {
    // The matcher must not own a second coordinate/edit model: no
    // SourceEdit/SourcePatch/TextEdit/EditOp/Selection/Cursor types (§4.1
    // canonical data contracts; the S01 source gate).
    let root = std::env::current_dir().expect("cwd");
    let root = root.ancestors().nth(1).expect("workspace root");
    let src = root.join("crates/flavite-text-search/src");
    let mut checked = 0;
    for entry in std::fs::read_dir(&src).expect("text-search src") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|extension| extension != "rs") {
            continue;
        }
        let content = std::fs::read_to_string(&path).expect("read");
        for line in content.lines() {
            let trimmed = line.trim_start();
            for forbidden in [
                "pub struct SourceEdit",
                "pub struct SourcePatch",
                "pub struct TextEdit",
                "pub struct EditOp",
                "pub struct Selection",
                "pub struct Cursor",
                "pub enum SourceEdit",
                "pub enum SourcePatch",
                "pub enum TextEdit",
                "pub enum EditOp",
                "pub enum Selection",
                "pub enum Cursor",
            ] {
                assert!(
                    !trimmed.starts_with(forbidden),
                    "{} declares a duplicate domain type: {forbidden}",
                    path.display()
                );
            }
        }
        checked += 1;
    }
    assert!(
        checked >= 3,
        "text-search sources were enumerated: {checked}"
    );
}
