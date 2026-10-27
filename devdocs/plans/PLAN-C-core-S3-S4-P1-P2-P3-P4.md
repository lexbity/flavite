# PLAN C — Core workstream: filesystem-truth spike (S3) + search/DB spike (S4) + core (P1) + buffer & safety (P2) + rename/move transactions (P3) + API surface (P4)

**Specification:** RFC-0001 v2.1 with command and durability/identity amendments (§4.5/4.13–4.16/10.6.1) — canonical copy `./SPEC-draft-v2.md`; synchronized mirror `./RFC-0001-v2.1-spec.md`.
**Spec phases owned:** §9 Phase 0 spikes S3 and S4, Phase 1 (core: fs, syntax, index, vault, cli), Phase 2 (buffer and safety), Phase 3 (rename/move transactions), Phase 4 (API surface).
**Acceptance criteria owned:** AC-1, AC-2, AC-3, AC-4, AC-5 (parser side), AC-6, AC-7, AC-8, AC-9, AC-10, AC-11, AC-18, AC-19, AC-20 (fuzz targets), AC-21, AC-22, AC-26 (core makes no network calls), AC-32 (command execution, shared with consumer plans), AC-33 (index/feed recovery), AC-34 (content verification), AC-35 (coordinates/native identity), AC-36 (fault-model conformance, shared with PLAN-A).
**Depends on:** PLAN-A A1–A4 (gates, FaultFs, fuzz workspace). **Feeds:** PLAN-A A7–A11 (scanner, EditorModel, api), PLAN-B B5–B9 (index feed, BufferManager, api), PLAN-D D1–D8 (everything).
**Coverage burden:** this plan carries the Tier-1 floors — flavite-fs, flavite-syntax, flavite-buffer, flavite-vault, flavite-index ≥ 90 % line each (§10.5); flavite-api ≥ 80 % (Tier 2); flavite-cli measured (Tier 3).

## 0. How to read this plan

Every slice is a **vertical cut** through the specification — implemented,
runs, rigorously proven, honestly validated — not a horizontal layer. The
"fully and honestly" bar (working end-to-end behavior; verification built
during the slice; debt paid in touched code; rigor in choices, edge cases, and
error paths; coherent shippable state; no false completion claims) applies to
every slice — see `README.md` in this directory. Locked spec decisions are
cited by number; an agent executes them, it does not re-design them. This
workstream is where the product's identity lives: **D1 — files are truth;
everything else is derived.** The acceptance criteria it owns (AC-1–11, 18–22)
are the durability proofs.

## 1. Workstream context

§9 orders this work by dependency: spikes S3/S4 prove the filesystem and
search/DB designs before the core is built on them; Phase 1 builds the core
(headless, no GPU); Phase 2 adds the buffer and safety machinery that can start
once `fs` and `syntax` exist; Phase 3 adds rename/move transactions; Phase 4
adds the API surface in parallel with 2–3. Everything here is testable without a
GPU (§4.1) — that is the point of the crate boundary.

Two spec algorithms are quoted verbatim in the slices that implement them
(§4.4's `save`, `reconcile`, `open_vault`; D9's journal protocol) — the slices
implement the spec, not a paraphrase of it.

## 2. Slice dependency graph

```
A4 → C1 → C2 → C5                   [filesystem evidence/implementation]
A4 → C3, C4, C6; C6 → C10          [search, DB, shared text types, then buffer]
C3 + C4 + C6 → C7                   [index]
C5 + C6 + C7 + C10 → C8 → C9        [Session/actors, then CLI]
C8 + C10 → C11 → C12 → C13          [save, journal, rename inference]
C7 + C8 + C9 + C10 → C14            [initial registry/facade + CLI migration]
C11–C13 and C14: whichever lands later adds registry wiring/conformance checks
                  (no reverse crate dependency or hard execution-order edge)
```

C8 also needs C10's BufferManager/revision foundation to ship the Session.
C10 needs C6's pure TextSnapshot/coordinate types; it has no C8 dependency. C8's filesystem
actor work can start earlier, but its slice exits after Session integration.
C12 needs C7 (links table), C10 (buffer-applied patches), C11 (durable checkpoint
lineage). C13 needs C8 (reconcile's vanished/changed sets) and C12 (T0
in-app renames). C14 needs C7 (Query AST), C8 (Pulse/Notice), C10 (BufferState)
and migrates C9's CLI onto the registry.

**Command-contract staging (§4.5).** C8/C10 establish the headless Session,
typed engine commands, and operation transport before C14's public facade.
C11–C13 add save/conflict/rename handlers and their AC-32 scenarios. C14 may
land in parallel with those slices: register only implemented handlers, report
`Unavailable` for absent ones, and extend the registry plus facade tests in the
slice delivering each handler. Early facade checks use deterministic worker
transcripts; production I/O checks belong to C11/C12. PLAN-C exits only after
all handlers and cumulative AC-32 checks are complete. This creates no
vault → api dependency and no placeholder implementation of future behavior.

## 3. Locked decisions this workstream implements (do not re-open)

| Spec ref | Decision | Where it lands here |
|---|---|---|
| D1 | Files are truth; the index is fully reconstructible; deleting the cache loses nothing | C7 (rebuild paths), C8 (reconcile), AC-21 tests |
| D2 | Index outside the vault, keyed `<vault-uuid>/v<schema-major>/`; `vault.toml` authoritative, registry re-mints the same UUID (R2) | C8 `identity.rs` |
| D3 | No async runtime; named threads + bounded channels; exactly one index writer; read pool; pulse (lossy) vs notice (lossless) channels (R5) | C8 channels, C7 pool, C14 channel types |
| D4 | Eventually-consistent index; read-your-writes for own saves (commit-before-search for search) | C7, C11 `save.rs` |
| D5 | Reconcile by idempotence; watcher is a hint; racy-file rule; drift counter; startup race ordering | C1 (probe), C8 `reconcile.rs` |
| D6 | Atomic save; unconditional history snapshot (R1); tiered retention; recovery files; residual TOCTOU risk stated | C11 |
| D7 / §4.5 | Session owns one BufferManager; revisioned commands/read views; conflict protocol; autosave disabled in Conflicted | C8 Session, C10 model, C11 save, C14 facade |
| D8 | Link identity/resolution (5 steps); lazy IDs; tiered rename inference T0–T4 (R6); duplicate-ID handling | C7 `resolver.rs`, C13 |
| D9 | Rename/move rewrites are a journaled roll-forward transaction; cancel-on-undo (R7) | C12 |
| D10 | One link grammar, two parsers; scanner is the single semantics source; differential parity fuzz | C6 |
| D11 | Frontmatter edits are byte-preserving patches, never re-serialization; well-known key mapping (R13) | C6 `frontmatter.rs` |
| D12 | SQLite is the record; tantivy a derived view with deferred commits, commit-before-search, `committed_generation` (R4) | C3 (probe), C7 `tantivy.rs` |
| D13 | Query-friendly schema; typed Query AST only; no SQL for callers | C7 `query.rs` |
| D16 / §4.5 | Same Session commands/results for GUI and CLI; typed registry dispatch; operation completion separate from events/notices | C9 (Session driver), C14 (api migration), AC-32 |
| D17 | The vault is hostile input: no read/write outside the vault, no execution, bounded resources | C5 `path.rs`, C6 limits, C7 budgets |
| §4.10 budgets | `MAX_NOTE_BYTES` 32 MB; `ATTACHMENT_HASH_MAX` 64 MB; image ≤ 64 MP / ≤ 64 MB decoded; embed depth 5 + size budget | C5, C7, C8 |
| §4.12 | Single-instance advisory lock + focus IPC in the OS data dir only; no lock artifact in the vault (R14) | C8 `instance.rs` |
| §10.6 | FaultFs is a library behind feature `fault`; fuzz targets enforce budgets; corpora checked in | A2 (FaultFs), C5/C6/C12 (targets) |

## 4. Crate dependency contract

Full table + interpretation notes in PLAN-A §4 (encoded in
`devdocs/architecture/crates.toml`). Edges this plan creates: `index → syntax`, `buffer → syntax` (shared text coordinates),
`vault → {fs, syntax, index, buffer}`, `api → {vault, index, buffer}`,
`cli → api` (after C14's migration; before it, `cli → vault` directly — the
migration is C14's explicit content), `graph → index` (PLAN-B B5).

## 5. Slices

**Review amendment integration:** §4.13–4.16 and §10.6.1 are normative.
C2 proves native-root/containment/persistence primitives through safe production
wrappers; raw platform FFI may remain in xtask probes, not unsafe core code.
C5 implements native PathKey vs lookup-key separation and opened-target checks;
C6/C10 share TextSnapshot/SourceByte/TextByte mapping. C7 owns AC-33, C8 AC-34,
C5/C6/C10/C13 share AC-35, and A2/C5/C11/C12 share AC-36. Each associated
slice carries these checks in G1; PLAN-C exits only when all are complete.

### Slice C1 — S3 slice 1 of 2: watcher-truth probe (per-OS event behavior)

**Spec basis:** §9 Phase 0 spike S3 ("watcher behavior per OS (rename events,
overflow, coalescing)"), D5 (events are hints; overflow → full reconcile;
periodic + focus reconcile; drift counter; racy-file rule; startup race).
**Depends on:** PLAN-A A1–A4. **Feeds:** C5 (`watch.rs` wrapper parameters), C8 (coalescing constants).
**Vertical thread:** a probe binary records raw `notify` event streams for scripted
filesystem scenarios on each OS; the resulting per-OS matrix confirms or
adjusts D5's parameters with evidence.

**Goal.** Per-OS evidence for D5's watcher design: rename event pairing
(inotify cookie / FSEvents flags / `RENAMED_OLD`+`RENAMED_NEW`), overflow
behavior, coalescing, and silent loss — confirming or adjusting the 300 ms quiet
window, 2 s max window, 500-events/s bulk threshold, and 2 s racy-file rule.

**Decisions already locked.**
- Watcher events only *nominate paths* for reconcile; all change handling goes
  through idempotent `reconcile(scope)` (D5).
- Overflow signals trigger full reconcile; periodic (10 min) + focus (throttled)
  reconcile catch silent loss; `reconcile.drift_found_total` counts drift (D5).
- Any file with `mtime ≥ scan_time − 2 s` is racy and re-hashed next pass (D5).

**What to implement.**
- `xtask/src/bin/s3_watcher_probe.rs` — uses `notify` directly (the same crate
  and version the wrapper will use). Scenarios, each run per OS (Linux inotify,
  macOS FSEvents, Windows ReadDirectoryChangesW):
  1. single-file modify → event latency distribution;
  2. rename pairing: write A, rename A→B — record whether old+new arrive as a
     pairable unit (cookie/flags) on each OS;
  3. create/delete storm (10k files) → count events vs. operations; detect
     overflow signals; measure coalescing;
  4. burst pattern → measure the effective quiet window the OS imposes.
- `devdocs/spikes/S3-watcher.md` — the per-OS event matrix + parameter
  confirmations/exceptions (SHOULD-level exceptions documented per the spec's
  Conventions, each with a reason).
- Keep the probe as a dev tool: CI re-runs scenarios 1–3 on Linux headless and
  verify eventual path nominations/overflow handling. Raw event counts are
  measurements, not expected to equal operation counts.

**How to implement.**
1. Write the probe with deterministic scenarios (fixed file counts, fixed
   names, tempdir vault).
2. Run per OS (CI for Linux; recorded manual runs for macOS/Windows — labeled
   honestly as manual evidence).
3. Compare the OS-imposed behavior against D5's parameters; record
   confirmations and exceptions.

**Tests, validation, verification.**
- Probe self-check: changed paths are eventually nominated, or explicit
  overflow/fallback is observed. Record duplicates/coalescing/loss; never
  equate event count with operation count (CI Linux).
- `cargo test -p xtask` — probe scenario builders unit-tested.
- Grep gates:
  - `grep -rn "TODO\|FIXME\|TBD" devdocs/spikes/S3-watcher.md xtask/src/bin/s3_watcher_probe.rs` — empty.
  - `grep -c "Linux\|macOS\|Windows" devdocs/spikes/S3-watcher.md` — all three OSes recorded.
- `just gate` green.

**Exit criteria.**
- [ ] S3-1 recorded: per-OS watcher matrix in `devdocs/spikes/S3-watcher.md`.
- [ ] D5's coalescing/bulk/racy parameters confirmed or adjusted with
      documented exceptions.
- [ ] CI happy-path watcher test green on Linux.

### Slice C2 — S3 slice 2 of 2: filesystem-semantics probe (atomic replace, fsync, placeholders, advisory locks)

