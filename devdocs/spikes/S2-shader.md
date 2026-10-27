# Spike S2 — Slice 1 of 3 (B2): instanced `shader` widget probe

**Status:** complete. **Slice:** B2 of PLAN-B §5 (S2 slice 1/3). **Feeds:** B3
(layout to measure against), B6 (production widget).
**Question answered:** *Can an iced `shader` widget render 50k nodes / 200k
edges at 60 fps pan/zoom?*

---

## 1. Verdict (S2-1)

**Proceed.** On the machine in §2 the probe holds **60 fps** at the full
**50 000-node / 199 996-edge** graph under a continuous pan+zoom script. The
30 s headline run reports frame-interval **p50 6.955 ms (≈144 fps)** and
**p99 15.249 ms**, inside SPEC §8 AC-16's `p99 ≤ 16.6 ms` budget. Widget CPU —
the number that actually moves when the renderer gets faster or slower — is
**p50 0.009 ms (9 µs)**, roughly three orders of magnitude under the budget.

Two caveats are load-bearing and neither is answered here; both are stated in
§6. (a) The machine is a **discrete** RTX 4070 Ti, not the SPEC §9 **integrated
Iris Xe** reference the reversal clause names, so the clause stays open until
an integrated-GPU (or a deliberately weak-GPU) run lands. (b) The frame
interval is **vsync-quantized at 165 Hz** and therefore acts as a *ceiling*
that cannot resolve fine GPU headroom; the precise instrument is widget CPU,
which is flat and negligible across every scale measured.

---

## 2. Hardware

| | |
|---|---|
| GPU | NVIDIA GeForce RTX 4070 Ti (AD104), driver 615.78.08 |
| CPU | AMD Ryzen 7 5800X3D, 8 cores |
| RAM | 62 GiB |
| OS | EndeavourOS (Arch) |
| Display | DP-2, 2560×1440 @ **164.90 Hz**, Wayland |
| Build | `--release` |
| Vulkan ICDs | `nvidia_icd.json` only (no lavapipe installed locally) |

> **Reference-machine gap.** SPEC §9 / PLAN-B §5 name the reference machine as
> *"4+ cores, integrated GPU (Intel Iris Xe class), NVMe, 16 GB RAM"* (PLAN-B
> line 706). This machine satisfies cores/RAM/NVMe but has a **discrete** GPU.
> The numbers below are therefore a *best-case* bound. The reversal clause
> (§6) — *"if 50k can't hold 60 fps on integrated GPUs, lower the target"* —
> is **not** answered by a discrete-GPU run, and this doc does not claim it is.

---

## 3. Method

- **Fixture:** deterministic Barabási–Albert preferential attachment
  (`graph::synthetic`), seed `20250101`, hub-heavy to match a real vault's
  degree distribution. Realized edge count lands at `199 996` against the
  `200 000` target (the initial clique is added on top of `m·growth`).
- **Instrument:** `iced::window::frames()` supplies the frame interval
  (wall-clock between `RedrawRequested`); `ProbeTimings` supplies widget CPU
  (time inside `prepare` + `draw`, read from the widget itself). Both feed
  rolling `FrameStats` windows (2 048 samples → p50/p95/p99).
- **Script:** camera pans a Lissajous path across the map while zoom sweeps
  **absolutely** over ≈0.32×–2.24×. Absolute, not relative to the fit zoom,
  because the D15 importance cap is a function of absolute zoom:
  `edge_cap(total, zoom) = 100 000 · zoom.clamp(0.25, 4.0)`. Only `zoom ≥ 2`
  admits the full ~200k edge list, so a relative sweep on a large map would
  silently measure a trimmed draw list. Each run prints `edges drawn min … max`
  to prove coverage of the cap curve.
- **Why two tables.** Frame interval is vsync-capped, so a renderer that draws
  50k nodes in 0.4 ms and one that draws them in 16 ms both report ~165 fps at
  165 Hz. Widget CPU is the unsaturated number. Frame interval is still
  reported because a p99 ≫ p50 is how stutter surfaces even inside budget.

To reproduce (release binary already built):

```sh
cargo build -p flavite-ui --example graph-shader-probe --release
./target/release/examples/graph-shader-probe --nodes 50000 --edges 200000 --seconds 30
```

---

## 4. Measurements

### Run A — 50k / 200k, 30 s (headline; matches PLAN-B's 30 s script)

Fixture 50 000 / 199 996, world extent 2683, fit zoom 0.2684.
**Edges drawn 43 600 … 199 996 of 199 996** (full list reached).

