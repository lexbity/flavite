# RFC-0004 Slice S4 — Search plumbing

> Completion record for W-C. Locked decisions: D11; chunk 512 rows;
> `SearchPage.has_more` semantics = `fetched > limit`; scores
> `Option<f32>` relative-only.

## Summary

Search results now carry an honest, typed paging sentinel and relative
relevance, excerpt assembly is one batched query per 512 rows (the N+1 is
gone), and the double-truncate that discarded the pool's own `limit+1`
probe row is deleted. Same corpus ⇒ same ids, order, and excerpts —
proven against a preserved reference implementation of the old algorithm.

**C10 mechanism, refined honestly.** The RFC calls the shipped `has_more`
"provably always false". At the facade it was not observably false: the
service's `limit+1` request plus `items.len() > limit` accidentally
produced the right answer (the pre-existing
`paging_reports_more_results_within_one_generation` passes even pre-S4).
The dead part was the *contract*: the pool fetched `limit+1`, threw the
probe row away via its second truncate, and had no field to express
"more" — every consumer had to re-perform the +1 dance, and the empty/
structural pages carried no signal at all. S4 fixes the contract per
§6.3: the single truncation point computes the sentinel where the probe
was fetched, and `execute_search` consumes it
(`has_more = page.has_more || items.len() > limit` — the OR is exactly
"a `limit+1`'th row exists" across both layers' windows).

## Changes

| File | Change |
|---|---|
| `crates/flavite-index/src/search.rs` | `Hit { note_id, score }`; `search_reader` carries TopDocs scores (they existed and were discarded); `TantivyBackend::search_hits` (both branches; oversized-filter `retain` keeps rank order, 100k bound documented); `search_text` = thin ids wrapper for degraded/title-only callers (D-08); trait `search`/`SearcherTrait` shapes unchanged |
| `crates/flavite-index/src/pool.rs` | `SearchPage { … has_more }` and `SearchNote { … score: Option<f32> }` (Eq derives dropped for f32 — semantics unchanged); actor `search_hits` (commit-before-search stamping identical); one truncate + sentinel from the probe slot; empty-hybrid page `has_more: false`; `search_structural_only` computes `has_more` from its `limit+1` fetch and scores `None`; `search_note_rows` now batched: 512-id `IN` chunks, HashMap reorder into hit order, `build_excerpt` verbatim |
| `crates/flavite-index/src/search_docs.rs` | `get_many` = chunked 100-id `IN` queries; the >100 full-table-scan branch (C11) deleted |
| `crates/flavite-api/src/query_service.rs` | `execute_search` consumes `page.has_more`, truncates once; `SearchResultRow` unchanged (scores deliberately not exposed across the facade) |
| `crates/flavite-index/tests/search_plumbing.rs` | **new** — frozen-corpus equality vs a test-local reference of the old algorithm (ids/order/excerpts); `has_more` truth table T ∈ {L−1, L, L+1}; 513/1,024-row chunk crossing; `get_many` equality at 99/100/101/2,000 ids incl. a hole at 777 |
| `crates/flavite-api/tests/workspace_search.rs` | `has_more_is_true_exactly_when_a_limit_plus_one_row_exists` (T=5: limits 3/4/5 → sentinel true/true/false; pre-S4 the pool contract made this unrepresentable) |
| `crates/flavite-index/benches/apply_and_search.rs` | Search half lands (S2 stub completed): 200-row assembly wall, first-call deferred commit absorbed separately; **bench-setup hazard fixed** — both bench vaults now own separate cache directories (two `IndexActor`s sharing one `tantivy/` directory corrupt each other's segments; production ownership is one-dir-per-actor, so no product change) |

## Numbers (this machine, `cargo bench -p flavite-index --bench apply_and_search`)

```
apply (targeted spine):        p50 562µs  p99 762µs   (serial full-walk baseline: p50 22.7ms p99 24.0ms)
search 200-row assembly:       p50 878µs  p99 1.09ms  (first-call tantivy commit: 12.4ms, excluded)
```

AC-5 reference (≤ 5 ms p99 at the bound corpus): met with margin at
10k notes; the 50k-note scale test is wired into `just bench-scale` in
S8.

## Exit criteria verification (S4)

- `cargo test -p flavite-index --test search_plumbing` — 5 passed.
- `cargo test -p flavite-api --test workspace_search` — 7 passed (incl.
  the new sentinel test; all pre-existing assertions green unmodified).
- Frozen-corpus equality green (old vs new: ids, order, excerpts).
- `SearchPage.has_more` provably live end-to-end (truth table at the
  exact T = L+1 boundary at both pool and facade).
- D11 surface restraint: `SearchResultRow` has no score field; UI search
  view unchanged (`state.has_more` now receives honest values).
- `just gate` — G0 passed (fmt, clippy `--workspace --all-targets
  -D warnings`, full workspace tests incl. S1–S3 suites + the 5,000-case
  proptest, fault tests, lint-arch, arch-check).

## Next

S5 (ticketed catalog acquisition) is independent of S4 and can proceed;
S6 (graph mirror) consumes the S1 feed contract.