**Spec basis:** §9 Phase 0 spike S3 ("atomic-replace semantics on Windows and a
network FS, cloud placeholder detection (notes and attachments), advisory-lock
behavior after `kill -9`"), D6 (atomic write; symlinks resolved so we write the
target; MUST NOT replace a symlink with a regular file), §4.8 (cloud placeholders
never hydrated by background scans), §4.12 (advisory locks die with the process).
**Depends on:** C1, PLAN-A A2 (Fs trait exists — this slice may adjust its method
contracts from evidence, updating A2's trait + tests in this slice). **Feeds:**
C5 (`os.rs`, `path.rs`, `atomic.rs`, `watch.rs`), C8 (`instance.rs`).
**Vertical thread:** a probe binary verifies, per OS: rename-over semantics
(metadata/ACL preservation, symlink behavior), fsync behavior, cloud-placeholder
detection attributes, and that an advisory lock is acquirable immediately after
the holder is `kill -9`ed.

**Goal.** Per-OS evidence for the `Fs` trait's method contracts: `rename_replace`
primitive choice (Windows `ReplaceFileW` vs `MoveFileEx`), symlink handling,
fsync honesty (incl. network FS caveats), placeholder detection, and advisory-lock
release on process death.

**Decisions already locked.**
- Atomic write: temp file in the **same directory**, write, copy permissions,
  `fsync`, rename-over (`rename(2)`; `ReplaceFileW` on Windows), `fsync` the
  directory (D6).
- Symlinks are resolved so we write the target; we MUST NOT replace a symlink
  with a regular file (D6).
- Cloud placeholders are detected via platform attributes and **not hydrated by
  background scans** — notes and attachments alike (§4.8).
- Advisory locks are released by the OS on process death — a crash leaves no
  stale lock (§4.12). Root ownership also follows §4.13.
- C2 must prove same-root alias identity, opened-target containment, no-clobber
  rename, and file/directory persistence through safe production wrappers.
  Record unsupported filesystem behavior and its weaker durability mode; raw
  FFI in the probe does not establish a safe production implementation.

**What to implement.**
- `xtask/src/bin/s3_fs_probe.rs` — scenarios:
  1. rename-over on Windows: compare `std::fs::rename` (`MoveFileExW`) vs
     `ReplaceFileW` (via `windows-sys`) for metadata/ACL preservation and
     symlink behavior; record which the trait must use.
  2. symlink: rename-over a symlink must not clobber the link (D6) — verify the
     chosen primitive per OS.
  3. fsync: file + directory fsync timing; note network-FS (SMB/NFS) caveats
     (documented as best-effort, §4.9).
  4. cloud placeholders: Windows `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`;
     macOS dataless-file detection (stat flags / `com.apple.*` xattrs — record
     what is reliably detectable); Linux (no standard — record "treat as
     normal").
  5. advisory lock: child process acquires flock/LockFileEx, parent `kill -9`s
     it, lock acquirable immediately after.
- `devdocs/spikes/S3-fs.md` — the semantics matrix; finalize the `Fs` trait
  method contracts from the evidence (adjust A2's `fs.rs` if needed, with
  tests updated in the same slice).

**How to implement.**
1. Implement the probe scenarios (Windows-specific code behind `cfg(windows)`,
   etc.).
2. Run per OS (CI: Linux + Windows + macOS jobs; network-FS scenario recorded
   as a lab test or documented as unavailable — honest labeling).
3. Finalize the trait contracts; update A2's `Fs` trait + MemFs/FaultFs to
   match (debt paid in the slice that touches it).
4. Record the matrix.

**Tests, validation, verification.**
- Probe assertions run in CI per OS (the kill -9 lock test, placeholder
  detection, rename-over semantics on Windows).
- `cargo test -p flavite-fs --all-features` — green after any trait adjustment.
- Grep gates:
  - `grep -rn "TODO\|FIXME\|TBD" devdocs/spikes/S3-fs.md xtask/src/bin/s3_fs_probe.rs` — empty.
  - `grep -c "ReplaceFileW\|rename(2)\|MoveFileEx" devdocs/spikes/S3-fs.md` — primitive decision recorded.
- `just gate` green.

**Exit criteria.**
- [ ] S3-2 recorded: `devdocs/spikes/S3-fs.md` with the per-OS semantics matrix.
- [ ] `Fs` trait contracts finalized (A2's trait adjusted if evidence demands,
      tests updated in the same slice).
- [ ] Advisory-lock-after-kill -9 proven on all three OSes.

### Slice C3 — S4 slice 1 of 2: search probe (CJK quality + deferred commits under autosave load)

**Spec basis:** §9 Phase 0 spike S4 ("tantivy CJK bigram quality, deferred-commit
behavior under autosave load (commit-before-search latency,
`committed_generation` replay)"), D12, §4.7 (search UX), §8 AC-15.
**Depends on:** PLAN-A A1–A4. **Feeds:** C7 (`tantivy.rs` design + tokenizer).
**Vertical thread:** a probe builds a 20k-note CJK-heavy fixture, measures
bigram-tokenizer quality on a curated JA/ZH/KO query set, then simulates 1 s
autosave writes and measures commit frequency, commit-before-search latency, and
`committed_generation` replay after a simulated crash.

**Goal.** Evidence for D12: (a) is the CJK bigram tokenizer good enough, and
(b) does deferred commit + commit-before-search + generation replay behave under
autosave load — including the go/no-go on tantivy vs the FTS5 reversal.

**Decisions already locked.**
- Tokenizer pipeline: `SimpleTokenizer → LowerCaser → diacritic folding`, with
  **bigram tokenization for Han/Kana/Hangul runs** (D12).
- Deferred commit: documents applied to the tantivy `IndexWriter` immediately,
  `commit()` deferred — commit-before-search; otherwise at most every 5 s while
  dirty; at end of bulk reconcile; on clean shutdown (D12, R4).
- Index version is epoch + generation; the search checkpoint is published
  atomically with its durable commit. Replay bounded invalidations including
  deletions from a consistent SQLite snapshot/search_docs; missing log coverage
  or epoch mismatch rebuilds search (§4.14). The SQLite mirror is advisory.
- Reversal (recorded, not silent): if drift incidents appear or CJK quality is
  mediocre, collapse to SQLite FTS5 (ICU tokenizer) via the `SearchBackend`
  trait (D12).

**What to implement.**
- `xtask/src/bin/s4_search_probe.rs` — builds the 20k-note fixture (generated,
  deterministic, CJK-heavy: Japanese kana/kanji, Chinese, Korean hangul notes);
  runs a curated query set with known-relevant documents; measures
  precision/recall of the bigram pipeline vs. the plain pipeline; simulates
  autosave (1 s writes for a 10-minute window) measuring commit count +
  commit-before-search added latency p95; simulates crash between apply and
  commit → reopen → replay timing + correctness. Prove atomic publication of
  the search checkpoint, including crashes before/after the SQLite diagnostic
  mirror update, deletion before search commit, and reader reload (§4.14).
- `devdocs/spikes/S4-search.md` — quality numbers, commit behavior, replay
  numbers, and the tantivy go/no-go (with the reversal decision recorded if
  triggered).

**How to implement.**
1. Build the fixture generator (deterministic seed; ~80 MB total, matching
   §8's rebuild fixture).
2. Implement the probe's index harness against tantivy directly (the same
   APIs C7 will use).
3. Run the quality + load measurements on the reference machine; record.
4. Write the verdict (proceed / reversal) with numbers.

**Tests, validation, verification.**
- Probe assertions: replay correctness (post-replay search results ==
  pre-crash committed results + delta), commit-count bound under autosave
  (≪ one commit per save), commit-before-search latency p95 recorded.
- CI runs a scaled-down version (2k notes) on Linux.
- Grep gates:
  - `grep -rn "TODO\|FIXME\|TBD" devdocs/spikes/S4-search.md xtask/src/bin/s4_search_probe.rs` — empty.
  - `grep -c "verdict" devdocs/spikes/S4-search.md` — recorded.
- `just gate` green.

**Exit criteria.**
- [ ] S4-1 recorded: `devdocs/spikes/S4-search.md` with quality + commit + replay
      numbers and the tantivy verdict.
- [ ] If the verdict is "reversal to FTS5": the decision is recorded with
      numbers and C7's `SearchBackend` trait becomes the swap point (still
      built in C7 — the trait exists either way).

### Slice C4 — S4 slice 2 of 2: DB concurrency probe (WAL single-writer + read pool under reconcile load)

**Spec basis:** §9 Phase 0 spike S4 ("WAL read/write concurrency under reconcile
load"), D3 (exactly one writer thread owning the SQLite writer connection and
the tantivy `IndexWriter`; readers use a pool of read-only WAL connections),
D12 (generations).
**Depends on:** PLAN-A A1–A4. **Feeds:** C7 (`pool.rs` sizing), C8 (reconcile batch sizing).
**Vertical thread:** a probe runs a simulated 20k-file bulk reconcile against
SQLite WAL with one writer + N readers, measuring read p99 during writes and
tantivy writer behavior — the numbers C7's pool and C8's batching are sized
from.

**Goal.** Evidence for D3/D12's concurrency design: WAL single-writer + read-pool
under reconcile load, and generation-counter behavior under batch applies.

**Decisions already locked.**
- The core MUST NOT depend on an async runtime; named threads + bounded
  channels; the index has exactly one writer thread; readers use a pool of
  read-only WAL connections (D3).
- SQLite `index.generation` bumps inline on every applied change and is
  authoritative (D12).

**What to implement.**
- `xtask/src/bin/s4_db_probe.rs` — simulated bulk reconcile (20k note writes
  batched) with 1 writer + 8 read-only WAL connections; measures read p99
  during writes, write throughput, batch-apply generation bump timing;
  tantivy writer ops during reconcile; open-time generation check timing.
- `devdocs/spikes/S4-db.md` — the numbers; pool sizing and batch-size
  recommendations recorded for C7/C8.

**How to implement.**
1. Build the probe against rusqlite (bundled) + tantivy directly.
2. Run the load scenarios on the reference machine; record.
3. Write the sizing recommendations into the spike doc.

**Tests, validation, verification.**
- Probe assertions: no SQLITE_BUSY on readers during writes (WAL), generation
  monotonic under batches.
- CI runs a scaled-down version on Linux.
- Grep gates:
  - `grep -rn "TODO\|FIXME\|TBD" devdocs/spikes/S4-db.md xtask/src/bin/s4_db_probe.rs` — empty.
- `just gate` green.

**Exit criteria.**
- [ ] S4-2 recorded: `devdocs/spikes/S4-db.md` with concurrency numbers and
      pool/batch sizing recommendations.
- [ ] **S3 + S4 closed** (both spikes recorded; any parameter adjustments
      documented as SHOULD-level exceptions with reasons).

### Slice C5 — P1 slice 1 of 5: `flavite-fs` — `OsFs`, path safety, atomic write, watcher wrapper + `path_safety` fuzz target

**Spec basis:** §9 Phase 1, §4.1 (`flavite-fs`: trait Fs, atomic_write, watcher
wrapper, path safety, FaultFs), D17 (the vault is hostile input), §4.8 (case,
Unicode, filesystem hazards), §4.10 (path traversal, ADS, reserved names,
Windows tricks), §8 AC-19, §10.6 (`path_safety` fuzz target).
**Depends on:** C1, C2 (S3 evidence finalizes the trait — A2/C2 own the trait
definition), PLAN-A A2 (MemFs/FaultFs). **Feeds:** C8 (vault actor uses Fs
+ atomic write + watcher), C11 (save path), fuzz target.
**Vertical thread:** a scripted `FaultFs` test proves `atomic_write` is
crash-safe (file always old-or-new byte-exact, temp orphans listed for
`doctor`); the path-safety suite proves no resolved path escapes the vault
root on any OS — including via symlinks, ADS, and traversal.
**Coverage:** flavite-fs Tier 1 — ≥ 90 % line (§10.5).

**Goal.** The real filesystem layer: `OsFs` implementing the `Fs` trait, path
safety (vault-root containment, Windows tricks, reserved names, NFC), the
atomic-write primitive, and the watcher wrapper.

**Decisions already locked.**
- Resolve through pinned root/parent capabilities and validate the opened
  target (§4.16.2), including symlink-swap races/nonexistent destinations.
  External targets are forbidden in v1; internal symlinks preserve link identity.
- Reject at the path-normalization layer: `\\?\`, ADS (`file.md:stream`), 8.3
  names (§4.10).
- Native PathKey is lossless and unique; normalized lookup keys are nonunique.
  Preserve/report both NFC/case-colliding entries (§4.16.2); never merge them.
- Creating a note from link text sanitizes Windows-illegal characters, reserved
  names, trailing dots and spaces; link text preserved as an alias if the
  filename differs (§4.8, D8).
- Casefolding uses Unicode **default** (locale-independent) folding (§4.8).
- Atomic write per D6 (temp sibling, write, copy permissions, fsync,
  rename-over, fsync dir).

**What to implement.**
- `crates/flavite-fs/src/os.rs` — `OsFs`: full trait impl. Windows:
  replacement/persistence and root-contained handle operations through the
  safe production wrapper proved in C2; raw FFI stays in xtask probes. Core
  `forbid(unsafe_code)` remains enforced. `fsync_file`/`fsync_dir` (dir fsync via
  opened directory fd; Windows: best-effort, documented). `copy_mode` (Unix
  mode bits; Windows readonly attribute). `resolve_symlink` (canonicalize).
  `is_placeholder` per S3-2's per-OS detection. `read_capped` enforces the cap.
- `crates/flavite-fs/src/path.rs` — path safety:
  - encode/decode platform-tagged native PathKey (including non-UTF-8 Unix
    names) separately from NFC/casefold lookup and escaped display text;
  - resolve/open relative to a pinned vault root/parent and validate the actual
    opened target; a prior string containment check alone is insufficient;
  - reject `\\?\` prefixes, ADS (`:` in a filename component), 8.3 short-name
    patterns (`~1` suffixes);
  - reserved names: `CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`
    (case-insensitive, any extension);
  - `sanitize_filename(link_text)` → legal filename (replace
    `<>:"/\|?*` + control chars, trim trailing dots/spaces, handle reserved
    names by prefixing `_`), returning `(filename, alias_if_differs)`;
  - casefold helper (Unicode default folding) for stem keys.
- `crates/flavite-fs/src/atomic.rs` — `atomic_write(fs, path, bytes)`: temp
  sibling `.<name>.flavite-tmp.<rand>` (same directory — same mount, same ACL
  inheritance), `write_new`, `copy_mode`, `fsync_file`, `rename_replace`,
  `fsync_dir`; `list_orphan_temps(dir)` for `doctor`.
- `crates/flavite-fs/src/watch.rs` — the `notify` wrapper: recursive watch,
  per-OS config informed by S3-1, overflow → signal, restart logic, event
  stream to a channel.
- `fuzz/fuzz_targets/path_safety.rs` — fuzzes normalize/containment/sanitize:
  **no resolved path escapes the vault root** (AC-19 fuzz half; §10.6).
- `fuzz/corpus/path_safety/` — seeds (traversal payloads, ADS, reserved names,
  unicode tricks, symlink-ish strings).
- Deps: `notify`, `unicode-normalization`, `blake3` (file hashing helper for
  callers), C2's safe platform wrapper, `fs4` or `fd-lock` (only if needed
  here — instance lock lands in C8; decide and keep the dep list minimal —
  cargo-shear gate).

