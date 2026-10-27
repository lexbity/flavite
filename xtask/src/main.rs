//! flavite xtask — development tooling (§10.2).
//!
//! Phase 0.0 slice A1 ships a usage-printing binary. Subcommands land in A4.

fn print_usage() {
    eprintln!("flavite xtask — development tooling");
    eprintln!();
    eprintln!("Usage: cargo xtask <command>");
    eprintln!();
    eprintln!("Commands (implemented in Phase 0.0 slice A4):");
    eprintln!("  arch gen                 Generate crate-graph.json + DEPENDENCIES.md (§10.2)");
    eprintln!("  arch check               Verify drift, edges, and layering (§10.2)");
    eprintln!("  arch deps <crate>        List a crate's dependencies (JSON)");
    eprintln!("  arch dependents <crate>  List a crate's dependents (JSON)");
    eprintln!("  arch path <from> <to>    Find a dependency path (JSON)");
    eprintln!("  arch why <crate>         Show a crate's tier + responsibility (JSON)");
    eprintln!("  arch violations          List layering violations (JSON)");
    eprintln!("  cov-check                Check coverage floors (§10.5)");
    eprintln!();
    eprintln!("Phase 0.0 slice A1: usage text only. Subcommands land in A4.");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        print_usage();
        std::process::exit(2);
    }
    eprintln!(
        "error: subcommand '{}' is not implemented yet (lands in Phase 0.0 slice A4)",
        args[1]
    );
    eprintln!();
    print_usage();
    std::process::exit(2);
}
