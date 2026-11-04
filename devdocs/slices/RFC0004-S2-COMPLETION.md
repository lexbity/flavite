# RFC-0004 Slice S2 — Targeted resolution (the O(change) apply path)

> Completion record for W-A slice S2. Locked decisions: D1–D4, D13 (backstop
> list fixed); IN-list chunk size 900; overlay drop-on-rollback; base rebuild
> triggers exactly {epoch change, DB recreate, 4096 generations, doctor,
> parity failure — the last two wired in S3}.

## Summary

Apply transactions now re-resolve only affected links against a persistent
catalog + per-transaction overlay. `re_resolve_all` left the apply paths;
`id_conflict` maintenance became group-scoped; the five divergent apply
bodies delegate their shared pre/mutate/post/resolve/log/commit/fold
sequence to one `apply_entity_core` spine. The S1 pre/post O(links)
diff was deleted — changed sources now arrive from the targeted pass
itself. The equality property test landed in this slice and is permanent.

## Soundness discovery (D12 clause exercised)

§6.2.3's `target_key IN K(E)` fetch is **not** sound as written: the
catalog's stem-fallback branch (`note_stems[stem(raw)]`) lets a link
target an entity from any directory/extension spelling
(`[[folder/idea]]`, `[[sub/idea.png]]` → stem `idea`), and those stored
`target_key` values are not enumerable from any finite identity-image.
The S2 pool test `resolver_persists_ambiguous_winner_and_handles_folded_ids_and_suffixes`
(target `folder/idea`) is exactly this family.

Per D12's discovery clause ("`SCHEMA_MINOR` bump solely if implementation
discovers a needed index"), schema **3.1** adds:

- `links.target_stem TEXT NOT NULL DEFAULT ''` — the final-component stem
  of `target_key`, written by the link insert loop through the same
  `target_stem_key` helper `resolve_at_path` uses to compute its stem
  consult (one implementation, no drift);
- `CREATE INDEX links_target_stem ON links(target_stem)`.

The candidate fetch becomes two indexed arms: `target_key IN exact-K`
(`links_target`) ∪ `target_stem IN stem-K` (`links_target_stem`), where
stem-K is the entity's stem + alias-key surface. Superset-sound by
construction (FR-3), O(change) by fetch (both arms index-seek). Upgrade
path: `open_or_init`'s schema probe now requires `links.target_stem`;
older caches take the existing rebuild-from-scratch branch (the same
disposable-cache mechanism as every prior major change; no in-place ALTER
— the vault re-ingests through the normal reconcile path).

`K(exact)` was also widened beyond the §6.2.2 sketch with the catalog's
own consult inverses: every path/suffix form minus a trailing `.md`
(`folder/idea` for `folder/idea.md`), and `.md` forms as stored.

## Files

| File | Change |
|---|---|
| `crates/flavite-index/src/resolver.rs` | Backstop-policy header; `NoteImage`/`AttImage`; `note_identity_keys`/`attachment_identity_keys` (+ widened path forms); `target_stem_key`; `AffectedKeys`; `identity_relevant` (D4); `ResolverOverlay` (shadowed ids + sparse catalog + authoritative `stable_winners`); `ResolverState` (base, overlay, cadence, counters, `commit_backstop`/`note_targeted_pass`/`fold_overlay`/`backstop_due`); `resolve_affected` two-arm indexed fetch, conditional batch UPDATEs; `refresh_id_conflict_groups` (group-scoped §6.2.6); `group_winner_in_tx`; `re_resolve_all` rewritten to conditional writes returning flipped sources, doc = backstop-only; `take_catalog`; `projected_resolution` shared by write paths |
| `crates/flavite-index/src/catalog.rs` | `empty()`; `upsert_note`/`upsert_attachment`/`upsert_alias`; `invalidate_note`→`remove_note`, `invalidate_attachment`→`remove_attachment` (rename for accuracy, semantics unchanged); `fold_overlay`; D8 body moved into `resolve_at_path_with_overlay` reading every index through base-minus-shadowed-plus-overlay merge views (`resolve`/`resolve_at_path` delegate with `None` — external API and precedence/ranking unchanged); stem line now shares `target_stem_key` |
| `crates/flavite-index/src/pool.rs` | `apply_entity_core` spine (all five paths delegate); pre/post image loaders; overlay staged from exactly the rows this tx wrote, folded at commit, dropped on rollback; counters booked at commit; `outgoing(src)` rows now come from `resolve_affected` (S1 pre/post diff + `snapshot_link_states`/`changed_outgoing_rows` deleted); link INSERT writes `target_stem`; full-table `id_conflict` statements deleted; `IndexActor::with_resolver_backstop` (measurement seam) + `resolver_diagnostics()`; `saved_note_updates…` unit test updated to S2 logging truth (2,2,1) — outgoing rows are flip-driven now |
| `crates/flavite-index/src/schema.rs` | 3.1: `links.target_stem` column + `links_target_stem` index + schema probe; `SCHEMA_MINOR = 1` |
| `crates/flavite-index/src/lib.rs` | `SCHEMA_MINOR` test updated to 1 |
| `crates/flavite-index/tests/resolution_equivalence.rs` | **new, the gate** — 5,000-case proptest (oracle after every op, backstop cadence 7 to interleave strategies), 10 seeded trap families × 3 variants = 30 deterministic cases, rollback test, short-circuit test, lowered-const backstop test |
| `crates/flavite-index/benches/apply_and_search.rs` + `Cargo.toml` | **new** — 10k-note apply-wall bench, targeted vs serial full-walk baseline in one run (harness=false manual timing per repo convention; search half lands S4) |