**How to implement.**
1. `OsFs` method by method, cross-checked against S3-2's matrix.
2. Path safety: implement the checks as pure functions over path strings +
   a `Fs`-backed canonicalization; unit-test each hazard class.
3. `atomic_write` per D6; property-test over FaultFs.
4. Watcher wrapper; test with real FS events + simulated overflow/restart.
5. Fuzz target + seeds; run smoke.

**Tests, validation, verification.**
- **AC-19 path-safety suite** (integration, all 3 OSes in CI): traversal
  (`[[../../etc/passwd]]`-style paths through the resolver-facing API),
  symlink escape (fixture vault with a symlink pointing outside; reads/writes
  rejected, including a concurrent symlink swap), ADS, reserved names, `\\?\`, 8.3.
  Assert: **no read or write outside vault root, ever.**
- **Atomic-write property test** (proptest over FaultFs): random fault scripts
  at modeled write/sync/replace boundaries in both crash models (§10.6.1).
  Check installed old/new bytes and acknowledged durability; keep native
  conformance/process-kill evidence distinct from the PowerLoss simulation.
- AC-35/36: preserve both native normalization-colliding files, round-trip
  native names, detect same-target aliases, and reject conflicting destinations.
  Prove omitted directory sync/short-write regressions fail the model and that
  orphan cleanup retains files referenced by nonterminal journals.
- Watch wrapper tests: event delivery per OS, overflow signaling, restart.
- `cargo fuzz run path_safety -- -max_total_time=30` smoke green;
  `just fuzz-smoke` includes it.
- Coverage: `cargo llvm-cov -p flavite-fs --all-features --lcov` ≥ 90 %.
- Commands: `cargo test -p flavite-fs --all-features` · `cargo clippy -p flavite-fs --all-targets --all-features -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-fs/src fuzz/fuzz_targets/path_safety.rs` — empty.
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` — green; compiler-enforced core `forbid(unsafe_code)`.
  - `grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-fs/Cargo.toml` — no matches.

**Exit criteria.**
- [ ] `OsFs` + path safety + `atomic_write` + watcher wrapper implemented and
      tested; AC-19 suite green on all OSes.
- [ ] Atomic-write property green over FaultFs; `path_safety` fuzz target in
      `just fuzz-smoke`.
- [ ] Coverage ≥ 90 %; `just gate` + `just ci-gate` green.

### Slice C6 — P1 slice 2 of 5: `flavite-syntax` — scanner, frontmatter patcher, pulldown + tree-sitter glue, well-known keys + `scanner`/`frontmatter_patcher`/`parity` fuzz targets

**Spec basis:** §9 Phase 1, D10 (one link grammar, two parsers), D11 (byte-preserving
frontmatter patches; well-known key mapping R13), §4.3 (ParsedNote inputs to the
index: links with spans, tags, title, preview), §4.8 (case/Unicode), §8 AC-5
(parity), §10.6 (three fuzz targets).
**Depends on:** C5 (fs for nothing — syntax is pure; depends on PLAN-A gates
only). **Feeds:** C7 (index consumes ParsedNote), PLAN-A A8/A9 (editor consumes
scanner spans + ts_ranges), fuzz targets.
**Vertical thread:** a fixture document → scanner spans with byte-exact source
ranges (links/embeds/tags/block-ids, code/math excluded); a frontmatter patch
that preserves comments and quoting byte-for-byte; the `parity` fuzz target
asserting editor-side spans == indexer-side spans (AC-5).
**Coverage:** flavite-syntax Tier 1 — ≥ 90 % line (§10.5).

**Goal.** The single source of link semantics (D10): a pure, IO-free inline
scanner for the extensions, a byte-preserving frontmatter patcher, and both
parse glues (pulldown-cmark for the indexer, tree-sitter for the editor) that
feed the same scanner.

**Decisions already locked.**
- `flavite-syntax` owns a small hand-written **inline scanner** for the
  extensions: `[[link]]`, `![[embed]]`, `#tag`, `^block-id`, `id:` form. It runs
  over text ranges that aren't code spans, code blocks, or math. Pure and
  IO-free (D10).
- The indexer: pulldown-cmark determines block/inline structure, then the
  scanner runs on text events. The editor: tree-sitter for block structure,
  highlighting, fenced-code injection; the same scanner runs over the
  tree-sitter's non-code inline ranges. **Link semantics come from exactly one
  function. Tree-sitter never decides what a link is** (D10).
- Frontmatter: read with a YAML 1.2 core-schema parser with alias and depth
  limits; write by **textual patch** (insert or replace a single key's line
  range); MUST NOT parse-and-reserialize (D11).
- Well-known keys: `aliases` (string or list), `tags`/`tag`, `title` mapped
  into typed tables on parse; title = frontmatter `title` > first H1 > stem;
  on write, only `aliases` is ever synthesized (D11/R13, D8 alias strategy).
- Math spans MUST be parsed, excluded from link and tag extraction, and shown
  as styled source (deferred rendering, §0).

**What to implement.**
- `crates/flavite-syntax/Cargo.toml` — deps: `pulldown-cmark`, `tree-sitter`,
  `tree-sitter-markdown`, `unicode-normalization`, `serde`/`serde_json`, a
  YAML 1.2 core-schema parser (evaluate `saphyr` / `yaml-rust2` — license check
  against deny.toml; enforce alias count, depth, node-count limits ourselves).
  No iced-family deps (arch check). Own TextSnapshot and the pure
  SourceByte/TextByte/scalar/grapheme/platform-offset conversion types in
  `src/text.rs`; buffer consumes this contract via buffer → syntax. Decode
  valid UTF-8 only and normalize CRLF explicitly, retaining source/EOL/BOM maps.
- `src/scanner.rs` — the inline scanner over (source, text-mask): emits
  `Span { kind, start, end, payload }`; payload for links = `{ target_raw,
  alias, anchor, by_id, embed }`; tags (incl. nested `a/b`, no spaces, word
  boundary); `^block-id`; `id:` form. Handles nested brackets, escaped
  brackets, alias `|`, heading anchors `#`.
- `src/mask.rs` — text-mask construction: which source ranges are plain text
  (not code span/block, not math, not HTML).
- `src/pulldown_glue.rs` — pulldown-cmark events → mask (block structure:
  headings, code blocks, math; text events → ranges) → scanner →
  `ParsedNote { spans, links, tags, title_h1, preview }` (preview ≤ 1 KB).
  Scanner spans are TextByte on the normalized snapshot; index output maps
  them to SourceByte plus raw hash. Both glues scan that same representation;
  parity compares like coordinates (§4.16.1).
- `src/ts_ranges.rs` — tree-sitter-markdown parse → same mask (non-code
  inline ranges) → the editor and the parity target consume this.
- `src/frontmatter.rs` — YAML 1.2 core parse (limits enforced) + the
  byte-preserving patcher: locate the frontmatter block (`---` fences),
  locate a key's line range (block lists, flow lists, quoted scalars,
  multi-line scalars), insert/replace preserving everything else; **refuse**
  (`PatchRefused`) on: anchors/aliases in the block, multi-line scalars on
  the target key, flow mappings sharing a line, duplicate keys.
- `src/wellknown.rs` — `aliases`/`tags`/`tag`/`title` mapping; title
  derivation; write path (only `aliases` synthesized).
- `src/lib.rs` — `ParsedNote`, `Frontmatter`, `Span` types; crate docs
  (pure, IO-free, §4.1).
- `fuzz/fuzz_targets/scanner.rs` — invariants: spans in-bounds and
  non-overlapping; code/math excluded (§10.6).
- `fuzz/fuzz_targets/frontmatter_patcher.rs` — patch → result is valid YAML;
  comments and quoting preserved; unpatchable input → refuse, never corrupt.
- `fuzz/fuzz_targets/parity.rs` — grammar-generated documents through both
  glues: `editor_link_spans == indexer_link_spans` (AC-5, D10).
- `fuzz/corpus/{scanner,frontmatter_patcher,parity}/` — curated seeds.

