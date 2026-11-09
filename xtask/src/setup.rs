//! Developer onboarding (§10.8): `just setup` — the first command after clone.
//!
//! Installs Rust components, the nightly toolchain, cargo sub-tools, and the
//! pre-push hook. Idempotent: re-running is a no-op success.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use std::process::Command;

/// The pre-push hook script content (§10.3: G0 + fuzz smoke, bypassable via
/// `--no-verify`; main is gated by the CI ruleset once it lands — see
/// devdocs/plans/ci-gate-backend-headless-2026-11-08.md Phase 8).
const PRE_PUSH_HOOK: &str = r#"#!/usr/bin/env bash
# flavite pre-push hook (§10.3): runs G0 (just gate) + fuzz smoke.
# Bypass with `git push --no-verify`; main additionally requires green CI via PR.
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

/// Check if a binary file exists on PATH WITHOUT executing it. Cargo
/// subcommand binaries (cargo-llvm-cov, cargo-binstall, …) reject a bare
/// `--version` when run directly, so `command_exists` cannot detect them —
/// use this for install-skip checks on cargo sub-tools.
fn on_path(name: &str) -> bool {
    let Ok(paths) = std::env::var("PATH") else {
        return false;
    };
    let exe = if std::env::consts::OS == "windows" {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&paths).any(|dir| dir.join(&exe).is_file() || dir.join(name).is_file())
}

/// Install ripgrep per-OS. The source-gate scripts (`scripts/*.sh`) and
/// `just lint-arch` shell out to `rg`, and GitHub-hosted runners do not ship
/// it — a missing `rg` turns the gate into exit 127 (audit 2026-11-08, H4).
/// Unlike the cargo tools this MUST succeed: return Err on failure.
fn install_ripgrep() -> Result<()> {
    if command_exists("rg") {
        println!("  ✓ rg already installed");
        return Ok(());
    }
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = Command::new("brew");
        c.args(["install", "ripgrep"]);
        c
    } else if cfg!(target_os = "windows") {
        if command_exists("choco") {
            let mut c = Command::new("choco");
            c.args(["install", "ripgrep", "-y"]);
            c
        } else {
            // Fallback: build from source (slow, but correct on any host).
            let mut c = Command::new("cargo");
            c.args(["install", "ripgrep", "--locked"]);
            c
        }
    } else {
        let mut c = Command::new("sudo");
        c.args(["apt-get", "install", "-y", "ripgrep"]);
        c
    };
    let status = cmd
        .status()
        .context("failed to spawn the ripgrep installer")?;
    if !status.success() {
        anyhow::bail!(
            "ripgrep install failed with {status}; the gate scripts require `rg` on PATH"
        );
    }
    if !command_exists("rg") {
        anyhow::bail!("ripgrep install reported success but `rg --version` still fails");
    }
    println!("  ✓ rg installed and verified");
    Ok(())
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

/// Pinned cargo-binstall release used to fetch PREBUILT binaries for the
/// cargo sub-tools. Source-compiling cargo-deny/cargo-audit/cargo-llvm-cov
/// on every cold CI runner cost 10+ min of the gate (NFR-1), so setup
/// downloads binstall itself (sha256-pinned per platform asset) and only
/// falls back to `cargo install --locked` when no prebuilt exists.
const B_INSTALL_VERSION: &str = "1.26.2";

/// (asset filename, release sha256) of cargo-binstall for this platform.
fn binstall_asset() -> Option<(&'static str, &'static str)> {
    use std::env::consts::{ARCH, OS};
    Some(match (OS, ARCH) {
        ("linux", "x86_64") => (
            "cargo-binstall-x86_64-unknown-linux-gnu.tgz",
            "4f28eba175325f67f7c4bb8ae8686914d2d88fead279c0b8c4d3125264d7a512",
        ),
        ("linux", "aarch64") => (
            "cargo-binstall-aarch64-unknown-linux-gnu.tgz",
            "027438af19e7d9d90accabbb047101c8636c40253f760e92e84cf2f90feddad3",
        ),
        ("macos", _) => (
            "cargo-binstall-universal-apple-darwin.zip",
            "939e2e61eb4092fe8532769bf5179694be3acff239a13defe339234db6c2bd8e",
        ),
        ("windows", "x86_64") => (
            "cargo-binstall-x86_64-pc-windows-msvc.zip",
            "c377a4977ffdbdcbf58dd263055b2f1d56ba873c69fbd6bfc60bef98256183a3",
        ),
        ("windows", "aarch64") => (
            "cargo-binstall-aarch64-pc-windows-msvc.zip",
            "3736cca189d958ae80bd5770e77cce9bbc5653bb2e9561e5002e766f2a185804",
        ),
        _ => return None,
    })
}

