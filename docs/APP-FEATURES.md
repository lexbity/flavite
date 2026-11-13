# Flavite Features

This page describes the delivered editor capabilities of the Flavite desktop
app, in the shape of the finite command catalogue and the limits that bound
it. Everything here is a shipped surface: a listed command has production
behavior, availability, error handling, and a test (FEUX-0002 D-1).

---

## Editing model

Flavite is a **selection-first** editor: every edit operation acts on a
canonical *selection set* — an ordered, non-overlapping set of directed
ranges (each with an anchor and a head) plus one distinguished **primary**
member. Selections are:

- **Sorted** in document order and **non-overlapping** (adjacent ranges are
  kept separate; touching carets are absorbed into a range).
- **Grapheme-aligned**: endpoints always fall on extended-grapheme
  boundaries, even for a text that mixes combining marks, ZWJ sequences,
  or right-to-left scripts.
- **Bounded**: at most **10,000 members** per view. A command whose result
  would exceed the cap is refused whole, *before* any change, with the
  observed count (for example, `Would select 14,203 matches; the limit is
  10,000`).
- **Never empty**: a view always has at least one caret.

Editing happens in **atomic batches**: one command across N members is one
editor operation — one undo node, one notification, one selection-version
bump. Every mutation crosses the Session single-writer path; there is no
second buffer, no second undo tree, and no second matching engine.

---

## Context Mode

Context Mode is an optional selection-first command layer attached to one
editor view. Its horizontal toolbar sits inside the note pane, immediately
above the editing surface, with a permanently reachable **✕ Exit Context
Mode** control (highest priority, always visible even under overflow).

| Action | Keys (Linux/Windows) | Keys (macOS) |
|---|---|---|
| Enter / exit Context Mode | `Ctrl+Space` | `Ctrl+Shift+Space` |
| Exit Context Mode (toolbar / Esc behavior) | ✕ Exit control; `Esc` keeps primary | same |

While Context Mode is active, physical keys are interpreted as commands;
`Ctrl`/`Alt`/`Logo` chords never enter the grammar. The mode state is shown
in the toolbar badge; the detail row shows selection counts and messages.

### Command keys

| Key | Command | Effect |
|---|---|---|
| `i` / `Shift+I` | `editor.insert_start` / `editor.insert_line_start` | insert at each caret / at each line start (typing group) |
| `a` / `Shift+A` | `editor.insert_end` / `editor.insert_line_end` | insert after each caret / at each line end (typing group) |
| `o` / `Shift+O` | `editor.open_below` / `editor.open_above` | open a new line below/above each member (typing group) |
| `c` | `editor.change` | delete each member's range and enter typing |
| `d` / `Shift+D` | `editor.delete` / `editor.cut` | delete each member / cut each member (copy joins with LF, then delete) |
| `y` | `editor.copy` | copy each member; fragments join with LF in document order |
| `p` | `editor.paste` | paste the single clipboard string at every member (broadcast) |
| `u` / `Shift+U` | `buffer.undo` / `buffer.redo` | shared-buffer undo/redo |
| `x` | `editor.select_line` | select the containing line of each member |
| `;` / `Shift+;` | `editor.collapse` / `workspace.command_palette` | collapse every range to its head / open the command palette |
| `/` / `Shift+/` | `workspace.find` / `workspace.replace` | owned find / replace bar |
| `f` / `Shift+F` | `editor.find_char` | **find character** forward / backward (see below) |
| `Escape` | `editor.context_back` | pop menus; collapse to primary; exit with hint |

### Motions

`h j k l` move left / down / up / right; `w b e` move to next word,
previous word, next word end (per member, with per-member preferred column).
Hold `Shift` to extend. Line and document motions live in the **Go** menu
(`g`): `h` line start, `e` line end, `t` document start, `b` document end.
The **Select** menu (`v`) has `w` select word, `x` select line, `a` select
all.

### Find character (`f` / `Shift+F`)

`f` (forward) or `Shift+F` (backward) enters a **"Waiting for character…"**
capture state. The next event carrying a produced character resolves the
command; the caret moves to the next occurrence of that character after
each member's head (extend with `Shift` held at resolution). Dead keys and
modifier-only events are ignored (never committed); `Escape` cancels back
to commands; any mode exit cancels.

### Typing

Typing (`i a I A O o c` and ordinary text while a typing group is open)
inserts at **every caret** in one atomic batch under one undo group. The
group seals on repositioning, blur, or exit. `Escape` returns to commands;
the group stays undoable as one step.

### Selection-only commands

These change the selection set but create **no undo entry** and make no
text change:

| Command | Effect |
|---|---|
| `editor.keep_primary` | drop all members but the primary |
| `editor.collapse` | every range collapses to a caret at its head |
| `editor.clear_selection` | single caret at the primary head (at N=1 identical to collapse) |
| `editor.rotate_primary` | primary advances to the next/previous member |
| `editor.flip_selections` | swap anchor and head on every member |
| `editor.trim_selections` | trim whitespace from each member's edges; empty results become carets at the trim position |
| `editor.invert_selections` | line-bounded complement: for every line intersecting the set, the unselected spans of that line become the new set |
| `editor.add_caret_above` / `editor.add_caret_below` | one caret per member on the adjacent visual row; refused if the row does not exist or the cap would be exceeded |

### Pointer vocabulary

