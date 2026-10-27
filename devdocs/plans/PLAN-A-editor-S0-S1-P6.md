# PLAN A — Editor workstream: repository bootstrap (S0) + editor-viability spike (S1) + editor widget (P6)

**Specification:** RFC-0001 v2.1 with command and durability/identity amendments (§4.5/4.13–4.16/10.6.1) — canonical copy `./SPEC-draft-v2.md`; synchronized mirror `./RFC-0001-v2.1-spec.md`.
**Spec phases owned:** §9 Phase 0.0 (all of it — called "S0" in the workstream split), Phase 0 spike S1, Phase 6 (P6).
**Acceptance criteria owned:** AC-27, AC-28, AC-29, AC-30, AC-31 (infrastructure); AC-14, AC-24, AC-25 (editor); AC-5 parity target (created with C6, exercised from A8 on); AC-32 (editor command adapter, shared with PLAN-C/D), AC-35 (coordinates), AC-36 (fault-model foundation).
**Depends on:** nothing — this plan is first. PLAN-B, PLAN-C, and PLAN-D assume slices A1–A4 have landed.
**Feeds:** PLAN-C (gates, FaultFs, fuzz workspace), PLAN-B (gates, `flavite-ui` hosting), PLAN-D (D5 consumes A11's editor widget).

## 0. How to read this plan

Every slice is a **vertical cut**: a fine-grained path through the specification
that is implemented, runs, rigorously proven, and honestly validated. Each slice
names the behavior that works when the slice is done, the tests that prove it,
and the exact commands that demonstrate it. The "fully and honestly" bar
(working end-to-end behavior; verification built during the slice; debt paid in
touched code; rigor in choices, edge cases, and error paths; coherent shippable
state; no false completion claims) applies to every slice — see `README.md` in
this directory for the full statement. Locked spec decisions are cited by number
(D1–D18, §-sections, AC-numbers, R1–R18, ADR-0001/0002); an agent executing a
slice implements them, it does not re-design them. Below-spec choices a slice
must make are stated and defended inside the slice.

## 1. Workstream context

§9 is explicit: Phase 0.0 stands up the infrastructure **before any product
code**, because "gates adopted after the code exists are gates that get bypassed."
S1 is the highest-risk open unknown (ADR-0001's escalation path: upstream patches
→ fork iced → renegotiate the pure-Rust constraint, last resort only), and P6 is
the project's largest single cost (D14). This workstream therefore retires the two
biggest non-core risks and delivers the editor widget, while PLAN-C builds the
headless core and PLAN-B the graph in parallel.

**Phase 0.0 ownership.** A1–A4 land the entire §10 infrastructure: workspace
skeleton, fault-injection foundation, fuzz/property infrastructure, and the
project-RAG + gates + CI. PLAN-B, PLAN-C, and PLAN-D slices assume `just gate`,
`just setup`, `cargo xtask arch check`, `just fuzz-smoke`, and `just gc` /
`just clean` exist and are green.

## 2. Slice dependency graph

```
A1 (workspace skeleton + toolchain + lints + bootstrap gate)
 └─► A2 (Fs/MemFs + declared FaultFs crash models)
      └─► A3 (fuzz + setup + hooks + gc/clean)
           └─► A4 (arch RAG + coverage floors + G0/G1 recipes + CI + negative tests)   [Phase 0.0 exit]

A5 (S1-1: IME event-delivery probe, per OS)
 └─► A6 (S1-2: primitives audit + consolidated S1 verdict) ──► [verdict: proceed]
      └─► A7 (P6-1: flavite-editor crate + EditorSurface + minimal working editor)
           └─► A8 (P6-2: link-aware editing: decorations, ctrl/cmd-click, autocomplete)
                └─► A9 (P6-3: find-in-note, conflict/diagnostic decorations, tree-sitter injection)
                     └─► A10 (P6-4: IME production path + accessibility)
                          └─► A11 (P6-5: performance hardening + AC-14 harness)
```

Cross-plan edges: A7–A11 depend on PLAN-C's C6 (scanner spans), C10
(`EditorModel`/`BufferManager`), C11 (save handler), and C14 (`flavite-api`). If PLAN-A executes
before those land, A7–A11 wait; A1–A6 have no cross-plan dependencies.

## 3. Locked decisions this workstream implements (do not re-open)

| Spec ref | Decision | Where it lands here |
|---|---|---|
| §10.1–10.10 | Repository layout; project-RAG (generated `crate-graph.json` + `DEPENDENCIES.md`, hand-maintained `crates.toml`, `arch check` with drift/edges/layering checks, query CLI); gates G0/G1/G2; lint stack; coverage floors + ratchet; fuzz/property infrastructure; CI topology; onboarding; post-gate cleanup | A1–A4 |
| AC-27–31 | Infrastructure acceptance criteria (gate budget, arch negative tests, coverage negative test, fuzz smoke, post-gate cleanup) | A4 (AC-27/28/29), A3 (AC-30/31) |
| D14, ADR-0001 | Custom iced editor widget over headless `EditorModel`; `EditorSurface` trait retained for testability/substitution; stock `text_editor` is **not** a v1 fallback | A7–A11 |
| D10 | One link grammar, two parsers; `flavite-syntax`'s scanner is the single source of link semantics; tree-sitter never decides what a link is | A8–A9 consume C6's scanner |
| D18 | IME correctness is a MUST (CJK users); screen-reader support a SHOULD gated on S1; Linux X11+Wayland, macOS, Windows day one | A5, A6, A10 |
| §4.10 | No raw HTML ever rendered/executed; URL scheme allowlist (`http`, `https`, `mailto`) | A6 (paste-from-HTML path), A8 (click routing) |
| §4.1/§10.2 | iced/winit/wgpu/cosmic-text permitted only in `flavite-editor` and `flavite-ui`; enforced transitively by `arch check` | A7 crate placement |
| D3 | pulse (lossy, bounded 1024, drop-oldest) vs. notice (lossless, coalesced per key, ack on display) channels | consumed via `flavite-api` from A7 on; defined in PLAN-C C8/C14 |
| D6/§4.8 | BOM, per-line EOL, trailing-newline preservation; byte-exact round-trip of unedited files | A7/A11 consume C10's EOL side table |

## 4. Crate dependency contract (encoded in `devdocs/architecture/crates.toml` by A4)

| Edge | Justification |
|---|---|
| flavite-fs → — | base layer (§4.1) |
| flavite-syntax → — | pure, IO-free (§4.1) |
| flavite-buffer → flavite-syntax | headless model consumes pure TextSnapshot/coordinate types (§4.16.1); zero UI deps |
| flavite-index → flavite-syntax | parses notes for indexing (D10/D12) |
| flavite-vault → flavite-fs, flavite-syntax, flavite-index, flavite-buffer | headless Session owns BufferManager and coordinates actors, history, recovery, and journal patches (§4.5) |
| flavite-graph → flavite-index | epoch/generation invalidation feed with deletion and retention resets (§4.14); CPU only |
| flavite-api → flavite-vault, flavite-index, flavite-buffer | Lattice facade owns Session + registry; typed commands/results, revisioned views, events, queries, caps (D16/§4.5) |
| flavite-editor → flavite-api, flavite-buffer, flavite-syntax | iced widget over `EditorModel` (D14); same scanner (D10); commands via api (D16) |
| flavite-ui → flavite-api, flavite-editor, flavite-graph, flavite-buffer | app shell hosts Lattice; buffer dependency provides read-only view/EditOp types; all document mutations go through api (D16/§4.5) |
| flavite-cli → flavite-api | headless CLI; dogfood (D16, Phase 4 exit) |
| xtask → any (tooling) | dev tooling; never depended on by product crates |
| fuzz targets → any | not layering-gated (§10.6) |

**Layering (enforced transitively by `cargo xtask arch check`, §10.2):** for every
core-tier crate (fs, syntax, index, buffer, vault, api, graph, cli) the transitive
closure of normal + build dependencies MUST NOT contain `iced`, `iced_*`,
`winit`, `wgpu`, or `cosmic-text`. `flavite-editor` and `flavite-ui` are the only
crates that MAY. Dev-dependencies are exempt but listed in the artifact.

**Locked interpretation notes** (recorded here because §4.1's ASCII diagram is
ambiguous; `crates.toml` encodes the edge list above, and any §4.1 diagram
delta is updated in the same PR per §10.2 rule 2 — an architecture change is a
spec change, recorded not silent):
1. `flavite-api` is a **facade** (api → vault/index/buffer), built in Phase 4 after
   Phase 1 exists — matching §9's phase order. `flavite-vault` defines
   `Pulse`/`Notice` internally; api re-exports them wrapped in the versioned
   `Event` union (§4.5: "the `Event` enum is the wire form").
2. `flavite-graph → flavite-index` and `flavite-ui → flavite-graph` edges are
   required by D15's data flow (IndexActor → GraphModel → PositionBuffer →
   widget). The graph widget reads per-frame positions from the CPU model; all
   vault/index/fs *mutations* still go through api (D16).
3. `flavite-vault::Session` owns `BufferManager` and pending operations;
   `flavite-api::Lattice` owns Session + registry (§4.5). The GUI hosts Lattice
   on the UI thread and the CLI on its driving thread. Local edits execute
   synchronously through the same typed command handlers. Widgets consume
   revisioned read views and emit commands; they cannot mutate BufferManager.
   VaultActor patch requests and application replies use the correlated,
   lossless operation transport. Notice presentation acks never acknowledge
   buffer edits or journal progress. No extra session actor/runtime is added.

