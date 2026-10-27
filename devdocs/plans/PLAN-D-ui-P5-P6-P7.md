# PLAN D — UI workstream: application shell (P5) + editor integration (P6) + graph integration (P7)

**Specification:** RFC-0001 v2.1 with command and durability/identity amendments (§4.5/4.13–4.16/10.6.1) — canonical copy `./SPEC-draft-v2.md`; synchronized mirror `./RFC-0001-v2.1-spec.md`.
**Spec phases owned:** §9 Phase 5 (UI shell), plus the **integration** of Phase 6 (P6, editor) and Phase 7 (P7, graph) into the shell.
**Acceptance criteria owned:** AC-12, AC-13, AC-15, AC-17, AC-22 (UX half), AC-25 (smoke), AC-26 (network-denied run), AC-23 (CI green per OS, shared), AC-32 (GUI command adapter/integration, shared with PLAN-C), AC-33/34/35 (consumer checks).
**Depends on:** PLAN-C C14 (`flavite-api`), C7 (search), C8 (channels), C10 (`BufferManager`), C11 (recovery offers); PLAN-A A11 (editor widget, for D5); PLAN-B B9 (graph view, for D6).
**Feeds:** §9 Phase 8 (hardening and release) — consumes this plan's daily-usable app.
**Overlap note (read first):** the requested grouping lists P6 and P7 in both
this plan and PLAN-A/PLAN-B. **Construction of the editor widget is PLAN-A's
(A7–A11); construction of the graph is PLAN-B's (B5–B9).** This plan's D5 and
D6 are **integration slices**: they wire the delivered widgets into the
application shell (panes, autosave loop, graph focus, pin UI). They do not
rebuild editor or graph internals. PLAN-B created `crates/flavite-ui` with
only `src/graph/*` + examples; **this plan's D1 adds the application shell
around it.**

## 0. How to read this plan

Every slice is a **vertical cut** through the specification — implemented,
runs, rigorously proven, honestly validated — not a horizontal layer. The
"fully and honestly" bar (working end-to-end behavior; verification built
during the slice; debt paid in touched code; rigor in choices, edge cases, and
error paths; coherent shippable state; no false completion claims) applies to
every slice — see `README.md` in this directory. Locked spec decisions are
cited by number; an agent executes them, it does not re-design them.

## 1. Workstream context

§9 Phase 5: "Panes, palette, quick switcher, backlinks, search panel,
diagnostics, conflict banners, read mode (incl. attachment embeds), settings.
Uses a minimal editor surface. **Exit: daily-usable on a real vault.**" This
plan delivers that exit criterion. D16 is the architectural rule the shell
obeys: **flavite-ui talks to the core only through flavite-api** — first-party
panels (backlinks, graph, search) are consumers of that API, and if we need a
private back-door, that's a design bug to fix in the API.

Locked UI facts from the spec: app state hosts Lattice on the UI thread;
Lattice's headless Session owns BufferManager, while the UI owns panes and
graph view state (§4.2/§4.5). iced `Subscription`s bridge the pulse (lossy) and
notice (lossless) channels to `Message`s (D3); all commands go through the
registry (palette, keybindings, menus, graph clicks — §4.5); read mode renders
from the **buffer** so unsaved edits show (§4.6); the quick switcher is
in-memory (nucleo, no DB hit per keystroke) while full-text search reflects
saved content with commit-before-search (§4.7, D12); user-state files are
versioned and preserve unknown fields (§4.12).

Local document commands execute synchronously through the Session. Worker
effects/replies use a separate operation adapter with correlation ids; iced
Tasks/Messages adapt its headless types. Notice presentation acks only confirm
display. They never apply document edits or complete a transaction. Read-only
revisioned views feed editor/read/card widgets; the UI has no mutable manager
handle. Save Pending and terminal success/partial-failure states are distinct.

## 2. Slice dependency graph

```
C14 (flavite-api) ─► D1 (P5-1: app shell: panes, palette, subscriptions, settings)
C7 + C8 + C10 + C11 ─► D2 (P5-2: quick switcher + search panel + backlinks)
C8 + C11 + C13 ─────► D3 (P5-3: notice-driven UI: conflict banners, recovery, rename, diagnostics)
C6 + C7 ────────────► D4 (P5-4: read mode: Renderer trait, wiki: routing, attachment embeds)
A11 (editor widget) + D1 ─► D5 (P6 integration: editor pane, autosave, conflict wiring)
B9 (graph view) + D1 ────► D6 (P7 integration: graph pane, focus, pin UI)
D1–D6 ─────────────────► D7 (P5-5: settings UI + daily-usable validation on a real vault)
D1–D7 ─────────────────► D8 (hardening + acceptance: AC-12/13/15/17/22/25/26)
```