- Plain **click** replaces the set with one caret; **drag** selects one range.
- **Double / triple click** select word / line.
- **Shift-click** extends the primary.
- **Mod-click** (Ctrl/Cmd) toggles a caret: removes a caret that lands
  inside an existing selection (never the last), otherwise adds one.
- **Mod-drag** adds a range.
- **Right-click** enters Context Mode: inside any member it keeps the set;
  otherwise it selects the word/caret at the pointer.

### Multi-selection in ordinary Edit (OFF state)

Leaving Context Mode with more than one selection keeps them: the set
renders with a **muted fill** and a visible non-color count ("N selections")
in the note header/footer. In ordinary Edit, `Escape` first collapses
additional selections, then a remaining range to its head. Text drag-to-move
stays enabled outside Context Mode.

---

## Match-driven selection (select matches / split)

Both commands operate **document-wide** on the current frozen revision and
**replace** the whole selection set. They use the existing find engine
(literal or regex); no second matcher exists.

| Command | What it does |
|---|---|
| `editor.select_matches` | selects every match of the query (literal or regex, case-sensitive optional). |
| `editor.split_selection` | the Kakoune `S` complement: selects the gaps *between* matches, document-wide (leading/trailing gaps included only when nonempty). |

Both are available from the **Select** menu and the command palette. From
the menu, the command opens an owned query row in the note pane with
literal/regex/case toggles; a **draft** collects matches and paints them as
an **outline overlay** — the committed set is untouched until you choose
**Apply** (Enter). `Escape`/Close discards the preview with zero effect. If
the source changes while the preview is open, Apply re-collects before
installing.

Refusal rules (all atomic — the committed set is left unchanged):

- **Zero matches** — `NoMatches`: nothing changes (a selection set is never
  empty; "select nothing" is not expressible).
- **Zero-width matches** (`select_matches`) — refused with the count of
  zero-width hits (`ZeroWidthMatches 5`): a zero-width span cannot become a
  selection. Navigation/replace of zero-width matches keeps working through
  the existing find semantics.
- **No gaps** (`split_selection`) — `NoGaps`: matches exist but no nonempty
  text lies between them; the prior set is kept.
- **Over the cap** — refused with the observed count
  (`TooManySelections 14000`) before anything is materialized or installed.
- **Regex worker unavailable** — a typed, actionable error naming the
  executable path; literal mode and typing/Exit keep working.

---

## History: budgets, eviction, truncation

Editing history is in-memory and bounded per buffer: **128 MiB / 10,000
nodes** retained per buffer, **512 MiB** app-wide. Accounting counts unique
retained text plus selection metadata per recorded edit. When a budget is
reached, eviction is deterministic and **oldest-first** (off-path branches
first, then the oldest removable prefix of the current path); the current
node and any restore target are never evicted.

- An operation whose history could not be reserved is **refused before any
  mutation**, with a typed limit error — a giant edit can never silently
  become non-undoable.
- The **Editing history** chooser marks truncation: "Earlier states removed
  to stay within the memory budget (oldest retained #N)." Chips referencing
  evicted states are disabled with a reason.
- A live page/cursor invalidated by eviction reports `HistoryChanged` and
  refreshes.
- History is process-local: closing the last buffer owner clears it; nothing
  is serialized.

---

## Diagnostics and the burst hint

A bounded, content-free diagnostics ring (≤1,000 events, ≤512 B each) tracks
outcomes locally: operation tickets issued/applied/refused/cancelled/
expired/deduplicated, selection operations, match collection/preview/commit,
history page/restore/evict, clipboard, and burst hints. It is in-memory,
cleared on exit, and exported **only** by the explicit palette command
`editor.diagnostics_export` (content-free by schema — no note text,
clipboard values, queries, or paths are ever recorded).

**Burst hint:** five or more accelerator activations within 1.5 s, of
which at least four are mutating or typing-entry, shows a non-blocking
banner once per 30 s offering exactly two actions — **Exit Context Mode**
and **Open history**. It never offers an "undo burst" action (shared-buffer
edits make that promise false), it never blocks, and it is dismissible.
The hint can be disabled with the typed setting
`editor.burst_hint_enabled = false` (default `true`).

---

## Limits and guards

| Limit | Value |
|---|---|
| Maximum selections per view | 10,000 |
| Maximum query length (find / select matches) | 8 KiB |
| History retention per buffer | 128 MiB / 10,000 nodes |
| History retention app-wide | 512 MiB |
| Diagnostics ring | ≤1,000 events, ≤512 B each |

Guards:

- **Link insertion** with more than one caret is refused with
  "Choose one caret to insert a link." (a link has one anchor).
- **Stale operations** are refused with zero effect: stale revision, stale
  selection version, replaced buffer incarnation, read-only targets, and
  replayed/expired/cancelled tickets each leave text, revision, undo depth,
  and format byte-identical. No-op edits create no history node.
- **No compatibility layer**: there are no aliases, shims, stubs, or
  placeholder controls. Old command IDs are never kept as aliases.

---

## Out of scope (deferred)

Registers and clipboard history, macros, `.` repeat-last-change, counts
(`3dw`), shell pipes, LSP refactoring, surround, case/style/line
transforms, per-selection regex split/keep/drop, rectangular/column
selection, scoped/union selection matching across views, rebindable Context
Mode accelerators, and mode persistence across tab switches are not shipped
and are not planned in the current release horizon.
