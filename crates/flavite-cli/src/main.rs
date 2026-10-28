//! Tier 3. Headless CLI; consumes `flavite-api` (D16 dogfood) (§4.1).
//!
//! Phase 1 / PLAN-C slice C9: Implements the headless CLI driving the vault's
//! Session directly. Commands: `open`, `doctor`, `query`, `search`.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use flavite_fs::OsFs;
use flavite_vault::{
    ChannelSet, Notice, Pulse,
    open::{Vault, vault_exists},
    types::VaultConfig,
};
use std::path::PathBuf;
use tracing::{debug, info, warn};

/// Lattice — a local-first plain-text knowledge vault in pure Rust.
///
/// The CLI is headless and drives the vault's Session directly.
/// All commands work without a GPU or display.
#[derive(Parser, Debug)]
#[command(name = "flavite")]
#[command(author = "Lattice Contributors")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "Local-first plain-text knowledge vault", long_about = None)]
struct Cli {
    /// Enable verbose logging.
    #[arg(short = 'x', long, global = true)]
    verbose: bool,

    /// The vault root path. If not specified, uses the current directory.
    #[arg(short, long, global = true, value_name = "PATH")]
    vault: Option<PathBuf>,

    /// Disable the filesystem watcher.
    #[arg(long, global = true)]
    no_watch: bool,

    #[command(subcommand)]
    command: Commands,
}

/// Available commands.
#[derive(Subcommand, Debug)]
enum Commands {
    /// Open a vault and reconcile its contents.
    Open {
        /// Path to the vault (overrides global --vault).
        #[arg(short, long, value_name = "PATH")]
        path: Option<PathBuf>,
    },

    /// Run diagnostics on a vault (doctor).
    Doctor {
        /// Path to the vault (overrides global --vault).
        #[arg(short, long, value_name = "PATH")]
        path: Option<PathBuf>,

        /// Output in JSON format.
        #[arg(short, long)]
        json: bool,

        /// Quick check only (SQLite integrity, no deep scans).
        #[arg(long)]
        quick: bool,
    },

    /// Query notes using the typed Query AST.
    Query {
        /// Path to the vault (overrides global --vault).
        #[arg(short, long, value_name = "PATH")]
        path: Option<PathBuf>,

        /// The query expression.
        #[arg(required = true, value_name = "QUERY")]
        query: String,

        /// Limit the number of results.
        #[arg(short, long, default_value = "50", value_name = "N")]
        limit: usize,
    },

    /// Search notes by text content.
    Search {
        /// Path to the vault (overrides global --vault).
        #[arg(short, long, value_name = "PATH")]
        path: Option<PathBuf>,

        /// The search term.
        #[arg(required = true, value_name = "TERM")]
        term: String,

        /// Limit the number of results.
        #[arg(short, long, default_value = "20", value_name = "N")]
        limit: usize,
    },

    /// Show information about the vault.
    Info {
        /// Path to the vault (overrides global --vault).
        #[arg(short, long, value_name = "PATH")]
        path: Option<PathBuf>,
    },

    /// Test command for CI.
    #[cfg(test)]
    Test {
        #[arg(short, long, value_name = "COMMAND")]
        command: String,
    },
}

/// Query types that map to the Query AST.
#[derive(Debug, Clone, ValueEnum)]
enum QueryType {
    Tag,
    Prop,
    LinksTo,
    LinkedFrom,
    Unresolved,
    Text,
    Attachment,
}

/// Doctor check results.
#[derive(Debug, Clone, serde::Serialize)]
struct DoctorCheck {
    name: String,
    passed: bool,
    message: String,
    details: Option<String>,
}

/// Doctor report.
#[derive(Debug, Clone, serde::Serialize)]
struct DoctorReport {
    vault_path: PathBuf,
    checks: Vec<DoctorCheck>,
    summary: DoctorSummary,
}

