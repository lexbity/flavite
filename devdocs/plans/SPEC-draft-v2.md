# RFC-0001 v2: Lattice — a local-first plain-text knowledge vault in pure Rust

*Working name. Status: v2.1, amended with the command execution contract (§4.5) and the durability/identity review (§4.13–4.16, §10.6.1). Incorporates design-review round 1 (Appendix A) and the project-setup addition (§10). Supersedes `SPEC-draft-v1.md`. Canonical copy: `devdocs/plans/SPEC-draft-v2.md`; synchronized mirror: `RFC-0001-v2.1-spec.md` in this directory. Targets: Linux, macOS, Windows (all day one).*

## Conventions

Normative language follows RFC 2119:

- **MUST / MUST NOT** — hard requirements, non-negotiable.
- **SHOULD / SHOULD NOT** — strong recommendations with rare, justified, documented exceptions.
- **MAY** — genuinely optional.

A **default** is the shipped behavior. Every default carries a stated reversal condition. Where this document says "the setting X", the default value is given inline and is user-overridable unless marked otherwise.

---

## 0. Abstract, scope, and non-goals

Lattice is a desktop app that treats a folder of CommonMark files as the only source of truth. It layers a rebuildable index on top (links, backlinks, tags, typed properties, full-text search) and renders a graph view, with note cards at high zoom. The UI is iced, the editor is a custom widget over a headless buffer model, and everything below the UI is a headless library with a CLI.

The design target is to preserve note bytes across app failures, keep caches fully rebuildable, and visualize links as users build their knowledge base. The precise durability guarantees and accepted races are in D6 and §10.6.1. User settings and safety/transaction state have different deletion consequences (§1).

### Non-goals (deliberate)

- No proprietary format, no database-as-truth, no mandatory app-written metadata.
- No built-in sync or collaboration (no CRDT buffers). Users bring git, Syncthing, or iCloud. The design is *safe under* those tools, but does not implement them. (Closed: ADR-0002.)
- No real-time collaboration, and therefore no CRDT buffers. (Closed: ADR-0002.)
- No plugin host in this RFC. We build the seams (D16) and dogfood them. Tasks, daily notes, and templates will be first-party extensions through those seams later.
- No query DSL in this RFC. We build the query-friendly schema and a typed query AST (§4.3). A textual syntax is additive.
- No raw HTML rendering. Ever.
- **Mobile (iOS/Android) is out of scope.** iced's mobile targets are not a goal of this project. The crate boundary (§4.1) keeps a future mobile UI architecturally possible, but no mobile work is planned or budgeted.

### Interoperability posture: inspired by Obsidian, not a drop-in replacement

Lattice is inspired by Obsidian's feature set. It is **not** a drop-in replacement, and no compatibility beyond the list below is promised or implied.

| Surface | Posture |
|---|---|
| `[[wikilinks]]`, `![[embeds]]`, `#tags`, backlinks, graph, frontmatter properties | Shared syntax; first-class support |
| Frontmatter keys `aliases`, `tags`/`tag`, `title` | Read and mapped into the typed schema (§4.8) — free compatibility, taken |
| `.obsidian/` directory | **MUST NOT be read or written, ever.** It belongs to other apps |
| Obsidian plugins, themes, CSS snippets, Canvas | Not supported; no compatibility surface |
| Stable link form `[[id:…\|alias]]` | Opt-in (D8); vanilla Obsidian shows it unresolved. This cost is why it is opt-in |
| Vault opened by both apps | Supported by construction (we only read/write `*.md` + `.flavite/`), but no coordination protocol exists with Obsidian; the D7 conflict protocol is the safety net |

### Decided deferrals (not forgotten)

- *LaTeX rendering.* Rust has no mature math layout engine. Math spans MUST be parsed, excluded from link and tag extraction, and shown as styled source. Real rendering MAY arrive behind the `Renderer` trait (a typst-based renderer is the likely candidate). Reversal: a light, embeddable renderer shows up.
- *Real-time collaboration (CRDT buffers).* Closed as a non-goal. See ADR-0002.

---

## 1. Mental model: state classes

Most "lost my notes" bugs come from mixing these up, so every byte the app writes gets exactly one class:

| Class | Location | Lose it → | Examples |
|---|---|---|---|
| **Truth** | vault `*.md`, attachments | catastrophe | notes, images, PDFs |
| **User state** | `<vault>/.flavite/` settings and pins | annoying | pinned graph positions, vault settings |
| **Transaction state** | `<vault>/.flavite/journal/` | pending operations lose automatic recovery | rename manifests, replacement and undo payloads; retained until terminal |
| **Local safety state** | OS *data* dir, keyed by vault UUID | lose undo-of-last-resort | local history, dirty-buffer recovery files |
| **Cache** | OS *cache* dir, keyed by vault UUID + **schema major** | nothing, just a rebuild | SQLite index, tantivy segments, thumbnails |

Rename journals live in the vault because a half-applied multi-file rewrite is *truth-adjacent*. If the cache is wiped, the journal must still tell us what was in flight.

**The vault UUID is the linchpin.** History, recovery files, and the single-instance lock are all keyed by it, and it is minted into `<vault>/.flavite/vault.toml`. Since user state is *ignorable*, deleting `.flavite/` MUST NOT orphan the safety state. D2 specifies the vault registry that makes this true: a per-path record in the OS data dir lets the app re-mint the *same* UUID when `vault.toml` is missing. Deleting `.flavite/` while closed preserves access to local safety state through the registry, but MUST NOT be offered as cleanup while nonterminal journals exist. Removing those journals forfeits automatic completion; surviving note bytes and local history remain available. Only cache deletion promises a transparent rebuild.

OS directory layout (all paths keyed as shown):

```
<os-data>/flavite/roots/<root-key>.lock                      root ownership guard (§4.13)
<os-data>/flavite/vaults/<root-key>.toml                       vault registry/binding (D2)
<os-data>/flavite/vaults/<vault-uuid>/history/                 content-addressed zstd blobs (D6)
<os-data>/flavite/vaults/<vault-uuid>/recovery/<note-path-hash>.bin   dirty-buffer recovery (D6)
<os-data>/flavite/vaults/<vault-uuid>/instance.lock            single-instance advisory lock (§4.12)
<os-data>/flavite/vaults/<vault-uuid>/instance.ipc             focus-the-first-instance socket (§4.12)
<os-cache>/flavite/<vault-uuid>/v<schema-major>/               SQLite + tantivy (D2)
<os-cache>/flavite/<vault-uuid>/thumbs/<content-hash>.webp    attachment thumbnails (D15)
<vault>/.flavite/vault.toml                                    vault UUID (authoritative when present)
<vault>/.flavite/journal/<txn>.json                            versioned transaction manifest (D9)
<vault>/.flavite/journal/<txn>.data/                           durable replacement/undo payloads
<vault>/.flavite/graph.json                                     pinned positions (D15)
```

---

## 2. Blast radius & Technology stack

Greenfield doesn't mean undisturbed. The app **touches things it doesn't own**, and those are where the risk lives:

| Surface | How we disturb it | Handling |
|---|---|---|
| Other writers (git, Syncthing, vim, Obsidian, scripts) | They write while we have buffers open and an index live | D4, D5, D7 |
| Sync tools | Our temp files and any DB inside the vault would get synced | D2, D6 |
| git history | Frontmatter mutation or bulk rewrites create noise | D8, D11: lazy IDs, byte-preserving patches, opt-in rewrites |
| Obsidian compatibility | Same vault opened by both | §0: never touch `.obsidian/`; stable-link syntax opt-in (D8) |
| OS file-event APIs | inotify, FSEvents, and ReadDirectoryChangesW each lose or coalesce events differently | D5: events are hints, never truth |
| Filesystem semantics | Case-insensitivity, NFC/NFD, reserved names, symlinks, network FS, cloud placeholders | D17, §4.8 |
| Attachments | Large binaries hashed/thumbnail-decoded during scans | §4.3, §4.8: size caps, placeholder laziness |
| IME, clipboard, a11y APIs | A custom-drawn editor owns all of it | D18 |
| GPU/drivers | wgpu shader widget for the graph | Fallback path (D15) |
| Disk, memory | 50k-note vaults, 1 MB notes, 200k-edge graphs, 1 GB history store | Budgets in §8, D6 retention |
| Installers/updates | Cache schema across versions, downgrade | D2, §4.12 |
| Future plugin authors | Whatever we expose becomes a contract | D16 |
| Support/debugging | Users send "it ate my note" reports | §4.11: `doctor`, local logs, history |
| Second app instance | Two writers, two journals on one vault | §4.12: advisory lock + focus IPC in OS data dir only |

Crates:

- UI framework: iced-rs
- Text buffer: ropey
- Index storage. SQLite (rusqlite) for links/tags/metadata 
- Full-text search: tantivy
- Vault/storage: a folder of markdown files, with a file watcher (notify crate)
- Markdown parsing and rendering: pulldown-cmark



---

## 3. The decision spine

### D1. Files are truth; everything else is derived

- **Decision:** The index MUST be fully reconstructible from the vault. Deleting the cache MUST NOT lose user-visible data.
- **Tradeoff:** Cold rebuild costs time, and some features (graph layout, previews) must be recomputed or persisted as user state rather than cached "for free."
- **Reversal:** None expected. This is the product's identity. Only a first-party sync service with server-side state would force a rethink — and first-party sync is a closed non-goal (ADR-0002).

### D2. The index lives outside the vault; the vault UUID is recoverable

- **Decision:** SQLite and tantivy live in the OS cache dir, keyed `<vault-uuid>/v<schema-major>/`.
- **Decision (v2, R2; amended):** `vault.toml` is authoritative when well-formed; the per-root registry reconstructs the same UUID if it is absent. Startup first acquires the native-root ownership guard, selects identity without writing the vault, then acquires the UUID instance lock. Only after both locks may identity files be persisted or journals recovered (§4.13). Registry keys derive from native filesystem identity (§4.16.2), never a lossy/NFC-only pathname. Malformed identity or an existing UUID bound to another root returns `ActionRequired`; it never silently shares or overwrites another vault's safety state.

```text
root guard → read/select identity in memory → UUID lock → persist identity/binding
           → watcher + event buffer → recover journals → open cache → scan → replay
```

- **Why:** Syncing a live SQLite WAL across machines is a classic corruption generator. Keying by schema major gives free side-by-side downgrade safety: an old binary never opens a newer schema, it just builds its own. The registry closes the v1 hole where deleting the ignorable `.flavite/` re-minted the UUID and silently orphaned history, recovery files, and the lock — the undo-of-last-resort state — under the old UUID.
- **Tradeoff:** The index doesn't follow the vault between machines, so each machine rebuilds. Accepted. The registry adds a small binding per native root. A reused path, moved vault, or copied UUID requires the identity checks in §4.13; path reuse alone never authorizes attaching old recovery to unrelated notes.
- **Reversal:** If rebuild time for typical vaults exceeds ~60 s on the reference machine, add an *opt-in* exportable snapshot. Never a default.

### D3. Sync core on dedicated threads; single-writer actors; two event channels

- **Decision:** The core MUST NOT depend on an async runtime. SQLite, tantivy, ropey, and `notify` are blocking or callback-based anyway. Use named threads and bounded channels (`crossbeam`). iced `Subscription`s bridge channels to `Message`s. The index has **exactly one writer thread** owning the SQLite writer connection and the tantivy `IndexWriter`. Readers use a pool of read-only WAL connections.
- **Decision (v2, R5) — channel policy.** Events flow over two channels with opposite loss policies:

```rust
// Channel A — "pulse": lossy by design.
// Bounded (cap 1024), drop-oldest on overflow. Consumers MUST treat every pulse
// as a hint and re-query by generation; nothing requiring a reaction lives here.
enum Pulse { VaultChanged{ epoch: Epoch, generation: u64, added: u32, changed: u32, removed: u32 },
             ReconcileProgress{ done: u32, total: u32 },
             Invalidate{ note: NoteRef } }

// Channel B — "notice": lossless.
// Unbounded, but producers MUST coalesce per (note, kind) so at most one pending
// notice exists per key; the host acks presentation (display in the GUI,
// reporting in the CLI). This ack never completes a command or journal item.
// A conflict banner, a rename
// suggestion, or a recovery offer MUST NOT be dropped, ever.
enum Notice { RenameSuggested{ from: PathBuf, to: PathBuf, tier: Tier, inbound: u32 },
              Conflict{ note: NoteRef },
              RecoveryOffer{ note: NoteRef },
              ActionRequired{ code: &'static str, note: Option<NoteRef> } }
```

- **Why (R5):** v1 bridged everything over one bounded channel with no drop policy. Under bulk load (a `git checkout` over 20k files) the channel fills exactly when conflict notices are most likely to exist. Lossy telemetry and lossless user-action notices have opposite requirements and MUST NOT share a queue.
- **Tradeoff:** Hand-rolled actor plumbing, plus two subscription paths. Async-needing parts (future WASM host, network) get a facade at the edge.
- **Reversal:** If the plugin host or any network feature becomes first-class, add an async facade crate. Don't convert the core.

### D4. Consistency model

Three authorities, explicit relationships:

```
 DISK (truth) ──eventually──► INDEX (epoch E, generation N within E)
     ▲                              ▲
     │ save = compare-then-rename   │ read-your-writes for our own saves
     │                              │
 BUFFER (optimistic, has base_hash)─┘
```

- **Index vs. disk:** *eventually consistent.* Every index query returns `(results, epoch, generation)`. Single-file external changes SHOULD be visible within 500 ms p99. Bulk changes show progress and are consistent when done.
- **Own saves:** *read-your-writes.* The save path parses the exact bytes it wrote and applies them to the index **before** acking the UI. We don't wait for the watcher to tell us about our own write. For search specifically, read-your-writes is enforced by **commit-before-search** (D12): a query with uncommitted tantivy changes commits first, so a search MUST reflect every save acked before the query began.
- **Buffer vs. disk:** *optimistic concurrency with a base hash.* The final read observes bytes that are durably snapshotted before replacement. An uncoordinated write after that read can still be overwritten without entering history (D6). The window includes hashing, history persistence, and replacement; no fixed duration is promised.
- **Tradeoff:** The UI can show slightly stale backlinks after external edits. The residual overwrite window is accepted explicitly; history protects observed bytes, with no guarantee for later unobserved writes.
- **Reversal:** If a target platform offers a real atomic replace-if-unchanged primitive, use it.

### D5. Reconcile by idempotence; the watcher is a hint

- **Decision:** All change handling goes through `reconcile(scope, mode)`. `FastMetadata` scans may skip unchanged non-racy entries. `VerifyContent` hashes every eligible hydrated file in scope regardless of size/mtime equality. Watcher-nominated paths always use VerifyContent, including bulk nominees; overflow triggers a full VerifyContent pass. Only content-hash equality establishes a semantic no-op.
- **Startup:** acquire ownership, start the watcher and its event buffer, recover journals, then run the initial FastMetadata scan and replay buffered nominees using VerifyContent. Warm-open targets apply to that fast scan; verification runs in the background.
- **Silent loss:** a full VerifyContent pass starts at startup and at least every 10 minutes while available. Focus triggers a fast scan plus verification if overdue; it never repeatedly postpones a due verification. A changed same-size file with preserved old mtime is discovered by the next completed verification pass. Queue/concurrency limits preserve UI responsiveness; delayed/failed passes remain visibly overdue and retry. Unavailable drives never produce deletion purges.
- **Racy files:** the `mtime ≥ scan_time − 2 s` heuristic forces hashing even in FastMetadata mode; it is an optimization guard, not the silent-loss backstop. Touched/verified timestamps are tracked separately from semantic generations.
- **Idempotence:** self-save events rehash and compare equal to the index; origin tokens only classify UX. VerifyContent excludes ignored directories (including `.obsidian/`), never hydrates placeholders, and respects note/attachment budgets. Oversize attachments have metadata-only freshness; their unobserved content changes are outside the hashing guarantee.
- **Observability:** count drift found during verification and report last completed verification, overdue status, and unreadable/skipped files. Reversal: the 10-minute default may change with measured I/O costs; periodic content verification remains mandatory.

### D6. Saves are atomic, journaled by history, and crash-recoverable

