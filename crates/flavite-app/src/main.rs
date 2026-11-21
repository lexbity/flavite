//! The Flavite desktop application (RFC-0003 TD-02/W10/W13).
//!
//! Production startup opens the configured vault — never demo content —
//! and launches the workspace shell through the accessibility-patched
//! runtime. With no vault configured or openable, the start screen offers
//! real open/create actions (FR-22).

use clap::Parser;

/// The Flavite desktop workspace.
#[derive(Parser, Debug)]
#[command(
    name = "flavite",
    version = env!("CARGO_PKG_VERSION"),
    about = "A local-first plain-text knowledge vault (desktop workspace)"
)]
struct Args {
    /// Vault root folder to open. Falls back to the FLAVITE_VAULT
    /// environment variable; with neither, the start screen offers
    /// open/create actions.
    #[arg(short, long, value_name = "PATH", env = "FLAVITE_VAULT")]
    vault: Option<std::path::PathBuf>,
}

fn main() -> Result<(), iced_winit::Error> {
    let args = Args::parse();
    // The shell reads the vault root from the environment (host/env.rs);
    // an explicit --vault overrides whatever was there. Parsing happens
    // before the window loop, so `--help`/`--version` exit immediately
    // instead of "hanging" in the event loop. This runs before any thread
    // is spawned, so the environment write cannot race a reader.
    if let Some(vault) = args.vault {
        // Safety: no other threads exist yet in `main`.
        unsafe { std::env::set_var("FLAVITE_VAULT", &vault) };
    }
    // A windowed application cannot start without a display server; fail
    // with a clear message instead of the event loop's panic backtrace.
    let has_display = std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var_os("WAYLAND_SOCKET").is_some()
        || std::env::var_os("DISPLAY").is_some();
    if !has_display {
        eprintln!(
            "flavite is a desktop application: no display server is available \
             (set WAYLAND_DISPLAY or DISPLAY).\nUse `flavite-cli` for headless vault access."
        );
        std::process::exit(1);
    }
    install_trace_subscriber();
    iced_winit::run(flavite_ui::accessibility::program::FlaviteProgram)
}

/// RFC-0008 S1 (FR-10): install the `fmt` subscriber when `FLAVITE_TRACE` is
/// set. Default off — with the env var unset, no subscriber is installed and
/// the tracing events/spans compiled into the keystroke path are inert (zero
/// emission, zero hot-path allocation, S1-T2).
///
/// The filter falls back to `trace` when `RUST_LOG` is unset; setting
/// `RUST_LOG` alongside `FLAVITE_TRACE` narrows the recorded levels.
fn install_trace_subscriber() {
    if std::env::var_os("FLAVITE_TRACE").is_none() {
        return;
    }
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("trace"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .try_init()
        .expect("FLAVITE_TRACE subscriber install failed (a subscriber is already set)");
}
