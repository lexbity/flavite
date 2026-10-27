# Lattice — implementation plans (RFC-0001 v2.1)

This directory contains the specification and the four workstream implementation
plans derived from it. All plans are **vertical-slice plans**: every slice is a
fine-grained, end-to-end path through the specification that is implemented,
runs, and is rigorously proven — not a horizontal layer ("all the types", "all
the wiring").

| File | Contents |
|---|---|
| `SPEC-draft-v2.md` | Canonical specification (RFC-0001 v2.1 with command and durability/identity amendments, §4.5/4.13–4.16/10.6.1). |
| `RFC-0001-v2.1-spec.md` | Synchronized mirror of `SPEC-draft-v2.md` in this directory. Update both together and verify with `cmp`; there is no separate `devdocs/SPEC-draft-v2.md` in this checkout. |
| `PLAN-A-editor-S0-S1-P6.md` | Workstream A — repository bootstrap (§9 Phase 0.0, "S0"), editor-viability spike (S1), custom editor widget (Phase 6 / P6). |
| `PLAN-B-graph-S2-P7.md` | Workstream B — graph spike (S2), graph model / layout / widget (Phase 7 / P7). |
| `PLAN-C-core-S3-S4-P1-P2-P3-P4.md` | Workstream C — filesystem-truth spike (S3), search/DB spike (S4), core (P1), buffer & safety (P2), rename/move transactions (P3), API surface (P4). |
| `PLAN-D-ui-P5-P6-P7.md` | Workstream D — UI shell (P5) + integration of the editor (P6) and graph (P7) into the shell. |

## Why four workstreams

§9 of the specification orders phases by dependency and risk. These plans regroup
the same phases into four parallelizable workstreams so independent tracks can
proceed concurrently against one locked specification:

- **PLAN-A** retires the two non-product risks first: the development
  infrastructure itself (§10 — "gates adopted after the code exists are gates
  that get bypassed") and the editor-viability unknown (S1, ADR-0001's
  escalation path). It then delivers the editor widget (P6), the project's
  largest single cost (D14).
- **PLAN-B** retires the graph unknown (S2) and delivers the graph (P7), the
  second-hardest part of the project (D15).
- **PLAN-C** is the headless core: the spikes that prove the filesystem and
  search/DB designs (S3, S4), then core (P1), buffer & safety (P2), rename
  transactions (P3), and the API surface (P4). ~80 % of the system stays
  testable without a GPU because of this workstream (§4.1).
- **PLAN-D** is the UI: the application shell (P5) and the integration of the
  editor and graph into it.

## Overlap between plans (read this before executing)

The requested grouping assigns **P6 to both PLAN-A and PLAN-D**, and **P7 to
both PLAN-B and PLAN-D**. Resolution, stated once and applied everywhere:

- **Construction is owned by PLAN-A (P6) and PLAN-B (P7).**
- **PLAN-D's P6/P7 slices (D5, D6) are integration slices**: they wire the
  delivered editor widget and graph view into the application shell (panes,
  autosave loop, graph focus, pin UI). They do **not** rebuild editor or graph
  internals.
- **Phase 0.0 ("S0") is owned by PLAN-A (slices A1–A4).** PLAN-B, PLAN-C, and
  PLAN-D assume `just gate`, `just setup`, `cargo xtask arch check`,
  `just fuzz-smoke`, and `just gc` / `just clean` exist and are green.

## Execution order (dependency DAG)

The command-contract amendment adds C10 → C8: the Session requires the buffer
foundation. C10 now also needs C6's shared pure text/coordinate types
(buffer → syntax); C6 still runs after the gates alone. A7's working
save demo additionally requires C11. C14's facade can land before C11–C13;
those slices register their implemented handlers and extend conformance tests
when it already exists. D1 can host the initial facade; D3/D5 still require
the actual conflict/recovery/save handlers. No future behavior is stubbed.

```
A1 → A2 → A3 → A4                                      [bootstrap/gates]
A4 → A5 → A6                                          [S1]
A4 → B1 → B2 → B3 → B4                                [S2]
A4 → C1 → C2 → C5;  A4 → C3, C4, C6; C6 → C10       [core front]
C6 + C3 + C4 → C7
C5 + C6 + C7 + C10 → C8 → C9
C8 + C10 → C11 → C12 → C13                            [save/journal/repair]
C7 + C8 + C9 + C10 → C14                              [initial api facade]
                    C11–C13 extend implemented handlers in either merge order
A6 + C10 + C11 + C14 → A7 → A8 → A9 → A10 → A11       [editor; A8 needs C6/C7]
B4 + C7 → B5 → B6 (+ C14) → B7 (+ C10) → B8 (+ C13) → B9
B2 + C14 → D1 → D2
D1 + C11 + C13 → D3;  D1 + C6 + C7 + A6 → D4
D1 + D3 + A11 → D5;  D1 + B9 → D6
D1–D6 → D7 → D8
```

## Parallel execution

Short answer: **the plans can be executed partially in parallel — but the slice
DAG above, not the plan grouping, is the unit of parallelism.** Two things gate
everything else, and the plans converge hard on PLAN-C's chain.

### A4 runs first, alone

A1–A3 build the bootstrap gate (fmt/clippy/tests, then cleanup). A4 promotes
it to full G0 with arch check and establishes G1; other workstreams wait for
that exit. A1 initializes git before hooks/PR workflows; A4 records active
edges only, separately from the planned final graph. Empty crates report
coverage N/A until implementation activates their floor.

### The parallel front after A4

These six slices are mutually independent (no cross-plan data dependencies):

| Slice | Plan | Needs |
|---|---|---|
| A5 (S1-1 IME probe) | A | A4 |
| B1 (graph model + spatial) | B | A1–A4 |
| C1 (S3-1 watcher probe) | C | A1–A4 |
| C3 (S4-1 search probe) | C | A1–A4 |
| C4 (S4-2 db probe) | C | A1–A4 |
| C6 (flavite-syntax) | C | gates only |

C10 follows C6 for shared TextSnapshot/coordinate types; it has no C8 dependency.

C2's "depends on C1" is sequencing, not a data dependency — C2 can join the
front if S3 is run fully parallel.

### Wave structure

```
A1–A4 → parallel front: A5/A6, B1–B4, C1/C2/C5, C3/C4, C6→C10
         │
         └─ C7 + C5 + C6 + C10 → C8 → C9 → initial C14
                                   └─ C11 → C12 → C13

initial C14 + C11 + A6 → A7–A11 → D5 (also needs D1/D3)
C7 + B4 → B5;  C14 → B6/B7;  C13 → B8/B9 → D6 (also needs D1)
initial C14 + B2 → D1/D2;  C11/C13 → D3;  A6 + C6/C7 → D4
D1–D6 → D7 → D8
```

C14 does not wait for C13, and D1 does not wait for A11/B9. Handler
registration and conformance tests accumulate as the safety slices land.

### Where it serializes (the cross-plan chains)

- **PLAN-A's P6 (A7–A11) is downstream of PLAN-C**: A7 needs C10 + C11 + C14 (+
  the A6 verdict); A8 needs C6 + C7. The editor lane is mostly idle between
  A6 and C14.
