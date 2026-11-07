//! Tier 3. Headless CLI; consumes `flavite-api` (D16 dogfood) (§4.1).
//!
//! Phase 4 / PLAN-C slice C14: The CLI is a consumer of flavite-api.
//! This demonstrates the dogfooding requirement where the CLI uses the
//! public API surface instead of directly accessing core internals.
//!
//! Note: Full migration to remove all direct flavite-vault/flavite-index
//! imports is tracked separately (Phase 4 exit gate).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
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
#[command(name = "flavite-cli")]
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

    /// Search saved note content through a materialized search session
    /// (RFC-0006 S06). Plain text is the default mode: whitespace means
    /// AND and punctuation is text. Expression mode parses the explicit
    /// operator syntax (AND/OR/NOT, quotes, parentheses). Literal and
    /// regex modes are vault source matching (RFC-0006 S07) and are
    /// refused here with a typed error.
    Search {
        /// Path to the vault (overrides global --vault).
        #[arg(short, long, value_name = "PATH")]
        path: Option<PathBuf>,

        /// The search term.
        #[arg(required = true, value_name = "TERM")]
        term: String,

        /// Search mode: text (default) or expression.
        #[arg(long, value_enum, default_value_t = CliSearchMode::Text)]
        mode: CliSearchMode,

        /// Restrict text leaves to one indexed field (title, aliases,
        /// tags, body, all). Expression mode embeds field filters directly.
        #[arg(long, value_name = "FIELD")]
        field: Option<String>,

        /// Page size (1..=200, default 50).
        #[arg(short, long, default_value = "50", value_name = "N")]
        limit: usize,

        /// Consume every continuation token before exiting.
        #[arg(long)]
        all: bool,
    },

    /// Rename a note through the durable vault rename transaction.
    Rename {
        /// Vault-relative source path.
        from: PathBuf,

        /// Vault-relative destination path.
        to: PathBuf,

        /// How inbound links are handled.
        #[arg(long, value_enum, default_value_t = CliRenameStrategy::Rewrite)]
        strategy: CliRenameStrategy,
    },

    /// Convert browser theme packages and manage this vault's installed
    /// themes (xpi themes).
    Theme {
        #[command(subcommand)]
        action: ThemeAction,
    },

    /// Inspect or convert a note's file encoding explicitly (RFC-0006 §6.10).
    Encoding {
        /// Vault-relative note path.
        note: PathBuf,

        #[command(subcommand)]
        action: EncodingAction,
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

/// Encoding inspection/conversion actions (§6.10). Reopen reports/changes the
/// interpretation of the current bytes; Convert changes the save format and
/// then performs the guarded save, waiting for source and format receipts.
#[derive(Subcommand, Debug)]
enum EncodingAction {
    /// Reopen the note under an explicit encoding and report the resulting
    /// format. `auto` opens without an explicit family (BOM/hint rules).
    Reopen {
        #[arg(long, value_enum, default_value_t = CliEncoding::Auto)]
        encoding: CliEncoding,

        #[arg(long, value_enum, default_value_t = CliBom::Auto)]
        bom: CliBom,
    },
    /// Convert the note's save format, then save durably through the core
    /// command (never a CLI file write). Requires an explicit family.
    Convert {
        #[arg(long, value_enum)]
        encoding: CliEncoding,

        #[arg(long, value_enum, default_value_t = CliBom::Absent)]
        bom: CliBom,
    },
}

/// Encoding family (§6.10): `auto` never guesses BOM-less UTF-16.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CliEncoding {
    Auto,
    Ascii,
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl CliEncoding {
    fn code(self) -> Option<&'static str> {
        match self {
            Self::Auto => None,
            Self::Ascii => Some("ascii"),
            Self::Utf8 => Some("utf8"),
            Self::Utf16Le => Some("utf16le"),
            Self::Utf16Be => Some("utf16be"),
        }
    }
}