## 5. Slices

The §4 table describes the final planned graph. A4's exact edge check compares
only active dependencies in that checkout; edge-adding slices activate/update
crates.toml and regenerate artifacts together. Empty crates report coverage N/A
until executable implementation activates their floor. A1–A3 use the labelled
bootstrap gate; only A4 onward claims full G0/G1 (§10.2–10.5).

### Slice A1 — Phase 0.0 / 1 of 4: workspace skeleton, pinned toolchain, lint configs, crate skeletons

**Spec basis:** §10.1 (repository layout), §10.4 (lint stack), §10.8 (onboarding).
**Depends on:** nothing. **Feeds:** A2, A3, A4, and every other plan.
**Vertical thread:** fresh clone → `cargo check --workspace`, `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings` all green on the ten §10.1 crates + xtask, all empty-but-documented.

**Goal.** The repository exists exactly as §10.1 lays it out: workspace root
manifest with the ten crates and xtask, pinned stable toolchain, rustfmt /
clippy / cargo-deny configs, a justfile whose recipes all work, and a
`.gitignore` for build output. No product code yet — the slice proves the
skeleton compiles, lints, and formats.

**Decisions already locked.**
- Workspace: edition 2024, resolver "3"; `[workspace.lints]` carries the clippy
  set; G0 escalates with `-D warnings` (§10.3, §10.4).
- `rust-toolchain.toml` pins an exact stable release; MSRV is `rust-version` in the workspace,
  enforced by the G1 MSRV job, not by the pinned toolchain (§10.8).
- The justfile is the single source of truth for gates; CI is a thin caller (§10.3).
- Crate set and responsibilities are exactly §4.1 / §10.1 (table in §4 above).

**What to implement.**
- `Cargo.toml` (root): `members = ["crates/*", "xtask"]`; `[workspace.package]`
  (edition 2024, `rust-version` = pinned MSRV, license, repository);
  `[workspace.lints.clippy]` (all + pedantic as warn); `[workspace.dependencies]`
  pinning shared external versions (iced, ropey, rusqlite, tantivy,
  pulldown-cmark, notify, tree-sitter, blake3, zstd, nucleo, image, tracing,
  crossbeam, serde).
- `rust-toolchain.toml` (stable + components rustfmt, clippy, llvm-tools-preview),
  `rustfmt.toml` (edition 2024), `clippy.toml` (`msrv` = workspace rust-version),
  `deny.toml` (license allowlist: MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause,
  ISC, Unicode-DFS-2016, Zlib, MPL-2.0; copyleft default-reject — §10.4).
- Ten crate skeletons `crates/flavite-{fs,syntax,index,buffer,vault,api,graph,cli,editor,ui}/`:
  `Cargo.toml` (no deps yet) + `src/lib.rs` (`cli`: `src/main.rs`) with a
  crate-level doc comment stating tier + responsibility (from §4.1) and
  `#![forbid(unsafe_code)]` in Tier-1/2 crates.
- `xtask/`: `Cargo.toml` + `src/main.rs` that prints usage text. Subcommands
  (`arch gen|check|deps|dependents|path|why|violations`, `cov-check`) are
  implemented in A4 — A1 ships a usage-printing binary, not stubs.
- `justfile`: recipes that work now — `fmt`, `clippy`, `test`, `check` (thin
  wrappers), plus a labelled bootstrap `gate` running fmt/clippy/tests. A3
  adds cleanup; A4 promotes gate to full G0 with arch check and establishes
  G1. `setup`, `fuzz-smoke`, `gc`, `clean` land in A3. No recipe calls a
  future/nonexistent command or claims full-G0 acceptance prematurely.
- `.gitignore`: `/target`, `/fuzz/target`, `*.profraw`, `*.profdata`, editor
  noise. `Cargo.lock` is committed (application workspace).

**How to implement.**
1. Write the root manifest; `cargo check --workspace` must pass with empty crates.
2. Add toolchain + lint configs; `cargo fmt --all --check` and
   `cargo clippy --workspace --all-targets -- -D warnings` must pass.
3. Scaffold the ten crates + xtask with doc comments; re-run the three commands.
4. Write the justfile with working recipes; `just --list` shows them.
5. Initialize git on main if absent and commit the reviewed scaffold plus
   Cargo.lock (excluding generated/build output). This precedes hook/branch
   workflows; the plan is an instruction, not a claim git already exists.

**Tests, validation, verification.**
- `cargo check --workspace` — green.
- `cargo fmt --all --check` — green.
- `cargo clippy --workspace --all-targets -- -D warnings` — green.
- `just --list` — lists the recipes; each recipe runs.
- Grep gates:
  - `grep -rn "TODO\|FIXME\|XXX" crates/*/src xtask/src` — empty.
  - `grep -l '^iced' crates/*/Cargo.toml` — empty at this slice (no crate may
    depend on iced yet; editor/ui get it in A7/B2).
  - `test -f rust-toolchain.toml && cargo --version` — pinned channel.
- Error-handling convention decided here and used everywhere: `thiserror` for
  library error types, `anyhow` for binaries (xtask, cli, examples).

**Exit criteria.**
- [ ] The three cargo commands above are green on a fresh clone.
- [ ] `just --list` works; every recipe in the justfile runs.
- [ ] §10.1's directory layout exists exactly (crates/, xtask/, justfile, configs).
- [ ] `git status` shows only the new files; nothing else in the repo changed.

### Slice A2 — Phase 0.0 / 2 of 4: `flavite-fs` foundation — `Fs` trait, `MemFs`, `FaultFs`

**Spec basis:** §4.1 (`flavite-fs`: trait Fs, atomic_write, watcher wrapper, path safety, FaultFs), §10.6 (FaultFs is a library behind feature `fault`, not `#[cfg(test)]`, because fuzz targets and integration tests both link it), §8 AC-1/AC-8 (the properties FaultFs exists to prove).
**Depends on:** A1. **Feeds:** C5 (OsFs, path safety, atomic write, watcher), C11 (fault-injected save property), fuzz targets (`path_safety`, `rename_planner`).
**Vertical thread:** exercise ProcessCrash and PowerLoss over separate volatile/
durable file and directory state (§10.6.1). Negative cases with missing fsync or
short writes must fail; trace model boundaries without claiming they cover every
native syscall or prove arbitrary storage hardware (AC-36).
**Coverage:** flavite-fs is Tier 1 (≥ 90 % line, §10.5). The code that exists after this slice (trait + MemFs + FaultFs) must already meet the floor; it ratchets up as C5 lands.

