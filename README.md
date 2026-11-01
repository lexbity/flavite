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

## Quick start

Prerequisites: Rust toolchain (see `rust-toolchain.toml`), plus `just`.

```sh
git clone https://github.com/lexbity/flavite
cd flavite
cargo build -p flavite-ui --example app-demo
FLAVITE_VAULT=/path/to/your/vault ./target/debug/examples/app-demo
```

The demo binary opens the vault directory named by the `FLAVITE_VAULT`
environment variable (the legacy `LATTICE_VAULT` name is deprecated). It never
seeds sample content; point it at an existing folder of Markdown files to try
it on real notes.

For the full check suite:

```sh
just gate          # fmt + clippy + tests + arch + fault tests
just workspace-gate W09
```

Visual (Linux) checks require Xvfb and friends:

```sh
just ui-visual-setup
just workspace-visual read-structure
```

## Data locations

- Workspace layouts / UI preferences: OS data dir under
  `flavite/vaults/<vault-uuid>/ui/` (versioned, with backups).
- Graph pins: `<vault>/.flavite/graph.json`.
- Recovery uses the core journal/safety contracts; resetting a layout never
  touches notes.

## Project layout

- `crates/flavite-ui` — desktop shell, views, host adapters.
- `crates/flavite-workspace` — pure layout/context model (IO-free).
- `crates/flavite-editor` — text editor widget.
- `crates/flavite-api` — facade between UI and core.
- `crates/flavite-vault`, `flavite-index`, `flavite-buffer`, `flavite-syntax`,
  `flavite-graph` — core services.
- `scripts/`, `justfile`, `xtask/` — gates, visual harness, architecture checks.

## License

Apache-2.0. See `LICENSE`.
