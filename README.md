# Flavite

A local-first, desktop Zettelkasten workspace. Write notes in Markdown, link
them, and explore the resulting knowledge graph — all in one movable,
split-pane layout. Notes are plain files; Flavite never replaces your editor
or your vault.

> Status: pre-release / in active development. The workspace shell and editor
> are usable; production packaging, some workflows, and native accessibility
> evidence are still in progress.

## Features

- **Movable tab workspace** — drag, split, merge, maximize, and reopen tabs
  across recursively split tab groups. Layout is saved per vault, device-local.
- **Shared editing** — multiple views of one note share text and Undo history,
  while keeping independent carets, selections, and scroll.
- **Rendered reading** — toggle Edit/Read (Ctrl/Cmd+E) on the same tab;
  CommonMark plus Flavite extensions, with guarded task/property edits.
- **Knowledge graph** — live, filterable view of note links with saved vs.
  unsaved provenance; double-click inserts a link at the remembered caret.
- **Context tools** — Preview, Backlinks, Outline, and Properties follow an
  explicit source; pin them when you want a fixed entity.
- **Safe mutations** — Session-owned writes, revision guards, conflict and
  recovery handling, atomic saves. No silent target rebasing.
- **Local and private** — no network, no accounts, no telemetry. Layouts and
  preferences stay on the device.

See [docs/APP-FEATURES.md](docs/APP-FEATURES.md) for the full feature guide,
including every keyboard shortcut.

## Two binaries, one codebase

| Binary | Crate | What it is |
|---|---|---|
| `flavite` | `crates/flavite-app` | The desktop app (iced/winit GUI). Requires a display server. |
| `flavite-cli` | `crates/flavite-cli` | Headless vault access: open, init, doctor, query, search, rename, info. No GPU or display needed. |

There is also a demo example (`cargo build -p flavite-ui --example app-demo`)
that runs the same UI without CLI parsing; it exists for visual testing, not
daily use.

## Quick start

Prerequisites: Rust toolchain (pinned in `rust-toolchain.toml`; rustup picks
it up automatically) and [`just`](https://github.com/casey/just).

```sh
git clone https://github.com/lexbity/flavite
cd flavite

# Build the desktop app and the CLI
cargo build -p flavite-app -p flavite-cli

# Run the desktop app on a folder of Markdown notes
FLAVITE_VAULT=/path/to/your/vault ./target/debug/flavite
# …or just run it and pick a vault folder on the start screen
./target/debug/flavite

# Headless: initialize a vault, check its health
./target/debug/flavite-cli init --path /path/to/your/vault
./target/debug/flavite-cli doctor --path /path/to/your/vault
```

Flavite never seeds sample content; point it at an existing folder of Markdown
files (or an empty folder — it is adopted as a vault on first open). Full
setup instructions, platform notes, and troubleshooting:
[docs/SETUP.md](docs/SETUP.md).

## Querying

`flavite-cli query` and `flavite-cli search` accept a query expression:

```sh
flavite-cli query "tag:rust AND NOT unresolved" --path /path/to/vault
flavite-cli search "zettelkasten" --path /path/to/vault
```

The expression language (`tag:`, `text:`, `unresolved`, `AND`/`OR`/`NOT`,
quoting, parentheses) is documented in [docs/QueryLanguage.md](docs/QueryLanguage.md).
CLI commands and flags: [docs/CLI-FEATURES.md](docs/CLI-FEATURES.md).

## Data locations

Notes stay wherever your vault folder is. Device-local state lives outside the
vault:

- Workspace layouts / UI preferences: OS data dir under
  `flavite/workspace/<vault-uuid>/` (`workspace-layouts.json`,
  `device-preferences.json`), versioned with last-good backups. Override the
  base directory with `FLAVITE_DATA_DIR`.
- App-scope state (remembered last vault): `flavite/app/app-launch.json`
  under the OS data dir.
- Vault-local user state: `<vault>/.flavite/` — `vault.toml` identity,
  `graph.json` graph pins, `journal/` recovery journal.
- Note saves are atomic (temp sibling file, then replace); recovery uses the
  core journal/safety contracts. Resetting a layout never touches notes.

## Development

- `just setup` — first command after clone: installs components, nightly,
  cargo tools, and the pre-push hook.
- `just gate` — fmt + clippy + tests + arch check (≤ 5 min budget).
- `just ci-gate` — the full CI-level gate (coverage, docs, deny/audit,
  fuzz smoke, network-denied, MSRV).

Visual (Linux) checks require Xvfb and friends:

```sh
just ui-visual-setup
just workspace-visual read-structure
```

### Project layout

- `crates/flavite-ui` — desktop shell, views, host adapters.
- `crates/flavite-workspace` — pure layout/context model (IO-free).
- `crates/flavite-editor` — text editor widget.
- `crates/flavite-api` — facade between UI and core.
- `crates/flavite-vault`, `flavite-index`, `flavite-buffer`, `flavite-syntax`,
  `flavite-graph` — core services.
- `scripts/`, `justfile`, `xtask/` — gates, visual harness, architecture checks.

## License

Apache-2.0. See `LICENSE`.