- **PLAN-B's production half (B5–B9) is downstream of PLAN-C**: B5 needs C7,
  B6 needs C14, B7 needs C10, B8 needs C13. Only B1–B4 (model + S2 spike)
  run early.
- **PLAN-D is downstream of the initial facade**: D1/D2 need C14 (D1 also B2);
  D4 also needs A6, and D3 needs C11/C13 handlers. D5 needs A11; D6 needs B9.
  The graph tail includes the cross-plan chain:
  C8 → C11 → C12 → C13 → B8 → B9 → D6, because C13 (tiered rename inference)
  gates B8's pin carry-over.
- **PLAN-C is mostly sequential**: C8 needs C5+C6+C7+C10; C11 needs C8+C10;
  C12 needs C7+C8+C10+C11; C13 needs C12; C14 needs C7+C8+C10+C9.
  Parallelism inside C is limited to the early pairs (C6 ∥ C1, C10 ∥ C5,
  C3/C4 ∥ C1).

### Shared-artifact coordination (the real cost of parallel PRs)

- **`devdocs/architecture/crates.toml` + generated artifacts** — every
  edge-adding slice (A7, B2/B5/B7, C7, C8, C14, D1) touches these, and
  `arch check` enforces exact edge equality + drift. Rule: after any merge
  that touched a `Cargo.toml`, run `cargo xtask arch gen && cargo xtask arch
  check` before pushing. Conflicts here are small and mechanical.
- **Root `Cargo.toml` `[workspace.dependencies]`** — A1 pre-pinned the majors;
  late small additions (diffy/similar, strsim, directories, fs4/fd-lock, the
  YAML crate, tree-sitter-highlight) will collide across plans. Use a shared
  dependency-queue PR or accept trivial merges.
- **`crates/flavite-ui` activation** — A1 creates the empty skeleton; B2 adds
  its graph module, then D1 adds the shell. In
  practice this resolves itself (D1 needs C14, far later), but D1 must not
  recreate the crate.
- **`justfile` / `.github/workflows/`** — A4 owns the base; perf jobs land in
  A11/B9/D8 (late, low concurrency). Low risk.
- **`xtask/src/bin/` probes, `devdocs/spikes/`, `fuzz/`** — distinct files per
  spike/target; `just fuzz-smoke` discovers targets dynamically, so partial
  target sets are fine. Only PLAN-C adds fuzz targets — no cross-plan
  contention.
- **Coverage ratchet** — per-crate vs `main`'s lcov; parallel PRs are fine as
  long as each keeps its own crate above its §10.5 floor.

### Recommendation

Run **lanes by slice, not by plan**. With four teams, teams 2–4 all have
work in the early front (A5, B1–B4, C1/C3/C4/C6/C10 — eight slices), then
everyone funnels onto the C chain. The UI lane is idle earliest — assign it
C-chain slices (C9, C10 after C6, C11) rather than leaving it blocked. The two long
tails (editor: A7–A11 → D5; graph: B6–B9 → D6) converge at D7/D8, which is
where final integration happens.