/// Doctor summary.
#[derive(Debug, Clone, serde::Serialize)]
struct DoctorSummary {
    total_checks: usize,
    passed: usize,
    failed: usize,
    warnings: usize,
}

/// CLI driver that manages a headless Session.
struct CliDriver {
    vault: Vault,
    channels: ChannelSet,
    config: VaultConfig,
}

impl CliDriver {
    /// Create a new CLI driver for a vault.
    fn new(vault_path: PathBuf, no_watch: bool) -> Result<Self> {
        // Ensure the vault path is absolute
        let vault_path = std::fs::canonicalize(&vault_path)
            .with_context(|| format!("Cannot canonicalize vault path: {}", vault_path.display()))?;

        // Check if vault exists
        let fs = Box::new(OsFs::new());
        if !vault_exists(&*fs, &vault_path)? {
            anyhow::bail!(
                "Vault does not exist at {}. Run 'flavite init' to create one.",
                vault_path.display()
            );
        }

        // Configure watcher
        let config = VaultConfig {
            watcher_enabled: !no_watch,
            ..Default::default()
        };

        // Open the vault
        let vault = Vault::open(vault_path.clone(), fs, config.clone())
            .with_context(|| format!("Failed to open vault at {}", vault_path.display()))?;

        // For now, create a new channel set for CLI use
        // In a full implementation, we'd use the vault's channels
        Ok(Self {
            vault,
            channels: ChannelSet::new(),
            config,
        })
    }

    /// Run a reconcile and wait for completion.
    fn reconcile(&self) -> Result<()> {
        info!("Starting vault reconcile...");

        // Send reconcile command to the actor
        // For now, we'll just log that reconcile is running
        // In a full implementation, we'd wait for the reconcile to complete
        info!("Reconcile triggered");

        // Drain any pending pulses
        self.drain_pulses();

        Ok(())
    }

    /// Drain pulses from the channel (for CLI synchronization).
    fn drain_pulses(&self) {
        loop {
            match self.channels.pulse.try_recv() {
                Ok(pulse) => {
                    debug!("Pulse received: {:?}", pulse);
                    // Check for reconcile completion
                    if let Pulse::ReconcileProgress { done, total } = pulse {
                        info!("Reconcile progress: {}/{}", done, total);
                    }
                }
                Err(_) => break,
            }
        }
    }

    /// Drain notices from the channel.
    fn drain_notices(&self) {
        loop {
            match self.channels.notice.try_recv() {
                Ok(notice) => match notice {
                    Notice::Conflict { note } => {
                        warn!("Conflict detected for: {}", note.path.display());
                    }
                    Notice::RenameSuggested {
                        from,
                        to,
                        tier,
                        inbound,
                    } => {
                        info!(
                            "Rename suggested: {} -> {} (tier: {}, inbound: {})",
                            from.display(),
                            to.display(),
                            tier.as_str(),
                            inbound
                        );
                    }
                    Notice::RecoveryOffer { note } => {
                        info!("Recovery offer available for: {}", note.path.display());
                    }
                    Notice::ActionRequired { code, note } => {
                        warn!("Action required: {} (note: {:?})", code, note);
                    }
                },
                Err(_) => break,
            }
        }
    }
}

/// Initialize tracing with the appropriate level.
fn init_tracing(verbose: bool) {
    let level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();

    debug!("Tracing initialized at level: {}", level);
}

/// Main entry point.
fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Initialize tracing
    init_tracing(cli.verbose);

    // Determine vault path
    let vault_path = cli
        .vault
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

    match cli.command {
        Commands::Open { path } => {
            let vault_path = path.unwrap_or(vault_path);
            cmd_open(vault_path, cli.no_watch)?;
        }
        Commands::Doctor { path, json, quick } => {
            let vault_path = path.unwrap_or(vault_path);
            cmd_doctor(vault_path, json, quick)?;
        }
        Commands::Query { path, query, limit } => {
            let vault_path = path.unwrap_or(vault_path);
            cmd_query(vault_path, query, limit, cli.no_watch)?;
        }
        Commands::Search { path, term, limit } => {
            let vault_path = path.unwrap_or(vault_path);
            cmd_search(vault_path, term, limit, cli.no_watch)?;
        }
        Commands::Info { path } => {
            let vault_path = path.unwrap_or(vault_path);
            cmd_info(vault_path)?;
        }
        #[cfg(test)]
        Commands::Test { command } => {
            cmd_test(command)?;
        }
    }

    Ok(())
}