**How to implement.**
1. Scanner first (it's the semantics source): fixtures for every extension +
   exclusion; property tests for span invariants.
2. Mask + pulldown glue; offset-mapping property test.
3. ts_ranges; parity harness comparing both glues over generated documents.
4. Frontmatter: parser with limits; patcher; property tests (below).
5. Well-known mapping + title derivation.
6. Fuzz targets + seeds; `just fuzz-smoke` green.

**Tests, validation, verification.**
- Scanner fixtures: `[[link]]`, `![[embed]]`, `[[target|alias]]`,
  `[[target#heading]]`, `[[id:01J9…|alias]]`, `#tag`, nested `#a/b`,
  `^block-id`, code spans/blocks/math excluded, escaped brackets, tags in
  headings.
- Frontmatter patcher property tests (proptest): random YAML docs × patch
  ops → result re-parses as valid YAML; exact bytes outside the permitted
  patch ranges are unchanged (including comments/quoting); unpatchable → `Err`, never
  corrupt output; unpatched docs round-trip byte-exactly.
- AC-35 offset mapping: raw→normalized→raw at legal boundaries preserves
  BOM/mixed EOL and the written token. Include expanding casefolds, emoji/RTL,
  combining marks, edits/undo line split/join, and a dirty-buffer rename patch.
  Parser parity compares TextByte spans; stored rewrites validate SourceByte
  spans against the corresponding content hash.
- Parity: `cargo fuzz run parity -- -max_total_time=30` smoke green;
  `just fuzz-smoke` includes all three targets.
- Title derivation + well-known mapping tests (incl. `tags` vs `tag`, string
  vs list, `no` stays text — the Norway problem, YAML 1.2 core).
- Coverage: `cargo llvm-cov -p flavite-syntax --lcov` ≥ 90 %.
- Commands: `cargo test -p flavite-syntax` · `cargo clippy -p flavite-syntax --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-syntax/src fuzz/fuzz_targets` — empty.
  - `grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-syntax/Cargo.toml` — no matches.
  - `grep -rn "fs::\|std::io" crates/flavite-syntax/src/lib.rs crates/flavite-syntax/src/scanner.rs` — no matches (pure, IO-free — D10).

**Exit criteria.**
- [ ] Scanner + both glues + patcher implemented; AC-5 parity harness live and
      green in smoke.
- [ ] Property tests green (patcher, offsets, spans).
- [ ] Coverage ≥ 90 %; `just gate` + `just ci-gate` green.

### Slice C7 — P1 slice 3 of 5: `flavite-index` — SQLite record + tantivy view + resolver + Query AST

**Spec basis:** §9 Phase 1, §4.3 (schema + Query AST, quoted in the spec),
D12 (two generations, deferred commit), D13 (query-friendly; no SQL for
callers), D8 (resolution), §4.10 (budgets), §8 AC-4 (index equivalence),
AC-15 (search side).
**Depends on:** C6 (ParsedNote), C3/C4 (S4 evidence for pool/commit sizing).
**Feeds:** C8 (vault applies parsed notes), C14 (api re-exports Query), PLAN-B
B5 (graph feed), PLAN-D D2 (search panel).
**Vertical thread:** a fixture vault → parsed → indexed → queried (tag / prop /
text / links-to / unresolved) with correct results; `[[link]]` resolution
including attachments; a search issued immediately after a save ack returns
the saved content (commit-before-search, AC-15).
**Coverage:** flavite-index Tier 1 — ≥ 90 % line (§10.5).

**Goal.** The index: SQLite (WAL) as the structural record with the §4.3 schema
(incl. the `attachments` table), the D8 resolver, the typed `Query` AST as the
only way in, and tantivy as a derived search view with deferred commits.

**Decisions already locked.**
- SQLite (WAL) holds everything structural; tantivy holds only searchable text
  and ranking (notes only; attachment *filenames* searched via SQLite stem
  keys) (D12).
- Schema: exactly §4.3 (`meta`, `notes`, `attachments`, `aliases`, `links`,
  `tags`, `properties`, `ext_facts`) with the stated indexes and CHECKs.
- Resolution (`target_key = NFC(casefold(stem))`): (1) `id:` prefix →
  `stable_id` lookup; (2) path-suffix match if the target contains `/` (notes,
  then attachments); (3) stem match over `notes.stem_key ∪ aliases.alias_key ∪
  attachments.stem_key` — embed prefers attachment, link prefers note;
  (4) multiple candidates → rank same-directory → shallowest → lexicographic,
  mark `ambiguous`, pick deterministically, surface in Diagnostics;
  (5) zero candidates → `unresolved` (D8).
- Store all duplicate raw stable IDs; flag the full group and choose the native
  PathKey winner deterministically with visible ambiguity. Conflicted IDs never
  drive T2 identity/pin transfer; repairs remain explicit (D8/§4.16.2).
- Epoch + generation, bounded invalidation/tombstone log, exact search_docs
  cache, and atomic search-commit checkpoint (§4.14). Commit-before-search
  captures matching structural/search readers at an IndexActor barrier. Missing
  coverage/epoch mismatch resets consumers or rebuilds search; never guess from
  live rows after a deletion.
- Hybrid queries: SQLite narrows to a `note_id` set; ≤ 10k ids → tantivy
  filter; otherwise intersect afterward (D12).
- Internal query API is a typed AST, not SQL strings; plugins MUST NOT receive
  SQL access (D13).
- `SearchBackend` trait is the FTS5 reversal swap point (D12).

**What to implement.**
- `crates/flavite-index/Cargo.toml` — deps: `rusqlite` (bundled), `tantivy`,
  `blake3`, `serde`, `flavite-syntax`. `#![forbid(unsafe_code)]` (rusqlite's
  bundled C is not our unsafe).
- `src/schema.rs` — the §4.3 SQL verbatim + PRAGMAs (`journal_mode=WAL`,
  `foreign_keys=ON`) + `meta` rows (`schema_version`, `parser_version`,
  `vault_uuid`, `epoch`, `generation`, `log_floor`, diagnostic search checkpoint)
  + search_docs/changes tables and native path/lookup columns (§4.14/4.16) + migration logic
  (schema-major in the cache path means migrations are per-major; still keep
  `schema_version` for in-place upgrades).
- `src/pool.rs` — one writer connection + a pool of read-only WAL connections
  (D3); sized from S4-2's numbers.
- `src/resolver.rs` — the D8 5-step resolution over the schema; ambiguity
  ranking + marking; `id_conflict` handling; re-resolution entry point
  (`re_resolve_links(keys)`) used by reconcile when notes are created/renamed.
- `src/query.rs` — `Query` AST (§4.3) + executor: `Tag { hierarchical }`
  (range scan `[ 'a/', 'a0' )`), `Prop`, `LinksTo`/`LinkedFrom`, `Unresolved`,
  `Text` (tantivy), `Attachment`, `And`/`Or`/`Not`; `Page<T> { items,
  epoch, generation, next }`; cursor mismatch returns StaleCursor (§4.14).
- `src/search.rs` — `trait SearchBackend` + tantivy impl: tokenizer pipeline
  (SimpleTokenizer → LowerCaser → diacritic folding + bigram for Han/Kana/
  Hangul runs, per S4-1's verdict), deferred commit (commit-before-search; ≤ 5 s
  while dirty; end of bulk reconcile; clean shutdown), `committed_generation`
  + open-time replay/rebuild, hybrid narrowing (≤ 10k ids), using §4.14's
  durable checkpoint authority and matching reader barrier.
- `src/change.rs` — bounded (64 MiB default) invalidation log, log_floor,
  complete snapshot batches, delete records without cascading FKs, monotonic
  cache IDs, and ResetRequired for epoch/retention mismatch. Re-resolution
  invalidates all affected sources, even previously resolved links.
- `src/search_docs.rs` — cache exact searchable body/hash in each structural
  transaction; replay never indexes newer disk bytes as an older version.
- `src/note.rs` — `NoteRef`, `AttRef`.
- `src/lib.rs` — crate docs; the public read/write API used by vault (apply
  parsed notes, apply batches, touch, re-resolve).

**How to implement.**
1. Schema + migrations; `cargo test -p flavite-index` harness over an in-memory
   or tempdir SQLite.
2. Writer/pool; apply paths (`apply_note`, `apply_batch` — one SQLite txn +
   generation + invalidations + search_docs + resolver updates in one transaction;
   tantivy publication follows separately with its own atomic checkpoint).
3. Resolver; tests for every D8 step + ambiguity + id_conflict.
4. Query executor; tests per query kind.
5. Tantivy backend: tokenizer, deferred commit, generations, replay; tests
   (below).
6. The AC-4 model test (below) — the slice's centerpiece.

**Tests, validation, verification.**
- **AC-4 index-equivalence model test** (proptest): a model oracle
  (`HashMap<path, doc>` + link graph) and the real index both receive random
  sequences of create/edit/rename/delete/external-write; after **every**
  sequence, a logical dump compare asserts incremental == from-scratch rebuild.
- Resolver tests: all 5 steps; embed-vs-link preference; ambiguity ranking
  (same-dir → shallowest → lexicographic) + `ambiguous` marking; `id_conflict`
  (all duplicates retained/flagged, rebuild-stable native-path winner, no T2
  transfer while conflicted, explicit repair); attachment targets.
- Query tests: hierarchical tags (`a/b` prefix), props (all vtypes), hybrid
  (≤ 10k path and > 10k intersect path), And/Or/Not, pagination carries
  `(epoch, generation)` and stale pagination is rejected.
- Tantivy tests: **commit-before-search** (apply uncommitted doc → search
  finds it — AC-15's mechanism); deferred commit (no commit without a query
  within the window; ≤ 5 s timer fires); epoch/checkpoint/log replay and reset
  paths (§4.14); CJK tokenizer unit tests (JA/ZH/KO fixtures).
- AC-33: delete before search commit; crash at checkpoint/reader/mirror steps;
  prune below a graph/search cursor; rebuild cache while consumers are live;
  change resolver candidates/aliases with previously resolved edges. Assert
  tombstones survive row deletion, graph reset clears old IDs, and hybrid
  results use matching versions. AC-4 dumps compare native paths/destinations
  while excluding generated IDs, epochs, generations, and retention logs.
- Attachments: table populated; `ATTACHMENT_HASH_MAX` (64 MB) → listed, not
  hashed; placeholders `hydrated=0`, never hydrated by scans (§4.8).
- Budgets: `MAX_NOTE_BYTES` (32 MB) — listed, not parsed/indexed (§4.10).
- Coverage: `cargo llvm-cov -p flavite-index --lcov` ≥ 90 %.
- Commands: `cargo test -p flavite-index` · `cargo clippy -p flavite-index --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-index/src` — empty.
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` — green; compiler-enforced core `forbid(unsafe_code)`.
  - `grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-index/Cargo.toml` — no matches.
  - `grep -c "SearchBackend" crates/flavite-index/src/search.rs` — trait present (the reversal swap point, D12).

**Exit criteria.**
- [ ] Schema + resolver + Query AST + tantivy backend implemented and tested.
- [ ] AC-4 (index equivalence) green; AC-15 mechanism (commit-before-search)
      green.
- [ ] Coverage ≥ 90 %; `just gate` + `just ci-gate` green.

### Slice C8 — P1 slice 4 of 5: `flavite-vault` — VaultActor, coalescing, reconcile, identity/registry, instance lock/IPC, pulse/notice channels

**Spec basis:** §9 Phase 1, D2 (identity/registry, quoted), D3 (channels,
quoted), D5 (reconcile), §4.4 (`open_vault`, quoted), §4.12 (single instance),
§4.9 (failure behavior), §8 AC-9, AC-17 (channel half), AC-18, AC-21, AC-22.
**Depends on:** C5 (fs), C6 (syntax), C7 (index), C10 (BufferManager/revisions for Session; independent of C8). **Feeds:** C9 (cli), C12
(journal recovery call in open), C13 (vanished/changed sets), C14 (api wraps
vault), PLAN-B/D (pulse consumers).
**Vertical thread:** open a fixture vault → index reconciles (startup scan +
buffered watcher replay) → an external edit appears in the index within
500 ms p99 → a second instance focuses the first and exits → deleting
`.flavite/` and reopening keeps the same vault UUID.
**Coverage:** flavite-vault Tier 1 — ≥ 90 % line (rolling; this slice lands the
actor core and must keep the crate above the floor on the code that exists).

**Goal.** The vault core: the `VaultActor` (watcher, Fs, write tokens, journal
and history handles), a headless Session owning BufferManager and worker handles,
event coalescing with bulk mode, the reconcile algorithm,
vault identity/registry (D2), single-instance lock + focus IPC (§4.12), and
the pulse/notice channels (D3).

**Decisions already locked.**
- D2's `open_identity` (quoted in the spec) — `vault.toml` authoritative when
  present and well-formed; the registry exists solely to reconstruct it;
  re-mint the SAME UUID (R2).
- OS directory layout per §1 (registry record keyed by
  native `root-key`; root guard at `<os-data>/flavite/roots/<root-key>.lock`;
  history/recovery/instance under
  `<os-data>/flavite/vaults/<vault-uuid>/`; cache under
  `<os-cache>/flavite/<vault-uuid>/v<schema-major>/`).
- D3's channel policy (quoted): pulse = bounded 1024, drop-oldest, hints only;
  notice = unbounded but coalesced per (note, kind), acked on display;
  conflict/rename/recovery/action notices MUST NOT be dropped, ever (R5).
- §4.13 startup: root guard → read-only identity selection → UUID lock/binding
  check → identity persistence → watcher/event buffer → journal recovery →
  cache/scan → replay. Both guards remain held for Session lifetime. Losing
  openers perform no recovery or state writes; copied UUIDs require resolution.
- D5/§4.4 reconciliation has FastMetadata and VerifyContent modes. Nominees
  and overflow/bulk changes are force-hashed; startup fast scan keeps warm-open
  targets while a full verification starts in the background. Verification runs
  at least every 10 minutes and overdue focus cannot postpone it. A preserved
  old mtime/size cannot evade the next completed verification (AC-34). Apply
  structure, resolver updates, epoch/generation, and invalidations atomically.
  Placeholder/oversize limits and ignored-directory pruning apply in both modes.

- Event coalescing: keyed by path, 300 ms quiet window (max 2 s); > 500 events
  within 1 s → bulk mode (suspend per-event UI, one full reconcile, progress
  on pulse) (§4.4).
- Single instance: advisory lock at `<os-data>/.../instance.lock`
  (flock(2)/LockFileEx); second instance connects to `instance.ipc`
  (Unix socket / named pipe), sends `Focus`, exits; **no lock artifact is ever
  written inside the vault** (R14).
- Failure behavior per §4.9: SQLite corrupt/locked → quarantine + rebuild (UI
  runs from disk meanwhile); cache deleted while running → detect, rebuild
  online; vault on unmounted drive → read-only "vault unavailable" state.

**What to implement.**
- `crates/flavite-vault/Cargo.toml` — deps: `flavite-fs`, `flavite-syntax`,
  `flavite-index`, `flavite-buffer` (Session owns BufferManager; C10's
  foundation must land before C8 exits), `crossbeam`, `notify`
  (via fs), `directories` (OS dirs), `fs4`/`fd-lock` (advisory lock), `serde`/
  `serde_json`, `tracing`, `blake3`, `uuid` (v7).
- `src/actor.rs` — `VaultActor`: owns watcher, Fs, write tokens, journal +
  history handles; message loop; single-writer discipline; runs reconcile on
  a worker thread; `catch_unwind` around per-file parse work (§4.2).
- `src/session.rs` — headless, single-owner Session: BufferManager, pending
  ledger, typed command execution, revisioned read views, and worker handles.
  GUI drives it on the UI thread; CLI on its driving thread. Local edits
  complete synchronously; worker effects process immutable snapshots. C11/C12
  extend the handler set; no actor or async runtime is added for the Session.
- `src/operation.rs` — typed request/reply/effect envelopes with request id,
  buffer identity/revision, and txn/item correlation where applicable. Bounded
  lossless transport with nonblocking host admission (`Busy` before effects),
  pending ledger, and explicit worker-loss completion (§4.5.4). Separate from
  coalesced notices; workers cannot mutate buffers or hold a callback cycle.
- `src/tokens.rs` — self-write token table (`expect(path, hash, now+5s)`) —
  classification only (D5).
- `src/coalesce.rs` — event keying by path, 300 ms quiet (max 2 s), bulk mode
  at > 500 events/s.
- `src/reconcile.rs` — the §4.4 algorithm (verbatim structure); drift counter
  `reconcile.drift_found_total`; periodic (10 min) + focus (throttled)
  triggers; overflow → full VerifyContent. Record verification completion/lag
  separately from metadata scans; never purge a disconnected root.
- `src/identity.rs` — read-only identity selection followed by guarded publication
  (`<os-data>/flavite/vaults/<root-key>.toml`), native root/UUID bindings,
  interrupted-publication handling, copy/move/reused-path decisions (§4.13).
- `src/instance.rs` — root then UUID guards + root-bound IPC (§4.13); same-root
  aliases contend, different-root copied UUIDs cannot focus/share state.
- `src/open.rs` — §4.4's `open_vault` sequence; calls `journal::recover_all`
  (C8 ships the format + validation/classification; committed work returns
  ActionRequired/read-only until C12 supplies safe mutation/recovery with
  history/checkpoint support. No early slice claims full replay).
- `src/journal/mod.rs` — journal file format (`{txn, state, rename, patches[]}`
  per amended D9/§4.15), atomic manifest versions + parent fsync,
  `.flavite/journal/<txn>.json` and `<txn>.data/`; prepare/commit classification,
  verified payload/evidence validation and divergence classification. C8 tests
  classification of complete fixture manifests; C12 supplies producer/replay.
  Unknown versions/corruption are preserved with ActionRequired.
- `src/history.rs` — `HistoryStore` **trait** + path layout (impl lands in C11).
- `src/pulse.rs`, `src/notice.rs` — D3's `Pulse` and `Notice` enums +
  channels (bounded drop-oldest; unbounded coalesced per key + ack).
- `src/lib.rs` — `Vault`, `open_vault`, Session and engine types/public handles;
  no publicly mutable BufferManager handle for consumers.

**How to implement.**
1. Native root guard/identity selection and UUID guard/binding, then publication
   (AC-9/22, §4.13). Test concurrent first launch, aliases, reused paths, and
   crashes between writes; no state mutation before ownership.
2. Instance lock + IPC; test second-instance focus + kill -9 release.
3. Channels; test drop-oldest + coalescing/ack.
4. Coalescing + reconcile; test idempotence, racy rule, bulk mode, drift
   counter, external-edit latency.
5. open_vault sequence; test the startup race ordering (watcher first, then
   scan, then replay).
6. Journal format/validation and read-only blocking on committed work; C12
   supplies the mutation-capable recovery engine.
7. Integrate C10's BufferManager into Session; test host admission, ordered
   command/external-change/completion processing, and operation pumping with a
   deterministic worker harness. Choose queue/ledger bounds and defend their
   overload behavior; no correctness dependence on presentation acknowledgements.

**Tests, validation, verification.**
- **AC-9 vault identity recovery:** close, delete `<vault>/.flavite/` entirely,
  reopen → same UUID; history/recovery reachable; `doctor` reports no orphaned
  safety state.
- **AC-18 watcher-loss simulation:** drop all events for 60 s → index converges
  after focus/periodic reconcile; drift counter increments.
- **AC-21 cache deletion:** delete the cache dir while closed *and* while open
  → correct rebuilt state, zero user-visible data loss.
- **AC-22 single instance:** second instance focuses the first and exits;
  `kill -9` of the first releases the lock (third launch succeeds); **no lock
  or socket artifact ever appears inside the vault** (grep gate + filesystem
  assertion in the test).
- Reconcile: no-change reconcile is a no-op (generation unchanged); racy-file
  rule; bulk mode (simulated 20k-file rewrite, scaled in tests); single-file
  external change visible ≤ 500 ms p99; attachments scanned; placeholders not
  hydrated; drift counting.
- Coalescing unit tests (synthetic event streams): 300 ms quiet, 2 s max, bulk
  threshold.
- Pulse: flood 2000 events into the bounded channel → consumer sees newest
  (drop-oldest). Notice: coalescing per (note, kind) — at most one pending;
  ack on display.
- Startup/ownership: events during recovery/scan are buffered and replayed;
  two first launches choose one identity; a losing opener cannot mutate a live
  pending journal; same-root aliases contend and UUID copies are diagnosed.
- AC-34: suppress all events while changing bytes without changing size/old
  mtime; VerifyContent detects it. Test repeated focus, overdue verification,
  placeholders/large attachments, unreadable files, and unavailable roots.
- AC-32 foundation: local edits execute without I/O waits; queue saturation
  rejects unadmitted operations without effects and retains admitted results;
  worker loss produces an explicit outcome. A headless driver pumps correlated
  requests/replies while notices remain unpresented; no command/journal state
  may advance merely because a notice was acknowledged.
- Coverage: `cargo llvm-cov -p flavite-vault --lcov` ≥ 90 % on existing code.
- Commands: `cargo test -p flavite-vault` · `cargo clippy -p flavite-vault --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-vault/src` — empty.
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` — green; compiler-enforced core `forbid(unsafe_code)`.
  - `grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-vault/Cargo.toml` — no matches.
  - AC-22 filesystem assertion in test: after running the instance tests, `find <fixture-vault> -name '*.lock' -o -name '*.ipc'` — empty.

**Exit criteria.**
- [ ] VaultActor + reconcile + identity/registry + instance + channels
      implemented and tested; AC-9, AC-18, AC-21, AC-22 green.
- [ ] Journal format/classification in place; a pending committed journal
      blocks writes until the recovery engine exists in C12.
- [ ] Session owns BufferManager; operation transport and AC-32 foundation
      checks are green. C10 integration is complete before this slice exits.
- [ ] Coverage ≥ 90 % on existing code; `just gate` + `just ci-gate` green.

### Slice C9 — P1 slice 5 of 5: `flavite-cli` + `doctor`

**Spec basis:** §9 Phase 1 (cli), §4.11 (observability: `flavite doctor` with
the full check list; metrics), D16 (dogfooding — the CLI becomes an api
consumer in C14), §4.9 (failure behavior surfaced).
**Depends on:** C8 (vault). **Feeds:** C14 (migrates the CLI onto flavite-api —
the dogfooding check).
**Vertical thread:** `flavite open <vault>` reconciles headlessly; `flavite
doctor` reports on a fixture vault (healthy + faulted variants); `flavite query
'tag:project'` and `flavite search 'foo'` return results.
**Coverage:** flavite-cli Tier 3 (measured, reported, not gating — §10.5).

**Goal.** A headless CLI driving the vault's Session directly at this phase (C14
migrates the same command engine onto flavite-api) plus `flavite doctor` with every §4.11 check and
the metrics counters.