**Goal.** The `Fs` trait exists with its full method surface (finalized against
spike S3's evidence in C2 — this slice may adjust signatures, with tests), an
in-memory implementation (`MemFs`) for tests, and `FaultFs` — a scriptable
fault-injection wrapper behind the `fault` cargo feature with interleaving
hooks for external writers (AC-8's requirement).

**Decisions already locked.**
- FaultFs lives in `flavite-fs` behind feature `fault` — a library, not
  `#[cfg(test)]` (§10.6).
- The trait is the seam the rest of the system codes against; `OsFs` (C5) and
  `MemFs` (here) are interchangeable behind `&dyn Fs`.
- FaultFs faults are **scriptable at modeled operations/sub-operations** and support
  **interleaving hooks** (an external writer can be injected at any point) —
  both are required by AC-1 and AC-8 respectively.

**What to implement.**
- `crates/flavite-fs/Cargo.toml`: deps = `blake3` (hash helper), `thiserror`;
  feature `fault = []` (no extra deps — FaultFs is pure Rust over the trait).
- `src/fs.rs` — `pub trait Fs` (object-safe, `&dyn Fs` everywhere):
  `canonicalize(&self, path) -> Result<PathBuf>`;
  `resolve_symlink(&self, path) -> Result<PathBuf>`;
  `read_if_exists(&self, path) -> Result<Option<Vec<u8>>>`;
  `read_capped(&self, path, max: u64) -> Result<Vec<u8>>` (errors past `max`);
  `write_new(&self, path, bytes) -> Result<()>` (create-or-fail, no truncate of existing);
  `copy_mode(&self, from, to) -> Result<()>`;
  `fsync_file(&self, path) -> Result<()>`;
  `fsync_dir(&self, path) -> Result<()>`;
  `rename_replace(&self, from, to) -> Result<()>`;
  `remove(&self, path) -> Result<()>`;
  `metadata(&self, path) -> Result<Metadata>` (size, mtime, native file identity);
  native PathKey encode/decode, root/parent capability and opened-target
  validation, directory enumeration, and no-clobber rename seams (C2 finalizes
  safe platform contracts);
  `is_placeholder(&self, path) -> Result<bool>` (cloud placeholder detection;
  `OsFs` implements per S3-2 evidence, `MemFs` returns false).
  Every method returns `Result<_, FsError>`; no panics on IO paths.
- `src/mem.rs` — `MemFs`: in-memory tree (`BTreeMap<PathBuf, MemNode>`), full
  trait impl, symlink nodes, native identities, mode bits, injectable clock,
  and separate volatile/durable bytes plus directory entries (§10.6.1).
- `src/fault.rs` (feature `fault`) — `FaultFs<F: Fs>`: wraps any `Fs`; a
  `FaultScript` (ordered list of `FaultPoint` × `FaultKind`); `FaultPoint` =
  method/sub-operation + occurrence + before/after/progress boundary. Faults
  include error, partial write then error, short write, unsupported/failed sync,
  process crash, power loss, and external interleaving. ProcessCrash retains
  OS-visible filesystem state; PowerLoss selects legal durable-state outcomes.
  Both stop callers until a fresh process/Session is created; trace persistence
  and ownership release separately. Model limits are reported explicitly.
- `src/lib.rs` — crate docs, re-exports, `#![forbid(unsafe_code)]`.

**How to implement.**
1. Define the trait from §4.4's `save`/`reconcile` call sites (the methods above
   are exactly what those algorithms call); keep it object-safe.
2. Implement `MemFs` with an injectable clock and symlink support; it is the
   oracle for every later fs test.
3. Implement the §10.6.1 fault models with explicit crash mode and deterministic
   scripts. Inject within chunked writes, not only before/after write_new.
   Model file vs directory fsync separately; the property harness explores
   unsynced legal outcomes and records replayable seeds/traces.
4. Unit tests: MemFs behavior per method (incl. symlink resolution, capped
   reads, mode copy); FaultFs fault matrix (every FaultKind at every method
   class), crash latch + revive, interleaving hook ordering, trace recording.

**Tests, validation, verification.**
- `cargo test -p flavite-fs --all-features` — green (unit tests above).
- Property test (proptest, in-crate): random op sequences over `MemFs` vs. a
  naive `HashMap<PathBuf, Vec<u8>>` oracle — reads agree, existence agrees,
  symlink resolution agrees.
- Property test: `FaultFs<MemFs>` with random fault scripts — invariants:
  every returned error is typed; dead processes cannot continue; interleavings
  follow the trace. AC-36 proves that process crash retains unsynced OS-visible
  state while power loss may discard it; file sync without directory sync is
  distinguishable. Seed omitted-sync/unchecked-short-write defects and require
  failing invariants. Native OsFs conformance lands in C2/C5 separately.
- Coverage: `cargo llvm-cov -p flavite-fs --all-features --lcov` — ≥ 90 % line
  on the code that exists (run `cargo xtask cov-check` once A4 lands; until
  then, measure with llvm-cov directly and record the number in the slice's PR).
- Grep gates:
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` — green; compiler-enforced core `forbid(unsafe_code)`.
  - `grep -rn "TODO\|FIXME" crates/flavite-fs/src` — empty.
  - `grep -n 'cfg(test)' crates/flavite-fs/src/fault.rs` — empty (FaultFs is a
    library, not test-only — §10.6).
- Lint: `cargo clippy -p flavite-fs --all-targets --all-features -- -D warnings`.

**Exit criteria.**
- [ ] `MemFs` passes the oracle property test.
- [ ] FaultFs passes both declared crash models and negative persistence cases
      (AC-36 foundation), with deterministic interleaving/trace checks.
- [ ] `cargo test -p flavite-fs --all-features` green; clippy `-D warnings` green.
- [ ] Coverage of the existing flavite-fs code ≥ 90 % (measured, recorded).
- [ ] `just gate` green (fmt/clippy/test still pass workspace-wide).

### Slice A3 — Phase 0.0 / 3 of 4: fuzz workspace, `fuzz-smoke`, `just setup`, hooks, `gc`/`clean`

**Spec basis:** §10.6 (fuzz and property-testing infrastructure), §10.8 (onboarding), §10.10 (post-gate cleanup and disk budgets), §10.7 (CI topology — workflow files land in A4), AC-30, AC-31.
**Depends on:** A1, A2. **Feeds:** A4 (CI calls `fuzz-smoke`; `gate` ends with `gc`), every PLAN-C slice (fuzz targets land with their crates; `fuzz-smoke` discovers targets by directory listing).
**Vertical thread:** `just setup` on a fresh clone installs the toolchain; `just fuzz-smoke` runs every fuzz target that exists (two at this slice) over checked-in seeds plus generated input; `just clean --dry-run` reports sizes and removes nothing; `just gc` is a no-op under budget.

**Goal.** The development-time fuzz infrastructure exists and works: an isolated
cargo-fuzz workspace with the two targets that only need external crates
(`parser`, `image_limits`), curated checked-in seed corpora, a smoke recipe that
scales to the seven §10.6 targets as PLAN-C adds them, the onboarding command,
the pre-push hook, and the git-aware, size-budgeted cleanup commands.

**Decisions already locked.**
- `fuzz/` is a cargo-fuzz workspace, excluded from the root workspace's
  `default-members` (it is not a member at all — it has its own `[workspace]`),
  so application builds never compile it (§10.6).
- Every fuzz target MUST enforce the input budgets (§4.10: `MAX_NOTE_BYTES`,
  `ATTACHMENT_HASH_MAX`, image limits, per-input parse time); exceeding a budget
  is a finding, not a crash to ignore.
- Every bug found by fuzzing lands its reproducer in the corpus in the same PR
  that fixes it; corpora are curated, minimized (nightly `cmin`), checked in.
- `just gc` (post-gate) is threshold-based: ≤ 10 GB soft → no-op; over → remove
  `target/*/incremental`, `fuzz/target/`, profiling intermediates; still over
  20 GB hard → escalate to `just clean` with a warning. `just clean` removes
  `target/`, `fuzz/target/`, `fuzz/artifacts/`, `*.profraw`, `*.profdata`,
  `target/llvm-cov/`; `--dry-run` reports without deleting. Never: corpus seeds,
  `~/.cargo`, anything outside the workspace root (verify root; never follow
  symlinks out) (§10.10).
- The pre-push hook runs G0 + fuzz smoke and is bypassable via `--no-verify`;
  G1 is the real wall (§10.3).

**What to implement.**
- `fuzz/Cargo.toml` — own workspace (`[workspace]` empty table to detach),
  `[package]` for the fuzz crate, deps: `libfuzzer-sys`, `pulldown-cmark`,
  `image`, plus `flavite-syntax`/`flavite-fs` **path deps added by PLAN-C slices
  as those targets appear** (at this slice: none). `fuzz/.gitignore`:
  `/target`, `/artifacts`.
- `fuzz/fuzz_targets/parser.rs` — fuzzes the pulldown-cmark event stream over
  arbitrary bytes: reject inputs > `MAX_NOTE_BYTES` (32 MB) as non-findings
  (budget enforcement per §10.6), run `Parser::new` with all extensions,
  drain the event stream; invariants: no panic, no hang (libfuzzer timeout),
  event stream completes. This target works at Phase 0.0 because it depends
  only on an external crate.
- `fuzz/fuzz_targets/image_limits.rs` — hostile images through `image::Limits`
  (≤ 64 MP, ≤ 64 MB decoded): decode with limits on a worker-thread budget;
  invariants: no OOM, no panic (decompression-bomb regression, §4.10).
- `fuzz/corpus/parser/`, `fuzz/corpus/image_limits/` — curated seed corpora,
  checked in (parser: empty, plain markdown, pathological nesting, code fences,
  math, unicode, entity soup; image_limits: tiny PNG, truncated PNG, small
  bomb-like PNG).
- `justfile` additions (all working when this slice ends):
  - `just fuzz-smoke` — discovers `fuzz/fuzz_targets/*.rs`, runs each 30 s
    locally (`cargo +nightly fuzz run <target> -- -max_total_time=30
    -rss_limit_mb=4096`) over seeds plus generated input. Scales automatically
    as targets are added; G1 overrides to 60 s.
  - `just setup` — installs rustfmt, clippy, llvm-tools-preview, cargo-deny, cargo-audit,
    cargo-shear, cargo-llvm-cov, cargo-fuzz, and the nightly toolchain; installs
    the pre-push hook; idempotent.
  - `just clean` / `just clean --dry-run` / `just gc` — per §10.10 above;
    implemented as a thin just recipe delegating to `cargo xtask clean` /
    `cargo xtask gc`? **No** — decide here: implement `gc`/`clean` logic in
    `xtask` (subcommands `xtask gc`, `xtask clean [--dry-run]`) so it is unit-
    testable; justfile recipes stay thin wrappers (§10.9: "justfile accumulates
    logic → a second build system; recipes stay thin wrappers; logic lives in
    xtask/cargo").
- `xtask/src/clean.rs` — the `gc`/`clean` implementation: measure `target/` +
  `fuzz/target/` (`du`-equivalent via `walkdir`, OS-cached, sub-second warm),
  apply soft/hard budgets, remove only the §10.10 table's paths, verify
  workspace root (canonicalize and compare; refuse to follow symlinks out),
  report freed bytes, `--dry-run` prints per-path sizes and removes nothing.
- `.git/hooks/pre-push` (installed by `just setup`): runs `just gate` then
  `just fuzz-smoke`; exits non-zero on failure (bypassable via `git push
  --no-verify`, documented in the hook output).

**How to implement.**
1. Create the fuzz workspace; `cargo +nightly fuzz run parser -- -max_total_time=5`
   must work over the seed corpus.
2. Add `image_limits` the same way; verify both targets run under
   `just fuzz-smoke`.
3. Implement `xtask clean`/`xtask gc` with unit tests over a synthetic directory
   tree (tempdir): budget thresholds, dry-run removes nothing, corpus seeds and
   `~/.cargo` never touched, symlink-out-of-root refused.
4. Wire the justfile recipes; run `just setup` end-to-end in a scratch clone.

**Tests, validation, verification.**
- `just fuzz-smoke` — green over both targets and their seeds.
- AC-30 demonstration (seeded crash caught within the smoke window): in a
  scratch copy, add `fuzz/fuzz_targets/_probe.rs` that panics on a magic input,
  seed the magic input into its corpus, run `just fuzz-smoke`, assert non-zero
  exit, remove. Record the transcript in the slice's PR. (A4 wires this as a CI
  job `fuzz-smoke-negative`.)
- AC-31 demonstration: `just clean --dry-run` prints per-path sizes, removes
  nothing, `git status --porcelain` unchanged; `just gc` under budget is a no-op
  and prints the measured size; `just clean` frees and reports bytes.
- `just setup` — idempotent second run is a no-op success.
- Grep gates:
  - `grep -rn "TODO\|FIXME" justfile xtask/src` — empty.
  - `test -z "$(git status --porcelain)"` after `just clean --dry-run` — clean
    tree preserved (no tracked file is ever removed).
  - `find fuzz/corpus -type f | wc -l` — seeds are tracked files (never removed
    by gc/clean).
- `cargo test -p xtask` — clean/gc unit tests green; `just gate` green.

**Exit criteria.**
- [ ] `just fuzz-smoke` green; both targets run over seeds.
- [ ] Seeded-crash demonstration recorded (AC-30 mechanism proven).
- [ ] `just clean --dry-run` / `just clean` / `just gc` behave per §10.10
      (AC-31 mechanism proven; `git status` clean afterwards).
- [ ] `just setup` works on a fresh clone and installs the hook.
- [ ] `just gate` green.

### Slice A4 — Phase 0.0 / 4 of 4: project-RAG (`arch gen`/`check`/query CLI), coverage floors, G0 + G1 recipes, CI, negative tests

**Spec basis:** §10.2 (project-RAG: generation, hand-maintained `crates.toml`, three verification checks, query CLI), §10.3 (gates), §10.4 (lint/static checks), §10.5 (coverage floors + ratchet), §10.7 (CI topology), §10.8 (onboarding verification), AC-27, AC-28, AC-29.
**Depends on:** A1, A2, A3. **Feeds:** every later slice in every plan (the gate is the contract).
**Vertical thread:** `just gate` (G0) runs fmt + clippy + test + `arch check` in ≤ 5 min and ends with `just gc`; the gate demonstrably **fails** on seeded violations (undocumented internal dependency, iced in a core crate, coverage below floor).
**Coverage config:** floors per §10.5 — Tier 1 (fs, syntax, buffer, vault, index) ≥ 90 % line; Tier 2 (api, graph) ≥ 80 %; Tier 3 (cli, editor, ui) reported only. Ratchet: a PR MUST NOT decrease a gating crate's coverage vs `main`.

**Goal.** The project-RAG exists (generated + checked in), `arch check` enforces
drift/edges/layering, the coverage gate enforces floors, G0 and G1 are wired as
justfile recipes, CI runs the matrix, and the infrastructure's own acceptance
criteria (AC-27/28/29) are demonstrated with seeded violations.

**Decisions already locked.**
- `crate-graph.json` and `DEPENDENCIES.md` are GENERATED and checked in; they
  MUST NOT be hand-edited (§10.2). `crates.toml` is hand-maintained: tiers,
  responsibilities, expected internal edges, layering rules.
- `arch check` performs three checks: (1) **drift** — regenerate and diff;
  (2) **edges** — actual internal edges == expected edges, exactly, both
    directions; (3) **layering** — transitive closure of normal+build deps of
    every core-tier crate MUST NOT contain iced, iced_*, winit, wgpu,
    cosmic-text. Dev-deps exempt but listed.
- A new internal dependency requires updating `crates.toml` **and** the §4.1
  diagram in the same PR — an architecture change is a spec change.
- G0 = `fmt --all --check` · `clippy --workspace --all-targets -- -D warnings` ·
  `test --workspace` · `arch check`; budget ≤ 5 min; ends with `just gc`.
- G1 = G0 + coverage floors · FaultFs + integration suites · doc build
  (`RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps`) · cargo-deny +
  cargo audit · cargo-shear · fuzz smoke 60 s/target · network-denied test run ·
  MSRV build; on ubuntu-latest, macOS-latest, windows-latest; coverage + fuzz
  smoke + network-denied on Linux only. Workflow files are thin callers of
  justfile recipes only (§10.7).
- Branch protection: `main` requires G1, one approving review, resolved threads.

**What to implement.**
- `xtask/src/arch.rs`:
  - `arch gen` — reads `cargo metadata --format-version 1`, writes
    `devdocs/architecture/crate-graph.json` (nodes: workspace crates +
    first-party external deps — iced, winit, wgpu, cosmic-text, ropey, rusqlite,
    tantivy, pulldown-cmark, notify, tree-sitter, blake3, zstd, nucleo, image,
    tracing; edges: direct deps split normal/build/dev) and renders
    `devdocs/architecture/DEPENDENCIES.md` (same data, human-readable, with a
    GENERATED header). Both checked in.
  - `arch check` — the three checks above; non-zero exit with a precise
    diff/report on failure.
  - `arch deps <crate>` · `arch dependents <crate>` · `arch path <from> <to>` ·
    `arch why <crate>` (tier + responsibility) · `arch violations` — JSON on
    stdout for agent consumption.
- `devdocs/architecture/crates.toml` — hand-maintained: per-crate tier +
  responsibility (§4.1), active internal edges for this checkout separately
  from §4's planned graph, and layering rules. Empty skeletons do not activate
  future edges. Generation retains cfg/kind/all-feature target information and
  excludes host-dependent paths; check byte-stability across OSes. Include the three interpretation notes
  from §4 of this plan (api as facade; graph→index and ui→graph edges;
  BufferManager ownership) so the RAG records them.
- `xtask/src/cov.rs` — `cov-check`: reads an `lcov.info` + the floors config,
  fails if a gating crate is below its floor; supports `--ratchet <main-lcov>`
  to fail on any decrease vs main's artifact.
- Floors config: `devdocs/architecture/coverage.toml` (tier → crates → floor/active),
  zero executable lines reports N/A; activating implementation requires its
  floor in that slice. Missing data for an active crate fails. Config is
  consumed by `cov-check` and rendered into `DEPENDENCIES.md`.
- `justfile`: `gate` (G0, ends with `just gc`), `ci-gate` (G1), `doc`,
  `msrv`, `network-denied` (build first, then run runtime tests under unshare
  with INET connect/send attempt monitoring; a seeded attempt fails AC-26),
  `fuzz-smoke-ci` (60 s/target).
- `.github/workflows/ci.yml` — G1 matrix (3 OSes; Linux adds coverage merge
  `cargo llvm-cov --workspace --lcov` over unit + integration + FaultFs
  suites, fuzz smoke, network-denied run; macOS/Windows run suites + report
  coverage, do not gate), thin caller of `just ci-gate`.
- `.github/workflows/nightly.yml` — long fuzz (≥ 1 h/target, AC-20), corpus
  minimization (`cargo fuzz cmin`), coverage + benchmark trends.
- `.github/workflows/weekly.yml` — iced-`master` advisory build (§7 risk
  table) + the §10.8 fresh-clone job (fresh clone → `just setup` → `just gate`)
  on each OS.
- Negative tests (AC-28/AC-29), as `xtask` integration tests over a synthetic
  fixture workspace in `xtask/tests/fixtures/` **plus** CI jobs that seed
  violations into the real workspace and assert failure:
  - `arch-edges-negative`: select an absent internal edge in the current
    fixture, add it, and regenerate the artifact without updating active expected
    edges. Assert check 2 fails specifically, rather than only the drift check.
  - `arch-layering-negative`: add iced to a core crate and regenerate artifacts;
    assert the layering violation specifically, with other checks isolated.
  - `coverage-negative` (AC-29): doctor an lcov with a Tier-1 crate at 50 % →
    `cargo xtask cov-check` fails.
  - `fuzz-smoke-negative` (AC-30, mechanism proven in A3): seeded crash caught.

**How to implement.**
1. Implement `arch gen` first; generate and check in both artifacts.
2. Write active edges in crates.toml from the actual manifests, retaining §4's
   final graph separately; implement `arch check`
   (drift = regen + `git diff --exit-code`-equivalent in-process; edges =
   set equality both directions; layering = transitive closure over
   normal+build deps, dev-deps listed).
3. Implement the query CLI; smoke-test each subcommand's JSON output.
4. Implement `cov-check` + floors config; wire `ci-gate`.
5. Write the three workflows as thin callers; add the negative-test jobs.
6. Measure full G0 on a clean working tree with provisioned dependencies and
   warm build cache; require ≤ 5 min (AC-27). Record cold setup/build and total
   hook time separately; bootstrap timings cannot substitute.

**Tests, validation, verification.**
- `just gate` — green, ≤ 5 min (AC-27), ends with the `gc` report line.
- `cargo xtask arch check` — green; `cargo xtask arch gen && git diff --exit-code devdocs/architecture/` — clean (drift check).
- `cargo xtask arch deps flavite-vault` — JSON listing its deps;
  `arch dependents flavite-syntax`, `arch path flavite-vault flavite-syntax`,
  `arch why flavite-vault`, `arch violations` — all return correct JSON.
- Negative tests: each seeded violation makes the corresponding gate command
  exit non-zero (AC-28, AC-29, AC-30) — run and record exit codes.
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` — green.
- `cargo deny check` · `cargo audit` · `cargo shear` — green on the current dep set.
- MSRV job: `cargo +<msrv> check --workspace` — green.
- Grep gates:
  - `grep -rn "GENERATED" devdocs/architecture/DEPENDENCIES.md | head -1` — header present.
  - `grep -c "hand-edit" devdocs/architecture/crate-graph.json` — 0 (generated
    file carries no hand-edit markers; drift check is the enforcement).
  - `grep -rn "TODO\|FIXME" xtask/src devdocs/architecture/crates.toml` — empty.
- `just gate` timing recorded in the PR (AC-27 evidence).

**Exit criteria.**
- [ ] **Phase 0.0 exit (spec §9):** G0 green on the workspace; the arch check and
      coverage gate demonstrably fail on seeded violations (AC-27–29); fuzz
      smoke works and catches a seeded crash (AC-30, from A3); post-gate
      cleanup works (AC-31, from A3).
- [ ] `crate-graph.json` + `DEPENDENCIES.md` generated and checked in;
      `crates.toml` records tiers, edges, layering, and the interpretation notes.
- [ ] G0 (`just gate`) ≤ 5 min; G1 (`just ci-gate`) defined and green on all
      three OSes in CI; negative-test CI jobs exist and fail-as-expected.
- [ ] Fresh-clone → `just setup` → `just gate` path verified (weekly job defined).

### Slice A5 — S1 slice 1 of 2: IME event-delivery probe (Linux X11 + Wayland, macOS, Windows)

**Spec basis:** §9 Phase 0 spike S1 (rescoped in v2: verify the *custom* path), D18 (IME correctness is a MUST), ADR-0001 (escalation path), AC-24.
**Depends on:** A4 (gates). **Feeds:** A6 (consolidated verdict), A10 (production IME path).
**Vertical thread:** a probe app whose custom iced widget records every event it
receives; a headless test drives the widget's event handler with synthetic IME
events; the per-OS delivery question is answered from winit's platform support +
the recorded manual checklist; the verdict is written down.

**Goal.** Answer, with evidence, on each target OS at the pinned iced/winit
versions: **can IME preedit / commit / candidate events reach a custom iced
widget?** This is the one remaining external dependency of the custom-editor
decision (ADR-0001). Spike output is a written result, not production code.

**Decisions already locked.**
- Q1 is closed (ADR-0001): the custom widget is the decision; S1 verifies the
  custom path's viability, it does not re-open the question.
- Escalation path if delivery fails: upstream patches → fork iced → (last
  resort) renegotiate the pure-Rust constraint. Abandoning pure Rust is the
  last resort, not the first.
- IME correctness is a MUST for CJK users (D18); composition (preedit)
  rendering, commit, and candidate-window positioning are acceptance-tested per
  platform.

**What to implement.**
- `xtask/src/bin/s1_ime_probe.rs` — a minimal iced application hosting one
  custom widget (implementing the iced `Widget` trait via `iced::advanced`) that
  logs every event variant received in `on_event` (with timestamps) to stdout,
  with special recording for IME events (preedit text, commit text, candidate
  list). Run manually per OS with a real IME.
- `devdocs/checklists/ime-acceptance.md` — the per-OS × per-IME acceptance
  checklist (Japanese, Chinese Pinyin, Korean × Linux X11, Linux Wayland, macOS,
  Windows): launch probe → switch IME → type a composition → verify preedit
  appears in the widget's log → commit → verify commit text → verify candidate
  list events. This checklist is the AC-24 instrument A10 automates as far as
  possible.
- Headless synthetic tests (in the probe's crate, `xtask` tests): construct the
  widget, drive `on_event` with synthetic IME events (the exact iced event types
  at the pinned version — verify the names against the pinned iced), assert the
  widget records preedit text, commit replaces it, and candidates are received.
  These prove *our* handling; OS delivery is answered by the winit support
  matrix + the manual checklist.
- `devdocs/spikes/S1-ime-delivery.md` — per-OS event matrix (which winit
  version delivers which IME events on which backend), probe transcripts,
  checklist outcomes, and the delivery verdict per OS.

**How to implement.**
1. Pin the iced/winit versions (already pinned in `[workspace.dependencies]`);
   read their IME event plumbing (winit `WindowEvent::Ime::*` → iced event →
   `Widget::on_event`) and record the exact path per version.
2. Build the probe; run it on each OS (CI runs the synthetic tests on all
   three; the manual checklist is executed per OS and recorded — a spike may
   rely on recorded manual evidence, stated honestly as such).
3. Write the delivery verdict per OS: delivered / delivered-with-caveats
   (record the caveat) / not delivered (→ escalation).

**Tests, validation, verification.**
- `cargo test -p xtask` — synthetic IME event tests green on all 3 OSes in CI.
- Manual checklist executed per OS; transcripts recorded in the spike doc.
- Grep gates:
  - `grep -rn "TODO\|FIXME" xtask/src/bin/s1_ime_probe.rs devdocs/spikes/S1-ime-delivery.md` — empty.
  - `grep -c "verdict" devdocs/spikes/S1-ime-delivery.md` — ≥ 3 (one per OS).
- `just gate` green.

**Exit criteria.**
- [ ] S1-1 verdict recorded per OS (delivered / caveat / not delivered).
- [ ] Synthetic IME event tests green in CI on Linux, macOS, Windows.
- [ ] Manual checklist outcomes recorded (honestly labeled manual).
- [ ] If any OS cannot deliver: escalation per ADR-0001 documented and A7+
      blocked pending the decision. Otherwise: proceed.

### Slice A6 — S1 slice 2 of 2: editor-primitives audit + consolidated S1 verdict

**Spec basis:** §9 Phase 0 spike S1 (full checklist: caret geometry, HiDPI hit-test precision, undo-grouping determinism, multi-cursor, paste-from-HTML sanitization, cursor mapping across reload, markdown-widget coverage for read mode, AccessKit status), D14, ADR-0001, AC-25 (gated on S1).
**Depends on:** A5. **Feeds:** A7–A11 (the primitives the editor is built on), PLAN-D D4 (Renderer trait decision).
**Vertical thread:** a headless cosmic-text harness measures caret-rect and hit-test precision at fractional scales; the paste-sanitization, markdown-coverage, and AccessKit questions get recorded decisions; `devdocs/spikes/S1-verdict.md` records proceed / upstream / fork / renegotiate.

**Goal.** Complete spike S1: every checklist item answered with evidence, and
the ADR-0001 escalation decision recorded.

**Decisions already locked.**
- Undo grouping and the buffer model are `EditorModel`-owned (deterministic by
  construction, D14) — the audit verifies the widget adds no hidden state.
- No raw HTML is ever rendered (§0/§4.10) — the paste path must sanitize.
- Read mode sits behind a `Renderer` trait (§4.6); S1 records what iced's
  markdown widget does and does not cover.
- Screen-reader support is a SHOULD gated on this spike (D18); if the AccessKit
  path is missing, we contribute upstream or document the gap honestly (AC-25).

**What to implement.**
- `xtask/src/bin/s1_caret_probe.rs` — headless cosmic-text harness: shape text
  at scale factors 1.0 / 1.25 / 1.5 / 2.0; measure caret-rect precision and
  hit-test round-trip (char index → point → char index) error. Acceptance:
  round-trip error ≤ 0.5 px at every scale.
- Paste-from-HTML audit: trace the clipboard path (winit/iced clipboard
  flavors: `text/plain` + `text/html`); decide the sanitization path — HTML →
  plain text (strip tags, decode entities, preserve line structure) before
  insertion; evaluate a hand-rolled minimal converter vs `lol_html` (license
  check against deny.toml allowlist); record the decision and its owner
  (implementation lands in A11's paste path).
- Markdown-widget coverage audit: render a fixture document exercising tables,
  footnotes, images, task lists, nested lists through iced's markdown widget at
  the pinned version; record coverage per feature; record the extend/replace
  decision behind the `Renderer` trait (§4.6).
- AccessKit audit: verify whether iced's a11y integration (at the pinned
  version) can expose text-range nodes + caret from a custom widget; record the
  path or the gap + upstream-contribution plan.
- `devdocs/spikes/S1-verdict.md` — consolidated S1 verdict: every checklist
  item answered, the ADR-0001 escalation decision (proceed expected), any spec
  deltas recorded (a spec change is a spec change — record, don't drift).

**How to implement.**
1. Build the caret probe against the pinned cosmic-text; measure at all four
   scales on the reference machine; record numbers.
2. Run the clipboard/markdown/AccessKit audits; write each decision with its
   evidence and reversal condition.
3. Consolidate into the verdict doc; if the verdict is not "proceed", stop and
   record the escalation instead of building A7+.

**Tests, validation, verification.**
- `cargo test -p xtask` — caret-probe assertions (round-trip ≤ 0.5 px at all
  scales) run in CI on all 3 OSes.
- `cargo run -p xtask --bin s1_caret_probe` — prints the measured table
  (recorded in the spike doc).
- Grep gates:
  - `grep -c "decision" devdocs/spikes/S1-verdict.md` — every checklist item has a decision.
  - `grep -rn "TODO\|FIXME\|TBD" devdocs/spikes/S1-verdict.md` — empty.
- `just gate` green.

**Exit criteria.**
- [ ] **S1 closed:** verdict recorded in `devdocs/spikes/S1-verdict.md` with the
      ADR-0001 escalation decision.
- [ ] Caret/hit-test numbers recorded (≤ 0.5 px round-trip at all scales, or
      the recorded exception with a plan).
- [ ] Paste-sanitization, markdown-coverage, and AccessKit decisions recorded
      with owners.
- [ ] If verdict ≠ proceed: escalation documented; P6 slices blocked pending
      decision. Otherwise A7 is unblocked.

### Slice A7 — P6 slice 1 of 5: `flavite-editor` crate + `EditorSurface` + minimal working editor

**Spec basis:** §9 Phase 6, D14, ADR-0001, §4.1 (crate boundary), §4.2 (hosted Session), §4.5 (command execution), AC-14 (foundation), AC-32 (editor adapter).
**Depends on:** A6 (S1 verdict: proceed), PLAN-C C6 (scanner spans — needed from A8; A7 needs only EditorModel), C10 (`EditorModel`/`BufferManager`), C11 (working save handler), C14 (`flavite-api`).
**Coverage:** flavite-editor is Tier 3 (§10.5) — measured and reported, not gating. Tests must still be real and numerous.
**Vertical thread:** open a note in the demo app → type → the buffer changes → save writes the file; proven headless by driving the widget's event handler with synthetic events (no GPU required for `on_event`/`layout` — cosmic-text shaping is CPU).

**Goal.** The custom iced editor widget exists and works end-to-end at a minimal
level: it renders text, places the caret, and applies keystrokes as `EditOp`s
through typed commands to the Session-owned `BufferManager` (D7/§4.5). The `EditorSurface` trait (ADR-0001:
retained for testability/substitution) is defined and implemented.

**Decisions already locked.**
- The widget consumes a revisioned read-only `EditorModel` view from Lattice.
  Session owns BufferManager (§4.5); keyboard, selection, undo/redo, and edits
  dispatch typed commands with buffer identity/revision. Local handlers run
  synchronously on the GUI thread; no JSON conversion or worker round trip is
  required for typing. Widget-only state includes viewport, hover, and preedit.
- iced/wgpu/cosmic-text are permitted in `flavite-editor` (and `flavite-ui`
  only) — enforced by `arch check`.
- The stock `text_editor` is NOT a fallback (ADR-0001).
- Keystrokes never wait on highlighting or parsing (D14/D15 pattern: time-budgeted, off the input path).

**What to implement.**
- `crates/flavite-editor/Cargo.toml` — deps: `iced`, `cosmic-text`,
  `flavite-api`, `flavite-buffer`, `flavite-syntax`. (tree-sitter +
  tree-sitter-highlight are added in A9.)
- `src/lib.rs` — crate docs (tier: UI; responsibility: iced widget over
  EditorModel, §4.1), re-exports.
- `src/surface.rs` — `pub trait EditorSurface`: text access, caret/selection,
  decoration spans, hit-testing, viewport — the testability/substitution seam
  (ADR-0001). The iced widget implements it; test doubles implement it too.
- `src/widget.rs` — `EditorWidget` implementing `iced::advanced::Widget` over
  a revisioned read-only `EditorModel` view: `layout` via cosmic-text shaping (viewport-scoped: only
  visible lines shaped), `draw` (glyphs, caret, selection), `on_event`
  (keyboard → typed edit command carrying `EditOp` + buffer identity/revision,
  dispatched by the app's update loop through Lattice; pointer → selection
  command via hit-testing). Handle `StaleRevision` by refreshing the view.
- `src/keymap.rs` — key → action mapping (insert, caret moves, selection,
  select-all, undo/redo) with platform-correct modifiers.
- `src/viewport.rs` — scroll offset, visible-line range, stale-span policy
  (stale decorations draw until updated — input never waits).
- `examples/editor-demo.rs` — minimal iced app: hosts Lattice/Session through
  flavite-api, opens a fixture note, hosts the widget, and requests autosave on
  1 s idle. Pump operation effects/replies independently of notice display;
  show pending save and terminal receipt/failure separately.
- `devdocs/architecture/crates.toml` — add the `editor → {api, buffer, syntax}`
  edges (and the §4.1 diagram note per §10.2 rule 2); `arch check` green.

**How to implement.**
1. Crate skeleton + arch edges; `cargo xtask arch check` green before any code.
2. `EditorSurface` trait — keep it small and total (no `unwrap`, all errors
   typed).
3. Widget over `EditorModel`: cosmic-text `Buffer` shaping per visible line,
   caret rect from the shaper, hit-testing via `Buffer::hit` (verify method
   names against the pinned cosmic-text — S1 recorded them).
4. Event path: keyboard events → typed edit command → Lattice registry/Session
   → revisioned view → redraw; pointer → hit-test → selection command. Prove
   local edits complete without waiting on a worker or a notice acknowledgement.
5. Demo app; type in it; save; verify the file on disk.

**Tests, validation, verification.**
- Headless widget tests (`cargo test -p flavite-editor`): construct
  `EditorModel` with fixture text; drive `on_event` with synthetic keyboard and
  pointer events; assert buffer contents, caret position, selection extents,
  undo/redo behavior. Layout tests: line wrapping, viewport scoping (lines
  outside the viewport are not shaped). Hit-test tests: click → correct char.
- `EditorSurface` conformance: a test double implementing the trait proves the
  trait is implementable outside iced (ADR-0001's reason for existence).
- AC-32 adapter checks: commands carry view identity/revision; stale edits have
  no effects; synchronous local edits return a new revision; delayed worker
  completion after close/reopen cannot alter the new buffer. Exercise the
  same document commands through the headless host and compare outcomes.
- Commands: `cargo test -p flavite-editor` · `cargo clippy -p flavite-editor --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-editor/src` — empty.
  - `grep -rn "unsafe" crates/flavite-editor/src` — empty or each hit has a
    `// SAFETY:` comment (target: zero).
  - `grep -c "iced" crates/flavite-{fs,syntax,index,buffer,vault,api,graph,cli}/Cargo.toml` — 0 (layering, also enforced by arch check).
- Demo run recorded (transcript or screenshot in the PR).

**Exit criteria.**
- [ ] The demo app runs; typing edits the buffer; save writes the file.
- [ ] Headless widget tests green (buffer, caret, selection, undo, layout,
      hit-testing).
- [ ] `arch check` green with the new edges recorded.
- [ ] `just gate` green; flavite-editor coverage measured and reported (Tier 3).

### Slice A8 — P6 slice 2 of 5: link-aware editing (scanner decorations, ctrl/cmd-click, autocomplete)

**Spec basis:** D10 (one link grammar — the editor consumes `flavite-syntax`'s scanner spans), D8 (resolution semantics, click-creates-note), §4.5 (commands), §4.10 (URL scheme allowlist), §4.7 (autocomplete sources).
**Depends on:** A7, PLAN-C C6 (scanner spans with `res_state`), C7 (resolution via index), C14 (command emission).
**Vertical thread:** in the demo app, `[[Ideas]]` renders as a decorated span; ctrl/cmd-clicking it emits `note.open` with the resolved target; typing `[[` opens autocomplete and inserting a candidate produces a correct link; clicking an unresolved note-link creates the note (D8).

**Goal.** The editor treats wikilinks as first-class: decorated spans,
ctrl/cmd-click navigation, and caret-anchored autocomplete — all driven by the
single scanner semantics (D10), never by a second parse.

**Decisions already locked.**
- Link semantics come from exactly one function (the scanner); the editor never
  re-implements link detection (D10).
- Clicking a note-link whose target doesn't exist **creates the note** (D8).
- All navigation goes through the command registry (`note.open`, §4.5) — the
  palette, keybindings, and clicks all use the same path.
- Scheme allowlist for any URL opened from the editor: `http`, `https`,
  `mailto`; everything else (incl. `file:`) prompts (§4.10).

**What to implement.**
- `src/decoration.rs` — map `flavite-syntax` spans → decorations: wikilink,
  embed, tag, block-id styles; unresolved/ambiguous styling from the index's
  `res_state` (queried via flavite-api); code/math spans excluded (the scanner
  already excludes them — assert it in tests).
- `src/hit.rs` — ctrl/cmd + click: hit-test → span lookup → resolve target
  through flavite-api (`Query`/`LinksTo`-style lookup) → emit `note.open` with
  `{ target }` and the common Invocation metadata from spec §4.5.
- `src/autocomplete.rs` — on `[[` at the caret: candidate popup (nucleo matcher
  over the in-memory stem + alias list, per §4.7 — no DB hit per keystroke);
  keyboard navigation; insert `[[target]]` or `[[target|alias]]`; choosing an
  unresolved candidate creates the note (D8).
- `src/link_insert.rs` — the `link.insert` command (palette/keybinding path),
  sharing the autocomplete machinery.

**How to implement.**
1. Decorations: pull spans + `res_state` per note (via flavite-api query),
   map to style ranges; render in `draw` (under the text, non-overlapping).
2. Hit-testing: pointer position → char index (A7 machinery) → containing
   span → resolve → dispatch `note.open`.
3. Autocomplete: detect `[[` before the caret (scanner span at caret), fetch
   candidates, filter with nucleo as the user types, insert on choose/enter;
   handle alias form and `id:` form insertion (opt-in, D8).
4. Wire `link.insert` into the command registry (flavite-api).

**Tests, validation, verification.**
- Synthetic-event tests: ctrl/cmd-click on a `[[link]]` span → exactly one
  `note.open` invocation with the resolved target; click on plain text → no
  invocation; click on an unresolved link → `note.open` with `create: true`.
- Autocomplete tests: filtering, insert forms (`[[target]]`, `[[target|alias]]`,
  `[[id:…|alias]]`), keyboard nav, cancel.
- Decoration tests: span→style mapping incl. unresolved/ambiguous; spans inside
  code spans/blocks/math produce no decorations (D10 exclusion).
- Commands: `cargo test -p flavite-editor` · `cargo clippy -p flavite-editor --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-editor/src` — empty.
  - `grep -rn "pulldown\|tree_sitter" crates/flavite-editor/src/decoration.rs crates/flavite-editor/src/hit.rs` — empty (the editor does NOT parse; it consumes scanner spans — D10).
- AC-5 tie-in: the `parity` fuzz target (created in C6) runs in `just fuzz-smoke`; editor spans and indexer spans must agree — any divergence found while building this slice is a bug to fix in the scanner, not the editor.

**Exit criteria.**
- [ ] Ctrl/cmd-click on a wikilink emits the correct `note.open` (demo verified).
- [ ] Autocomplete inserts correct link forms; unresolved candidate creates the note.
- [ ] Decorations render for links/embeds/tags; unresolved/ambiguous styled.
- [ ] Tests green; `just gate` green; `just fuzz-smoke` green (parity target included).

### Slice A9 — P6 slice 3 of 5: find-in-note, conflict/diagnostic decorations, tree-sitter injection highlighting

**Spec basis:** D14 (decorations: conflict and diagnostic, find-in-note highlights, tree-sitter injection highlighting), D10 (tree-sitter's role: block structure + fenced-code injection only), §4.7 (in-note find overlays only), §4.9 (parser panic → `parse_ok=0`, never takes down the actor), §8 AC-14 (interactivity budget).
**Depends on:** A8, PLAN-C C6 (ts_ranges glue), C14 (BufferState/Diagnostic events).
**Vertical thread:** a 1 MB note with fenced code blocks stays interactive while
injection highlighting is time-budgeted; find-in-note highlights matches; a
conflicted buffer shows its decoration.

**Goal.** The editor's full decoration surface: find-in-note, conflict and
diagnostic decorations, and tree-sitter fenced-code injection highlighting —
chunked with a per-frame time budget so a 1 MB note stays interactive (D14).

**Decisions already locked.**
- Highlight work is chunked with a time budget per frame; stale spans draw until
  updated (D14).
- Tree-sitter is used for block structure + fenced-code injection only; link
  semantics remain the scanner's (D10).
- A parser panic marks the file `parse_ok=0` and moves on; it never takes down
  the widget (D14/§4.9); `catch_unwind` boundary around parse work.
- Find-in-note overlays apply to the editor only (dirty buffers overlaid only
  for in-note find, §4.7).

**What to implement.**
- `src/find.rs` — in-buffer find: Unicode default casefold + NFC matching,
  match list, next/prev (wrapping), highlight spans; incremental (re-scan from
  the edit point on buffer change), retaining original-position mappings for
  NFC/casefold expansions (§4.16.1).
- `src/diagnostics.rs` — decoration sources: `BufferState` (conflict banner
  state), `Diagnostic` events (`parse_ok=0`, `id_conflict`, ambiguous links,
  unresolved-link styling from A8).
- `src/ts_highlight.rs` — tree-sitter-highlight over fenced code blocks:
  grammar set small and pinned; chunked with a per-frame time budget (default
  4 ms/frame — below-spec constant, reversal: raise/lower based on AC-14
  measurements); `catch_unwind` boundary;   spans tagged with buffer identity/revision and mapped through typed
  TextByte coordinates. Discard or explicitly map stale worker output (§4.16.1).
- Deps added to `Cargo.toml`: `tree-sitter`, `tree-sitter-highlight`, and the
  pinned grammar crates for the small language set.

**How to implement.**
1. Find: casefold/NFC matcher over ropey text; incremental rescan on edit;
   highlight spans merged with A8's decorations (non-overlapping by
   construction — assert in tests).
2. Diagnostics: subscribe to BufferState/Diagnostic via flavite-api; map to
   decorations; conflict state additionally blocks autosave (D7 — the app
   enforces; the widget shows).
3. Injection highlighting: tree-sitter parse of fenced blocks only (block
   structure from `flavite-syntax::ts_ranges`); highlight events → token spans;
   chunk per frame within the 4 ms budget; on budget exhaustion, leave stale
   spans and continue next frame.
4. Panic isolation: `catch_unwind` around parse+highlight; on panic, mark the
   file's highlight cache invalid, emit a Diagnostic, continue.

**Tests, validation, verification.**
- Find tests: casefold + NFC fixtures (Turkish İ/ı must not break under default
  folding — §4.8), next/prev wrap, incremental rescan after edit.
- Injection tests: fixture code blocks → correct token spans per language;
  time-budget test (large doc: highlight yields within budget per frame,
  completes over multiple frames); panic-isolation test (poisoned input →
  Diagnostic emitted, no crash, subsequent frames fine).
- Decoration merge tests: find + link + diagnostic spans coexist without
  overlap.
- Commands: `cargo test -p flavite-editor` · `cargo clippy -p flavite-editor --all-targets -- -D warnings` · `just gate` · `just fuzz-smoke` (scanner + parity targets must stay green — D10's differential invariant).
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-editor/src` — empty.
  - `grep -rn "catch_unwind" crates/flavite-editor/src/ts_highlight.rs | wc -l` — ≥ 1.
- 1 MB note smoke: demo app opens a generated 1 MB note with 200 fenced blocks;
  typing stays responsive (manual + recorded; automated latency harness lands in A11).

**Exit criteria.**
- [ ] Find-in-note works with highlights and next/prev.
- [ ] Conflict/diagnostic decorations render from BufferState/Diagnostic events.
- [ ] Tree-sitter injection highlighting works, is time-budgeted, and survives
      parser panics.
- [ ] 1 MB note with code blocks stays interactive in the demo.
- [ ] Tests + `just gate` + `just fuzz-smoke` green.

### Slice A10 — P6 slice 4 of 5: IME production path + accessibility

**Spec basis:** D18 (IME is a MUST; AccessKit path), ADR-0001 (S1 verifies the custom path), AC-24, AC-25, §4.8 (Unicode hazards), §4.10 (no raw HTML).
**Depends on:** A9, A6 (S1 findings: what winit/iced deliver per OS).
**Vertical thread:** synthetic composition sequences (preedit → update → commit,
preedit → cancel) drive the widget headlessly and assert buffer behavior; the
per-OS IME acceptance checklist (AC-24) is automated as far as the platform
allows and recorded as a manual checklist where it is not.

**Goal.** IME composition works in the production editor path: preedit rendered
inline at the caret, commit replaces it, candidate handling per the S1 findings;
and the accessibility path (AccessKit text-range nodes + caret) is implemented
or honestly waived.

**Decisions already locked.**
- IME correctness is a MUST (D18): composition rendering, commit, and
  candidate-window positioning are acceptance-tested per platform.
- Screen-reader support is a SHOULD gated on S1 (D18): if the AccessKit path is
  viable it is implemented; if not, a documented, tracked waiver exists (AC-25).
- The buffer is plain text; preedit is a rendering state, never buffer content
  until commit.

**What to implement.**
- `src/ime.rs` — preedit inline rendering (composition text at the caret with a
  distinct style), commit replaces preedit; candidate handling per the S1
  findings (OS-managed candidate windows where winit supports them; caret-rect
  reporting to the OS per platform findings); preedit state kept in the widget,
  merged into the draw path without touching `EditorModel` until commit.
- `src/a11y.rs` — AccessKit nodes for text ranges + caret via iced's a11y
  integration (per the S1 verdict); if the pinned iced cannot expose custom
  widget nodes, record the waiver with the upstream-contribution plan (AC-25).
- `devdocs/checklists/ime-acceptance.md` — finalized from A5: per-OS × per-IME
  checklist; automated parts are tests, manual parts are recorded steps.
- CI: the synthetic IME tests run on all three OSes in G1.

**How to implement.**
1. Wire the widget's `on_event` to the IME event types per the pinned iced
   (names verified in A5); maintain preedit state; render inline.
2. Commit: dispatch one revision-guarded Session edit command for the committed
   text (one undo unit). Revalidate after intervening reload/edit; cancel or
   refresh stale composition rather than inserting at stale coordinates.
3. Candidate handling + caret-rect reporting per OS findings.
4. a11y: expose text-range nodes + caret; write the smoke test (a reader can
   read note text and move the caret — at least one reader per OS, or the
   waiver).

**Tests, validation, verification.**
- Synthetic composition tests (CI, 3 OSes): preedit appears, updates, commits
  (buffer receives exactly the committed text as one undo step); preedit
  cancel leaves the buffer untouched; candidate list received and recorded.
- Caret-rect reporting test per platform path.
- a11y smoke test (or the recorded waiver with reason + tracking reference).
- Commands: `cargo test -p flavite-editor` · `cargo clippy -p flavite-editor --all-targets -- -D warnings` · `just ci-gate` (per-OS jobs).
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-editor/src devdocs/checklists/ime-acceptance.md` — empty.
  - `grep -c "preedit" crates/flavite-editor/src/ime.rs` — implementation present (≥ 1).
- Manual checklist: executed per OS with a real IME (JA/ZH/KO), transcripts
  recorded — labeled honestly as manual evidence.

**Exit criteria.**
- [ ] AC-24: synthetic composition tests green on all 3 OSes; manual checklist
      executed and recorded for JA/ZH/KO per OS.
- [ ] AC-25: a11y smoke test green on at least one reader per OS, **or** a
      documented, tracked waiver exists (S1-dependent).
- [ ] Preedit never enters the buffer before commit (tested).
- [ ] `just gate` + `just ci-gate` green.

### Slice A11 — P6 slice 5 of 5: performance hardening + AC-14 harness

**Spec basis:** D14 (large files: viewport-scoped layout, time-budgeted highlight, 1 MB note MUST stay interactive), §8 AC-14 (typing latency ≤ 16 ms p99 @ 100 KB, ≤ 33 ms p99 @ 1 MB), §4.9/§7 (huge paste: chunked insert, warn above `MAX_NOTE_BYTES`).
**Depends on:** A10. **Feeds:** PLAN-D D5 (editor pane wiring), D8 (AC-12/13/17 measurements include the editor).
**Vertical thread:** a benchmark harness measures keystroke-to-frame latency on
100 KB and 1 MB notes on the reference machine; CI records the numbers (nightly
trends; G2 gates them).

**Goal.** AC-14 measured and green: keystroke-to-frame ≤ 16 ms p99 on a 100 KB
note, ≤ 33 ms p99 on a 1 MB note; large pastes are chunked; undo grouping is
deterministic.

**Decisions already locked.**
- Layout and highlight are viewport-scoped; highlight work is chunked with a
  time budget per frame (D14).
- A 1 MB note MUST stay interactive (D14); above `MAX_NOTE_BYTES` (32 MB)
  warn and degrade features (§4.10).
- Reference machine: 4+ cores, integrated GPU (Intel Iris Xe class), NVMe,
  16 GB RAM (§8).

**What to implement.**
- Viewport-scoped layout productionization: only visible lines shaped; lazy
  shaping above/below the viewport; shape cache keyed by (line, scale, width).
- `src/paste.rs` — chunked large-paste insert op: a 50 MB paste becomes chunked
  `EditOp`s (no frame stall, no OOM); above `MAX_NOTE_BYTES`, warn and degrade
  (no full parse/index of the pasted content beyond budget).
- Undo-tree integration with `EditorModel`'s time-based coalescing: deterministic
  undo grouping (same keystroke sequence → same undo tree — tested).
- `benches/editor_latency.rs` (criterion) + `xtask/src/bin/perf_editor.rs` —
  harness measuring keystroke-to-frame p99 on generated 100 KB and 1 MB notes
  (fixture notes checked in under `xtask/tests/fixtures/` or generated
  deterministically).
- CI: perf job running the harness on the reference machine class; nightly
  trends; G2 gates vs §8 thresholds (§10.3).

**How to implement.**
1. Profile the demo app on the 1 MB fixture (frame-time metric `editor.frame_ms`
   from §4.11); find the hot paths (shaping, highlight, draw).
2. Viewport-scope shaping; add the shape cache; re-measure.
3. Chunk paste; test 50 MB paste behavior (memory bounded, UI responsive).
4. Wire the harness into CI; record baseline numbers.

**Tests, validation, verification.**
- Latency benches: `cargo bench -p flavite-editor` — p99 numbers recorded.
- Perf CI job: numbers vs AC-14 thresholds (≤ 16 ms p99 @ 100 KB; ≤ 33 ms p99
  @ 1 MB) — green, or an honest report with a recorded target adjustment and
  reason (the spec's own reversal pattern).
- Chunked-paste test: 50 MB paste → bounded memory, UI responsive, buffer
  correct.
- Undo-grouping determinism test: same sequence → same tree (property test over
  random keystroke sequences).
- 1 MB interactivity smoke in the demo (typing, scrolling, find).
- Commands: `cargo test -p flavite-editor` · `cargo bench -p flavite-editor` · `just gate` · `just ci-gate` (perf job).
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-editor benches xtask/src/bin/perf_editor.rs` — empty.
  - `grep -c "frame_ms" crates/flavite-editor/src xtask/src/bin/perf_editor.rs` — metric present (observability carried by the slice, §4.11).

**Exit criteria.**
- [ ] AC-14 measured: p99 numbers recorded on the reference machine; targets
      met, or honestly reported with a recorded adjustment + reason.
- [ ] 50 MB paste handled without frame stalls or OOM.
- [ ] Undo grouping deterministic (property-tested).
- [ ] 1 MB note interactive in the demo (recorded).
- [ ] `editor.frame_ms` metric emitted; perf CI job green; `just gate` green.

## 6. Verification command index

| Command | Gate | Purpose |
|---|---|---|
| `just gate` | G0 (every slice) | fmt + clippy + test + `arch check`, ≤ 5 min, ends with `just gc` |
| `just ci-gate` | G1 (CI, 3 OSes) | G0 + coverage floors + FaultFs/integration suites + doc build + deny/audit/shear + fuzz smoke 60 s + network-denied + MSRV |
| `cargo xtask arch check` | G0 | drift + edges + layering |
| `cargo xtask arch gen` | — | regenerate `crate-graph.json` + `DEPENDENCIES.md` |
| `cargo xtask arch deps\|dependents\|path\|why\|violations` | — | RAG query CLI (JSON) |
| `cargo xtask cov-check` | G1 | coverage floors + ratchet |
| `just fuzz-smoke` | G0 hook + G1 (60 s) | every fuzz target over seeds + generated input |
| `cargo +nightly fuzz run <target>` | dev loop | local campaigns |
| `cargo test -p flavite-fs --all-features` | per slice | FaultFs/MemFs suites |
| `cargo test -p flavite-editor` | per slice | headless widget suites |
| `cargo test -p xtask` | per slice | arch/cov/clean/probe unit tests |
| `cargo llvm-cov -p <crate> --lcov` | per slice | Tier-1/2 floor measurement |
| `cargo clippy --workspace --all-targets -- -D warnings` | G0 | lint |
| `cargo fmt --all --check` | G0 | format |
| `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` | G1 | docs |
| `cargo deny check` · `cargo audit` · `cargo shear` | G1 | supply chain |
| `just clean --dry-run` · `just clean` · `just gc` | per slice | disk budgets (§10.10) |
| `just setup` | onboarding | fresh-clone toolchain + hook |
| `cargo run -p xtask --bin s1_ime_probe` / `s1_caret_probe` | spikes | S1 evidence |
| `cargo run -p flavite-editor --example editor-demo` | per slice | vertical demo |
| `cargo bench -p flavite-editor` | A11 | AC-14 latency |

## 7. Source-review and gate index

Text searches aid review; they cannot establish snapshot, rendering, or
transaction correctness. Compiler/architecture checks and behavioral tests
provide the acceptance evidence (§10.3).

```bash
# Layering: no iced-family deps in core-tier crates (also enforced by arch check)
grep -rn -E '^\s*(iced|iced_|winit|wgpu|cosmic-text)' crates/flavite-{fs,syntax,index,buffer,vault,api,graph,cli}/Cargo.toml   # expect: no matches
# No deferred work
grep -rn "TODO\|FIXME\|XXX" crates/*/src xtask/src fuzz/fuzz_targets   # expect: no matches
# No unsafe without justification
cargo clippy --workspace --all-targets --all-features -- -D warnings   # core crates forbid unsafe code
# FaultFs is a library, not test-only (§10.6)
grep -n 'cfg(test)' crates/flavite-fs/src/fault.rs   # expect: no matches
# Generated artifacts carry their header
head -1 devdocs/architecture/DEPENDENCIES.md          # expect: GENERATED marker
# Clean tree after dry-run
just clean --dry-run && test -z "$(git status --porcelain)"
# Arch green
cargo xtask arch check
```

## 8. Plan exit criteria

- [ ] AC-35 editor conformance: typed offset boundaries, Unicode/grapheme/bidi
      caret mapping, casefold expansion, mixed-EOL undo/redo, stale highlight
      output, and guarded dirty-buffer link patches (§4.16.1).
- [ ] AC-36 foundation: separate crash models and seeded persistence defects
      proven; native conformance is separately labelled in C2/C5.

- [ ] **Phase 0.0 complete (§9 exit):** full G0 green ≤ 5 min on a clean
      working tree with provisioned dependencies/warm build cache (AC-27); `cargo xtask arch check` fails on seeded violations —
      undocumented internal dependency and iced in a core crate (AC-28); a
      change dropping a gating crate below its §10.5 floor fails G1 (AC-29);
      `just fuzz-smoke` runs every checked-in corpus entry clean and catches a
      seeded crash (AC-30); `just gc` / `just clean --dry-run` behave per
      §10.10 and leave `git status` clean (AC-31).
- [ ] **S1 closed:** `devdocs/spikes/S1-verdict.md` records the ADR-0001
      escalation decision with per-checklist-item evidence (IME delivery per OS,
      caret geometry, hit-test precision, undo determinism, multi-cursor,
      paste-from-HTML, markdown coverage, AccessKit).
- [ ] **P6 complete (§9 exit):** the editor widget works end-to-end (demo);
      AC-14 measured (≤ 16 ms p99 @ 100 KB, ≤ 33 ms p99 @ 1 MB) or honestly
      reported with a recorded adjustment; AC-24 green on all OSes (or manual
      checklist recorded); AC-25 met or waived per S1; the `parity` fuzz target
      (created in C6) is green in `just fuzz-smoke` (AC-5 editor side).
- [ ] G0/G1 green on Linux, macOS, Windows; coverage: flavite-fs ≥ 90 %
      (Tier 1, from A2 onward); flavite-editor measured and reported (Tier 3).
- [ ] Nothing in this plan's files is left half-wired: every crate the slices
      created compiles, lints, tests, and is reachable from the demo app or the
      gates.



