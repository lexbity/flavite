# S4 spike, slice 2 of 2 — DB (WAL single-writer + read pool under reconcile load)

**Status:** recorded for Linux (measured). D3 (one writer thread + a pool of
read-only WAL connections) and D12 (inline authoritative `generation`) **stand as
written** — no SHOULD-level parameter adjustments come out of this probe, so
there are no exceptions to record (see Exceptions). macOS/Windows rows are
source-verified against SQLite's WAL contract and pending manual probe runs
(labeled honestly below). With `S4-search.md` (slice 1) and the two S3 spike
docs, this closes PLAN-C's Phase-0 S3/S4 evidence.
**Spec basis:** SPEC-draft-v2 §9 Phase 0 spike S4 ("WAL read/write concurrency
under reconcile load"), D3 (exactly one writer thread owning the SQLite writer
connection and the tantivy `IndexWriter`; readers use a pool of read-only WAL
connections), D12 (the generation bumps inline on every applied change and is
authoritative), §4.14 (epoch + generation index versions, the invalidation
feed, checkpoint validation on open), §8 AC-15 (search-side latency budgets).
**Plan:** PLAN-C slice C4. **Probe:** `xtask/src/bin/s4_db_probe.rs` over
`xtask::spike_s4_db` (rusqlite 0.41 bundled = SQLite 3.54.1, tantivy 0.27,
blake3 1.8.7 — the same majors C7's `pool.rs`/`tantivy.rs` and C8's reconcile
will use). **Recorded:** 2026-10-26, Linux 7.2.11-arch1-1, AMD Ryzen 7 5800X3D
(8C/16T), x86_64, **ext4**, release profile.

Windows/macOS development is deferred (per project decision). Their rows are
**source-verified against SQLite's documented WAL behavior** — WAL gives readers
a stable snapshot and never blocks them on the writer, `SQLITE_BUSY` on a reader
during a write is a *rollback-journal* (not WAL) behavior, and the only per-OS
variance is the fsync durability tier already measured in S3-2 — and marked
*pending manual probe run*: evidence about documented behavior, not executed
measurements.

## Method

Five scripted scenarios over the deterministic CJK-heavy fixture slice 1 built
(`build_fixture`, 20k notes at full scale / 2k in CI), all against a **real
SQLite database in WAL mode with `synchronous=FULL`** — the conservative commit
tier, so every measured batch cost includes its fsync. The schema is the §4.3
structural subset that matters for concurrency: `meta`, `notes` (path_key /
stem_key / content_hash), `changes` (composite PK), `search_docs`.

1. **bulk** — a simulated bulk reconcile: one writer thread applies 40 batched
   structural transactions (500 rows each) while a pool of **8 read-only WAL
   connections** (D3) runs point-lookup / invalidation-feed-range / metadata
   reads, each on a **fresh read transaction per query**. Readers signal ready
   only after their first completed read, so the writer cannot start before
   reads overlap writes. Measures read p50/p95/p99/max *during* the writes,
   write throughput, per-batch (apply + generation bump + fsync) latency,
   `SQLITE_BUSY` counts, generation monotonicity on both sides, and
   commit → first-observation visibility lag.
2. **pool** — the same workload repeated at 1/2/4/8/16 readers (a 1k-note
   warm-up pass precedes the sweep): reader-count evidence for C7's `pool.rs`.
3. **batch** — the same workload at 50/100/500/1000/5000 rows per write
   transaction, plus a **generation-bump-only micro-bench** (500 iterations of
   "one transaction that only overwrites `meta.generation`") under
   `synchronous=FULL` and `=NORMAL`: evidence for C8's batch sizing and for what
   a generation bump actually costs.
4. **search** — the same reconcile with tantivy `add_document` interleaved on
   the one writer after each SQLite batch (D3), then exactly **one** deferred
   commit carrying the §4.14 checkpoint payload (D12), and the open-time
   generation check — fresh SQLite read connection reading `epoch` +
   `generation`, tantivy checkpoint payload, compare — timed over 10
   iterations.
5. **wal-snapshot** — commit half the fixture, open a read transaction and
   **hold** it, commit the other half with that transaction open, then compare
   the held transaction against a fresh one: the snapshot rules C7's pool must
   respect.

