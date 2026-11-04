# RFC-0004 Slice S5 — Ticketed catalog acquisition

> Completion record for W-D (D9). Locked decisions: reserved internal
> `CATALOG_CONTEXT_TAB`; sync `note_catalog` retained for CLI/tests with an
> explicit blocking contract + grep gate; UI triggers = {startup,
> vault-open, `VaultChanged` throttled ≥ 1 s, explicit retry}.

## Summary

The 50k-row catalog read left the host turn. The UI acquires the catalog
exclusively through the bounded QueryService (purpose `Catalog`), applying
`Arc`-shared, provenance-stamped snapshots wholesale in the pump drain;
the sync facade path remains for CLI/tests, gated out of `flavite-ui/src`.
First-open behavior moved from "frame blocks for the read" to "one honest
loading state" — strictly better UX, and (as the RFC predicted) a visible
timing change that required updating four pre-existing tests to the new
contract (they now wait for convergence instead of assuming same-turn
data).

## Facade / service (flavite-api)

- `QueryPurpose::Catalog`, `QueryPayload::Catalog { limit }` (service
  clamps 1..=50_000), `ContextQueryResult::Catalog { entries:
  Arc<[CatalogEntry]>, provenance, total, truncated }` — `CatalogEntry` is
  a re-export of the single `queries::NoteCatalogEntry` definition (the
  spec's "re-export not duplication" resolution).
- `execute_catalog` runs `session.note_catalog` on a query worker —
  blocking there is by design; Busy/backpressure semantics identical to
  Search.
- `Flavite::submit_catalog()` → reserved `CATALOG_CONTEXT_TAB = u128::MAX`
  (no workspace view can own that id; supersession stays catalog-only),
  internal monotonic `catalog_sequence` as binding sequence.
- `Flavite::note_catalog` retained unchanged in shape; doc contract added:
  "Blocks the calling thread … MUST NOT be called from a UI host turn."

## Host (flavite-ui)

- `CatalogStatus { Empty, Loading, Ready { epoch, generation, truncated },
  Failed { message } }` + `CATALOG_REFRESH_THROTTLE = 1 s`;
  `load_catalog` is gone: triggers call `request_catalog_refresh` /
  `note_vault_changed_pulse` (in-window pulses coalesce into exactly one
  `catalog_retry_tick` retry); `apply_catalog_outcome` swaps the snapshot
  wholesale. `Loading`/`Failed` never clear the ready snapshot (no flicker);
  `catalog_error()` (existing visible-text surface) now derives from the
  status machine.
- Picker and Notes render "Indexing notes…" over the existing empty-state
  pattern; the picker gained a provenance line in the established
  "index {epoch}/{generation}" style with the truncation flag surfaced
  (R9). Harness semantic snapshots carry `catalog_status` for evidence.
- **Attach-ordering fix discovered by this slice:** `restore_workspace_layout`
  validates membership via `plan_restore(&decoded, &catalog)`. With the
  catalog ticketed, restore now defers to the first terminal acquisition
  outcome (`restore_pending` → `run_pending_restore`), so restores keep
  exact membership semantics without ever blocking the turn on the read.

## Gate (justfile)

`lint-arch` gained the RFC-0004 debt greps (now enforced in `just gate`):
`\.note_catalog(` banned under `crates/flavite-ui/src` (AC-6's static half),
plus the S1/S2 statement-shape gates (`INSERT INTO changes(` in
`flavite-index/src`, full-table `UPDATE notes SET id_conflict=CASE` in
`pool.rs`). Patterns match statement forms so contract docs may still name
the forbidden shapes.

## Tests

- `crates/flavite-api/tests/catalog_tickets.rs` — 5 green: ticket roundtrip
  equals the sync path exactly (entries/provenance/total equality);
  supersession keeps only the newest reserved-context ticket; 64-slot
  saturation yields typed Busy, releases on drain, all 64 pages served;
  truncation flag honest at limits 2 and 0 (clamps to 1); **AC-6: 200-sample
  `submit_catalog` p99 < 100 µs asserted in the test itself**.
- `crates/flavite-ui/tests/catalog_acquisition.rs` — 2 green: startup →
  Ready with provenance (epoch 1, generation ≥ 3); explicit refresh lands
  `Loading` while the snapshot keeps serving (no-flicker), converges;
  late note converges to a 4-entry page.
- `crates/flavite-ui/src/app.rs` units — 3 green: state machine (Ready
  swaps wholesale, Failed/unexpected keep snapshot + visible text,
  truncation flag propagates — R9); inert triggers without attachment;
  throttle coalesces pulses into exactly one retry and the tick respects
  the window.
- Pre-existing tests updated to the ticketed contract (visible-change
  allowance of D9): `knowledge_workflow` (create → candidates converge),
  `link_target` (picker catalog carries the target before insert),
  `shared_views` (catalog arrives under pumping), `workspace_restore`
  (debug-era synchronous panic replaced by the established wait pattern).

## Recorded visual run (exit-criterion requirement)

`just workspace-visual semantic-link` (real X11 window, GPU render backend,
OS input through the actual picker): all checks `pass` —
`boot-single-group-catalog`, `semantic-link-inserted-at-caret`,
`single-undo-removes-link`, `redo-restores-link`. Evidence:
`target/workspace-evidence/semantic-link-1793771176-2342091/` (manifest +
screenshots + `harness.jsonl`, whose snapshots record
`catalog_status="ready:1/3"` with 3 entries).

## Exit criteria verification (S5)

- AC-6: ticket latency asserted (< 100 µs p99, debug build) + UI grep gate
  wired into `just gate` and green.
- Picker shows honest states (UI tests + recorded workspace-visual run above).
- `cargo test -p flavite-api --test catalog_tickets`,
  `cargo test -p flavite-ui` (21/21 binaries), `cargo clippy -p
  flavite-api -p flavite-ui -p flavite-vault --all-targets -- -D warnings`
  green; full `just gate` green (below).
- MUST NOT-change surfaces verified: slot/supersession machinery untouched
  (only enum arms added), `NoteCatalog`/`MAX_NOTE_CATALOG_LIMIT` shapes
  unchanged, picker nucleo matching untouched, CLI sync path unchanged.

## Next

S6 (incremental graph bridge) consumes the S1 typed feed; S7 parallelizes
the reconcile scan; S8 closes observability and re-measures thresholds.