**Decisions already locked.**
- `doctor` checks (§4.11): `quick_check` on SQLite; tantivy generation vs
  SQLite; sample-N hash verification against disk; orphan temp files; pending
  journals; duplicate and conflicting IDs; NFC/case collisions; broken and
  ambiguous link counts; attachment orphans and unhydrated placeholders;
  history store size; registry records with no vault; orphan instance
  sockets. `doctor --json` for machines.
- Metrics (local only, §4.11): `index.generation`,
  `tantivy.committed_generation`, `watcher.events_total`,
  `watcher.overflow_total`, `reconcile.drift_found_total`, `save.latency_ms`,
  `history.snapshot_total`, `history.evicted_total`, `buffer.conflicts_total`,
  `editor.frame_ms`, `graph.layout_iter_ms`, `index.rebuild_total{reason}`.
- `tracing` structured spans (`reconcile`, `save`, `index.apply`, `parse`,
  `layout.iter`, `frame`); local rotating log file + in-app ring buffer (§4.11).
- No network calls by default (§4.10, AC-26).

**What to implement.**
- `crates/flavite-cli/src/main.rs` — clap CLI: `open`, `doctor`, `query`,
  `search`, `links`, `backlinks`, `graph` (stats); expose `new` when C11
  supplies its save/create implementation, and `rename` when C12 lands.
  Advertise only available handlers.
- `src/driver.rs` — headless Session host: drive effects/replies until a
  terminal result; print pending, partial, and `ActionRequired` outcomes
  distinctly. Unresolved actions/unsupported GUI effects exit nonzero;
  destructive choices require explicit arguments. Preserve recovery or report
  pending work before exit (§4.5); never wait for a displayed banner. CLI
  commands promising writes await the necessary save result, rather than
  treating a buffer-applied edit as a disk save.
- `src/doctor.rs` — every §4.11 check + `--json`.
- `src/metrics.rs` — the counters; `tracing` spans wired per §4.11.
- `src/logging.rs` — rotating local log file setup.
- Tests: doctor on fixture vaults (healthy; corrupt SQLite → quarantine +
  rebuild path; generation drift → replay/rebuild; pending journal →
  reported; NFC collision → reported; attachment orphans + placeholders →
  reported); CLI command tests; metrics presence.

**How to implement.**
1. CLI driving the headless Session in-process, with the same document command
   semantics used by the GUI. Pump worker traffic until completion.
2. doctor checks one by one, each with a faulted fixture proving it fires.
3. Metrics + tracing spans.
4. Keep the CLI's call surface narrow — C14 will route it through the
   registry; structure command handlers as thin arg-parsing + one dispatch
   call so the migration is mechanical.

**Tests, validation, verification.**
- `cargo test -p flavite-cli` — doctor checks fire on faulted fixtures;
  commands work headless.
- `cargo run -p flavite-cli -- doctor <fixture-vault>` — transcript recorded.
- `cargo run -p flavite-cli -- doctor --json <fixture-vault>` — valid JSON.
- Commands: `cargo clippy -p flavite-cli --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-cli/src` — empty.
  - `grep -c "drift_found_total\|rebuild_total" crates/flavite-cli/src/metrics.rs crates/flavite-vault/src/*.rs` — counters present (§4.11).

**Exit criteria.**
- [ ] CLI works headless over a fixture vault; doctor implements every §4.11
      check and each fires on a faulted fixture.
- [ ] Metrics + tracing spans in place.
- [ ] `just gate` green; flavite-cli coverage measured (Tier 3).

### Slice C10 — P2 slice 1 of 2: `flavite-buffer` — `EditorModel`, undo tree, per-line EOL side table, D7 state machine, `BufferManager`, recovery writer

**Spec basis:** §9 Phase 2, D14 (EditorModel: ropey text, multi-selection, undo
**tree** with time-based coalescing, edit ops as `(range, replacement)`, the D7
state machine; zero UI dependencies, exhaustively testable headless), D7 (one
buffer authority; conflict protocol table), §4.8/R12 (per-line EOL
preservation), D6 (recovery files every 2 s, configurable `recovery_interval`),
§8 AC-3, AC-6 (recovery half), AC-7.
**Depends on:** PLAN-A A1–A4 and C6 (TextSnapshot/coordinate types). **Feeds:** C8 (Session owns this model/manager),
C11 (save path consumes the model's
serialization), C12 (buffer-applied patches), PLAN-A A7 (editor widget),
PLAN-B B7 (card edits), PLAN-D D5.
**Vertical thread:** a scripted keystroke sequence on a mixed-EOL file with BOM
and no trailing newline → serialize → only the edited line's bytes differ
(AC-7); every row of the D7 conflict table has an integration test (AC-3).
**Coverage:** flavite-buffer Tier 1 — ≥ 90 % line (§10.5).