One generation bump per structural transaction: a batch is one atomic change
set, so `meta.generation` advances once per commit (40 commits → generation 40),
and the bump-only micro-bench prices that single bump + commit on its own.

**What is asserted (self-check, `--self-check`):** zero `SQLITE_BUSY`/`LOCKED`
and zero other errors on every reader during every write run; writer
generations strictly increase to exactly the batch count; every reader observes
non-decreasing generations; every pooled connection actually overlapped the
writer; scenario 4 performs exactly one deferred commit whose checkpoint equals
SQLite's `(epoch, generation)` and runs the open-time check 10×; scenario 5's
held transaction is stale, its fresh peer is current, and the writer was not
blocked. CI re-runs all scenarios with `--self-check` on Linux headless at a
2k-note scale (dev profile).
**What is only measured:** every latency, throughput, read count, visibility
lag, and ratio.

Readers query back-to-back with **no think time** (worst case: they compete for
CPU with the writer for as long as the reconcile runs). Production reads are
paced by real work, so the throughput slope below is an upper bound on reader
interference, not a forecast.

## Measured matrix — Linux, x86_64, ext4 (release, 20k notes)

Probe output, verbatim (`--markdown`, self-check PASS):

| scenario | result |
| --- | --- |
| 1-bulk | 20000 notes → 40 batches (89392 notes/s); batch p95 13399 µs; reads 256502 p99 32 µs; visibility p99 801 µs; busy 0 / errors 0 |
| 2-pool | 5 pool sizes; read p99 by size: 1→13 µs, 2→16 µs, 4→19 µs, 8→20 µs, 16→28 µs; busy 0 / errors 0 |
| 3-batch | 5 batch sizes; txn p95 by size: 50→1834 µs, 100→2236 µs, 500→9095 µs, 1000→14891 µs, 5000→50740 µs; bump-only FULL p50 417 µs vs NORMAL p50 16 µs; busy 0 / errors 0 |
| 4-search | SQLite batch p95 11721 µs + tantivy apply p95 258 µs (overhead 0.04×); 1 commit 33427 µs; open-check p95 129 µs; checkpoint consistent true |
| 5-wal-snapshot | held stale true / fresh current true / writer not blocked true; 20 batches with an open reader in 57531 µs |

### Scenario 1 — reads during writes, by query kind (8 readers, batch 500)

| query kind | count | p50 (µs) | p95 (µs) | p99 (µs) | max (µs) |
| --- | --- | --- | --- | --- | --- |
| point lookup (`notes.path_key`) | 85174 | 6 | 21 | 54 | 1767 |
| invalidation feed (`changes` range) | 85940 | 5 | 12 | 18 | 992 |
| metadata (`meta.generation`) | 85388 | 2 | 5 | 10 | 1130 |
| all kinds merged | 256502 | 5 | 13 | **32** | 1767 |
| commit → first observation (visibility lag) | 320 | 134 | 465 | **801** | 935 |

Batch transaction (apply + bump + fsync): p50 4403 µs, p95 13399 µs, max
14383 µs over 40 batches; writer wall 223 ms; generation 40, monotonic on both
sides; `busy = 0`, `errors = 0`.

Visibility-lag count is exactly 320 = 40 generations × 8 readers: **every**
reader observed **every** committed generation, at a median 134 µs after commit.

### Scenario 2 — read-pool sweep

| readers | reads | read p50 (µs) | read p95 (µs) | read p99 (µs) | read max (µs) | throughput (notes/s) | busy | errors |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | 24753 | 5 | 8 | 13 | 100 | 161285 | 0 | 0 |
| 2 | 46658 | 5 | 9 | 16 | 131 | 161983 | 0 | 0 |
| 4 | 90217 | 5 | 12 | 19 | 615 | 149065 | 0 | 0 |
| 8 | 233010 | 5 | 12 | 20 | 2424 | 104788 | 0 | 0 |
| 16 | 578974 | 9 | 14 | 28 | 3985 | 49483 | 0 | 0 |

### Scenario 3 — batch-size sweep + generation-bump micro-bench