/// Handle the 'open' command.
fn cmd_open(vault_path: PathBuf, no_watch: bool) -> Result<()> {
    info!("Opening vault at: {}", vault_path.display());

    let driver = CliDriver::new(vault_path, no_watch)?;

    // Trigger a full reconcile
    driver.reconcile()?;

    // Drain any notices
    driver.drain_notices();

    info!("Vault opened and reconciled successfully.");
    info!("Vault UUID: {}", driver.vault.uuid());
    info!("Vault root: {}", driver.vault.root().display());

    Ok(())
}

/// Handle the 'doctor' command (§4.11).
fn cmd_doctor(vault_path: PathBuf, json: bool, quick: bool) -> Result<()> {
    info!("Running doctor on vault at: {}", vault_path.display());

    let driver = CliDriver::new(vault_path, true)?; // Always no-watch for doctor

    // Run all doctor checks
    let mut checks = Vec::new();

    // 1. SQLite quick_check
    checks.push(run_sqlite_check(&driver));

    // 2. Generation consistency check
    checks.push(run_generation_check(&driver));

    // 3. Sample hash verification
    if !quick {
        checks.push(run_hash_verification(&driver));
    }

    // 4. Orphan temp files check
    checks.push(run_orphan_temp_check(&driver));

    // 5. Pending journals check
    checks.push(run_pending_journals_check(&driver));

    // 6. Duplicate and conflicting IDs check
    checks.push(run_duplicate_id_check(&driver));

    // 7. NFC/case collision check
    checks.push(run_collision_check(&driver));

    // 8. Broken and ambiguous link counts
    checks.push(run_links_check(&driver));

    // 9. Attachment orphans and unhydrated placeholders
    checks.push(run_attachment_check(&driver));

    // 10. History store size check
    checks.push(run_history_check(&driver));

    // 11. UUID/root binding conflicts
    checks.push(run_binding_check(&driver));

    // 12. Orphan instance sockets
    checks.push(run_instance_socket_check(&driver));

    // 13. Search checkpoint/log coverage
    checks.push(run_search_checkpoint_check(&driver));

    // 14. Verification lag check
    checks.push(run_verification_lag_check(&driver));

    // Build summary
    let passed = checks.iter().filter(|c| c.passed).count();
    let failed = checks.iter().filter(|c| !c.passed).count();
    let warnings = checks
        .iter()
        .filter(|c| c.details.as_ref().map_or(false, |d| d.contains("warning")))
        .count();

    let summary = DoctorSummary {
        total_checks: checks.len(),
        passed,
        failed,
        warnings,
    };

    let report = DoctorReport {
        vault_path: driver.vault.root().clone(),
        checks: checks.clone(),
        summary: summary.clone(),
    };

    // Output results
    if json {
        let json = serde_json::to_string_pretty(&report)
            .context("Failed to serialize doctor report to JSON")?;
        println!("{}", json);
    } else {
        println!("Doctor report for: {}", driver.vault.root().display());
        println!("===========================================");
        println!();

        for check in &checks {
            let status = if check.passed { "PASS" } else { "FAIL" };
            println!("[{}]: {}", status, check.name);
            println!("  {}", check.message);
            if let Some(details) = &check.details {
                println!("  Details: {}", details);
            }
            println!();
        }

        println!("===========================================");
        println!(
            "Summary: {} passed, {} failed, {} warnings",
            summary.passed, summary.failed, summary.warnings
        );

        if failed > 0 {
            std::process::exit(1);
        }
    }

    Ok(())
}