/// BOM intent (§6.10 CLI contract): auto strips a matching signature for the
/// requested family and otherwise treats it as absent; present requires it;
/// absent preserves leading signature-shaped bytes as content.
#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum CliBom {
    Auto,
    Present,
    Absent,
}

impl CliBom {
    fn arg(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Present => "present",
            Self::Absent => "absent",
        }
    }
}

/// CLI search modes (RFC-0006 S06).
#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliSearchMode {
    /// Plain full-text: whitespace means AND, punctuation is text.
    Text,
    /// Explicit expression syntax (AND/OR/NOT, quotes, parentheses).
    Expression,
    /// Vault source literal matching (RFC-0006 S07; not available yet).
    Literal,
    /// Vault source regex matching (RFC-0006 S07; not available yet).
    Regex,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliRenameStrategy {
    Rewrite,
    Alias,
    None,
}

impl CliRenameStrategy {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Rewrite => "rewrite",
            Self::Alias => "alias",
            Self::None => "none",
        }
    }
}

/// CLI driver that uses flavite-api for all operations.
/// Theme package actions (xpi themes). `Convert` is an offline tool that
/// needs no vault; the other actions manage the device-local theme store
/// through the facade's typed `theme.*` commands.
#[derive(Subcommand, Debug)]
enum ThemeAction {
    /// Convert a standard browser theme package into a `.flavite.xpi`.
    Convert {
        /// Path to the standard `.xpi` (or zip) theme package.
        input: PathBuf,

        /// Output path (default: `<input>.flavite.xpi` beside the input).
        #[arg(short, long, value_name = "PATH")]
        output: Option<PathBuf>,
    },
    /// Convert and install a standard `.xpi` into this vault's device
    /// theme store.
    Install {
        /// Path to the standard `.xpi` theme package.
        path: PathBuf,
    },
    /// List installed themes.
    List,
    /// Select an installed theme, or clear the selection with `none`.
    Select {
        /// The theme id, or `none` for the built-in palettes.
        id: String,
    },
    /// Remove an installed theme.
    Uninstall {
        /// The theme id.
        id: String,
    },
}

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
            "vault.reconcile",
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
            std::fs::create_dir_all(vault_path).with_context(|| {
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
    /// Inspect or convert a note's encoding through the core commands
    /// (§6.10). Reopen never discards unsaved text; Convert is followed by
    /// the guarded save and both wait for real receipts instead of exiting
    /// after an ephemeral metadata change.
    /// Convert a standard theme package into a `.flavite.xpi` and print
    /// the conversion report (mapped slots, dropped keys, warnings).
    fn convert_theme(&self, input: PathBuf, output: Option<PathBuf>) -> Result<()> {
        let package =
            std::fs::read(&input).with_context(|| format!("reading {}", input.display()))?;
        let converted = flavite_api::flavite_theme::convert_xpi(&package)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let output = output.unwrap_or_else(|| {
            let mut name = input.file_name().map_or_else(
                || std::ffi::OsString::from("theme.flavite.xpi"),
                std::ffi::OsString::from,
            );
            name.push(".flavite.xpi");
            input.with_file_name(name)
        });
        let file = std::fs::File::create(&output)
            .with_context(|| format!("creating {}", output.display()))?;
        flavite_api::flavite_theme::write_package(&converted.files, file)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        let report = &converted.report;
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "output": output,
                    "report": report,
                }))?
            );
        } else {
            println!("Converted: {}", report.name);
            println!("Theme id: {}", report.theme_id);
            println!("Output: {}", output.display());
            println!(
                "Mapped {} color keys; dropped {}; animated header: {}; dark variant: {}",
                report.mapped.len(),
                report.dropped.len(),
                report.animated_frame,
                report.supports_dark_variant,
            );
            for dropped in &report.dropped {
                println!("  dropped {}: {}", dropped.key, dropped.reason);
            }
            for warning in &report.warnings {
                println!("  warning: {warning}");
            }
        }
        Ok(())
    }

    /// Run one facade `theme.*` command and print its typed result.
    fn run_theme_command(&self, id: &'static str, args: serde_json::Value) -> Result<()> {
        let flavite = self
            .flavite
            .as_ref()
            .context("theme commands require an opened vault")?;
        let request_id = flavite.command_registry().next_request_id();
        let result = flavite.execute(Invocation::new(request_id, id, args, Origin::Cli));
        let result = match result {
            ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
            result => result,
        };
        let success = result
            .into_success()
            .with_context(|| format!("{id} did not complete"))?;
        if self.json {
            println!("{}", serde_json::to_string(&success.data)?);
        } else if id == "theme.list" {
            match success
                .data
                .get("themes")
                .and_then(serde_json::Value::as_array)
            {
                Some(themes) if themes.is_empty() => println!("No themes installed."),
                Some(themes) => {
                    for theme in themes {
                        println!(
                            "{} — {}",
                            theme
                                .get("id")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("?"),
                            theme
                                .get("name")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("?"),
                        );
                    }
                }
                None => println!("{}", serde_json::to_string(&success.data)?),
            }
        } else if id == "theme.install" {
            println!(
                "Installed: {}",
                success
                    .data
                    .get("theme")
                    .and_then(|theme| theme.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
            );
        } else {
            println!("{id}: ok");
        }
        Ok(())
    }

    /// Select (or clear, with `none`) the installed theme in the device
    /// preferences. The write goes through the facade's compare-and-set
    /// user-state service — never a direct file write.
    fn select_theme(&mut self, id: &str) -> Result<()> {
        let selection = (id != "none").then(|| id.to_owned());
        let (generation, document) = {
            let flavite = self
                .flavite
                .as_ref()
                .context("theme selection requires an opened vault")?;
            if let Some(selected) = &selection {
                let installed = flavite
                    .themes()
                    .context("theme storage is unavailable on this device")?
                    .list()?;
                anyhow::ensure!(
                    installed.iter().any(|theme| &theme.id == selected),
                    "theme {selected:?} is not installed (see `flavite theme list`)"
                );
            }
            let stored = flavite.read_user_state(flavite_api::UserStateKind::DevicePreferences)?;
            let generation = stored.as_ref().map_or(0, |stored| stored.generation);
            let text = stored
                .as_ref()
                .map(|stored| String::from_utf8(stored.payload.clone()))
                .transpose()
                .ok()
                .flatten();
            let parsed = flavite_api::device_preferences::Preferences::parse(text.as_deref())?;
            let mut preferences = parsed.preferences;
            preferences.theme_package = selection;
            (generation, preferences.to_stored())
        };
        let flavite = self
            .flavite
            .as_mut()
            .context("theme selection requires an opened vault")?;
        flavite.write_user_state(
            flavite_api::UserStateKind::DevicePreferences,
            generation,
            document.as_bytes(),
        )?;
        println!("Theme selection saved: {}", id);
        Ok(())
    }

    fn run_encoding(&self, note: PathBuf, action: EncodingAction) -> Result<()> {
        let flavite = self
            .flavite
            .as_ref()
            .context("encoding commands require an opened vault")?;
        let run = |id: &'static str, args: serde_json::Value| -> Result<ExecutionResult> {
            let request_id = flavite.command_registry().next_request_id();
            let result = flavite.execute(Invocation::new(request_id, id, args, Origin::Cli));
            Ok(match result {
                ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
                result => result,
            })
        };
        match action {
            EncodingAction::Reopen { encoding, bom } => {
                if let Some(code) = encoding.code() {
                    // The core re-verifies the raw-hash precondition; a
                    // change between observation and reopen is a typed
                    // stale-source refusal, never a decode of unseen bytes.
                    let abs = flavite.vault_ref().root.join(&note);
                    let bytes = std::fs::read(&abs)
                        .with_context(|| format!("reading {}", abs.display()))?;
                    let raw_hash = hex::encode(blake3::hash(&bytes).as_bytes());
                    let result = run(
                        "note.reopen_encoding",
                        serde_json::json!({
                            "path": note,
                            "raw_hash": raw_hash,
                            "encoding": code,
                            "bom": bom.arg(),
                        }),
                    )?;
                    anyhow::ensure!(result.is_success(), "reopen did not complete: {result:?}");
                }
                let open = run("note.open", serde_json::json!({ "path": note }))?;
                anyhow::ensure!(open.is_success(), "open did not complete: {open:?}");
                let buffer_id = open
                    .into_success()
                    .and_then(|success| success.data.get("buffer_id").and_then(|v| v.as_u64()))
                    .context("open returned no buffer id")?;
                let view = flavite.get_buffer_view(flavite_api::BufferId::from(buffer_id), None)?;
                let format = view.source_format;
                let describe = format!(
                    "{}{}",
                    format.encoding_code(),
                    if format.bom() { "+bom" } else { "" }
                );
                if self.json {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "note": note,
                            "encoding": format.encoding_code(),
                            "bom": format.bom(),
                        }))?
                    );
                } else {
                    println!("{}: {describe}", note.display());
                }
            }
            EncodingAction::Convert { encoding, bom } => {
                let code = encoding
                    .code()
                    .context("convert requires an explicit encoding family")?;
                let open = run("note.open", serde_json::json!({ "path": note }))?;
                anyhow::ensure!(open.is_success(), "open did not complete: {open:?}");
                let data = open
                    .into_success()
                    .map(|success| success.data)
                    .context("open returned no data")?;
                let buffer_id = data
                    .get("buffer_id")
                    .and_then(|v| v.as_u64())
                    .context("open returned no buffer id")?;
                let revision = data
                    .get("revision")
                    .and_then(|v| v.as_u64())
                    .context("open returned no revision")?;
                let converted = run(
                    "note.convert_encoding",
                    serde_json::json!({
                        "buffer_id": buffer_id,
                        "revision": revision,
                        "encoding": code,
                        "bom": bom.arg(),
                    }),
                )?;
                anyhow::ensure!(
                    converted.is_success(),
                    "conversion was refused: {converted:?}"
                );
                let saved = run("note.save", serde_json::json!(note))?;
                anyhow::ensure!(saved.is_success(), "save did not complete: {saved:?}");
                if self.json {
                    println!(
                        "{}",
                        serde_json::to_string(&serde_json::json!({
                            "note": note,
                            "encoding": code,
                            "bom": bom.arg(),
                            "converted": true,
                            "saved": true,
                        }))?
                    );
                } else {
                    println!(
                        "{}: converted to {code}{} and saved",
                        note.display(),
                        bom.arg()
                    );
                }
            }
        }
        Ok(())
    }

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

    /// Run a saved-content search through a materialized search session
    /// (RFC-0006 S06): explicit modes, page tokens with total/next, and a
    /// deterministic machine-readable error object. The wrapper waits only
    /// on its caller thread — never on the SQL actor or the search-writer
    /// thread (§12).
    #[allow(clippy::too_many_lines)]
    fn run_search(
        &self,
        _vault_path: &Path,
        term: &str,
        mode: CliSearchMode,
        field: Option<&str>,
        limit: usize,
        consume_all: bool,
    ) -> Result<()> {
        info!("Running search: {}", term);
        use flavite_api::search_sessions::{SearchMode, SearchQueryOptions};

        let flavite = self
            .flavite
            .as_ref()
            .context("search requires an opened vault")?;
        let page_size = u64::try_from(limit.clamp(1, 200)).unwrap_or(50);
        let outcome = match mode {
            CliSearchMode::Literal | CliSearchMode::Regex => {
                return self.run_vault_source_find(term, mode, page_size, consume_all);
            }
            CliSearchMode::Text | CliSearchMode::Expression => {
                let text = match (field, mode) {
                    (Some(field), _) => format!("{field}:{term}"),
                    (None, _) => term.to_owned(),
                };
                let options = SearchQueryOptions {
                    mode: match mode {
                        CliSearchMode::Text => SearchMode::FullText,
                        CliSearchMode::Expression => SearchMode::Expression,
                        CliSearchMode::Literal | CliSearchMode::Regex => unreachable!(),
                    },
                    text,
                    page_size,
                };
                flavite.search_start(options)
            }
        };
        let mut page = outcome.map_err(|error| {
            eprintln!("{}", json_error(&error));
            anyhow::anyhow!("search failed")
        })?;
        let mut pages = 0u64;
        loop {
            pages += 1;
            if self.json {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({
                        "session": page.session.as_hex(),
                        "epoch": page.epoch,
                        "generation": page.generation,
                        "total": page.total,
                        "rows": page.rows.iter().map(|row| serde_json::json!({
                            "path": row.path,
                            "title": row.title,
                            "excerpt": row.excerpt,
                            "score": row.score,
                        })).collect::<Vec<_>>(),
                        "next": page.next.as_ref().map(|token| token.next_ordinal),
                    }))?
                );
            } else {
                for row in &page.rows {
                    println!("{}\t{}", row.path, row.title);
                }
                if let Some(token) = &page.next {
                    eprintln!(
                        "page {pages}: {} of {} results; next ordinal {}",
                        token.next_ordinal, page.total, token.next_ordinal
                    );
                } else {
                    eprintln!(
                        "{} results in {} page(s); snapshot {}/{}",
                        page.total, pages, page.epoch, page.generation
                    );
                }
            }
            match &page.next {
                Some(_) if consume_all => {
                    let token = page.next.expect("next checked");
                    page = flavite.search_page(&token).map_err(|error| {
                        eprintln!("{}", json_error(&error));
                        anyhow::anyhow!("search failed")
                    })?;
                }
                _ => break,
            }
        }
        // Explicit session close (search.close, §6.1): the spool is dropped.
        flavite.search_close(page.session).map_err(|error| {
            eprintln!("{}", json_error(&error));
            anyhow::anyhow!("search failed")
        })?;
        Ok(())
    }

    /// Vault source find (RFC-0006 S07): literal/regex matching over saved
    /// Markdown source with exact per-note match paging (D-08).
    fn run_vault_source_find(
        &self,
        term: &str,
        mode: CliSearchMode,
        page_size: u64,
        consume_all: bool,
    ) -> Result<()> {
        let flavite = self
            .flavite
            .as_ref()
            .context("search requires an opened vault")?;
        let request_id = flavite.command_registry().next_request_id();
        let result = flavite.execute(Invocation::new(
            request_id,
            "source_find.start",
            serde_json::json!({
                "query": term,
                "mode": if matches!(mode, CliSearchMode::Regex) { "regex" } else { "literal" },
            }),
            Origin::Cli,
        ));
        let result = match result {
            ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
            result => result,
        };
        let ExecutionResult::Local(LocalResult::Success(start)) = result else {
            eprintln!("{}", serde_json::to_string(&execution_error_json(&result))?);
            anyhow::bail!("search failed");
        };
        let finder = start
            .data
            .get("finder_id")
            .and_then(serde_json::Value::as_str)
            .context("source find result lacks finder_id")?
            .to_owned();
        let mut pages = 0u64;
        let mut cursor = start.data.clone();
        loop {
            pages += 1;
            self.print_source_find_page(&cursor, mode, pages);
            let next_note = cursor
                .get("page")
                .and_then(|page| page.get("next_note_index"))
                .and_then(serde_json::Value::as_u64);
            let next_match = cursor
                .get("page")
                .and_then(|page| page.get("next_match_offset"))
                .and_then(serde_json::Value::as_u64);
            let note_index = cursor
                .get("page")
                .and_then(|page| page.get("note_index"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            if !consume_all || (next_note.is_none() && next_match.is_none()) {
                break;
            }
            let request_id = flavite.command_registry().next_request_id();
            let result = flavite.execute(Invocation::new(
                request_id,
                "source_find.page",
                serde_json::json!({
                    "finder_id": finder,
                    "note_index": note_index,
                    "match_offset": next_match.unwrap_or(0),
                    "page_len": page_size,
                }),
                Origin::Cli,
            ));
            let result = match result {
                ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
                result => result,
            };
            let ExecutionResult::Local(LocalResult::Success(next)) = result else {
                eprintln!("{}", serde_json::to_string(&execution_error_json(&result))?);
                anyhow::bail!("search failed");
            };
            cursor = next.data.clone();
        }
        let request_id = flavite.command_registry().next_request_id();
        let _ = flavite.execute(Invocation::new(
            request_id,
            "source_find.close",
            serde_json::json!({ "finder_id": finder }),
            Origin::Cli,
        ));
        Ok(())
    }

    fn print_source_find_page(&self, data: &serde_json::Value, mode: CliSearchMode, pages: u64) {
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "mode": if matches!(mode, CliSearchMode::Regex) { "regex" } else { "literal" },
                    "snapshot": data.get("snapshot"),
                    "total_notes": data.get("total_notes"),
                    "total_matches": data.get("total_matches"),
                    "truncated_scope": data.get("truncated_scope"),
                    "exclusions": data.get("exclusions"),
                    "page": data.get("page"),
                }))
                .unwrap_or_default()
            );
        } else {
            if let Some(rows) = data
                .get("page")
                .and_then(|page| page.get("rows"))
                .and_then(serde_json::Value::as_array)
            {
                for row in rows {
                    let path = row
                        .get("path")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("");
                    let count = row
                        .get("note_matches")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(0);
                    println!("{path}\t{count}");
                }
            }
            if pages > 1 {
                eprintln!("page {pages}: source find continuation consumed");
            }
        }
    }

    fn run_rename(&self, from: PathBuf, to: PathBuf, strategy: CliRenameStrategy) -> Result<()> {
        let flavite = self
            .flavite
            .as_ref()
            .context("rename requires an opened vault")?;
        let request_id = flavite.command_registry().next_request_id();
        let result = flavite.execute(Invocation::new(
            request_id,
            "note.rename",
            serde_json::json!({
                "from": from,
                "to": to,
                "strategy": strategy.as_str(),
            }),
            Origin::Cli,
        ));
        let result = match result {
            ExecutionResult::Pending { request_id } => flavite.wait_for_operation(request_id),
            result => result,
        };
        anyhow::ensure!(
            result.is_success(),
            "rename did not complete successfully: {result:?}"
        );
        if self.json {
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({
                    "strategy": strategy.as_str(),
                    "result": result,
                }))?
            );
        } else {
            println!(
                "Rename completed with {} strategy: {result:?}",
                strategy.as_str()
            );
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
                    "autosave.failures_total": info.autosave_failures_total,
                    "rename.items_skipped_total": info.rename_items_skipped_total,
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
            println!("Autosave failures: {}", info.autosave_failures_total);
            println!("Rename items skipped: {}", info.rename_items_skipped_total);
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
    let prior_hook = std::panic::take_hook();
    let hook_log_path = log_path.clone();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = &hook_log_path
            && let Some(parent) = path.parent()
        {
            let crash_path = parent.join("crash-report.txt");
            let tail = fs::read_to_string(path).unwrap_or_default();
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
            let _ = fs::write(crash_path, report);
        }
        prior_hook(info);
    }));
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