- **Decision (atomic write):** temp file in the **same directory** (same mount, same ACL inheritance), write, copy permissions, `fsync`, rename-over (`rename(2)`; `ReplaceFileW` on Windows), `fsync` the directory. Symlinks are resolved so we write the target. We MUST NOT replace a symlink with a regular file. Preserve BOM, line endings (per-line LF/CRLF — see §4.8), and trailing-newline state.
- **Decision (v2, R1) — unconditional history snapshot.** Before any save that overwrites existing bytes, and before any destructive resolution (reload-discard, keep-mine, rename rewrites, merge), the prior bytes MUST be snapshotted to the content-addressed store (zstd, blake3-keyed) in the data dir. **The snapshot is unconditional — it is not gated on conflict detection.** This preserves the bytes actually observed by the final read, including conflicting external content present at that time. It cannot preserve an external write arriving after the read.
- **Decision (v2) — retention tiers.** v1's "30 days or 50 versions per note" is replaced, because with 1 s autosave a flat version count expires in minutes. Retention is time-tiered, with a per-note floor:
  - keep every version for 5 minutes;
  - then 1 per minute for 1 hour;
  - then 1 per hour for 30 days;
  - floor: **at least 50 distinct versions per note** within the window, subject to the per-vault cap;
  - hard cap: 1 GB per vault, LRU eviction by snapshot time.
  - Content addressing dedups identical bytes, so repeat saves of unchanged content cost one stat.
- **Decision (recovery):** dirty buffers are written to a recovery file every 2 s (default; **configurable**, `recovery_interval`), atomic, with their buffer identity and revision. A successful save retires only recovery state covered by its saved revision; recovery for newer edits MUST remain reachable (§4.5.3). After a crash, restore is offered via a Channel-B notice.
- **Tradeoff:** The unconditional snapshot doubles read I/O on the save path (one extra read of the prior file; the write is deduped and compressed). Save runs on the VaultActor, off the UI thread. Same-dir temp files can briefly appear to sync tools. History costs disk, bounded by the cap. All bounded; all accepted.
- **Residual risk:** an external write after our final read and before replacement may be absent from both disk and history. The interval includes durable history I/O; its duration depends on the storage system. The app makes no restore guarantee for these unobserved bytes. Save tests separate observed-before-read writes from this accepted race (AC-3/8).
- **Reversal:** If sync tools commonly propagate our temp files, move to `.flavite/tmp/` with EXDEV fallback.

### D7. One buffer authority per note, plus an explicit conflict protocol

- **Decision:** There's exactly one `Buffer` per open note in a `BufferManager`, owned by a headless `Session` in `flavite-vault`. `flavite-api::Lattice` exposes that session through the command registry and revisioned read views. The GUI hosts it on its UI thread; the CLI hosts the same session on its driving thread. All document mutations (typing, checkbox toggles, reloads, undo/redo, link rewrites) execute as session commands and apply `EditOp`s through its BufferManager. Widgets receive read-only views; ownership and the command protocol are defined in §4.5.
- **Decision (conflict protocol):** each buffer keeps `base_hash` (hash of what we last loaded or saved). Conflict banners are Channel-B notices.

| Buffer | Disk vs. base | Action |
|---|---|---|
| Clean | changed | Silent reload. Map cursor and scroll through the diff |
| Clean | deleted | Keep pane open as *Orphaned*, banner "file deleted" |
| Dirty | changed | **Conflict banner:** Reload (snapshot first) · Keep mine (snapshot first) · Save mine as conflict copy · Merge |
| Dirty | changed to *identical* content as buffer | Auto-resolve → Clean |
| Dirty | deleted | Recreate · Save as · Discard |
| Any | renamed (tier T0/T1/T2 only — D8) | Re-key buffer silently, no prompt |
| Conflicted | n/a | **Autosave MUST be disabled** and manual save blocked until resolved |

- **Merge:** three-way (base from the history store, mine, theirs) via `diffy` or `similar`. Unresolved hunks open a resolution UI. We MUST NOT write `<<<<<<<` markers to disk unless the user explicitly chooses that.
- **Tradeoff:** Autosave-off-during-conflict means a long-ignored banner leaves work unsaved. The recovery file covers that.
- **Reversal:** None.

### D8. Link identity, resolution, and tiered rename inference

*Correction from v1 stands:* an ID preserves **identity** (graph position, history, buffer association, tracking across renames). It does not preserve link *text*, because the link text is the stem. Link text needs one of three strategies:

1. **Rewrite** the inbound links (journaled; D9).
2. **Alias:** add the old stem to the renamed note's `aliases` frontmatter so old links keep resolving, with zero edits elsewhere.
3. **Stable form:** `[[id:01J9ZK…|Ideas]]`. Opt-in per link or per vault, immune to renames. The display text is the alias portion. Vanilla Obsidian shows it as unresolved, and that cost is why it's opt-in.

- **Defaults:** in-app rename → rewrite (setting: `rewrite | alias | none`, default `rewrite`). External rename → **never auto-rewrite other files.** Emit a Channel-B notice: *"Ideas → Concepts detected. 14 links affected."* [Update links] [Add alias] [Ignore].
- **Lazy IDs:** an ID (UUIDv7/ULID) is written to frontmatter only when needed: user requests stable links, ambiguity can't be resolved otherwise, or collision. We MUST NOT write IDs on import or scan, because mutating 20k untouched files means git noise and a self-write storm.
- **Resolution** (`target_key = NFC(casefold(stem))`):
  1. `id:` prefix → `stable_id` lookup.
  2. Path-suffix match if the target contains `/` (notes, then attachments).
  3. Stem match over `notes.stem_key ∪ aliases.alias_key ∪ attachments.stem_key`. For `![[embed]]`, an attachment match wins over a note match; for `[[link]]`, a note match wins.
  4. If multiple candidates → rank: same directory as source, then shallowest, then lexicographic. Mark **`ambiguous`**, pick deterministically, and surface it in Diagnostics. Never silent.
  5. Zero candidates → `unresolved`. Clicking a note-link creates the note. Clicking an embed whose target has an attachment-like extension offers *locate file*; clicking an attachment link opens it with the OS default application (default behavior).