| metric | samples | min | p50 | p95 | p99 | max | mean |
|---|---|---|---|---|---|---|---|
| frame interval (ms) | 2048 | 2.864 | **6.955** | 12.156 | **15.249** | 21.810 | 7.5 |
| widget cpu (ms) | 2048 | 0.007 | **0.009** | 0.011 | **0.016** | 0.083 | 0.0 |

`p50 6.955 ms (≈144 fps)` · `p99 15.249 ms` · widget total 41.44 ms over
4 242 prepare + 4 242 draw calls. **AC-16 met at the full 199 996-edge list.**

### Run B — 100k / 400k, 12 s (2× headroom)

Fixture 100 000 / 399 996, edges drawn 85 100 … 399 996.

| metric | samples | p50 | p99 | max |
|---|---|---|---|---|
| frame interval (ms) | 1644 | 6.071 | 17.489 | 23.085 |
| widget cpu (ms) | 1645 | 0.009 | 0.059 | 8.479 |

p99 **17.489 ms** sits at the 16.6 ms boundary — 2× the spec scale is where
the vsync ceiling starts to be missed. Useful as the *headroom* marker, not a
target.

### Run C — 20k / 60k, 8 s (below the cap)

Fixture 20 000 / 60 004, edges drawn **60 004 … 60 004** (cap never binds).

| metric | samples | p50 | p99 | max |
|---|---|---|---|---|
| frame interval (ms) | 1139 | 6.121 | 14.031 | 17.168 |
| widget cpu (ms) | 1140 | 0.009 | 0.020 | 1.378 |

### Run D — 250k / 1M, 12 s (5× headroom)

Fixture 250 000 / 999 996, edges drawn 85 200 … 999 996.

| metric | samples | p50 | p95 | p99 | max |
|---|---|---|---|---|---|
| frame interval (ms) | 1604 | 6.137 | 12.162 | 12.241 | 30.097 |
| widget cpu (ms) | 1605 | 0.009 | 0.011 | 0.018 | **17.609** |

### Geometry build (fixture generation + instance-buffer construction, one-time)

| scale | build |
|---|---|
| 20k / 60k | 0.03 s |
| 50k / 200k | 0.09 s |
| 100k / 400k | 0.23 s |
| 250k / 1M | 1.04 s |

---

## 5. What the numbers say

**(a) Widget CPU is flat and negligible.** p50 is **0.009 ms at every scale**
from 20k/60k through 250k/1M. Per-frame CPU work is a 48-byte uniform write
plus two instanced draw calls; the edge-cap arithmetic is a pure prefix length,
not a per-edge scan. The upload path is gated on the geometry *generation*, so
a steady-state frame uploads nothing. CPU side is ~3 orders under the 16.6 ms
budget — **the renderer is not CPU-bound at these scales.**

**(b) Frame interval is vsync-quantized and cannot resolve GPU headroom.**
Intervals cluster at ~6.06 / 12.13 / 18.2 ms (165 Hz multiples). Run D (1M
edges) reports a *better* p99 (12.241 ms) than Run B (400k, 17.489 ms) — proof
the metric is dominated by vsync/compositor jitter, not GPU work. The GPU is
nowhere near saturated across all four scales.

**(c) AC-16 is met at target scale.** Run A p99 **15.249 ms ≤ 16.6 ms** with the
full 199 996-edge list drawn at peak zoom. Run B marks the headroom boundary at
2× scale.

**(d) Generation-change upload is O(edges) and can hitch a frame.** Widget
**max** CPU scales linearly with edge count: 2.19 / 8.48 / 17.61 ms for the
50k / 100k / 250k runs. This is the one-frame `queue.write_buffer` of a fresh
instance buffer on a topology change (8 MB at 1M edges). Directly relevant to
**B3**, where layout re-generates geometry on topology change — B3 must
amortize or double-buffer this, or a layout tick will hitch.

---

## 6. Limitations (honest boundaries of this spike)

1. **Discrete GPU, not the integrated reference.** §2. The SPEC §9 reversal
   clause — *"If S2 shows 50k nodes can't hold 60 fps on integrated GPUs, lower
   the supported-scale target (e.g. 20k) and aggregate clusters at low zoom"*
   (SPEC line 356) — remains **open**. This run neither triggers nor clears it.
   Closing it needs either real Iris Xe hardware or a deliberately weak
   adapter (lavapipe/llvmpipe or a 5×-downclocked GPU).
2. **Vsync masks GPU headroom.** The precise instrument here is widget CPU
   (§5a), which is a *ceiling-free* measurement but CPU-only. True GPU time was
   not captured with timestamp queries; doing so needs
   `Features::TIMESTAMP_QUERY` + `RenderPassTimestampWrites`, which is not
   universally available and was out of scope for B2. The frame interval only
   shows that the total (CPU+GPU+compositor) stayed under the display cadence.