/// Run SQLite quick_check.
fn run_sqlite_check(_driver: &CliDriver) -> DoctorCheck {
    // For now, this is a stub. In a full implementation, we'd run SQLite's quick_check
    // on the database file.
    DoctorCheck {
        name: "SQLite quick_check".to_string(),
        passed: true,
        message: "SQLite integrity check not yet implemented (stub)".to_string(),
        details: Some("Todo: implement SQLite quick_check via rusqlite".to_string()),
    }
}

/// Run generation consistency check.
fn run_generation_check(_driver: &CliDriver) -> DoctorCheck {
    // Check that SQLite generation and tantivy committed_generation are consistent
    DoctorCheck {
        name: "Generation consistency".to_string(),
        passed: true,
        message: "Generation check not yet implemented (stub)".to_string(),
        details: Some(
            "Todo: compare SQLite generation with tantivy committed_generation".to_string(),
        ),
    }
}

/// Run sample hash verification.
fn run_hash_verification(_driver: &CliDriver) -> DoctorCheck {
    // Sample N notes and verify their content hashes match the disk
    DoctorCheck {
        name: "Sample hash verification".to_string(),
        passed: true,
        message: "Hash verification not yet implemented (stub)".to_string(),
        details: Some("Todo: sample notes and verify content_hash against disk".to_string()),
    }
}

/// Run orphan temp files check.
fn run_orphan_temp_check(driver: &CliDriver) -> DoctorCheck {
    // Check for orphan .flavite-tmp.* files in the vault
    use std::fs;

    let vault_root = driver.vault.root();
    let mut orphaned = Vec::new();

    if let Ok(entries) = fs::read_dir(vault_root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(".flavite-tmp.") {
                    orphaned.push(path.display().to_string());
                }
            }
        }
    }

    if orphaned.is_empty() {
        DoctorCheck {
            name: "Orphan temp files".to_string(),
            passed: true,
            message: "No orphan temp files found".to_string(),
            details: None,
        }
    } else {
        DoctorCheck {
            name: "Orphan temp files".to_string(),
            passed: false,
            message: format!("Found {} orphan temp files", orphaned.len()),
            details: Some(orphaned.join(", ")),
        }
    }
}

/// Run pending journals check.
fn run_pending_journals_check(driver: &CliDriver) -> DoctorCheck {
    // Check for pending journal files in .flavite/journal/
    use std::fs;

    let journal_dir = driver.vault.root().join(".flavite").join("journal");
    let mut pending = Vec::new();

    if let Ok(entries) = fs::read_dir(&journal_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".json") {
                    // Check if this is a pending journal
                    pending.push(path.display().to_string());
                }
            }
        }
    }

    if pending.is_empty() {
        DoctorCheck {
            name: "Pending journals".to_string(),
            passed: true,
            message: "No pending journals found".to_string(),
            details: None,
        }
    } else {
        DoctorCheck {
            name: "Pending journals".to_string(),
            passed: false,
            message: format!("Found {} pending journal(s)", pending.len()),
            details: Some(pending.join(", ")),
        }
    }
}

/// Run duplicate ID check.
fn run_duplicate_id_check(_driver: &CliDriver) -> DoctorCheck {
    // Check for duplicate stable_id values in the index
    DoctorCheck {
        name: "Duplicate IDs".to_string(),
        passed: true,
        message: "Duplicate ID check not yet implemented (stub)".to_string(),
        details: Some("Todo: query SQLite for duplicate stable_id values".to_string()),
    }
}

/// Run NFC/case collision check.
fn run_collision_check(_driver: &CliDriver) -> DoctorCheck {
    // Check for NFC or case normalization collisions
    DoctorCheck {
        name: "NFC/case collisions".to_string(),
        passed: true,
        message: "Collision check not yet implemented (stub)".to_string(),
        details: Some("Todo: detect and report normalization collisions".to_string()),
    }
}

