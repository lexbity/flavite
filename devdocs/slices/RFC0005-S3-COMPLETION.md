# RFC-0005 Slice S3 — Mirror handoff: cached snapshot, lock out of `build` (F1)

> Completion record. Locked decisions honored: §4.6 (handoff cache keyed
> on the cursor; owned view stays the splice target; memory 2×), NFR-7,
> FR-8. `build`'s signature stays `&SavedGraph` (deref through the `Arc`)
> — the S4 spec's `&Arc<SavedGraph>` is its own change, not this slice's.

## Summary

`GraphMirror` now hands pumps an `Arc<SavedGraph>` snapshot keyed by the
mirror cursor (`view_arc()`), and the service pump releases the mutex
before overlay assembly: the critical section is exactly `synchronize` →
poison → parity-when-due → `view_arc`. The doctor's `check_mirror_parity`
can no longer wait behind a `build` pass — measured worst wall **3.07 ms**
across 461 concurrent parity calls during a live delta stream (NFR-7
budget: 200 ms). The keystroke path (stamp-only republish over a quiet
index) reuses the cached `Arc`: zero `O(V)` work, proven by counters.
Clone cost moved from per-publish to per-delta-window-that-reaches-a-pump.

## Mirror (`graph_mirror.rs`)

- `handoff: Option<(IndexVersion, Arc<SavedGraph>)>` on `GraphMirror` —
  one slot, written only by the mutex holder, replaced wholesale: a torn
  snapshot is structurally impossible (§4.11 row).
- `view_arc(&mut self) -> Option<Arc<SavedGraph>>`: cache hit (free `Arc`
  clone) while `handoff` is keyed at the cursor; miss = one clone of the
  owned view + wholesale slot replace + `snapshots_built_total` increment.
  Returns `None` with no cursor position (pre-reset), matching the pump's
  existing "graph mirror holds no view" error path. (The §4.6 sketch
  signature returns `Arc`; `Option` preserves the no-view error shape the
  service already handles — same semantics, honest surface.)
