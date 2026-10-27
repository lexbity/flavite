//! S3 watcher-truth probe — thin CLI over `xtask::spike_s3` (PLAN-C slice C1,
//! §9 Phase 0 spike S3-1, D5).
//!
//! Records raw `notify` event streams for scripted filesystem scenarios and
//! measures the per-OS watcher behavior D5's parameters are built on. Findings
//! land in `devdocs/spikes/S3-watcher.md`.
//!
//! Usage:
//!   s3_watcher_probe [--scenarios all|1,2,3,4] [--storm-count N]
//!                    [--self-check] [--json PATH] [--markdown]
//!
//! `--self-check` asserts the D5 nomination property (changed paths are
//! eventually nominated, or an explicit overflow/rescan signal is observed)
//! and exits nonzero on failure. CI re-runs scenarios 1–3 with `--self-check`
//! on Linux headless.

use std::path::PathBuf;
use std::process::ExitCode;

use xtask::spike_s3::{self, STORM_FILES, ScenarioSelection, TempVault};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("s3_watcher_probe: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    let mut selection = ScenarioSelection {
        modify: true,
        rename: true,
        storm: true,
        burst: true,
    };
    let mut storm_count = STORM_FILES;
    let mut self_check = false;
    let mut json_path: Option<PathBuf> = None;
    let mut markdown = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scenarios" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--scenarios requires a value"))?;
                selection = spike_s3::parse_selection(&value)?;
            }
            "--storm-count" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--storm-count requires a value"))?;
                storm_count = value
                    .parse()
                    .map_err(|_| anyhow::anyhow!("--storm-count must be a number"))?;
            }
            "--self-check" => self_check = true,
            "--json" => {
                let value = args
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--json requires a path"))?;
                json_path = Some(PathBuf::from(value));
            }
            "--markdown" => markdown = true,
            "--help" | "-h" => {
                println!(
                    "usage: s3_watcher_probe [--scenarios all|1,2,3,4] [--storm-count N] \
                     [--self-check] [--json PATH] [--markdown]"
                );
                return Ok(ExitCode::SUCCESS);
            }
            other => anyhow::bail!("unknown argument '{other}'"),
        }
    }

    let vault = TempVault::new("probe");
    eprintln!("probe vault: {}", vault.path().display());
    let summary = spike_s3::run_all(vault.path(), &selection, storm_count)?;
    print!("{}", spike_s3::render_summary(&summary));

    if markdown {
        println!("\n{}", summary.render_markdown());
    }
    if let Some(path) = json_path {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&summary)?)?;
        eprintln!("wrote JSON report: {}", path.display());
    }

    if self_check {
        let failures = spike_s3::self_check(&summary);
        if failures.is_empty() {
            eprintln!("self-check PASS: the D5 nomination property held in every scenario");
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