/// Render a typed search-session error as the deterministic machine-readable
/// error object (RFC-0006 S06).
fn json_error(error: &flavite_api::FlaviteError) -> String {
    let kind = match error {
        flavite_api::FlaviteError::SearchSession(inner) => match inner {
            flavite_api::search_sessions::SearchSessionError::Busy(_) => "busy",
            flavite_api::search_sessions::SearchSessionError::Invalid { .. } => "invalid",
            flavite_api::search_sessions::SearchSessionError::LimitExceeded { .. } => {
                "limit-exceeded"
            }
            flavite_api::search_sessions::SearchSessionError::ResetRequired => "reset-required",
            flavite_api::search_sessions::SearchSessionError::Expired => "expired",
            flavite_api::search_sessions::SearchSessionError::Failed(_) => "failed",
        },
        _ => "failed",
    };
    serde_json::json!({ "error": { "kind": kind, "message": error.to_string() } }).to_string()
}

/// Render an `ExecutionResult` failure as the deterministic machine-readable
/// error object (RFC-0006 S07 source-find paths).
fn execution_error_json(result: &ExecutionResult) -> serde_json::Value {
    let inner = match result {
        ExecutionResult::Local(LocalResult::Error(error)) => {
            serde_json::json!({ "kind": "error", "message": error.message })
        }
        ExecutionResult::Rejected(rejected) => serde_json::json!({
            "kind": "rejected",
            "reason": format!("{:?}", rejected.reason),
            "message": rejected.message,
        }),
        ExecutionResult::ActionRequired { code, .. } => {
            serde_json::json!({ "kind": "action-required", "code": code })
        }
        _ => serde_json::json!({ "kind": "failed" }),
    };
    serde_json::json!({ "error": inner })
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
        Commands::Search {
            path,
            term,
            mode,
            field,
            limit,
            all,
        } => {
            let vault_path = get_vault_path(path, cli.vault);
            driver.open_vault(vault_path.clone(), cli.no_watch)?;
            driver.run_search(&vault_path, &term, mode, field.as_deref(), limit, all)?;
        }
        Commands::Rename { from, to, strategy } => {
            let vault_path = get_vault_path(None, cli.vault);
            driver.open_vault(vault_path, cli.no_watch)?;
            driver.run_rename(from, to, strategy)?;
        }
        Commands::Encoding { note, action } => {
            let vault_path = get_vault_path(None, cli.vault);
            driver.open_vault(vault_path, cli.no_watch)?;
            driver.run_encoding(note, action)?;
        }
        Commands::Theme { action } => match action {
            // The converter is an offline artifact tool: no vault needed.
            ThemeAction::Convert { input, output } => driver.convert_theme(input, output)?,
            ThemeAction::Install { path } => {
                let vault_path = get_vault_path(None, cli.vault);
                driver.open_vault(vault_path, cli.no_watch)?;
                driver.run_theme_command(
                    "theme.install",
                    serde_json::json!({
                        "path": path.display().to_string(),
                    }),
                )?;
            }
            ThemeAction::List => {
                let vault_path = get_vault_path(None, cli.vault);
                driver.open_vault(vault_path, cli.no_watch)?;
                driver.run_theme_command("theme.list", serde_json::json!({}))?;
            }
            ThemeAction::Select { id } => {
                let vault_path = get_vault_path(None, cli.vault);
                driver.open_vault(vault_path, cli.no_watch)?;
                driver.select_theme(&id)?;
            }
            ThemeAction::Uninstall { id } => {
                let vault_path = get_vault_path(None, cli.vault);
                driver.open_vault(vault_path, cli.no_watch)?;
                driver.run_theme_command("theme.uninstall", serde_json::json!({ "id": id }))?;
            }
        },
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
        let events = flavite.poll_events(256);
        if !events.is_empty() {
            // S4: a contained vault-session panic gets an operator warning
            // line here; it also surfaces below through the generic stderr
            // event print (json or human), no vault-type import required.
            for delivery in &events {
                if let flavite_api::Event::SessionPanicked {
                    message,
                    panics_total,
                    ..
                } = &delivery.event
                {
                    tracing::warn!(
                        panics_total,
                        "vault session contained an internal panic: {message}"
                    );
                }
            }
            if driver.json {
                for delivery in events {
                    eprintln!("{}", serde_json::to_string(&delivery.event)?);
                    if let Some(id) = delivery.notice_id {
                        flavite.acknowledge_notice(id);
                    }
                }
            } else {
                for delivery in events {
                    eprintln!(
                        "event {}: {:?}",
                        delivery.event.event_type(),
                        delivery.event
                    );
                    if let Some(id) = delivery.notice_id {
                        flavite.acknowledge_notice(id);
                    }
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
