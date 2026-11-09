# Flavite Guide - Draft

Flavite is a local-first desktop app for Zettelkasten-style note-taking. You write plain Markdown files in a folder (the "vault"), and Flavite gives you:

- a movable tab workspace to view/edit notes side by side,
- a knowledge graph of the links between notes,
- full-text search and structural queries,
- safe editing: revision guards, conflict handling, recovery, atomic saves.

Your notes are just files. Flavite never converts them to a proprietary format, never phones home, and never touches a network or account. Everything Flavite remembers about your vault (layout, settings, graph pins, themes) lives *inside* the vault folder under `.flavite/`.

There are two ways to use it:

| Binary | What it is |
|---|---|
| `flavite` | Desktop app. Needs a display server (Wayland/X11). GPU optional. |
| `flavite-cli` | Headless command line: init, doctor, query, search, rename, theme, replace, encoding, info. No display, no GPU. |

**When NOT to use it:** it's pre-release, so expect rough edges (see "Known rough edges"). It is not a cloud-sync tool, not a multi-user system, and not a file browser. If you want online collaboration or a ready-made hosted service, this isn't it.

---

## Prerequisites

- **Rust toolchain** — pinned to `1.98.1` by `rust-toolchain.toml`; rustup installs it automatically on first build. MSRV is 1.90.0.
- **`just`** — command runner for dev recipes (`apt install just` or `cargo install just`).
- **Display + optionally GPU** — only for the desktop app. The CLI needs neither.
- **Linux** — currently supported fully. macOS/Windows support is deferred (CI runs Linux only).

---

## Install and setup

```sh
git clone https://github.com/lexbity/flavite
cd flavite

cargo build -p flavite-app -p flavite-cli -p flavite-text-search-worker
```

This produces three binaries in `target/debug/`: `flavite`, `flavite-cli`, and `flavite-text-search-worker`. Keep the worker in the **same directory** as the other binaries — the app and CLI look for it as a sibling and never search `PATH` (it powers regex find/replace; literal matching works without it).

If you're going to develop Flavite itself, run `just setup` once (installs components, nightly for fuzzing, cargo tools; no git hooks — gating lives in CI). Not needed just to use it.

> Note: building the desktop app needs the usual Rust GUI system libraries (X11/Wayland dev packages, Vulkan or llvmpipe). On Debian/Ubuntu: `sudo apt install build-essential pkg-config libxkbcommon-dev libssl-dev` and for the graph view `mesa-vulkan-drivers` (or use the canvas backend, below).

---

## Quickstart (under 5 minutes)

The CLI is the fastest way to see the system work. Everything here produces visible output.

### 1. Create a vault

```sh
mkdir -p ~/notes
flavite-cli init --path ~/notes
```

