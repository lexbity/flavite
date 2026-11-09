# flavite-cli - draft

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

Run a **structural** query expression against the vault. `QUERY` is
required; `--limit` defaults to **50**. See
[QueryLanguage.md](QueryLanguage.md) for the syntax. Text predicates are
rejected explicitly (use `search`); supported structural terms are
`tag:…`, `unresolved`, and their `AND`/`OR`/`NOT` combinations.

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
flavite-cli search [TERM] [--path PATH] [--mode text|expression|literal|regex]
                   [--field title|aliases|tags|body|all] [--limit N] [--all] [--json]
```

Saved-content search through a materialized search session (RFC-0006 S06).
Plain text is the default mode: whitespace means `AND` and punctuation is
text. `--mode expression` parses the explicit operator syntax (`AND`/`OR`
`NOT`, quotes as phrases, parentheses) — it is **not** whitespace-split.
`--field` restricts every text term to one indexed field; in expression mode
field filters are written directly (`title:rocket`). `--limit` is the page
size (1–200, default **50**); each JSON page reports `session`, the frozen
`epoch`/`generation` snapshot, the exact `total`, the `rows`, and the
`next` ordinal token. `--all` consumes every continuation token before
printing; the session is explicitly closed afterwards.

Machine-readable failures are one deterministic JSON object on stderr:
`{"error": {"kind": "busy|invalid|limit-exceeded|reset-required|expired|denied|failed", "message": …}}`.

`literal`/`regex` modes are **vault source matching** (RFC-0006 S07): they
scan saved Markdown source and report per-note match pages — `snapshot`,
`total_notes`, `total_matches`, `truncated_scope`, `exclusions`, and a
`page` with rows (`path`, `note_matches`) plus `next_note_index` /
`next_match_offset` continuation ordinals; `--all` consumes every
continuation. Human output prints one `path<TAB>count` per note. Literal
matching runs in-process; regex runs through the
`flavite-text-search-worker` helper — without it (or with an invalid
pattern) the command fails with a typed message (`regex is unavailable` /
`invalid regular expression`), never a silent text search. `--field`
applies to `text`/`expression` modes only; `--limit` pages source-find per
note. `--json` prints each page as one JSON object on stdout.

### `query` (text predicates)

`flavite-cli query` remains the structural-only endpoint: a query AST that
contains any text predicate fails with
`text predicates are not supported on the structural query endpoint; use
the search session endpoint` — never an empty success.

### `rename`

```
flavite-cli rename FROM TO [--strategy rewrite|alias|none]
```

Rename/move a note (from/to are vault-relative paths), rewriting links per
the strategy — `rewrite` (default), `alias`, or `none`. Journaled, executed
through the core `note.rename` command; the CLI reports skipped items.
`rename` has no per-command `--path`; use the global `--vault PATH`.

- `--json`: `{"strategy": "...", "result": <execution result>}`.

### `theme`

```
flavite-cli theme <command> [flags]
```

Convert browser theme packages (Firefox-style `.xpi`) and manage the
installed themes in this vault's theme store (`<vault>/.flavite/themes/`):

- `theme convert INPUT [-o OUTPUT]` — offline conversion into a
  `.flavite.xpi` (default output: `<input>.flavite.xpi` beside the input).
  No vault is needed. Human output prints the theme name, id, output path,
  and mapped/dropped key counts; `--json` prints `{"output": …,
  "report": …}`.
- `theme install PATH` — convert and install a standard `.xpi` into the
  vault's theme store.
- `theme list` — list installed themes (`id — name` per line, or
  "No themes installed.").
- `theme select ID` — select an installed theme (persisted as
  `appearance.theme_package` in `<vault>/.flavite/settings.toml`); use
  `theme select none` for the built-in palettes.
- `theme uninstall ID` — remove an installed theme.

`install`/`list`/`select`/`uninstall` require an opened vault.

### `replace`

```
flavite-cli replace <command> [flags]
```

Durable vault replacement operations (RFC-0006 S08/S09): seal a preview,
record a selection, apply, track job status, cancel, resume, or revert.
All actions require an opened vault (`--path` or global `--vault`) and take
an idempotency `--key` (caller-generated, 128-bit hex) where noted. `--json`
prints the typed result object; human output prints a status summary for
`status`.

- `replace preview QUERY REPLACEMENT [--mode literal|regex] [--case-sensitive]
  [--whole-word] [--include-drafts]` — seal a read-only preview; prints the
  preview id. `--include-drafts` admits open unsaved drafts as candidates.
- `replace select --preview-id HEX [--select all|none|keys=a,b,c]` — record
  a frozen selection revision over a preview. `keys=` takes hex native keys.
- `replace apply --key HEX (--preview-id HEX --selection-revision N | --query
  QUERY --replacement TEXT [--mode literal|regex])` — apply a sealed preview
  under a frozen selection revision; with `--query`/`--replacement` instead,
  a preview is sealed and fully selected inline in one process. Returns the
  job id and phase/totals.
- `replace status --job-id HEX` — report job phase and totals (saved,
  conflicts, stale, needs-recovery).
- `replace cancel --job-id HEX` — stop admission of the next item.
- `replace resume --job-id HEX --key HEX` — explicitly resume an
  untouched/paused job.
- `replace revert --job-id HEX --key HEX` — start a guarded Revert epoch
  (RFC-0006 S09): restore the sealed before-payloads of applied items under
  after-hash/lineage preconditions.

### `encoding`

```
flavite-cli encoding NOTE <command> [flags]
```

Inspect or convert a single note's file encoding (RFC-0006 §6.10). `NOTE`
is vault-relative. Requires an opened vault.

- `encoding NOTE reopen [--encoding auto|ascii|utf8|utf16le|utf16be]
  [--bom auto|present|absent]` — reopen the note under an explicit encoding
  and report the resulting format; `auto` opens without an explicit family
  (BOM/hint rules). Never discards unsaved text.
- `encoding NOTE convert --encoding FAMILY [--bom absent|present|auto]` —
  convert the note's save format, then save durably through the core command
  (never a CLI file write). Requires an explicit family; `--bom` defaults to
  `absent`.

- `--json` output: `{"note": …, "encoding": …, "bom": …}` for `reopen`;
  `{"note": …, "encoding": …, "bom": …, "converted": true, "saved": true}`
  for `convert`.

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
flavite-cli rename old/place.md new/place.md --strategy rewrite --vault ~/notes
```

