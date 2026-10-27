# PLAN B — Graph workstream: graph spike (S2) + graph (P7)

**Specification:** RFC-0001 v2.1 with command and durability/identity amendments (§4.5/4.13–4.16/10.6.1) — canonical copy `./SPEC-draft-v2.md`; synchronized mirror `./RFC-0001-v2.1-spec.md`.
**Spec phases owned:** §9 Phase 0 spike S2, Phase 7 (P7).
**Acceptance criteria owned:** AC-16 (graph performance), AC-11 (attachment nodes half), AC-12/13 (UI-usable-during-rebuild, shared with PLAN-D), AC-32 (card command guards, shared with PLAN-C/D), AC-33 (feed resets), AC-35 (native pin identity).
**Depends on:** PLAN-A A1–A4 (gates, fuzz infra). B5 additionally depends on PLAN-C C7 (flavite-index) and C10 (BufferManager, for card edits in B7).
**Feeds:** PLAN-D D6 (graph pane integration consumes B9's graph view).
**Overlap note:** P7 is also listed in PLAN-D's grouping (P5+P6+P7). **Construction
of the graph is owned by this plan**; PLAN-D's D6 is the integration slice that
wires this graph view into the application shell. D6 does not rebuild graph
internals. This plan activates A1's existing `crates/flavite-ui` skeleton with **only**
`src/graph/*` plus examples — PLAN-D's D1 adds the application shell around it;
do not build shell pieces in this plan.

## 0. How to read this plan

Every slice is a **vertical cut** through the specification — implemented,
runs, rigorously proven, honestly validated — not a horizontal layer. The
"fully and honestly" bar (working end-to-end behavior; verification built
during the slice; debt paid in touched code; rigor in choices, edge cases, and
error paths; coherent shippable state; no false completion claims) applies to
every slice — see `README.md` in this directory. Locked spec decisions are
cited by number; an agent executes them, it does not re-design them.

## 1. Workstream context

D15 calls the graph "the second-hardest part of the project": a custom wgpu
pipeline plus a custom zoomable container. §9 kills the unknown first (S2), then
builds the production pipeline. The crate split is fixed by §4.1: `flavite-graph`
is CPU-only (layout, spatial); the wgpu `shader` widget, the `Zoomable`
container, and the `CardLayer` live in `flavite-ui` (the only crates permitted
to depend on iced/wgpu are `flavite-editor` and `flavite-ui`).

**D15's pipeline (locked):**

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

Locked algorithm facts: ForceAtlas2-style (degree-weighted repulsion + gravity +
adaptive per-node speed), Barnes-Hut θ≈0.8, new nodes spawn at their neighbors'
centroid plus jitter (warm start), layout cools and **freezes**, waking only on
topology change. Zoomed out: instanced draws with degree-based sizing, edge alpha
by zoom, edge count capped by importance; no locks on the frame path. Zoomed in:
a `Zoomable` container lays out real iced widgets at world positions; text is
**re-laid-out at the scaled font size** (quantized scale steps, cached), not
GPU-scaled. Pinned positions are user state (`.flavite/graph.json`) keyed by
`stable_id` else path, with carry-over **only from tiers T0–T2** (D8). Fallback:
software renderer → `canvas` with a node cap + a visible "reduced mode" notice.

## 2. Slice dependency graph

```
B1 (flavite-graph: GraphModel CSR + SpatialGrid + delta types)
 └─► B2 (S2-1: shader-widget prototype — creates flavite-ui/src/graph + probe)
      └─► B3 (S2-2: layout prototype — Barnes-Hut ForceAtlas2 + rayon + PositionBuffer)
           └─► B4 (S2-3: fallback + card-layer prototype + consolidated S2 verdict)
                └─► B5 (P7-1: production LayoutWorker + PositionBuffer + index feed)
                     └─► B6 (P7-2: production shader widget)
                          └─► B7 (P7-3: Zoomable + CardLayer production)
                               └─► B8 (P7-4: attachment nodes + pinned positions)
                                    └─► B9 (P7-5: AC-16 acceptance + perf CI)
                                                                                    └─► PLAN-D D6
```

Cross-plan edges: B5 needs PLAN-C C7 (`flavite-index` read API + `index.generation`
polling); B7 needs C10 (`BufferManager` for card checkbox toggles); B6/B8 need
C14 (`flavite-api` commands: `note.open`, `graph.focus`, pin commands). If PLAN-B
runs ahead of PLAN-C, B5+ wait; B1–B4 have no cross-plan dependencies.

## 3. Locked decisions this workstream implements (do not re-open)

| Spec ref | Decision | Where it lands here |
|---|---|---|
| D15 | LOD rendering, off-thread layout, one buffer authority; pipeline above | B1–B9 |
| D15 | Pinned positions are user state; carry-over only from tiers T0–T2; a cache never holds anything the user *did* | B8 |
| D15 | Attachment nodes (v2): squares sized by degree; embed edges included; thumbnails in cards are a MAY (default: colored square + filename) | B8 |
| D15/D8 / §4.5 | Card edits dispatch Session commands through api; buffer identity/revision guards dirty-buffer positions, and indexed spans carry source hash | B7; AC-32 |
| D8 | Embed-vs-link resolution preference (embed prefers attachment, link prefers note) feeds which edges exist | B5, B8 |
| §4.1/§10.2 | `flavite-graph` is CPU-only (layout, spatial); iced/wgpu only in `flavite-ui` | B1 vs B2+ split |
| §4.9 | GPU lost/device reset → recreate pipeline from `GraphModel` (CPU state is authoritative); GPU init fails → software renderer + reduced mode | B6, B9 |
| D15 reversal | If S2 shows 50k nodes can't hold 60 fps on integrated GPUs, lower the supported-scale target (e.g. 20k) and aggregate clusters at low zoom | B4 verdict, B9 |
| §10.5 | flavite-graph is Tier 2 (≥ 80 % line); flavite-ui is Tier 3 (measured, not gating) | coverage throughout |

## 4. Crate dependency contract (encoded in `devdocs/architecture/crates.toml`; full table + interpretation notes in PLAN-A §4)

| Edge | Justification |
|---|---|
| flavite-graph → — (B1–B4) / → flavite-index (B5+) | CPU-only layout/spatial; from B5, edge/node deltas derived by polling `index.generation` through the index read API (D15) |
| flavite-ui → flavite-graph | the graph view consumes the CPU model + PositionBuffer (D15 pipeline); permitted: ui is a UI-tier crate |
| flavite-ui → flavite-api | all commands/events/queries (D16) |
| flavite-ui → flavite-buffer (B7+) | read-only view/EditOp types for cards (D15); Session owns BufferManager and all mutations dispatch through api; direct dep recorded in crates.toml |
| flavite-editor → (none of this plan) | editor is PLAN-A's |

Layering: `flavite-graph` MUST NOT depend on iced/winit/wgpu/cosmic-text (enforced
transitively by `arch check`). The `graph → flavite-index` edge and the
`flavite-ui → flavite-graph` edge are recorded in `crates.toml` and the §4.1
diagram parentheticals in the same PR (interpretation note 2 in PLAN-A §4).

## 5. Slices

### Slice B1 — `flavite-graph` crate: `GraphModel` (CSR adjacency) + `SpatialGrid` + delta types

**Spec basis:** §4.1 (`flavite-graph`: layout, spatial — CPU only), D15 (GraphModel holds CSR adjacency; SpatialGrid does hit-testing).
**Depends on:** PLAN-A A1–A4. **Feeds:** B2–B9.
**Vertical thread:** a fixture set of notes/links/embeds → `NodeDelta`/`EdgeDelta`s → `GraphModel` → CSR adjacency correct → `SpatialGrid` hit-test returns the right node. Proven headless, no GPU.
**Coverage:** flavite-graph Tier 2 (≥ 80 %, §10.5) — the model/spatial code must meet the floor as it lands.

**Goal.** The CPU-side graph model and spatial index exist and are proven
headless: CSR adjacency with topology versioning, delta application, and a
spatial grid for hit-testing.

**Decisions already locked.**
- `GraphModel` is the CPU-authoritative state; the GPU pipeline is recreated
  from it on device loss (§4.9).
- Deltas are plain data (`NodeDelta`/`EdgeDelta`) defined here; whoever watches
  the index produces them (B5 wires the index feed; tests use synthetic deltas).
- CSR adjacency: `offsets: Vec<u32>` + `indices: Vec<u32>` + degree counts;
  `topology_version` bumps on every structural change (layout wakes on it).

**What to implement.**
- `crates/flavite-graph/Cargo.toml` — no internal deps at this slice (rayon
  added in B3, flavite-index in B5). `#![forbid(unsafe_code)]`.
- `src/model.rs` — `GraphModel`: nodes (note/attachment identity), CSR
  adjacency, degree counts, `topology_version`, `apply(deltas)`,
  `snapshot()` for the worker, node/edge counts.
- `src/spatial.rs` — `SpatialGrid`: uniform grid over world coordinates, cell
  size adaptive to zoom, `insert`/`remove`/`clear`, `hit(x, y, radius)` →
  nearest node.
- `src/delta.rs` — `NodeDelta::{Added{..}, Removed{..}}`,
  `EdgeDelta::{Added{..}, Removed{..}}` (the interface the index feed will
  produce, D15).
- `src/lib.rs` — crate docs (tier: core; responsibility: layout, spatial — CPU
  only, §4.1), re-exports.

**How to implement.**
1. Crate skeleton; `cargo check -p flavite-graph` green.
2. `GraphModel` with CSR arrays; apply deltas incrementally (rebuild CSR on
   batch apply — simple and correct; optimize later if profiling demands).
3. `SpatialGrid` with cell size = f(zoom); hit-test by nearest-cell scan.
4. Tests: CSR correctness vs. a naive adjacency oracle (property test over
   random delta sequences); add/remove; hit-test precision (click at node
   position ± 1 px returns the node; empty regions return `None`); 50k-node
   build smoke (< 1 s).

**Tests, validation, verification.**
- `cargo test -p flavite-graph` — green (unit + property tests).
- Property test (proptest): random delta sequences; model == naive oracle;
  `topology_version` monotonic; CSR invariants (offsets sorted, indices in
  range).
- Hit-test tests: precision, radius behavior, node removal.
- Coverage: `cargo llvm-cov -p flavite-graph --lcov` — ≥ 80 % on the code that
  exists (record; ratchets up as slices land).
- Commands: `cargo clippy -p flavite-graph --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-graph/src` — empty.
  - `grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-graph/Cargo.toml` — no matches (CPU-only, §4.1).
  - `cargo clippy --workspace --all-targets --all-features -- -D warnings` — green; compiler-enforced core `forbid(unsafe_code)`.

**Exit criteria.**
- [ ] `GraphModel` + `SpatialGrid` + delta types exist and are tested.
- [ ] Property tests green; hit-test precision proven.
- [ ] Coverage ≥ 80 % on existing code; `just gate` green.

### Slice B2 — S2 slice 1 of 3: shader-widget prototype (50k nodes, instanced wgpu, frame-time harness)

**Spec basis:** §9 Phase 0 spike S2 ("50k nodes in a `shader` widget plus rayon
Barnes-Hut layout, frame-time under load, tiny-skia fallback behavior, card layer
with real widgets under scale, attachment nodes"), D15 (instanced dots + lines,
degree-based sizing, edge alpha by zoom, edge count capped by importance).
**Depends on:** B1, PLAN-A A1–A4. **Feeds:** B3 (renderer to measure layout
against), B6 (production widget).
**Vertical thread:** a probe app renders a synthetic 50k-node / 200k-edge graph
(hub-heavy degree distribution) with instanced wgpu draws and per-frame
instrumentation; pan/zoom frame times are measured on the reference machine.

**Goal.** Answer with numbers: can an iced `shader` widget render 50k nodes /
200k edges at 60 fps pan/zoom on the reference machine's integrated GPU?

**Decisions already locked.**
- Rendering is instanced (dots + lines); degree-based node sizing; edge alpha by
  zoom; edge count capped by importance (D15).
- The render thread only swaps an `Arc` and never waits on layout (D15) — the
  prototype must already respect this (positions arrive via `PositionBuffer`).
- iced/wgpu only in `flavite-ui` (§4.1/§10.2).

**What to implement.**
- Activate A1's `crates/flavite-ui` skeleton (`Cargo.toml` + `src/lib.rs`; deps: `iced`,
  `flavite-graph`, `flavite-api` (added when C14 lands — at B2, api may not
  exist yet; keep the probe free of api deps and add them in B6), `wgpu` via
  iced's re-export at the pinned version).
- `crates/flavite-ui/src/graph/mod.rs` — module root (this plan owns `src/graph/*`
  only; PLAN-D adds the shell — do not build shell pieces here).
- `crates/flavite-ui/src/graph/shader_probe.rs` — the prototype `shader`
  widget: instanced vertex buffer (node positions + degree-based size), edge
  instances (200k lines, alpha by zoom), WGSL shaders, pan/zoom camera uniforms,
  per-frame ms instrumentation (rolling p50/p95/p99, printed + logged).
- `crates/flavite-ui/examples/graph-shader-probe.rs` — the probe app: builds a
  synthetic 50k/200k graph (deterministic seed), hosts the widget, runs the
  frame-time measurement, prints the table.
- `devdocs/spikes/S2-shader.md` — measured frame times on the reference machine
  (integrated GPU); CI note: headless CI uses a software Vulkan adapter
  (lavapipe) — CI numbers are advisory, the reference-machine numbers are the
  verdict.

**How to implement.**
1. Extend the existing flavite-ui skeleton with the graph module; arch check green
   (record the `ui → graph` edge; iced permitted in ui).
2. Implement the instanced renderer: node instance buffer (position, size,
   color-by-kind), edge instance buffer (two endpoints, alpha), camera uniform
   (pan/zoom), WGSL vertex/fragment programs.
3. Frame instrumentation: per-frame ms, rolling p50/p95/p99 over a 30 s
   pan/zoom script.
4. Run on the reference machine; record numbers in the spike doc.

**Tests, validation, verification.**
- CPU-side tests (`cargo test -p flavite-ui`): instance-buffer construction
  (counts, ordering, degree-based sizing, edge cap by importance, edge alpha by
  zoom levels).
- Shader validation: the probe creates a wgpu device in CI (software adapter)
  and asserts the pipeline compiles/links — catches WGSL regressions headless.
- `cargo run -p flavite-ui --example graph-shader-probe` — runs; frame table
  recorded in the spike doc.
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -rn -E '^\s*(iced|wgpu)' crates/flavite-graph/Cargo.toml` — no matches (graph stays CPU-only).

**Exit criteria.**
- [ ] Probe renders 50k/200k at measured frame rates on the reference machine.
- [ ] S2-1 verdict recorded in `devdocs/spikes/S2-shader.md` (proceed /
      adjust scale target).
- [ ] Instance-buffer tests + shader compile validation green in CI.
- [ ] `just gate` green.

### Slice B3 — S2 slice 2 of 3: layout prototype (Barnes-Hut ForceAtlas2, rayon, PositionBuffer)

**Spec basis:** §9 spike S2, D15 (ForceAtlas2-style: degree-weighted repulsion +
gravity + adaptive per-node speed; Barnes-Hut θ≈0.8; warm start at neighbors'
centroid + jitter; layout cools and freezes, wakes on topology change; LayoutWorker
on rayon, ≤ cores−1; PositionBuffer double-buffer with atomic Arc swap; no locks
on the frame path).
**Depends on:** B2 (renderer to measure against). **Feeds:** B5 (production worker).
**Vertical thread:** the probe app renders (B2) while the layout worker lays out
the 50k-node graph on rayon; frame-time p99 is measured **while layout runs** —
the AC-16 load condition.

**Goal.** Answer with numbers: does Barnes-Hut ForceAtlas2 lay out 50k nodes
with frame-time p99 ≤ 16.6 ms while layout runs, and does the layout converge
and freeze?

**Decisions already locked.**
- Knowledge graphs have hub-heavy degree distributions; plain
  Fruchterman-Reingold turns those into hairballs — ForceAtlas2-style forces
  are the locked choice (D15).
- Barnes-Hut θ≈0.8; adaptive per-node speed (local temperature) with a cooling
  schedule; layout **freezes** when cool and wakes only on topology change.
- Workers run behind a `catch_unwind` boundary (§4.2).
- The render thread never waits on layout: `PositionBuffer` is double-buffered
  with an atomic `Arc` swap (D15).

**What to implement.**
- `crates/flavite-graph/src/layout.rs` — the force model: Barnes-Hut quadtree
  (θ≈0.8), repulsion scaled by (deg_i+1)(deg_j+1), gravity toward the centroid,
  adaptive per-node speed, cooling schedule; `warm_start(new nodes at neighbor
  centroid + jitter)`; iteration step returns per-node displacements.
- `crates/flavite-graph/src/position.rs` — `PositionBuffer`: double buffer
  (write side for the worker, read side for the renderer), atomic `Arc` swap,
  generation counter.
- `crates/flavite-graph/src/worker.rs` — `LayoutWorker` prototype: rayon pool
  (≤ cores−1), message loop (deltas in → layout iterations → positions out),
  `catch_unwind` boundary, freeze on cool / wake on topology change.
- Deps added to flavite-graph: `rayon`.
- Probe app updated: layout runs concurrently with rendering; frame-time p99
  measured during layout; layout iteration time (`graph.layout_iter_ms`,
  §4.11) recorded.
- `devdocs/spikes/S2-layout.md` — iteration times, convergence curve,
  freeze/wake behavior, frame p99 under load.

**How to implement.**
1. Implement the quadtree + forces; unit-test force sanity (two nodes repel;
  gravity pulls to centroid; hub nodes repel more).
2. Implement the cooling/freezing state machine; test transitions.
3. Implement `PositionBuffer` swap semantics; test concurrent read/write
   (renderer never blocks: assert reads always see a complete buffer).
4. Wire the worker into the probe; measure frame p99 while layout runs.
5. Record numbers; adjust constants (θ, cooling rate) if p99 misses — record
   every adjustment with its measurement.

**Tests, validation, verification.**
- `cargo test -p flavite-graph` — force sanity, determinism (same input + seed
  → same layout), convergence (net displacement → ~0), freeze/wake transitions,
  panic isolation (poisoned input → worker survives, Diagnostic path).
- Property test: layout of N nodes terminates (bounded iterations) and
  positions stay finite/bounded (no NaN, no explosion).
- `cargo bench -p flavite-graph` — layout iteration benchmarks (50k nodes).
- Probe: frame p99 ≤ 16.6 ms while layout runs — measured on the reference
  machine and recorded.
- Commands: `cargo test -p flavite-graph` · `cargo clippy -p flavite-graph --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-graph/src` — empty.
  - `grep -rn "catch_unwind" crates/flavite-graph/src/worker.rs | wc -l` — ≥ 1.

**Exit criteria.**
- [ ] S2-2 verdict recorded in `devdocs/spikes/S2-layout.md` (iteration ms,
      convergence, frame p99 under load vs the 16.6 ms budget).
- [ ] Determinism, freeze/wake, and panic-isolation tests green.
- [ ] `just gate` green; coverage ≥ 80 % on existing flavite-graph code.

### Slice B4 — S2 slice 3 of 3: fallback + card-layer prototype + consolidated S2 verdict

**Spec basis:** §9 spike S2 (tiny-skia fallback behavior; card layer with real
widgets under scale; attachment nodes), D15 (fallback: canvas + node cap +
"reduced mode" notice; Zoomable container; text re-laid-out at scaled font size,
quantized steps, cached; CardLayer ≤ 64 real widgets; card content = preview +
parsed task lines; checkbox toggles through Session commands with buffer
identity/revision and indexed source-hash guards), §4.9 (GPU init fails → software renderer + reduced mode).
**Depends on:** B3. **Feeds:** B7 (production Zoomable/CardLayer), B9 (verdict actions), PLAN-D D6.
**Vertical thread:** the probe app runs under a software renderer → reduced
mode engages with a visible notice; zooming in lays out real widgets at world
positions with re-scaled text; a card's checkbox toggles through the
`CardEditSink` seam (production wiring in B7).

**Goal.** Answer with evidence: (a) does the software-renderer fallback behave
(canvas + node cap + notice), and (b) is the card layer viable — real widgets at
world positions, text re-laid-out at scaled size, ≤ 64 cards, checkbox toggles
through the buffer seam — at ≥ 60 fps?

**Decisions already locked.**
- Fallback: on tiny-skia (software) renderers, the graph falls back to `canvas`
  with a node cap and a visible "reduced mode" notice (D15).
- Text is re-laid-out at the scaled font size (quantized scale steps, cached) —
  never GPU-scaled (D15).
- Card edits dispatch Session commands with buffer identity/revision guards;
  indexed task spans carry source content hash. Session opens an unopened note
  through BufferManager and checks the guards (D7/D15/§4.5.2).
- D15's reversal: if 50k nodes can't hold 60 fps on integrated GPUs, lower the
  supported-scale target (e.g. 20k) and aggregate clusters at low zoom — the
  verdict must make this call explicitly.

**What to implement.**
- `crates/flavite-ui/src/graph/fallback.rs` — software-renderer detection →
  `canvas` fallback with a node cap + "reduced mode" notice widget.
- `crates/flavite-ui/src/graph/zoomable.rs` — `Zoomable` container prototype:
  world→screen transform, real iced widgets laid out at world positions, text
  re-laid-out at the scaled font size (quantized scale steps, cached).
- `crates/flavite-ui/src/graph/card_layer.rs` — `CardLayer` prototype: ≤ 64
  real widgets; card content = note preview + parsed task lines; checkbox
  toggle → a guarded edit request through a `CardEditSink` **trait**. The
  prototype uses a working in-memory sink with revision/source-hash checks;
  B7 replaces it with direct facade dispatch using the same request/result
  semantics. Test the prototype toggle path end-to-end.
- `devdocs/spikes/S2-cards.md` + consolidated **`devdocs/spikes/S2-verdict.md`**
  — fallback behavior, card-layer frame times, scale-target decision (confirm
  50k or adjust per D15's reversal, with numbers), attachment-node rendering
  approach confirmation.

**How to implement.**
1. Detect the renderer backend (iced/wgpu device type at the pinned version);
   route to canvas fallback; show the notice; cap nodes.
2. Build `Zoomable`: transform math, widget layout at world positions,
   quantized scale-step text cache; measure re-layout cost per zoom step.
3. Build `CardLayer` over the `CardEditSink` seam; test toggle → EditOp with
   precondition (success + hash-mismatch failure paths).
4. Measure card mode ≥ 60 fps with 64 cards; record.
5. Consolidate the S2 verdict (all three sub-spikes + scale target).

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — zoomable transform math; scale-step cache
  (hit/miss, quantization); card toggle → EditOp via the sink (success +
  stale revision/source hash → no edit + error); 64-card cap enforcement;
  fallback routing (software → canvas + notice shown).
- Probe runs in software mode (CI, headless) — reduced mode verified.
- `cargo run -p flavite-ui --example graph-shader-probe` — card-mode frame
  times recorded.
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src devdocs/spikes/S2-verdict.md` — empty.
  - `grep -c "reduced mode" crates/flavite-ui/src/graph/fallback.rs` — notice present.

**Exit criteria.**
- [ ] **S2 closed:** `devdocs/spikes/S2-verdict.md` records the scale-target
      decision (50k confirmed, or adjusted per D15's reversal with numbers) and
      the fallback/card verdicts.
- [ ] Reduced mode works under a software renderer (CI-verified).
- [ ] Card layer prototype meets its frame target with 64 cards.
- [ ] `just gate` green.

### Slice B5 — P7 slice 1 of 5: production `LayoutWorker` + `PositionBuffer` + index feed

**Spec basis:** §9 Phase 7, D15 (production pipeline), §4.2 (workers behind
`catch_unwind`), §4.4 (index generation is monotonic; read-your-writes), D12
(SQLite `index.generation` authoritative).
**Depends on:** B4, PLAN-C C7 (`flavite-index` read API). **Feeds:** B6–B9, PLAN-D D6.
**Vertical thread:** open a fixture vault in the probe app → the feed polls
`index.generation` → deltas flow into `GraphModel` → the worker lays out →
positions appear on screen → a new link (topology change) wakes the frozen
layout. Proven in the probe app.
**Coverage:** flavite-graph Tier 2 (≥ 80 %).

**Goal.** The production layout pipeline: index deltas → `GraphModel` →
`LayoutWorker` → `PositionBuffer`, with freeze/wake, warm starts, and no locks
on the frame path.

**Decisions already locked.**
- Poll the `(epoch, generation)` index version and consume §4.14's bounded
  invalidation feed. Deleted entities survive in the log; retention/epoch
  mismatch returns ResetRequired for a consistent snapshot. The graph → index edge is recorded
  in `crates.toml` (+ §4.1 diagram note) in this slice.
- Layout cools and freezes; wakes only on topology change; new nodes warm-start
  at their neighbors' centroid plus jitter (D15).
- No locks on the frame path: `PositionBuffer` is double-buffered with an
  atomic `Arc` swap (D15).

**What to implement.**
- `crates/flavite-graph/src/feed.rs` — `GraphFeed`: given an index reader and a
  cursor `(epoch, generation)`, consume a complete invalidation batch or
  ResetRequired (§4.14). Replace each affected node/outgoing set from the
  snapshot, removing absent IDs. Apply graph content and cursor together; do
  not reconstruct deletions from surviving live rows alone.
- `crates/flavite-graph/src/worker.rs` (productionize B3) — message-driven
  worker: topology deltas in, layout iterations, `PositionBuffer` out;
  deterministic seed; freeze on cool; wake on `topology_version` change;
  `catch_unwind`; rayon pool ≤ cores−1.
- `crates/flavite-graph/src/position.rs` (productionize B3) — double buffer +
  atomic swap + index epoch/topology version. Position/node-ID mappings from
  different topologies may never be combined; discard stale layout results.
- Probe app updated: opens a fixture vault through flavite-vault, polls the
  feed (250 ms tick + on `VaultChanged`), feeds the worker, renders positions.
- `devdocs/architecture/crates.toml` — add `graph → index` edge (+ §4.1 note);
  `arch check` green.

**How to implement.**
1. Implement the feed over the index read API; test against a fixture index
   (add/remove notes and links → exact delta sets; generation monotonic).
2. Productionize the worker (B3 code + message loop + determinism).
3. Wire the probe app end-to-end; verify: initial layout appears, freezes,
   wakes on a new link, warm-starts the new node near its neighbor.
4. Measure: layout iter ms, wake latency, frame p99 during layout (harness for B9).

**Tests, validation, verification.**
- Feed tests: fixture index → correct deltas for create/edit/rename/delete of
  notes and links; attachments included; generation bookkeeping exact.
- AC-33: delete nodes/edges, prune beyond the cursor, rebuild a live index with
  a new epoch, and change resolver candidates. Assert reset clears old cache IDs
  and reloads pins by native paths/unconflicted IDs; snapshots and positions
  remain topology-matched.
- Worker tests: determinism (property), freeze/wake, warm start (new node near
  neighbor centroid), panic isolation, position buffer never exposes a
  partial buffer (concurrent read/write property test).
- Probe: end-to-end run recorded.
- Commands: `cargo test -p flavite-graph` · `cargo clippy -p flavite-graph --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-graph/src` — empty.
  - `grep -rn -E '^\s*(iced|winit|wgpu|cosmic-text)' crates/flavite-graph/Cargo.toml` — no matches.

**Exit criteria.**
- [ ] Feed + worker + position buffer production and tested.
- [ ] Probe app shows a live, freezing, waking graph fed by a real vault index.
- [ ] Coverage ≥ 80 % (flavite-graph); `just gate` green; `arch check` green
      with the new edge recorded.

### Slice B6 — P7 slice 2 of 5: production shader widget

**Spec basis:** §9 Phase 7, D15 (instanced dots + lines, degree-based sizing,
edge alpha by zoom, edge count capped by importance, LOD via zoom/visible-count
threshold, SpatialGrid hit-test), §4.5 (commands: `note.open`, `graph.focus`),
§4.9 (GPU lost → recreate pipeline from GraphModel), §4.11 (`graph.layout_iter_ms`).
**Depends on:** B5, PLAN-C C14 (`flavite-api` commands). **Feeds:** B7–B9, PLAN-D D6.
**Vertical thread:** the probe app's graph is the production widget: pan/zoom at
60 fps, click a node → `note.open` dispatched via flavite-api, drag a node →
pin, GPU device loss → pipeline recreated from the model.

**Goal.** The production wgpu `shader` widget: LOD rendering, hit-testing routed
to commands, and GPU-loss recovery.

**Decisions already locked.**
- Zoomed out: instanced draws, degree-based sizing, edge alpha by zoom, edge
  count capped by importance (D15).
- Zoomed in: the `CardLayer` takes over (threshold on zoom/visible count) — B7.
- Click/drag on nodes → commands via the registry (`note.open`, `graph.focus`,
  pin) — §4.5 (palette, keybindings, menus, graph clicks ALL go through the
  command path).
- GPU lost/device reset: recreate the pipeline from `GraphModel` (CPU state is
  authoritative); never a blank canvas or a crash (§4.9).

**What to implement.**
- `crates/flavite-ui/src/graph/shader_view.rs` — production widget (from B2):
  LOD thresholds (zoom/visible-count), edge cap by importance, degree-based
  sizing, edge alpha by zoom; hit-testing via `SpatialGrid` → commands through
  flavite-api (`note.open` on click, `graph.focus` on ctrl/cmd-click, drag →
  pin candidate).
- `crates/flavite-ui/src/graph/gpu.rs` — device-loss handling: recreate the
  pipeline from `GraphModel` state; fallback routing to `canvas` + "reduced
  mode" notice (B4's fallback) on init failure.
- `crates/flavite-ui/src/graph/camera.rs` — pan/zoom camera (uniform math,
  inertia, clamps).
- Probe app updated to the production widget.

**How to implement.**
1. Port B2's renderer to production quality: LOD thresholds, importance-capped
   edges, degree sizing; keep instance buffers CPU-built and uploaded per frame
   only when positions changed (generation check).
2. Input: pointer → world coords → `SpatialGrid::hit` → command dispatch.
3. GPU-loss: hook device-loss/reset; rebuild pipeline from `GraphModel`
   snapshot; test with a simulated loss.
4. Wire `graph.focus` and pin commands through flavite-api.

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — LOD threshold behavior (zoom levels → which
  layer renders), edge cap by importance, hit-test → command mapping (click →
  `note.open` with the right note; drag → pin), camera math.
- GPU-loss simulation test: pipeline recreation from model state.
- Shader compile validation in CI (software adapter).
- Probe app run recorded (frame times, interaction transcript).
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -c "graph.focus\|note.open" crates/flavite-ui/src/graph/shader_view.rs` — commands routed via api (§4.5).

**Exit criteria.**
- [ ] Production shader widget: LOD, hit-test → commands, GPU-loss recovery —
      tested.
- [ ] Probe app runs the production widget with commands wired.
- [ ] `just gate` green.

### Slice B7 — P7 slice 3 of 5: `Zoomable` + `CardLayer` production

**Spec basis:** §9 Phase 7, D15 (Zoomable container; text re-laid-out at scaled
font size, quantized steps, cached; CardLayer ≤ 64 real widgets; card content =
`notes.preview` + parsed task lines overlaid by revisioned open-buffer views;
edits through Session commands with revision/source-hash guards), D7 (one
buffer authority).
**Depends on:** B6, PLAN-C C10 (`BufferManager`), C7 (previews/task lines from the
index), C14 (commands). **Feeds:** B8, PLAN-D D6.
**Vertical thread:** zoom in past the threshold → real note cards appear at
world positions with re-scaled text; toggling a card's checkbox edits the note
through the Session-owned `BufferManager` (with revision/source-hash guards); zoom out → instanced
dots again.

**Goal.** The zoomed-in half of the graph: real widgets at world positions,
buffer-authoritative card edits, ≥ 60 fps with ≤ 64 cards.

**Decisions already locked.**
- Text is re-laid-out at the scaled font size (quantized scale steps, cached) —
  never GPU-scaled (D15).
- Card content starts from indexed preview/task lines; an existing buffer's
  revisioned view takes precedence so unsaved edits show (D7/D15).
- Card edits dispatch Session commands through api with buffer identity/revision
  guards; indexed task spans also carry source content hash. The Session opens
  unopened notes through BufferManager and validates before applying (§4.5.2).
- ≤ 64 real widgets (D15).

**What to implement.**
- `crates/flavite-ui/src/graph/zoomable.rs` (productionize B4) — the container:
  world→screen transform, widget layout at world positions, quantized
  scale-step text re-layout cache.
- `crates/flavite-ui/src/graph/card_layer.rs` (productionize B4) — the layer:
  ≤ 64 cards from indexed previews/task lines and revisioned buffer views via
  flavite-api. Checkbox toggles dispatch a typed task command to Lattice with
  displayed buffer identity/revision, or indexed source hash for an unopened
  note. Session validates/opens/applies through BufferManager and returns the
  new revision. Replace B4's CardEditSink seam with the api adapter; no mutable
  BufferManager access is exposed to widgets.
- Card rendering: note title, preview excerpt, task lines with checkboxes;
  attachment cards: colored square + filename (thumbnails are a MAY — default
  off, D15).
- Deps: `flavite-buffer` added to flavite-ui (edge recorded in crates.toml).

**How to implement.**
1. Productionize `Zoomable`: scale-step quantization + cache; measure re-layout
   cost per step; evict cold entries.
2. Productionize `CardLayer` over Lattice commands/read views: render current
   buffer content when open; wire checkbox → guarded task command. A stale
   revision/hash refreshes the card before retrying; never blindly replay an
   edit at old offsets. A completed toggle means buffer-applied; save status
   follows Session receipts separately.
3. Delete the B4 `CardEditSink` seam (debt paid in the slice that touches it).
4. Measure card-mode frame times with 64 cards.

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — card content generation (preview + task lines),
  checkbox toggle → guarded Session command (success edits the buffer;
  stale revision/source hash refuses + refreshes), 64-card cap, scale-step
  cache behavior, zoom threshold handoff between shader view and card layer.
- AC-32: modify a dirty buffer without changing its disk base, then submit a
  toggle from an older card revision; no edit occurs. Open a note from an
  indexed card, verify source hash before applying, and compare equivalent
  task commands through the headless driver. No notice ack completes a toggle.
- Card-mode frame benchmark (64 cards) — ≥ 60 fps recorded.
- Commands: `cargo test -p flavite-ui` · `cargo clippy -p flavite-ui --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src` — empty.
  - `grep -rn "CardEditSink" crates/flavite-ui/src` — no matches (B4 seam deleted, not left to rot).

**Exit criteria.**
- [ ] Zoomable + CardLayer production, wired to Session commands/revisioned
      views through api; stale-card AC-32 checks green; B4 seam removed.
- [ ] Card-mode ≥ 60 fps with 64 cards (measured).
- [ ] Checkbox toggle edits the note (integration-tested through the buffer).
- [ ] `just gate` green.

### Slice B8 — P7 slice 4 of 5: attachment nodes + pinned positions

**Spec basis:** §9 Phase 7, D15 (attachment nodes: squares sized by degree;
embed edges included; thumbnails in cards are a MAY — default colored square +
filename; pinned positions are user state in `.flavite/graph.json`, keyed by
`stable_id` if present else path, carry-over only from tiers T0–T2), D8 (tier
rules), §1 (user state is ignorable), §4.12 (user-state files carry a `version`
and preserve unknown fields), AC-11 (attachment half).
**Depends on:** B7, PLAN-C C7 (attachments table + embed edges), C13 (tier
carry-over rules). **Feeds:** B9, PLAN-D D6.
**Vertical thread:** an `![[diagram.png]]` embed appears as a square node with an
edge from the embedding note; pinning a node writes `.flavite/graph.json`;
reopening restores the pin; an external (T3) rename does **not** carry the pin.

**Goal.** Attachments are first-class graph citizens, and pinned positions are
correct user state with tier-gated carry-over.

**Decisions already locked.**
- Attachment nodes render as squares, sized by degree; edges from `![[embeds]]`
  are included (D15, v2/R3).
- Pinned positions are user state (`.flavite/graph.json`), keyed by unconflicted
  stable_id, otherwise lossless native PathKey (§4.16.2). No T2 carry-over on
  duplicate IDs; T0/T1 retain explicit/native evidence. A cache
  must never hold anything the user *did*.
- User-state files carry a `version` and preserve unknown fields on write
  (§4.12).
- Deleting `.flavite/` loses pins (annoying, not catastrophic — §1) and MUST
  NOT orphan safety state (D2 registry — PLAN-C).

**What to implement.**
- `crates/flavite-graph/src/model.rs` — attachment node kind (square, degree
  sizing); embed edges (links `kind='embed'` → `dst_att`) in the feed (B5's
  feed extended).
- `crates/flavite-ui/src/graph/nodes.rs` — node visuals: notes (dots),
  attachments (squares); attachment cards: colored square + filename
  (thumbnails MAY — default off, D15).
- `crates/flavite-ui/src/graph/pins.rs` — pin/unpin commands; persistence to
  `<vault>/.flavite/graph.json` (versioned, unknown fields preserved); keying
  by unconflicted stable_id else native PathKey; carry-over from T0–T2 only
  with valid evidence (consumes C13's tier
  outcomes).
- Tests + fixtures: an attachment embed appears as a node + edge; pin round-trip;
  unknown-field preservation; state deletion without live journals loses pins
  but retains local safety access. Never clean pending journal payloads.

**How to implement.**
1. Extend the feed: attachments → nodes; embed links → edges (embed prefers
   attachment per D8 — the index already resolves; the feed just reads
   `links.dst_att`).
2. Node visuals + attachment cards.
3. Pin persistence: read/write `graph.json` with `version` + unknown-field
   preservation (serde `#[serde(flatten)]` extra map); keying rules.
4. Tier-gated carry-over: consume rename-tier outcomes (T0–T2 carry; T3/T4 do
   not) — the red-team R6 regression test lives here.

**Tests, validation, verification.**
- `cargo test -p flavite-graph` — attachment nodes in the model; embed edges
  from the feed.
- `cargo test -p flavite-ui` — pin/unpin round-trip; `graph.json` unknown-field
  preservation; keying by `stable_id` vs path; **tier matrix test**: T0/T1/T2
  carry the pin, T3/T4 do not (R6 regression); attachment card rendering data.
- AC-11 (graph half): `![[diagram.png]]` resolves to the attachment, appears as
  a graph node, and is not reported unresolved (integration test with the
  index).
- Commands: `cargo test -p flavite-{graph,ui}` · `cargo clippy --workspace --all-targets -- -D warnings` · `cargo xtask arch check` · `just gate`.
- Grep gates:
  - `grep -rn "TODO\|FIXME" crates/flavite-ui/src crates/flavite-graph/src` — empty.
  - `grep -c "version" crates/flavite-ui/src/graph/pins.rs` — versioned user state (§4.12).

**Exit criteria.**
- [ ] Attachment nodes + embed edges work (AC-11 graph half green).
- [ ] Pinned positions persist, round-trip, preserve unknown fields, and
      carry over only from T0–T2 (tier matrix tested).
- [ ] `just gate` green.

### Slice B9 — P7 slice 5 of 5: AC-16 acceptance + perf CI

**Spec basis:** §8 AC-16 (50k nodes / 200k edges, zoomed out, 60 fps pan/zoom;
frame-time p99 ≤ 16.6 ms **while layout runs**; card mode with ≤ 64 cards
≥ 60 fps), §10.3 (G2 gates perf vs §8 thresholds; nightly benchmarks), §4.9
(weak GPU → reduced mode), §7 (graph perf risk: frame-time metric with bench CI).
**Depends on:** B8. **Feeds:** PLAN-D D6, D8.
**Vertical thread:** a benchmark harness drives a 50k-node/200k-edge fixture
vault through the production graph (probe app): zoomed-out pan/zoom at 60 fps,
frame p99 ≤ 16.6 ms while layout runs, card mode ≥ 60 fps with 64 cards; CI
records the numbers (nightly trends; G2 gates them).

**Goal.** AC-16 measured and green on the reference machine, with the perf CI
in place and the weak-GPU reduced-mode path verified.

**Decisions already locked.**
- AC-16 thresholds are targets to be re-validated in spike results (§8) — S2's
  verdict (B4) already confirmed or adjusted the scale target; this slice
  measures the production graph against the confirmed target.
- Reduced mode: on weak GPUs (frame-time metric), switch to canvas + node cap
  + "reduced mode" notice (D15/§4.9).
- Reference machine: 4+ cores, integrated GPU (Intel Iris Xe class), NVMe,
  16 GB RAM (§8).

**What to implement.**
- `xtask/src/bin/perf_graph.rs` — benchmark harness: generates the 50k/200k
  fixture vault (deterministic), drives the production graph (probe app in
  headless-bench mode): zoomed-out pan/zoom script, layout running concurrently,
  card mode with 64 cards; measures frame p99 and fps.
- `crates/flavite-ui/src/graph/reduced.rs` — frame-time watchdog: sustained
  p99 above budget → switch to reduced mode + show the notice (auto, with the
  metric feeding Diagnostics, §4.11).
- CI: perf job (nightly trends + G2 gate vs §8 thresholds); the harness runs
  on the reference-machine-class runner.

**How to implement.**
1. Build the harness; run on the reference machine; record the AC-16 table.
2. Implement the reduced-mode watchdog; test the switch + notice (simulated
   slow frames).
3. Wire CI; record baseline; set the G2 gate.

**Tests, validation, verification.**
- `cargo test -p flavite-ui` — reduced-mode watchdog tests (threshold crossing
  → switch + notice; recovery path).
- Perf harness run: AC-16 numbers recorded (fps zoomed out; p99 while layout
  runs; card-mode fps) — green vs thresholds, or an honest report with a
  recorded target adjustment and reason.
- Commands: `cargo test -p flavite-ui` · `cargo run -p xtask --bin perf_graph` · `just gate` · `just ci-gate` (perf job).
- Grep gates:
  - `grep -rn "TODO\|FIXME" xtask/src/bin/perf_graph.rs crates/flavite-ui/src/graph` — empty.
  - `grep -c "layout_iter_ms\|frame" crates/flavite-ui/src/graph/reduced.rs xtask/src/bin/perf_graph.rs` — metrics present (§4.11).

**Exit criteria.**
- [ ] **P7 complete (§9 exit): AC-16 measured** — 60 fps zoomed-out pan/zoom,
      frame p99 ≤ 16.6 ms while layout runs, card mode ≥ 60 fps with ≤ 64
      cards — on the reference machine; green or honestly reported.
- [ ] Reduced-mode watchdog works (tested); perf CI job green with trends.
- [ ] flavite-graph coverage ≥ 80 % (Tier 2); flavite-ui measured (Tier 3).
- [ ] `just gate` + `just ci-gate` green.

## 6. Verification command index

| Command | Gate | Purpose |
|---|---|---|
| `just gate` | G0 (every slice) | fmt + clippy + test + `arch check`, ≤ 5 min, ends with `just gc` |
| `just ci-gate` | G1 (CI, 3 OSes) | G0 + coverage + suites + doc + deny/audit/shear + fuzz smoke + network-denied + MSRV |
| `cargo xtask arch check` | G0 | drift + edges + layering (graph must stay iced-free) |
| `cargo test -p flavite-graph` | per slice | model/spatial/layout/worker suites |
| `cargo test -p flavite-ui` | per slice | widget/card/pin/fallback suites |
| `cargo llvm-cov -p flavite-graph --lcov` | per slice | Tier-2 floor (≥ 80 %) |
| `cargo run -p flavite-ui --example graph-shader-probe` | B2–B9 | vertical probe/demo |
| `cargo run -p xtask --bin perf_graph` | B9 | AC-16 harness |
| `cargo bench -p flavite-graph` | B3+ | layout iteration benchmarks |
| `cargo clippy --workspace --all-targets -- -D warnings` | G0 | lint |
| `cargo fmt --all --check` | G0 | format |
| `just fuzz-smoke` | G0 hook + G1 | fuzz targets over seeds + generated input |
| `just clean --dry-run` · `just clean` · `just gc` | per slice | disk budgets (§10.10) |

## 7. Source-review and gate index

Text searches aid review; they cannot establish snapshot, rendering, or
transaction correctness. Compiler/architecture checks and behavioral tests
provide the acceptance evidence (§10.3).

```bash
# flavite-graph stays CPU-only (§4.1, enforced by arch check too)
grep -rn -E '^\s*(iced|iced_|winit|wgpu|cosmic-text)' crates/flavite-graph/Cargo.toml   # expect: no matches
# No deferred work
grep -rn "TODO\|FIXME\|XXX" crates/flavite-graph/src crates/flavite-ui/src xtask/src   # expect: no matches
# No unsafe in core tier
cargo clippy --workspace --all-targets --all-features -- -D warnings   # core crates forbid unsafe code
# No locks on the frame path (PositionBuffer is Arc-swapped)
grep -rn "Mutex\|RwLock" crates/flavite-graph/src/position.rs   # expect: no matches
# Catch-unwind boundary on parse/layout work (§4.2)
grep -rn "catch_unwind" crates/flavite-graph/src/worker.rs | wc -l   # expect: >= 1
# Reduced-mode notice exists (D15 fallback)
grep -c "reduced mode" crates/flavite-ui/src/graph/fallback.rs   # expect: >= 1
# Arch green
cargo xtask arch check
```

## 8. Plan exit criteria

- [ ] AC-33/35 graph-side checks pass for deletion/retention/epoch resets,
      topology-matched positions, native pin keys, and duplicate-ID handling.

- [ ] **S2 closed:** `devdocs/spikes/S2-verdict.md` records the shader, layout,
      and fallback/card verdicts plus the scale-target decision (50k confirmed
      or adjusted per D15's reversal, with numbers).
- [ ] **P7 complete (§9 exit):** AC-16 measured and green (60 fps zoomed-out
      pan/zoom; frame p99 ≤ 16.6 ms while layout runs; card mode ≥ 60 fps with
      ≤ 64 cards) on the reference machine, or honestly reported with a
      recorded adjustment.
- [ ] AC-11 (graph half) green: attachment embeds resolve, appear as nodes,
      and are never reported unresolved.
- [ ] Pinned positions: user state, versioned, unknown-field-preserving,
      T0–T2-only carry-over (tier matrix tested).
- [ ] flavite-graph coverage ≥ 80 % (Tier 2); flavite-ui measured (Tier 3);
      G0/G1 green on all three OSes.
- [ ] `crates/flavite-ui` contains only `src/graph/*` + examples at the end of
      this plan — the shell is PLAN-D's D1; nothing here is half-wired into a
      shell that doesn't exist yet.