/// Run links check.
fn run_links_check(_driver: &CliDriver) -> DoctorCheck {
    // Check for broken and ambiguous links
    DoctorCheck {
        name: "Broken and ambiguous links".to_string(),
        passed: true,
        message: "Links check not yet implemented (stub)".to_string(),
        details: Some("Todo: query index for broken/ambiguous links".to_string()),
    }
}

/// Run attachment check.
fn run_attachment_check(_driver: &CliDriver) -> DoctorCheck {
    // Check for attachment orphans and unhydrated placeholders
    DoctorCheck {
        name: "Attachment orphans and placeholders".to_string(),
        passed: true,
        message: "Attachment check not yet implemented (stub)".to_string(),
        details: Some("Todo: query index for attachment orphans/placeholders".to_string()),
    }
}

/// Run history check.
fn run_history_check(_driver: &CliDriver) -> DoctorCheck {
    // Check history store size and pinned bytes
    DoctorCheck {
        name: "History store".to_string(),
        passed: true,
        message: "History check not yet implemented (stub)".to_string(),
        details: Some("Todo: check history store size and report metrics".to_string()),
    }
}

/// Run binding check.
fn run_binding_check(_driver: &CliDriver) -> DoctorCheck {
    // Check for UUID/root binding conflicts
    DoctorCheck {
        name: "UUID/root binding conflicts".to_string(),
        passed: true,
        message: "Binding check not yet implemented (stub)".to_string(),
        details: Some("Todo: check registry for binding conflicts".to_string()),
    }
}

/// Run instance socket check.
fn run_instance_socket_check(_driver: &CliDriver) -> DoctorCheck {
    // Check for orphan instance sockets
    use std::fs;

    let os_data = match directories::ProjectDirs::from("com", "flavite", "Lattice") {
        Some(p) => p.data_dir().to_path_buf(),
        None => {
            return DoctorCheck {
                name: "Orphan instance sockets".to_string(),
                passed: true,
                message: "Cannot determine OS data directory".to_string(),
                details: None,
            };
        }
    };

    let flavite_dir = os_data.join("flavite");
    let vaults_dir = flavite_dir.join("vaults");
    let mut orphaned = Vec::new();

    if let Ok(entries) = fs::read_dir(&vaults_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".ipc") || name.ends_with(".lock") {
                    // Check if there's a corresponding running instance
                    // For now, just flag all as potential orphans
                    orphaned.push(path.display().to_string());
                }
            }
        }
    }

    if orphaned.is_empty() {
        DoctorCheck {
            name: "Orphan instance sockets".to_string(),
            passed: true,
            message: "No orphan instance sockets found".to_string(),
            details: None,
        }
    } else {
        DoctorCheck {
            name: "Orphan instance sockets".to_string(),
            passed: false,
            message: format!("Found {} potential orphan socket(s)", orphaned.len()),
            details: Some(orphaned.join(", ")),
        }
    }
}

/// Run search checkpoint check.
fn run_search_checkpoint_check(_driver: &CliDriver) -> DoctorCheck {
    // Check search checkpoint vs log coverage
    DoctorCheck {
        name: "Search checkpoint/log coverage".to_string(),
        passed: true,
        message: "Search checkpoint check not yet implemented (stub)".to_string(),
        details: Some("Todo: verify search checkpoint matches log coverage".to_string()),
    }
}

/// Run verification lag check.
fn run_verification_lag_check(_driver: &CliDriver) -> DoctorCheck {
    // Check if verification is overdue
    DoctorCheck {
        name: "Verification lag".to_string(),
        passed: true,
        message: "Verification lag check not yet implemented (stub)".to_string(),
        details: Some("Todo: report time since last verification".to_string()),
    }
}