## Flavite Environment Variables:

| Variable | Read by | Effect |
|---|---|---|
| `FLAVITE_VAULT` | `flavite-app` (`clap` env + `host/env.rs`) | Default vault folder for the desktop app. Overridden by `--vault PATH`. Unset → start screen. The CLI does **not** read it. |
| `FLAVITE_GRAPH_BACKEND` | `flavite-ui/src/host/graph.rs` | `gpu` (default) or `canvas`. Selects the graph renderer. |
| `FLAVITE_DATA_DIR` | none (tests/scripts only) | Documented as "replace the OS data dir". Not honored by production code in this checkout. |
| `FLAVITE_HARNESS_OUT_FD` | `flavite-ui/src/harness.rs` | fd number for harness JSONL output (test harnesses only; compiled out of production). |
| `FLAVITE_HARNESS_AUTOSAVE_IDLE_SECS` | `flavite-ui/src/app.rs` | Overrides autosave idle interval (harness). |
| `FLAVITE_HARNESS_INJECT_HIDE_TAB` | `flavite-ui/src/harness.rs` | Harness UI fault injection. |
| `FLAVITE_SERIAL_SCAN` / `FLAVITE_SCAN_TIMING` | `flavite-vault` | Test/bench hooks: force serial scan; log scan timing. |
| `FLAVITE_RENAME_KILL_ROOT`, `FLAVITE_W12_CRASH_ROOT`, `FLAVITE_TANTIVY_KILL_CHILD_ROOT`, `FLAVITE_RECOVERY_KILL_CHILD_ROOT`, `FLAVITE_TANTIVY_READY`, `FLAVITE_RECOVERY_READY_MARKER`, `FLAVITE_DEMO_THEMES`, `FLAVITE_DEBUG_RENAME` | crash-simulation tests | Destructive kill/ready hooks. Do not set outside test scenarios. |
| `FLAVITE_REQUIRE_WGPU_ADAPTER` | `flavite-ui/tests/shader_validation.rs` | CI: make skipped GPU tests fail. |
| `LLVM_PROFILE_FILE` | `flavite-graph/src/model.rs` | Coverage runs only. |

## CLI Flag reference:

Global flags (before or after the subcommand):

| Flag | Short | Type | Default | Meaning |
|---|---|---|---|---|
| `--vault PATH` | `-v` | path | CWD | Vault root. Resolution order: command `--path` → `--vault` → current directory. Not `FLAVITE_VAULT`. |
| `--verbose` | `-x` | flag | off | DEBUG logging instead of INFO. |
| `--no-watch` | — | flag | off | Disable the filesystem watcher for this invocation. |
| `--json` | — | flag | off | Machine-readable JSON output; events as JSONL on stderr. |

