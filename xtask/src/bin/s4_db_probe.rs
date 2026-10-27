//! S4 DB probe — thin CLI over `xtask::spike_s4_db`
//! (PLAN-C slice C4, §9 Phase 0 spike S4, D3/D12/§4.14/§8 AC-15).
//!
//! Simulates a bulk reconcile on one SQLite writer connection while a pool of
//! read-only WAL connections serves point / invalidation-feed / metadata
//! reads, then sweeps the read-pool size and the reconcile batch size, times
//! the generation-bump-only transaction at both synchronous tiers, interleaves
//! tantivy applies on the single writer with one deferred checkpointed commit,
//! and proves the held-snapshot rules C7's read pool must respect. Findings
//! land in `devdocs/spikes/S4-db.md`.
//!
//! Usage:
//!   s4_db_probe [--scenarios all|1..5] [--notes N] [--dir PATH]
//!               [--batch N] [--readers N] [--self-check] [--json PATH]
//!               [--markdown]
//!
//! `--notes` scales the fixture (full 20k; CI passes 2000). `--dir` picks the
//! working directory (defaults to a temp vault; point it at a real filesystem
//! so the WAL fsync cost is representative — the report records the fs).
//! `--self-check` asserts the properties the production wrappers rely on and
//! exits nonzero on failure; CI re-runs it on Linux headless.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use xtask::spike_s4_db::{self, DEFAULT_BATCH, DEFAULT_READERS, ScenarioSelection};
use xtask::spike_s4_search::build_fixture;

/// Full-probe fixture size (§9 Phase 0 spike S4: 20k notes).
const DEFAULT_NOTES: usize = 20_000;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("s4_db_probe: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut selection = ScenarioSelection {
        bulk: true,
        pool: true,
        batch: true,
        search: true,
        wal: true,
    };
    let mut notes = DEFAULT_NOTES;
    let mut batch = DEFAULT_BATCH;
    let mut readers = DEFAULT_READERS;
    let mut dir: Option<PathBuf> = None;
    let mut self_check = false;
    let mut json_path: Option<PathBuf> = None;
    let mut markdown = false;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--scenarios" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--scenarios requires a value"))?;
                selection = spike_s4_db::parse_selection(value)?;
            }
            "--notes" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--notes requires a value"))?;
                notes = parse_positive(value, "--notes")?;
            }
            "--batch" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--batch requires a value"))?;
                batch = parse_positive(value, "--batch")?;
            }
            "--readers" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--readers requires a value"))?;
                readers = parse_positive(value, "--readers")?;
            }
            "--dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--dir requires a path"))?;
                dir = Some(PathBuf::from(value));
            }
            "--self-check" => self_check = true,
            "--json" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--json requires a path"))?;
                json_path = Some(PathBuf::from(value));
            }
            "--markdown" => markdown = true,
            "--help" | "-h" => {
                println!(
                    "usage: s4_db_probe [--scenarios all|1..5] [--notes N] [--dir PATH] \
                     [--batch N] [--readers N] [--self-check] [--json PATH] [--markdown]"
                );
                return Ok(ExitCode::SUCCESS);
            }
            other => anyhow::bail!("unknown argument '{other}'"),
        }
    }

    // An explicit `--dir` is kept (it is the developer's working directory and
    // may be a real filesystem); otherwise a temp vault cleans up after itself.
    let vault;
    let root: &Path = match &dir {
        Some(path) => {
            std::fs::create_dir_all(path)?;
            path.as_path()
        }
        None => {
            vault = xtask::spike_s3::TempVault::new("db-probe");
            vault.path()
        }
    };

    eprintln!("building {notes}-note fixture");
    let fixture = build_fixture(notes);
    eprintln!("probe dir: {}", root.display());

    let summary = spike_s4_db::run_all(root, &fixture, batch, readers, &selection)?;
    print!("{}", spike_s4_db::render_summary(&summary));

    if markdown {
        println!("{}", summary.render_markdown());
    }
    if let Some(path) = json_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&summary)?)?;
        eprintln!("wrote JSON report: {}", path.display());
    }

    if self_check {
        let failures = spike_s4_db::self_check(&summary);
        if failures.is_empty() {
            eprintln!("self-check PASS: every measured property held");
            Ok(ExitCode::SUCCESS)
        } else {
            for failure in &failures {
                eprintln!("self-check FAIL: {failure}");
            }
            Ok(ExitCode::FAILURE)
        }
    } else {
        Ok(ExitCode::SUCCESS)
    }
}

fn parse_positive(value: &str, flag: &str) -> anyhow::Result<usize> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("{flag} must be a positive integer"))?;
    if parsed == 0 {
        anyhow::bail!("{flag} must be a positive integer");
    }
    Ok(parsed)
}
