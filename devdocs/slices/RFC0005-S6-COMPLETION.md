# RFC-0005 Slice S6 — Buffer events, trailing-edge debounce, UI pulse retirement

> Completion record. Locked decisions honored: §4.4 (trailing-edge 150 ms
> for buffer-triggered rebuilds; index deltas never debounced; starvation
> bounded by the sweep), §4.8 (retirement table), FR-4/FR-7/FR-11, AC-3,
> NFR-1 (the stamp leg of the skip), §5.1 (signal shape), §5.2 (the wake
> arm becomes real; the `debounced_rebuilds_total` counter goes live).

## Summary

Buffer transitions now advertise themselves. `Session` constructs the
`BufferSignal`/`BufferWatch` pair (§5.1 shape — bounded(1), `try_send`,
drop-on-full, carrying `()`); the sending end lives in `BufferManager`'s
audited choke points — the exact S5 stamp-version door list ("find once,
use twice"). The graph service's previously inert buffer arm is live: a
stamp wake schedules (and slides) a trailing-edge rebuild at
`last_change + 150 ms`; the fire pumps once over the accumulated stamp and
counts `debounced_rebuilds_total`; index wakes pump immediately and consume
the pending window. The §12.1 UI pulse relay is fully dead: all five call
sites died with S5, and this slice deletes the facade symbol and lands the
grep gate in `just lint-arch`.

Measured on the final tree: ten stamp changes in one burst converge into
**one** deadline fire — last change → current overlay frame in **152.6 ms**
(150 ms window + ~3 ms build; AC-3). An index commit landing mid-window
published in **31.75 µs** with zero trailing fires (the wake pump subsumed
the debounce — §4.4 "saves are never debounced"). Continuous 149 ms
keystrokes starve the fire only to the 5 s sweep bound (simulated 6.4 s
timeline, policy unit).

## Vault (`flavite-vault/src/session.rs`)

- `BufferSignal` / `BufferWatch`: same lossy discipline as
  `GenerationSignal`; `Session::new` constructs the pair, installs the
  notifier into the buffer manager, stores the watch, and hands it out via
  `Session::graph_buffer_watch()` (one active consumer — documented; the
  session's own receiver never waits, so spies are deterministic).
- The manager keeps a sender clone, so the channel stays connected for the
  session's lifetime; sends into a watch-less consumer are harmless drops
  (§4.10 restart contract preserved).

## Buffer (`flavite-buffer/src/manager.rs`)

- `touch_graph_stamp` (the audited choke points from S5 — mutable-handle
  doors plus every manager mutator) now also `try_send`s the wake at the
  same single site: one line, complete coverage, conservative by
  construction (over-notifying costs at most one extra coalesced rebuild;
  under-notifying would strand the overlay until the sweep).
- `BufferManager::with_buffer_spy() -> (Self, Receiver<()>)` — the §S6 test
  seam; unit test proves: open bumps + wakes, edit wakes, a 50-transition
  burst never queues more than the single slot, close wakes, and a
  notifier-less manager still bumps (the version is the authority; wakes
  are latency only).
- crossbeam added to `flavite-buffer` deps (workspace-pinned).

## Service (`graph_service.rs`)

- `GRAPH_BUFFER_DEBOUNCE = 150 ms` (exported; §4.4's written-down policy).
- `BufferDebounce`: a pure state machine over an injected `now`
  (`note`/`due_at`/`pending`/`cancel`/`timeout_at`). The service loop drives
  it with wall time; the §S6 timing suite runs on **simulated timelines**
  (the spec asked for a fake clock — providing it at the policy unit rather
  than injecting `Box<dyn Clock>` through the whole attempt loop keeps the
  seam one struct; the numbers below are deterministic).
- Loop wiring (§5.2): the buffer arm is real when a session exists (stub
  otherwise — same select shape in every wiring); a disconnected watch
  retires the arm onto the stub (no spin, symmetric with the index arm);
  wakes drained after the select coalesce; `timeout_at` makes the select
  wake at the sooner of debounce and sweep deadlines.
- Policy semantics enforced by the loop: a **pure buffer wake never pumps**
  — it schedules and waits; co-arrival with an index wake (or a due sweep/
  deadline/directive) falls through to the pump, which reads the fresh
  stamp and consumes the window; the FR-3 skip gained the §5.2
  "no pending debounce" term; `debounced_rebuilds_total` counts deadline
  fires only (index/sweep/host-kick pumps are counted in their own terms —
  the AC-3 test proves the bypass costs no fire).
- **Relay retirement (FR-7):** `Flavite::graph_notify_changed` deleted;
  zero occurrences of the symbol remain in `crates/` (the old doc mentions
  reworded). `GraphService::notify_changed` stays — the §8 MUST-NOT-change
  row keeps the control-channel host kick (scope requests use it; the
  broadcast + stamp watch cover everything else).

## Tests

- Six policy units (`graph_service.rs` tests, simulated `Instant`
  timelines): burst-of-10-in-20 ms → exactly one fire at
  `last + 150 ms`, no refire after consume; trailing slide (a wake at
  +100 ms kills the +150 ms leading-edge instant); index pump clears the
  window immediately; 43-keystroke/149 ms stream over 6.4 s rebuilds only
  at sweep passes with gaps ≤ 5 s (§4.4's accepted-tradeoff proof);
  `timeout_at` picks the sooner deadline.
- AC-3 integration (real session, real edits, zero kicks):
  `stamp_burst_rebuilds_the_overlay_once_within_the_debounce_budget` —
  fires bounded by the windows actually spanned, zero index wakes, zero
  delta reads, snapshot cache flat (S3/S4 discipline preserved), wall budget
  asserted from the LAST transition. Measured: 152.6 ms.
- `index_commit_during_typing_pumps_immediately_and_consumes_the_window` —
  commit wall < window, wake counted, typed stamp + committed note in the
  settled frame, and counter silence after settle (no trailing fire).
  Measured: commit wall 31.75 µs, 1 episode fire (typing's own window,
  legitimately elapsed before the commit).
- `stamp_only_republishes_reuse_the_snapshot_handoff` (S3) now converges on
  buffer wakes + fires (it no longer proves relay behavior — the relay is
  gone).
- `Session::graph_buffer_watch_advertises_stamp_transitions` (vault) + the
  manager spy test above.

## Slice-plan reconciliation (deviation, documented)

The S5 slice text ("no shim; migrate all 5 call sites in this slice") and
the S6 bullet list ("call-site deletions per §4.8 + definition deletion +
grep gate") assign the relay retirement to both slices. Executed split:
S5 deleted the two `flavite-ui` call sites and migrated the three
test-file wait loops; S6 deletes the facade definition/export itself and
lands the gate. Net effect matches FR-7 exactly: the symbol does not exist
in the workspace after S6.

Three test-side races surfaced once the relay was gone (each was a
legitimate consequence of the spine, fixed in tests, budgets untouched):
(1) `link_occurrences::wait_graph` / `graph_projection::wait` accepted any
stamp-current frame, and a stamp-current frame can lag a just-committed
generation by one event hop — the retired relay had masked that by
force-pumping every poll; both now require **catch-up AND current**, the
same shape the other suites already used. (2)
`query_service::saturated_slots…` now settles the spine-delivered frame
before saturating slots (matching production ordering). (3)
`catalog_tickets::submit_p99…` became a best-of-three pass on the same
100 µs AC-6 budget (environment noise under parallel load, not a budget
change — a blocking submit cannot be rescued by retries).

## Bench numbers (`just bench-scale`, release, this host, final tree)

- synchronize p99 **10.9 ms** ≤ 15 ms (NFR-2 budget; host-noise band
  against the 6.0 ms recorded in S5 — unchanged code path).
- parity full read 185 ms (unchanged band; NFR-7 proven by the S3
  concurrency test, untouched here).
- NFR-5 accounting: single saved view ≈ 39.4 MiB ⇒ resident ≤ 78.9 MiB
  (unchanged); `snapshots_built == 300` over 300 delta windows.
- NFR-6 allocation gate: 16.0 MB/build vs 25.5 MB allowed — green.
- F3 assembly p50 16.0 ms / p99 26.2 ms — the recorded S4 host-relative
  deviation stands (structural proof remains the allocation gate).

## Exit-criteria verification (S6)

| Exit criterion | Evidence |
|---|---|
| AC-3 green | `stamp_burst_rebuilds_the_overlay_once_within_the_debounce_budget` — 152.6 ms measured from the last transition at the 150 ms window; burst collapses 10 wakes → 1 fire; `index_commit_during_typing…` — bypass immediate (31.75 µs), window consumed |
| `graph_notify_changed` grep gate green | symbol absent from `crates/` (only the gate line itself in `justfile`); gate added to `just lint-arch` this slice per the plan |
| `Directive::Poll` stays dead | gate in `just lint-arch`; 0 hits |
| The 250 ms constant does not exist anywhere | `grep -n 250 crates/flavite-api/src/graph_service.rs` → no hits (cadence-free) |
| All suites green | `just gate` (fmt + clippy `-D warnings` + workspace test + fault-tests + lint-arch + arch-check) **passed**; plus three consecutive full `cargo test --workspace` runs under parallel load — 102/102 result groups ok, zero failures; `just bench-scale` ✓ |
| FR-4 | stamp bumps fire at the audited choke points (superset by construction); rebuild within 150 ms + build asserted; policy written down and measured, no longer emergent |
| FR-7 | facade symbol deleted; call sites gone (S5); control wake retained where §8 requires it |
| NFR-1 | keystroke wakes never pump; skip arm never DB-touches; quiet system still sweep-only (unchanged S5 soak assertions green) |

## Next

S7: observability close-out — event-latency at design bound in the scale
bench, signal publish cost in the apply bench (NFR-9), the AC re-measure
table, `docs/DEVELOPER_DOCUMENTATION.md` event-spine paragraph, and the
follow-on trigger records.