Expected output: nothing on stdout (exit 0). It creates `~/notes/.flavite/vault.toml` (the vault's identity) and registers it on this device.

### 2. Drop in some notes

```sh
cd ~/notes
mkdir -p notes
cat > notes/zet.md <<'EOF'
# Zettelkasten

Some thoughts about #second-brain and [[notes/foo]].
EOF
cat > notes/foo.md <<'EOF'
# Foo

A note with a [[notes/zet]] link.
EOF
```

### 3. Open the vault (builds the index)

```sh
flavite-cli open --path ~/notes
```

This runs a full reconcile in verify mode. Exit 0 means it completed.

### 4. Check its health

```sh
flavite-cli doctor --path ~/notes
```

You'll see a report with `✓`/`⚠` lines. **Warnings are fine** (e.g. "search index has not been built yet" before first search). Exit code is 0 unless there are ERROR-level findings.

### 5. Query and search

```sh
flavite-cli query "tag:second-brain" --path ~/notes
flavite-cli search "zettelkasten" --path ~/notes
```

Expected:

```
notes/zet.md	Zettelkasten
```

(one line per result: `path<TAB>title`).

---

## Common tasks

### Find notes with the structural query

```sh
flavite-cli query "tag:rust AND NOT unresolved" --path ~/notes
flavite-cli query "tag:project" --limit 100 --path ~/notes
```

`query` is structural only: `tag:…`, `unresolved`, and `AND`/`OR`/`NOT` with parentheses. Text terms here are rejected explicitly — use `search` for text.

### Search note text

```sh
flavite-cli search "rocket science" --path ~/notes        # whitespace = AND
flavite-cli search "title:rocket AND moon" --mode expression --path ~/notes
flavite-cli search "second brain" --field body --path ~/notes
flavite-cli search "TODO" --mode regex --path ~/notes     # needs the worker binary
```

Add `--json` for machine-readable output (pages report `total` and a `next` token; use `--all` to consume every page).

### Rename a note and rewrite its links

```sh
flavite-cli rename notes/foo.md notes/bar.md --strategy rewrite --vault ~/notes
```

`rewrite` (default) updates inbound links in other notes via a journaled transaction; `alias` adds the old stem as an alias and leaves text unchanged; `none` renames only and reports broken links.

### Durable find-and-replace

The `replace` command family makes a *sealed preview*, records a selection, then applies it as a durable job. One-shot version:

```sh
flavite-cli replace --path ~/notes apply \
  --key aabbccddaabbccddaabbccddaabbccdd \
  --query "second-brain" --replacement "second-brain v2" --mode literal
```

`--key` is your caller-generated 128-bit hex idempotency key (32 hex chars). Output includes the `job_id` and phase/totals, e.g.:

```json
{"job_id":"...","status":{"phase":"completed","totals":{"saved":1,"selected":1,"...":"..."}}}
```

> **Note on the multi-step flow:** preview → select → apply is the desktop app's UI flow, where one session stays alive the whole time. The CLI runs one command per process and the preview ledger is session memory — so a preview created in one `flavite-cli` invocation is gone in the next (you'll get `preview-expired`). For the CLI, always use the inline form above. Also, a given plan is applied once: re-running the same query+replacement errors with `file already exists` in `.flavite/operations/replace/` — use `replace revert` or a different query/scope instead.

### Run the desktop app

```sh
FLAVITE_VAULT=~/notes ./target/debug/flavite
```

Or run it with no arguments and use the start screen to pick a folder. If the graph view is blank or errors, fall back to the software renderer:

```sh
FLAVITE_GRAPH_BACKEND=canvas ./target/debug/flavite
```

The app is a normal window: tabs, split panes, Edit/Read toggle (Mod+E), a search panel, and the graph. See `docs/APP-FEATURES.md` for the full shortcut list.

---

## How it works

Think of a vault as **the source of truth** and everything else as a cache with a paper trail.

- Your notes are the ground truth: plain Markdown files.
- `<vault>/.flavite/` is the vault's "memory": identity (`vault.toml`), settings (`settings.toml`), layout, graph pins, installed themes, a recovery journal, and durable operation records. It travels with the vault folder.
- The **index** (SQLite + a full-text engine) lives in the OS cache directory, keyed by the vault's UUID. It is **disposable**: if it's missing, damaged, or from an older schema, it's rebuilt from your notes. That's why `doctor` warns "index cache has not been created" — it just hasn't been built yet.
- All edits flow through one path: UI/CLI → typed **command** → **session** (which owns buffers, revisions, undo, and saves) → **journal** (a durable transaction record) → disk. Writes are atomic (temp sibling file, then replace). This is why a crash mid-rename doesn't corrupt notes: the journal rolls forward on next open.

The query language is two dialects: `query` is structural (tags, unresolved links, boolean combos) and runs as SQL against the index; `search` is text (full-text or regex over source) and materializes a *frozen snapshot* per search session so paging is stable even as you edit.



## Workspace shell

The window is a top bar, a pane grid, and a status bar. Notifications float
over the workspace and take no layout space.

- **Top bar** — a "Flavite" button (app menu), the vault name, standard menus
  (File, Edit, Selection, View, Go, Graph, Tools, Help), plus **Quick switch**
  and **Commands** buttons.
- **Pane grid** — recursively split tab groups with proportional splitters.
- **Status bar chips** — current writing target, focused note load state,
  "N in flight" pending operations, older-outcome overflow, and
  "Indexing done/total" reconcile progress.