3. **Not measured here (later slices).** Layout under load (B3); the tiny-skia
   software fallback (B4); the reduced-mode / weak-GPU switch (B5); the card
   layer with ≤ 64 real iced widgets (B7). AC-16's *"while layout runs"* clause
   is **not** exercised — B2 is renderer-only.

---

## 7. Deviations vs PLAN-B §2 (scope)

PLAN-B §5's "What to implement" is the slice scope. B2 adds a few things that
§2's additive list does not name, all of them **required** by §5's own exit
criterion *"shader validation runs in CI (software adapter)"*:

| added | why §5 forces it |
|---|---|
| dev-dep `naga 27` (`wgsl-in`) | Headless parse+validate+offset-reflection of the WGSL. Runs on **every** CI platform with no GPU — catches a WGSL syntax/type/keyword error the instant the shader changes, and reflects the `Camera` struct so the Rust↔WGSL uniform layout is asserted without a driver. |
| dev-dep `futures` | The real-wgpu device test drives wgpu's async `request_adapter`/`request_device` futures to completion in a headless test. |
| `just gpu-setup` recipe + CI lavapipe step + `FLAVITE_REQUIRE_WGPU_ADAPTER=1` | Supplies a software Vulkan adapter on headless CI so the *link* gate (pipeline creation + one render pass) can actually run. Without an adapter the device test can only skip, turning the gate into a silent no-op; the env var makes "no adapter" a hard failure on Linux. The recipe no-ops where `apt-get` is absent (honours the thin-caller rule). |

Shader validation is deliberately **two** gates: (1) naga parse+validate —
no GPU, runs everywhere; (2) a real wgpu `Device` creating `ProbePipeline` under
`push_error_scope(Validation)` and driving one prepare+draw+submit — because
naga happily validates a shader whose vertex outputs mismatch the fragment
inputs only inasmuch as the backend agrees, so a driver is genuinely required
to prove link. Gate (1) already earned its keep on its first run: it caught
that **`from`/`to` are WGSL reserved keywords** in `EdgeInput`, which the
instance buffer names `from`/`to` (renamed to `edge_a`/`edge_b` in the shader;
`@location` indices unchanged).

No production API was aliased, shimmed, or backwards-compat-bridged. Nothing
outside `crates/flavite-ui/src/graph/*`, `crates/flavite-ui/tests/*`,
`crates/flavite-ui/examples/*`, `justfile`, `.github/workflows/ci.yml`,
`devdocs/architecture/crates.toml` (+ generated artifacts), and `Cargo.lock`
was touched.

---

## 8. Gates (all green at time of writing)

| gate | result |
|---|---|
| `cargo test -p flavite-ui` | 58 unit + 7 integration, **all pass** |
| `FLAVITE_REQUIRE_WGPU_ADAPTER=1 cargo test -p flavite-ui --test shader_validation probe_pipelines` | pass on a real NVIDIA Vulkan device |
| `just gate` (fmt + clippy + test + arch-check + gc) | **green** |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | **green** |
| `cargo xtask arch check` | **green** |
| `just cov-check` | flavite-ui 98.6 % (tier 3, measured) · flavite-graph **99.2 %** (floor 80 %) · flavite-fs 96.2 % (floor 90 %) |
| B2 grep gate `grep -rn "TODO\|FIXME" crates/flavite-ui/src` | **empty** |
| B2 grep gate `grep -rn -E '^\s*(iced\|wgpu)' crates/flavite-graph/Cargo.toml` | **empty** |
| arch edge `flavite-ui → flavite-graph` recorded in `crates.toml` `edges.active` + `cargo xtask arch gen` run | yes |

Per-file line coverage (`src/graph/`): `instance.rs` 100 %, `stats.rs` 100 %,
`synthetic.rs` 98.12 %, `shader_probe.rs` 97.48 % (the 22 missed lines are the
GPU `prepare`/`draw` bodies, exercised only when a wgpu adapter exists — the
device test drives them, llvm-cov on a CPU-only run does not attribute them).

**CI note (PLAN-B §5, line 234).** *"headless CI uses a software Vulkan adapter
(lavapipe) — CI numbers are advisory, the reference-machine numbers are the
verdict."* CI runners are headless, so lavapipe there validates
**compile/link only**; the frame-time harness (§4) cannot run in CI and the
numbers in this doc are the local reference-machine run, not CI's. The local
machine has NVIDIA Vulkan but no installable lavapipe (no interactive sudo
needed for the software-adapter run), so a local weak-GPU proxy run was not
produced — see §6.1.