- **Invalidation discipline** — documented at the method: a successful
  delta needs no explicit invalidation (`synchronize` advances the cursor
  strictly forward — the `UpToDate` early return proves it — so the key
  check misses exactly once per window). `force_reset` DOES drop the slot:
  a drift-repair reset can land at the *same* version the poisoned
  snapshot was keyed at (poison/parity repair with no newer commit), and
  the drifted bytes must never be handed out again. **The S3 spec sentence
  ("force_reset … invalidate nothing explicitly — the version key check IS
  the invalidation") is not safe for that case; the slot-drop is the
  deviation, documented in the code and pinned by the regression test.**
- `MirrorStats.snapshots_built_total` — cold builds of the handoff
  (per-pump `view_arc` misses), §5.2 counter table.
- Module header gained the F1 handoff-contract section (the doc beside
  the enforcement); `Synchronize`'s enum doc now names `view_arc` as the
  pump path and `view()` as the under-lock comparison path (replacing the
  now-false "read while the mirror is borrowed … without cloning" claim —
  the snapshot does clone once per window, which is exactly §4.6).

## Service (`graph_service.rs`)

- Pump restructure (minimal, per S3 — the loop rewrite is S5): the
  critical section block ends with `view_arc`; `crate::graph_projection::build`
  consumes `&snapshot` after the guard drops. Verifiable by reading the
  diff: the `build` call sits after the block's `};` (lines 639–653).
- `Counters::publish_mirror` moved after `view_arc` so the published set
  always includes the pump's own snapshot build — making the identity
  `snapshots_built == deltas + resets` hold at *every* settled read
  (machine-checked invariant in tests, not a sampling artifact).
- `GraphServiceMetrics.mirror_snapshots_built_total` surfaced; the
  `Shared::mirror` doc now states the FR-8 invariant (S2's honest interim
  wording replaced, exactly as promised there).
- Untouched (S3's MUST-NOT-change column): wake-on-hint, restart policy,
  statuses, `GraphFrame`/`RenderKey`, pins, scope path, `Directive::Poll`
  mechanics, `build` itself.

## Doctor (`doctor.rs`)

`graph-mirror-parity` report line extended with `snapshots_built=`
(the §8 "report line extended" row). No logic change.

## Tests

- **`view_arc_caches_by_cursor_and_reset_drops_the_slot`** (mirror unit):
  `None` pre-cursor; snapshot `==` owned view (S3's one-time byte-equality
  assertion); repeated calls hand out the same `Arc` (ptr_eq) with the
  counter flat; a delta misses exactly once; the post-delta snapshot again
  `==` the owned view; **and the same-version repair regression**: with
  the slot deliberately poisoned (tampered bytes, key at the cursor),
  `force_reset` at the same version must rebuild — the tampered content
  can never be handed to a pump.
- **`stamp_only_republishes_reuse_the_snapshot_handoff`** (a): settle
  pinned to the index head (`vault_info().generation` caught by the
  frame), then 5 buffer edits with digest-climbing republishes —
  `mirror_snapshots_built_total` and `mirror_deltas_total` stay flat while
  `batches_total` climbs. The keystroke path is clone-free, measured by
  counters, not asserted by prose.
- **`each_delta_window_rebuilds_the_snapshot_exactly_once`** (b): the
  identity `snapshots == deltas + resets` asserted after every settle and
  across an idle ≥600 ms window (cadence pumps observe
  `polls_total` climbing with the counter set frozen — every wake is a
  cache hit).
- **`doctor_parity_never_waits_behind_overlay_assembly`** (c, AC-5/NFR-7):
  service pumping 64 scripted deltas while another thread loops
  `check_mirror_parity`; **worst wall 3.07 ms over 461 calls, all settled
  verdicts matched**; quiescent landing explicitly asserted matched.
  During-stream refusals are tolerated only in their honest form ("could
  not settle" — the §6.5.4 bounded catch-up declining to render a verdict
  while the writer outruns it); a mismatch or any other failure fails the
  test. The old shape (parity behind `build`) could not offer that bound.
- `graph_mirror_equivalence` 9 green (byte-equality unchanged — the
  handoff adds no behavior to the owned view), `graph_projection` 3,
  `link_occurrences` 5, service suite 9/9 × 4 consecutive runs (flakiness
  pass).

## Scale bench at bound (release, `just bench-scale` numbers)

- The delta loop now mirrors the service pump: `view_arc()` per window
  (untimed — the p99 budget guards `synchronize` alone).
- **F1 identity at bound**: `snapshots_built_total: 300` ==
  `deltas_total: 300` (resets 1 — the cold reset never saw a `view_arc`
  call in the bench pattern), `mismatches_total: 0`.
- **NFR-5 accounting printed, allocation-count × record-size, not
  RSS-guesswork**: single saved-view footprint ≈ **39.4 MiB** (50 000
  node records + 1.37 MiB heap, 146 412 edges, 149 406 occurrences +
  6.96 MiB heap, 2 991 unresolved nodes+edges, metadata 0) ⇒ handoff slot
  keeps resident **≤ 2× = 78.9 MiB** — "tens of MiB" as §4.6 predicted.
- Synchronize budget held with the handoff loop live: p50 3.50 ms,
  p99 5.24 ms, mean 3.59 ms (n=300) ≤ 15 ms (NFR-8); cold reset 186 ms;
  parity full read 160 ms.

## Exit-criteria verification

- All new tests green — ✔ (unit + (a)(b)(c), four repeated runs).
- `graph_projection.rs`-style suites green — ✔ (projection/link suites
  unmodified; equivalence unchanged).
- Pump critical section contains exactly synchronize/poison/parity/
  view_arc; `build` sits after `drop(mirror_guard)` — ✔ by reading the
  diff (`graph_service.rs:639–653`), plus the AC-5 concurrency test
  empirically pins the wait bound.
- NFR-5 accounting recorded in the scale bench output (allocation count ×
  record-size estimate printed; the 2× statement is structural: one slot,
  replaced wholesale) — ✔.
- Workspace clippy `-D warnings` green; `just gate` — **green, end to
  end**: "G0 passed (fmt + clippy + test + arch check)" after "arch check
  passed (drift + edges + layering)"; `just` aborts on the first failing
  recipe, so fmt, clippy, the full workspace test suite, fault-tests,
  lint-arch, and arch-check all succeeded in one run.

## Costs accepted (per §4.6)

- One `O(V)` view clone per delta window that reaches a pump (at bound
  measured inside the untimed handoff: 300 clones/300 windows during the
  bench run), versus per-publish-with-keystrokes today.
- One extra resident copy of the saved view at worst (39.4 MiB at bound;
  the slot is replaced, never accumulated).

## Next

S4 — projection materialization: `OccurrenceSet`, shared base/metadata
`Arc`s, and the single-clone `effective` construction measured against
this slice's honest baseline (materialization p99 25 ms → ≤ 15 ms, AC-6).
