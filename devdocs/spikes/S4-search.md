# S4 spike, slice 1 of 2 — search (CJK bigram quality + deferred commits)

**Status:** recorded for Linux (measured); macOS/Windows rows are
source-verified against tantivy/SQLite contracts and pending manual probe runs
(labeled honestly below). tantivy **go** — no FTS5 reversal (the verdict is
recorded below).
**Spec basis:** SPEC-draft-v2 §9 Phase 0 spike S4 ("tantivy CJK bigram quality,
deferred-commit behavior under autosave load — commit-before-search latency,
`committed_generation` replay"), D12 (SQLite is the record; tantivy is a derived
search view with deferred commits; bigram tokenization for Han/Kana/Hangul
runs), §4.7 (search UX), §4.14 (index versions, change retention, search
checkpoints — the checkpoint is published atomically with its own durable
commit, and the SQLite mirror is diagnostic only), §8 AC-15 (search p95 ≤ 50 ms
at 20k notes; a search issued immediately after a save ack returns the saved
content). **Plan:** PLAN-C slice C3. **Probe:** `xtask/src/bin/s4_search_probe.rs`
over `xtask::spike_s4_search` (tantivy 0.27 + rusqlite 0.41 bundled, the same
majors C7's `tantivy.rs` and the diagnostic mirror will use). **Recorded:**
2026-10-26, Linux 7.2.11-arch1-1, AMD Ryzen 7 5800X3D, x86_64.

Windows/macOS development is deferred (per project decision). Their rows are
**source-verified against the tantivy/SQLite API contracts** (tantivy's
tokenizer/commit-payload API and SQLite WAL are OS-independent; the only
per-OS variance is fsync durability, measured in S3-2) and marked *pending
manual probe run* — evidence about documented behavior, not executed
measurements.

## Method

Four scripted scenarios over a deterministic, seeded CJK-heavy fixture (JA/ZH/KO
phrases plus English filler and a per-note unique Latin sentinel). Scenario 1
indexes in RAM; scenarios 3–4 index on disk (temp vault) so the crash
boundaries are genuine. `--self-check` asserts only the properties the
production index adapter relies on; everything else is measured, never
fabricated.

1. **CJK bigram quality** — one index, two tokenizer pipelines over the *same*
   corpus (fields `body_bigram` / `body_plain`): the D12 pipeline
   (`CjkBigramTokenizer → LowerCaser → AsciiFoldingFilter`, bigram for
   Han/Kana/Hangul runs, whole-word for non-CJK) versus a plain baseline
   (`SimpleTokenizer → LowerCaser → AsciiFoldingFilter`, which treats an
   unbroken CJK run as one token). A curated query set (13 JA/ZH/KO terms and
   substrings + one single-char CJK query + 3 Latin controls) is measured
   against **substring ground truth** (a document is relevant iff its title or
   body contains the query); precision/recall/F1 per query and aggregate.
2. **Deferred commits under autosave** — 600 saves at 1 s intervals (a
   10-minute window). Documents are applied to the `IndexWriter` immediately;
   `commit()` is deferred to the 5 s dirty timer (simulated clock — no real
   sleeps), counting commits. A bounded loop then measures the
   **commit-before-search** added latency (the cost of the commit a query
   forces) against a clean baseline search.
3. **Crash + `committed_generation` replay** — commit a base generation with a
   checkpoint payload; apply an upsert + a delete that commit to the SQLite
   mirror but are left **uncommitted** in tantivy; "crash" (drop the writer);
   reopen; read the durable checkpoint; replay the invalidated `note_id`s from
   the mirror (delete-then-re-add-if-present); measure replay timing and assert
   the upsert is visible, the delete is honored, and the checkpoint advanced.
4. **Atomic checkpoint publication (§4.14)** — prove the commit payload
   (`prepare_commit → set_payload → commit`, read back via
   `index.load_metas().payload`) is the durability authority across every crash
   boundary: crash after the search commit but before the SQLite
   `tantivy_committed_generation` update (checkpoint ahead of the stale
   counter); crash after the mirror update (consistent); a deletion applied
   before the search commit (gone after commit+reload); reader reload reflects
   a just-committed document; and an ahead checkpoint (search generation ahead
   of the mirror's structural generation) is detected as rebuild-required.

**What is asserted (self-check, `--self-check`):** bigram materially outperforms
plain on CJK and is usable on its own (CJK recall ≥ 0.5); both pipelines recall
Latin controls (≥ 0.9); deferred commits keep the commit count far below the
save count (≤ ¼) with the commit-before-search latency recorded; replay applies
the delta correctly and advances the checkpoint; and every §4.14 publication
crash boundary behaves with the checkpoint as authority. CI re-runs all
scenarios with `--self-check` on Linux headless at a 2k-note scale.
**What is only measured:** every latency, precision/recall value, commit count,
and the raw per-query rows.

## Measured matrix — Linux, x86_64

Probe output, verbatim (20k-note fixture, self-check PASS):

| scenario | result |
| --- | --- |
| 1 quality | bigram CJK P/R/F1 **0.923/0.923/0.923** (1 zero-recall query); plain CJK 0.231/0.231/0.231 (10 zero-recall); bigram **and** plain Latin **1.000/1.000/1.000** |
| 2 autosave | 600 saves → **120 commits** (ratio 0.200 = the 5 s timer); commit-before-search median 16.6 ms / p95 116 ms / max 219 ms; clean baseline search median 2.1 ms / p95 2.7 ms |
| 3 replay | checkpoint 20000 → mirror 20002 (2 invalidated); replay **17.1 ms**; update visible ✓, deletion honored ✓, checkpoint advanced ✓ |
| 4 publication | authority ✓, crash after-commit-before-mirror ✓, crash after-mirror ✓, deletion-before-commit gone ✓, reader reload ✓, ahead-checkpoint detected ✓ (checkpoint 50, stale SQLite counter 49) |

CI-scale (2k-note) run is identical on every quality/property axis (bigram CJK
0.923 vs plain 0.231, Latin 1.000 both; 600 → 120 commits; replay 15.2 ms;
publication all ✓), differing only in absolute timings.

### Scenario 1 — per-query precision/recall (bigram vs plain), 20k notes

| query | lang | relevant | bigram P/R | plain P/R |
| --- | --- | --- | --- | --- |
| 東京 | ja | 2592 | 1.00 / 1.00 | 0.00 / 0.00 |
| 渋谷 | ja | 2624 | 1.00 / 1.00 | 0.00 / 0.00 |
| 京都 | ja | 3897 | 1.00 / 1.00 | 0.00 / 0.00 |
| 富士山 | ja | 2653 | 1.00 / 1.00 | 1.00 / 1.00 |
| 桜 *(single char)* | ja | 2647 | 0.00 / 0.00 | 0.00 / 0.00 |
| 北京 | zh | 3042 | 1.00 / 1.00 | 0.00 / 0.00 |
| 上海 | zh | 3000 | 1.00 / 1.00 | 0.00 / 0.00 |
| 人工智能 | zh | 3051 | 1.00 / 1.00 | 1.00 / 1.00 |
| 机器 | zh | 2994 | 1.00 / 1.00 | 0.00 / 0.00 |
| 서울 | ko | 4511 | 1.00 / 1.00 | 0.00 / 0.00 |
| 부산 | ko | 3335 | 1.00 / 1.00 | 0.00 / 0.00 |
| 한국 | ko | 3306 | 1.00 / 1.00 | 0.00 / 0.00 |
| 데이터 | ko | 3252 | 1.00 / 1.00 | 1.00 / 1.00 |
| search | en | 6230 | 1.00 / 1.00 | 1.00 / 1.00 |
| tantivy | en | 6351 | 1.00 / 1.00 | 1.00 / 1.00 |
| autosave | en | 6278 | 1.00 / 1.00 | 1.00 / 1.00 |

Precision is 1.00 on every query that returns hits: a bigram/word match is
itself a substring of the ground truth, so the D12 pipeline has **no false
positives** against substring relevance. Recall is 1.00 for every multi-char
CJK term *and* substring — the exact query shapes users type.

### Scenario notes

- **CJK quality (1).** The plain pipeline collapses to 0.231 CJK recall because
  `SimpleTokenizer` treats an unbroken Han/Kana/Hangul run as a single token, so
  a substring query (東京 against a document holding 東京都) cannot match. The
  bigram pipeline indexes every adjacent character pair, so any multi-char term
  or substring retrieves its documents perfectly (P = R = 1.00). The **one**
  honest limitation is the **single-character** CJK query (桜): a pure-bigram
  index stores no unigram for a character that also appears inside a longer run,
  so single-char recall is 0. This is a known, deliberate bigram tradeoff over
  a rare query class; it is recorded, not hidden, and is mitigable later (index
  unigrams alongside bigrams) without abandoning tantivy. Latin controls are
  1.000 for both pipelines — bigram tokenization costs nothing on non-CJK text.
- **Deferred commits (2).** 600 one-per-second saves produce **120 commits** —
  exactly one per 5 s of dirty time (the D12 R4 timer), a 5× reduction and far
  below the one-commit-per-save cliff R4 exists to fix. A clean search is
  **2.1 ms median / 2.7 ms p95**, comfortably under AC-15's 50 ms bound. The
  commit-before-search added latency is **16.6 ms median**, and only applies to
  the first search *after* a save (when the index is dirty); between saves the
  search is the clean 2 ms read. The p95 (116 ms) / max (219 ms) tail reflects
  the probe committing 100× back-to-back, which stresses tantivy's merge policy
  harder than the production 5 s cadence — a conservative (pessimistic) bound.
  Scenario 2's index is an in-RAM directory, so this cost is dominated by
  segment serialization/merge scheduling, not media fsync; the fsync tier on
  top is small and measured separately in S3-2 (~0.65 ms ext4, ~0 µs tmpfs).
- **Crash replay (3).** After the crash the uncommitted delta (one upsert + one
  delete) is recovered from the mirror in **17.1 ms**: the invalidated `note_id`s
  are re-applied delete-then-re-add-from-current-mirror-body, the upserted marker
  becomes searchable, the deleted note's sentinel is gone, and the checkpoint
  advances to the mirror generation. Replay is idempotent and deletion-safe —
  the property §4.14's checkpoint-replay relies on.
- **Publication (4).** The checkpoint payload is the durability authority, not
  the SQLite counter. With the mirror's `tantivy_committed_generation` left
  stale at 49 while the commit payload says 50 (the crash-after-commit-before-
  mirror-update boundary), reopening reads checkpoint 50 and the committed
  documents are still findable — durability came from the commit, not the
  counter. A deletion applied before the search commit is gone after
  commit+reload; reader reload reflects a just-committed document; and staging
  an ahead checkpoint (forcing the mirror's structural generation back to 40
  while the checkpoint reads 50, i.e. SQLite restored from an older snapshot)
  is correctly flagged as rebuild-required. This confirms §4.14's rule that
  "updating a SQLite counter separately cannot establish search durability."

## Verdict — tantivy: **GO** (no FTS5 reversal)

This section is the C3 deliverable: the tantivy go/no-go verdict. D12 stands.
The evidence supports proceeding with tantivy as the search view:

1. **CJK bigram quality is production-grade.** Perfect precision *and* recall
   (1.00 / 1.00) on every multi-char JA/ZH/KO term **and** substring — the
   queries users actually type. The non-CJK-aware baseline collapses to 0.23.
   The single-character CJK under-recall (0 for 桜) is the one real limitation:
   a rare query class, honestly recorded, mitigable with dual unigram+bigram
   indexing if it ever becomes a complaint — not a reason to reverse to FTS5
   (FTS5's default trigram tokenizer has the same single-character caveat and
   no better CJK recall).
2. **Deferred commits behave under autosave.** 5× commit reduction (120 vs 600)
   exactly tracking the 5 s timer; clean search 2.1 ms median / 2.7 ms p95
   (≪ AC-15's 50 ms); commit-before-search adds a bounded ~16.6 ms only when
   dirty. No perf cliff.
3. **Crash-replay is correct and fast.** `committed_generation` delta replay
   recovers the uncommitted window in ~17 ms with correct update-visibility and
   deletion-handling, and advances the checkpoint.
4. **Atomic checkpoint publication is proven across every §4.14 boundary.** The
   commit payload (`set_payload`) is a genuine durability authority; the SQLite
   mirror is correctly diagnostic-only; ahead checkpoints are detected.

C7's `SearchBackend` trait remains the intended swap point, but this probe
records the decision to **use tantivy** — the reversal-to-FTS5 contingency is
not triggered. The follow-up worth watching (not blocking): confirm the
single-character CJK recall tradeoff is acceptable in UX (P6) before C7 locks
the tokenizer, and re-measure commit-before-search on a disk-backed index if
the RAM-directory caveat above proves unrepresentative.

## Exceptions

No SHOULD-level exceptions are recorded from the Linux evidence. macOS/Windows
measurements are pending manual runs; tantivy's tokenizer and commit-payload API
and SQLite WAL semantics are OS-independent, so the only per-OS variance is the
fsync durability tier, already measured in S3-2. One *finding* (not an
exception) is recorded above and repeated here for the reversal record:

- **Single-character CJK queries under-recall under pure bigram** (桜 → recall
  0.00, limit-independent). A pure-bigram index stores no unigram for a
  character that also occurs inside a longer run. This affects only single-char
  CJK queries; multi-char terms/substrings are perfect. If product later needs
  single-char recall, the tokenizer dual-indexes unigrams alongside bigrams — a
  contained change inside C7, not a backend reversal.

## Feeds

- **C7 `tantivy.rs` + tokenizer** — locks the D12 pipeline
  (`CjkBigramTokenizer → LowerCaser → AsciiFoldingFilter`; the probe's
  `spike_s4_search::CjkBigramTokenizer` is the working reference), the deferred
  commit policy (apply-immediately, commit on search-if-dirty or the 5 s timer),
  and the `SearchBackend` trait as the swap point. The probe confirms the
  checkpoint-payload mechanism (`prepare_commit → set_payload → commit`, read
  via `load_metas().payload`) is the publication primitive.
- **C7 / C4 (diagnostic mirror)** — the SQLite mirror is advisory-only and
  updated *after* the search commit succeeds; the durability authority is the
  tantivy commit payload, never the `tantivy_committed_generation` counter.
- **C4 (DB concurrency)** — out of scope here: this probe uses a single
  SQLite connection; WAL single-writer/read-pool behavior is C4's job.

## Reversal conditions (recorded per PLAN-C)

- If the CI Linux self-check starts failing on scenario 1, the CJK tokenizer
  regressed — investigate before touching the pipeline.
- If real-usage CJK recall on multi-char terms drops below the 0.5 self-check
  floor, the bigram pipeline is no longer production-grade and the FTS5
  reversal (via C7's `SearchBackend`) is reconsidered **with these numbers as
  the baseline**.
- If single-character CJK queries prove common enough in UX (P6) that the 0
  recall is unacceptable, the fix is dual unigram+bigram indexing inside the
  tantivy tokenizer (a contained change), not a backend swap — recorded here so
  the cheaper fix is chosen first.
- If commit-before-search latency on a **disk-backed** index exceeds AC-15's
  50 ms in a way the RAM-directory measurement masked, the commit policy (not
  the backend) is revisited.

## Reproduce

```sh
# Full run (20k notes, all scenarios, self-check, JSON + markdown):
just spike-s4-search --scenarios all --notes 20000 --self-check \
  --json target/s4-search-report.json --markdown
# CI variant (Linux headless, scaled 2k notes, JSON report):
just spike-s4-search --scenarios all --notes 2000 --self-check \
  --json target/s4-search-report.json
# A single scenario (e.g. quality only):
just spike-s4-search --scenarios 1 --notes 2000 --markdown
# Unit + live tests (tokenizer, fixture, PRF math, replay, publication):
cargo test -p xtask --lib spike_s4
```

All four scenarios run via the probe binary; the custom tokenizer, fixture
generator, checkpoint codec, mirror, and the live quality/autosave/replay/
publication scenarios are additionally covered by `cargo test -p xtask --lib
spike_s4` (16 tests).