Every menu item, palette entry, and keyboard shortcut resolves through one
command catalog, so all three stay in sync.

## Tabs and splitting

- Tab kinds: Notes, Search, History, Note, Graph, Preview, Backlinks,
  Outline, Properties (plus an Empty state with hints).
- **Drag a tab** to move it; drop onto the left/right/top/bottom edge of a
  group to split. Press Escape mid-drag to cancel without changes.
- **Maximize** a group (Mod+Shift+M) to show it alone with a group switcher;
  the underlying layout tree and saved ratios are untouched.
- Reopen closed tab, duplicate tab, cycle tabs/groups, move tab between
  groups, merge/close groups, save/reset layout — all in the menus and
  palette.

Multiple tabs can show the **same note**: they share text and Undo history
while keeping independent carets, selections, and scroll positions.

## Editing notes

- Each note tab has a per-tab toolbar with the **Edit/Read toggle** (Mod+E)
  and a footer showing buffer state: Saved / Unsaved changes / Conflict /
  Missing on disk.
- **Find within note** (Mod+F): query field, match counter, previous/next;
  editor input is disabled while find is focused. Find defaults to literal,
  case-insensitive matching; the Regex control enables Rust-regex syntax and
  capture replacements (`$0`, `$1`, `${name}`, `$$`). Whole-word and frozen
  selection-scope controls apply to either mode. Match pages continue across
  the full note instead of stopping at the first page.
- **Replace** expands the Find bar with Replace next and Replace all. A
  replacement is prepared against the current buffer revision and becomes one
  Undo step. Regex work requires the `flavite-text-search-worker` executable
  beside `flavite`/`flavite-cli`; if it is missing or incompatible, Regex shows
  an actionable availability error while literal Find remains available.
- Links: clicking a wikilink opens the target, creating the note if it does
  not exist (pre-filled from the link).
- Saving: Mod+S saves; saves are atomic (temp file + replace) through the
  core session, guarded by revisions.

## Context Mode

Enter from the note header's **Context Mode** button, right-click inside the
source editor, or use **Ctrl+Space** (macOS: **Ctrl+Shift+Space**). The toggle
is remappable in Preferences. Right-click keeps a range when clicked inside
it; otherwise it selects the Unicode word under the pointer.

A horizontal toolbar replaces the note header. Its leading **×** always exits,
including while its menu or command palette is open. Double-click the toolbar
caption or editor footer to exit. Changing tabs or switching to Read exits;
window focus loss retains the mode. Exiting preserves the selection.

- **COMMAND**: physical letter keys act on the selection. **I/A** type before/
  after it; Shift+I/A type at the line boundaries; O/Shift+O open a line below/
  above; C changes; D deletes; Shift+D cuts; Y copies; P pastes; U/Shift+U undo/
  redo. With an empty selection, Copy/Cut select the current line, and Delete/
  Change use one grapheme on that line. An empty line is not silently joined.
- **TYPING**: text input, IME, and ordinary navigation remain available.
  Repositioning keeps typing active. **Escape** returns to commands; it never
  exits Context Mode. An open menu consumes Escape first.
- **Select / Go / Type** menus expose word/line/all selection, clearing a
  selection, movement, and typing entries. H/J/K/L move; W/B move by Unicode
  word; Shift extends. The menu hints identify physical key positions.
- **Cut/Copy/Paste, Undo/Redo, Commands, Find and Replace** use the existing
  clipboard, shared buffer history, palette and in-note search. Text fields in
  those overlays own typing. Narrow panes move actions into **More** while
  keeping Exit visible. Below 320 logical pixels, editing pauses and an
  **Expand pane** control appears; the mode and selection are retained.
- The detail row shows recent **editing-history chips**, marks the current
  state, and offers a redo chip after moving backward. Click a chip to restore
  its text and recorded selection. **Editing history** in More or the command
  palette opens a paged chooser, including other branches. Restoring moves the
  existing history head; it preserves branches and leaves saved disk bytes
  alone. Escape closes the chooser first, and Exit remains reachable above it.
  States across a rename transaction require ordinary Undo/Redo.

