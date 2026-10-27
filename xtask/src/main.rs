//! flavite xtask — development tooling (§10.2, §10.8, §10.10).
//!
//! Subcommands:
//! - `arch gen|check|deps|dependents|path|why|violations` — project-RAG (§10.2, A4)
//! - `cov-check` — coverage floors (§10.5, A4)
//! - `clean [--dry-run]` — full cleanup (§10.10, A3)
//! - `gc` — post-gate threshold-based cleanup (§10.10, A3)
//! - `setup` — developer onboarding (§10.8, A3)

mod clean;
mod setup;

use anyhow::Result;

fn print_usage() {
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

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
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
        "arch" => {
            eprintln!("error: 'arch' subcommands land in Phase 0.0 slice A4");
            std::process::exit(2);
        }
        "cov-check" => {
            eprintln!("error: 'cov-check' lands in Phase 0.0 slice A4");
            std::process::exit(2);
        }
        other => {
            eprintln!("error: unknown command '{other}'");
            eprintln!();
            print_usage();
            std::process::exit(2);
        }
    }
}
