# RFC-0005 Slice S5 — The event loop: select, sweep, watermark skip, long-lived connection

> Completion record. Locked decisions honored: §4.1 (three independent
> legs — events, sweep, parity; the channel is never a correctness
> dependency), §4.2 (bounded(1) latest-wins, already S1), §4.5 (one
> revalidated read connection per attempt), §4.9 (watermark as advisory
> lower bound; epoch mismatch never skips; recreate owns rewind cursors),
> §4.10 (restart policy untouched; receivers per attempt, senders survive),
> §5.2 (the loop shape and the counter set), FR-2/3/5/6/11, NFR-1.

## Summary

The graph service stops polling and starts chasing commits. The pump now
waits in one `crossbeam::select!` over four sources — the writer-side
`GenerationWatch` receiver, the buffer-stamp channel (an inert, never-sending
stub by S5 decision: S6 adds the trailing-edge 150 ms policy on top of this
plumbing), the control channel (`Directive::Relayout` as a pure wake), and
stop — with the sweep deadline as the select's `default`. `Directive::Poll`
and `GRAPH_POLL_INTERVAL` are gone; `GRAPH_SWEEP_INTERVAL = 5 s` is recovery,
not the mechanism.

Measured on the final tree: an index commit reaches a published frame in
**11.75 ms** at a 1k-note vault (budget: 100 ms, AC-2); a burst of 300
coalesced commits produced **65 wakes / 112 delta windows** (≈4.6:1 wake
coalescing, §4.2); an external commit with no events at all converged in
**4.96 s** via the self-verifying sweep (§4.1 leg 2, NFR-1's 0.2 Hz); a
watermark-equal wake skipped with **zero** DB work, zero mirror lock, zero
republish (FR-3); a DB recreate reopened the long-lived connection and took
the contractual reset path to the correct frame (FR-6/AC-8).

## Service (`graph_service.rs`)

- **Wake loop per §5.2.** One select over `index_rx | buffer_rx(stub) |
  hint_rx | stop_rx | default(sweep deadline)`; coalesced leftovers drained
  after the select (latest-wins). A disconnected index watch (host dropped
  the pool) retires the arm onto a never-filled stub instead of spinning.
  Watch-less services use the same select shape with the stub — no special
  case in the loop.
- **FR-3 watermark skip.** A pure index wake skips the entire pump when
  `watch.watermark() == mirror cursor` **and** the mirror is healthy **and**
  the session stamp version is unchanged since the last pump. No DB open, no
  DB read, no mirror lock: the comparison reads a lock-free cursor shadow
  (`Shared::mirror_cursor_epoch/_generation/_ready`), written only by the
  mutex holder at the end of every critical section that can move the cursor
  (pump and `check_mirror_parity` — §4.11's "written by the mutex holder,
  replaced wholesale" discipline extended to the cursor itself). The skip
  compares the *latest advertised* watermark, not the delivered message —
  a coalesced/dropped message can never cause a false skip of a newer
  advertised generation. Any epoch mismatch never skips (§4.9).
- **Sweep (FR-5).** 5 s timeout wake; verifies against the database
  (cursor read inside `synchronize`), never the channel; recovery for
  external writers and dropped events. First deadline is immediate, so a
  cold service bootstraps like the old t=0 poll.
- **Long-lived connection (FR-6, `MaybeConnection`).** One read-only handle
  per service attempt, kept across pumps, revalidated on every pump that
  touches the DB: `meta.vault_uuid` **and** the file's on-disk identity
  (inode on unix — a recreated database would otherwise be read as a ghost
  through the stale fd, matching its own stale uuid forever). Mismatch or
  any read error drops and reopens; `connection_reopens_total` counts
  reopens (the first connect is not a reopen). The vault uuid is written
  once at actor startup (`VaultActor::run` → `IndexActor::ensure_vault_uuid`,
  meta only — no generation bump, no signal).
- **Buffer stub.** The select arm exists and is real plumbing; the sender
  is held attempt-local and never sends (S6's `BufferSignal` + debounce
  becomes policy on top of this arm). The FR-3 stamp leg already reads the
  session-side version atomic, so S6's wake path needs no new comparison
  logic.
- **Counters per §5.2.** `polls_total` retired (breaking metrics-surface
  change, called out in the RFC blast radius): added `wakes_total`,
  `sweeps_total`, `watermark_skips_total`, `connection_reopens_total`,
  `debounced_rebuilds_total` (field + reporting wired now; counted from
  S6's deadline fires — the S5 arm never fires). Doctor's
  `graph-mirror-parity` details line extended with all five. UI harness
  JSON keys: `polls` → `wakes`/`sweeps`/`watermarkSkips`.
- **Directives.** `Directive::Poll` deleted; `Relayout` is a wake *only* —
  the layout reinstall rides on the `retry_layout` flag, which
  `GraphService::relayout` sets before sending. (The in-progress tree had
  the handler re-storing the flag, which would have reseeded the layout
  worker on every scope change and every test kick — fixed.) `Panic` stays
  behind `#[cfg(test)]` on the control channel.
- **Restart/status semantics unchanged (§4.10):** catch_unwind, backoff,
  three-per-60 s then explicit retry; receivers built per attempt from the
  `Arc`ed watch; senders survive restarts; `Indexing`/`Live`/`Failed`
  untouched.

## Vault (`flavite-vault`, `flavite-buffer`)

- **Stamp version (FR-3's stamp leg).** `BufferManager` owns an
  `Arc<AtomicU64>` bumped at every choke point that can change
  `graph_buffer_stamp()` output: the mutable-handle doors
  (`get_buffer_mut`, `get_buffer_by_id_mut` — the audit finding is that
  *every* workspace mutation of a `Buffer` enters through these), plus
  `open_note`, `close_note`, `apply_edit`, `notify_external_change`,
  `notify_file_deleted`, `notify_file_recreated`, `rename_buffer`,
  `notify_save_completed`, `clear`. Over-bumping is conservative (one extra
  pump at worst); the door-hoisting makes the set small enough to audit and
  is the exact site list S6's `BufferSignal` fires from ("find once, use
  twice"). `Session` captures the handle at construction and exposes
  `graph_stamp_version()` — an atomic load, no manager lock, no actor hop
  (§5.2 decision).
- `vault_uuid` seeding in `VaultActor::run` (FR-6's comparison value),
  logged honestly on failure.

## Index / changelog / feed (deviations — see below)

- `GenerationWatch::receiver()` accessor (already staged in S1's file) lets
  the watch multiplex in one select; the watermark atomics remain the
  authority for the *latest* advertised version.
- `ResetReason::Recreated` (additive enum variant): same-epoch cursor ahead
  of the feed head — the DB-recreate-under-a-live-consumer shape §4.9 names.
  `ChangeLogReader::read_since` itself is untouched (its contract stays
  "future cursor is an error, never a silent success").
- `GraphMirror::synchronize` maps that shape to `Synchronize::Reset` before
  reading the log — the §4.9 claim ("the existing Reset path owns it") was
  not true of the code: schema init reseeds `INITIAL_EPOCH = 1` on every
  fresh file, so a recreated database rewinds the generation at the same
  epoch, and the pump would loop on the reader error (poison never set —
  the `?` on `read_since` bypasses it) until the service died.
- `GraphPublisher::apply_current` no longer treats a **reset** batch as
  stale: a forced reset re-anchors from the consumer's full consistent read
  and is by definition the authoritative current (without this, the
  reopened-and-reset frame was dropped by the generation-rewind guard and
  the service stuck on the ghost topology).

## Wiring & relay retirement

- `Flavite::open_root` (production): `GenerationSignal::new()` → sender into
  `Vault::open_with_generation_signal`, watch through `attach_services` into
  `GraphService::start_with_index_watch`. The facade stores the watch so
  `set_graph_db_path_for_test`/`set_graph_service_for_test` rewire the same
  one (new doc(hidden) seam taking the watch for fixtures that own a
  signal).
- §4.8 call sites all retired: `app.rs` VaultChanged relay deleted (catalog
  invalidation stays; freshness rides the broadcast), `host/graph.rs`
  RetryService relay deleted (`graph_relayout()` is both wake and reinstall),
  `graph_projection.rs`, `link_occurrences.rs`, `knowledge_workflow.rs` wait
  loops poll the frame and rely on broadcast (index commits) plus the
  self-verifying sweep (stamp-only transitions — until S6's buffer wake
  restores sub-150 ms stamp convergence; deadlines accommodate the 5 s
  bound). `GraphService::notify_changed()` remains as the *host kick*
  control wake (scope requests, test forcing) — §8 keeps "wake-on-hint
  existence (control channel)"; the facade symbol
  `Flavite::graph_notify_changed` keeps its declared S6 deletion (see
  deviations).

## Tests

- `graph_mirror_service.rs` (14 green): five new S5 tests —
  `index_commit_reaches_a_published_frame_within_the_event_budget` (1k-note
  vault, timed 11.75 ms < 100 ms, poll-free wait),
  `watermark_equal_wakes_skip_all_work` (quiet window delta-flat; a re-advertised
  head wakes, skips, and changes zero counters, zero reopens, same frame Arc),
  `burst_commits_coalesces_into_windows_not_per_generation_wakes` (300 commits
  → 65 wakes, 112 windows, final frame at final generation),
  `db_recreate_forces_reopen_and_the_reset_path` (two-ghost → one-note swap;
  reopen counted, reset taken, frame carries the new topology only; also
  exercises the disconnected-watch leg),
  `sweep_recovers_external_commits_without_events` (signal-less second handle;
  convergence 4.96 s ≤ sweep + slack; wakes unchanged; sweep delta ≤ 2 —
  NFR-1's 0.2 Hz).
- `each_delta_window_rebuilds_the_snapshot_exactly_once`'s idle block rewritten
  to S5 semantics: no wakes without events, ≤ one sweep per 600 ms, snapshot
  cache flat across the idle pass.
- `query_service.rs` fixture rewired to the spine (it had depended on the
  250 ms cadence to catch a backlinks frame up — with the cadence gone it
  must, and now does, chase events instead).
- `links_fixture` rides the broadcast (actor with signal + service with
  watch) — per §4.8 these tests now prove the wiring end-to-end.

## Bench numbers (`just bench-scale`, release, this host, final tree)

- synchronize p99 **6.0 ms** ≤ 15 ms (NFR-2 budget — unchanged by S5;
  the cursor-shadow store costs nothing measurable).
- parity full read **170 ms** (unchanged band; NFR-7 worst doctor wall from
  the S3 test remains 3.07 ms across a live pump stream).
- F3 assembly p50 15.6 ms / p99 22.6 ms — the recorded host-relative S4
  deviation stands; structural gate holds: allocation profile at 5k notes /
  300k links measures 16.0 MB/build vs the 25.5 MB budget
  (`projection_materialization_allocates_only_the_effective_batch_at_scale`
  green).
- NFR-5 accounting printed: ≈39.4 MiB single saved view ⇒ ≤78.9 MiB resident
  with the handoff slot — unchanged by this slice.

## Grep gates (in `just lint-arch` now)

- `Directive::Poll` → 0 hits workspace-wide; `GRAPH_POLL_INTERVAL` → 0 hits.
- `open_with_flags` appears exactly once in `graph_service.rs` and never
  inside `run_service_attempt`'s body (awk-scoped gate).

## Deviations (documented, not hidden)

1. **Three surfaces the RFC declared untouched were touched, minimally,
   because AC-8 proved the RFC's assumption wrong.** §4.9 asserted the
   existing Reset path owns the recreate-rewind cursor; it does not —
   `open_or_init` always seeds epoch 1, so the rewind is same-epoch, the
   reader errors forever, and the publisher's stale guard then refuses the
   corrected frame. Fixes: additive `ResetReason::Recreated` (index), a
   guard in `GraphMirror::synchronize` (mirror), and `reset` now outranks
   the generation-rewind stale check (feed). The change-log contract tests
   (`future_cursor_is_an_error`) stay green — the raw reader behavior is
   unchanged; only the consumer that *proved the reopen* maps it to a
   rebuild.
2. **The facade symbol `Flavite::graph_notify_changed` still exists** with
   zero in-tree callers; its declared deletion point is S6 (§4.8 table and
   the S6 item list own "definition + grep gate"). S5's explicit decision —
   "no shim; migrate all five call sites in this slice, they're ours" — is
   honored for the call sites; the symbol and the `justfile` absence gate
   die in S6 with the buffer wake that makes them unnecessary.
3. **Stamp-only convergence runs at sweep pace inside S5's own suites**
   (`stamp_only_republishes_reuse_the_snapshot_handoff`, `graph_projection`,
   `link_occurrences`): by §5.2/S5 decision the buffer channel is an inert
   stub until S6, so those waits pass at ≤ one sweep interval each
   (deadlines accommodate; S6 restores the ≤150 ms + build policy and the
   suite speedups with it).
4. **The service seam is `start_with_parity_policy_and_watch` +
   `set_graph_service_for_test`, not the spec's literal
   `with_signals(index_watch, buffer_watch, control)`** — there is no
   `BufferWatch` type before S6; when it arrives the stub sender/receiver
   swaps in at the same arm.

## Exit-criteria verification (S5)

| Exit criterion | Evidence |
|---|---|
| AC-2 (event latency) demonstrated at test scale | `index_commit_reaches_a_published_frame_within_the_event_budget` — 11.75 ms at 1k notes (< 100 ms); bound vault = S7 bench |
| AC-4 (quiet system) | `watermark_equal_wakes_skip_all_work` (deltas/wakes/skips/reopens flat while quiet; skips increment only on a real wake), `each_delta_window_...` (≤1 sweep per 600 ms), `sweep_recovers_...` (sweep delta ≤ 2 in ~5 s) |
| AC-8 (DB recreate) | `db_recreate_forces_reopen_and_the_reset_path` |
| FR-3 lock-free skip | skip branch reads atomics only; test asserts zero pump counters and identical frame Arc |
| FR-11 / grep gates authored | `Directive::Poll`, `GRAPH_POLL_INTERVAL` absent (enforced in `just lint-arch`); `open_with_flags` confined to `MaybeConnection` (count-1 + body-scope gates, enforced) |
| All prior gates green | `just fmt` ✓, `cargo clippy --workspace --all-targets -D warnings` ✓, `cargo test --workspace` ✓, `just fault-tests` ✓, `just lint-arch` ✓, `just arch-check` ✓, `just bench-scale` (sync p99 6.0 ms ≤ 15 ms; allocation gate green) ✓ |
| Mirror/projection untouched | NOT met — see deviations 1; the mirror change is one guard, the projection files are untouched |

## Next

S6: `BufferSignal` from the audited choke points, the trailing-edge 150 ms
debounce on the stub arm, `debounced_rebuilds_total` counting, deletion of
`Flavite::graph_notify_changed` + its grep gate, AC-3 end-to-end.
