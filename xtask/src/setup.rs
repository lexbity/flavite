//! Developer onboarding (§10.8): `just setup` — the first command after clone.
//!
//! Installs Rust components, the nightly toolchain, cargo sub-tools, and the
//! pre-push hook. Idempotent: re-running is a no-op success.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

/// The pre-push hook script content (§10.3: G0 + fuzz smoke, bypassable via
/// `--no-verify`; G1 is the real wall).
const PRE_PUSH_HOOK: &str = r#"#!/usr/bin/env bash
# flavite pre-push hook (§10.3): runs G0 (just gate) + fuzz smoke.
# Bypass with `git push --no-verify` — CI still blocks on G1.
set -euo pipefail

echo "=== pre-push: just gate (G0) ==="
just gate

echo "=== pre-push: just fuzz-smoke ==="
just fuzz-smoke

echo "=== pre-push: all checks passed ==="
"#;

/// Run a command, returning Ok if it succeeds or if the tool is already present.
fn run_or_skip(cmd: &mut Command, description: &str) -> Result<()> {
    match cmd.status() {
        Ok(status) if status.success() => {
            println!("  ✓ {description}");
            Ok(())
        }
        Ok(status) => {
            println!("  ⚠ {description} exited with {status} (continuing)");
            Ok(())
        }
        Err(e) => {
            println!("  ⚠ {description} failed to start: {e} (continuing)");
            Ok(())
        }
    }
}

/// Check if a command exists on PATH.
fn command_exists(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Install the pre-push hook into .git/hooks/.
fn install_hook(workspace_root: &Path) -> Result<()> {
    let git_dir = workspace_root.join(".git");
    if !git_dir.exists() {
        println!("  ⚠ .git not found — skipping hook installation (run `git init` first)");
        return Ok(());
    }
    let hooks_dir = git_dir.join("hooks");
    fs::create_dir_all(&hooks_dir).context("failed to create hooks directory")?;
    let hook_path = hooks_dir.join("pre-push");
    fs::write(&hook_path, PRE_PUSH_HOOK).context("failed to write pre-push hook")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&hook_path, perms)?;
    }
    println!("  ✓ pre-push hook installed at {}", hook_path.display());
    Ok(())
}

/// Run `just setup` (§10.8).
pub fn setup() -> Result<()> {
    println!("flavite setup — developer onboarding (§10.8)");
    println!();

    // 1. Rust components.
    println!("[1/4] Rust components (rustfmt, clippy, llvm-tools-preview)");
    run_or_skip(
        Command::new("rustup").args([
            "component",
            "add",
            "rustfmt",
            "clippy",
            "llvm-tools-preview",
        ]),
        "rustup component add rustfmt clippy llvm-tools-preview",
    )?;

    // 2. Nightly toolchain (for cargo-fuzz, §10.6).
    println!("[2/4] Nightly toolchain (for cargo-fuzz)");
    run_or_skip(
        Command::new("rustup").args(["toolchain", "install", "nightly", "--profile", "minimal"]),
        "rustup toolchain install nightly",
    )?;

    // 3. Cargo sub-tools.
    println!("[3/4] Cargo sub-tools");
    let tools = [
        ("cargo-fuzz", "cargo-fuzz"),
        ("cargo-deny", "cargo-deny"),
        ("cargo-audit", "cargo-audit"),
        ("cargo-llvm-cov", "cargo-llvm-cov"),
    ];
    for (install_name, bin_name) in &tools {
        if command_exists(bin_name) {
            println!("  ✓ {bin_name} already installed");
        } else {
            println!("  → installing {install_name} (this may take a while)...");
            run_or_skip(
                Command::new("cargo").args(["install", install_name, "--locked"]),
                &format!("cargo install {install_name}"),
            )?;
        }
    }
    // cargo-shear requires nightly.
    if command_exists("cargo-shear") {
        println!("  ✓ cargo-shear already installed");
    } else {
        println!("  → installing cargo-shear (this may take a while)...");
        run_or_skip(
            Command::new("cargo").args(["+nightly", "install", "cargo-shear", "--locked"]),
            "cargo +nightly install cargo-shear",
        )?;
    }

    // 4. Pre-push hook.
    println!("[4/4] Pre-push hook");
    let root = crate::clean::find_workspace_root()?;
    install_hook(&root)?;

    println!();
    println!("Setup complete. Run `just gate` to verify the workspace.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_script_contains_gate_and_fuzz() {
        assert!(PRE_PUSH_HOOK.contains("just gate"));
        assert!(PRE_PUSH_HOOK.contains("just fuzz-smoke"));
        assert!(PRE_PUSH_HOOK.contains("--no-verify"));
    }

    #[test]
    fn hook_script_is_valid_bash() {
        assert!(PRE_PUSH_HOOK.starts_with("#!/usr/bin/env bash"));
        assert!(PRE_PUSH_HOOK.contains("set -euo pipefail"));
    }

    #[test]
    fn command_exists_finds_cargo() {
        // cargo should always be available in a Rust dev environment.
        assert!(command_exists("cargo"));
    }

    #[test]
    fn command_exists_rejects_nonexistent() {
        assert!(!command_exists("flavite-nonexistent-command-xyz"));
    }
}