Context edits are revision guarded. A delayed paste is refused after its source
or selection changes or the mode exits. A temporary typing run is one Undo unit
until a selection/motion, toolbar command, Escape, save, foreign edit, or exit
ends the group; an idle pause alone does not split it. Different editor views
cannot merge their typing runs. Mode state is transient and is not restored on
restart. This interface currently uses the editor's single selection; multiple
selections remain further specification work. Editing history is in memory and
is distinct from the saved note snapshots in the History view.

## Reading mode

Mod+E on the same tab renders CommonMark plus Flavite extensions:

- Proportional prose, nested blocks, images, and `![[...]]` embeds (nested
  note embeds render recursively, with an unsaved-content marker when
  applicable).
- **Task checkboxes** are real actions (core `task.toggle`).
- **Properties** can be edited conservatively from the rendered view
  (core `property.set`) — these two are the only edits permitted from the
  reading surface.
- External links open in the system browser.

## Context tools

Preview, Backlinks, Outline, and Properties are first-class tabs with a
context header that always states their source ("Following: <note>",
"Following: Selection", or unbound):

- **Preview** — rendered or raw-source view; shows the exact open-buffer
  revision (including unsaved text) or saved text with index provenance.
- **Backlinks** — inbound links, each row with an Open action.
- **Outline** — heading list; click to navigate regardless of keyboard focus.
- **Properties** — frontmatter inspection with typed scalar edits and an
  "Edit source" jump.

Tools **follow** their source by identity — not widget position — and can be
**pinned** to a fixed note (context.pin command) so they stop following.

## Finding notes and content

- **Quick switcher** (Mod+P): fuzzy open over the vault catalog. Enter opens;
  Ctrl+Enter inserts a link at the caret. The footer shows index provenance
  (epoch/generation) and truncation notice at the 50k catalog limit.
- **Search view**: saved-content search with 120 ms debounce, 50-row pages
  with explicit paging, and a persistent "results reflect saved content
  only" label. Unsaved edits are never searched.
- **Notes view**: the vault note list with folder grouping and an in-memory
  filter.
- Query syntax for the search view is the same expression language as the
  CLI: whitespace-separated terms AND together; structured terms (`tag:`,
  …) parse individually. See [QueryLanguage.md](QueryLanguage.md).

## Note history

Snapshots are content-addressed (Blake3) with timestamp, reason, and pins.
The History view lists them and offers a guarded, undoable restore through
core commands.

## Knowledge graph

The graph view is a first-class workspace tab over the note-link network:

