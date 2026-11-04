# RFC-0005 Slice S1 — Generation signal: writer contract (the spine)

> Completion record. Locked decisions honored: §4.2 (bounded(1),
> latest-wins coalescing), §4.3 (store point = every generation-bump commit
> site; post-commit; structural via contract test), §4.9 (watermark store
> order, epoch handling), §5.1 (type shape in `flavite-index`).
> No consumer exists yet: the graph service is untouched and green; the
> watch is wired into its constructor seam and ignored until S5.

## Summary

The writer-side generation broadcast landed: every `IndexActor` apply
commit that bumps `MetaKey::Generation` publishes its `IndexVersion`
immediately after `tx.commit()` succeeds; `FastPath` commits (stat refresh,
no bump, no change rows) and rolled-back applies publish nothing. The
contract is property-tested against the **real apply spine** through the
**real channel** — 500 random op sequences over the five public apply
functions, plus the three named scenarios (rename storm, id-conflict churn,
DB recreate). The graph service's wake mechanics (250 ms `recv_timeout`
poll + `Directive::Poll` hints) are exactly as RFC-0004 shipped them; this
slice added no behavior there, only a stored, unread watch handle.

## Signal type (`flavite-index/src/signal.rs`, new)

- `GenerationSignal` / `GenerationWatch` per §5.1: `bounded(1)` crossbeam
  channel, two `Arc<AtomicU64>`s (epoch, generation), `publish` = atomic
  stores + `try_send` (drop-on-full), `watermark()`, `recv_timeout()`.
- Store order **epoch → generation**; read order **generation → epoch**.
  The module header carries the full §4.9 argument: the dangerous tear
  (stale epoch + fresh generation) is impossible to observe (release-store
  happens-before chain), the surviving tear (fresh epoch + stale
  generation) is made non-actionable by the consumer rule "skip only on
  exact `watermark == cursor`, never across epochs".
- Single-writer rule documented (§4.3): clones publish only from the writer
  thread / callers ordered after the commit they advertise.
- `#[derive(Clone)]` sender side; hand-rolled `Debug` on both halves (the
  crossbeam channel is not `Debug`). `new()` intentionally has no `Default`
  — a default sender would drop its watch half and silently disconnect the
  channel (documented `allow(clippy::new_without_default)`).
- Unit tests (7): 1_000-publish burst coalesces to one queued wake with
  the newest version on the watermark; quiet `recv_timeout` → `None`;
  watermark tracks every publish in order including epoch jumps; cloned
  senders share the slot and atomics; publishes into a dropped watch are
  harmless (§4.10 restart shape); **concurrent torn-pair test** (3 epochs ×
  4 000 publishes, sampled watermark invariants: the dangerous tear never
  occurs, per-atomic monotonicity holds, the harmless tear is counted and
  bounded); `Debug` impressions.

## Pool (`flavite-index/src/pool.rs`)

- `IndexActor` gains `signal: Option<GenerationSignal>`; `construct()`
  threads it; `new` and `with_resolver_backstop` keep their signatures and
  pass `None` — **every existing constructor call site compiles
  unchanged**; `IndexActor::with_generation_signal` is the production
  seam. A `None` pool works signal-less (CLI/tests).
- `publish_generation(&self, version)` — private, doc-carries the FR-1
  contract verbatim (post-commit MUST / FastPath MUST-NOT / no-op when
  signal-less / ~50 ns cost, NFR-9).
- Call sites — the spine's two bump-and-commit sites, immediately after
  commit success and ahead of the in-memory fold (the feed is the truth
  subscribers verify against):
  1. backstop-ride commit with drift flips (`Applied`), and
  2. the main structural apply commit (`Applied`).
  Both `FastPath` commit sites (early hash-match; ride-without-flips) carry
  explicit no-publish comments; `RolledBack` writes nothing and publishes
  nothing.
- **Generation-write enumeration** (S1 debt rule; `grep` over all
  `crates/*/src` for `MetaKey::Generation` / `'generation'` meta writes):
  production writes are exactly the spine's two. The two non-spine
  sites — the fresh-schema seed in `schema.rs::create_schema` and the
  `schema.rs::next_generation` helper — cannot be folded into the spine,
  so each now carries an explicit comment naming why (seed: no bump, no
  prior state, no possible cursor; `next_generation`: test/diagnostic
  helper that bypasses the spine and MUST NOT be used by writer code).
  All remaining matches are `#[cfg(test)]` fixtures (`change.rs`,
  `schema.rs`, `search.rs` mod tests, `doctor.rs` mod tests).

## Wiring

- `flavite-vault/src/open.rs`: `Vault::open_with_generation_signal(root,
  fs, config, signal)` — the production open path; `Vault::open` delegates
  signal-less so its signature and every caller stay unchanged.
  `open_or_rebuild_index` constructs `IndexActor::with_generation_signal`
  when handed one.
- `flavite-api/src/flavite.rs` (the only place owning both sides, §5.3):
  `Flavite::open_root` — the real production path used by the CLI and the
  UI — constructs `GenerationSignal::new()`, hands the sender into the
  vault's index actor, and threads the watch through
  `with_vault_inner` → `attach_services` → `GraphService::start_with_index_watch`.
  `Flavite::open` / `with_session` / `with_vault` pass `None` (their
  `IndexActor`s are host-injected; a signal-less pool is contractual).