One-time prerequisite: this directory is not yet a git repository. The plans
assume PRs, hooks, and branch protection — **A1 must include `git init` +
initial commit on `main`** before any parallel branching starts.

## Shared rules for every slice in every plan

### Shared command execution contract

Spec §4.5.1–4.5.5 is authoritative for all four workstreams. The command system
already planned in C14 now has explicit ownership, ordering, and results:

- `flavite-vault::Session` owns BufferManager and pending operations;
  `flavite-api::Lattice` owns Session + registry. GUI and CLI host the same
  headless engine on their respective driving threads. Local edits execute
  synchronously; serialization/disk/index work remains off the GUI thread.
- Widgets consume revisioned read views and emit commands. Position-based
  edits require buffer identity/revision; indexed task spans also require their
  source hash. The disk `base_hash` guards a different authority.
- Pending acceptance, buffer application, durable replacement, and structural
  index application are explicit milestones. Save receipts identify the saved
  revision, hash, and index generation. Newer edits retain Dirty/recovery state;
  partial failures report actual disk outcome and safe retry behavior.
- Correlated operation requests/replies are bounded and lossless, with
  nonblocking admission and explicit overload/worker-loss handling. Notice
  display/report acknowledgements never apply edits or advance transactions.
- C8/C10 establish the engine; C11–C13 add behavior; C14 exposes the registry
  and migrates CLI; A7/B7/D1/D5 prove consumers. AC-32 accumulates in G1 as
  handlers land, including GUI/CLI equivalence and interleaving tests.

### Remaining review contracts

| Contract | Authority | Slice/test owners |
|---|---|---|
| Root + UUID ownership before identity writes/recovery; copied/moved vault bindings | §4.13 | C2/C8; AC-9/22 |
| Epochs, bounded invalidation log including deletions, atomic search checkpoints, consumer reset | §4.14 | C3/C7/B5/D2/D6; AC-33 |
| Prepare vs commit, durable payloads/checkpoints, completed undo cancellation, terminal-only cleanup | D9/§4.15 | C8/C11/C12/D3/D5; AC-2/10/32 |
| Forced nominees and periodic full content verification | D5 | C8/D8; AC-34 |
| Native PathKey vs lookup keys; typed snapshot coordinates; duplicate-ID policy | §4.16 | C5/C6/C10/C13/A7/B8; AC-35 |
| ProcessCrash vs PowerLoss, persistence/short-write negative tests, native proof limits | §10.6.1 | A2/C2/C5/C11/C12; AC-36 |

D6 guarantees snapshots of observed prior bytes; post-read external writes can
still be lost. Full-cache deletion is rebuildable; live transaction journals and
local recovery/history have different lifetimes. Source searches are review
aids; acceptance requires the behavioral/compiler/structural checks. G0 timing
uses a provisioned warm build cache with a clean working tree; cold setup and
complete hook time are measured separately. Both spec copies remain identical.

### Slice discipline

1. **Vertical cut.** Each slice is a fine-grained path through the whole
   specification that is implemented, runs, rigorously proven, and honestly
   validated — not a horizontal layer.
2. **The "fully and honestly" bar** (non-negotiable, per slice):
   - Deliver **working, end-to-end behavior** — not a stub, not a scaffold, not
     a TODO renamed as done.
   - **Build verification as it builds the behavior**: tests written inside the
     slice, exercising real behavior, capable of actually failing. No tests
     that assert nothing, mock away the thing under test, or are tuned to pass.
   - **Pay off the debt in the code the slice touches** — smells,
     anti-patterns, dead code, and disorder in the files the slice modifies are
     fixed *within the slice*. Deferred cleanup is cut cleanup.
   - **Meet the rigor bar**: in-slice choices defended (nothing silent), edge
     cases handled, error paths real, types tight, no shortcuts.
   - **Leave the system in a coherent, shippable state** — nothing broken,
     half-wired, or dirtier than before.
   - **Not claim completion on partial work** — ship what is genuinely done and
     name precisely what remains.
3. **Locked decisions are implemented, not re-designed.** Spec decisions are
   cited by number (D1–D18, §-sections, AC-numbers, R1–R18, ADR-0001/0002).
   Where a slice must make a below-spec choice (a constant, a file layout inside
   a crate, a test strategy), the choice is stated and defended in the slice,
   and every default carries a stated reversal condition.
4. **Gates.** Every slice ends with `just gate` (G0, ≤ 5 min on the reference
   machine) green plus the slice's own verification commands. G1 (`just
   ci-gate`) must stay green in CI on Linux, macOS, and Windows. Coverage
   floors (§10.5): Tier 1 (flavite-fs, flavite-syntax, flavite-buffer,
   flavite-vault, flavite-index) ≥ 90 % line; Tier 2 (flavite-api,
   flavite-graph) ≥ 80 %; Tier 3 (flavite-cli, flavite-editor, flavite-ui)
   measured and reported, not gating.
5. **Carry your own failure handling, observability, and security** for the
   behavior a slice introduces — those are not a later slice's problem for the
   path the slice ships.