| batch | batches | txn p50 (µs) | txn p95 (µs) | txn p99 (µs) | throughput (notes/s) | read p99 (µs) | busy | errors |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 50 | 400 | 991 | 1834 | 3283 | 47055 | 25 | 0 | 0 |
| 100 | 200 | 1382 | 2236 | 4861 | 70099 | 23 | 0 | 0 |
| 500 | 40 | 4821 | 9095 | 12273 | 104127 | 17 | 0 | 0 |
| 1000 | 20 | 8954 | 14891 | 14891 | 120014 | 17 | 0 | 0 |
| 5000 | 4 | 41280 | 50740 | 50740 | 140382 | 17 | 0 | 0 |

Generation-bump-only transaction (500 iterations, no rows): `synchronous=FULL`
p50 **417 µs** / p95 491 µs / p99 1181 µs; `synchronous=NORMAL` p50 **16 µs** /
p95 21 µs / p99 27 µs.

### Scenario notes

- **Bulk (1).** 20k notes land in 223 ms (**89k notes/s**) while 8 pooled
  readers serve 256k queries, p99 **32 µs** (max 1.77 ms) and never once hit
  `SQLITE_BUSY`. Read cost is query-shaped: the indexed point lookup is p95
  21 µs, the `changes` range scan p95 12 µs, the `meta` read p95 5 µs — all far
  under AC-15's 50 ms. A committed generation becomes visible to a fresh reader
  at median 134 µs / p99 801 µs: the invalidation feed's freshness bound is
  **sub-millisecond**, because a fresh transaction re-reads the WAL tail rather
  than waiting on any cache. Scenario 1 runs first with no warm-up (cold page
  cache, first WAL growth), so its 89k notes/s is the conservative bound of the
  three same-parameter runs (the warmed sweeps reach 104–149k at batch 500).
- **Pool (2).** Reads scale almost exactly 2× per pool doubling (24.7k → 46.7k →
  90k → 233k → 579k served in the same 124–404 ms window) while read p99 stays
  13–20 µs through 8 readers and only reaches 28 µs at 16. The cost shows up on
  the **writer**: throughput is flat to 2 readers (161k → 162k), −7 % at 4
  (149k), −35 % at 8 (105k), −69 % at 16 (49k), with the 16-reader tail reaching
  4.0 ms. Readers never block, never retry, never error at any size — WAL
  delivers the D3 contract at every pool size tested.
- **Batch (3).** Per-batch cost = a fixed ~417 µs fsync + ~8–12 µs per row
  (4821 µs − 417 µs ≈ 4404 µs of row work at 500 rows ⇒ 8.8 µs/row). Throughput
  rises with batch size (47k → 70k → 104k → 120k → 140k notes/s, ±15 %
  run-to-run) but so does the stall a batch represents: p95 1.8 ms at 50 rows
  versus **50.7 ms at 5000**. Read p99 is *flat* across the whole sweep
  (17–25 µs): batch size is invisible to readers, it only moves writer-side
  latency. The bump micro-bench isolates the fsync tier: FULL 417 µs vs NORMAL
  16 µs ⇒ **~400 µs of every batch transaction is the WAL commit fsync** — 42 %
  of a 50-row transaction, 8.7 % of a 500-row one, 1.0 % of a 5000-row one.
- **Search (4).** Tantivy applies ride the single writer thread and cost
  **0.04× the SQLite batch time** (p95 258 µs vs 11721 µs; 8.2 ms total across
  20k documents) — negligible beside the structural writes. The deferred commit
  is the expensive op: **33.4 ms** once per reconcile (20k documents), against
  40 batches that would have cost 40 × 33 ms ≈ 1.3 s if committed per batch —
  D12's deferral is worth ~40× on this workload. Readers are unaffected by the
  interleaving (p99 19 µs, busy 0, errors 0). The open-time generation check —
  SQLite `(epoch, generation)` vs the tantivy checkpoint — costs **27 µs median
  / 129 µs p95**, so §4.14's startup validation is free enough to run on every
  open, and it read back `(1, 40) == (1, 40)` (checkpoint consistent).