Commands (all exit 0 on success, 1 on error; `doctor` is graded — see below):

| Command | Signature | Notes |
|---|---|---|
| `init` | `[--path PATH]` | Creates the directory if missing, writes `<vault>/.flavite/vault.toml` (format: `[vault]\nuuid = "<uuid>"`) + device-local registry entry. |
| `open` | `[--path PATH]` | Open + full reconcile in `verify` mode. Fails if reconcile fails. |
| `doctor` | `[--path PATH] [--json]` | Read-only health checks (below). Exit nonzero **only on ERROR-level findings**; warnings exit 0. Refuses while the vault is open. |
| `query` | `QUERY [--path PATH] [--limit N]` | Structural query (no text predicates — those fail explicitly). Default limit **50**. Human output `path<TAB>title`; `--json` = bare array of `{note_id, path, title, excerpt}`. |
| `search` | `TERM [--path PATH] [--mode text\|expression\|literal\|regex] [--field title\|aliases\|tags\|body\|all] [--limit N] [--all] [--json]` | Saved-content search. Default mode `text` (whitespace = AND, punctuation is text). `--mode expression` parses `AND`/`OR`/`NOT`, quotes, parentheses (no implicit AND). Page size 1–200, default **50**. `--all` consumes every continuation token. `literal`/`regex` = vault source matching (regex via the worker binary; typed refusal "regex is unavailable" / "invalid regular expression" otherwise). |
| `rename` | `FROM TO [--strategy rewrite\|alias\|none]` | Vault-relative paths. Default `rewrite`. No per-command `--path`; use global `--vault`. |
| `theme convert` | `INPUT [-o OUTPUT]` | Offline `.xpi` → `.flavite.xpi` (default output `<input>.flavite.xpi`). No vault needed. |
| `theme install/list/select/uninstall` | `theme install PATH` / `theme list` / `theme select ID\|none` / `theme uninstall ID` | Need an opened vault. `select none` = built-in palettes. |
| `replace` | subcommands below | Durable replacement jobs. All need an opened vault. `--key` is a caller-generated 128-bit hex idempotency key (32 hex chars). |
| `replace preview` | `QUERY REPLACEMENT [--mode literal\|regex] [--case-sensitive] [--whole-word] [--include-drafts]` | Seals a read-only preview; prints the preview id. The preview ledger is **session memory**: for the CLI (one command per process) the usable path is `apply --query --replacement` inline. |
| `replace select` | `--preview-id HEX [--select all\|none\|keys=a,b,c]` | Records a frozen selection revision. |
| `replace apply` | `--key HEX (--preview-id HEX --selection-revision N \| --query QUERY --replacement TEXT [--mode …])` | Applies a sealed preview. Inline `--query`/`--replacement` seals+selects-all in one process. Returns job id + phase/totals. |
| `replace status` | `--job-id HEX` | Reports phase + totals (saved, conflicts, stale, needs-recovery). |
| `replace cancel` | `--job-id HEX` | Stops admission of the next item. |
| `replace resume` | `--job-id HEX --key HEX` | Resumes an untouched/paused job. |
| `replace revert` | `--job-id HEX --key HEX` | Guarded revert epoch: restores sealed before-payloads of applied items. |
| `encoding reopen` | `NOTE [--encoding auto\|ascii\|utf8\|utf16le\|utf16be] [--bom auto\|present\|absent]` | Reopen under an explicit encoding; reports resulting format. Never discards unsaved text. |
| `encoding convert` | `NOTE --encoding FAMILY [--bom absent\|present\|auto]` | Converts save format, then saves durably through the core command. `--bom` defaults to `absent`. |
| `info` | `[--path PATH] [--json]` | Vault overview: `vault`, `api_version`, `note_count`, `epoch`, `generation`, `tantivy_generation`, `watcher`, `drift_found_total`, `autosave.failures_total`, `rename.items_skipped_total`. |
| `api-test` | no args | Internal self-test; not part of normal workflows. |

Events: commands emit events on **stderr** — JSONL with `--json`, `event {type}: …` otherwise. Notices must be acknowledged (`acknowledge_notice`) after display.

Logs: rotating `flavite.log` (5 MiB → `flavite.log.1`) under the OS data dir's `flavite/logs` (falls back to `$TMPDIR/flavite.log`). Panics write `crash-report.txt` with the last 200 log lines.