- `flavite-api/src/graph_service.rs`: `Shared` gains `index_watch:
  Option<Arc<GenerationWatch>>` — stored per service, **intentionally
  unread until S5** (`#[allow(dead_code)]` with the reason; S1's spec line
  is "wire it to the service constructor seam now, service ignores it").
  `start_with_index_watch` is the seam; `start` / `start_with_session` /
  `start_with_parity_policy` delegate with `None`. The pump loop, counters,
  `Directive::Poll`, `GRAPH_POLL_INTERVAL`, and `graph_notify_changed` are
  untouched — S5/S6 retire them.

## Tests (`crates/flavite-api/tests/graph_event_contract.rs`, new)

- **AC-1 property test** (500 cases): random sequences (1–30 ops) of
  `apply_saved_note`, `apply_unparsed_note`, `apply_saved_attachment`,
  `remove_note_path`, `remove_attachment_path` against a tempdir index
  with a spy over the real signal. Per op: database truth read first
  (`meta.generation` via the writer connection), then — bump ⇒ exactly one
  wake carrying the committed version and watermark equal; no bump ⇒ zero
  wakes and a unmoved watermark; wakes monotone non-decreasing across the
  sequence. Final: the **published generation set equals the logged
  generation set** (`SELECT DISTINCT generation FROM changes`) — the
  broadcast never advertises a window the typed feed cannot splice, and no
  bump escapes the broadcast.
- `rename_storm_publishes_every_applied_generation` — three consecutive
  Applied commits in one feed window; one wake each, in order, each with
  rows.
- `id_conflict_churn_still_publishes_once_per_bump` — the FR-1 flip rows
  (S6's graph-visible `id_conflict` churn) ride inside the bumping commit:
  never an extra wake, never a swallowed one; generations 1–4 each
  published once.
- `fast_path_commits_stay_silent` — same-bytes re-apply and
  same-size+mtime unparsed re-apply commit silently; a rolled-back removal
  of an absent path publishes nothing.
- `db_recreate_survives_with_the_new_epoch` — actor dropped, file + WAL
  sidecars + tantivy cache recreated with epoch 7; the same signal (via
  the clone the new actor receives) publishes `(7, 1)` — the consumer's
  epoch-mismatch ⇒ reset path (AC-8's later dependency).
- `signal_less_pool_still_commits` — the `None` seam is not a footgun.
- `burst_commits_coalesce_but_the_watermark_stays_current` — 50 undrained
  commits → at most one queued wake, watermark at the newest generation,
  proved through the real spine (§4.2 latest-wins, coalescing under a busy
  pump).

## Baseline repair required for green

- `crates/flavite-index/src/resolver.rs:807`: removed an empty line after
  a doc comment. clippy 1.98's `empty_line_after_doc_comments` (in the
  `all` group this repo denies via `-D warnings`) flags it; **pre-existing
  at `e177324`**, unrelated to S1, but `cargo clippy --workspace
  --all-targets -- -D warnings` cannot go green with it standing. Whitespace-only.

## Gates & measurements

- `cargo test -p flavite-index --lib` — 58 green (7 new signal tests), 15.0 s.
- `cargo test -p flavite-index --all-features` — all targets green (58 + 8
  + 6 + 2 + 1 + 14 + 5 + 1), ≈ 117 s.
- `cargo test -p flavite-api --test graph_event_contract` — 7 green
  (including the 500-case property test), 9.2 s.
- `cargo clippy --workspace --all-targets -- -D warnings` — green.
- Prior suites green unmodified: `graph_mirror_equivalence` 9,
  `graph_mirror_service` 6, `graph_projection` 3, `link_occurrences` 5.
- `just gate` — green (recorded when the background run completed).

## Exit-criteria verification

- Contract property test green (Applied ⇒ exactly one signal; FastPath ⇒
  zero) — ✔.
- Signal unit tests green — ✔.
- All prior suites green — ✔ (gate above).
- Graph service behavior byte-identical (it ignores the watch) — ✔: the
  pump, `Directive::Poll`, `GRAPH_POLL_INTERVAL`, `notify_changed`, and
  `graph_notify_changed` are untouched; the service suites pass
  unmodified.
- `grep -rn "TODO\|unimplemented" crates/flavite-index/src/signal.rs` —
  empty ✔.
- Grep gate (authored here, checked manually; wired into `justfile` at
  S7): every production `MetaKey::Generation` write sits on a
  commit-success path next to `publish_generation` (the two spine sites),
  or is annotated as a non-writer by contract (seed, `next_generation`) — ✔.

## Costs accepted (S1 text)

- Every generation-bumping commit pays two atomic stores + one `try_send`
  on the writer thread (~50 ns, smaller than the change-row write it
  accompanies; NFR-9 measurement lands in S7's bench).
- `Shared.index_watch` is stored, not read, until S5 — the honest,
  allow-annotated forward wiring the S1 spec mandates.

## Next

S2 — mirror hygiene F2 (`rebuild_unresolved` dedup linearization), the
`node_id_of` helper, and the doc-truth fixes; it shrinks the diff S3/S4
review against.