- **WAL snapshot (5).** With a read transaction held open across the writer's
  second phase, the writer committed **20 more batches (10k notes) in 57.5 ms**
  without blocking (`writer_error = null`), the held transaction still read
  generation 20 (its snapshot, stale), and a fresh transaction read generation
  40 (current). This is the usage rule C7's pool must encode: **one fresh read
  transaction per query; never hold a transaction across operations** — a held
  one is both stale and (on a rollback-journal database) a writer blocker.

### Profile note (dev vs release)

CI runs the probe in the dev profile (as `just spike-s4-db` does), so its
absolute numbers are lower but every asserted property is identical. 20k-note
dev-profile run, same machine: bulk 43k notes/s (vs 89k), read p99 57 µs (vs
32 µs), batch-500 txn p95 19.1 ms (vs 9.1 ms), bump FULL 437 µs / NORMAL 31 µs,
one deferred commit 846 ms (vs 33 ms), open-check p95 354 µs. The 2k-note CI run
is identical on every property axis (busy 0, errors 0, generations monotonic,
checkpoint consistent, held-stale/fresh-current/writer-free), differing only in
absolute timings. All recommendations below are drawn from the release run and
hold in both.

## Recommendations — pool sizing (C7) and batch sizing (C8)

**Read pool (`C7 pool.rs`): default 4 connections, hard cap 8.**

- Read latency does not decide the size: p99 is 19 µs at 4 and 20 µs at 8, and
  capacity scales linearly with the pool — the decision is writer interference.
- At 4 readers the writer loses 7 % of throughput; at 8 it loses 35 %; at 16 it
  loses 69 % and the read tail reaches 4 ms. 8 stays inside the acceptable
  envelope (and is what this probe used as its baseline), 16 does not.
- No busy timeout, no retry loop, no `SQLITE_BUSY` handling on the reader path —
  measured zero across every size and every scenario. Readers open
  `SQLITE_OPEN_READ_ONLY` and take a fresh transaction per query.
- The probe's readers are unpaced (worst case). If production later measures a
  *paced* load that needs more than 8 concurrent readers, re-run scenario 2
  before raising the cap — do not raise it on throughput intuition alone.

**Reconcile batch (`C8`): default 500 rows, cap 1000.**

- 500 rows: **9.1 ms p95** transaction, 104k notes/s, read p99 17 µs — a full
  20k-note reconcile completes in ~0.2 s of writer time.
- The cap exists for the **single-writer** reason, not for readers: any other
  write (a user's autosave, D3's one writer thread) queues behind the in-flight
  batch, so batch p95 is the worst-case save-ack delay. 1000 rows ⇒ 14.9 ms;
  5000 rows ⇒ **50.7 ms**, which spends AC-15's whole 50 ms budget on one batch.
- Below 100 rows the fixed 417 µs fsync stops amortizing (42 % of a 50-row
  transaction) and throughput collapses to 47k notes/s for no read-side benefit
  (reads are flat at 17–25 µs across the entire sweep).
- One generation bump per batch transaction; do not move the bump outside the
  transaction — the atomic (rows + generation) commit is what makes a batch
  observable as one generation.

**Search writer (`C7 tantivy.rs`)**: applies after each SQLite batch on the
same thread (0.04× cost); **exactly one deferred commit per reconcile**, at the
reconcile end or the 5 s dirty timer — never per batch (measured 40× penalty);
run the open-time generation check on every open (27 µs median).

## Exceptions

No SHOULD-level exceptions are recorded from the Linux evidence: D3's one-writer
+ read-pool shape and D12's inline authoritative generation both hold exactly as
specified, at every pool size and batch size tested, with zero `SQLITE_BUSY`.
macOS/Windows measurements are pending manual runs (see the status note above).
Scope notes, recorded so they are not mistaken for gaps:

- **Path keys are ASCII-lowered in this probe** (`path_key`/`stem_key`), because
  the fixture's paths are ASCII. Per-OS native-vs-lookup key derivation and
  Unicode normalization are C5's scope (§4.16.2); this probe exercises the *shape*
  (a separate normalized key column), not the normalization rules.
