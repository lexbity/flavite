---
title: "flavite-editor - flavite-cli — reference and guide"
weight: 25
type: docs
draft: false
lastmod: 2026-11-04T21:58:24+00:00
---

# flavite-cli — reference and guide

`flavite-cli` is the headless companion to the desktop app: vault access
without a GPU or display. This is the complete command reference, verified
against the current `crates/flavite-cli`.

```
flavite-cli [global flags] <command> [command flags]
```

## Global flags

All global flags work before or after the subcommand.

| Flag | Effect |
|---|---|
| `--vault PATH`, `-v PATH` | Vault to operate on. See resolution order below. |
| `--verbose`, `-x` | DEBUG-level logging instead of INFO. |
| `--no-watch` | Disable the filesystem watcher for this invocation. |
| `--json` | Machine-readable JSON output where supported (also emits JSONL events on stderr). |

Note the short flags: verbose is `-x`, vault is `-v`.

**Vault resolution order:** per-command `--path` → global `--vault` →
current working directory. Unlike the desktop app, the CLI does **not** read
`FLAVITE_VAULT`. The path must exist and be a directory, or the command
fails with "Vault does not exist at … Run 'flavite init' to create one."

## Commands

### `init`

```
flavite-cli init [--path PATH]
```

Create a vault: creates the directory if missing and writes
`<vault>/.flavite/vault.toml` plus the device-local registry entry. The only
way to create a vault from the CLI; the desktop app adopts folders on open.

### `open`

```
flavite-cli open [--path PATH]
```

Open the vault and run a full reconcile in `verify` mode. Fails if reconcile
does not complete successfully. Use this to exercise the same open path the
app uses.

### `doctor`

```
flavite-cli doctor [--path PATH] [--json]
```

Read-only health check (holds identity/instance locks read-only; refuses to
run while the vault is open). Checks: SQLite integrity, tantivy generation
("search index has not been built yet" as a warning when there is no index
cache), broken links, ambiguous links, duplicate IDs, attachments,
resolver parity, change-log health, graph-mirror parity, and observability
(log + crash report presence).

- Human output: one line per check with OK/WARNING/ERROR.
- `--json`: pretty-printed `DoctorReport` (`vault`, `timestamp`, `checks[]`
  with `name`/`description`/`status`/`details`/`fix`, `summary` with
  `ok_count`/`warning_count`/`error_count`/`total`).

**Exit code:** nonzero only when the report contains ERROR-level findings;
warnings are visible but do not fail the command.

### `query`

```
flavite-cli query [QUERY] [--path PATH] [--limit N]
```

Run a query expression against the vault. `QUERY` is required; `--limit`
defaults to **50**. See [QueryLanguage.md](QueryLanguage.md) for the syntax.

- Human output: one result per line (`path` + `title`).
- `--json`: a bare JSON array of note results:

```json
[
  {
    "note_id": 42,
    "path": "notes/zettelkasten.md",
    "title": "Zettelkasten",
    "excerpt": "…bounded excerpt around the first matched term…"
  }
]
```

`excerpt` is built from **saved** content only (empty for structural-only
matches).

### `search`

```
flavite-cli search [TERM] [--path PATH] [--limit N]
```

Identical pipeline to `query` — the term is parsed with the same query
grammar. Differences are only the defaults: `TERM` positional and `--limit`
defaults to **20**.

### `rename`

```
flavite-cli rename FROM TO [--strategy rewrite|alias|none]
```

Rename/move a note (from/to are vault-relative paths), rewriting links per
the strategy — `rewrite` (default), `alias`, or `none`. Journaled, executed
through the core `note.rename` command; the CLI reports skipped items.

- `--json`: `{"strategy": "...", "result": <execution result>}`.

### `info`

```
flavite-cli info [--path PATH] [--json]
```

Vault overview. Keys (both output modes): `vault` (root, UUID), `api_version`,
`note_count`, `epoch`, `generation`, `tantivy_generation`, `watcher`
("enabled"/"disabled"), `drift_found_total`, `autosave.failures_total`,
`rename.items_skipped_total`.

### `api-test`

No arguments. Internal self-test: prints the API version, event API version,
a synthetic vault ref, a request ID, and a sample invocation. Not part of
normal workflows.

## Exit codes

There is no custom exit-code table: commands exit **0** on success and **1**
on any error (Rust's standard `Termination` for `Result`), printing the
error with context. `doctor` is the only command with a graded outcome:
warnings exit 0, errors exit nonzero.

## Events

While a command runs, core events are delivered to **stderr** — as JSONL
(one event per line) with `--json`, or as human-readable `event {type}: …`
lines otherwise.

## Logs and crash reports

- Rotating log: `flavite.log` (5 MiB, rolls to `flavite.log.1`) under the
  OS data dir's diagnostics location; falls back to `$TMPDIR/flavite.log`.
- On panic, `crash-report.txt` is written containing the last 200 log lines.

## Typical workflows

```sh
# Create and validate a vault
flavite-cli init --path ~/notes
flavite-cli doctor --path ~/notes

# Find notes
flavite-cli query "tag:rust AND NOT unresolved" --path ~/notes
flavite-cli search "zettelkasten" --path ~/notes --json

# Vault status
flavite-cli info --path ~/notes

# Move a note and rewrite inbound links
flavite-cli rename old/place.md new/place.md --strategy rewrite --path ~/notes
```

## See also

- [QueryLanguage.md](QueryLanguage.md) — the query expression grammar.
- [SETUP.md](SETUP.md) — install, first run, environment variables.