- **Duplicate IDs:** retain every raw frontmatter ID, flag every member of a duplicate group, and choose the lowest native PathKey in deterministic byte order for lookup while reporting ambiguity. This rule is rebuild-stable and replaces earliest-first-seen. Never silently use a conflicted ID for T2 re-key or pin association; offer explicit mint-new-ID repair without automatic note edits (§4.16.2).
- **Decision (v2, R6) — tiered rename inference** (stolen from git's "renames are inferred, not recorded", now tiered by trust). Tiers decide not only link repair but **which side effects are silent**:

| Tier | Evidence | Buffer re-key | Pin carry-over | Link repair |
|---|---|---|---|---|
| **T0** | In-app rename command | yes, silent | yes, silent | journaled rewrite per setting (D9) |
| **T1** | OS rename event pair (inotify cookie / FSEvents flag / `RENAMED_OLD+NEW`) | yes, silent | yes, silent | notice; never auto-rewrite (external) |
| **T2** | Same `stable_id` on a vanished + created pair | yes, silent | yes, silent | notice; never auto-rewrite (external) |
| **T3** | Same content hash, size ≥ 64 B, exactly one candidate on each side; prefers same-directory candidates | **no** | **no** | notice only; user confirms |
| **T4** | Fuzzy similarity ≥ 0.9, exactly one candidate | **no** | **no** | notice only; user confirms |

  Otherwise it's delete + create, and links go unresolved and are reported rather than guessed.

- **Why (R6):** v1 let a T3 content-hash match — which is exactly what "copy this note into a new note" produces — silently move pinned graph positions and re-key buffers. Identity carry-over is only silent where the evidence is authoritative (T0–T2). T3/T4 require user confirmation for anything beyond a notification.
- **Tradeoff:** Three link strategies plus five inference tiers is a lot of surface, and the stable form fragments link syntax.
- **Reversal:** If stable links see almost no use after a release, hide them behind an advanced flag. Keep the parser support.

### D9. Rename/move rewrites are a journaled roll-forward transaction

- **Decision:** the transaction has distinct prepare and committed phases (§4.15). Persist a versioned preparing manifest, then durable replacement payloads and staged siblings. Publish the committed manifest only after all required payloads and their directory entries are durable. This is the roll-forward commitment point; a preparing transaction has changed no truth files and may be abandoned safely.

```text
PLAN → VERIFY → PREPARE → STAGE/PAYLOADS → COMMIT → APPLY → WAIT_BUFFERS → CLOSE
recover preparing: no truth mutation; discard only this transaction's staging
recover committed: restage from verified payload; apply only against expected base
recover waiting: restore checkpoint/operation metadata; await save or explicit action
```

- **Closed files:** verify raw source spans against the matching content hash; reparse/replan when stale. Recheck immediately before each replacement, snapshot the observed prior bytes, and honor D6's residual race. Existing rename destinations are rejected; v1 refuses cross-filesystem moves. Recovery classifies each item as base/new/externally diverged, retaining unresolved evidence rather than declaring success.
- **Open buffers:** the Session validates buffer identity/revision and applies one txn/item-tagged EditOp. Before acknowledging `buffer-applied`, persist a checkpoint containing its bytes, revision, and operation markers. An identical txn/item retry is idempotent. The correlated application reply, rather than a displayed notice, advances the journal (§4.5.4).
- **Undo:** the in-memory undo returns Pending until its post-undo snapshot and cancellation record are durable. A completed undo must never restore/reapply the cancelled patch after a crash. Before completion, recovery may restore the last durable checkpoint; no promise is made for an unacknowledged undo. Recovery uses explicit operation markers and checkpoint lineage, never text-hash equality alone. Redo creates a new operation identity.
- **Lifetime:** pending buffer saves, cancellations without a durable successor checkpoint, missing payloads, and externally diverged items keep the manifest/evidence alive. CLOSE requires terminal disposition of every item; history/payloads needed by a live transaction are pinned against cleanup/eviction (§4.15).
- **Tradeoff:** a temporary duplicate of replacement bytes and durable buffer checkpoints costs disk and I/O. Disk-full before COMMIT leaves truth unchanged; failure afterward keeps recoverable evidence and reports a partial/ActionRequired outcome. Reversal: none without a replacement protocol preserving these invariants.

### D10. One link grammar, two parsers

- **Problem:** the editor wants an incremental, error-tolerant parse (tree-sitter-markdown), and the indexer wants a spec-correct parse (pulldown-cmark). If they disagree about what's a link, users see a blue link that isn't a link, or the reverse.
- **Decision:**
  - `flavite-syntax` owns a small hand-written **inline scanner** for the extensions: `[[link]]`, `![[embed]]`, `#tag`, `^block-id`, `id:` form. It runs over text ranges that aren't code spans, code blocks, or math. It is pure and IO-free.
  - The indexer: pulldown-cmark determines block and inline structure, then the scanner runs on text events.
  - The editor: tree-sitter for block structure, highlighting, and **fenced-code injection** (its real payoff). The same scanner runs over the tree-sitter's non-code inline ranges.
  - Link semantics (*what's a link*) come from exactly one function. Tree-sitter never decides it.
- **Enforced by test:** a differential fuzz corpus asserting `editor_link_spans == indexer_link_spans` (§8).
- **Tradeoff:** We maintain a scanner, and tree-sitter's markdown is not fully CommonMark-compliant in edge cases. Cosmetic highlighting may differ from the semantic parse in weird documents.
- **Reversal:** If the differential fuzz keeps finding divergences, drop tree-sitter for markdown structure and highlight from pulldown-cmark offsets. Keep tree-sitter only for code-block injection.

### D11. Frontmatter edits are byte-preserving patches, never re-serialization

- **Decision:** read frontmatter with a YAML 1.2 core-schema parser with alias and depth limits. Write by **textual patch** (insert or replace a single key's line range). We MUST NOT parse-and-reserialize the block.
- **Why:** re-serializing destroys comments, key order, quoting style, and anchors, which means git noise and angry users. YAML 1.2 core also avoids the "Norway problem" (`no` stays text, not `false`).
- **Types:** `bool | number | date (ISO) | datetime | text | list | null`. Dates are inferred from ISO shape only.
- **Decision (v2, R13) — well-known key mapping.** On parse, the keys `aliases` (string or list), `tags`/`tag`, and `title` are mapped into the typed tables (`aliases`, `tags` with `source='frontmatter'`, `title` override). All other keys land in `properties`. `notes.title` derivation: frontmatter `title` if present, else first H1, else stem. On *write*, the only conventional key we ever synthesize is `aliases` (the D8 alias strategy); everything else patches user-authored text.
- **Tradeoff:** The patcher is finicky, with multi-line scalars and flow vs. block lists. Anything it can't safely patch → refuse and ask the user to edit by hand.
- **Reversal:** If a lossless YAML editing crate matures, adopt it behind the same trait.

### D12. Index: SQLite is the record; tantivy is a derived search view with deferred commits

- **Decision:** SQLite (WAL) holds everything structural. Tantivy holds only searchable text and ranking (notes only — titles, aliases, tags, body text; attachment *filenames* are searched via SQLite stem keys).
- **Decision (v2, R4) — two generations, deferred commit.** SQLite's `index.generation` bumps inline on every applied change and is authoritative. A second counter, `tantivy.committed_generation`, records what tantivy has actually committed. Document adds/deletes are applied to the tantivy `IndexWriter` immediately (in-memory, cheap), but `commit()` is **deferred**:
  - a search query with uncommitted changes commits first (**commit-before-search** — this is what preserves read-your-writes for search, D4);
  - otherwise, commit at most every 5 s while dirty (timer on the IndexActor);
  - at the end of a bulk reconcile, and on clean shutdown.
  - On open: use the checkpoint published atomically with the search commit, not an independently updated SQLite counter. Replay complete invalidations/tombstones from the bounded change log and current cached search documents; missing log coverage, epoch mismatch, an ahead checkpoint, or unreadable search state rebuilds tantivy only (§4.14).
- **Why (R4):** v1 implied a tantivy commit per save. With 1 s autosave that is a commit/merge cycle per idle tick on a 20k-document index — a serious perf cliff. Deferral with commit-before-search keeps search exactly read-your-writes while bounding commit frequency. Worst-case staleness without a query is 5 s; with a query, zero.
- **Tokenization:** tantivy's default tokenizer doesn't segment CJK. We use `SimpleTokenizer → LowerCaser → diacritic folding`, with **bigram tokenization for Han/Kana/Hangul runs**.
- **Hybrid queries** ("tag:project AND text:'foo'"): SQLite narrows to a `note_id` set. If ≤ 10k ids, pass it to tantivy as a filter. Otherwise intersect afterward.
- **Tradeoff:** Two stores can drift, and we've added a recovery path for it. Two generation counters instead of one. Binary size and compile time grow.
- **Reversal:** If drift incidents show up in `doctor` telemetry, or CJK quality is mediocre, collapse to SQLite FTS5 (ICU tokenizer) and delete tantivy. The `SearchBackend` trait makes this a swap.

### D13. The index is query-friendly from day one

Typed properties, hierarchical tags, aliases, attachments, and an extension-facts table for future plugins. Schema in §4.3. Internal query API is a typed AST, not SQL strings. Plugins MUST NOT receive SQL access (injection, locking, and resource exhaustion).

### D14. The editor: custom widget over a headless model

*Correction from v1 stands: ropey stays first-class; tree-sitter's role is as defined in D10. Q1 is closed (ADR-0001): iced's current text primitives cannot provide caret geometry, IME preedit access, and custom-span hit-testing to the required standard, so a custom widget is the **decision**, not a default pending a spike.*

- **Decision:** `flavite-buffer` contains `EditorModel`: ropey text, multi-selection, undo **tree** with time-based coalescing, edit ops as `(range, replacement)`, and the D7 state machine. It has zero UI dependencies and is exhaustively testable headless. `flavite-editor` is an iced widget implementing `EditorSurface` over it, using iced's advanced text primitives (cosmic-text shaping, hit-testing, cursor geometry).
- **Decision — the `EditorSurface` trait is retained, but the stock `text_editor` is NOT a fallback for v1.** The trait exists for testability and substitution in tests; the fork it was hedging is closed.
- **Why custom:** the stock `text_editor` owns its own buffer and exposes limited hooks. We need: wikilink autocomplete anchored to the caret, ctrl/cmd-click on links (hit-testing against scanner spans), conflict and diagnostic decorations, find-in-note highlights, tree-sitter injection highlighting, deterministic undo grouping, and one buffer shared by many views (D7).
- **Residual risk (escalation path):** a custom widget still depends on iced/winit delivering IME preedit/commit/candidate events to it. Spike S1 verifies this per OS. If it cannot be made to work: upstream patches → fork iced → (last resort) renegotiate the pure-Rust constraint. Abandoning pure Rust is the last resort, not the first.
- **Large files:** layout and highlight are viewport-scoped. Highlight work is chunked with a time budget per frame. A 1 MB note MUST stay interactive.
- **Tradeoff:** This is the project's largest single cost, and IME, selection, bidi, and accessibility are now ours.
- **Reversal:** ADR-0001 records the closure and its reversal trigger.

### D15. Graph: level-of-detail rendering, off-thread layout, one buffer authority

```
IndexActor ─edge/node deltas─► GraphModel (CSR adjacency) ─► LayoutWorker (rayon, ≤ cores−1)
                                                                 │ Barnes-Hut ForceAtlas2
                                              PositionBuffer (double-buffer, atomic Arc swap)
                                                    │                          │
                                       wgpu `shader` widget            SpatialGrid (hit-test)
                                       instanced dots + lines
                                                    │
                          zoom/visible-count threshold → CardLayer (≤ 64 real widgets)
```

- **Layout algorithm:** ForceAtlas2-style (degree-weighted repulsion + gravity + adaptive per-node speed). Knowledge graphs have hub-heavy degree distributions, and plain Fruchterman-Reingold turns those into hairballs. Barnes-Hut θ≈0.8. New nodes spawn at their neighbors' centroid plus jitter (warm start). Layout cools and **freezes**, and only wakes on topology change.
- **Rendering:** zoomed out, instanced wgpu draws with degree-based sizing, edge alpha by zoom, and edge count capped by importance. The render thread only swaps an `Arc` and never waits on layout. No locks on the frame path.
- **Nodes include attachments** (v2): attachment nodes render as squares, sized by degree; edges from `![[embeds]]` included. Thumbnails inside graph cards are a MAY for v1; the default is a colored square + filename. Read-mode embeds render full images (§4.6).
- **Zoomed in (the "in-node features"):** a custom `Zoomable` container lays out real iced widgets at world positions. Text is **re-laid-out at the scaled font size** (quantized scale steps, cached), not GPU-scaled. Scaled glyph textures get blurry. This is the same idea as map tiles vs. vector labels. Card content uses indexed previews until a buffer exists, then its revisioned view. Checkbox toggles dispatch session commands with the displayed buffer identity/revision; indexed task spans also carry their source content hash. The Session opens an unopened note through BufferManager and validates these guards before applying the edit (§4.5.2). Stale cards refresh before retry; a disk `base_hash` alone cannot validate positions in a dirty buffer.
- **Pinned positions** are user state (`.flavite/graph.json`), keyed by `stable_id` if present, else by path with carry-over **only from tiers T0–T2** (D8). A cache must never hold anything the user *did*.
- **Fallback:** the `shader` widget needs wgpu. On tiny-skia (software) renderers, the graph falls back to `canvas` with a node cap and a visible "reduced mode" notice.
- **Tradeoff:** A custom wgpu pipeline plus a custom zoomable container, which is the second-hardest part of the project.
- **Reversal:** If S2 shows 50k nodes can't hold 60 fps on integrated GPUs, lower the supported-scale target (e.g., 20k) and aggregate clusters at low zoom, rather than complicating rendering.

### D16. Plugin readiness is a dogfooding rule, not a promise

- **Decision:** `flavite-ui` MUST talk to the core only through `flavite-api`, the same types a plugin host would later expose (commands, events, queries, capability checks). First-party panels (backlinks, graph, search) are consumers of that API. If we need a private back-door, that's a design bug to fix in the API.
- **Contracts in v1:** command registry with explicit execution/results (§4.5), versioned event bus, `Query` AST, and extension-fact writes (`ext_facts`). A future WASM host (wasmtime, component model) maps onto these. Capabilities (`ReadNotes`, `WriteNotes{scope}`, `Ui::Panel`, `Net{allowlist}`) are declared now even though nothing enforces them yet. All consumers, including future plugins, submit document mutations to the same Session command path through BufferManager.
- **Tradeoff:** Extra indirection and a stable-ish API to maintain. Marked `unstable` until a host exists.
- **Reversal:** If the API calcifies around guesses, bump the API major freely while it's unstable. That's the point of labeling it.

### D17. Trust boundary: the vault is hostile input

Vaults arrive from git clones, shared drives, and downloads. Everything inside is untrusted. Threats and controls are in §4.10. The one-line version: **no vault content can make us read or write outside the vault, execute anything, or consume unbounded resources.** This applies to note text *and* to attachment filenames/paths.

### D18. Platforms, IME, accessibility

- **Decision:** Linux (X11 + Wayland), macOS, Windows all supported day one, with CI on all three. Mobile is a non-goal (§0).
- **IME correctness is a MUST** (CJK users can't use a broken editor). Composition (preedit) rendering, commit, and candidate-window positioning are acceptance-tested per platform. S1 verifies that iced/winit can deliver these events to a custom widget on all three OSes.
- **Screen reader support is a SHOULD** gated on Spike S1. Path: AccessKit integration exposing text-range nodes. If it's missing, we either contribute upstream or document the gap honestly.
- **Tradeoff:** Accessibility may trail, and that's a real cost to real users.
- **Reversal:** If S1 shows a viable AccessKit path, promote screen reader support to MUST.

---

## 4. The design

### 4.1 Crates (dependency arrows point down)

```
flavite-cli ─┐                 flavite-ui (iced app) ──► flavite-editor (iced widget)
             │                        │                        │
             ▼                        ▼                        ▼
        ┌─────────────────────  flavite-api  (commands, events, queries, caps; serde) ──┐
        ▼                                                                               ▼
   flavite-vault  (Session + actors: reconcile, journal, history, recovery)          flavite-graph (layout, spatial)
        ├──► flavite-index   (sqlite, tantivy, resolver, query exec)
        ├──► flavite-buffer  (EditorModel/ropey, undo tree, D7 state machine)
        ├──► flavite-syntax  (scanner, frontmatter patcher, pulldown glue) — pure
        └──► flavite-fs      (trait Fs, atomic_write, watcher wrapper, path safety, FaultFs)
```

**Rule:** nothing below `flavite-ui`/`flavite-editor` may depend on `iced`. Enforced transitively by the architecture check (§10.2), which reads `cargo metadata` — `cargo-deny` cannot express layering rules. That keeps ~80% of the system testable without a GPU.

### 4.2 Runtime topology

```
GUI host (UI thread) / CLI host (driving thread)
  Lattice registry → Session [BufferManager, pending operations]
    │ local edit → immediate revision/result
    │ worker effects ⇅ correlated requests/replies (lossless)
    ▼
  VaultActor [watcher, Fs, write tokens, journal, history]
    ├─ ParsePool (rayon, catch_unwind)
    └─ IndexActor [sole SQLite + tantivy writer, read pool]
         └─ graph feed → LayoutWorker → PositionBuffer → GUI graph view

Pulse/notice subscriptions → host presentation and revisioned re-query
Operation replies → Session state transition → correlated command result
```

Workers run behind a `catch_unwind` boundary. A panic in parsing one file marks that file `parse_ok=0` and moves on. It never takes down the actor or touches a buffer.

flavite-syntax owns pure TextSnapshot/coordinate types (§4.16.1); flavite-buffer
depends on syntax for those types, so C10 follows C6. The host pumps Session
inputs and worker effects in both GUI and CLI modes.
The Session is headless, single-owner state; it adds no actor or async runtime.
Local edits run in the caller's turn, while serialization, disk I/O, and index
work run on workers over immutable snapshots. Workers never acquire mutable
buffer access. The GUI's iced Tasks/Subscriptions adapt the headless protocol;
iced types MUST NOT appear in its command/result types. Ordering is in §4.5.

### 4.3 Data model

Notes are files with the `.md` extension (**default; fixed for v1**). Attachments are files with extensions in the fixed map below; all other files are ignored by the index (present in the vault, untouched).

```
images:  png jpg jpeg gif webp svg avif bmp ico
pdf:     pdf
audio:   mp3 wav ogg flac m4a
a/video: mp4 webm mov mkv
archive: zip tar gz tgz
```

```sql
PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);
 -- schema_version, parser_version, vault_uuid, epoch, generation, log_floor,
 -- tantivy_committed_generation (advisory; search commit checkpoint is authority)

CREATE TABLE notes(
  note_id      INTEGER PRIMARY KEY,           -- cache-internal; never reuse within epoch
  path         TEXT NOT NULL,                 -- display only; never used for IO
  path_native  BLOB NOT NULL UNIQUE,           -- reversible native PathKey (§4.16.2)
  path_key     TEXT NOT NULL,                 -- normalized lookup; collisions allowed
  stem_key     TEXT NOT NULL,                 -- NFC + default casefold of stem
  title        TEXT NOT NULL,                 -- frontmatter title > first H1 > stem (D11)
  stable_id    TEXT,                          -- raw frontmatter ID; duplicates retained
  id_conflict  INTEGER NOT NULL DEFAULT 0,
  content_hash BLOB NOT NULL,                 -- blake3, 32 B
  size         INTEGER NOT NULL,
  mtime_ns     INTEGER NOT NULL,
  scan_ns      INTEGER NOT NULL,              -- for racy detection (D5)
  frontmatter  TEXT,                          -- JSON
  preview      TEXT,                          -- ≤1 KB, for cards/switcher
  parse_ok     INTEGER NOT NULL DEFAULT 1,
  generation   INTEGER NOT NULL
);
CREATE INDEX notes_stem ON notes(stem_key);
CREATE INDEX notes_path_key ON notes(path_key);
CREATE INDEX notes_stable_id ON notes(stable_id);

CREATE TABLE attachments(                    -- v2 (R3): attachments are first-class
  att_id       INTEGER PRIMARY KEY,
  path         TEXT NOT NULL,                 -- display only; never used for IO
  path_native  BLOB NOT NULL UNIQUE,           -- reversible native PathKey (§4.16.2)
  path_key     TEXT NOT NULL,                 -- normalized lookup; collisions allowed
  stem_key     TEXT NOT NULL,
  kind         TEXT NOT NULL CHECK(kind IN ('image','pdf','audio','video','archive','other')),
  mime         TEXT,
  size         INTEGER NOT NULL,
  content_hash BLOB,                          -- NULL when > ATTACHMENT_HASH_MAX (64 MB)
  width        INTEGER,                       -- images, decoded under budget (§4.10)
  height       INTEGER,
  mtime_ns     INTEGER NOT NULL,
  scan_ns      INTEGER NOT NULL,
  hydrated     INTEGER NOT NULL DEFAULT 1,    -- 0 = cloud placeholder, not downloaded
  generation   INTEGER NOT NULL
);
CREATE INDEX attachments_stem ON attachments(stem_key);
CREATE INDEX attachments_path_key ON attachments(path_key);

CREATE TABLE aliases(note_id INTEGER REFERENCES notes ON DELETE CASCADE,
                     alias TEXT, alias_key TEXT, PRIMARY KEY(note_id, alias_key));
CREATE INDEX aliases_key ON aliases(alias_key);

CREATE TABLE links(
  src        INTEGER NOT NULL REFERENCES notes ON DELETE CASCADE,
  ord        INTEGER NOT NULL,
  kind       TEXT NOT NULL CHECK(kind IN ('link','embed')),
  dst_raw    TEXT NOT NULL,                   -- exactly as written
  target_key TEXT NOT NULL,                   -- normalized target for (re)resolution
  anchor     TEXT,                            -- heading or ^block
  alias      TEXT,
  by_id      INTEGER NOT NULL DEFAULT 0,      -- stable-form link
  span_start INTEGER NOT NULL, span_end INTEGER NOT NULL,  -- SourceByte, notes.content_hash
  dst_note   INTEGER REFERENCES notes ON DELETE SET NULL,
  dst_att    INTEGER REFERENCES attachments ON DELETE SET NULL,   -- v2 (R3)
  res_state  TEXT NOT NULL CHECK(res_state IN ('resolved','unresolved','ambiguous')),
  PRIMARY KEY(src, ord));
CREATE INDEX links_dst        ON links(dst_note);
CREATE INDEX links_dstatt     ON links(dst_att);
CREATE INDEX links_unresolved ON links(target_key) WHERE res_state <> 'resolved';
CREATE INDEX links_target ON links(target_key); -- invalidate resolved candidates too

CREATE TABLE tags(note_id INTEGER REFERENCES notes ON DELETE CASCADE,
                  tag TEXT NOT NULL, source TEXT CHECK(source IN ('inline','frontmatter')),
                  PRIMARY KEY(note_id, tag, source));
CREATE INDEX tags_tag ON tags(tag);   -- "a/b" prefix query = range scan [ 'a/', 'a0' )

CREATE TABLE properties(
  note_id INTEGER REFERENCES notes ON DELETE CASCADE,
  key TEXT NOT NULL, ord INTEGER NOT NULL DEFAULT 0,          -- ord for list items
  vtype TEXT NOT NULL CHECK(vtype IN ('text','num','bool','date','datetime','null')),
  v_text TEXT, v_num REAL, PRIMARY KEY(note_id, key, ord));
CREATE INDEX props_text ON properties(key, v_text);
CREATE INDEX props_num  ON properties(key, v_num);

CREATE TABLE ext_facts(                       -- for first-party extensions / future plugins
  note_id INTEGER REFERENCES notes ON DELETE CASCADE,
  ns TEXT NOT NULL, key TEXT NOT NULL, value_json TEXT NOT NULL,
  PRIMARY KEY(note_id, ns, key));

CREATE TABLE search_docs(                    -- exact indexed source, rebuildable cache
  note_id INTEGER PRIMARY KEY REFERENCES notes ON DELETE CASCADE,
  content_hash BLOB NOT NULL, body_text TEXT NOT NULL);
CREATE TABLE changes(                        -- no cascading FK; deletion records survive
  generation INTEGER NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('note','attachment','outgoing')),
  entity_id INTEGER NOT NULL,
  PRIMARY KEY(generation, kind, entity_id));
```

Why `dst_raw`/`target_key`/`dst_note` are separate: resolution is derived. Creation, deletion, aliases, ID changes, and renames invalidate all potentially affected targets, including previously resolved links whose preferred candidate or ambiguity changed. Re-resolution and source-node/outgoing invalidations commit in the same structural transaction (§4.14). Attachment changes follow the same rule.

```rust
// Typed query AST (the only way in; no SQL for callers)
enum Query {
    Tag { name: String, hierarchical: bool },
    Prop { key: String, op: Cmp, value: Value },
    LinksTo(NoteRef), LinkedFrom(NoteRef), Unresolved,
    Text(String),                       // tantivy
    Attachment { kind: Option<AttKind>, name: Option<String> },
    And(Vec<Query>), Or(Vec<Query>), Not(Box<Query>),
}
struct Page<T> { items: Vec<T>, epoch: Epoch, generation: u64, next: Option<Cursor> }
```

### 4.4 Core algorithms

**Atomic save** (v2: staged before the race window; snapshot unconditional — R1)

```rust
// Worker-side I/O outline. The Session supplies an immutable SaveRequest and
// classifies failures by the last completed stage (§4.5.3); errors and `?` below
// abbreviate that stage-aware wrapping/propagation.
fn save(fs: &dyn Fs, request: SaveRequest) -> Result<SaveReceipt, SaveFailure> {
    let (path, bytes, expect_disk) = request.content();
    let real = fs.resolve_symlink(path)?;                 // never clobber a symlink
    let tmp  = tmp_sibling(&real);                        // ".<name>.flavite-tmp.<rand>"
    // 1. Stage the new bytes before reading current disk content.
    fs.write_new(&tmp, bytes)?; fs.copy_mode(&real, &tmp)?; fs.fsync(&tmp)?;
    // --- overwrite race begins: durable history I/O remains in this interval ---
    let prior = fs.read_if_exists(&real)?;                // bounded read of observed bytes
    if let Some(p) = &prior { history.put(p)?; }          // UNCONDITIONAL snapshot (R1):
                                                          // preserves observed prior bytes only
    let cur = prior.as_deref().map(blake3);
    if !expect_disk.matches(cur) {                       // present hash or absent-file guard
        let _ = fs.remove(&tmp);                          // best-effort temp cleanup
        return Err(Conflict { current: cur });
    }
    tokens.expect(real, blake3(bytes), now() + 5s);       // classify only (D5)
    fs.rename_replace(&tmp, &real)?;                      // ReplaceFileW on Windows
    fs.fsync_dir(parent(&real))?;
    // --- overwrite race ended at rename; durability is now established ---
    let generation = index.apply_inline(&real, bytes)?;   // read-your-writes (D4)
    Ok(SaveReceipt { request_id: request.id, buffer: request.buffer,
                     saved_revision: request.revision, hash: blake3(bytes),
                     epoch: index.epoch(), generation })
}
```

Before replacement, a failure leaves the app's original destination untouched. A replacement/fsync error may leave an uncertain disk outcome; Session reports that stage and reconciles before retry (§4.5.3). Under the supported persistence model (§10.6.1), recovery yields old or new complete bytes; unsynced operations and storage that cannot meet that model receive explicitly weaker guarantees. History contains the prior bytes actually read, including conflict-branch bytes; later external writes remain the accepted D6 race. Orphan cleanup must retain files referenced by live journals.

**Reconcile** (v2: scope covers notes + attachments)

```rust
fn reconcile(scope: Scope, mode: ReconcileMode) {
    let disk = stat_scan(scope);                           // stat only, no reads
    for p in disk.keys() ∪ index.paths(scope) {
        match (disk.get(p), index.get(p)) {
            (Some(d), Some(i)) if mode == FastMetadata && d.size==i.size
                && d.mtime==i.mtime && !i.racy() => continue,
            (Some(d), i) => {
                let bytes = read_capped(p, MAX_NOTE_BYTES)?;       // budget, §4.10
                let h = blake3(&bytes);
                if i.map(|i| i.hash) == Some(h) { index.touch(p, d); continue; }  // idempotent
                changed.push(parse(p, &bytes));
            }
            (None, Some(i)) => vanished.push(i),
            _ => {}
        }
    }
    // attachments: same loop shape over the attachments table; content hashed only
    // when size ≤ ATTACHMENT_HASH_MAX (64 MB); cloud placeholders are never hydrated.
    let renames = infer_renames(&vanished, &changed);      // D8 tiers T1–T4
    index.apply_batch_and_resolve(changed, vanished, renames); // one SQLite txn:
        // structural rows + resolver updates + generation + change invalidations
        // search_docs captures exact parsed bytes; tantivy commit deferred (D12)
    pulse.send(Pulse::VaultChanged { epoch, generation, summary });
}
```

**Event coalescing:** events are keyed by path with a 300 ms quiet window, extended while events keep arriving (max 2 s). If more than 500 events arrive within 1 s, enter **bulk mode**: suspend per-event UI updates, run one full reconcile, show progress on the pulse channel. A `git checkout` over 20k files is the design case, not an edge case.

**Vault open (v2)**

```rust
fn open_vault(fs: &dyn Fs, root: &Path) -> Vault {
    let root_guard = acquire_root_guard(fs, root)?;       // no vault mutation yet
    let candidate = select_identity_read_only(fs, root)?;
    let lock = acquire_uuid_lock_or_focus(candidate)?;    // verifies root binding
    persist_identity_and_binding(fs, root, candidate)?;   // both guards held
    let watcher = start_watcher_and_buffer(root)?;
    recover_journals(fs, root)?;                          // ownership precedes recovery
    let index = open_or_rebuild_index(candidate)?;        // epoch/checkpoint checks
    reconcile(Full, FastMetadata);
    replay_buffered_paths(VerifyContent);
    schedule_full_verification();
    Vault { root_guard, lock, watcher, index }
}
```

### 4.5 Commands and events

```rust
pub struct CommandSpec { pub id: &'static str,          // "note.open", "link.insert", "graph.focus",
                         pub title: &'static str, pub args: ArgSchema,
                         pub when: Option<Predicate>, pub keys: &'static [Chord] }
pub struct Invocation { pub request_id: RequestId, pub id: CommandId,
                        pub args: serde_json::Value, pub origin: Origin,
                        pub preconditions: Preconditions }

// Host: Invocation → registry validation → typed Session command → Dispatch
// GUI: map returned effects/completions to iced Tasks/Messages at the boundary.
// Palette, keybindings, menus, graph clicks, and (later) plugins ALL go through here.

#[non_exhaustive] #[derive(Serialize)] #[serde(tag="type", content="data")]
pub enum Event {                                          // versioned, API_VERSION in handshake
    VaultChanged{ epoch: Epoch, generation: u64, added: u32, changed: u32, removed: u32 },
    BufferStateChanged{ note: NoteRef, buffer: BufferId, revision: Revision, state: BufferState },
    ExternalChangeDetected{ note: NoteRef, kind: ChangeKind },
    RenameSuggested{ from: PathBuf, to: PathBuf, tier: Tier, confidence: Confidence, inbound: u32 },
    LinksBroken{ count: u32, candidates: Vec<RepairHint> },
    Diagnostic{ level: Level, code: &'static str, note: Option<NoteRef> },
}
```

`Event` is the wire form. Internally, events are split across the pulse/notice channels per D3: `VaultChanged` and progress ride the pulse channel; `RenameSuggested`, conflict banners, and recovery offers ride the notice channel. The `Event` enum is the versioned union a future plugin host subscribes to.

#### 4.5.1 Ownership and dispatch

`flavite-vault::Session` owns BufferManager, pending operations, and worker
handles. `flavite-api::Lattice` owns a Session and the command registry. The
headless session engine lands with C8/C10; C14 adds the public registry/facade.
The pre-C14 CLI drives the same engine directly, then migrates onto the facade.
There MUST be one Session per open vault in the owning process. Document views
share its buffers, including notes opened temporarily for graph/task edits.

The registry validates `CommandSpec.args` and converts JSON at the boundary to
typed commands with explicit preconditions and results. The built-in editor
MAY use a typed dispatch entry to avoid JSON allocation per keystroke; it MUST
share the registry's validation, handlers, and ordering. The inventory includes
document open/close/create, edits, undo/redo, save, conflict resolutions, recovery
restore/discard, rename/repair, and task toggles. Register each handler only when
its owning slice implements it; absent handlers return `Unavailable` with no
effects. C14's initial facade does not require future handlers to be stubbed.

Presentation commands such as `graph.focus` produce typed host effects and
never introduce UI types into the core. A headless host returns `Unsupported`
for effects requiring a GUI. Commands that need user input return
`ActionRequired` with actionable data; the CLI reports this and exits nonzero
unless explicit arguments resolve it. Hosts MUST NOT silently choose a
destructive conflict action. Queries return data without document mutation.

#### 4.5.2 Ordering and preconditions

All commands, external-change observations, and worker completions enter the
Session on its driving thread and are processed one at a time in observed order.
A local edit returns its new revision synchronously; it never waits for an I/O
round trip. Each opened buffer has an opaque instance identity (independent of
path and cache note_id) and a monotonically increasing revision. Reopen creates
a new identity; T0–T2 re-key preserves it. Undo, redo, reload, and journal patches
advance revision even if text returns to previous bytes. Preconditions are
checked when the Session executes the command, rather than only at dispatch.

Position-based edits carry buffer identity + expected revision; disk writes
also carry `base_hash` (or an explicit absent-file precondition for creation).
A stale edit returns `StaleRevision` without applying any part. Multi-selection
edits validate and apply as one command/undo unit against one revision. Commands
from indexed task spans carry the indexed source hash; after opening the note,
the Session verifies it against current buffer content before locating/applying
the task edit. Existing dirty buffers always supply the authoritative view.

Per-buffer disk mutations are serialized: at most one save is in flight, later
save requests coalesce to the newest requested revision. Superseded callers
receive an explicit `Superseded { by_request }` terminal result, without a save
success claim; every admitted request remains accounted for. Rename/close or
destructive conflict resolution waits for the relevant mutation to settle.
External observations are checked against the latest known disk base; an
observation predating an acknowledged save MUST NOT reload it over newer text.
Unknown ordering triggers a fresh reconcile/read through the workers. Blocking
waits on the host thread and worker-to-host callback cycles are forbidden.

#### 4.5.3 Completion, saves, and partial failure

Dispatch returns either a completed local result, a rejected command with no
effects, or `Pending { request_id, effects }`. `Pending` means accepted for
execution; callers await its correlated terminal result. Command results are
delivered directly to the caller and MUST NOT rely on a pulse or notice ack.
The command contract states its completion milestone. In particular:

| Operation | Successful completion means |
|---|---|
| Edit / undo / redo / task toggle | Applied to the authoritative buffer at the returned revision; disk remains unchanged until save |
| Save | Replacement and the platform's declared durability steps completed, and the exact saved bytes reached the structural index; receipt includes saved revision, hash, epoch, and generation |
| Rename / link repair | Its journal protocol reached the reported state; outstanding buffer saves, skipped items, and partial outcomes are explicit in the result |
| Presentation effect | The host handled it; this establishes no document durability |

Undo/redo involving a journal obligation returns Pending until the required
durable journal update completes (D9). An ordinary local completion is valid
only when no such obligation remains; the host never blocks waiting on fsync.

A SaveRequest captures request id, buffer identity, revision, disk precondition,
and an immutable content/EOL snapshot. Serialization runs off the host thread.
Its receipt identifies that same snapshot. On successful completion, update the
disk base to the saved hash; mark Clean only if the current revision still equals
the saved revision and no newer external conflict is unresolved. Newer edits
remain Dirty and retain recovery coverage. Stale completions for a closed or
reopened buffer MUST NOT alter its replacement. Recovery writes and retirement
are serialized per buffer so an older save cannot delete a newer recovery file.

Failures carry request/buffer/revision, last completed stage, and the observed
disk outcome: `NotReplaced`, `ReplacedDurabilityUncertain`,
`ReplacedIndexPending`, or `OutcomeUnknown`. A failed index update after durable
replacement schedules index repair against the actual saved bytes and preserves
recovery state; it does not repeat the disk overwrite. A retry of save checks
current disk state first. An uncertain replacement/fsync outcome requires a
worker read/reconcile before another mutation. No failure returns the ordinary
successful save receipt or claims read-your-writes before the index applies.
History failure before replacement aborts the overwrite. Report these states
consistently in GUI and CLI; the operation result and retry rules belong to the
Session even when presentation is absent.

#### 4.5.4 Operation transport, events, and notices

Session effects and worker requests/replies use a separate, bounded, lossless
operation transport with correlation ids. Buffer patch requests include txn/item
ids and expected revision; application replies include resulting revision.
Workers mark journal progress only after the matching application reply. Apply
backpressure at worker/admission boundaries: the GUI thread never blocks on a
full queue, and unadmitted work returns `Busy` without side effects. Once admitted,
an operation remains in the pending ledger until a terminal result or explicit
worker-loss outcome is recorded; queue pressure cannot drop it. Retry duplicate
txn/item requests idempotently within the live session. Crash replay continues
to use D9's durable journal/recovery protocol.

Events describe resulting state and carry revisions/generations as appropriate;
lossy pulses prompt re-query. Notices request user attention and coalesce by D3.
Notice presentation acknowledgements neither execute edits nor commit, cancel,
or complete operations. No transactional request or completion travels through
the coalesced notice queue. Both hosts pump operation traffic while work is
pending; the CLI drains or reports outstanding work before exit. Shutdown/close
MUST preserve dirty buffers in recovery or obtain an explicit discard decision.
The Session owns autosave eligibility/deadlines across all its dirty buffers;
GUI timers submit clock ticks, including for notes edited only through cards.
CLI commands promising a disk mutation must drive the required save to its
terminal result before reporting success; a buffer-edit result alone is insufficient.

#### 4.5.5 Verification and ownership of the contract

C10 proves revision/precondition and atomic edit behavior; C8 establishes the
Session/worker transport; C11 proves save/completion/recovery ordering; C12
proves correlated journal patch application. C14 verifies their public command
facade and CLI behavior, with consumer checks in A7, B7, and D1/D5. Required
scenarios include edit during save, stale card edit against a dirty buffer,
duplicate patch delivery, delayed replies after reopen, queue saturation,
external change during save, and index/fsync failure after replacement. GUI and
CLI harnesses must reach the same document state and terminal operation result
for equivalent commands. AC-32 collects these checks; it is independent of
whether a notice was displayed.

### 4.6 Read mode

Read mode renders from the **buffer** (so unsaved edits show). Our semantic parser emits pulldown-cmark-compatible events where `[[x]]` becomes a Link event with URL `wiki:x`. iced's markdown widget is event-driven, so a click handler routes `wiki:` URLs to `note.open`. Rendering sits behind a `Renderer` trait, because iced's markdown widget may not cover tables, footnotes, or images well enough (S1 checks). We expect to extend it or replace it with our own. Images decode through the `image` crate with explicit `Limits` (≤ 64 MP, ≤ 64 MB decoded), and thumbnails are cached. **Embeds of attachments render the attachment**: images inline, other kinds as a clickable file card (default: OS default application on click). Embed cycles are cut by cycle detection + depth limit 5 + total-size budget (§4.10).

### 4.7 Search UX details

Quick switcher: in-memory title and alias list, matched with `nucleo`, no DB hit per keystroke. Full-text search reflects **saved** content, and by D12's commit-before-search it reflects every save acked before the query began; worst-case staleness without a query is 5 s. Autosave (1 s idle, default on) keeps it close, and dirty buffers are overlaid only for in-note find.

### 4.8 Case, Unicode, and filesystem hazards

- Native paths are stored losslessly and used for IO; separate NFC/casefold keys serve lookup. Both members of normalization/case collisions remain represented and diagnosed (§4.16.2).
- Casefolding uses Unicode **default** (locale-independent) folding, so Turkish İ/ı doesn't change resolution depending on the user's locale.
- Creating a note from link text sanitizes Windows-illegal characters, reserved names (`CON`, `NUL`…), trailing dots and spaces. The link text is preserved as an alias if the filename differs.
- **Decision (v2, R12) — per-line EOL preservation.** Lattice's input adapter explicitly normalizes CRLF to LF for EditorModel and records original LF/CRLF, UTF-8 BOM, and trailing-newline state. Do not assume Ropey performs this conversion. Newly inserted lines inherit dominant EOL; edits and their undo carry EOL metadata. Serialization returns exact bytes and a coordinate map (§4.16.1). Unedited UTF-8 files round-trip byte-exactly; mixed EOLs are preserved. Unsupported encodings remain read-only and are never rewritten automatically.
- Cloud placeholders (OneDrive, iCloud "dataless" files): detected via platform attributes, and **not hydrated by background scans** — for notes and attachments alike. Indexed lazily on open, with a visible "not downloaded" state. Otherwise we'd download someone's whole cloud vault on first launch.
- Attachments larger than `ATTACHMENT_HASH_MAX` (64 MB) are listed but not content-hashed during scans (path/size/mtime only); their thumbnails are not generated.

### 4.9 Failure behavior

| Failure | Behavior |
|---|---|
| Crash mid-save | Original or new file, never partial (atomic rename). Recovery file offers restore |
| Disk full during save | Temp write fails, original intact, buffer stays Dirty, persistent error banner, recovery file attempt continues best-effort |
| fsync unsupported/failed or storage violates the supported model | Report weaker/uncertain durability; preserve recovery/history and reconcile before retry. No ordinary durable-success claim |
| Crash mid rename-rewrite | Journal roll-forward on next open (D9) |
| SQLite corrupt/locked | Quarantine DB, rebuild, UI runs from disk meanwhile. No user data affected |
| Tantivy behind/ahead/corrupt | Delta replay via `committed_generation`, or rebuild tantivy only (D12) |
| Watcher overflow / dies | Full reconcile, restart watcher, increment counters |
| Parser panic on a file | `parse_ok=0`, file still listed and editable, Diagnostic emitted |
| Attachment unreadable/corrupt | Listed, not indexed, `hydrated=0` or Diagnostic; never blocks startup |
| GPU init fails | Software renderer + graph reduced mode |
| Recovery file corrupt | Ignore it, log it, never block startup |
| Vault on unmounted/disconnected drive | Read-only "vault unavailable" state. Buffers keep recovery files. No index purge |
| Second app instance | Focus the first via IPC and exit; if IPC fails, error out. No read-only fallback in v1 |
| `.flavite/` deleted while closed | UUID re-minted from registry (D2); history and recovery remain reachable |
| External write after the final read loses the overwrite race | Bytes may be absent from disk and history; this is the explicitly accepted D6 residual risk |

### 4.10 Security and trust boundary

| Threat | Control |
|---|---|
| `[[../../etc/passwd]]`, `![[../../../secret]]`, absolute paths (notes **and** attachments) | Every resolved path MUST be canonicalized (after symlink resolution) and verified to be under vault root. Symlinks escaping the vault are refused in v1; validate opened handles/parent directories against the root (§4.16.2) |
| Windows tricks: `\\?\`, ADS (`file.md:stream`), 8.3 names | Reject at the path-normalization layer |
| Giant or pathological notes | `MAX_NOTE_BYTES` (default 32 MB) beyond which file is listed but not parsed or indexed. Parse time budget per file |
| Giant attachments | `ATTACHMENT_HASH_MAX` (64 MB): listed, not hashed/thumbnailed; never hydrated by scans if placeholders |
| YAML alias bombs / deep nesting | Parser limits on aliases, depth, node count |
| Markdown nesting bombs | Depth cap on nesting, and **no recursion on the stack for rendering** |
| Embed cycles (`A` embeds `B` embeds `A`) | Cycle detection plus depth limit 5 plus total-size budget |
| Image decompression bombs | `image::Limits`, decode on worker pool with time budget |
| Malicious URLs on click | Scheme allowlist (`http`, `https`, `mailto`). Everything else, including `file:`, prompts |
| Raw HTML in notes | Never rendered or executed |
| C grammar bugs (tree-sitter) | Fuzzed in CI. Parse work is budgeted. Known gap: segfaults can't be caught by `catch_unwind` |
| Future plugins | Capabilities (D16). Plugins never touch fs. Fuel and memory limits in the WASM host |
| App telemetry | MUST NOT make network calls by default |

### 4.11 Observability

- `tracing` with structured spans: `reconcile`, `save`, `index.apply`, `parse`, `layout.iter`, `frame`. Logs to a local rotating file plus an in-app ring buffer.
- Metrics also include index epoch/log_floor, search commit checkpoint, verification last-completed/overdue, pending journal states/blocked items, and recovery checkpoint age. Metrics (local only, surfaced in a Diagnostics panel and `flavite doctor --json`): `index.generation`, `tantivy.committed_generation`, `watcher.events_total`, `watcher.overflow_total`, **`reconcile.drift_found_total`**, `save.latency_ms`, `history.snapshot_total`, `history.evicted_total`, `buffer.conflicts_total`, `editor.frame_ms`, `graph.layout_iter_ms`, `index.rebuild_total{reason}`.
- **`flavite doctor`:** `quick_check` on SQLite, tantivy generation vs. SQLite, sample-N hash verification against disk, orphan temp files, pending journals, duplicate and conflicting IDs, NFC/case collisions, broken and ambiguous link counts, **attachment orphans and unhydrated placeholders**, history size/pinned bytes, UUID/root binding conflicts, **orphan instance sockets**, search checkpoint/log coverage, verification lag, and live journal payload/checkpoint integrity. Doctor never cleans files referenced by a live journal.
- Panic hook writes a local crash report with the last 200 log lines. A "copy diagnostics" button exists for support.

### 4.12 Versioning and operations

- Cache path includes `schema_major`, so a newer app never mutates an older app's cache and downgrade can't corrupt. A `parser_version` bump triggers a background reindex (old results stay queryable until replaced).
- User-state files (`.flavite/*.toml|json`) carry a `version` and **preserve unknown fields** on write, so an older app can't strip a newer app's data.
- Bad release safety net: rename rewrites snapshot observed prior bytes and pin transaction evidence. Reindexing changes only cache state and never rewrites note files.
- **Single-instance ownership:** retain both the native-root guard and UUID advisory lock in OS data (§4.13). A verified same-root second opener sends Focus over the bound IPC endpoint; a UUID copy conflict needs explicit resolution. Recovery and identity writes require both guards. No lock/IPC artifact is written in the vault. Cross-machine concurrent writing remains unsupported; D6/D7 describe its limits.
- Packaging: signed installers per platform, update channel independent of vault data.

### 4.13 Startup ownership and vault identity

Registry records retain the native canonical path as well as root identity;
before treating an unbound root as new, check existing records for a conflicting
binding at that path. Path reuse is not detected from a new root-key lookup alone.
The root guard and UUID lock live only in OS data. Acquire them in this order
and retain both for the Session lifetime; release in reverse order. C2 must
establish native root identity (directory/file identity plus volume identity
where reliable, otherwise a proven canonical native-path key) for supported
filesystems. Alternate paths to the same root must contend on the same guard;
unsupported alias/identity semantics disable write mode rather than guessing.
Never NFC-normalize or lossily stringify an OS path to choose a lock/registry.

1. Resolve the selected root without reading ignored vault subdirectories;
   acquire its root guard before selecting identity. A competing opener reads
   only ownership/IPC metadata and focuses the owning root, or reports startup
   still in progress. It performs no recovery, registry mutation, or vault write.
2. Read `vault.toml` and registry without mutation. Well-formed TOML wins; an
   absent TOML uses the matching root registry; neither present selects a new
   UUID in memory. Malformed state or a registry for a different physical root
   at a reused path requires explicit identity resolution.
3. Acquire the selected UUID lock. If another process owns it, focus only after
   verifying its IPC endpoint is bound to this root; a different root is a
   copied-UUID conflict. Never attach its cache, recovery, or history to a copy.
4. Under both guards, validate the persistent UUID/root binding, then atomically
   persist TOML and registry. A proven move of the same native directory may
   rebind its path. An ambiguous move/copy offers explicit adoption or a new
   UUID for a separate copy; no silent re-minting. Forking refuses pending copied
   journals until they are explicitly resolved. Local safety state is not copied
   by this action. Conflicting existing roots cannot both adopt one UUID.
5. Start watcher/event buffering, recover valid journals, open/rebuild the cache,
   run the initial scan, and replay nominees. Identity publication interrupted
   by a crash converges on next open under the same guards. A running Session
   keeps its binding if `.flavite/` disappears; another opener cannot mint a
   different identity around its root guard. IPC failure reports an error.

AC-9/22 test concurrent first launches, same-root aliases, pending recovery with
an owner running, root reuse, copied UUIDs, crashes between identity writes, and
lock release. No losing opener mutates vault or safety/transaction state.

### 4.14 Index versions, change retention, and search checkpoints

An index version is `(epoch, generation)`. Recreating SQLite creates a fresh
opaque epoch; generations are monotonic only within it. Allocate note/attachment
IDs monotonically without reuse inside an epoch. Queries, pulses, graph feeds,
search checkpoints, and pagination cursors carry the epoch. Old-epoch IDs/cursors
are invalid. Pagination at a different generation returns `StaleCursor` rather
than silently skipping/duplicating items; callers restart the query.

A structural write transaction atomically updates notes/attachments/search_docs,
re-resolves all affected links (including resolved candidates), increments the
generation once, and writes `changes` invalidations. Record `note`/`attachment`
for added, changed, and deleted entities, and `outgoing` for every source whose
edge set/resolution changed. A removed row leaves its invalidation behind;
changes has no cascading foreign key. Source hashes and cached searchable body
text come from the exact indexed bytes, so replay never reads newer disk bytes
and pretends they belong to an older snapshot.

A change reader takes one SQLite read snapshot at version G and requests all
invalidations in `(cursor.generation, G.generation]`. Deduplicate entity IDs;
replace each node/outgoing set from current rows, or remove it if absent. This
is an invalidation feed, not a historical snapshot of every intermediate edit.
Return G only with the complete batch. If the epoch differs, the cursor is ahead,
or cursor.generation < log_floor, return `ResetRequired` and a consistent full
snapshot path. GraphFeed applies the batch and its version atomically; position
snapshots carry the same topology version so changed node indices cannot be
paired with older positions. An epoch reset clears cache IDs and reloads pins
from native paths/unconflicted stable IDs.

Default log storage budget: 64 MiB, configurable for measured rebuild/churn
costs. Prune complete generations and advance `log_floor` in the same transaction;
consumers slower than retention reset/rebuild instead of blocking pruning.
Oversized batches are chunked into complete structural transactions with visible
progress. Limits may trigger rebuilding a lagging search view; they must never
omit a deletion or advance an incomplete consumer cursor.

Search uses the checkpoint `(epoch, generation)` published atomically with its
own durable commit. S4 must prove the backend adapter's publication mechanism
(commit metadata or a checkpoint document in that same commit). Updating a
SQLite counter separately cannot establish search durability. The SQLite mirror
is diagnostic only and is updated after the search commit succeeds.

On startup, validate the search checkpoint and log coverage. For replay to a
SQLite snapshot G, delete each invalidated note ID from search, then add its
current structural fields and cached search_docs body if it still exists. This
makes replay idempotent and deletion-safe. Publish checkpoint G only after every
operation is included; then reload/capture the matching search reader. Invalid
checkpoint, missing log, epoch mismatch, or corrupt search rebuilds search from
a consistent SQLite snapshot; it never writes note files. Search/structural
reader preparation uses an IndexActor barrier so hybrid queries combine matching
versions and include all saves acknowledged before the query began.

AC-33 crashes after note deletion and between SQLite apply, search commit,
checkpoint publication, reader reload, and mirror update. It also exercises log
pruning, cache rebuild with live consumers, ID non-reuse, resolver changes, and
hybrid-query consistency. Logical rebuild comparisons exclude cache-internal
IDs/versions/logs and compare entities by native paths and resolved destinations.

### 4.15 Journal records, replay, and buffer checkpoints

A v2 journal has `version`, monotonic `seq`, transaction state, native path
endpoints, expected identities/hashes, and per-item state/payload references.
Write each complete manifest version atomically and fsync its directory; a torn
append is never treated as a valid state transition. All manifest paths and
payloads are hostile input: root containment, schema/size validation, and hashes
are checked before access/application. Unknown versions or corrupt manifests
return ActionRequired and remain preserved. Mutation-capable recovery always
runs under §4.13 ownership. Before C12 supplies the full recovery engine, C8
only validates/classifies committed journals and opens the vault read-only with
ActionRequired; it must not apply them without history/checkpoint support.
Each transition has a fault-injection point.

| Phase/item state | Durable evidence and recovery rule |
|---|---|
| preparing | Manifest describes the plan; no truth files/buffers changed. Delete only owned uncommitted staging, or restart preparation. |
| committed / pending | Manifest references durable hashed replacement payloads; restage a missing sibling from payload. Missing/corrupt payload blocks the item and retains evidence. |
| file-applied | Disk equals planned new bytes, or a durable receipt records the outcome; recovery is idempotent. Disk equals base permits apply; any other content is externally diverged and requires action. |
| buffer-applied | Durable recovery checkpoint has bytes plus original buffer identity, revision, txn/item markers and checkpoint sequence; restore through Session and wait for save/action. |
| cancelled | Manifest references a durable post-undo checkpoint; stale pre-undo recovery cannot restore the cancelled patch. |
| saved / skipped | Saved receipt covers the item's operation marker, or the user explicitly accepted a reported skip; terminal disposition is durable. |

Preparation creates `.flavite/journal/<txn>.json` and `<txn>.data/`. Store full
replacement bytes there, then stage sibling temps for atomic replacement; all
required payloads, manifest entries, files, and parent directories must be durable
before COMMIT. Preflight disk space and a 256 MiB default payload budget; refuse
before commitment if exceeded (raising the budget is explicit and measured).
Stream payloads rather than collecting a vault's rewritten files in memory.
No rename/rewrite occurs before COMMIT. The v1 move operation supports one note
or attachment on the same filesystem; directory and cross-filesystem moves are
rejected before commitment. Existing destinations are never clobbered. Case-only
renames use a journaled unique intermediate path and track both durable steps.
For unhashed large attachments, use the native identity/metadata precondition
and expose its weaker validation; rewriting inbound notes still uses full hashes.

APPLY rechecks each closed file against its expected base just before replacing
it and snapshots observed bytes per D6. Payloads permit restaging after a crash
between staging and apply. Never overwrite external divergence during replay.
A missing source, occupied destination, unknown intermediate, or failed durability
step preserves the transaction and reports partial/ActionRequired. This protocol
converges each uncontested item; it does not promise a globally atomic multi-file
view or automatic completion over later external edits.

Before replying `buffer-applied`, Session captures a revisioned snapshot and the
worker persists its bytes and operation markers. The reply references that
checkpoint; a crash before the manifest advances is recognized by the marker.
Later edits/checkpoints carry operation state even when their text hash changes.
Old indexed spans are never applied directly to a dirty buffer: reparse/replan
against its guarded current snapshot (§4.16.1).

Undo applies its inverse once in memory and returns Pending. Persist an immutable
post-undo snapshot first, then publish `cancelled` with its original buffer
identity/revision/checkpoint reference. Only then may undo report success. Keep
newer edits and serialize checkpoint writes so an older write cannot replace a
newer durable recovery record. On crash, a cancelled item's fallback is its
post-undo snapshot unless a verified later checkpoint from the same buffer
lineage supersedes it. A mere matching/mismatching text hash is insufficient.
Before durable cancellation, an unacknowledged undo may recover the preceding
durable state; the UI shows that it is pending. Redo allocates a new operation
identity and never reactivates a terminal cancelled record.

CLOSE requires every item saved, cancelled with durable recovery handed off, or
explicitly skipped. `buffer-applied` without a saved successor keeps the journal
in `waiting_buffers`. A save receipt may cover later edits when it includes the
active operation markers; its bytes need not equal the original patch hash.
Cancellation payloads remain until a durable successor recovery record/save owns
them. Report `AppliedPendingSave`/Partial results with txn ID and outstanding
items; ordinary completed-success requires terminal disposition. Keep undo txn
references valid until their obligation is handed off; undo of a fully closed,
saved patch is then an ordinary buffer edit. Cleanup removes payloads only after
a durable terminal manifest; it never evicts live evidence.

Capture base bytes on load; before the first durable dirty checkpoint/save,
persist its merge base and pin the reference (C11). Keep the in-memory base
snapshot while that work is pending; a base_hash alone cannot reconstruct it.
History versions used as open-buffer merge bases or transaction preimages are
pinned. The 1 GiB history cap includes pinned blobs; insufficient evictable space
rejects the prospective overwrite before it occurs. Record note identity, time,
reason, and references separately from deduplicated blob content so retention
and merge-base lookup survive deduplication. Journal payloads have their separate
budget. AC-2/10 exercise all phase/checkpoint/undo boundaries, missing staging,
external edits, and evidence cleanup; recovery can report blocked items without
misreporting their completion.

### 4.16 Text coordinates and filesystem identity

#### 4.16.1 Text snapshots and spans

V1 edits valid UTF-8 with an optional UTF-8 BOM. Other encodings are listed with
a diagnostic and read-only behavior; no lossy decode may feed a rewrite. Lattice
normalizes CRLF to LF at input, retains per-line EOL/BOM metadata, and preserves
standalone CR bytes as content. It does not NFC-normalize user text on save.

Use distinct types for `SourceByte` (original UTF-8 bytes), `TextByte` (normalized
UTF-8), scalar index, grapheme boundary, and platform UTF-16 offset. Ranges are
half-open and tied to a source hash or buffer identity/revision. EditOp ranges
use TextByte at valid scalar boundaries; Ropey conversions are explicit at its
boundary. Interactive cursor movement/deletion uses grapheme boundaries; shaping
supplies bidi caret affinity. Find/casefold/normalization matching carries a map
back to the original snapshot, including expanding matches and combining marks.

TextSnapshot owns normalization metadata and a reversible mapping at legal edit
boundaries. BOM and CRLF interiors are never edit boundaries. Both parser glues
scan the same normalized text representation; AC-5 compares TextByte spans at
the same revision. Index storage maps link spans to SourceByte plus content_hash
for disk rewrites. Open-buffer patches reparse that buffer's current normalized
snapshot and validate revision, instead of reusing disk spans after unsaved edits.
Serialization returns bytes, source map, and hash from one immutable text/EOL
snapshot. Apply multiple patches in validated nonoverlapping ranges as one edit;
undo/redo restores the accompanying EOL metadata. Save/find/highlight worker
results carry snapshot identity and are discarded or explicitly mapped if stale.

AC-35 covers BOM, LF/CRLF mixtures, missing final newline, split/join/undo/redo,
emoji/combining/RTL text, casefold expansion, and a dirty-buffer rename patch.
Assertions compare unmodified byte ranges, not only comment counts or parseability.

#### 4.16.2 Native paths, lookup keys, and IDs

PathKey is a reversible encoding of vault-relative native components; reject
absolute/traversal components. Use a portable UTF-8 component representation
when lossless, otherwise an explicitly platform-tagged native-bytes/units form.
Preserve Unix bytes and Windows native units without Unicode normalization or
lossy conversion; portable names keep the same persisted pin key across OSes. `path_native` is its unique
cache key; `path` is display text, while NFC/default-casefold `path_key` and
stem/alias keys are nonunique lookup aids. IO, registry/recovery hashes, journals,
and path-keyed pins always use native identity. Represent both members of an
NFC/case collision, report ambiguous resolution deterministically, and refuse a
new conflicting filename instead of overwriting/merging existing files.

Resolve relative paths through a pinned root/parent capability and validate the
opened target, not only a pathname checked before use. C2/C5 must establish the
per-OS strategy, including concurrent symlink swaps and nonexistent destination
parents; refuse a path if containment cannot be established. Internal symlinks
may refer to a target inside the root; saving changes that target and preserves
the link. External symlink targets are forbidden in v1 (fixed, no override).
Scans prune `.obsidian/` and other ignored directories before descent. Native
file identity detects alternate paths to the same opened note so Session does
not create two writable buffers for one resolved target. Distinct hard-linked
entries whose replacement semantics cannot be preserved are read-only with an
explicit unsupported-write result in v1; never silently duplicate authority.

Duplicate frontmatter stable_id values are stored without a UNIQUE constraint.
Flag all members, choose the deterministic native-PathKey winner with a visible
ambiguity diagnostic, and offer explicit mint-new-ID repair. Only unconflicted
IDs qualify for T2 identity transfer or stable-ID pin keys; conflicted notes use
native path pins. Cache rebuild yields the same winner. UUID/root bindings and
copy/move behavior follow §4.13. AC-35 covers normalization collisions, native
non-UTF-8 names, same-target aliases, case-only moves, and duplicated IDs.

---

## 5. Red team (and what changed because of it)

| Attack / failure | What broke | Design change |
|---|---|---|
| External tool writes *identical bytes* during our save | Self-write token confused | Not a bug: hash-equality reconcile is a no-op (D5). Tokens only classify |
| Two empty notes deleted + one created | Rename inference matches wrong pair | Size floor + unique-candidate requirement; T3/T4 are suggest-only (D8) |
| Bulk changes preserving size and old mtime | Metadata-only scans miss real change | Force-hash nominated paths; periodic full VerifyContent catches silent event loss (D5) |
| User edits source note while rename-rewrite is mid-flight | Patch applied over unseen edits | VERIFY step excludes mismatched files; per-file hash check on APPLY (D9) |
| Crash between "rename note file" and "patch inbound links" | Half-renamed vault | Roll-forward journal in vault (D9) |
| Dirty buffer + external delete + external recreate with different content | Ambiguous state | Treated as "changed" (hash≠base) → conflict protocol. Delete→recreate within the coalescing window is indistinguishable and correctly handled as a change |
| Copied note duplicates `stable_id` | Cache uniqueness hides a note or gives unstable identity | Store all raw IDs; deterministic path winner + conflict flags; no T2/pin transfer until resolved (D8) |
| Syncthing "conflict copy" files appear (`note.sync-conflict-…md`) | Thousands of phantom notes and ambiguous links | Treat as normal notes (we can't know), but Diagnostics flags known sync-conflict filename patterns. Configurable ignore globs |
| Watcher silently stops delivering | Stale index forever | Periodic + focus reconcile; drift counter alerts in Diagnostics (D5) |
| Merge produces wrong text silently | Data corruption by "helpful" tool | Always snapshot first; unresolved hunks need explicit user choice; never auto-resolve non-trivial merges |
| Autosave fires during a conflict | Overwrites the other writer | Autosave hard-disabled in Conflicted state (D7), tested |
| Huge paste (50 MB) | Frame stalls, OOM | Chunked insert op, parse budget, ropey handles it. Above `MAX_NOTE_BYTES`, warn and degrade features |
| Rapid keystrokes + slow highlight | Input lag | Highlight is time-budgeted per frame and off the input path. Stale spans draw until updated |
| GPU lost / device reset in graph | Blank canvas or crash | Recreate pipeline from `GraphModel` (CPU state is authoritative); fallback mode |
| Cache deleted while app running | Reads fail | Detect, rebuild online; UI shows "reindexing" |
| Two app instances on one vault | Two writers, two journals | Advisory lock + focus IPC in OS data dir only; no vault artifacts (§4.12) |
| Plugin writes around the buffer | Conflict-protocol bypass | Capability design forbids it. The API surface has no raw fs path (D16) |
| Downgrade after schema bump | Old app corrupts new cache | Schema-major in cache path (D2, §4.12) |
| **External write after our final read** | Other writer's bytes can be lost from disk/history | Observed-byte snapshot guarantee only; post-read race explicitly accepted (D6, AC-8) |
| **User deletes `.flavite/` (v2)** | Vault UUID re-mints; history/recovery orphaned | Vault registry in OS data dir re-mints the same UUID (D2, R2) |
| **Embed of an image (v2)** | Resolves as `unresolved`; doctor cries wolf | Attachments are first-class: `attachments` table, `links.dst_att`, unified resolution (D8, §4.3, R3) |
| **Autosave × tantivy commit per save (v2)** | Commit/merge churn per idle tick | Deferred commits, commit-before-search, `committed_generation` (D12, R4) |
| **Bulk reconcile floods the event channel (v2)** | Conflict notices dropped | Pulse/notice channel split with opposite loss policies (D3, R5) |
| **Copy note into new note (same content) (v2)** | Pin moves and buffer re-keys silently | Tiered inference: T3/T4 never carry identity state silently (D8, R6) |
| **Undo followed by crash** | Stale recovery text could resurrect a patch | Durable post-undo payload + cancellation marker before success; checkpoint lineage, not hash equality (D9/§4.15) |
| **Mixed-EOL file saved (v2)** | Whole-file EOL normalization → git noise | Per-line EOL side table; byte-exact round-trip (§4.8, R12) |
| **Lockfile inside the vault (v2)** | Synced by Syncthing/iCloud; cross-machine false conflicts | Lock and IPC live in the OS data dir only (§4.12, R14) |
| **Scan hashes a 2 GB video (v2)** | IO spike on open | `ATTACHMENT_HASH_MAX`; placeholders never hydrated (§4.8) |
| **History fills with autosave versions (v2)** | 1 GB cap reached in hours; old versions evicted too early | Tiered retention + content-addressed dedup (D6) |

---

## 6. Alternatives considered

| Alternative | Why rejected | Reconsider when |
|---|---|---|
| **Webview shell (Tauri/Dioxus) + CodeMirror/ProseMirror** | Violates the pure-Rust requirement. Fastest route to polished editing, but leaves us with a split-brain codebase | S1 shows iced *cannot* deliver IME events to a custom widget, upstream patches and a fork both fail, and "pure" is up for renegotiation (ADR-0001 last resort) |
| **Stock iced `text_editor`** | Owns its buffer, limited decoration/hit-test hooks, no shared-buffer model. Q1 answered: primitives insufficient (ADR-0001) | Closed. Not a v1 fallback |
| **GPUI** (considered earlier, dropped) | Platform maturity, API churn | Its Linux/Windows story matures *and* iced's editing primitives prove inadequate |
| **SQLite FTS5 only (no tantivy)** | Simpler, one store, zero drift | Drift incidents appear, or binary-size and build-time costs of tantivy hurt. Swap via `SearchBackend` |
| **ID-only links (Notion/Logseq style)** | Ugly raw files, breaks Obsidian/Zim ethos and plain-text readability | Never as default. Stable-form exists as opt-in |
| **Index inside the vault** | Syncs with the vault, so WAL corruption and merge conflicts on a binary file | Never |
| **Rollback-style rename transaction** | Roll-forward keeps conflict handling explicit; history retains prior bytes for user recovery | A replacement protocol offers a stronger verified guarantee |
| **CRDT buffers (yrs/automerge/loro)** | Large complexity, and the on-disk format stays plain text anyway | A committed roadmap item for real-time multi-user editing (ADR-0002 reversal trigger) |
| **Async core (tokio)** | Everything underneath is blocking. Pure overhead | Plugin host or network features dominate |
| **Drop-in Obsidian replacement** | Plugin/theme/Canvas compatibility surface is unbounded and not our product | Never; posture is "inspired by" (§0) |
| **Mobile targets** | Out of scope; no budget | A funded mobile initiative exists |

---

## 7. Risks

| Risk | Impact | Mitigation | Detection |
|---|---|---|---|
| iced API churn / breaking releases | Constant rebase tax | Pin version, upgrade deliberately, keep iced out of core crates | Weekly CI job building against iced `master`. Failure count tracked |
| Custom editor: IME / selection / bidi bugs | Unusable for CJK/RTL users | Per-platform IME acceptance checklist, synthetic composition tests, early alpha with CJK users; S1 verifies event delivery | Failing IME suite in CI. Bug-tag trend |
| iced cannot deliver IME events to a custom widget | Architecture fork (ADR-0001 escalation) | S1 first, before any editor code; escalation path: upstream → fork → renegotiate purity | S1 result recorded before Phase 6 |
| Accessibility gap in iced | Screen reader users excluded | S1 verifies AccessKit path, upstream contribution | S1 result recorded. A11y smoke test per platform |
| Data-loss bug in save/rename/merge | Existential | Fault-injection filesystem, model-based tests, **unconditional** history snapshots, recovery files | `FaultFs` suite in CI (§8). `doctor` hash-verify. Alpha dogfooding on real vaults with nightly `doctor` |
| Tree-sitter C grammar crash (segfault) | App dies. `catch_unwind` can't save us | Fuzzing, parse budgets, keep grammar set small and pinned. Optional out-of-process parse for non-bundled grammars | Crash reports. Fuzz CI |
| Watcher unreliability (network FS, Docker, WSL) | Stale index | Periodic reconcile, polling fallback mode | `reconcile.drift_found_total` > threshold → Diagnostics hint, auto-switch to polling |
| Tantivy/SQLite drift | Wrong search results | `committed_generation` delta replay, auto-repair | `doctor` and startup generation check log `index.rebuild_total{reason=drift}` |
| Graph performance on weak GPUs | Unusable graph | LOD, edge caps, reduced mode | Frame-time metric with bench CI |
| Plugin API ossification | Locked into wrong abstractions | `unstable` label, dogfooding rule | API-diff check in CI shows churn. Review before first stable tag |
| Scope: editor + graph + index are each large | Schedule blowout | Spikes first, headless core before UI | Milestone exit criteria (§9). Burn-down review per phase |
| Cloud placeholder hydration storm | Bandwidth/battery burn | Don't hydrate during scans (§4.8), for notes and attachments | Network/IO counters in Diagnostics. Test with simulated placeholders |
| Index rebuild too slow for huge vaults | Bad first-run experience | Parallel parse, progressive availability (UI usable while indexing) | Rebuild timing benchmark in CI (§8) |
| History store disk growth | Surprise disk usage | Tiered retention, 1 GB cap, `history.evicted_total` metric | `doctor` history report |
| Attachment scan IO on huge binaries | Open-time latency | `ATTACHMENT_HASH_MAX`, stat-only for large files | Reconcile timing metrics |
| Gate atrophy / hook-bypass culture | Quality gates stop being trusted | G0 time budget measured in CI; G1 is the hard wall; bypass rate reviewed monthly | G1 failure rate and time-to-green trend |

---

## 8. Acceptance criteria

*Reference machine: 4+ cores, integrated GPU (Intel Iris Xe class), NVMe, 16 GB RAM. Numbers are targets to be re-validated in Spike results.*

**Durability and correctness**

1. **Fault-injected saves:** ≥ 10,000 randomized saves in each declared ProcessCrash and PowerLoss model (§10.6.1), injecting at modeled write/sync/replace boundaries, yield complete old or new destination bytes without concurrent external writers. Check acknowledged durability separately from pre-ack crash outcomes; label unsupported storage behavior explicitly. Temporary files may be partial; installed destination files may not.
2. **Rename transaction:** crash at every prepare/payload/commit/apply/checkpoint/close boundary (§4.15). Preparing transactions leave truth unchanged. Committed uncontested items converge idempotently to new state, while external divergence or missing evidence is preserved and reported as blocked. No partial destination bytes, false completed result, or untracked pending buffer patch. Global multi-file atomicity is not asserted.
3. **Conflict protocol:** every D7 row has an integration test; a detected base-hash mismatch blocks overwrite until explicit resolution. Test external writes before/during/after the final read. Writes arriving afterward are the accepted D6 race, never a claimed history restore guarantee.
4. **Index equivalence:** model-based test applies random sequences of create/edit/rename/delete/external-write. The incrementally maintained index MUST equal a from-scratch rebuild (logical dump compare) after every sequence.
5. **Parity:** differential fuzz over ≥ 100k generated documents compares TextByte semantic spans on the same normalized snapshot (§4.16.1), with zero divergences; mapping back to raw source bytes has independent round-trip checks.
6. **Crash recovery:** under the healthy reference workload, kill the typing harness at arbitrary times and measure a ≤ 2 s loss target with restore offered on next launch. Report checkpoint age/write latency and exercise delayed/disk-full writes separately; a 2 s timer alone is not proof of a bound under failed storage.
7. **EOL round-trip (v2, R12):** a mixed-EOL file with BOM and no trailing newline, edited in one line and saved, differs from the original only in that line's bytes. Unedited files round-trip byte-exactly.
8. **TOCTOU backstop (v2, R1):** a scripted `FaultFs` interleaves an external write at every point in `save`. In every interleaving where the external write lands before our read, the external bytes are present in the history store. Property-tested over interleavings.
9. **Vault identity recovery:** with the app closed and no live journal, delete `.flavite/`, reopen under both guards, and retain UUID/history/recovery. Also test interrupted identity publication, copied UUIDs, reused paths, and proven moves (§4.13); ambiguous bindings never silently share safety state.
10. **Journal undo:** crash before/after post-undo payload persistence, durable cancellation, reply, successor checkpoint, and cleanup (§4.15). Completed undo never resurrects its patch, even with a stale pre-undo recovery file or subsequent edits. Before completion only the last durable state is promised. Redo uses a new operation identity; cleanup retains evidence until handoff.

**Attachments (v2, R3)**

11. `![[diagram.png]]` resolves to the attachment, appears as a graph node, renders inline in read mode, and is never reported as an unresolved link by `doctor`. An in-app attachment rename rewrites all embeds through the D9 journal; an external attachment rename (T1/T3) produces a notice, never a silent rewrite.

**Performance** (reference machine)

12. Cold open of a 20k-note vault with warm cache: first interactive paint ≤ 500 ms p95. No-change reconcile completes ≤ 3 s in the background.
13. Full index rebuild of 20k notes (~80 MB): ≤ 30 s, UI usable throughout.
14. Typing latency, keystroke-to-frame: ≤ 16 ms p99 on a 100 KB note, ≤ 33 ms p99 on a 1 MB note.
15. Search: top-20 results p95 ≤ 50 ms on 20k notes. **A search issued immediately after a save ack MUST return the saved content** (commit-before-search). Quick switcher ≤ 16 ms per keystroke over 50k titles.
16. Graph: 50k nodes / 200k edges, zoomed out, 60 fps pan/zoom. Frame-time p99 ≤ 16.6 ms **while layout runs**. Card mode with ≤ 64 cards ≥ 60 fps.
17. `git checkout` rewriting 20k files: UI frame p99 ≤ 33 ms throughout, index consistent within 30 s of completion. **Zero notices lost on the notice channel** during the bulk reconcile (v2, R5).

**Robustness and security**

18. Watcher-loss simulation (drop all events for 60 s): index converges after next focus/periodic reconcile. Drift counter increments.
19. Path-traversal, symlink-escape, ADS, and reserved-name test suites pass on all three OSes, for note links **and** attachment embeds. No read or write outside vault root, ever.
20. Fuzz targets (parser, scanner, frontmatter patcher, rename planner) run ≥ 1 hour each in CI with zero panics, OOMs, or hangs. Budgets enforced (`MAX_NOTE_BYTES`, `ATTACHMENT_HASH_MAX`, embed depth/size, image limits).
21. Deleting the cache directory while the app is closed *and* open yields a correct rebuilt state and zero user-visible data loss.
22. **Single instance:** concurrent first opens and same-root aliases serialize under root + UUID guards before any identity write/recovery. A losing opener cannot replay a pending journal; copied UUIDs never focus a different root. Process death releases ownership; no lock/socket artifact appears in the vault (§4.13).

**Platform and UX**

23. CI green on Linux, macOS, Windows. Release binaries signed.
24. IME composition (preedit, commit, candidate window placement) passes the checklist for at least Japanese, Chinese (Pinyin), and Korean on each OS.
25. Screen reader can read note text and move the caret on at least one reader per OS, **or** a documented, tracked waiver exists (S1-dependent).
26. No network traffic or attempted outbound connections with default settings: a network-denied CI run checks attempted socket/connect activity, alongside functionality. The harness excludes its own tooling traffic and a seeded connection attempt must fail the assertion; merely denying networking is insufficient.

**Development infrastructure (§10)**

27. **Local gate:** from a clean working tree with provisioned dependencies and warm build cache, full G0 passes within 5 minutes on the reference machine. Record cold first-build/setup time separately; A1–A3's bootstrap gate cannot claim full-G0 acceptance.
28. **Arch check (negative tests):** adding an undocumented internal dependency fails `cargo xtask arch check`; adding an `iced` dependency to any core-tier crate fails the layering rule. Both are exercised by seeded violations in CI.
29. **Coverage gate (negative test):** a change that drops a gating crate below its §10.5 floor fails G1.
30. **Fuzz smoke:** every checked-in corpus entry runs clean under `just fuzz-smoke`; a seeded crash is caught within the smoke window.
31. **Post-gate cleanup:** after a gate that pushes `target/` past the soft budget, `just gc` brings it back under budget; `just clean --dry-run` reports without deleting; cleanup leaves `git status` clean — no tracked file is ever removed.
**Cross-cutting contracts**

32. **Command execution contract:** the §4.5.5 scenarios pass headlessly and through the GUI adapter, including revision guards, serialized disk mutations, edit-during-save recovery preservation, correlated patch replies, overload handling, and explicit partial-failure results. Equivalent GUI/CLI document commands produce equivalent core outcomes; notice presentation cannot acknowledge a transaction. G1 runs these tests as their owning slices land.
33. **Index recovery/feed:** AC-33 scenarios in §4.14 preserve deletions, search/structural consistency, bounded retention resets, and epoch-safe graph/pagination behavior across all checkpoint crash boundaries.
34. **Content verification:** drop watcher events, preserve a changed note's size and old mtime, and verify focus/periodic VerifyContent detects it. Repeated focus cannot postpone a due pass; metadata-only large attachments and placeholders keep their documented limits.
35. **Coordinates/identity:** §4.16's text round-trip, collision/native-path, duplicate-ID, and alias-buffer tests pass on all supported OSes; raw paths/bytes remain independently addressable.
36. **Fault-model conformance:** ProcessCrash and PowerLoss differ at unsynced writes/directory changes; seeded missing-fsync and short-write defects fail the model. OsFs conformance/process-kill results are reported separately from model coverage (§10.6.1).


---

## 9. Execution plan

Order is by dependency and by risk: kill the biggest unknowns first, build the part you can fully test without a GPU next, and put the UI on top of something that's already trustworthy.

**Phase 0.0: Repository bootstrap (before any product code).** Workspace skeleton, justfile, G0, `xtask` arch gen/check, fuzz workspace skeleton with seed corpora, CI matrix, `just setup` + hooks. *Exit:* G0 green on the empty workspace; the arch check and coverage gate demonstrably fail on seeded violations (AC-27–30). Gates adopted after the code exists are gates that get bypassed.

**Phase 0: Spikes (≈3 weeks). Output: written results that confirm the v2 numbers in §8 and close the remaining unknowns.**

- **S1: custom-editor viability (rescoped in v2).** Q1 is closed (custom editor), so S1 verifies the *custom* path: can iced/winit deliver IME preedit/commit/candidate events to a custom widget on Linux (X11 + Wayland), macOS, and Windows; does cosmic-text provide caret rects and hit-testing at fractional (HiDPI) scale; what is iced's AccessKit status; does iced's markdown widget cover tables/footnotes/images for read mode; what is the paste-from-HTML sanitization path. Outcomes: proceed / upstream patches / fork iced / (last resort) ADR-0001 renegotiation.
- **S2: graph.** 50k nodes in a `shader` widget plus rayon Barnes-Hut layout, frame-time under load, tiny-skia fallback behavior, card layer with real widgets under scale, attachment nodes.
- **S3: filesystem truth.** Watcher behavior per OS (rename events, overflow, coalescing), atomic-replace semantics on Windows and a network FS, cloud placeholder detection (notes and attachments), advisory-lock behavior after `kill -9`.
- **S4: search/DB.** Tantivy CJK bigram quality, **deferred-commit behavior under autosave load** (commit-before-search latency, `committed_generation` replay), WAL read/write concurrency under reconcile load.

**Phase 1: Core (`fs`, `syntax`, `index`, `vault`, `cli`).** `Fs` trait + `FaultFs`; atomic write with unconditional history snapshot; watcher + coalescing; reconcile over notes **and attachments**; vault registry (D2); scanner and frontmatter patcher with well-known key mapping; schema incl. `attachments`; resolver incl. attachment targets; queries; `doctor`. *Exit:* acceptance 1, 4, 5 (parser side), 7, 8, 9, 11, 18–21 green headlessly.

**Phase 2: Buffer and safety (`buffer`, history, recovery).** C6 supplies shared pure TextSnapshot/coordinate types before C10. `EditorModel` with per-line EOL side table, undo tree, D7 state machine, tiered-retention history store, recovery files (configurable interval), three-way merge. Can start once `fs` and `syntax` exist. *Exit:* acceptance 3, 6, 7.

**Phase 3: Rename/move transactions.** Planner, journal, recovery, buffer-aware patching with cancel-on-undo, tiered rename inference (D8), attachment renames, notifications. *Exit:* acceptance 2, 10, 11.

**Phase 4: API surface (parallel with 2–3).** `flavite-api`: registry/facade over the headless Session, typed commands/results, operation transport adapter, pulse/notice channels, queries, capabilities. C8/C10 establish the engine; C11–C13 extend its handlers. *Exit:* CLI itself is a consumer of the API (dogfooding check); command-contract checks (AC-32) run for the handlers implemented at each slice and accumulate through integration.

**Phase 5: UI shell.** Panes, palette, quick switcher, backlinks, search panel, diagnostics, conflict banners, read mode (incl. attachment embeds), settings. Uses a minimal editor surface. *Exit:* daily-usable on a real vault.

**Phase 6: Editor widget.** Custom `EditorSurface` over `EditorModel`, tree-sitter injection highlighting, autocomplete, link hit-testing, IME, a11y path. *Exit:* acceptance 14, 24, (25).

**Phase 7: Graph.** Graph model and layout worker, shader widget, LOD, spatial hit-test, attachment nodes, cards via `BufferManager`, pinned positions (T0–T2 carry-over). *Exit:* acceptance 16.

**Phase 8: Hardening and release.** Fuzz campaigns, perf CI gates, packaging/signing, docs, API-surface audit for plugin readiness, alpha with real vaults and CJK/RTL users.

```
 S1 ────────────► P6 (editor)
 S2 ───────────────────────► P7 (graph)
 S3 ─► P1 ─► P2 ─► P3 ─┐
 S4 ─┘        └► P4 ────┴► P5 ─► P6 ─► P7 ─► P8
```

---

## 10. Project setup and development infrastructure

This section is normative for the **repository**, not the product. A durability-first product needs a durability-first process: the gates below are the mechanism by which §8's acceptance criteria stay true after the first commit. The infrastructure MUST be stood up in Phase 0.0, before product code — gates adopted after the code exists are gates that get bypassed.

### 10.1 Repository layout

```
flavite/
├── Cargo.toml                  workspace root: members, [workspace.lints], edition 2024, resolver "3"
├── rust-toolchain.toml         pinned stable toolchain
├── rustfmt.toml · clippy.toml · deny.toml
├── justfile                    single entry point for every gate recipe (§10.3)
├── crates/
│   ├── flavite-fs/             Fs trait, atomic_write, watcher, path safety; FaultFs behind feature "fault"
│   ├── flavite-syntax/         scanner, frontmatter patcher, pulldown glue — pure, IO-free
│   ├── flavite-index/          SQLite, tantivy, resolver, query exec
│   ├── flavite-buffer/         EditorModel/ropey, undo tree, D7 state machine
│   ├── flavite-vault/          Session/BufferManager ownership + actors, journal, history, recovery
│   ├── flavite-api/            registry, typed commands/results, revisioned views, events, capabilities
│   ├── flavite-graph/          layout, spatial — CPU only
│   ├── flavite-cli/            headless CLI; consumes flavite-api (D16 dogfood)
│   ├── flavite-editor/         iced widget over EditorModel (iced permitted here and in flavite-ui only)
│   └── flavite-ui/             iced application
├── xtask/                      dev tooling: arch graph generation, verification, query CLI (§10.2)
├── fuzz/                       cargo-fuzz workspace, excluded from default-members (§10.6)
│   ├── fuzz_targets/            parser · scanner · frontmatter_patcher · rename_planner · path_safety · parity · image_limits
│   └── corpus/<target>/         checked-in, curated, minimized seed corpora
├── devdocs/
│   ├── plans/SPEC-draft-v2.md  this document (canonical; mirror alongside it)
│   └── architecture/
│       ├── crates.toml         hand-maintained: tiers, responsibilities, expected edges, rules
│       ├── crate-graph.json    GENERATED — the project-RAG artifact
│       └── DEPENDENCIES.md     GENERATED — human/agent-readable rendering
└── .github/workflows/          thin forge layer; invokes justfile recipes only
```

### 10.2 The project-RAG: architecture knowledge and verification

**Decision:** the project MUST carry a generated, checked-in, machine-readable map of what depends on what, plus a gate that proves the code still matches it. The RAG exists so reviewers, agents, and future maintainers answer "what depends on what" and "what is this crate allowed to depend on" by retrieval instead of by reading the ten product manifests plus xtask.

- **Generation.** `cargo xtask arch gen` reads `cargo metadata --format-version 1` and writes `devdocs/architecture/crate-graph.json` (nodes: workspace crates + first-party external dependencies — iced, winit, wgpu, cosmic-text, ropey, rusqlite, tantivy, pulldown-cmark, notify, tree-sitter, blake3, zstd, nucleo, image, tracing; edges: direct dependencies, split by kind: normal / build / dev) and renders `DEPENDENCIES.md`. Both files are GENERATED and checked in; they MUST NOT be hand-edited.
- **Hand-maintained source.** `crates.toml` records active internal edges for this checkout, separately from the full planned dependency table. Exact equality compares active edges only; each edge-adding slice updates them and regenerates artifacts. A4 initially records the implemented edges, not future dependencies on empty crates. Generation uses a deterministic all-features/target-aware metadata view, retaining dependency kinds and cfg expressions without host paths; layering is evaluated for each supported target configuration. Cross-platform generation must be byte-stable.
- **Verification.** `cargo xtask arch check` (in G0 and G1) performs three checks:
  1. **Drift:** regenerate and diff; any difference fails the gate.
  2. **Edges:** actual internal edges MUST equal expected edges, exactly, in both directions. A new internal dependency requires updating `crates.toml` **and** the §4.1 diagram in the same PR — an architecture change is a spec change.
  3. **Layering (transitive):** for every core-tier crate (fs, syntax, index, buffer, vault, api, graph, cli), the transitive closure of normal + build dependencies MUST NOT contain `iced`, `iced_*`, `winit`, `wgpu`, or `cosmic-text`. Dev-dependencies are exempt (test-only) but listed in the artifact. `flavite-editor` and `flavite-ui` are the only crates that MAY depend on them.
- **Query CLI (the retrieval half).** `cargo xtask arch deps <crate>` · `arch dependents <crate>` · `arch path <from> <to>` · `arch why <crate>` (tier + responsibility) · `arch violations`. JSON on stdout for agent consumption; `DEPENDENCIES.md` is the same data rendered for humans.
- **Tradeoff:** a generated artifact plus an xtask is code to maintain, and exact edge matching will occasionally feel bureaucratic. The alternative — prose-only architecture — is how layering rots. Accepted.
- **Reversal:** if the workspace grows past ~20 crates and exact matching becomes noise, relax check 2 to "no undocumented edges" (an exceptions allowlist instead of full equality). Keep checks 1 and 3 either way.

### 10.3 Gates

Three gates, all invoked through the same justfile recipes locally and in CI. The justfile is the single source of truth; CI is a thin caller.

A1 supplies a clearly labelled bootstrap `just gate` (fmt/clippy/tests); A3
adds cleanup. A4 promotes it to full G0 with arch check and establishes G1.
Only A1–A3 may use bootstrap acceptance; other workstreams wait for A4. Record
warm-G0 timing separately from dependency provisioning/cold build and total
pre-push time including fuzz smoke. Text searches are review aids, never proofs
that code obeys a safety invariant; behavioral/compiler/structural checks decide
acceptance. Network-denied runs inspect attempted connections as well (AC-26).

| Gate | When | Contents | Blocking |
|---|---|---|---|
| **G0** — local fast gate | pre-push hook; manual `just gate` | `fmt --all --check` · `clippy --workspace --all-targets -- -D warnings` · `test --workspace` · `arch check`. Budget: ≤ 5 min on the reference machine | yes (hook bypassable via `--no-verify`; CI still blocks) |
| **G1** — CI gate | every PR, Linux + macOS + Windows | G0 + coverage floors (§10.5) · FaultFs + integration suites · doc build (`-D warnings`) · cargo-deny + cargo audit · cargo-shear · fuzz smoke 60 s/target · network-denied test run · MSRV build | yes (branch protection on `main`) |
| **G2** — release gate | release branch | G1 + signed packaging per OS · perf benchmarks vs §8 thresholds · long fuzz campaigns (≥ 1 h/target) green | yes |

- **Nightly (not blocking):** long fuzz campaigns (AC-20), corpus minimization, coverage and benchmark trends.
- **Weekly (advisory, §7):** build against iced `master`; failure count tracked, does not block.
- **AC-to-gate mapping:** AC 1–11, 18–22, and 32–36 run inside G1's suites; AC 12–17 run in nightly benchmarks and gate G2; AC 23–26 run in G1/G2 per-OS jobs; AC 27–31 gate the infrastructure itself.
- **Post-gate step:** every gate invocation (`just gate`, `just ci-gate`) ends with `just gc`, the size-budgeted cleanup of §10.10; CI additionally runs `just clean` after artifacts are collected.
- **Tradeoff:** the hook is bypassable; the design accepts this because G1 is the real wall and G0 is the feedback loop. A gate that is slow is a gate that gets bypassed — G0's 5-minute budget is a requirement, measured in CI.
- **Reversal:** if the pre-push hook proves useless in practice (high bypass rate), drop the hook and keep `just gate` as documented muscle memory. Never weaken G1 to compensate.

### 10.4 Lint and static checks

| Check | Tool | Gate | Rule |
|---|---|---|---|
| Formatting | rustfmt (edition 2024) | G0 | `cargo fmt --all --check` MUST pass |
| Lint | clippy + `[workspace.lints]` | G0 | `cargo clippy --workspace --all-targets -- -D warnings` MUST pass |
| Architecture | `cargo xtask arch check` (§10.2) | G0 | graph == §4.1 |
| Unused deps | cargo-shear (nightly) | G1 | zero unused dependencies |
| Licenses & bans | cargo-deny | G1 | allowlist only (MIT, Apache-2.0, BSD-2/3-Clause, ISC, Unicode-DFS-2016, Zlib, MPL-2.0); copyleft requires a recorded decision — default reject |
| Vulnerabilities | cargo audit (RUSTSEC) | G1 | zero untriaged advisories; a waiver MUST carry a reason and an expiry date |
| Docs | rustdoc | G1 | `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`; Tier-1/2 crates deny `missing_docs` |
| MSRV | `rust-version` pinned in workspace | G1 | MSRV job builds and tests |
| Supply chain | cargo vet | — | MAY be adopted; deny + audit are the default |

### 10.5 Code coverage

- **Tool:** cargo-llvm-cov (line coverage, all three OSes). Reports are CI artifacts. **No third-party coverage service** — consistent with the no-network posture (§4.10); CI is the only consumer.
- **Floors (defaults; re-validated after Phase 1):**

| Tier | Crates | Floor |
|---|---|---|
| 1 — durability-critical | flavite-fs, flavite-syntax, flavite-buffer, flavite-vault, flavite-index | ≥ 90% line |
| 2 | flavite-api, flavite-graph | ≥ 80% line |
| 3 — measured, not gating until Phase 6 | flavite-cli, flavite-editor, flavite-ui | reported only |

- **Empty crates:** a crate with zero executable lines reports N/A and is inactive for floors; the first executable implementation activates its floor in the same slice. Missing coverage for an active crate fails; empty crates never report a fictional 100%.
- **Measurement:** G1's Linux job merges coverage from unit + integration + FaultFs suites (`cargo llvm-cov --workspace --lcov`). Floors are enforced on Linux; macOS/Windows jobs run the suites and report coverage but do not gate — platform-specific code cannot be covered on Linux.
- **Ratchet:** a PR MUST NOT decrease a gating crate's measured coverage (CI compares against `main`'s lcov artifact). Floors ratchet up, never down, without a recorded reason in the PR.
- **Waivers:** a line that cannot be covered portably (platform-specific syscall) MAY be excluded from measurement with a comment citing the reason; exclusions are counted in the coverage summary and reviewed at floor-ratchet time.
- **Tradeoff:** coverage gates can block legitimate work on hard-to-test seams; the waiver mechanism and per-OS policy exist to keep the gate honest rather than ignored.

### 10.6 Fuzz and property-testing infrastructure (development-time)

- **Layout:** `fuzz/` is a cargo-fuzz workspace, excluded from `default-members` so application builds never compile it. Fuzzing uses the nightly toolchain (installed by `just setup`).
- **Targets** — each maps to a §4.10/§8 surface, and each MUST enforce the input budgets (`MAX_NOTE_BYTES`, `ATTACHMENT_HASH_MAX`, embed depth/size, image limits, per-input parse time); exceeding a budget is a finding:

| Target | Fuzzes | Key invariant |
|---|---|---|
| `parser` | pulldown-cmark event stream over arbitrary bytes | no panic, no hang, budget respected |
| `scanner` | flavite-syntax inline scanner | spans in-bounds and non-overlapping; code/math excluded |
| `frontmatter_patcher` | byte-preserving YAML patcher | result is valid YAML; comments and quoting preserved; unpatchable input → refuse, never corrupt |
| `rename_planner` | D9 plan/stage/apply/recover over FaultFs | AC-2 invariants hold at every crash point |
| `path_safety` | canonicalization / traversal / ADS / reserved-name normalization | no resolved path escapes the vault root |
| `parity` | differential harness (D10, AC-5): grammar-generated documents through both parsers | `editor_link_spans == indexer_link_spans` |
| `image_limits` | hostile images through `image::Limits` | no OOM, no panic (decompression-bomb regression) |

- **Corpora:** `fuzz/corpus/<target>/` holds curated, minimized seeds, checked in. Every bug found by fuzzing MUST land its reproducer in the corpus in the same PR that fixes it. Nightly runs `cargo fuzz cmin`.
- **Dev loop:** `cargo fuzz run <target>` for local campaigns; `just fuzz-smoke` runs every target 30 s locally / 60 s in G1 over seeds plus generated input; nightly CI runs ≥ 1 h per target (AC-20) and uploads corpus artifacts. Crash triage: `cargo fuzz fmt` the artifact, check in the reproducer, fix and seed in one PR.
- **Property tests** (in-crate, proptest — not libfuzzer): index-equivalence model test (AC-4), TOCTOU interleaving model test (AC-8), fault-injected save property (AC-1, ≥ 10,000 cases), EOL round-trip (AC-7), D7 conflict-table tests (AC-3).
- **FaultFs** lives in `flavite-fs` behind a `fault` cargo feature — a library, not `#[cfg(test)]`, because fuzz targets and integration tests both link it: scriptable modeled operation/sub-operation boundaries plus external-writer hooks; the two crash models are defined in §10.6.1.
- **Tradeoff:** this is real, maintained infrastructure — a fuzz workspace, corpora, nightly compute. It is the cheapest existential-risk insurance the project can buy, and the product's identity (D1) is durability. Accepted without reservation.

#### 10.6.1 Crash models and proof limits

`MemFs` has separate volatile file bytes, durable bytes, volatile directory
entries, and durable directory entries. File fsync persists file contents;
directory fsync persists name creation/replacement/removal. `FaultFs` records
before/after boundaries and internal short-write/progress boundaries, rather
than pretending an entire write_new call is one native syscall.

- **ProcessCrash:** stop the simulated process, release ownership, and retain
  filesystem state visible to the OS; revive a new Session/worker set.
- **PowerLoss:** discard/reorder only operations not made durable by the model's
  sync rules. Enumerate legal old/new namespace outcomes for unsynced atomic
  replace, and partial temporary-file writes. Acknowledged durability requires
  the file and namespace syncs; pinned history/checkpoints use the same rules.
- Inject short writes, prefix-written-then-error, disk-full, failed/unsupported
  fsync, missing/renamed temps, and external writes. Seed omitted fsync and
  unchecked short-write defects to prove the harness can expose them. Deterministic
  scripts drive both models; randomized tests record seeds and full traces.

C2 records the supported per-filesystem persistence assumptions; C5 checks OsFs
against model operation traces and platform integration tests. Native process-kill
tests verify process recovery; they do not simulate a machine losing power.
Passing a model establishes the algorithm under that model, never all storage
hardware/driver behavior. Unsupported/lying fsync is an explicitly weaker mode
with a visible durability status; no ordinary durable-success claim is made.
FaultFs model code and OsFs have independent conformance cases (AC-36).

### 10.7 CI topology and forge neutrality

- Default forge: GitHub Actions until a remote says otherwise. All gate logic lives in justfile recipes; workflow files are thin callers (`just ci-gate`), so moving forges is a porting exercise, not a redesign.
- Matrix: G1 on ubuntu-latest, macOS-latest, windows-latest; coverage + fuzz smoke + the network-denied run on Linux only. Nightly: long fuzz + benchmarks. Weekly: iced-`master` advisory build.
- Branch protection: `main` requires G1, one approving review, and resolved threads.

### 10.8 Developer onboarding

- `just setup` (first command after clone): installs rustfmt, clippy, llvm-tools-preview, cargo-deny, cargo-audit, cargo-shear, cargo-llvm-cov, cargo-fuzz, and the nightly toolchain; installs the pre-push hook (G0 + fuzz smoke).
- `rust-toolchain.toml` pins an exact stable release; MSRV is enforced by the G1 MSRV job, not by the pinned toolchain.
- **Tradeoff:** setup is per-OS, dev-only, unshipped code. It is verified by a weekly "fresh clone → `just setup` → `just gate`" CI job on each OS, not by users.

### 10.9 Self-check: what breaks in the infrastructure itself

| Failure | What breaks | Fold-in |
|---|---|---|
| G0 exceeds 5 minutes | devs bypass the hook | budget is a measured requirement; G1 is the backstop |
| Coverage floor blocks a legitimate PR | gate atrophy | per-OS policy, commented waivers, ratchet against `main` |
| Layering false positive (dev-dependency on iced in a core crate's tests) | noise, bypass | rules check normal + build deps only; dev-deps listed, not gated |
| Generated artifact hand-edited | silent drift | drift check in G0/G1; regeneration is one command |
| Corpus grows unboundedly | slow smokes, repo bloat | nightly `cmin`; per-target seed budget |
| justfile accumulates logic | a second build system | recipes stay thin wrappers; logic lives in xtask/cargo |
| Hook not installed (fresh clone) | G0 skipped | `just setup` is onboarding step 1; weekly fresh-clone CI job verifies the path |
| deny.toml rejects a needed crate | blocked dependency | waiver = recorded decision with reason; copyleft default reject |
| Fuzz target flaky on loaded CI | false red | per-target time budget, retry-once policy; persistent flakes get reduced iterations and a tracking issue |
| `target/` grows past the disk budget mid-gate | gate fails on ENOSPC; machine wedged | post-gate `just gc` with soft/hard budgets (§10.10); CI cleans after artifact collection | gc report line in gate output; CI disk-usage metric |

### 10.10 Post-gate cleanup and disk budgets

**Decision:** build output is this project's largest disk consumer — debug, release, fuzz-instrumented, and coverage-instrumented builds of an iced/wgpu/tantivy workspace routinely total tens of GB — so every gate invocation MUST end with a cleanup step, and a manual `just clean` MUST exist. The step is git-aware, size-budgeted, and dry-run capable.

- **What is cleaned, and what is never touched:**

| Path | Removed by | Notes |
|---|---|---|
| `target/` (debug, release, incremental, llvm-cov) | `just gc` (incremental only), `just clean` (full) | largest consumer |
| `fuzz/target/` | both | separate workspace target dir |
| `fuzz/artifacts/` | both | crash artifacts; a reproducer is `cargo fuzz fmt`-ed into the corpus before its artifact is deleted |
| `*.profraw`, `*.profdata`, `target/llvm-cov/` | both | coverage intermediates |
| `fuzz/corpus/<target>/` | **never by `gc`/`clean`** | checked-in seeds are tracked files; untracked entries are kept for nightly `cmin`; only the explicit `just clean --corpus` removes untracked entries |
| `~/.cargo` | **never** | shared user state, not ours |
| anything outside the workspace root | **never** | the script MUST verify it runs at the workspace root and MUST NOT follow symlinks out of it |

- **Commands:**
  - `just clean` — full removal of the table's first four rows. `just clean --dry-run` prints per-path sizes and removes nothing. Freed bytes are reported.
  - `just gc` — the post-gate step, threshold-based. After measuring `target/` + `fuzz/target/`: at or under the soft budget (default **10 GB**) → no-op; over → remove `target/*/incremental`, `fuzz/target/`, and profiling intermediates, then re-measure; still over the hard budget (default **20 GB**) → escalate to `just clean` and print a warning that the next gate rebuilds from scratch.
- **Wiring:** `just gate` and `just ci-gate` invoke `just gc` as their final step; CI additionally runs `just clean` after artifacts are collected. The G2 job (release build + benchmarks + fuzz in one job) is the disk-critical case.
- **Why not clean unconditionally after every local gate:** wiping `target/` destroys the incremental cache, and the next gate pays a full rebuild — slow gates are bypassed gates (§10.9). `gc` bounds disk without taxing the feedback loop.
- **Tradeoff:** `gc` adds a `du` pass (OS-cached, sub-second warm) to every gate, and hard-budget escalation can surprise a developer with a cold next gate. Accepted; the alternative is a full disk and a wedged machine.
- **Reversal:** if the `du` pass is ever measurable in gate time, sample the measurement (at most once per hour) instead of per-gate.

---

## Appendix A: Changelog, v1 → v2

| ID | Review finding (round 1) | Resolution in v2 |
|---|---|---|
| R1 | TOCTOU race contradicted the history guarantee: `save` snapshotted only on detected conflict | D6/§4.4: **unconditional** content-addressed snapshot of prior bytes on every overwrite; save reordered so the temp file is staged *before* the race window; conflict branch preserves the other writer's bytes; residual risk stated honestly; retention changed to time tiers (autosave made a flat version count meaningless) |
| R2 | `vault.toml` was load-bearing but classified ignorable; deleting `.flavite/` orphaned history/recovery | D2/§4.4: vault registry in OS data dir keyed by canonical path; `vault.toml` authoritative when present, registry reconstructs it with the **same** UUID |
| R3 | Attachments were unmodeled: every image embed resolved `unresolved` | §4.3: `attachments` table + `links.dst_att`; D8 unified resolution (embed prefers attachment, link prefers note); D9 journals attachment renames; D15 attachment graph nodes; §4.8 size caps; AC-11 |
| R4 | Autosave × tantivy commit per save = commit churn | D12: deferred commits, commit-before-search, `tantivy.committed_generation`, 5 s idle timer; AC-15 |
| R5 | One bounded channel, no drop policy; bulk load could drop conflict notices | D3/§4.2/§4.5: pulse (lossy) vs. notice (lossless, coalesced per key) channels; AC-17 |
| R6 | T3 content-hash inference silently moved pins and re-keyed buffers on copy-note | D8: tiered inference T0–T4; only T0–T2 carry identity state silently; T3 prefers same-directory |
| R7 | Undo of a rename patch didn't cancel the journal entry; crash could resurrect it | Original v2 used recovery-file hashes; superseded by durable checkpoints/cancellation markers and lifetime rules in §4.15; AC-10 |
| R8 | Q2 open: first-party sync / CRDT? | **Closed: no.** ADR-0002. Buffer model (D7/D14) is final |
| R9 | Q1 open: can iced's primitives support D14? | **Closed: no.** Custom editor is the decision. ADR-0001. S1 rescoped to custom-path viability (IME event delivery, caret geometry, AccessKit, markdown widget coverage) |
| R10 | Mobile unaddressed | §0: explicit non-goal |
| R11 | Obsidian relationship unstated | §0: interoperability posture — inspired by, not a drop-in; `.obsidian/` never read or written |
| R12 | Mixed-EOL files would be normalized on save | §4.8/D6: per-line EOL side table in `EditorModel`; byte-exact round-trip; AC-7 |
| R13 | Obsidian frontmatter keys unmapped; title derivation unspecified | D11: `aliases`/`tags`/`tag`/`title` mapped on parse; title = frontmatter > first H1 > stem |
| R14 | Lockfile in `.flavite/` would be synced cross-machine | §4.12: lock + IPC in OS data dir only; advisory locks die with the process; cross-machine unsupported, documented |
| R15 | S1 checklist gaps | §9: S1 adds undo-grouping determinism, multi-cursor, paste-from-HTML sanitization, cursor mapping across reload, HiDPI hit-test precision |
| R16 | Recovery interval hardcoded | D6: configurable, default 2 s |

### v2 → v2.1 (post-review edits)

| ID | Change | Location |
|---|---|---|
| E1 | Design target extended: "…can visualize all links between their notes as they build their knowledge base" (editor) | §0 |
| E2 | Technology-stack list added to the blast-radius section (editor) | §2 |
| E3 | Restored ADR-0001 reversal bullet, ADR-0002, and the end marker — accidentally truncated during editing; restored verbatim | Appendix B |
| E5 | Durability/identity review: exclusive startup; bounded invalidation log and epoch/search checkpoints; prepared/committed journal and durable undo; verification scans; native paths and text coordinates; explicit crash models and bootstrap gates | §4.13–4.16, §10.6.1; AC-1/2/3/5/6/9/10/22/26/27 amended; AC-33–36 added; plans A–D |
| E4 | Command-contract amendment: headless Session owns BufferManager; registry validates typed commands; revisions, correlated operation replies, save milestones/partial failures, and GUI/CLI parity are explicit. Canonical document path corrected to this directory | D3/D6/D7/D9/D15/D16; §4.2/§4.4/§4.5; AC-32; plans A–D |
| R17 | Project setup: gates G0/G1/G2, lint stack, project-RAG (architecture graph + verification + query CLI), tiered coverage floors, development fuzz/property infrastructure, CI topology, onboarding | §10; AC-27–30; §9 Phase 0.0 |
| R18 | Post-gate cleanup for build-artifact disk growth: size-budgeted `just gc` after every gate, manual `just clean` with dry-run, git-aware (never removes tracked files or corpus seeds) | §10.10; §10.3; §10.9; AC-31 |

## Appendix B: Architecture decision records

### ADR-0001: Custom editor widget (Q1 — closed)

- **Decision:** Lattice builds a custom iced editor widget over the headless `EditorModel`. The stock `text_editor` is not a v1 fallback.
- **Context:** Q1 asked whether iced's current text primitives can support D14 (caret geometry, IME preedit, custom-span hit-testing, deterministic undo grouping, shared buffer). Answer: **no**.
- **Consequences:** D14's costs are accepted in full (IME, selection, bidi, a11y are ours). The `EditorSurface` trait is retained for testability and test substitution. Spike S1 verifies the custom path's one remaining external dependency: iced/winit delivering IME events to a custom widget on all three OSes, plus cosmic-text caret geometry and the AccessKit story.
- **Reversal trigger:** S1 shows IME event delivery is impossible without forking iced, upstream patches are rejected or stall beyond one release cycle, *and* the pure-Rust constraint is renegotiated. Last resort only: a webview-based editor. The decision is revisited, never silently drifted.

### ADR-0002: No first-party sync; no CRDT buffers (Q2 — closed)

- **Decision:** Lattice does not implement sync or real-time collaboration. File-level sync via external tools (git, Syncthing, iCloud) is the supported model, and the design is safe under those tools. Buffers are not CRDTs and will not become CRDTs in this architecture.
- **Context:** Q2 asked whether first-party sync or real-time collaboration is in the product's future. Answer: **no**. The question conflated two things — first-party *sync* and *concurrent same-buffer editing* — and CRDTs are only required by the second. File-level sync, even a future first-party sync service, works at file granularity through the D7 conflict protocol.
- **Consequences:** The D7/D14 buffer model is final; no CRDT-compatibility shim is required beyond `EditOp` position mapping, which the design already has. The "last cheap moment to change the buffer model" is removed — there is no pending fork.
- **Reversal trigger:** A committed roadmap item for real-time multi-user editing of the same note. At that point D7/D14 change shape and this ADR is superseded by a new one. Absent that trigger, this decision stands.

---

*End of RFC-0001 v2.1.*