- **Two renderers**, GPU (default) and software canvas, showing identical
  geometry. Backend selection: [SETUP.md → Graph backend](SETUP.md#graph-backend).
- **Camera**: pan and cursor-anchored wheel zoom (clamped); the camera
  follows auto-layout until you take control with a pan/zoom.
- **Click semantics** (amended by RFC-0006 D-02): single click selects and
  shows the node's actions in a reserved overlay band — the canvas never
  shifts for selecting, deselecting, or inspecting occurrences;
  double-click on the same node or Enter **opens** it through the shared
  destination policy (a visible matching view is reused, otherwise a new
  tab lands in the last-used document group and the graph stays visible
  when geometry permits); Ctrl+Enter or Insert is the only link-insertion
  path, always at the acknowledged caret; background click clears
  selection; node drags move spatially and never open or insert;
  multi-selected notes offer Open-in-tabs (max 20 per invocation, native
  key order).
- **Node drag** with a drag preview; **pins** are vault user state, shared
  across graph views, persisted in `<vault>/.flavite/graph.json`.
- **Filters**: neighborhood depth, direction, attachments, unresolved,
  embeds, match-any, negate — with Apply / Clear / Undo.
- **List mode** (`graph.list`): the same graph as keyboard-navigable rows,
  with the same Open/Insert intents as the pointer, truncated with a
  notice past 50 rows.
- **Non-note targets**: activating an attachment node drives the existing
  local attachment inspector (core-validated open; an unsupported file
  types its refusal); an unresolved link offers explicit Create (the
  prefilled new-note dialog) or Locate (the prefilled picker) — navigation
  never creates a note.
- **Provenance**: the header shows base counts and the writing-target line
  before any explicit insertion; the footer labels a pending frame
  "Updating" (navigation survives the gesture hold; insertion waits for
  the paired frame), reports staleness ("Frame stale — insertion
  disabled") and scope problems.
- **Link removal**: inspect an occurrence, choose, open, prepare, confirm —
  always showing source→target and Saved/Unsaved provenance.

## Safety model

- **Session-owned writes**: every mutation goes through the core API with
  revision guards — no GUI-side file mutation, no silent rebasing.
- **Close policy**: `note.close` refuses to drop a pending save/unsaved
  buffer unless a durable recovery snapshot provably covers it (SaveClose,
  RetainRecovery, or explicit revision-bound Discard).
- **Conflicts**: external file changes surface a conflict dialog; three-way
  merges are core-prepared with explicit confirmation and compare hunks.
- **Recovery**: retained unsaved-change snapshots are offered at startup;
  restore/discard are explicit core commands.
- **Rename/move**: dialog with strategy preview; externally detected renames
  carry confidence tiers (T0–T2 authoritative, T3–T4 uncertain); executed as
  the journaled `note.rename` command.

## Autosave and persistence

- Workspace layout autosave: 500 ms debounce, max 2 s dirty age, generation
  receipts, capped exponential backoff on failure. Persisted in the vault at
  `<vault>/.flavite/workspace-layouts.json`, with last-good `.bak` backups.
- Failures are surfaced as typed states (Unavailable / GenerationMismatch /
  Failed) — never silent loss.

## Preferences

Mod+, opens Preferences: theme (Light/Dark/System), interface scale, editor
font size, line width, reduced motion, and per-command **shortcut overrides**
(capture the next keypress; collisions are caught; restore defaults per
command). Preferences persist in the vault at
`<vault>/.flavite/settings.toml`.

## Keyboard shortcuts (defaults)

Remappable in Preferences.

| Keys | Action |
|---|---|
| Mod+S | Save note |
| Mod+E | Toggle Edit/Read |
| Mod+F | Find in current note |
| Mod+H | Open Find/Replace in current note |
| Mod+P | Quick switch (open note) |
| Mod+Shift+P | Command palette |
| Mod+N | New note… |
| Mod+O | Open note… |
| Mod+T / Mod+Shift+T | New tab / Reopen closed tab |
| Mod+W | Close tab |
| Mod+B | Toggle the notes sidebar |
| Mod+Tab / Mod+Shift+Tab | Next / previous tab |
| F6 / Shift+F6 | Next / previous group |
| Mod+Alt+Right / Mod+Alt+Down | Split group right / down |
| Mod+Alt+Shift+Right / Left | Move tab to next / previous group |
| Mod+Alt+Shift+Arrows | Resize the adjacent split (2.5% per press) |
| Mod+Shift+M | Maximize or restore group |
| Alt+Left / Alt+Shift+Right | Navigate back / forward |
| Shift+F10 | Group actions menu |
| Mod+, | Preferences |
| Enter / Mod+Enter (graph, with selection) | Open node / insert link at caret |
| Ctrl+Enter (quick switcher) | Insert link at caret |
| Enter / Escape (dialogs) | Primary action / cancel |
| ArrowUp/Down + Enter (menus, palette, picker) | Navigate + run |

Editor keys: standard select-all/copy/cut/paste, Undo (Mod+Z) / Redo
(Mod+Shift+Z or Mod+Y), line/document Home/End, grapheme Left/Right,
wrapped-row Up/Down, Page Up/Down, Backspace/Delete, Enter. Vertical movement
retains the horizontal position through short and empty lines. Ctrl+Left/Right
and Ctrl+Backspace/Delete move/delete by Unicode word (Option on macOS).
Shift extends keyboard selections; Shift-click extends from the existing
anchor. Double-click selects a word and triple-click selects a source line;
dragging afterward extends by the selected unit. IME composition is supported
(Edit/Read toggle is disabled while composing).