D1/D2 can proceed after the initial C14 facade (and B2's graph-host crate).
D4 also needs A6's renderer decision; D3 needs the real C11/C13 handlers.
D5 waits for A11; D6 waits for B9; D7–D8 wait for both. Handler availability
follows the owning slice; an early facade does not provide future behavior.

## 3. Locked decisions this workstream implements (do not re-open)

| Spec ref | Decision | Where it lands here |
|---|---|---|
| D16 | flavite-ui talks to the core only through flavite-api; first-party panels are consumers of the API; no private back-doors | every slice; grep-gated |
| §4.2 / §4.5 | UI hosts Lattice; headless Session owns BufferManager; revisioned views + typed commands; independent operation replies and pulse/notices | D1, D5; AC-32 |
| D3 | pulse (lossy) vs notice (lossless, coalesced, acked) — conflict/rename/recovery notices MUST NOT be dropped | D1 subscriptions, D3 UI |
| §4.5 | Palette, keybindings, menus, graph clicks ALL go through the command registry | D1 palette, D5/D6 |
| D7 | Conflict protocol actions (Reload/Keep mine/Save as conflict copy/Merge); autosave disabled in Conflicted | D3, D5 |
| D6 | Recovery offer via Channel-B notice; autosave 1 s idle (default on) | D3, D5 |
| §4.6 | Read mode renders from the buffer; `Renderer` trait; `wiki:` clicks → `note.open`; attachment embeds render; embed cycles cut (depth 5 + size budget) | D4 |
| §4.7 | Quick switcher in-memory (nucleo, ≤ 16 ms/keystroke @ 50k titles); full-text search = saved content + commit-before-search | D2 |
| §4.10 | No raw HTML ever; URL scheme allowlist (`http`, `https`, `mailto`; `file:` prompts); image limits ≤ 64 MP / ≤ 64 MB decoded | D4 |
| §4.8/R12 | Settings + user state in `.flavite/`, versioned, unknown fields preserved | D1, D7 |
| §4.9 | Cache deleted while running → detect + rebuild online ("reindexing"); vault unavailable → read-only state | D8 |
| §8 AC-12/13/15/17 | Cold open ≤ 500 ms p95 first paint (warm cache); rebuild ≤ 30 s with UI usable; search p95 ≤ 50 ms @ 20k + save-then-search immediate; bulk reconcile frame p99 ≤ 33 ms + zero lost notices | D2, D8 |
| §10.5 | flavite-ui is Tier 3 — measured, reported, not gating | coverage throughout |

## 4. Crate dependency contract

Full table + interpretation notes in PLAN-A §4. This plan's edges:
`flavite-ui → {flavite-api, flavite-editor, flavite-graph, flavite-buffer}`
(buffer for read-only view/EditOp types — Session ownership stays behind api;
recorded in crates.toml). iced/wgpu
permitted in flavite-ui (§4.1/§10.2). **Grep gate: no `flavite_vault`,
`flavite_index`, or `flavite_fs` imports in `crates/flavite-ui/src`** — all
core traffic through flavite-api (D16).

## 5. Slices

### Slice D1 — P5 slice 1 of 5: application shell — panes, palette, subscription bridging, settings

**Spec basis:** §9 Phase 5, §4.2 (runtime topology), D16, §4.5 (commands),
D3 (channels), §4.12 (settings files), §4.11 (diagnostics panel home).
**Depends on:** PLAN-C C14 (initial Lattice facade), C10 (buffer foundation),
C8 (Session/transport via api), PLAN-B B2 (existing flavite-ui crate).
**Feeds:** D2–D8.
**Vertical thread:** the app opens a fixture vault, shows the pane layout, the
palette lists registry commands with keybindings, settings round-trip through
`.flavite/settings.toml`, and pulse/notice subscriptions deliver messages —
all through flavite-api.
**Coverage:** flavite-ui Tier 3 (measured, not gating — §10.5).

**Goal.** The iced application shell: pane grid, command palette, subscription
bridging (pulse lossy / notice lossless with ack), settings load/save, and the
app state that hosts Lattice/Session. Include independent operation transport
adapters so worker progress requires no banner display.

**Decisions already locked.**
- App state (UI thread) hosts Lattice and owns panes/graph view state; Lattice
  owns the headless Session/BufferManager (§4.2/§4.5).
- `Message::Invoke` or typed editor commands → api registry/Session → immediate
  local result or Pending effects. GUI maps effects and correlated terminal
  results to Tasks/Messages; no iced types enter the core (§4.5).
- Subscriptions bridge pulse/notice to Messages; notice acks on display (D3).
  Operation requests/replies are a separate lossless, nonblocking adapter;
  their correlation/application rules establish command completion.
- Settings live in `<vault>/.flavite/settings.toml` — versioned, unknown fields
  preserved (§4.12).

**What to implement.**
- `crates/flavite-ui/src/app.rs` — the iced `Application`: `State`
  (Lattice, panes, graph view state), `Message`, `Subscription`, `update` —
  document commands use api's JSON or typed registry entry; local edits complete
  in the same host turn. Views read revisioned snapshots without mutation access.
- `src/pane.rs` — pane grid (editor / read / graph / sidebar).
- `src/palette.rs` — command palette: nucleo over registry commands +
  keybindings; keyboard-driven.
- `src/subscription.rs` — pulse → `Message::Pulse` (lossy), notice →
  `Message::Notice` (lossless; ack sent on display).
- `src/operation.rs` — pump Session effects, worker requests/replies, and
  correlated completions independently of notice UI. Track Pending vs terminal
  results, nonblocking Busy, ActionRequired, and save partial-failure states;
  preserve core correlation ids and delegate ordering/retries to Session.
- `src/settings.rs` — settings model + load/save: `rewrite | alias | none`
  (default `rewrite`), `recovery_interval` (default 2 s), ignore globs,
  verification interval (10 min) and change-log budget (64 MiB); external
  symlink targets are forbidden in v1 with no override. `version` field; unknown
  fields preserved on write.
- `src/lib.rs` — crate docs; module root (absorbs PLAN-B's `src/graph/*`).
- `crates/flavite-ui/examples/app-demo.rs` — the demo app over a fixture vault.

**How to implement.**
1. App skeleton: State/Message/Subscription/update over flavite-api; open a
   vault; show panes.
2. Palette over the registry; keybindings.
3. Subscription bridging; ack on display.
   Wire operation transport separately; continue pumping even when notices
   are hidden/unacknowledged. A presentation ack must not mutate documents.
4. Settings round-trip (test below).
5. Demo app; run.

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — app-state transitions driven by synthetic
  messages; palette filtering + keybinding resolution; settings round-trip +
  unknown-field preservation + defaults; subscription bridging (pulse
  drop-oldest observed; notice ack sent).
- AC-32 adapter tests: local edit completes synchronously; Pending displays no
  false save success; worker reply routes by request/buffer identity; notice
  presentation cannot advance a journal. Compare document command outcomes
  with the headless host, including overload and ActionRequired.
- Demo run recorded.
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -rn "flavite_vault\|flavite_index\|flavite_fs" crates/flavite-ui/src` — no matches (D16).
  - `grep -c "Invocation" crates/flavite-ui/src/app.rs` — commands via the registry (§4.5).

**Exit criteria.**
- [ ] The shell runs: panes, palette, subscriptions, settings — over flavite-api
      only.
- [ ] Tests green; `just gate` green; flavite-ui coverage measured (Tier 3).
- [ ] Lattice/Session hosting and independent operation pump verified (AC-32);
      no direct mutable BufferManager access in UI consumers.

### Slice D2 — P5 slice 2 of 5: quick switcher + search panel + backlinks panel

**Spec basis:** §9 Phase 5, §4.7 (search UX: switcher in-memory with nucleo, no
DB hit per keystroke; full-text search reflects saved content via
commit-before-search), D12, §8 AC-15 (search p95 ≤ 50 ms @ 20k; a search
issued immediately after a save ack MUST return the saved content; switcher
≤ 16 ms/keystroke @ 50k titles).
**Depends on:** D1, PLAN-C C7 (SearchBackend), C14 (api queries). **Feeds:** D7, D8.
**Vertical thread:** the switcher opens a note from 50k titles within budget;
a search issued immediately after a save ack returns the saved content (AC-15
e2e); the backlinks panel lists inbound links.

**Goal.** The three query surfaces: quick switcher, full-text search panel,
backlinks/unresolved panels — with AC-15's latency and read-your-writes
guarantees measured.

**Decisions already locked.**
- Quick switcher: in-memory title + alias list, matched with `nucleo`, no DB
  hit per keystroke (§4.7).
- Full-text search reflects **saved** content; by D12's commit-before-search
  it reflects every save acked before the query began (§4.7).
- Dirty buffers are overlaid only for in-note find (§4.7) — not here.

**What to implement.**
- `src/switcher.rs` — quick switcher: in-memory title+alias list (fed by index
  events), nucleo matcher, keyboard nav, opens the note via `note.open`.
- `src/search_panel.rs` — full-text search UI over the `SearchBackend` (via
  api): query box, results with epoch/generation, result → note.open. Reject
  stale cursors and re-query on epoch reset; switcher/backlinks also refresh.
- `src/backlinks.rs` — backlinks panel (LinksTo/LinksFrom) + unresolved-links
  view with repair hints (§4.5's `LinksBroken`).
- Bench hooks: switcher latency micro-bench (50k titles), search latency
  bench (20k notes) — feeding D8's AC-15 measurements.

**How to implement.**
1. Switcher: build the in-memory list from index events; nucleo; measure.
2. Search panel: query → api → results; commit-before-search is the index's
   guarantee (C7) — the e2e test proves it through the app path.
3. Backlinks/unresolved panels.
4. Wire benches.

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — switcher filtering/nav; search panel result
  routing; backlinks content.
- **AC-15 e2e:** save a note (ack) → immediately search for its new content
  → present. Switcher bench: ≤ 16 ms/keystroke over 50k titles. Search bench:
  p95 ≤ 50 ms over 20k notes.
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -c "nucleo" crates/flavite-ui/Cargo.toml crates/flavite-ui/src/switcher.rs` — switcher is in-memory (§4.7).

**Exit criteria.**
- [ ] Switcher, search panel, backlinks panel work; AC-15 e2e + latency benches
      measured (green or honestly reported).
- [ ] `just gate` green.

### Slice D3 — P5 slice 3 of 5: notice-driven UI — conflict banners, recovery offer, rename suggestions, diagnostics

**Spec basis:** §9 Phase 5, D7 (conflict protocol actions), D6 (recovery
offer), D8 (RenameSuggested actions), §4.5 (Event variants), §4.11
(Diagnostics panel), §8 AC-17 (zero notices lost during bulk reconcile).
**Depends on:** D1, PLAN-C C8 (notices), C11 (recovery offers), C13 (rename
notices). **Feeds:** D7, D8.
**Vertical thread:** an external edit lands on a dirty buffer → the conflict
banner appears with working actions and autosave is disabled (D7); a crash
left a recovery file → the offer appears and restores; an external rename →
the suggestion appears with [Update links] [Add alias] [Ignore]; a bulk
reconcile delivers every notice (AC-17).

**Goal.** Every Channel-B notice becomes UI, losslessly: conflict banners
with the D7 actions, recovery offers, rename suggestions, and the diagnostics
panel.

**Decisions already locked.**
- Conflict banner actions (D7): Reload (snapshot first) · Keep mine (snapshot
  first) · Save mine as conflict copy · Merge.
- Conflicted state: autosave MUST be disabled, manual save blocked until
  resolved (D7).
- Recovery offer via `Notice::RecoveryOffer` (D6).
- RenameSuggested: "Ideas → Concepts detected. 14 links affected."
  [Update links] [Add alias] [Ignore] (D8).
- Notices MUST NOT be dropped, ever (D3/R5); the UI acks on display.
- Display acknowledgement only retires presentation delivery. User actions
  dispatch commands and await their own results; pending journal patches,
  saves, or recovery restores never complete from the banner ack (§4.5.4).

**What to implement.**
- `src/notice_ui.rs` — banner components for every notice kind, with actions
  dispatching commands through flavite-api (reload/keep-mine/save-as/
  merge; restore; update-links/add-alias/ignore). Expose AppliedPendingSave,
  Partial/ActionRequired with txn/outstanding items, and pending durable undo.
  Handle command Pending,
  rejection, ActionRequired, and terminal outcomes distinctly from display ack.
- `src/diagnostics_panel.rs` — the Diagnostics panel: metrics (§4.11) +
  drift hint + broken/ambiguous link counts + attachment orphans. Include
  verification age/overdue, search epoch/checkpoint/log coverage, UUID binding
  conflicts, pinned history bytes, and nonterminal/blocked journal items.
- Conflict wiring: `BufferStateChanged` → banner; Conflicted → autosave off
  (D7, enforced in D5's autosave path).

**How to implement.**
1. Notice → banner mapping with actions; test each action's command.
2. Diagnostics panel over the metrics.
3. AC-17 integration test (with C8's bulk mode): bulk reconcile including
   renames + conflicts → zero notices lost (count delivered vs. emitted).

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — every D7 table row → banner + action path
  tested; autosave disabled in Conflicted; recovery offer restores; rename
  suggested actions work; **AC-17: zero notices lost during a bulk reconcile**
  (integration test with the vault actor).
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -c "Reload\|Keep mine\|Save mine as conflict copy\|Merge" crates/flavite-ui/src/notice_ui.rs` — all four D7 actions present.

**Exit criteria.**
- [ ] All notice kinds have working UI; D7 actions work; autosave disabled in
      Conflicted; AC-17 green.
- [ ] `just gate` green.

### Slice D4 — P5 slice 4 of 5: read mode — `Renderer` trait, `wiki:` routing, attachment embeds

**Spec basis:** §9 Phase 5, §4.6 (read mode: renders from the **buffer** so
unsaved edits show; pulldown-cmark-compatible events where `[[x]]` becomes a
Link event with URL `wiki:x`; iced's markdown widget is event-driven; click
handler routes `wiki:` URLs to `note.open`; rendering sits behind a `Renderer`
trait; images decode through `image` with explicit `Limits`; embeds of
attachments render the attachment), §4.10 (no raw HTML ever; URL scheme
allowlist; embed cycles cut by cycle detection + depth limit 5 + total-size
budget; image decompression bombs → `image::Limits`), S1's markdown-coverage
findings (A6).
**Depends on:** D1, PLAN-C C6 (semantic parse), C7 (resolution), A6 (coverage
decision). **Feeds:** D7, D8.
**Vertical thread:** open a note in read mode → unsaved edits show (rendered
from the buffer) → click a wikilink → the target note opens → an image embed
renders inline → an embed cycle is cut.

**Goal.** Read mode: buffer-backed rendering, `wiki:` navigation, attachment
embeds, cycle cutting, and the security limits — behind the `Renderer` trait.

**Decisions already locked.**
- Read mode renders from the **buffer** (unsaved edits show) (§4.6).
- `[[x]]` becomes a Link event with URL `wiki:x`; a click handler routes
  `wiki:` URLs to `note.open` (§4.6).
- Rendering sits behind a `Renderer` trait because iced's markdown widget may
  not cover tables, footnotes, or images well enough (S1 checks; §4.6) — we
  expect to extend it or replace it with our own.
- Images decode through `image` with explicit `Limits` (≤ 64 MP, ≤ 64 MB
  decoded); thumbnails cached (§4.6, D15).
- Embeds of attachments render the attachment: images inline, other kinds as a
  clickable file card (default: OS default application on click) (§4.6).
- Embed cycles: cycle detection + depth limit 5 + total-size budget (§4.10).
- No raw HTML rendering, ever (§0); URL scheme allowlist `http`, `https`,
  `mailto` — everything else, including `file:`, prompts (§4.10).

**What to implement.**
- `src/readmode/mod.rs` — `trait Renderer` (§4.6) + the iced markdown widget
  implementation (extend/replace per S1's coverage findings).
- `src/readmode/parse.rs` — semantic parse via flavite-api's headless read
  helper → pulldown-cmark-compatible events with `wiki:` URLs. Read the
  Session's revisioned buffer view; never acquire a mutable manager handle.
- `src/readmode/wiki.rs` — `wiki:` click routing → `note.open` (via api).
- `src/readmode/embed.rs` — attachment embeds: images inline (decode via
  `image` with `Limits`; thumbnails from the OS cache dir, D15); other kinds
  as clickable file cards (OS default application on click); cycle detection +
  depth 5 + total-size budget.
- `src/readmode/html.rs` — the no-raw-HTML guarantee: the renderer never
  emits raw HTML (asserted by tests).

**How to implement.**
1. Renderer trait + markdown widget impl; render from the buffer (subscribe to
   buffer changes).
2. wiki: routing; test.
3. Embeds: images inline with limits; file cards; cycle cutting; test.
4. Security tests (below).

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — wiki routing (click `[[Ideas]]` → `note.open`
  with the resolved target); read mode shows unsaved edits (buffer-backed);
  image embed renders inline (fixture PNG); file card for a PDF; embed cycle
  A→B→A cut; depth limit 5; total-size budget; URL scheme allowlist
  (`file:` prompts, `http/https/mailto` pass); **no raw HTML** (fixture with
  HTML → not rendered/executed); image limits (bomb-like fixture → refused,
  no OOM).
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -c "Renderer" crates/flavite-ui/src/readmode/mod.rs` — trait present (§4.6).
  - `grep -rn "raw_html\|RawHtml" crates/flavite-ui/src/readmode` — no matches (no raw HTML, ever — §0).

**Exit criteria.**
- [ ] Read mode works: buffer-backed, wiki navigation, attachment embeds,
      cycle cutting, security limits — tested.
- [ ] `just gate` green.

### Slice D5 — P6 integration: editor pane production wiring

**Spec basis:** §9 Phases 5–6, D7 (autosave disabled in Conflicted; manual save
blocked), D6 (autosave 1 s idle, default on), D4 (read-your-writes),
PLAN-A A7–A11 (the delivered editor widget — construction is PLAN-A's; this
slice wires it).
**Depends on:** D1, D3 (conflict wiring), PLAN-A A11 (editor widget),
PLAN-C C10/C11 (BufferManager, save). **Feeds:** D7, D8.
**Vertical thread:** type in the editor pane → ~1 s after idle the file on disk
has the bytes (autosave); an external edit on a clean buffer → silent reload
with cursor mapped; a conflict disables autosave and shows the banner; a
save-ack → a search immediately returns the saved content (read-your-writes
e2e).

**Goal.** The editor widget wired into the app: editor pane, autosave loop,
dirty indicator, conflict wiring, save-ack → read-your-writes.

**Decisions already locked.**
- Autosave: 1 s idle, default on (D6); **disabled in Conflicted** (D7).
- One buffer authority per note (D7): the pane, read view, graph card, and
  embeds all view the same buffer.
- Read-your-writes: the save path applies the exact bytes to the index before
  acking (D4).
- Cursor/scroll mapping across reloads (D7 clean+changed row).

**What to implement.**
- `src/editor_pane.rs` — hosts `EditorWidget` (PLAN-A's) over the app's
  Lattice revisioned buffer view; autosave timer (iced `Subscription`, 1 s idle)
  drives Session deadlines; dirty indicator; open/close notes; find-in-note host.
- Autosave wiring: GUI clock ticks drive Session deadlines for every dirty
  buffer, including card-only notes. Session requests eligible saves;
  **Conflicted → autosave skipped** (D7), enforced for every host. Pending
  is acceptance, not save success. Apply saved-revision receipts so newer
  edits stay Dirty; display partial disk/index failure and preserve recovery
  per §4.5.3. UI never schedules a blind overwrite retry.
- Reload mapping: external change on a clean buffer → silent reload with
  cursor/scroll mapped through the diff (D7).
- Wire the editor's diagnostics decorations (PLAN-A A9) to the app's
  BufferState/Diagnostic events.

**How to implement.**
1. Host the widget over Lattice's read views; open/close/edit through commands.
2. Autosave subscription: send clock ticks; Session evaluates 1 s idle and
   Conflicted eligibility across all dirty buffers. Dirty indicator from
   revisioned BufferState; save status from correlated results.
3. Reload mapping path.
4. e2e tests (below).

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — autosave writes within ~1 s of idle; conflict
  disables autosave (D7, tested); manual save blocked while Conflicted;
  reload maps cursor/scroll; save-ack → search reflects content (e2e with
  C7's commit-before-search through the app path).
- AC-32 GUI interleavings: save revision N, type N+1 while pending, complete N
  → N+1 stays Dirty with recovery coverage. Exercise stale completions after
  reopen, queued save/rename, external change while saving, and disk replacement
  followed by fsync/index failure. Compare Session outcomes with the CLI driver;
  save status must follow terminal results, independently of notice display.
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -c "Conflicted" crates/flavite-ui/src/editor_pane.rs` — conflict wiring present (D7).

**Exit criteria.**
- [ ] Editor pane works end-to-end in the app: typing, autosave (~1 s),
      conflict disables autosave, reload mapping, read-your-writes e2e.
- [ ] `just gate` green.

### Slice D6 — P7 integration: graph pane

**Spec basis:** §9 Phases 5 + 7, D15 (pipeline; pinned positions are user
state; carry-over only T0–T2), §4.5 (`graph.focus`), PLAN-B B5–B9 (the
delivered graph view — construction is PLAN-B's; this slice wires it).
**Depends on:** D1, PLAN-B B9 (graph view in `flavite-ui/src/graph/*`),
PLAN-C C7 (index feed), C13 (tier outcomes for pin carry-over), C14
(commands). **Feeds:** D7, D8.
**Vertical thread:** open a vault → the graph pane renders the vault's graph
(index feed → GraphModel → LayoutWorker → PositionBuffer → shader view) →
ctrl-click a link in the editor → `graph.focus` centers that node → pin a node
→ the pin persists in `.flavite/graph.json` → an external (T3) rename does not
carry the pin.

**Goal.** The graph view wired into the app: graph pane, live feed, focus
command, pin UI, reduced-mode notice.

**Decisions already locked.**
- The graph widget code lives in `flavite-ui/src/graph/*` (PLAN-B); this slice
  adds the pane + wiring around it.
- `graph.focus` from editor clicks and the palette (§4.5).
- Pin/unpin commands; `.flavite/graph.json`; carry-over only T0–T2 (D8/D15).
- Reduced mode notice on weak GPUs (D15/§4.9; PLAN-B B9's watchdog).

**What to implement.**
- `src/graph_pane.rs` — hosts the graph view; subscription: poll the index
  version (250 ms + on VaultChanged pulse) → invalidation/reset feed (§4.14) → GraphModel →
  `LayoutWorker`; render via the shader view / card layer.
- `src/graph/focus.rs` — `graph.focus` command (from editor ctrl-click,
  palette, switcher).
- `src/graph/pin_ui.rs` — pin/unpin UI + the tier-gated carry-over display.
- Reduced-mode notice host (PLAN-B B9's watchdog output).

**How to implement.**
1. Graph pane hosting the PLAN-B view; wire the feed subscription.
2. `graph.focus` command; test editor → graph focus.
3. Pin UI; persistence via the commands; tier display.
4. Reduced-mode notice.

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — pane renders a fixture graph; focus command
  centers the node; pin persists + round-trips (graph.json); reduced-mode
  notice shown on software renderer; card checkbox toggle → buffer edit
  (D15, through BufferManager).
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -c "graph.focus" crates/flavite-ui/src/graph_pane.rs crates/flavite-ui/src/graph/focus.rs` — focus wired (§4.5).

**Exit criteria.**
- [ ] Graph pane works in the app: live graph, focus, pins, reduced mode.
- [ ] `just gate` green.

### Slice D7 — P5 slice 5 of 5: settings UI + daily-usable validation on a real vault

**Spec basis:** §9 Phase 5 exit ("**daily-usable on a real vault**"), §4.12
(settings + versioned user state), §4.11 (doctor integration), D8 (rewrite |
alias | none, default `rewrite`), D6 (`recovery_interval`, default 2 s).
**Depends on:** D1–D6. **Feeds:** D8, §9 Phase 8.
**Vertical thread:** a scripted session on a **real vault** — open → create a
note from a link → rename with rewrite → external edit → conflict banner →
recovery offer → search-after-save → graph pin → doctor clean — passes
end-to-end, and the settings UI edits every setting with round-trip proof.

**Goal.** The app is daily-usable on a real vault, proven by a scripted
end-to-end session, with a settings UI bound to the versioned settings model.

**Decisions already locked.**
- Phase 5 exit: daily-usable on a real vault (§9).
- Settings: `rewrite | alias | none` (default `rewrite`), `recovery_interval`
  (default 2 s), ignore globs, verification interval (10 min), and change-log
  budget (64 MiB); external symlink targets stay forbidden in v1;
  `.flavite/settings.toml`, versioned, unknown fields preserved (§4.12).
- Every default carries a stated reversal condition (spec Conventions).

**What to implement.**
- `src/settings_ui.rs` — settings UI bound to the settings model (D1's
  `settings.rs`): every setting editable, with round-trip + unknown-field
  preservation.
- `devdocs/manual-verification.md` — the step-by-step manual verification
  script for the daily-usable session (cold start; create from link; rename
  with rewrite; external edit → conflict → resolve; crash → recovery offer;
  search-after-save; graph pin; doctor clean).
- `xtask/src/bin/e2e_session.rs` — a scripted end-to-end harness driving the
  app's core paths headlessly (buffer/index/vault level: open, create,
  rename-with-rewrite, external-write conflict, recovery, search, pin,
  doctor) so CI proves the session automatically; UI-level steps (banner
  clicks, palette) are covered by the manual script + widget tests.
- Polish pass on the surfaces D1–D6 created: empty states, error surfaces,
  "reindexing" indicator, "vault unavailable" read-only state (§4.9).

**How to implement.**
1. Settings UI; round-trip tests.
2. e2e harness; run green in CI.
3. Manual verification script; execute it on a real vault and record the
   transcript (honest labeling: what was automated vs. manual).
4. Polish pass; no deferred TODOs (debt paid here).

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — settings UI round-trip; e2e harness green
  (`cargo run -p xtask --bin e2e_session <fixture-vault>`).
- Manual verification transcript recorded in the PR.
- Commands: `cargo test -p flavite-ui` · `cargo run -p xtask --bin e2e_session` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src xtask/src/bin/e2e_session.rs devdocs/manual-verification.md` — empty.
  - `grep -c "version" crates/flavite-ui/src/settings.rs` — versioned user state (§4.12).

**Exit criteria.**
- [ ] **Phase 5 exit: daily-usable on a real vault** — e2e harness green in
      CI + manual verification transcript recorded.
- [ ] Settings UI complete with round-trip proof.
- [ ] `just gate` green.

### Slice D8 — Hardening + acceptance: AC-12/13/15/17/22/25/26 + perf CI

**Spec basis:** §8 AC-12 (cold open 20k-note vault, warm cache: first
interactive paint ≤ 500 ms p95; no-change reconcile ≤ 3 s in the background),
AC-13 (full index rebuild of 20k notes ~80 MB: ≤ 30 s, UI usable throughout),
AC-15 (search latencies), AC-17 (`git checkout` rewriting 20k files: UI frame
p99 ≤ 33 ms throughout, index consistent within 30 s, **zero notices lost**),
AC-22 (single instance UX), AC-25 (screen reader smoke or waiver), AC-26 (no
network traffic with default settings, network-denied CI run), §10.3 (perf
benchmarks gate G2; nightly trends), §4.9 (cache deleted while running →
detect + rebuild online).
**Depends on:** D1–D7. **Feeds:** §9 Phase 8 (hardening and release).
**Vertical thread:** a benchmark harness drives a 20k-note fixture vault
through the app: cold open paint, rebuild timing with UI-usable measurement,
bulk-reconcile frame times with notice counting, search latencies; the
network-denied run and a11y smoke run in CI.

**Goal.** The UI-layer acceptance criteria measured and green (or honestly
reported with recorded adjustments), with perf CI in place.

**Decisions already locked.**
- Reference machine: 4+ cores, integrated GPU (Intel Iris Xe class), NVMe,
  16 GB RAM; numbers are targets to be re-validated (§8).
- G2 (release gate) = G1 + signed packaging + perf benchmarks vs §8
  thresholds + long fuzz campaigns (§10.3); nightly benchmarks track trends.
- AC-26: no network traffic with default settings, verified by a
  network-denied CI run.
- AC-25: screen reader can read note text and move the caret on at least one
  reader per OS, **or** a documented, tracked waiver exists (S1-dependent).

**What to implement.**
- `xtask/src/bin/perf_ui.rs` — the benchmark harness (20k-note fixture vault,
  deterministic): cold open first paint p95 (warm cache), no-change reconcile
  time, full rebuild time with UI-usable sampling, bulk-reconcile frame p99 +
  notice-loss count (AC-17), search latency p95 (AC-15).
- `xtask/src/bin/e2e_bulk.rs` — the `git checkout`-scale scenario: rewrite
  20k files externally while the app runs; measure frame p99 ≤ 33 ms,
  consistency within 30 s, zero notices lost.
- Cache-deleted-while-running handling: detect + rebuild online with the
  "reindexing" indicator (§4.9) — test.
- Vault-unavailable read-only state (§4.9) — test (fixture: unmounted drive
  simulated via a revoked fixture path).
- CI: perf job (nightly trends + G2 gate); network-denied job (AC-26);
  a11y smoke job (AC-25) or the recorded waiver.
- Single-instance UX polish (AC-22): second launch focuses the first — the
  focus path is user-visible (window raise + a toast), tested at the api level
  (C8) and smoke-checked here.

**How to implement.**
1. Build the harnesses; run on the reference machine; record the AC tables.
2. Fix what the numbers expose (within this slice — no deferred perf debt).
3. Wire CI jobs; set the G2 perf gate.
4. a11y smoke per OS (or the waiver with reason + tracking reference).

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — cache-deleted-while-running detection +
  online rebuild; vault-unavailable read-only state.
- Perf harness runs: AC-12 (≤ 500 ms p95 first paint, warm; reconcile ≤ 3 s),
  AC-13 (rebuild ≤ 30 s, UI usable throughout), AC-15 (p95 ≤ 50 ms @ 20k),
  AC-17 (frame p99 ≤ 33 ms; zero notices lost) — numbers recorded; green vs
  thresholds or honestly reported with a recorded adjustment + reason.
- AC-26: network-denied runtime test also inspects attempted connections; a
  seeded attempt must be detected. Denied calls cannot silently pass as proof
  of the no-network policy.
- AC-25: a11y smoke green on at least one reader per OS, or the documented,
  tracked waiver.
- Commands: `cargo test -p flavite-ui` · `cargo run -p xtask --bin perf_ui` · `cargo run -p xtask --bin e2e_bulk` · `just gate` · `just ci-gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src xtask/src/bin/perf_ui.rs xtask/src/bin/e2e_bulk.rs` — empty.
  - `grep -rn "flavite_vault\|flavite_index\|flavite_fs" crates/flavite-ui/src` — no matches (D16, final check).

**Exit criteria.**
- [ ] AC-12, AC-13, AC-15, AC-17, AC-22 (UX half), AC-25 (or waiver), AC-26
      measured — green or honestly reported with recorded adjustments.
- [ ] AC-32 GUI/CLI command equivalence and adapter interleavings green;
      independent operation completion is verified with notices unpresented.
- [ ] Perf CI job green with trends; G2 perf gate set.
- [ ] `just gate` + `just ci-gate` green on all three OSes.

## 6. Verification command index

| Command | Gate | Purpose |
|---|---|---|
| `just gate` | G0 (every slice) | fmt + clippy + test + `arch check`, ≤ 5 min, ends with `just gc` |
| `just ci-gate` | G1 (CI, 3 OSes) | G0 + coverage + suites + doc + deny/audit/shear + fuzz smoke + network-denied + MSRV |
| `cargo xtask arch check` | G0 | drift + edges + layering |
| `cargo test -p flavite-ui` | per slice | shell/panel/readmode/editor-pane/graph-pane suites |
| `cargo llvm-cov -p flavite-ui --lcov` | per slice | Tier-3 measurement (reported, not gating) |
| `cargo run -p flavite-ui --example app-demo` | D1+ | vertical demo app |
| `cargo run -p xtask --bin e2e_session <vault>` | D7 | scripted daily-usable session |
| `cargo run -p xtask --bin perf_ui` | D8 | AC-12/13/15 harness |
| `cargo run -p xtask --bin e2e_bulk` | D8 | AC-17 bulk scenario |
| `just network-denied` | G1 Linux | AC-26 no-network run |
| `cargo clippy --workspace --all-targets -- -D warnings` | G0 | lint |
| `cargo fmt --all --check` | G0 | format |
| `just fuzz-smoke` | G0 hook + G1 | fuzz targets over seeds + generated input |
| `just clean --dry-run` · `just clean` · `just gc` | per slice | disk budgets (§10.10) |

## 7. Source-review and gate index

Text searches aid review; they cannot establish snapshot, rendering, or
transaction correctness. Compiler/architecture checks and behavioral tests
provide the acceptance evidence (§10.3).

```bash
# D16: flavite-ui talks to the core only through flavite-api
grep -rn "flavite_vault\|flavite_index\|flavite_fs" crates/flavite-ui/src   # expect: no matches
# iced/wgpu only in the UI-tier crates (enforced by arch check too)
grep -rln -E '^\s*(iced|wgpu)' crates/*/Cargo.toml   # expect: flavite-editor + flavite-ui only
# No deferred work
grep -rn "TODO\|FIXME\|XXX" crates/flavite-ui/src xtask/src   # expect: no matches
# Commands go through the registry (§4.5)
grep -c "Invocation" crates/flavite-ui/src/app.rs   # expect: >= 1
# Read mode sits behind the Renderer trait (§4.6)
grep -c "trait Renderer" crates/flavite-ui/src/readmode/mod.rs   # expect: 1
# No raw HTML in read mode (§0)
grep -rn "raw_html\|RawHtml" crates/flavite-ui/src/readmode   # expect: no matches
# Settings are versioned user state (§4.12)
grep -c "version" crates/flavite-ui/src/settings.rs   # expect: >= 1
# Conflict wiring present (D7)
grep -c "Conflicted" crates/flavite-ui/src/editor_pane.rs   # expect: >= 1
# Arch green
cargo xtask arch check
```

## 8. Plan exit criteria

- [ ] AC-33/34/35 GUI checks pass for live epoch reset/stale cursors, overdue
      verification, native collision diagnostics, and mixed-EOL/Unicode views.
- [ ] Journal UI distinguishes durable completion, pending buffer saves, blocked
      items, and undo awaiting durable cancellation (AC-2/10/32).

- [ ] **Phase 5 exit (§9): daily-usable on a real vault** — the scripted
      end-to-end session (D7) is green in CI (harness) and recorded (manual
      transcript).
- [ ] P6 integrated: the editor pane (D5) delivers typing, autosave (~1 s,
      disabled in Conflicted), conflict wiring, reload mapping, and
      read-your-writes e2e — construction remains PLAN-A's.
- [ ] P7 integrated: the graph pane (D6) delivers the live graph, focus,
      pins, and reduced mode — construction remains PLAN-B's.
- [ ] AC-12, AC-13, AC-15, AC-17, AC-22 (UX half), AC-25 (or waiver), AC-26
      measured — green or honestly reported with recorded adjustments.
- [ ] D16 holds everywhere: `grep` proves flavite-ui imports no core crate
      directly; all traffic flows through flavite-api.
- [ ] flavite-ui coverage measured and reported (Tier 3); G0/G1 green on
      Linux, macOS, Windows; perf CI trends running.
- [ ] The app left in a coherent, shippable state: nothing half-wired between
      the shell, the editor pane, and the graph pane.


