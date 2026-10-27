//! flavite xtask library — public API for integration tests (§10.2, §10.5, §10.10).
//!
//! The binary (`src/main.rs`) is a thin wrapper over this library.

pub mod arch;
pub mod clean;
pub mod cov;
pub mod s1_caret_probe;
pub mod s1_probe;
pub mod setup;
pub mod spike_s3;
pub mod spike_s3_fs;

use anyhow::{Context, Result};

pub fn print_usage() {
    eprintln!("flavite xtask — development tooling");
    eprintln!();
    eprintln!("Usage: cargo xtask <command> [args]");
    eprintln!();
    eprintln!("Commands:");
    eprintln!("  arch gen                 Generate crate-graph.json + DEPENDENCIES.md (§10.2)");
    eprintln!("  arch check               Verify drift, edges, and layering (§10.2)");
    eprintln!("  arch deps <crate>        List a crate's dependencies (JSON)");
    eprintln!("  arch dependents <crate>  List a crate's dependents (JSON)");
    eprintln!("  arch path <from> <to>    Find a dependency path (JSON)");
    eprintln!("  arch why <crate>         Show a crate's tier + responsibility (JSON)");
    eprintln!("  arch violations          List layering violations (JSON)");
    eprintln!(
        "  cov-check [--lcov <path>] [--ratchet <main-lcov>]   Check coverage floors (§10.5)"
    );
    eprintln!("  clean [--dry-run]        Full cleanup of build output (§10.10)");
    eprintln!("  gc                       Post-gate threshold-based cleanup (§10.10)");
    eprintln!("  setup                    Developer onboarding: tools + hook (§10.8)");
}

pub fn run(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        print_usage();
        std::process::exit(2);
    }

    match args[1].as_str() {
        "clean" => {
            let dry_run = args.iter().any(|a| a == "--dry-run");
            let root = clean::find_workspace_root()?;
            let report = clean::clean(&root, dry_run)?;
            report.print();
            Ok(())
        }
        "gc" => {
            let root = clean::find_workspace_root()?;
            let report = clean::gc(&root)?;
            if !report.paths_removed.is_empty() {
                report.print();
            }
            Ok(())
        }
        "setup" => setup::setup(),
        "arch" => handle_arch(&args[2..]),
        "cov-check" => handle_cov_check(&args[2..]),
        other => {
            eprintln!("error: unknown command '{other}'");
            eprintln!();
            print_usage();
            std::process::exit(2);
        }
    }
}

fn handle_arch(args: &[String]) -> Result<()> {
    if args.is_empty() {
        eprintln!("error: arch requires a subcommand");
        eprintln!("  arch gen|check|deps|dependents|path|why|violations");
        std::process::exit(2);
    }
    let root = clean::find_workspace_root()?;
    match args[0].as_str() {
        "gen" => arch::generate(&root),
        "check" => {
            let report = arch::check(&root)?;
            let layering_ok = report.layering_violations.is_empty();
            if !layering_ok {
                eprintln!("LAYERING violations:");
                for v in &report.layering_violations {
                    eprintln!("  {v}");
                }
            }
            if report.drift || !report.edges_ok || !layering_ok {
                eprintln!();
                eprintln!("arch check FAILED:");
                if report.drift {
                    eprintln!(
                        "  - drift: generated artifacts out of date (run `cargo xtask arch gen`)"
                    );
                }
                if !report.edges_ok {
                    eprintln!("  - edges: actual internal edges != expected (update crates.toml)");
                }
                if !layering_ok {
                    eprintln!(
                        "  - layering: {} violation(s)",
                        report.layering_violations.len()
                    );
                }
                std::process::exit(1);
            }
            println!("arch check passed (drift + edges + layering)");
            Ok(())
        }
        "deps" | "dependents" | "path" | "why" | "violations" => {
            arch::query(&root, &args[0], &args[1..])
        }
        other => {
            eprintln!("error: unknown arch subcommand '{other}'");
            std::process::exit(2);
        }
    }
}

fn handle_cov_check(args: &[String]) -> Result<()> {
    let root = clean::find_workspace_root()?;
    let mut lcov_path = root.join("target/llvm-cov/lcov.info");
    let mut ratchet_path: Option<std::path::PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--lcov" => {
                i += 1;
                lcov_path =
                    std::path::PathBuf::from(args.get(i).context("--lcov requires a path")?);
            }
            "--ratchet" => {
                i += 1;
                ratchet_path = Some(std::path::PathBuf::from(
                    args.get(i).context("--ratchet requires a path")?,
                ));
            }
            other => {
                eprintln!("error: unknown cov-check option '{other}'");
                std::process::exit(2);
            }
        }
        i += 1;
    }

    println!("Coverage check ({})", lcov_path.display());
    let report = cov::check(&lcov_path, &root, ratchet_path.as_deref())?;
    if !report.failures.is_empty() {
        eprintln!();
        eprintln!("cov-check FAILED:");
        for f in &report.failures {
            eprintln!("  {f}");
        }
        std::process::exit(1);
    }
    println!("cov-check passed");
    Ok(())
}