## Behavior notes (contract-neutral, visible in tests)

- **Flip-driven `outgoing` rows.** The unconditional `outgoing(applied)`
  row for parsed-note applies is gone: rows appear only when the applied
  note's own links actually change resolution (fresh `NULL→resolved`
  flips count), per §4.3/§6.2.3 and D4. Consumers of the applied note's
  own outgoing set use the entity row (§6.5.1 mirror point-read).
- **Conditional writes.** Both the targeted pass and the backstop walk
  write a link row only when the projected state differs from the stored
  state (idempotent re-resolution ⇒ no spurious churn, no spurious rows).
- **Counters track committed applies only** — a rolled-back transaction
  advances neither `generations_since_full`, nor `resolve_targeted_total`,
  nor the base (verified by the rollback test).

## Bench (AC-2 precursor; threshold enforcement lands in S8)

Recorded on this machine, `cargo bench -p flavite-index --bench apply_and_search`:

```
apply_and_search (10k notes): build 4.406664846s; update-save p50 563.105µs
  p99 765.857µs (targeted spine; full_total=2, targeted_total=10298)
apply_and_search serial baseline (whole-vault walk every apply, same corpus):
  p50 23.19425ms p99 24.934687ms
```

Targeted spine is ~30x cheaper than the full-walk baseline on identity-stable update saves at 10k notes; backstop cadence fires exactly twice
across 10,298 applies (`RESOLVER_BACKSTOP_GENERATIONS = 4096`), matching the
documented counter walkthrough. AC-2's 50k/150k p99 threshold is asserted in
S8 with the extended bench; the self-relative `targeted_p99 <= full_p99`
assertion in the bench already guards the regression direction.


## Exit criteria verification (S2)

- AC-1 equality gate: `cargo test -p flavite-index --test resolution_equivalence` —
  proptest 5,000 cases, **zero** mismatches; 30 seeded cases green.
- All five paths delegate the shared spine: `apply_entity_core` call sites =
  `remove_note_path`, `apply_parsed_note`, `apply_unparsed_note_inner`,
  `apply_saved_attachment`, `remove_attachment_path` (6 spine methods incl.
  the core itself).
- Apply-path grep gates: `grep -n "re_resolve_all" crates/flavite-index/src/pool.rs`
  → the single backstop invocation inside `apply_entity_core`;
  `grep -c "UPDATE notes SET id_conflict=CASE" crates/flavite-index/src/pool.rs` → 0.
- Backstop fires at the configured cadence: `backstop_fires_at_configured_interval`
  (test seam `IndexActor::with_resolver_backstop`).
- `cargo test -p flavite-index` green; `just gate` (fmt/clippy/workspace/
  fault/arch) green; bench recorded.

## Next

S3 wires the operational detector (doctor `resolver-parity`/`change-log-health`
on top of `resolver_diagnostics()`, drift fixtures, `VerifyContent`
force-backstop). S6 makes the graph mirror the feed's next consumer.
Follow-on per §7: the `target_stem` fetch arms make bulk-rename cost O(affected),
not O(vault).
