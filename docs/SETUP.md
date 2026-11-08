# Setup and installation

How to build, install, and first-run Flavite from source. Every command and
path here reflects the current codebase.

## Prerequisites

| Requirement | Details |
|---|---|
| Rust toolchain | Pinned to **1.98.1** by `rust-toolchain.toml` (rustup installs it automatically on first build). Components used: `rustfmt`, `clippy`, `llvm-tools-preview`. The workspace MSRV is 1.90.0 (enforced by CI, not by the pin). |
| `just` | Command runner for the development recipes. |
| Display server (desktop app only) | `flavite` needs Wayland (`WAYLAND_DISPLAY`/`WAYLAND_SOCKET`) or X11 (`DISPLAY`). Without one it prints an error and exits 1, pointing you to `flavite-cli` for headless use. |
| GPU (graph view) | The graph view defaults to an instanced wgpu (Vulkan) renderer. Without a working adapter the app still runs — notes and editing are never disabled — but see [Graph backend](#graph-backend) for the software fallback. |

Headless use (`flavite-cli`) needs none of the display/GPU requirements.

## Install

```sh
git clone https://github.com/lexbity/flavite
cd flavite
cargo build -p flavite-app -p flavite-cli -p flavite-text-search-worker
```

This produces three binaries in `target/debug/`:

- `flavite` — the desktop app (crate `flavite-app`).
- `flavite-cli` — the headless CLI (crate `flavite-cli`).
- `flavite-text-search-worker` — the isolated regex helper. Keep this binary
  in the same directory as the desktop and CLI binaries; the applications
  resolve that exact sibling and never search `PATH` or accept a configured
  executable path. If it is absent or has a mismatched protocol version,
  regex Find/Replace reports that state and literal matching remains usable.

For a faster daily-driver build use
`cargo build --release -p flavite-app -p flavite-cli -p flavite-text-search-worker`.
When installing the executables manually, install the helper into the same
`bin` directory as both Flavite binaries.

## Developer setup (one command after clone)

If you plan to work on Flavite itself, run:

```sh
just setup
```

This runs `cargo xtask setup`, which is idempotent and installs:

1. The pinned Rust components (`rustfmt`, `clippy`, `llvm-tools-preview`).
2. The **nightly** toolchain (minimal profile) — used by `cargo-fuzz`.
3. Cargo tools, if missing: `cargo-fuzz`, `cargo-deny`, `cargo-audit`,
   `cargo-llvm-cov`, and `cargo-shear` (nightly).
4. A **pre-push git hook** that runs `just gate` + `just fuzz-smoke` before
   every push. Bypass a single push with `git push --no-verify` — CI still
   enforces the full gate.

Individual steps tolerate failure with a warning and continue; setup ends by
suggesting `just gate` to verify the workspace.

## First run

The desktop app never creates note content for you. A folder of Markdown
files is **adopted as a vault on first open**:

- With no `--vault` argument and no `FLAVITE_VAULT` set, `flavite` opens the
  start screen: type a vault folder path and press **Open**, use the
  **Choose folder…** native dialog, or use **Open last vault** if one is
  remembered on this device.
- With `--vault PATH` (or the `FLAVITE_VAULT` env var), the app opens that
  vault directly. A non-existent path cannot be adopted; the folder must
  exist.

To create a vault explicitly, use the CLI:

```sh
flavite-cli init --path /path/to/vault    # creates the directory if needed, writes vault.toml
```

First open of a vault writes `<vault>/.flavite/vault.toml` (its identity) and
registers it in the OS data dir.

## Verify the installation

```sh
# Vault health check (exit code 0 = healthy, nonzero only on ERROR-level findings)
flavite-cli doctor --path /path/to/vault
flavite-cli doctor --path /path/to/vault --json   # machine-readable report

# Workspace-level verification (developer installs)
just gate      # fmt + clippy + tests + arch check, ~5 min
```

`doctor` checks SQLite integrity, tantivy index generation, broken/ambiguous
links, duplicate IDs, attachments, and more. Warnings are reported but do not
fail the command.

## Platform notes

### Linux

- Wayland and X11 are both supported for the app (winit decides).
- Development-only visual harnesses need extra packages:
  `just ui-visual-setup` installs `xvfb`, `xdotool`, `imagemagick`,
  `python3-pil`, `tesseract-ocr` via apt (Debian-style systems; elsewhere
  install them manually).
- CI uses `just gpu-setup` (mesa-vulkan-drivers / lavapipe) on machines
  without a GPU.

### macOS / Windows

Covered by CI (`just setup`, `just gate`, docs, deny/audit, MSRV). No extra
packages are required. The visual/fuzz/coverage harnesses are Linux-only.

## Graph backend

The graph view has two interchangeable renderers publishing identical
geometry:

| Backend | Selection |
|---|---|
| GPU (default) | Instanced wgpu shader. |
| Canvas | Software renderer. Force with `FLAVITE_GRAPH_BACKEND=canvas`. |

A failed GPU initialization never disables the network or note editing —
switching to `canvas` is a workaround for driver problems, not a necessity.

## Environment variables (user-relevant)

| Variable | Effect |
|---|---|
| `FLAVITE_VAULT` | Default vault folder for the desktop app. Overridden by `--vault PATH`. Unset: start screen / fallback root. |
| `LATTICE_VAULT` | **Deprecated.** Honored only if `FLAVITE_VAULT` is unset; prints a deprecation notice. |
| `FLAVITE_DATA_DIR` | When set to an absolute path, replaces the OS data dir entirely (vault registry, root guards, instance locks, app-launch memory, encoding hints, recovery checkpoints, operation keys). Vault-rooted state under `<vault>/.flavite/` is unaffected. |
| `FLAVITE_GRAPH_BACKEND` | `gpu` (default) or `canvas` for the graph renderer. |

Additional variables exist for test harnesses and crash-simulation
(`FLAVITE_HARNESS_*`, `FLAVITE_RENAME_KILL_ROOT`, `FLAVITE_W12_CRASH_ROOT`,
`FLAVITE_TANTIVY_KILL_CHILD_ROOT`, `FLAVITE_RECOVERY_KILL_CHILD_ROOT`, …).
Do not set these outside test scenarios: several are destructive hooks that
kill processes or inject UI faults.

## Where state lives

| What | Where |
|---|---|
| Your notes | Your vault folder (plain Markdown). |
| Vault identity | `<vault>/.flavite/vault.toml` |
| Graph pins | `<vault>/.flavite/graph.json` |
| Workspace layout | `<vault>/.flavite/workspace-layouts.json` (plus last-good `.bak`) |
| Preferences / settings | `<vault>/.flavite/settings.toml` |
| Installed themes | `<vault>/.flavite/themes/<id>/` |
| Recovery journal | `<vault>/.flavite/journal/` |
| Durable replacement jobs | `<vault>/.flavite/operations/replace/` |
| Remembered last vault | `<data>/flavite/app/app-launch.json` |
| Vault registry / root guards / instance locks | `<data>/flavite/vaults/`, `<data>/flavite/roots/` |
| Recovery checkpoints / operation keys | `<data>/flavite/vaults/<vault-uuid>/recovery/`, `<data>/flavite/vaults/<vault-uuid>/operations/` |

Everything under `<vault>/.flavite/` is vault-rooted: it travels with the
vault and is never machine-pinned. Everything under `<data>` is device-local.
`<data>` is the OS data directory for the app (`ProjectDirs` for
`com.flavite/Flavite` — e.g. `~/.local/share/Flavite` on Linux), with
`FLAVITE_DATA_DIR` overriding it entirely when set to an absolute path.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| `flavite` exits with "no display server is available" | No Wayland/X11 environment. Run inside a desktop session, or use `flavite-cli` headlessly. |
| "Vault does not exist at …" (CLI) | The path does not exist or is not a directory. Create/adopt it first (`flavite-cli init` or the app's start screen). |
| Graph view blank / GPU errors | Try `FLAVITE_GRAPH_BACKEND=canvas`. Editing is unaffected either way. |
| `doctor` reports "search index has not been built yet" | The index cache has not been created for this vault yet; it builds on first indexed use. |
| Setup step fails | `just setup` continues past failed steps with a warning; re-run it after fixing the underlying tool. |

## See also

- [APP-FEATURES.md](APP-FEATURES.md) — desktop app guide and shortcuts.
- [CLI-FEATURES.md](CLI-FEATURES.md) — full CLI reference.
- [QueryLanguage.md](QueryLanguage.md) — query expression syntax.

### Source-search platform boundary

The regex helper currently enforces process-group and address-space limits on
Unix. Other platforms explicitly refuse regex execution until their required
resource isolation is implemented; literal Find/Replace remains available.
A bare child process without resource limits is not a supported fallback.

Shortcut names use canonical named keys such as `Enter`, `Escape`, `Delete`
and `ArrowUp/ArrowDown/ArrowLeft/ArrowRight`. Removed spellings
`Return/Esc/Del/Up/Down/Left/Right` must be changed explicitly in overrides.