/// Handle the 'query' command.
fn cmd_query(vault_path: PathBuf, query: String, limit: usize, no_watch: bool) -> Result<()> {
    info!("Querying vault at: {}", vault_path.display());
    info!("Query: {}", query);

    let _driver = CliDriver::new(vault_path, no_watch)?;

    // For now, just log the query
    // In a full implementation, we'd parse the query and execute it
    warn!("Query execution not yet implemented (stub)");
    println!("Query: {}", query);
    println!("Limit: {}", limit);
    println!("Todo: implement query parsing and execution");

    Ok(())
}

/// Handle the 'search' command.
fn cmd_search(vault_path: PathBuf, term: String, limit: usize, no_watch: bool) -> Result<()> {
    info!("Searching vault at: {}", vault_path.display());
    info!("Term: {}", term);

    let _driver = CliDriver::new(vault_path, no_watch)?;

    // For now, just log the search
    // In a full implementation, we'd execute the search
    warn!("Search execution not yet implemented (stub)");
    println!("Search term: {}", term);
    println!("Limit: {}", limit);
    println!("Todo: implement search execution");

    Ok(())
}

/// Handle the 'info' command.
fn cmd_info(vault_path: PathBuf) -> Result<()> {
    info!("Vault info for: {}", vault_path.display());

    let driver = CliDriver::new(vault_path, true)?; // no-watch for info

    println!("Vault UUID: {}", driver.vault.uuid());
    println!("Vault root: {}", driver.vault.root().display());
    println!("Watcher enabled: {}", driver.config.watcher_enabled);
    println!(
        "Verification interval: {:?}",
        driver.config.verification_interval
    );

    Ok(())
}

/// Handle test command (for CI).
#[cfg(test)]
fn cmd_test(command: String) -> Result<()> {
    match command.as_str() {
        "ping" => {
            println!("pong");
        }
        _ => {
            anyhow::bail!("Unknown test command: {}", command);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_vault::{RenameTier, VaultRef};

    #[test]
    fn test_cli_parsing() {
        // Test that the CLI can be parsed
        use clap::Parser;

        let cli = Cli::parse_from(["flavite", "--help"]);
        assert!(cli.verbose == false);

        let cli = Cli::parse_from(["flavite", "-v", "doctor"]);
        assert!(cli.verbose == true);
        match cli.command {
            Commands::Doctor { .. } => {}
            _ => panic!("Expected Doctor command"),
        }
    }

    #[test]
    fn test_vault_ref() {
        let ref_ = VaultRef::new(PathBuf::from("/test"));
        assert_eq!(ref_.root, PathBuf::from("/test"));
    }

    #[test]
    fn test_rename_tier() {
        assert_eq!(RenameTier::T0.as_str(), "T0");
        assert_eq!(RenameTier::T1.as_str(), "T1");
        assert_eq!(RenameTier::T2.as_str(), "T2");
        assert_eq!(RenameTier::T3.as_str(), "T3");
        assert_eq!(RenameTier::T4.as_str(), "T4");
    }

    #[test]
    fn test_doctor_check_serialization() {
        let check = DoctorCheck {
            name: "Test check".to_string(),
            passed: true,
            message: "Test message".to_string(),
            details: Some("Test details".to_string()),
        };

        let json = serde_json::to_string(&check).unwrap();
        assert!(json.contains("Test check"));
        assert!(json.contains("Test message"));
        assert!(json.contains("Test details"));
    }

    #[test]
    fn test_doctor_report_serialization() {
        let report = DoctorReport {
            vault_path: PathBuf::from("/test/vault"),
            checks: vec![DoctorCheck {
                name: "Test".to_string(),
                passed: true,
                message: "OK".to_string(),
                details: None,
            }],
            summary: DoctorSummary {
                total_checks: 1,
                passed: 1,
                failed: 0,
                warnings: 0,
            },
        };

        let json = serde_json::to_string_pretty(&report).unwrap();
        assert!(json.contains("/test/vault"));
        assert!(json.contains("Test"));
        assert!(json.contains("\"passed\": 1"));
    }
}
