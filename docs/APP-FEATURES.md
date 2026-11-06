# Flavite app — features and guide

The desktop application (binary `flavite`, crate `flavite-app`): a movable,
split-pane workspace for a vault of Markdown notes. This guide covers what
each feature does and how to drive it. Setup: [SETUP.md](SETUP.md). Headless
equivalents: [CLI-FEATURES.md](CLI-FEATURES.md).

Key notation: **Mod** = Ctrl on Linux/Windows, Cmd on macOS.

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
- **Click semantics**: single click selects; double-click on the same node
  opens it — or inserts a link to it at the remembered caret, per the
  writing target; background click clears selection.
- **Node drag** with a drag preview; **pins** are vault user state, shared
  across graph views, persisted in `<vault>/.flavite/graph.json`.
- **Filters**: neighborhood depth, direction, attachments, unresolved,
  embeds, match-any, negate — with Apply / Clear / Undo.
- **List mode** (`graph.list`): the same graph as keyboard-navigable rows,
  truncated with a notice past 50 rows.
- **Provenance**: the header shows base counts and the writing-target line
  before any double-click insertion; the footer reports staleness
  ("Frame stale — insertion disabled") and scope problems.
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
  receipts, capped exponential backoff on failure. Persisted per vault,
  device-local, with last-good `.bak` backups.
- Failures are surfaced as typed states (Unavailable / GenerationMismatch /
  Failed) — never silent loss.

## Preferences

Mod+, opens Preferences: theme (Light/Dark/System), interface scale, editor
font size, line width, reduced motion, and per-command **shortcut overrides**
(capture the next keypress; collisions are caught; restore defaults per
command).

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
(Mod+Shift+Z or Mod+Y), line/document Home/End, arrow movement, Backspace/
Delete, Enter. IME composition is supported (Edit/Read toggle is disabled
while composing).

## Data locations

See [SETUP.md → Where state lives](SETUP.md#where-state-lives). Summary:
notes stay in your vault; layouts/preferences and the remembered vault live
device-locally under the OS data dir; identity, graph pins, and the recovery
journal live in `<vault>/.flavite/`.

## Current limitations

Known gaps, tracked in `devdocs/verification/KNOWN_MANUAL_UX_GAPS.md`:

- Right-click context menus are not implemented yet (deferred).
- Drag ghost / empty-space drop feedback for split gestures is still open;
  the underlying split commands work.
- No vault file browser (folder-tree notes list) yet; the Notes view and
  quick switcher are the navigation surfaces.
- No note lifecycle UI (create/rename/delete) beyond New note… and the
  rename dialog.
- Editor selection feel, link-click pointer targeting, graph auto-fit
  behavior, and narrow-window layout still need manual verification on real
  hardware.
