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
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use tracing::{debug, info};
use tracing_subscriber::fmt::MakeWriter;

fn flavite_vault_identity(fs: &OsFs, root: PathBuf) -> Result<flavite_api::open::IdentityManager> {
    flavite_api::open::IdentityManager::open(fs, root).map_err(|error| {
        anyhow::anyhow!("Failed to initialize vault identity and registry: {error}")
    })
}

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

    /// Emit machine-readable JSON output and JSON Lines for pending events.
    #[arg(long, global = true)]
    json: bool,

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
struct CliDriver {
    /// Flavite facade from flavite-api
    flavite: Option<Flavite>,
    /// Vault reference
    vault_ref: Option<ApiVaultRef>,
    json: bool,
}

impl CliDriver {
    /// Create a new CLI driver.
    fn new() -> Self {
        Self {
            flavite: None,
            vault_ref: None,
            json: false,
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
        // Create the directory if it doesn't exist
        if !vault_path.exists() {
            std::fs::create_dir_all(&vault_path).with_context(|| {
                format!("Failed to create vault directory: {}", vault_path.display())
            })?;
        }
        let vault_path = std::fs::canonicalize(vault_path)
            .with_context(|| format!("Cannot canonicalize vault path: {}", vault_path.display()))?;

        // Opening through IdentityManager writes vault.toml and the matching
        // OS-data registry record. The actor/index setup also verifies this is
        // an openable vault; dropping closes the host resources.
        let fs = OsFs::new();
        drop(flavite_vault_identity(&fs, vault_path.clone())?);
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
    fn run_query(
        &self,
        _vault_path: &Path,
        query: &flavite_api::Query,
        limit: usize,
    ) -> Result<()> {
        info!("Running query");
        let flavite = self
            .flavite
            .as_ref()
            .context("query requires an opened vault")?;
        let result = flavite.query_with_limit(query, limit)?;
        if self.json {
            println!("{}", serde_json::to_string(&result.notes)?);
        } else {
            for note in &result.notes {
                println!("{}\t{}", note.path, note.title);
            }
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
        if self.json {
            println!("{}", serde_json::to_string(&result.notes)?);
        } else {
            for note in &result.notes {
                println!("{}\t{}", note.path, note.title);
            }
        }
        Ok(())
    }

    /// Show vault info.
    #[allow(clippy::ptr_arg)]
    fn show_info(&self, _vault_path: &Path) -> Result<()> {
        info!("Showing opened vault info");

        let flavite = self
            .flavite
            .as_ref()
            .context("info requires an opened vault")?;
        let info = flavite.vault_info()?;
        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "vault": flavite.vault_ref(), "api_version": flavite_api::API_VERSION,
                    "note_count": info.note_count, "epoch": info.epoch,
                    "generation": info.generation, "tantivy_generation": info.tantivy_generation,
                    "watcher": if info.watcher_enabled { "enabled" } else { "disabled" },
                    "drift_found_total": info.drift_found_total,
                }))?
            );
        } else {
            let vault_ref = flavite.vault_ref();
            println!("Vault: {}", vault_ref.root.display());
            println!("UUID: {}", vault_ref.uuid);
            println!("API Version: {}", flavite_api::API_VERSION);
            println!("Notes: {}", info.note_count);
            println!("Generation: {}", info.generation);
            println!("Tantivy generation: {}", info.tantivy_generation);
            println!(
                "Watcher: {}",
                if info.watcher_enabled {
                    "enabled"
                } else {
                    "disabled"
                }
            );
            println!("Drift found: {}", info.drift_found_total);
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
#[derive(Clone)]
struct RotatingLogWriter {
    path: PathBuf,
    max_bytes: u64,
}

impl<'a> MakeWriter<'a> for RotatingLogWriter {
    type Writer = fs::File;
    fn make_writer(&'a self) -> Self::Writer {
        if fs::metadata(&self.path).is_ok_and(|metadata| metadata.len() >= self.max_bytes) {
            let backup = self.path.with_extension("log.1");
            let _ = fs::remove_file(&backup);
            let _ = fs::rename(&self.path, backup);
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .unwrap_or_else(|_| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(std::env::temp_dir().join("flavite.log"))
                    .unwrap()
            })
    }
}

fn init_tracing(verbose: bool) {
    let level = if verbose {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    let log_path = flavite_api::doctor::diagnostics_log_dir().map(|dir| {
        let _ = fs::create_dir_all(&dir);
        dir.join("flavite.log")
    });
    if let Some(path) = &log_path {
        let crash_path = path.parent().unwrap().join("crash-report.txt");
        let log_for_hook = path.clone();
        std::panic::set_hook(Box::new(move |info| {
            let tail = fs::read_to_string(&log_for_hook).unwrap_or_default();
            let last = tail
                .lines()
                .rev()
                .take(200)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            let report = format!("Flavite crash report\n{info}\n\nLast log lines:\n{last}\n");
            let _ = fs::write(&crash_path, report);
        }));
    }
    let writer = log_path.map_or_else(
        || RotatingLogWriter {
            path: std::env::temp_dir().join("flavite.log"),
            max_bytes: 5 * 1024 * 1024,
        },
        |path| RotatingLogWriter {
            path,
            max_bytes: 5 * 1024 * 1024,
        },
    );
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_writer(writer)
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
    driver.json = cli.json;

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
            let query = flavite_api::Query::parse(&query).map_err(anyhow::Error::msg)?;
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
            driver.open_vault(vault_path.clone(), cli.no_watch)?;
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

    if let Some(flavite) = driver.flavite.as_ref() {
        let events = flavite.drain_events();
        if !events.is_empty() {
            if driver.json {
                for event in events {
                    println!("{}", serde_json::to_string(&event)?);
                    flavite.acknowledge_event(&event);
                }
            } else {
                for event in events {
                    eprintln!("event {}: {event:?}", event.event_type());
                    flavite.acknowledge_event(&event);
                }
            }
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
