//! S3 filesystem-semantics probe — thin CLI over `xtask::spike_s3_fs`
//! (PLAN-C slice C2, §9 Phase 0 spike S3-2, D6/§4.8/§4.12).
//!
//! Records per-OS filesystem semantics (rename-over, symlink hazard, fsync,
//! cloud placeholders, advisory locks after kill -9, native identity) as
//! evidence for the `flavite-fs` `Fs` trait contracts (C5) and the
//! instance-lock design (C8). Findings land in
//! `devdocs/spikes/S3-fs.md`.
//!
//! Usage:
//!   s3_fs_probe [--scenarios all|1..7] [--fsync-dir PATH]
//!               [--self-check] [--json PATH] [--markdown]
//!
//! `--self-check` asserts the properties the production wrappers rely on and
//! exits nonzero on failure. CI re-runs it on Linux headless. A hidden child
//! mode (`--hold-lock PATH`) is spawned by the advisory-lock scenario; it
//! acquires the lock, announces `LOCKED`, and sleeps until killed.

use std::path::PathBuf;
use std::process::ExitCode;

use xtask::spike_s3_fs::{self, ScenarioSelection};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("s3_fs_probe: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<ExitCode> {
    // Hidden child mode (spawned by the advisory-lock scenario).
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(idx) = args.iter().position(|a| a == "--hold-lock") {
        let path = args
            .get(idx + 1)
            .ok_or_else(|| anyhow::anyhow!("--hold-lock requires a path"))?;
        spike_s3_fs::hold_lock_child(std::path::Path::new(path))?;
        return Ok(ExitCode::SUCCESS);
    }

    let mut selection = ScenarioSelection {
        rename_over: true,
        symlink: true,
        fsync: true,
        placeholder: true,
        lock: true,
        identity: true,
        no_clobber: true,
    };
    let mut self_check = false;
    let mut json_path: Option<PathBuf> = None;
    let mut markdown = false;
    let mut extra_fsync_dir: Option<PathBuf> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--scenarios" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--scenarios requires a value"))?;
                selection = spike_s3_fs::parse_selection(value)?;
            }
            "--fsync-dir" => {
                let value = iter
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--fsync-dir requires a path"))?;
                extra_fsync_dir = Some(PathBuf::from(value));
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
                    "usage: s3_fs_probe [--scenarios all|1..7] [--fsync-dir PATH] \
                     [--self-check] [--json PATH] [--markdown]"
                );
                return Ok(ExitCode::SUCCESS);
            }
            other => anyhow::bail!("unknown argument '{other}'"),
        }
    }

    let vault = xtask::spike_s3::TempVault::new("fs-probe");
    eprintln!("probe vault: {}", vault.path().display());
    let summary =
        spike_s3_fs::run_all(vault.path(), &selection, extra_fsync_dir.as_deref())?;
    print!("{}", spike_s3_fs::render_summary(&summary));

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
        let failures = spike_s3_fs::self_check(&summary);
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
