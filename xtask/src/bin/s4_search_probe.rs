//! S4 search probe — thin CLI over `xtask::spike_s4_search`
//! (PLAN-C slice C3, §9 Phase 0 spike S4, D12/§4.7/§4.14/AC-15).
//!
//! Measures CJK bigram-tokenizer quality against a plain pipeline, deferred
//! commit behavior under 1 s autosave load (commit-before-search latency,
//! commit frequency), `committed_generation` replay after a simulated crash, and
//! atomic checkpoint publication across the §4.14 crash boundaries — evidence
//! for the D12 go/no-go on tantivy vs an FTS5 reversal (C7's `SearchBackend`
//! trait is the swap point either way). Findings land in
//! `devdocs/spikes/S4-search.md`.
//!
//! Usage:
//!   s4_search_probe [--scenarios all|1..4] [--notes N] [--self-check]
//!                   [--json PATH] [--markdown]
//!
//! `--notes` scales the fixture (full 20k; CI passes 2000). `--self-check`
//! asserts the properties the production index adapter relies on and exits
//! nonzero on failure; CI re-runs it on Linux headless.

use std::path::PathBuf;
use std::process::ExitCode;

use xtask::spike_s4_search::{self, ScenarioSelection};

/// Full-probe fixture size (§9 Phase 0 spike S4: 20k notes).
const DEFAULT_NOTES: usize = 20_000;

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("s4_search_probe: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut selection = ScenarioSelection {
        quality: true,
        autosave: true,
        replay: true,
        publication: true,
    };
    let mut notes = DEFAULT_NOTES;
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
                selection = spike_s4_search::parse_selection(value)?;
            }
            "--notes" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--notes requires a value"))?;
                notes = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--notes must be a positive integer"))?;
                if notes == 0 {
                    anyhow::bail!("--notes must be a positive integer");
                }
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
                    "usage: s4_search_probe [--scenarios all|1..4] [--notes N] \
                     [--self-check] [--json PATH] [--markdown]"
                );
                return Ok(ExitCode::SUCCESS);
            }
            other => anyhow::bail!("unknown argument '{other}'"),
        }
    }

    eprintln!("building {notes}-note fixture");
    let fixture = spike_s4_search::build_fixture(notes);

    let vault = xtask::spike_s3::TempVault::new("search-probe");
    eprintln!("probe vault: {}", vault.path().display());
    let summary = spike_s4_search::run_all(vault.path(), &fixture, &selection)?;
    print!("{}", spike_s4_search::render_summary(&summary));

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
        let failures = spike_s4_search::self_check(&summary);
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
