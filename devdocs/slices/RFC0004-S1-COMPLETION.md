# RFC-0004 Slice S1 — Change-log contract (the spine)

> Completion record for W-B slice S1 of RFC-0004 (Incremental Core).
> Locked decisions: D5, D6, D12 (no schema change), retention floor
> `MIN_LOG_RETENTION = 4096`, wire strings unchanged, `INSERT OR IGNORE` dedup.

## Summary

S1 turns the `changes` invalidation log into a complete, typed, contract-tested
change feed without changing any observable behavior:

- One sanctioned writer (`ChangeLogWriter`) replaces the six raw
  `INSERT INTO changes` statements scattered across the five apply paths.
- One typed reader (`ChangeLogReader::read_since`) returns `Delta(records, to)`
  or `Reset(reason)` — never the empty-success-on-epoch-mismatch trap.
- Every re-resolution flip now emits an `outgoing(src)` row in the same
  transaction and generation (the C4 gap, closed and contract-tested).
- The dead `InvalidationFeed`/`ChangeBatch` API (C6) is deleted outright —
  no compat layer, no re-export, no shim.
- Tantivy's `replay_since` is the first consumer migrated to the typed
  reader; its bespoke cursor checks are deleted (a `Reset` maps to the
  established full-rebuild semantics of §4.14).

After S1 the log fully describes every resolution flip even though
resolution is still O(links) — correctness of the contract landed before
the speedup, exactly as §6.1.2 prescribes.

## Files

| File | Change |
|---|---|
| `crates/flavite-index/src/changelog.rs` | **new** — `ChangeKind` (+`TryFrom<&str>`), `ChangeRecord`, `ChangeLogWriter::{record, record_all}`, `ChangeLogReader::{new, read_since, current_cursor}`, `ChangeLogRead`, `ResetReason`, `MIN_LOG_RETENTION`; contract lives in the module header |
| `crates/flavite-index/src/change.rs` | `InvalidationFeed`/`ChangeBatch` + their tests deleted (D6); retained as the retention side of §4.3: `DEFAULT_LOG_BUDGET`, free functions `prune` (clamps retention to `MIN_LOG_RETENTION`, floor never moves backwards, one tx) and `needs_prune` (estimate unchanged) |
| `crates/flavite-index/src/types.rs` | `ChangeKind`/`ChangeRecord` definitions moved out to `changelog.rs` (§6.1.6, shapes unchanged); `IndexVersion` stays (generic cursor, not changelog-specific — §6.1.6 permits either) |
| `crates/flavite-index/src/lib.rs` | `pub mod changelog;` + `pub use changelog::*;`; `pub use change::{ChangeBatch, InvalidationFeed};` removed (crate-root export of `ChangeKind`/`ChangeRecord` is unchanged) |
| `crates/flavite-index/src/pool.rs` | five apply paths: raw INSERTs → `ChangeLogWriter::record`; temporary §6.1.2 pre/post `snapshot_link_states` diff around `re_resolve_all` → `ChangeLogWriter::record_all(outgoing srcs)`; `IndexActor::new` bespoke `log_floor` check deleted (typed `Reset{BelowLogFloor}` owns it) |
| `crates/flavite-index/src/search.rs` | `replay_since(IndexVersion)` via `ChangeLogReader`; `Reset` → `rebuild_from_sqlite` (§4.14); records deduped per §4.6 |
| `crates/flavite-index/tests/changelog_contract.rs` | **new** — the eight S1 contract tests |

## Exit criteria verification (S1)

- `cargo test -p flavite-index --test changelog_contract` — 8 passed.
  1. epoch mismatch → `Reset{EpochChanged}` (trap is gone, asserted with rows present)
  2. floor overrun → `Reset{BelowLogFloor}`
  3. future cursor → `Err("cursor from the future…")`
  4. dedup: double-record same `(gen,kind,id)` in one tx succeeds, one row
  5. `note` + `outgoing` same id/generation coexist
  6. generation atomicity: rows + meta bump visible to a second connection only after commit
  7. delete-note flip logging: `remove_note_path(A)` with B→A link ⇒ exactly one `'outgoing'(B)` row in the new generation (C4 gap, now contract-tested)
  8. `prune` honors `MIN_LOG_RETENTION` (requested 10 clamps to 4096 ⇒ floor 904 at gen 5000; floor never regresses; below-floor cursor reads `Reset`; `needs_prune` estimate preserved)
- `grep -rn "INSERT INTO changes" crates/flavite-index/src/` → only `changelog.rs` matches (its doc line).
- `grep -rn "InvalidationFeed\|ChangeBatch" crates/` → zero matches.
- `cargo clippy -p flavite-index --all-targets -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- `RUSTDOCFLAGS="-D warnings" cargo doc -p flavite-index` — clean.
- `cargo test -p flavite-index` (51 unit + all existing integration suites) — green.
- `cargo test --workspace` + `just gate` (fmt, clippy, test, fault-tests, lint-arch, arch-check) — green.

## Audits & judgment calls

- **prune caller audit** (grep `prune(` across the workspace): no production
  call site exists today — the only historical caller was `InvalidationFeed`,
  deleted by D6. The retention clamp therefore lives inside the free
  `prune` helper so no future caller can request sub-contract retention.
  The §4.3 budget-forced escape hatch (floor above the retention) is
  documented in `change.rs` and must be wired with its counter by whichever
  slice introduces a budget-driven caller; `needs_prune` keeps the
  conservative 32-byte/row estimate unchanged.
- **Duplicate `outgoing` rows are collapsed by the PK** (`INSERT OR IGNORE`),
  per §4.6: when the flipped source *is* the applied note, its already-written
  `'outgoing'` row and the diff-derived row are one row. Consumers still
  dedupe `(kind, entity_id)` per generation (see the migrated `replay_since`).
- **Wire strings unchanged**: `note|attachment|outgoing` remain the schema
  CHECK values; the reader maps unknown kinds to `Err` (corruption surfaced,
  never guessed).
- **Reader atomicity**: `read_since` reads the meta triple and the row range
  inside one `unchecked_transaction()` on the consumer's connection (WAL), so
  a torn view is structurally impossible; `current_cursor` is likewise atomic.
- **Test fixtures 4–8 seed `changes` with direct SQL** inside the test crate
  (never `src/`): seeding state-under-test is the sanctioned exception, and
  the S1 grep gate is scoped to `crates/flavite-index/src/`.
- **`MIN_LOG_BUDGET` deleted** with the feed: its only use was the budget
  clamp of `InvalidationFeed::with_budget`; no surviving caller.

## Cost accepted (removed by S2)

Each apply transaction now performs two extra full `links` state reads
(pre/post diff) alongside the existing `re_resolve_all` walk. This is the
deliberate one-slice cost of §6.1.2: the contract (FR-1) lands complete and
trustworthy before the O(change) speedup deletes the diff machinery.

## Next

S2 (targeted resolution) replaces the walk + diff with `resolve_affected`
and the `K(pre) ∪ K(post) ∪ fresh outgoing keys` candidate set; S6 makes the
graph mirror the second typed consumer of `ChangeLogReader`.