**Goal.** The headless editor model: `EditorModel` (ropey, multi-selection,
undo tree, `EditOp`s), the per-line EOL side table with byte-exact
round-tripping, the D7 state machine, `BufferManager` (one buffer per open
note), and the recovery-file writer.

**Decisions already locked.**
- `EditorModel` has zero UI dependencies and is exhaustively testable headless
  (D14). ropey stays first-class (D14).
- Per-line EOL side table (bitmap, LF/CRLF per line; ~4 KB per 1 MB note);
  newly inserted lines inherit the file's dominant EOL; BOM and
  trailing-newline state are stored flags; **an unedited file MUST round-trip
  byte-exactly; a file with mixed EOLs MUST NOT be normalized** (§4.8/R12).
- D7 state machine: exactly one `Buffer` per open note; all edits are
  `EditOp`s applied through it; `base_hash` tracks what we last loaded/saved;
  conflict banners are notices; autosave MUST be disabled in Conflicted and
  manual save blocked until resolved.
- Dirty buffers are written to a recovery file every 2 s (default;
  **configurable** `recovery_interval` — R16), atomic and revision-tagged. A
  successful save retires only covered recovery revisions; newer edits retain
  recovery coverage (§4.5.3). After a crash, restore is offered via a Channel-B notice (D6).

**What to implement.**
- `crates/flavite-buffer/Cargo.toml` — deps: `flavite-syntax` (pure snapshot/
  coordinate types), `ropey`, `blake3`, `serde`,
  `zstd` (recovery files are compressed? — decide: recovery files are raw
  bytes + a small header; keep zstd out unless profiling says otherwise —
  cargo-shear gate), `thiserror`. Zero UI deps.
- `src/model.rs` — `EditorModel`: ropey `Rope`, `Vec<Selection>`
  (multi-selection), `apply(EditOp { range, replacement })`, cursor/scroll
  mapping through diffs (for external reloads, D7), `MAX_NOTE_BYTES` guard.
- `src/revision.rs` — buffer instance identity + monotonic revision, revisioned
  read views, and typed TextByte edit preconditions (§4.5.2/4.16.1). Identity survives path re-key,
  changes on reopen; edits/undo/redo/reload/patch advance revision. Validate a
  multi-selection edit as one atomic command against one revision. Disk
  `base_hash` and buffer revision have separate meanings.
- `src/undo.rs` — undo **tree** (nodes with ops + inverses; branches on
  undo-after-edit), time-based coalescing (default window 500 ms —
  below-spec constant, reversal: tune from AC-14 measurements), deterministic
  grouping.
- `src/eol.rs` — the per-line EOL side table: bitmap (LF/CRLF per line),
  dominant-EOL inheritance for new lines, BOM flag, trailing-newline flag,
  byte-exact `serialize()` + SourceByte/TextByte mapping bound to the snapshot
  (re-emits each line with its original EOL; the
  serialization pass runs off the UI thread — D6/R12).
- `src/state.rs` — D7 state machine: Clean/Dirty/Conflicted/Orphaned +
  `base_hash`; transitions per the D7 table; conflict entry points (reload /
  keep-mine / save-as-conflict-copy / merge — merge lands in C11).
- `src/manager.rs` — `BufferManager`: one `Buffer` per open note; open/close;
  snapshot capture, guarded mutation, and `BufferStateChanged` outcomes.
  Session in flavite-vault owns the manager. Consumer API exposes revisioned
  read views and commands, never unrestricted mutable manager access. This
  crate provides pure model operations; Session supplies command orchestration.
- `src/recovery.rs` — recovery writer: dirty buffers → recovery file every
  `recovery_interval` (default 2 s), atomic write to
  `<os-data>/flavite/vaults/<uuid>/recovery/<note-path-hash>.bin` (path
  supplied by vault using native PathKey), header includes buffer identity,
  revision, checkpoint sequence, and txn/item state/lineage (§4.15). Serialize
  recovery writes/retirement: a save retires only covered revisions and must
  not delete newer recovery state. Corrupt recovery file →
  ignore + log, never block (§4.9).
- `src/lib.rs` — crate docs; public API.

**How to implement.**
1. `EditorModel` core (ropey + selections + EditOps); test multi-cursor ops.
2. Undo tree; test coalescing, branching, determinism.
3. EOL side table + byte-exact serializer; the AC-7 property test drives it.
4. D7 state machine; the AC-3 table tests drive it.
5. `BufferManager` + recovery writer.
6. Wire metrics (`buffer.conflicts_total`).

**Tests, validation, verification.**
- **AC-7 EOL round-trip property** (proptest): mixed-EOL file + BOM + no
  trailing newline; edit one line; serialize; differs from the original
  **only in that line's bytes**; unedited files round-trip byte-exactly;
  mixed EOLs never normalized.
- **AC-3 D7 conflict table** — every row has an integration test: clean +
  changed → silent reload with cursor/scroll mapped through the diff; clean +
  deleted → Orphaned; dirty + changed → conflict banner (Reload snapshot-first
  / Keep mine snapshot-first / Save mine as conflict copy / Merge);
  dirty + changed-to-identical → auto-resolve to Clean; dirty + deleted →
  Recreate / Save as / Discard; renamed (T0–T2) → silent re-key; Conflicted →
  autosave disabled + manual save blocked. Plus the explicit test: **no code
  path overwrites content after a detected base mismatch without explicit
  resolution.** Post-read unobserved writes retain the accepted D6 race.
- Undo tree: coalescing boundaries, branch-on-edit-after-undo, determinism
  (property test: same op sequence → same tree shape).
- Recovery: interval writes (fake clock), covered revision retired on save,
  newer recovery preserved, corrupt file ignored + logged.
- AC-32 model checks: stale buffer identity/revision rejects without mutation;
  multi-selection edits are atomic; revisions advance across undo/redo/reload;
  re-key preserves identity and reopen invalidates old commands. These tests
  have no C8 dependency; C8 supplies orchestration tests over the model.
- Coverage: `cargo llvm-cov -p flavite-buffer --lcov` ≥ 90 %.
- Commands: `cargo test -p flavite-buffer` · `cargo clippy -p flavite-buffer --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-buffer/src` — empty.
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` — green; compiler-enforced core `forbid(unsafe_code)`.
  - `grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-buffer/Cargo.toml` — no matches (D14: zero UI deps).

**Exit criteria.**
- [ ] EditorModel + undo tree + EOL table + D7 state machine + BufferManager +
      revisioned views/recovery writer implemented; AC-3, AC-7, and AC-32 model
      checks green. Session ownership integrates in C8.
- [ ] Coverage ≥ 90 %; `just gate` + `just ci-gate` green.

### Slice C11 — P2 slice 2 of 2: history store + the save path + three-way merge + crash recovery

**Spec basis:** §9 Phase 2, §4.5.2–4.5.3 (revisioned saves/results), D6 (atomic save — the §4.4 `save` function is
quoted in the spec; unconditional history snapshot R1; tiered retention;
recovery), D4 (read-your-writes), D7 (merge: three-way via `diffy`/`similar`;
never write `<<<<<<<` unless the user chooses), §8 AC-1, AC-6, AC-8.
**Depends on:** C8 (vault actor + HistoryStore trait + paths), C10 (EditorModel
serialization, recovery files). **Feeds:** C12 (journal's destructive
operations snapshot first), PLAN-D D3 (recovery offer UI).
**Vertical thread:** ≥ 10,000 randomized saves in each declared crash model
produce old/new installed bytes without concurrent writers (AC-1/36). External
bytes observed by the final read enter durable history (AC-8); post-read writes
retain D6's accepted race. Measure crash recovery against the healthy reference
workload and report delayed/failed checkpoint writes separately (AC-6).
**Coverage:** flavite-vault Tier 1 — ≥ 90 % (rolling).

**Goal.** Durability: the content-addressed history store with tiered
retention, the atomic save path with the unconditional snapshot, read-your-writes
index application, three-way merge, and crash recovery offers.

**Decisions already locked.**
- §4.4's `save` (quoted in the spec): resolve symlink → temp sibling → write +
  copy_mode + fsync → **read prior → durable unconditional `history.put`**
  (R1) → base-hash compare → `Err(Conflict)` → token expect →
  `rename_replace` → `fsync_dir` → `index.apply_inline` (read-your-writes).
  The outline's errors are stage-aware per §4.5.3; ordinary success follows
  the index apply, while post-replacement failure reports actual disk outcome.
- Retention (D6): keep every version 5 min → 1/min for 1 h → 1/h for 30 days;
  floor ≥ 50 distinct versions per note within the window; hard cap 1 GB per
  vault, LRU eviction by snapshot time; content addressing dedups identical
  bytes.
- History protects observed bytes only. The post-read overwrite interval
  includes history persistence; later external bytes can be absent from history.
  A detected mismatch blocks save, with the residual race explicitly separate.
- Merge: three-way (base from the history store, mine, theirs) via `diffy` or
  `similar`; unresolved hunks open a resolution UI; MUST NOT write `<<<<<<<`
  markers unless the user explicitly chooses (D7).
- Recovery: after a crash, restore is offered via a Channel-B notice (D6).

**What to implement.**
- `crates/flavite-vault/src/history.rs` — the `HistoryStore` impl:
  content-addressed zstd blobs keyed by blake3 hex under
  `<os-data>/flavite/vaults/<uuid>/history/`; `put(bytes) -> Hash` (dedup),
  `get(hash)` plus per-note/version/reason metadata and pinned merge/transaction
  references (§4.15). Retention is fake-clock-testable; 1 GiB includes pinned
  blobs. Reject an overwrite before mutation if unpinned eviction cannot make
  room; never evict a live merge base or pending transaction preimage. Metrics
  include pinned bytes, snapshot_total, and evicted_total.
- `crates/flavite-vault/src/save.rs` — the §4.4 `save` function, verbatim
  structure, on the `Fs` trait; `SaveRequest` captures immutable text/EOL,
  buffer identity/revision, request id, and disk precondition. `SaveReceipt`
  returns request id, buffer identity, saved revision/hash, epoch, and generation.
  `SaveFailure` distinguishes NotReplaced / ReplacedDurabilityUncertain /
  ReplacedIndexPending / OutcomeUnknown, with stage and retry rules (§4.5.3).
  The `Conflict` error
  carries the current hash; the conflict branch's other-writer bytes are in
  history (asserted by AC-8's test).
- `crates/flavite-vault/src/merge.rs` — three-way merge (base from history,
  mine, theirs) via `diffy`; unresolved hunks → resolution data for the UI;
  never auto-resolve non-trivial merges; snapshot-first guarantee.
- On load/first dirty transition retain base bytes; persist and pin the merge
  base before the first durable recovery checkpoint/save (§4.15). Test an
  external edit before any prior save and retention pressure against that base.
- Recovery offer: `Notice::RecoveryOffer { note }` on open when a recovery
  file exists (D6); the app restores via the buffer (C10's reader).
- Deps: `zstd`, `diffy` (or `similar` — pick one, record the choice), `blake3`.

**How to implement.**
1. History store: put/get/dedup; retention tiers with an injectable clock;
   cap + LRU; tests with a fake clock.
2. `save` on the actor (off the UI thread, D6); wire `index.apply_inline`
   (read-your-writes).
   Session admits at most one save per buffer, coalesces later requests to
   the newest requested revision with explicit Superseded results for replaced
   requests, and defers conflicting disk mutations.
   Handle completions on the Session thread; newer edits remain Dirty and
   retain recovery. A closed/reopened buffer rejects the old identity's reply.
   Repair an index failure after replacement without repeating the overwrite.
3. Merge; tests.
4. The three property/integration tests below — the slice's centerpiece.

**Tests, validation, verification.**
- **AC-1 fault-injected saves** (proptest over FaultFs): crash at every
  modeled write/sync/replace boundary in both ProcessCrash and PowerLoss,
  ≥ 10,000 randomized cases each (§10.6.1). Assert old/new installed bytes,
  acknowledged durability, and detected omitted-sync/short-write defects; list
  orphan temps while retaining live journal references. Label native kill tests
  separately from simulated machine power loss.
- **AC-8 TOCTOU backstop** (proptest over interleavings): FaultFs interleaves
  an external write at every point in `save`; in every interleaving where the
  external write lands before our read, the external bytes are present in the
  history store.
- **AC-6 crash recovery** (integration): `kill -9` during typing (child
  process harness) measures the ≤ 2 s healthy-reference target and offers restore.
  Log checkpoint age/latency; simulate delays/disk-full and report missed
  checkpoints honestly instead of asserting a universal timer-derived bound.
- Retention tiers: time-tiered eviction + floor (≥ 50/note) + 1 GB cap + LRU
  (fake clock).
- Save: read-your-writes (index reflects an acked save immediately);
  conflict path preserves the other writer's bytes in history; symlink
  target written, link not clobbered; permissions copied; BOM/EOL/trailing
  newline preserved (with C10's serializer).
- AC-32 save interleavings: hold revision N's save while typing N+1; complete N
  and assert disk/index reflect N while buffer stays Dirty at N+1 with recovery
  coverage. Race recovery write against retirement; delay replies past reopen;
  queue another save, rename, or external change. Inject failure after replace,
  at directory fsync, and during index apply; assert explicit outcome, no false
  successful ack, and no blind duplicate overwrite. Test at the Session level
  and through the facade when C14 is available; add registry entries in this
  slice if the facade already landed.
- Merge: three-way correctness; unresolved hunks surfaced; no
  auto-resolution; no `<<<<<<<` unless chosen.
- Coverage: `cargo llvm-cov -p flavite-vault --lcov` ≥ 90 % (rolling).
- Commands: `cargo test -p flavite-vault` · `cargo clippy -p flavite-vault --all-targets -- -D warnings` · `just gate` · `just ci-gate` (FaultFs suite in G1).
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-vault/src` — empty.
  - `grep -c "history.put" crates/flavite-vault/src/save.rs` — unconditional snapshot present (R1).