/// Hash a file with the platform's tool and return the hex digest.
fn sha256_of(path: &Path) -> Result<String> {
    use std::env::consts::OS;
    let out = match OS {
        "windows" => Command::new("certutil")
            .args(["-hashfile", &path.to_string_lossy(), "SHA256"])
            .output()?,
        "macos" => Command::new("shasum")
            .args(["-a", "256", &path.to_string_lossy()])
            .output()?,
        _ => Command::new("sha256sum").arg(path).output()?,
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.split_whitespace()
        .next()
        .map(str::to_lowercase)
        .context("no digest in hash tool output")
}

/// Download the pinned cargo-binstall release into ~/.cargo/bin.
/// Soft-fails (warn) — the sub-tools fall back to source builds without it.
fn install_binstall() -> Result<()> {
    if on_path("cargo-binstall") {
        println!("  ✓ cargo-binstall already installed");
        return Ok(());
    }
    let Some((asset, expected)) = binstall_asset() else {
        println!(
            "  ⚠ no pinned cargo-binstall asset for this platform; sub-tools will build from source"
        );
        return Ok(());
    };
    let url = format!(
        "https://github.com/cargo-bins/cargo-binstall/releases/download/v{B_INSTALL_VERSION}/{asset}"
    );
    let home = std::env::var("HOME").context("HOME not set")?;
    let bin_dir = Path::new(&home).join(".cargo/bin");
    fs::create_dir_all(&bin_dir)?;
    let archive =
        std::env::temp_dir().join(format!("flavite-binstall-{B_INSTALL_VERSION}-{asset}"));
    let extract_dir = std::env::temp_dir().join(format!("flavite-binstall-{B_INSTALL_VERSION}"));
    fs::create_dir_all(&extract_dir)?;

    println!("  → downloading cargo-binstall v{B_INSTALL_VERSION} ({asset})");
    let status = Command::new("curl")
        .args(["-sSfL", &url, "-o", &archive.to_string_lossy()])
        .status()
        .context("failed to spawn curl")?;
    if !status.success() {
        println!("  ⚠ curl failed; sub-tools will build from source");
        return Ok(());
    }
    let actual = sha256_of(&archive)?;
    if actual != expected {
        anyhow::bail!(
            "cargo-binstall download checksum mismatch: got {actual}, expected {expected}"
        );
    }
    // bsdtar (ships with macOS/Windows 10+) handles both .tgz and .zip.
    let status = Command::new("tar")
        .args([
            "-xf",
            &archive.to_string_lossy(),
            "-C",
            &extract_dir.to_string_lossy(),
        ])
        .status()
        .context("failed to spawn tar")?;
    if !status.success() {
        anyhow::bail!("failed to extract {asset}");
    }
    let exe = if std::env::consts::OS == "windows" {
        "cargo-binstall.exe"
    } else {
        "cargo-binstall"
    };
    let src = extract_dir.join(exe);
    let dst = bin_dir.join(exe);
    fs::copy(&src, &dst).with_context(|| format!("failed to install {}", dst.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dst)?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&dst, perms)?;
    }
    println!("  ✓ cargo-binstall v{B_INSTALL_VERSION} installed (sha256 verified)");
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

    // 3. Cargo sub-tools — pinned versions, PREBUILT via cargo-binstall when
    // available (seconds even on a cold runner); `cargo install --locked`
    // (source compile, minutes) is only the fallback.
    println!("[3/4] Cargo sub-tools (pinned; prebuilt via cargo-binstall)");
    install_binstall()?;
    let tools: [(&str, &str); 5] = [
        ("cargo-fuzz", "0.13.2"),
        ("cargo-deny", "0.20.2"),
        ("cargo-audit", "0.22.2"),
        ("cargo-llvm-cov", "0.9.1"),
        ("cargo-shear", "1.14.0"),
    ];
    for (name, version) in &tools {
        if on_path(name) {
            println!("  ✓ {name} already installed");
            continue;
        }
        let spec = format!("{name}@{version}");
        if on_path("cargo-binstall") {
            println!("  → {spec} (prebuilt via cargo-binstall)");
            let ok = Command::new("cargo")
                .args(["binstall", "--no-confirm", &spec])
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok && on_path(name) {
                println!("  ✓ {name} {version} installed (prebuilt)");
                continue;
            }
            println!("  ⚠ prebuilt install failed for {spec}; falling back to source build");
        }
        println!("  → installing {name} {version} from source (slow)...");
        // cargo-shear's source build needs nightly; the prebuilt path does not.
        let mut cmd = Command::new("cargo");
        if *name == "cargo-shear" {
            cmd.arg("+nightly");
        }
        cmd.args(["install", name, "--locked", "--version", version]);
        let status = cmd
            .status()
            .with_context(|| format!("failed to spawn cargo install {name}"))?;
        if !status.success() || !on_path(name) {
            anyhow::bail!(
                "failed to install {name} {version} (both prebuilt and source paths failed)"
            );
        }
        println!("  ✓ {name} {version} installed (source)");
    }

    // ripgrep: required by scripts/check-workspace-contracts.sh,
    // scripts/lib/source-gates.sh and `just lint-arch`. Hard failure if it
    // cannot be installed — a missing `rg` must never silently skip a gate.
    install_ripgrep()?;

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
    fn binstall_asset_is_pinned_for_supported_platforms() {
        use std::env::consts::{ARCH, OS};
        if matches!(
            (OS, ARCH),
            ("linux", "x86_64" | "aarch64") | ("macos", _) | ("windows", "x86_64" | "aarch64")
        ) {
            let (asset, sha) = binstall_asset().expect("asset for supported platform");
            assert!(asset.starts_with("cargo-binstall-"));
            assert_eq!(sha.len(), 64, "sha256 hex digest");
            assert!(
                sha.chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
            );
        }
    }

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