- **WAL file growth / checkpoint pressure is not measured.** Scenario 5's held
  reader lasted 57 ms; a reader held for minutes would pin the WAL and defer
  checkpoints. Nothing in D3 requires a long-lived read transaction (the rule is
  the opposite — fresh txn per query), so this is recorded as unmeasured, not as
  a defect.
- **One machine, one filesystem (ext4), one SQLite/tantivy pair.** Absolute
  numbers will move on other hardware; the properties (zero busy, monotonic
  generations, stale-held/fresh-current snapshot) are SQLite contract behavior,
  not hardware behavior.

## Feeds

- **C7 `pool.rs`** — pool size **4 (cap 8)**; read-only connections; fresh read
  transaction per query; no busy timeout and no retry path on readers (measured
  zero busy); expect commit→observation visibility of **~134 µs median / 801 µs
  p99**, which bounds how fresh an invalidation feed read is.
- **C7 `tantivy.rs`** — apply on the single writer thread after each SQLite
  batch (0.04× the SQLite cost); **one deferred commit per reconcile** (33 ms
  for 20k docs) carrying the §4.14 checkpoint payload; open-time
  `(epoch, generation)` vs checkpoint check at **27 µs median**, run every open.
- **C8 (reconcile)** — batch **500 rows (cap 1000)**; each batch is one
  transaction with one inline generation bump; expect ~9 ms p95 per batch, ~104k
  notes/s, ~0.2 s for 20k notes; batch size does not affect readers (p99 17 µs
  at every size).
- **§4.14 / D12** — generation is authoritative and observed monotonically by
  every reader in every scenario (40 commits → generation 40; readers never saw
  it decrease); the checkpoint payload matched SQLite in every check.
- **S4-search (slice 1)** — this probe supplies the WAL concurrency half that
  `S4-search.md` explicitly left to C4: single-connection results there are
  unchanged, and the deferred-commit verdict gains a writer-thread cost number.

## Reversal conditions (recorded per PLAN-C)

- If the CI Linux self-check starts failing on "readers saw N SQLITE_BUSY",
  WAL concurrency has regressed — check `journal_mode`, `busy_timeout` on the
  writer path, and any read transaction held across operations before touching
  pool sizing.
- If a reader ever observes a *decreasing* generation, D12's authority claim is
  broken (or a reader is reusing a snapshot across queries) — stop and
  investigate; every recommendation here assumes fresh-transaction reads.
- If read p99 during writes on the reference machine exceeds 1 ms at pool size
  4 (currently 19 µs), the pool cap is revisited with a fresh scenario-2 run.
- If the save-ack path ever queues behind a reconcile batch and the measured
  batch p95 approaches AC-15's 50 ms at the chosen 500-row batch (currently
  9.1 ms), lower the batch — the throughput trade is ~35 % at most, the latency
  trade is the whole budget.
- If the single deferred commit ever exceeds the 5 s dirty-timer budget on a
  realistic corpus (33 ms for 20k notes today), revisit the *commit policy*
  (C7), not the pool or batch sizes.
- If a product requirement forces a long-lived read transaction, WAL growth and
  checkpoint starvation must be measured first (explicitly out of scope above).

## Reproduce

```sh
# Full run (20k notes, all scenarios, self-check, JSON + markdown) — the
# numbers recorded above are the release-profile run:
cargo run --release -p xtask --bin s4_db_probe -- --scenarios all --notes 20000 \
  --dir target/s4-db-probe --self-check --json target/s4-db-report-release.json --markdown
# CI variant (Linux headless, 2k notes, dev profile, JSON report):
just spike-s4-db --scenarios all --notes 2000 --dir target/s4-db-probe \
  --self-check --json target/s4-db-report.json
# A single scenario (e.g. the pool sweep only):
just spike-s4-db --scenarios 2 --notes 20000
# Unit + live tests (schema, busy classification, visibility lag, and the five
# live scenarios plus the end-to-end run):
cargo test -p xtask --lib spike_s4_db
```

All five scenarios run via the probe binary; the schema/connection setup, busy
classification, visibility-lag join, scenario selection, filesystem detection,
and the five live scenarios plus `run_all` are additionally covered by
`cargo test -p xtask --lib spike_s4_db` (12 tests).