**Exit criteria.**
- [ ] History store + save path + merge + recovery offer implemented.
- [ ] AC-1, AC-6, AC-8 green (the durability proofs).
- [ ] AC-32 save ordering/partial-failure tests green; facade handler and
      result mapping present when C14 has landed.
- [ ] Coverage ≥ 90 % (rolling); `just gate` + `just ci-gate` green.

### Slice C12 — P3 slice 1 of 2: prepared/committed journal, checkpoints, durable undo + rename fuzz target

**Spec basis:** amended D9/§4.15, §4.5 operation replies, §4.16 coordinates/native
paths, §10.6.1 persistence models; AC-2, AC-10, AC-11, AC-32, AC-36.
**Depends on:** C7 (hashed source spans/index), C8 (ownership, Session/transport,
manifest reader), C10 (revisioned buffers/checkpoints), C11 (save/history pins).
**Feeds:** C13, B8, D3/D5; register handlers through C14 when its facade exists.
**Coverage:** flavite-vault Tier 1 ≥ 90 %.
**Vertical thread:** rename with closed and dirty open sources; fault every
prepare/stage/commit/apply/checkpoint/undo/close transition. Recover uncontested
items without overwriting divergence or dropping acknowledged buffer/undo state.

**Goal.** A transaction has recoverable evidence before commitment, explicit
per-item completion, durable operation markers, and bounded payload/history
retention. Spec §4.15 is the state-machine authority; neither a notice ack nor
text-hash equality establishes application/cancellation.

**Decisions already locked.**
- PLAN/VERIFY build nonoverlapping SourceByte patches against exact disk hashes.
  Dirty sources are reparsed as their current guarded TextSnapshot; never apply
  stale index offsets to a normalized/edited buffer.
- PREPARE publishes a versioned manifest; STAGE stores full replacement payloads
  in `<txn>.data/` plus sibling temps. COMMIT is published only after all required
  file/directory syncs. Before COMMIT no truth or buffer changes have occurred.
- APPLY checks each item's base immediately before mutation, snapshots observed
  bytes, and honors D6's residual race. Recreate a missing temp from verified
  durable payload. Unknown/corrupt evidence or external divergence blocks the
  item and remains reported/preserved; it never becomes falsely completed.
- In-app move supports a single note/attachment on the same filesystem. Reject
  occupied destinations, directory moves, and cross-filesystem moves before
  commitment. Case-only moves use a unique journaled intermediate. Unhashed
  large attachments report their weaker native-identity/metadata validation.
- Open patches use correlated txn/item requests and revision guards. Persist
  bytes + operation markers before acknowledging buffer-applied. Later edits
  retain those markers; duplicate requests apply once.
- Undo returns Pending after the in-memory inverse. Persist post-undo snapshot,
  then durable cancellation, then success. Recovery uses checkpoint lineage;
  cancelled markers override stale applied snapshots. Unacknowledged undo can
  recover the preceding durable state; redo creates a new operation identity.
- WAIT_BUFFERS retains live manifests/payloads. CLOSE requires saved/cancelled
  durable handoff or explicit skip for every item. AppliedPendingSave/Partial
  results name outstanding items. Cleanup/GC never removes live evidence or
  pinned merge bases; budget failure before mutation refuses the operation.
- Strategy rewrite/alias/none and attachment embed rewriting remain D8/D11.
  Payload budget is 256 MiB by default with explicit measured adjustment;
  stream payloads and preflight free space.

**What to implement.**
- `rename/mod.rs` / `planner.rs`: planning, validation, payload accounting,
  prepare/stage/commit/apply/wait/close engine and result inventory.
- `journal/manifest.rs`: version/seq/txn/item state, native endpoints, hashes,
  payload/checkpoint refs; atomic manifest updates + parent sync. The C8 reader
  and producer share one format; unknown version/corruption stays intact.
- `rename/buffer_patch.rs`: guarded Session application, snapshot/marker
  persistence, correlated application replies, idempotent retry ledger.
- `rename/undo.rs`: post-undo snapshot/cancellation/result ordering, subsequent
  edit handling, successor handoff, redo identity, and retained undo references.
- `journal/recover.rs`: prepare abort, committed restaging, external-divergence
  handling, checkpoint/cancellation lineage, and terminal-only cleanup.
- `rename/cmd.rs`: user commands with explicit partial/pending-save results;
  facade registration/tests if C14 exists, including headless CLI execution.
- `fuzz/fuzz_targets/rename_planner.rs` + curated seeds: complete engine over
  both FaultFs models, including short writes/sync failure (§10.6.1).

**Implementation and verification.**
1. Write transition/invariant tests alongside manifest/payload implementation;
   every durable transition and every installed rename is a fault point.
2. AC-2: crash before/after commitment, remove stage/payload, corrupt/introduce
   unknown manifests, occupy destination, change a source during apply. Preparing
   leaves truth unchanged; uncontested committed items converge; blocked items
   remain explicit. No global atomic multi-file assertion is substituted.
3. AC-10: crash before/after post-undo snapshot, cancellation persistence, reply,
   successor recovery/save, and evidence cleanup. Seed a stale pre-undo recovery
   file, edit after undo, and redo. Acknowledged undo never resurrects its patch;
   pending undo has only last-durable-state guarantees.
4. AC-32: stale/duplicate/mismatched request/reply; unpresented notices; save of
   later text retaining operation markers; queued save/rename. Compare GUI and
   CLI core outcomes including AppliedPendingSave and Partial.
5. AC-11/35: attachment rename, existing destination refusal, case-only move,
   mixed-EOL dirty-buffer rewrite, duplicate stable IDs, native path collisions.
6. AC-36: run ProcessCrash and PowerLoss schedules with seeded omitted-sync and
   short-write defects. Assert live history/payload references survive cleanup;
   budget rejection cannot start an overwrite.

**Commands:** `cargo test -p flavite-vault` ·
`cargo fuzz run rename_planner -- -max_total_time=30` · `just fuzz-smoke` ·
`cargo xtask arch check` · `just gate` · `just ci-gate`.

**Exit criteria.**
- [ ] Producer and C8 recovery reader share §4.15 format/state semantics;
      prepare/commit/apply/wait/close, durable buffer patch/undo, and cleanup work.
- [ ] AC-2/10/11 and journal-side AC-32/35/36 pass; both crash models labelled.
- [ ] Fuzz target/seeds included in smoke; Tier-1 coverage and G0/G1 green.

### Slice C13 — P3 slice 2 of 2: tiered rename inference (T0–T4) + rename notices + attachment renames + duplicate-ID handling

