# RFC-0004 Slice S3 — Backstop policy, doctor parity, drift fixtures

> Completion record for W-A/S2's operational safety net (D13 complete).
> Locked decisions: doctor checks are read-only (never auto-repair; repair =
> explicit `vault.reconcile {mode:"verify"}`); parity = full comparison,
> no sampling heuristics.

## Summary

After S3 a hypothetical targeted-resolver bug is *detected*, never silent:

- `resolver-parity` — a read-only, full recomputation of the from-scratch
  resolution projection over every stored link, diffed against stored
  state. Zero writes: `re_resolve_all`'s walk was decomposed into a shared
  fetch/project core so the preview runs the identical comparison the
  committed walk uses, without an UPDATE anywhere.
- `change-log-health` — `(log_floor, generation]` window sanity, row
  placement relative to the window, and the `MIN_LOG_RETENTION` contract,
  all read in one atomic read transaction via the new typed
  `change_log_health()` primitive. A below-contract retention surfaces as
  a warning (the §4.3 budget-forced escape must never be silent); a
  nonsensical window or stranded rows surface as an error.
- Both checks surface the §6.7 counters. The vault actor now drains
  `IndexActor::resolver_diagnostics()` to meta at every reconcile boundary
  (the established `runtime_counters` pattern), so the standalone doctor
  sees `generations_since_full`, `resolve_full_total`,
  `resolve_targeted_total`, `affected_links_last`,
  `id_conflict_group_updates_total`, `apply_generations_total`, and
  `changes_rows_total` without a live actor.
- Full-verify jobs force the backstop re-resolve (D13's explicit trigger):
  `ReconcileMode::VerifyContent` sets the demand at enqueue time, and the
  apply spine consumes it — see "repair semantics" below.

## Repair semantics (the S2↔S3 seam, refined)

`VerifyContent` re-saves unchanged content (the scan's skip path is
`FastMetadata`-only), so the forced demand would have starved on a
content-clean vault with drifted link state — exactly the case repair
exists for. The spine therefore lets a forced demand ride even the
content-hash fast path:

- drift found ⇒ the transaction upgrades to a full structural apply:
  generation bump + entity row + one `outgoing(src)` row per repaired
  source (FR-1 holds on repair generations);
- clean ⇒ the demand is consumed, the freshly built catalog is still
  installed (the base stays current), and **no generation is invented**.

Cadence resets from any forced walk (`generations_since_full → 0`), and a
rolled-back ride leaves the demand standing (verification still owed).

## Files

| File | Change |
|---|---|
| `crates/flavite-index/src/resolver.rs` | `StoredLink`/`fetch_all_links`/`project_link` shared by the walk and the preview; `LinkParityMismatch`; `re_resolve_preview` (read-only, full comparison); `ResolverState::force_backstop` + `backstop_due` honoring it; `commit_backstop` consumes the demand; `note_committed_apply`; diagnostics gains `apply_generations_total`/`changes_rows_total` |
| `crates/flavite-index/src/changelog.rs` | `ChangeLogHealth` + `change_log_health()` — one atomic read tx: meta triple, retention window, below-floor/ahead-of-generation row placement; `is_corrupt`/`retention_violated` verdicts (saturating math — floor-above-generation is representable, not a panic) |
| `crates/flavite-index/src/pool.rs` | spine: `backstop_walk` free fn remains the single `re_resolve_all` call site; forced-demand fast-path ride (repair apply vs clean consume); all five paths unchanged otherwise |
| `crates/flavite-index/src/schema.rs` | 7 `resolver.*` counter meta keys (KV rows — no DDL, no version bump; `create_schema` seeds zeros) |
| `crates/flavite-vault/src/actor.rs` | `persist_runtime_meta` drains `resolver_diagnostics()`; `enqueue_reconcile` forces the backstop for `VerifyContent` jobs; new actor test proves the wiring end-to-end |
| `crates/flavite-api/src/doctor.rs` | `resolver_parity_check` + `change_log_health_check` wired into `run()` (both branches, cache-absent placeholders included); counter-detail helpers; 3 new doctor tests |
| `crates/flavite-index/tests/drift_fixture.rs` | **new** — healthy passes; exactly-one `res_state` drift reported (and destination drift); floor/window violations; forced-backstop firing/consumption; drift repair on unchanged content incl. logged repair generation |

## Exit criteria verification (S3)

- `cargo test -p flavite-index --test drift_fixture` — 6 passed.
- `cargo test -p flavite-api -- doctor` — 12 passed (3 new: healthy-pass,
  seeded `res_state` drift flips `resolver-parity` with exactly-one detail,
  seeded `log_floor` corruptions flip `change-log-health` warn+error).
- `cargo test -p flavite-vault --lib -- verify_reconcile` — 1 passed
  (full-verify forces ≥1 backstop walk; structure verified).
- `cargo test -p flavite-api` — green; `cargo test -p flavite-index` —
  green (51 lib + all integration suites); clippy
  `--workspace --all-targets -D warnings` green on the final tree.
- Grep gate: `grep -n "resolver_diagnostics" crates/flavite-api/src/doctor.rs`
  → present (surfacing doc on the parity check).
- S2 gates re-verified unchanged: exactly one `re_resolve_all` call in
  `pool.rs` (inside `backstop_walk`, spine-only), zero full-table
  `id_conflict` statements, S1 raw-INSERT gate intact.
- AC-10 first half: doctor checks pass on healthy fixture vaults; seeded
  drift flips each to a finding. (`graph-mirror-parity` is the third check,
  landing with S6.)
- `just gate` — green (see below).

## Behavior notes

- Doctor findings are **read-only**: fix strings name the explicit repair
  (`vault.reconcile {mode: "verify"}` or cache rebuild); nothing in the
  doctor path mutates SQLite.
- The repair ride emits an ordinary generation: feed consumers (graph
  mirror, tantivy) see the outgoing flips exactly like any apply — the
  drift event is not a side channel.
- `resolver-parity`'s preview shares `StoredLink`/`project_link` with the
  committed walk: the comparison in the doctor is byte-identical logic to
  the repair, so "the check would pass" and "the walk changes nothing"
  are the same statement by construction.

## Next

S6's graph mirror reuses `ChangeLogReader` + this slice's forcing seam
(parity failure will call `force_backstop` too); the remaining §6.7
observability (turn-budget guard, `bench-scale`, final grep gates) lands
in S8.
