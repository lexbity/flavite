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

## See also

- [QueryLanguage.md](QueryLanguage.md) — the query expression grammar.
- [SETUP.md](SETUP.md) — install, first run, environment variables.