**Spec basis:** §9 Phase 3, D8 (tier table, quoted), R6 (T3/T4 never carry
identity state silently), §5 red team (copy-note regression), §8 AC-11, AC-17
(notice-loss tie-in).
**Depends on:** C8 (reconcile's vanished/changed sets + watcher event pairs),
C12 (T0 in-app renames), C7 (stable_id + content hashes). **Feeds:** PLAN-B B8
(pin carry-over tiers), PLAN-D D3 (rename notices UI).
**Vertical thread:** copy a note (same content, new name) → pins do NOT move,
buffers do NOT re-key, a `RenameSuggested` notice is emitted; an OS-level rename
(T1) → silent re-key + pin carry + notice, never an auto-rewrite; an in-app
rename (T0) → journaled rewrite per the setting.
**Coverage:** flavite-vault Tier 1 — ≥ 90 % (rolling).

**Goal.** Rename inference that never silently moves identity state without
authoritative evidence, with lossless notices for everything external.

**Decisions already locked.**
- D8's tier table (quoted): **T0** in-app rename command → silent re-key +
  silent pin carry + journaled rewrite per setting; **T1** OS rename event pair
  (inotify cookie / FSEvents flag / `RENAMED_OLD`+`NEW`) → silent re-key +
  silent pin carry + notice, never auto-rewrite; **T2** same `stable_id` on a
  vanished+created pair → same as T1; **T3** same content hash, size ≥ 64 B,
  exactly one candidate each side, prefers same-directory → **no re-key, no
  pin carry**, notice only, user confirms; **T4** fuzzy similarity ≥ 0.9,
  exactly one candidate → same as T3. Otherwise delete+create: links go
  unresolved and are reported, never guessed (D8).
- External rename → **never auto-rewrite other files**; emit
  `Notice::RenameSuggested { from, to, tier, inbound }` — "Ideas → Concepts
  detected. 14 links affected." [Update links] [Add alias] [Ignore] (D8).
- Duplicate stable IDs remain represented and fully flagged; deterministic
  native-PathKey lookup plus ambiguity diagnostics is rebuild-stable. No silent
  T2 re-key/pin transfer while conflicted; repair requires explicit mint-new-ID
  action (D8/§4.16.2).
- Attachment renames: in-app → embeds rewritten through the D9 journal;
  external (T1/T3) → notice, never a silent rewrite (AC-11).

**What to implement.**
- `crates/flavite-vault/src/infer.rs` — the tier engine over reconcile's
  vanished/changed sets + watcher rename pairs (S3-1's per-OS pairing) +
  `stable_id` pairs + content hashes (size ≥ 64 B, unique candidates,
  same-directory preference) + fuzzy similarity (≥ 0.9, unique candidate;
  pick the similarity crate — `strsim` — record the choice).
- `crates/flavite-vault/src/notice.rs` — `Notice::RenameSuggested` +
  `LinksBroken` with the UI actions wired to commands (Update links → D9
  journal; Add alias → frontmatter patch via C6; Ignore).
- `crates/flavite-vault/src/attachments.rs` — attachment rename handling
  (in-app → journal; external → notice).
- `crates/flavite-vault/src/id_conflict.rs` — duplicate `stable_id` detection
  + the "mint new ID" action (never auto-edit).
- Pin carry-over plumbing to PLAN-B B8's pins (T0–T2 only).

**How to implement.**
1. Tier engine: implement each tier's evidence collection + decision; test
   the matrix (incl. negatives: two candidates → no inference; size < 64 B →
   no T3; fuzzy < 0.9 → no T4).
2. Wire T0 (from C12's rename command), T1 (watcher pairs), T2 (stable_id),
   T3 (content hash), T4 (fuzzy).
3. Notices + actions; id_conflict; attachment renames.
4. The R6 regression test: copy-note (T3) does NOT move pins or re-key
   buffers.

**Tests, validation, verification.**
- Tier matrix tests (every tier + negatives above).
- **R6 regression:** copy a note → pins unmoved, buffers not re-keyed,
  notice emitted.
- **AC-11:** in-app attachment rename rewrites embeds via the journal;
  external attachment rename (T1/T3) → notice, never a silent rewrite.
- `id_conflict`: rebuild-stable path winner; no T2 carry-over on duplicated IDs;
  explicit mint-new-ID works; never auto-edit.
- Notice losslessness under bulk load: during C8's bulk-mode reconcile,
  rename/conflict notices are all delivered (AC-17 tie-in — the notice
  channel's coalescing test plus an integration test with a bulk reconcile
  that includes renames).
- Coverage: `cargo llvm-cov -p flavite-vault --lcov` ≥ 90 % (rolling).
- Commands: `cargo test -p flavite-vault` · `cargo clippy -p flavite-vault --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-vault/src` — empty.
  - `grep -c "T0\|T1\|T2\|T3\|T4" crates/flavite-vault/src/infer.rs` — all five tiers present (D8).

**Exit criteria.**
- [ ] Tiered inference complete; AC-11 green; R6 regression locked.
- [ ] Rename notices lossless under bulk load; id_conflict handled.
- [ ] Coverage ≥ 90 % (rolling); `just gate` + `just ci-gate` green.

### Slice C14 — P4: `flavite-api` — Session facade, command execution contract, versioned events + CLI migration

**Spec basis:** §9 Phase 4, §4.5.1–4.5.5 (ownership, ordering, results, operation transport), D16
(dogfooding rule; capabilities declared now), D3 (channel policy), §4.3
(Query AST re-export), §10.5 (flavite-api Tier 2 ≥ 80 %).
**Depends on:** C7 (Query AST), C8 (Session, worker transport, initial handlers),
C10 (revisioned model), C9 (the CLI to migrate). C11–C13 extend the handler
set in either merge order, as specified in §2's staging rule. **Feeds:** PLAN-A A7 (editor
commands), PLAN-B B6 (graph commands), PLAN-D D1–D4 (the shell), C9 (migration
target).
**Vertical thread:** `flavite query 'tag:project'` dispatches through the
registry; a plugin-shaped consumer (test double) receives versioned events;
every CLI command goes through the registry (dogfooding check).
**Coverage:** flavite-api Tier 2 — ≥ 80 % line (§10.5).

**Goal.** The unstable API surface (D16): Lattice owns the headless Session and
registry; typed commands/results and revisioned read views expose the existing
engine to GUI/CLI equally. Include versioned events, operation transport
adapters, pulse/notice types, Query re-export, capability declarations, and CLI
migration. Semantic authority is spec §4.5; registration alone is insufficient.

**Decisions already locked.**
- §4.5: `CommandSpec { id, title, args, when, keys }`,
  `Invocation { request_id, id, args, origin, preconditions }`; registry validates
  JSON → typed Session command. Built-in typed dispatch shares the same
  handlers/validation and avoids JSON on the typing path. GUI maps headless
  effects/results to Tasks/Messages; api contains no iced types.
- Local commands complete synchronously with revision/result; I/O commands
  return Pending then a correlated terminal result. Save success requires
  disk durability steps + structural index apply; partial failure reports the
  actual stage/disk outcome. Events/notices never serve as completion replies.
- `Event` is the **wire form** — versioned (`API_VERSION` in the handshake),
  serde `tag="type"` — the union a future plugin host subscribes to; internally,
  events split across pulse/notice per D3 (§4.5).
- Capabilities (`ReadNotes`, `WriteNotes{scope}`, `Ui::Panel`,
  `Net{allowlist}`) are **declared now even though nothing enforces them
  yet**; plugin writes MUST go through `BufferManager`, never straight to disk
  (D16).
- The API is marked `unstable` until a host exists; bump the major freely
  while unstable (D16 reversal).
- Phase 4 exit: **the CLI itself is a consumer of the API (dogfooding check).**

**What to implement.**
- `crates/flavite-api/Cargo.toml` — deps: `flavite-vault`, `flavite-index`,
  `flavite-buffer`, `serde`/`serde_json`. `#![forbid(unsafe_code)]`.
- `src/command.rs` — `CommandSpec`, request ids, `Invocation`, typed dispatch,
  guards, and registry (validate/dispatch → result or pending effects);
  keybinding map and palette data. Inventory with argument/result schema,
  preconditions, completion milestone, and retry behavior per handler.
- `src/result.rs` — local/pending/rejected/terminal results, revisioned save
  receipts and stage-aware failure mapping; Busy, Unavailable, StaleRevision,
  Superseded, Unsupported, and ActionRequired outcomes (§4.5). Re-export engine
  types where possible, preserving one definition of core semantics.
- `src/event.rs` — the versioned `Event` enum (§4.5) + `API_VERSION` handshake
  + the mapping from vault's Pulse/Notice.
- `src/channel.rs` — the Pulse/Notice channel types + coalescing + ack
  (refined from C8; vault emits, api re-exports and bridges to iced
  `Subscription`s in PLAN-D), plus separate operation request/reply adapters.
  Presentation acks cannot complete commands, apply patches, or advance journals.
- `src/query.rs` — re-export the `Query` AST from flavite-index.
- `src/capability.rs` — `ReadNotes`, `WriteNotes { scope }`, `Ui::Panel`,
  `Net { allowlist }` — declared, not enforced.
- `src/lib.rs` — `Lattice` owns Session + registry (open vault, typed/JSON
  dispatch, revisioned buffer views, operation pump, queries, subscriptions);
  expose vault-backed semantic parse/read helpers for read mode without mutable
  buffer access or a new api → syntax edge. All public types are headless;
  mark the surface `unstable`.
- **CLI migration (C9's content):** every CLI command dispatches through the
  registry; the CLI keeps zero direct vault/index calls in command handlers.
- `devdocs/architecture/crates.toml` — add `cli → api` (replacing `cli → vault`
  for command paths), `api → {vault, index, buffer}`; `arch check` green.

**How to implement.**
1. Types first (command/event/channel/capability); tests.
2. Registry dispatch into vault's command handlers.
   Preserve Session ownership and ordered execution. Register only handlers
   already implemented; future slices extend inventory/command tests. GUI-only
   commands yield typed host effects, with Unsupported behavior in the CLI.
3. Event mapping + version handshake; serde round-trip tests.
4. Migrate the CLI; prove the dogfood (below).
5. Arch edges; `arch check`.

**Tests, validation, verification.**
- Registry dispatch + keybinding resolution + palette data tests.
- AC-32 facade conformance: JSON and typed entries reach the same validated
  handlers; compare GUI/CLI host drivers for equivalent document commands.
  Assert revision guards, immediate local completion, Pending vs terminal
  results, ActionRequired/Unsupported, bounded overload, and notice-ack
  independence. Use deterministic worker transcripts for early C14; cumulative
  real save/journal interleavings are required as C11–C13 land.
- Pulse drop-oldest; notice coalescing per key + ack on display (channel
  tests).
- Event serde round-trip + version handshake mismatch → clear error.
- **Dogfood test:** a counting registry wrapper asserts every CLI command
  dispatches through the registry (Phase 4 exit); drive an actual pending
  operation to its terminal result, including noninteractive failure reporting.
- Capability declarations compile + are queryable.
- Coverage: `cargo llvm-cov -p flavite-api --lcov` ≥ 80 %.
- Commands: `cargo test -p flavite-api -p flavite-cli` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-api/src crates/flavite-cli/src` — empty.
  - `grep -rn "flavite_vault\|flavite_index" crates/flavite-cli/src` — no
    matches after the migration (the CLI talks to the core only through
    flavite-api — D16; grep for the crate names in `use` statements).
  - `grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-api/Cargo.toml` — no matches.

**Exit criteria.**
- [ ] **Phase 4 exit:** Lattice owns Session + registry; typed commands/results,
      revisioned views, operation transport, events, queries, capabilities, and
      CLI migration work for implemented handlers. AC-32 facade checks green;
      C11–C13 register remaining handlers and production integration tests.
- [ ] Coverage ≥ 80 % (flavite-api); `just gate` + `just ci-gate` green.

## 6. Verification command index

| Command | Gate | Purpose |
|---|---|---|
| `just gate` | G0 (every slice) | fmt + clippy + test + `arch check`, ≤ 5 min, ends with `just gc` |
| `just ci-gate` | G1 (CI, 3 OSes) | G0 + coverage floors + FaultFs/integration suites + doc + deny/audit/shear + fuzz smoke 60 s + network-denied + MSRV |
| `cargo xtask arch check` | G0 | drift + edges + layering |
| `cargo test -p flavite-fs --all-features` | C5+ | fs + FaultFs suites |
| `cargo test -p flavite-syntax` | C6+ | scanner/patcher/glue suites |
| `cargo test -p flavite-index` | C7+ | schema/resolver/query/tantivy + AC-4 model test |
| `cargo test -p flavite-vault` | C8+ | actor/reconcile/identity/instance/channels/save/journal/inference |
| `cargo test -p flavite-buffer` | C10+ | EditorModel/undo/EOL/D7/recovery |
| `cargo test -p flavite-api -p flavite-cli` | C14 | registry/events/dogfood/doctor |
| `cargo llvm-cov -p <crate> --lcov` | per slice | Tier-1 (≥ 90 %) / Tier-2 (≥ 80 %) floors |
| `cargo +nightly fuzz run <target>` | dev loop | local campaigns (7 targets by C12) |
| `just fuzz-smoke` | G0 hook + G1 (60 s) | every target over seeds + generated input |
| `cargo test --workspace` | G0 | full suite |
| `cargo clippy --workspace --all-targets -- -D warnings` | G0 | lint |
| `cargo fmt --all --check` | G0 | format |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | G1 | docs |
| `cargo deny check` · `cargo audit` · `cargo shear` | G1 | supply chain |
| `just network-denied` (unshare -n test run) | G1 Linux | AC-26 no-network posture |
| `cargo run -p flavite-cli -- doctor <vault>` | C9+ | doctor transcript |
| `cargo run -p xtask --bin s3_watcher_probe` / `s3_fs_probe` / `s4_search_probe` / `s4_db_probe` | C1–C4 | spike evidence |
| `just clean --dry-run` · `just clean` · `just gc` | per slice | disk budgets (§10.10) |

## 7. Source-review and gate index

Text searches aid review; they cannot establish snapshot, rendering, or
transaction correctness. Compiler/architecture checks and behavioral tests
provide the acceptance evidence (§10.3).

```bash
# Layering: no iced-family deps in core-tier crates (enforced by arch check too)
grep -rn -E '^\s*(iced|iced_|winit|wgpu|cosmic-text)' crates/flavite-{fs,syntax,index,buffer,vault,api,graph,cli}/Cargo.toml   # expect: no matches
# flavite-syntax is pure and IO-free (D10)
grep -rn "std::fs\|std::io" crates/flavite-syntax/src/scanner.rs crates/flavite-syntax/src/lib.rs   # expect: no matches
# flavite-buffer has zero UI deps (D14)
grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-buffer/Cargo.toml   # expect: no matches
# No deferred work anywhere
grep -rn "TODO\|FIXME\|XXX" crates/*/src xtask/src fuzz/fuzz_targets   # expect: no matches
# No unsafe in Tier-1/2 crates
cargo clippy --workspace --all-targets --all-features -- -D warnings   # core crates forbid unsafe code
# Unconditional history snapshot on the save path (R1)
grep -c "history.put" crates/flavite-vault/src/save.rs   # expect: >= 1
# Cancel-on-undo in the journal (R7)
grep -c "cancelled" crates/flavite-vault/src/rename/undo.rs   # expect: >= 1
# All five rename tiers present (D8)
grep -c "T0\|T1\|T2\|T3\|T4" crates/flavite-vault/src/infer.rs   # expect: all five
# CLI talks to the core only through flavite-api (D16, after C14)
grep -rn "flavite_vault\|flavite_index" crates/flavite-cli/src   # expect: no matches
# No lock artifact inside the vault (R14, AC-22)
find <fixture-vault> \( -name '*.lock' -o -name '*.ipc' \)   # expect: no matches
# Arch green
cargo xtask arch check
```

## 8. Plan exit criteria

- [ ] **S3 + S4 closed:** `devdocs/spikes/S3-watcher.md`, `S3-fs.md`,
      `S4-search.md`, `S4-db.md` recorded; parameters confirmed or adjusted
      with documented exceptions.
- [ ] **Phase 1 exit (§9):** acceptance 1 (with C11), 4, 5 (parser side), 7
      (with C10), 8 (with C11), 9, 11 (with C13), 18–21 green **headlessly**.
- [ ] **Phase 2 exit (§9):** acceptance 3, 6, 7 green.
- [ ] **Phase 3 exit (§9):** acceptance 2, 10, 11 green.
- [ ] **Phase 4 exit (§9):** the CLI is a consumer of flavite-api (dogfooding
      check green); the API surface is complete and marked `unstable`.
- [ ] AC-33/34/35/36 complete: epoch/log/search recovery, forced verification,
      text/native identity, and both fault models with native conformance.
- [ ] AC-32 complete for all PLAN-C handlers: revision/precondition, operation
      ordering/transport, save partial failure, patch correlation, and headless
      host behavior tested through the facade. No placeholder handlers remain.
- [ ] AC-19 (path safety) green on all three OSes; AC-22 (single instance)
      green; AC-26 posture: the core makes no network calls (network-denied
      run green).
- [ ] All seven §10.6 fuzz targets exist and run clean in `just fuzz-smoke`:
      parser + image_limits (PLAN-A A3), path_safety (C5), scanner +
      frontmatter_patcher + parity (C6), rename_planner (C12).
- [ ] Coverage: Tier-1 crates (fs, syntax, buffer, vault, index) ≥ 90 %;
      Tier-2 (api) ≥ 80 %; flavite-cli measured (Tier 3). G0/G1 green on
      Linux, macOS, Windows.
- [ ] Nothing half-wired: every crate compiles, lints, tests, and the CLI can
      open, reconcile, query, and doctor a real vault end-to-end.



