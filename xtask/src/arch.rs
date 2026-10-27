//! Project-RAG: architecture knowledge and verification (§10.2).
//!
//! - `arch gen` — reads `cargo metadata`, writes `crate-graph.json` + `DEPENDENCIES.md`.
//! - `arch check` — drift (regenerate + diff), edges (actual == expected), layering
//!   (transitive closure of core-tier normal+build deps must not contain
//!   iced/iced_*/winit/wgpu/cosmic-text).
//! - Query CLI: `deps`, `dependents`, `path`, `why`, `violations` — JSON on stdout.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Command;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CargoMetadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct Package {
    name: String,
    id: String,
    dependencies: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
struct Dependency {
    name: String,
    kind: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CrateGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Node {
    pub id: String,
    pub kind: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
}

#[derive(Debug, Deserialize)]
struct CratesToml {
    #[serde(default)]
    tiers: Tiers,
    #[serde(default)]
    responsibilities: HashMap<String, String>,
    #[serde(default)]
    edges: EdgesSection,
    #[serde(default)]
    layering: LayeringSection,
    #[serde(default)]
    first_party_externals: FirstPartyExternals,
}

#[derive(Debug, Deserialize, Default)]
struct Tiers {
    #[serde(default)]
    tier1: Vec<String>,
    #[serde(default)]
    tier2: Vec<String>,
    #[serde(default)]
    tier3: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct EdgesSection {
    #[serde(default)]
    active: Vec<ActiveEdge>,
}

#[derive(Debug, Deserialize)]
struct ActiveEdge {
    from: String,
    to: String,
}

#[derive(Debug, Deserialize, Default)]
struct LayeringSection {
    #[serde(default)]
    core_tier: Vec<String>,
    #[serde(default)]
    forbidden: Vec<String>,
    #[serde(default)]
    ui_crates: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct FirstPartyExternals {
    #[serde(default)]
    names: Vec<String>,
}

// ---------------------------------------------------------------------------
// cargo metadata
// ---------------------------------------------------------------------------

fn run_cargo_metadata(workspace_root: &Path) -> Result<CargoMetadata> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--all-features"])
        .current_dir(workspace_root)
        .output()
        .context("failed to run cargo metadata")?;
    if !output.status.success() {
        anyhow::bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: CargoMetadata =
        serde_json::from_slice(&output.stdout).context("failed to parse cargo metadata JSON")?;
    Ok(metadata)
}

// ---------------------------------------------------------------------------
// Graph construction
// ---------------------------------------------------------------------------

fn build_graph(metadata: &CargoMetadata, first_party: &BTreeSet<String>) -> CrateGraph {
    let workspace_set: HashSet<&str> = metadata
        .workspace_members
        .iter()
        .map(|id| {
            // Package IDs look like "path+file:///...#name@version" or "registry+...#name@version"
            // We match by finding the package with this ID and using its name.
            metadata
                .packages
                .iter()
                .find(|p| &p.id == id)
                .map(|p| p.name.as_str())
                .unwrap_or("")
        })
        .collect();

    let mut nodes: Vec<Node> = Vec::new();
    let mut edges: Vec<Edge> = Vec::new();
    let mut seen_nodes: HashSet<String> = HashSet::new();

    // Add workspace crate nodes.
    for pkg in &metadata.packages {
        if workspace_set.contains(pkg.name.as_str()) && seen_nodes.insert(pkg.name.clone()) {
            nodes.push(Node {
                id: pkg.name.clone(),
                kind: "workspace".to_string(),
            });
        }
    }

    // Add edges and external nodes.
    for pkg in &metadata.packages {
        if !workspace_set.contains(pkg.name.as_str()) {
            continue;
        }
        for dep in &pkg.dependencies {
            let kind = match dep.kind.as_deref() {
                Some("build") => "build",
                Some("dev") => "dev",
                _ => "normal",
            };
            // Only include edges to workspace crates or first-party externals.
            let is_workspace = workspace_set.contains(dep.name.as_str());
            let is_first_party = first_party.contains(&dep.name);
            if !is_workspace && !is_first_party {
                continue;
            }
            // Add external node if first-party.
            if !is_workspace && seen_nodes.insert(dep.name.clone()) {
                nodes.push(Node {
                    id: dep.name.clone(),
                    kind: "external".to_string(),
                });
            }
            edges.push(Edge {
                from: pkg.name.clone(),
                to: dep.name.clone(),
                kind: kind.to_string(),
            });
        }
    }

    // Sort for deterministic output.
    nodes.sort_by(|a, b| a.id.cmp(&b.id));
    edges.sort();

    CrateGraph { nodes, edges }
}

// ---------------------------------------------------------------------------
// crates.toml loading
// ---------------------------------------------------------------------------

fn load_crates_toml(workspace_root: &Path) -> Result<CratesToml> {
    let path = workspace_root.join("devdocs/architecture/crates.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).context("failed to parse crates.toml")
}

// ---------------------------------------------------------------------------
// DEPENDENCIES.md rendering
// ---------------------------------------------------------------------------

fn render_dependencies_md(
    graph: &CrateGraph,
    crates_toml: &CratesToml,
    coverage_toml: Option<&CoverageToml>,
) -> String {
    let mut md = String::new();
    md.push_str("<!-- GENERATED by `cargo xtask arch gen` — do not hand-edit. (§10.2) -->\n\n");
    md.push_str("# Crate dependency graph\n\n");
    md.push_str("Generated from `cargo metadata --format-version 1 --all-features`.\n\n");

    // Workspace crates table.
    md.push_str("## Workspace crates\n\n");
    md.push_str("| Crate | Tier | Responsibility |\n");
    md.push_str("|---|---|---|\n");
    let mut workspace_nodes: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == "workspace")
        .collect();
    workspace_nodes.sort_by(|a, b| a.id.cmp(&b.id));
    for node in &workspace_nodes {
        let tier = tier_of(&node.id, crates_toml);
        let resp = crates_toml
            .responsibilities
            .get(&node.id)
            .map(|s| s.as_str())
            .unwrap_or("—");
        md.push_str(&format!("| `{}` | {} | {} |\n", node.id, tier, resp));
    }

    // Dependencies per crate.
    md.push_str("\n## Dependencies\n\n");
    for node in &workspace_nodes {
        let deps: Vec<&Edge> = graph
            .edges
            .iter()
            .filter(|e| e.from == node.id && e.kind != "dev")
            .collect();
        let dev_deps: Vec<&Edge> = graph
            .edges
            .iter()
            .filter(|e| e.from == node.id && e.kind == "dev")
            .collect();
        md.push_str(&format!("### `{}`\n\n", node.id));
        if deps.is_empty() && dev_deps.is_empty() {
            md.push_str("No dependencies.\n\n");
        } else {
            if !deps.is_empty() {
                md.push_str("Normal/build:\n\n");
                for edge in &deps {
                    md.push_str(&format!("- `{}` ({})\n", edge.to, edge.kind));
                }
                md.push('\n');
            }
            if !dev_deps.is_empty() {
                md.push_str("Dev-only:\n\n");
                for edge in &dev_deps {
                    md.push_str(&format!("- `{}`\n", edge.to));
                }
                md.push('\n');
            }
        }
    }

    // External nodes.
    let external_nodes: Vec<&Node> = graph
        .nodes
        .iter()
        .filter(|n| n.kind == "external")
        .collect();
    if !external_nodes.is_empty() {
        md.push_str("## First-party external dependencies\n\n");
        for node in &external_nodes {
            let dependents: Vec<&str> = graph
                .edges
                .iter()
                .filter(|e| e.to == node.id)
                .map(|e| e.from.as_str())
                .collect();
            md.push_str(&format!("- `{}` ← {}\n", node.id, dependents.join(", ")));
        }
        md.push('\n');
    }

    // Layering rules.
    md.push_str("## Layering rules (§10.2)\n\n");
    md.push_str(&format!(
        "Core-tier crates: {}\n\n",
        crates_toml.layering.core_tier.join(", ")
    ));
    md.push_str(&format!(
        "Forbidden in core-tier transitive closure (normal+build): {}\n\n",
        crates_toml.layering.forbidden.join(", ")
    ));
    md.push_str(&format!(
        "UI crates (may depend on forbidden): {}\n\n",
        crates_toml.layering.ui_crates.join(", ")
    ));

    // Coverage floors.
    if let Some(cov) = coverage_toml {
        md.push_str("## Coverage floors (§10.5)\n\n");
        md.push_str("| Tier | Crates | Floor |\n");
        md.push_str("|---|---|---|\n");
        md.push_str(&format!(
            "| 1 — durability-critical | {} | ≥ {:.0}% line |\n",
            crates_toml.tiers.tier1.join(", "),
            cov.floors.tier1
        ));
        md.push_str(&format!(
            "| 2 | {} | ≥ {:.0}% line |\n",
            crates_toml.tiers.tier2.join(", "),
            cov.floors.tier2
        ));
        md.push_str(&format!(
            "| 3 — measured, not gating | {} | reported only |\n",
            crates_toml.tiers.tier3.join(", ")
        ));
    }

    md
}

fn tier_of(crate_name: &str, crates_toml: &CratesToml) -> &'static str {
    if crates_toml.tiers.tier1.contains(&crate_name.to_string()) {
        "1"
    } else if crates_toml.tiers.tier2.contains(&crate_name.to_string()) {
        "2"
    } else if crates_toml.tiers.tier3.contains(&crate_name.to_string()) {
        "3"
    } else {
        "?"
    }
}

// ---------------------------------------------------------------------------
// arch gen
// ---------------------------------------------------------------------------

pub fn generate(workspace_root: &Path) -> Result<()> {
    let metadata = run_cargo_metadata(workspace_root)?;
    let crates_toml = load_crates_toml(workspace_root)?;
    let first_party: BTreeSet<String> = crates_toml
        .first_party_externals
        .names
        .iter()
        .cloned()
        .collect();

    let graph = build_graph(&metadata, &first_party);

    // Write crate-graph.json.
    let arch_dir = workspace_root.join("devdocs/architecture");
    std::fs::create_dir_all(&arch_dir).context("failed to create devdocs/architecture")?;
    let json_path = arch_dir.join("crate-graph.json");
    let json = serde_json::to_string_pretty(&graph).context("failed to serialize graph")?;
    std::fs::write(&json_path, &json)
        .with_context(|| format!("failed to write {}", json_path.display()))?;

    // Load coverage config for rendering.
    let coverage_toml = load_coverage_toml(workspace_root).ok();

    // Render DEPENDENCIES.md.
    let md = render_dependencies_md(&graph, &crates_toml, coverage_toml.as_ref());
    let md_path = arch_dir.join("DEPENDENCIES.md");
    std::fs::write(&md_path, &md)
        .with_context(|| format!("failed to write {}", md_path.display()))?;

    println!("Generated:");
    println!("  {}", json_path.display());
    println!("  {}", md_path.display());
    Ok(())
}

// ---------------------------------------------------------------------------
// arch check
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct CheckReport {
    pub drift: bool,
    pub edges_ok: bool,
    pub layering_violations: Vec<String>,
}

pub fn check(workspace_root: &Path) -> Result<CheckReport> {
    let metadata = run_cargo_metadata(workspace_root)?;
    let crates_toml = load_crates_toml(workspace_root)?;
    let first_party: BTreeSet<String> = crates_toml
        .first_party_externals
        .names
        .iter()
        .cloned()
        .collect();

    let graph = build_graph(&metadata, &first_party);

    // Check 1: Drift — regenerate and compare.
    let coverage_toml = load_coverage_toml(workspace_root).ok();
    let expected_json =
        serde_json::to_string_pretty(&graph).context("failed to serialize graph")?;
    let expected_md = render_dependencies_md(&graph, &crates_toml, coverage_toml.as_ref());

    let json_path = workspace_root.join("devdocs/architecture/crate-graph.json");
    let md_path = workspace_root.join("devdocs/architecture/DEPENDENCIES.md");

    let mut drift = false;
    if let Ok(actual_json) = std::fs::read_to_string(&json_path) {
        if actual_json.trim() != expected_json.trim() {
            drift = true;
            eprintln!(
                "DRIFT: {} is out of date. Run `cargo xtask arch gen`.",
                json_path.display()
            );
        }
    } else {
        drift = true;
        eprintln!(
            "DRIFT: {} is missing. Run `cargo xtask arch gen`.",
            json_path.display()
        );
    }
    if let Ok(actual_md) = std::fs::read_to_string(&md_path) {
        if actual_md.trim() != expected_md.trim() {
            drift = true;
            eprintln!(
                "DRIFT: {} is out of date. Run `cargo xtask arch gen`.",
                md_path.display()
            );
        }
    } else {
        drift = true;
        eprintln!(
            "DRIFT: {} is missing. Run `cargo xtask arch gen`.",
            md_path.display()
        );
    }

    // Check 2: Edges — actual internal edges == expected edges.
    let actual_internal: BTreeSet<(String, String)> = graph
        .edges
        .iter()
        .filter(|e| {
            graph
                .nodes
                .iter()
                .any(|n| n.id == e.from && n.kind == "workspace")
                && graph
                    .nodes
                    .iter()
                    .any(|n| n.id == e.to && n.kind == "workspace")
        })
        .map(|e| (e.from.clone(), e.to.clone()))
        .collect();

    let expected_internal: BTreeSet<(String, String)> = crates_toml
        .edges
        .active
        .iter()
        .map(|e| (e.from.clone(), e.to.clone()))
        .collect();

    let edges_ok = actual_internal == expected_internal;
    if !edges_ok {
        let missing: Vec<_> = expected_internal.difference(&actual_internal).collect();
        let unexpected: Vec<_> = actual_internal.difference(&expected_internal).collect();
        if !missing.is_empty() {
            eprintln!("EDGES: expected but not present:");
            for (from, to) in &missing {
                eprintln!("  {from} → {to}");
            }
        }
        if !unexpected.is_empty() {
            eprintln!("EDGES: present but not expected (update crates.toml):");
            for (from, to) in &unexpected {
                eprintln!("  {from} → {to}");
            }
        }
    }

    // Check 3: Layering — transitive closure of core-tier normal+build deps
    // must not contain forbidden crates.
    let layering_violations = check_layering(&graph, &crates_toml);

    Ok(CheckReport {
        drift,
        edges_ok,
        layering_violations,
    })
}

fn check_layering(graph: &CrateGraph, crates_toml: &CratesToml) -> Vec<String> {
    let mut violations = Vec::new();

    // Build adjacency map for normal+build edges only.
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        if edge.kind == "normal" || edge.kind == "build" {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
    }

    for core_crate in &crates_toml.layering.core_tier {
        // BFS for transitive closure.
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        queue.push_back(core_crate.as_str());
        visited.insert(core_crate.as_str());

        while let Some(current) = queue.pop_front() {
            if let Some(deps) = adjacency.get(current) {
                for &dep in deps {
                    if visited.insert(dep) {
                        queue.push_back(dep);
                    }
                }
            }
        }

        // Check if any forbidden crate is in the closure.
        for forbidden_pattern in &crates_toml.layering.forbidden {
            for visited_crate in &visited {
                if forbidden_pattern.ends_with('*') {
                    let prefix = &forbidden_pattern[..forbidden_pattern.len() - 1];
                    if visited_crate.starts_with(prefix) {
                        violations.push(format!(
                            "{core_crate} → {visited_crate} (matches forbidden pattern {forbidden_pattern})"
                        ));
                    }
                } else if *visited_crate == forbidden_pattern {
                    violations.push(format!(
                        "{core_crate} → {visited_crate} (forbidden: {forbidden_pattern})"
                    ));
                }
            }
        }
    }

    violations.sort();
    violations.dedup();
    violations
}

// ---------------------------------------------------------------------------
// Query CLI
// ---------------------------------------------------------------------------

pub fn query(workspace_root: &Path, subcommand: &str, args: &[String]) -> Result<()> {
    let metadata = run_cargo_metadata(workspace_root)?;
    let crates_toml = load_crates_toml(workspace_root)?;
    let first_party: BTreeSet<String> = crates_toml
        .first_party_externals
        .names
        .iter()
        .cloned()
        .collect();
    let graph = build_graph(&metadata, &first_party);

    match subcommand {
        "deps" => {
            let crate_name = args.first().context("usage: arch deps <crate>")?;
            let deps: Vec<&Edge> = graph
                .edges
                .iter()
                .filter(|e| &e.from == crate_name)
                .collect();
            println!("{}", serde_json::to_string_pretty(&deps)?);
        }
        "dependents" => {
            let crate_name = args.first().context("usage: arch dependents <crate>")?;
            let deps: Vec<&Edge> = graph.edges.iter().filter(|e| &e.to == crate_name).collect();
            println!("{}", serde_json::to_string_pretty(&deps)?);
        }
        "path" => {
            let from = args.first().context("usage: arch path <from> <to>")?;
            let to = args.get(1).context("usage: arch path <from> <to>")?;
            let path = find_path(&graph, from, to);
            println!("{}", serde_json::to_string_pretty(&path)?);
        }
        "why" => {
            let crate_name = args.first().context("usage: arch why <crate>")?;
            let tier = tier_of(crate_name, &crates_toml);
            let resp = crates_toml
                .responsibilities
                .get(crate_name)
                .cloned()
                .unwrap_or_else(|| "—".to_string());
            let info = serde_json::json!({
                "crate": crate_name,
                "tier": tier,
                "responsibility": resp,
            });
            println!("{}", serde_json::to_string_pretty(&info)?);
        }
        "violations" => {
            let violations = check_layering(&graph, &crates_toml);
            println!("{}", serde_json::to_string_pretty(&violations)?);
        }
        other => {
            anyhow::bail!("unknown arch query subcommand: {other}");
        }
    }
    Ok(())
}

fn find_path(graph: &CrateGraph, from: &str, to: &str) -> Option<Vec<String>> {
    // BFS for shortest path.
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &graph.edges {
        if edge.kind == "normal" || edge.kind == "build" {
            adjacency
                .entry(edge.from.as_str())
                .or_default()
                .push(edge.to.as_str());
        }
    }

    let mut visited: HashSet<&str> = HashSet::new();
    let mut queue: VecDeque<(Vec<String>, &str)> = VecDeque::new();
    queue.push_back((vec![from.to_string()], from));
    visited.insert(from);

    while let Some((path, current)) = queue.pop_front() {
        if current == to {
            return Some(path);
        }
        if let Some(deps) = adjacency.get(current) {
            for &dep in deps {
                if visited.insert(dep) {
                    let mut new_path = path.clone();
                    new_path.push(dep.to_string());
                    queue.push_back((new_path, dep));
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Coverage config (shared with cov.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct CoverageToml {
    #[serde(default)]
    pub floors: CoverageFloors,
    #[serde(default)]
    pub crates: HashMap<String, String>,
}

#[derive(Debug, Deserialize, Default)]
pub struct CoverageFloors {
    #[serde(default = "default_tier1")]
    pub tier1: f64,
    #[serde(default = "default_tier2")]
    pub tier2: f64,
    #[serde(default)]
    pub tier3: f64,
}

fn default_tier1() -> f64 {
    90.0
}
fn default_tier2() -> f64 {
    80.0
}

pub fn load_coverage_toml(workspace_root: &Path) -> Result<CoverageToml> {
    let path = workspace_root.join("devdocs/architecture/coverage.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    toml::from_str(&content).context("failed to parse coverage.toml")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_sorting_is_deterministic() {
        let mut edges = [
            Edge {
                from: "b".into(),
                to: "a".into(),
                kind: "normal".into(),
            },
            Edge {
                from: "a".into(),
                to: "b".into(),
                kind: "normal".into(),
            },
            Edge {
                from: "a".into(),
                to: "c".into(),
                kind: "dev".into(),
            },
        ];
        edges.sort();
        assert_eq!(edges[0].from, "a");
        assert_eq!(edges[0].to, "b");
        assert_eq!(edges[1].from, "a");
        assert_eq!(edges[1].to, "c");
        assert_eq!(edges[2].from, "b");
    }

    #[test]
    fn layering_check_detects_direct_forbidden() {
        let graph = CrateGraph {
            nodes: vec![
                Node {
                    id: "flavite-fs".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "iced".into(),
                    kind: "external".into(),
                },
            ],
            edges: vec![Edge {
                from: "flavite-fs".into(),
                to: "iced".into(),
                kind: "normal".into(),
            }],
        };
        let crates_toml = CratesToml {
            tiers: Tiers::default(),
            responsibilities: HashMap::new(),
            edges: EdgesSection::default(),
            layering: LayeringSection {
                core_tier: vec!["flavite-fs".into()],
                forbidden: vec!["iced".into()],
                ui_crates: vec![],
            },
            first_party_externals: FirstPartyExternals::default(),
        };
        let violations = check_layering(&graph, &crates_toml);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("iced"));
    }

    #[test]
    fn layering_check_detects_transitive_forbidden() {
        let graph = CrateGraph {
            nodes: vec![
                Node {
                    id: "flavite-fs".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "flavite-syntax".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "winit".into(),
                    kind: "external".into(),
                },
            ],
            edges: vec![
                Edge {
                    from: "flavite-fs".into(),
                    to: "flavite-syntax".into(),
                    kind: "normal".into(),
                },
                Edge {
                    from: "flavite-syntax".into(),
                    to: "winit".into(),
                    kind: "normal".into(),
                },
            ],
        };
        let crates_toml = CratesToml {
            tiers: Tiers::default(),
            responsibilities: HashMap::new(),
            edges: EdgesSection::default(),
            layering: LayeringSection {
                core_tier: vec!["flavite-fs".into(), "flavite-syntax".into()],
                forbidden: vec!["winit".into()],
                ui_crates: vec![],
            },
            first_party_externals: FirstPartyExternals::default(),
        };
        let violations = check_layering(&graph, &crates_toml);
        // Both flavite-fs (transitive) and flavite-syntax (direct) violate.
        assert!(!violations.is_empty());
        assert!(violations.iter().any(|v| v.contains("flavite-fs")));
        assert!(violations.iter().any(|v| v.contains("flavite-syntax")));
    }

    #[test]
    fn layering_check_ignores_dev_deps() {
        let graph = CrateGraph {
            nodes: vec![
                Node {
                    id: "flavite-fs".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "iced".into(),
                    kind: "external".into(),
                },
            ],
            edges: vec![Edge {
                from: "flavite-fs".into(),
                to: "iced".into(),
                kind: "dev".into(),
            }],
        };
        let crates_toml = CratesToml {
            tiers: Tiers::default(),
            responsibilities: HashMap::new(),
            edges: EdgesSection::default(),
            layering: LayeringSection {
                core_tier: vec!["flavite-fs".into()],
                forbidden: vec!["iced".into()],
                ui_crates: vec![],
            },
            first_party_externals: FirstPartyExternals::default(),
        };
        let violations = check_layering(&graph, &crates_toml);
        assert!(violations.is_empty(), "dev-deps are exempt from layering");
    }

    #[test]
    fn layering_check_wildcard_pattern() {
        let graph = CrateGraph {
            nodes: vec![
                Node {
                    id: "flavite-fs".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "iced_wgpu".into(),
                    kind: "external".into(),
                },
            ],
            edges: vec![Edge {
                from: "flavite-fs".into(),
                to: "iced_wgpu".into(),
                kind: "normal".into(),
            }],
        };
        let crates_toml = CratesToml {
            tiers: Tiers::default(),
            responsibilities: HashMap::new(),
            edges: EdgesSection::default(),
            layering: LayeringSection {
                core_tier: vec!["flavite-fs".into()],
                forbidden: vec!["iced_*".into()],
                ui_crates: vec![],
            },
            first_party_externals: FirstPartyExternals::default(),
        };
        let violations = check_layering(&graph, &crates_toml);
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn find_path_direct() {
        let graph = CrateGraph {
            nodes: vec![
                Node {
                    id: "a".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "b".into(),
                    kind: "workspace".into(),
                },
            ],
            edges: vec![Edge {
                from: "a".into(),
                to: "b".into(),
                kind: "normal".into(),
            }],
        };
        let path = find_path(&graph, "a", "b");
        assert_eq!(path, Some(vec!["a".to_string(), "b".to_string()]));
    }

    #[test]
    fn find_path_transitive() {
        let graph = CrateGraph {
            nodes: vec![
                Node {
                    id: "a".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "b".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "c".into(),
                    kind: "workspace".into(),
                },
            ],
            edges: vec![
                Edge {
                    from: "a".into(),
                    to: "b".into(),
                    kind: "normal".into(),
                },
                Edge {
                    from: "b".into(),
                    to: "c".into(),
                    kind: "normal".into(),
                },
            ],
        };
        let path = find_path(&graph, "a", "c");
        assert_eq!(
            path,
            Some(vec!["a".to_string(), "b".to_string(), "c".to_string()])
        );
    }

    #[test]
    fn find_path_none() {
        let graph = CrateGraph {
            nodes: vec![
                Node {
                    id: "a".into(),
                    kind: "workspace".into(),
                },
                Node {
                    id: "b".into(),
                    kind: "workspace".into(),
                },
            ],
            edges: vec![],
        };
        let path = find_path(&graph, "a", "b");
        assert_eq!(path, None);
    }

    #[test]
    fn tier_of_returns_correct_tier() {
        let crates_toml = CratesToml {
            tiers: Tiers {
                tier1: vec!["flavite-fs".into()],
                tier2: vec!["flavite-api".into()],
                tier3: vec!["flavite-ui".into()],
            },
            responsibilities: HashMap::new(),
            edges: EdgesSection::default(),
            layering: LayeringSection::default(),
            first_party_externals: FirstPartyExternals::default(),
        };
        assert_eq!(tier_of("flavite-fs", &crates_toml), "1");
        assert_eq!(tier_of("flavite-api", &crates_toml), "2");
        assert_eq!(tier_of("flavite-ui", &crates_toml), "3");
        assert_eq!(tier_of("unknown", &crates_toml), "?");
    }
}
