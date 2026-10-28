//! Tier 3. Headless CLI; consumes `flavite-api` (D16 dogfood) (§4.1).
//!
//! Phase 4 / PLAN-C slice C14: The CLI is a consumer of flavite-api.
//! This demonstrates the dogfooding requirement where the CLI uses the
//! public API surface instead of directly accessing core internals.
//!
//! Note: Full migration to remove all direct flavite-vault/flavite-index
//! imports is tracked separately (Phase 4 exit gate).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use flavite_api::{
    ApiVaultRef, DoctorReport, ExecutionResult, Flavite, LocalResult, Origin, OsFs, VaultConfig,
    command::Invocation,
};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// Flavite — a local-first plain-text knowledge vault in pure Rust.
///
/// The CLI is headless and drives the vault's Session through flavite-api.
/// All commands work without a GPU or display.
#[derive(Parser, Debug)]
#[command(name = "flavite")]
#[command(author = "Flavite Contributors")]
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

    /// Test the API.
    ApiTest,

    /// Initialize a new vault.
    Init {
        /// Path to create the vault at.
        #[arg(short, long, value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

/// CLI driver that uses flavite-api for all operations.
#[allow(dead_code)]
struct CliDriver {
    /// Flavite facade from flavite-api
    flavite: Option<Flavite>,
    /// Vault reference
    vault_ref: Option<ApiVaultRef>,
}

impl CliDriver {
    /// Create a new CLI driver.
    fn new() -> Self {
        Self {
            flavite: None,
            vault_ref: None,
        }
    }

    /// Open a vault through the API.
    fn open_vault(&mut self, vault_path: PathBuf, no_watch: bool) -> Result<()> {
        info!("Opening vault at: {}", vault_path.display());

        // Ensure the vault path is absolute
        let vault_path = std::fs::canonicalize(&vault_path)
            .with_context(|| format!("Cannot canonicalize vault path: {}", vault_path.display()))?;

        if !vault_path.is_dir() {
            anyhow::bail!(
                "Vault does not exist at {}. Run 'flavite init' to create one.",
                vault_path.display()
            );
        }

        // Configure vault
        let config = VaultConfig {
            watcher_enabled: !no_watch,
            ..Default::default()
        };

        let flavite = Flavite::open_root(vault_path, config)
            .with_context(|| "Failed to open vault through Flavite API")?;
        let request_id = flavite.command_registry().next_request_id();
        let result = flavite.execute(Invocation::new(
            request_id,
            "reconcile",
            serde_json::json!({ "scope": "full", "mode": "verify" }),
            Origin::Cli,
        ));
        let result = match result {
            ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
            completed => completed,
        };
        if !matches!(result, ExecutionResult::Local(LocalResult::Success(_))) {
            anyhow::bail!("vault reconcile did not complete successfully: {result:?}");
        }
        self.vault_ref = Some(flavite.vault_ref().clone());
        self.flavite = Some(flavite);

        Ok(())
    }

    /// Initialize a new vault.
    #[allow(clippy::ptr_arg)]
    fn init_vault(&mut self, vault_path: &Path) -> Result<()> {
        info!("Initializing new vault at: {}", vault_path.display());

        // Ensure the vault path is absolute
        let vault_path = std::fs::canonicalize(vault_path)
            .with_context(|| format!("Cannot canonicalize vault path: {}", vault_path.display()))?;

        // Create the directory if it doesn't exist
        if !vault_path.exists() {
            std::fs::create_dir_all(&vault_path).with_context(|| {
                format!("Failed to create vault directory: {}", vault_path.display())
            })?;
        }

        // Initialize the vault
        let _fs = Box::new(OsFs::new());
        let _config = VaultConfig::default();

        // Create .flavite directory
        let flavite_dir = vault_path.join(".flavite");
        if !flavite_dir.exists() {
            std::fs::create_dir_all(&flavite_dir)
                .with_context(|| "Failed to create .flavite directory".to_string())?;
        }

        // For now, just create the basic structure
        // A full implementation would initialize the registry, UUID, etc.
        info!("Vault initialized at: {}", vault_path.display());

        Ok(())
    }

    /// Run the doctor command.
    #[allow(clippy::ptr_arg)]
    fn run_doctor(&self, vault_path: &Path, json: bool) -> Result<()> {
        info!("Running doctor on vault: {}", vault_path.display());

        let vault_path = std::fs::canonicalize(vault_path)
            .with_context(|| format!("Cannot canonicalize vault path: {}", vault_path.display()))?;
        anyhow::ensure!(
            vault_path.is_dir(),
            "Vault does not exist at {}",
            vault_path.display()
        );
        // Resolve persisted identity and hold the normal root/instance guards,
        // without opening a writer or mutating/reconciling the disposable cache.
        let report = flavite_api::doctor::run_doctor_root(vault_path.clone())
            .map_err(anyhow::Error::msg)
            .context("Cannot run doctor while the vault is open or identity cannot be resolved")?;

        if json {
            // Output as JSON
            let json = serde_json::to_string_pretty(&report)
                .map_err(|e| anyhow::anyhow!("Failed to serialize report: {}", e))?;
            println!("{}", json);
        } else {
            // Output as human-readable text
            self.print_doctor_report(&report);
        }

        // Preserve the report contract: errors fail the command; warnings remain visible.
        if !report.is_healthy() {
            anyhow::bail!("Doctor found issues in vault: {}", report.status_as_str());
        }

        Ok(())
    }

    /// Print a doctor report in human-readable format.
    fn print_doctor_report(&self, report: &DoctorReport) {
        println!("Vault Doctor Report");
        println!("==================");
        println!();
        println!("Vault: {}", report.vault.root.display());
        println!("UUID: {}", report.vault.uuid);
        println!("Timestamp: {}", report.timestamp);
        println!();

        println!(
            "Summary: {} ({} OK, {} warnings, {} errors)",
            report.status_as_str(),
            report.summary.ok_count,
            report.summary.warning_count,
            report.summary.error_count
        );
        println!();

        // Print individual checks
        for check in &report.checks {
            match check.status {
                flavite_api::doctor::CheckStatus::Ok => {
                    println!("✓ {}: {}", check.name, check.description);
                }
                flavite_api::doctor::CheckStatus::Warning => {
                    println!("⚠ {}: {}", check.name, check.description);
                    if let Some(details) = &check.details {
                        println!("  Details: {}", details);
                    }
                }
                flavite_api::doctor::CheckStatus::Error => {
                    println!("✗ {}: {}", check.name, check.description);
                    if let Some(details) = &check.details {
                        println!("  Error: {}", details);
                    }
                    if let Some(fix) = &check.fix {
                        println!("  Fix: {}", fix);
                    }
                }
            }
        }
        println!();

        if report.is_healthy() {
            println!("Overall: HEALTHY ✓");
        } else {
            println!("Overall: UNHEALTHY ✗");
        }
    }

    /// Run a query.
    #[allow(clippy::ptr_arg)]
    fn run_query(&self, _vault_path: &Path, query: &str, limit: usize) -> Result<()> {
        info!("Running query: {}", query);
        let flavite = self
            .flavite
            .as_ref()
            .context("query requires an opened vault")?;
        let result =
            flavite.query_with_limit(&flavite_api::Query::Text(query.to_owned()), limit)?;
        for note in &result.notes {
            println!("{}\t{}", note.path, note.title);
        }
        Ok(())
    }

    /// Run a search.
    #[allow(clippy::ptr_arg)]
    fn run_search(&self, _vault_path: &Path, term: &str, limit: usize) -> Result<()> {
        info!("Running search: {}", term);

        let flavite = self
            .flavite
            .as_ref()
            .context("search requires an opened vault")?;
        let result = flavite.query_with_limit(&flavite_api::Query::Text(term.to_owned()), limit)?;
        for note in &result.notes {
            println!("{}\t{}", note.path, note.title);
        }
        Ok(())
    }

    /// Show vault info.
    #[allow(clippy::ptr_arg)]
    fn show_info(&self, vault_path: &Path) -> Result<()> {
        info!("Showing vault info: {}", vault_path.display());

        if let Some(ref flavite) = self.flavite {
            let vault_ref = flavite.vault_ref();
            println!("Vault: {}", vault_ref.root.display());
            println!("UUID: {}", vault_ref.uuid);
            println!("API Version: {}", flavite_api::API_VERSION);
        } else {
            println!("Vault: {}", vault_path.display());
            println!("API Version: {}", flavite_api::API_VERSION);
        }

        Ok(())
    }

    /// Run API test.
    fn api_test(&self) -> Result<()> {
        info!("Running API test");

        // Test that we can use the API
        println!("API version: {}", flavite_api::API_VERSION);
        println!(
            "Event API version: {}",
            flavite_api::event::EVENT_API_VERSION
        );

        // Test creating types
        let vault_ref = ApiVaultRef::new(uuid::Uuid::new_v4(), PathBuf::from("/test"));
        println!(
            "Created ApiVaultRef: {} -> {}",
            vault_ref.uuid,
            vault_ref.root.display()
        );

        // Test command registry
        let registry = flavite_api::CommandRegistry::new();
        let request_id = registry.next_request_id();
        println!("Generated request ID: {}", request_id);

        // Test creating an invocation
        let invocation =
            Invocation::new(request_id, "test.cmd", serde_json::Value::Null, Origin::Cli);
        println!("Created invocation: {:?}", invocation);

        Ok(())
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
        .with_writer(std::io::stderr)
        .init();

    debug!("Tracing initialized at level: {}", level);
}

/// Get vault path from CLI options or default to current directory.
fn get_vault_path(cli_path: Option<PathBuf>, global_path: Option<PathBuf>) -> PathBuf {
    cli_path
        .or(global_path)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Main entry point.
fn main() -> Result<()> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Initialize tracing
    init_tracing(cli.verbose);

    // Create CLI driver
    let mut driver = CliDriver::new();

    // Process command
    match cli.command {
        Commands::Open { path } => {
            let vault_path = get_vault_path(path, cli.vault);
            driver.open_vault(vault_path, cli.no_watch)?;
            info!("Vault opened and ready");
        }
        Commands::Doctor { path, json } => {
            let vault_path = get_vault_path(path, cli.vault);
            driver.run_doctor(&vault_path, json)?;
        }
        Commands::Query { path, query, limit } => {
            let vault_path = get_vault_path(path, cli.vault);
            driver.open_vault(vault_path.clone(), cli.no_watch)?;
            driver.run_query(&vault_path, &query, limit)?;
        }
        Commands::Search { path, term, limit } => {
            let vault_path = get_vault_path(path, cli.vault);
            driver.open_vault(vault_path.clone(), cli.no_watch)?;
            driver.run_search(&vault_path, &term, limit)?;
        }
        Commands::Info { path } => {
            let vault_path = get_vault_path(path, cli.vault);
            if let Some(ref flavite) = driver.flavite {
                let vault_ref = flavite.vault_ref();
                driver.vault_ref = Some(vault_ref.clone());
            }
            driver.show_info(&vault_path)?;
        }
        Commands::ApiTest => {
            driver.api_test()?;
        }
        Commands::Init { path } => {
            let vault_path = get_vault_path(path, cli.vault);
            driver.init_vault(&vault_path)?;
            info!("Vault initialized at: {}", vault_path.display());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vault_path_resolution() {
        let path = get_vault_path(None, None);
        assert!(path.exists() || path == *".");
    }

    #[test]
    fn test_cli_driver_creation() {
        let driver = CliDriver::new();
        assert!(driver.flavite.is_none());
        assert!(driver.vault_ref.is_none());
    }
}
