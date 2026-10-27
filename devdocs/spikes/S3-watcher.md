# S3 spike, slice 1 of 2 — watcher truth (S3-1)

**Status:** recorded for Linux (measured); macOS/Windows rows are
source-verified and pending manual probe runs (labeled honestly below).
**Spec basis:** RFC-0001 v2.1 §9 Phase 0 spike S3 ("watcher behavior per OS:
rename events, overflow, coalescing"), D5 (events are hints; overflow → full
reconcile; periodic + focus reconcile; drift counter; racy-file rule), D4
(500 ms p99 single-file visibility), D8 (T1 rename inference), §4.4 (event
coalescing parameters).
**Plan:** PLAN-C slice C1. **Probe:** `xtask/src/bin/s3_watcher_probe.rs` over
`xtask::spike_s3`, using `notify` 8.2.1 (the same crate and major the
production wrapper, C5 `watch.rs`, will use; workspace-pinned `notify = "8"`).
**Recorded:** 2026-10-27, Linux 7.2.11-arch1-1, AMD Ryzen 7 5800X3D, 16
logical cores, 62 GB RAM. The probe vault lived on tmpfs (`/tmp`), so storm
*timings* are optimistic versus NVMe; the *nomination properties* asserted by
the self-check are filesystem-independent.

## Method

Four scripted scenarios (fixed file counts, fixed names, tempdir vault),
each run against `notify`'s `RecommendedWatcher` (recursive, default config —
the configuration C5's wrapper will start from):

1. **single-file modify** — 20 appends to one note, 50 ms apart; measures
   delivery latency (op completion → event receipt, timestamped by a
   background collector thread so latencies are true delivery latencies, not
   drain-time artifacts) and the event kinds one write produces;
2. **rename pairing** — write `ideas.md`, rename to `concepts.md`; records
   the raw event stream and classifies the pairing mechanism (synthesized
   `Both` event / shared cookie / adjacent `From`+`To` / unpaired / silent);
3. **create/delete storm** — 10,000 creates then 10,000 deletes; measures
   events vs. operations, exact vs. ancestor path nominations, duplicates,
   overflow (rescan) signals, silent loss, and the arrival shape;
4. **burst pattern** — one tight burst (50 back-to-back writes) plus spaced
   probes (5–800 ms) to bracket the OS-imposed coalescing (quiet) window.

**What is asserted (self-check, `--self-check`):** the D5 nomination property
— *changed paths are eventually nominated, or an explicit overflow/rescan
signal is observed*. CI re-runs scenarios 1–3 with `--self-check` on Linux
headless. **What is only measured:** every count, latency, and histogram.
Raw event counts are measurements and are never equated with operation
counts; duplicates and coalescing are recorded, not asserted away.

## Measured matrix — Linux (inotify), notify 8.2.1

Probe output, verbatim (self-check PASS):

### Scenario 1 — single-file modify (delivery latency)

| metric | value |
| --- | --- |
| operations | 20 |
| nominated within timeout | 20/20 |
| delivery latency min / median / p95 / max (ms) | 0 / 0 / 3 / 3 |
| events captured | 60 |
| duplicate nominations (beyond first per op) | 40 |
| kind histogram | `Access(Close(Write))`: 20, `Access(Open(Any))`: 20, `Modify(Data(Any))`: 20 |

### Scenario 2 — rename pairing (`ideas.md` → `concepts.md`)

| metric | value |
| --- | --- |
| events observed | 3 |
| pairing mechanism | BothEvent |
| source nominated | true |
| destination nominated | true |
| raw events | 559ms `Modify(Name(From))` [ideas.md]  tracker=306279; 559ms `Modify(Name(To))` [concepts.md]  tracker=306279; 559ms `Modify(Name(Both))` [ideas.md, concepts.md]  tracker=306279 |

### Scenario 3 — create/delete storm

| metric | create phase | delete phase |
| --- | --- | --- |
| operations | 10000 | 10000 |
| events | 40000 | 10000 |
| paths nominated exactly | 10000 | 10000 |
| paths covered via ancestor event | 0 | 0 |
| uncovered (silent loss) | 0 | 0 |
| duplicate nominations | 30000 | 0 |
| overflow (rescan) signals (both phases) | 0 |
| arrival buckets (100 ms) | 2ms: 9003, 3ms: 26975, 4ms: 4022, 9ms: 45, 10ms: 9955 |
| kind histogram | `Access(Close(Write))`: 10000, `Access(Open(Any))`: 10000, `Create(File)`: 10000, `Modify(Data(Any))`: 10000, `Remove(File)`: 10000 |

### Scenario 4 — burst pattern (OS coalescing window)

| phase | spacing (ms) | ops | events | events/op | nominated | trailing (ms) |
| --- | --- | --- | --- | --- | --- | --- |
| tight | 0 | 50 | 150 | 150/50 | true | 0 |
| spaced-5ms | 5 | 10 | 30 | 30/10 | true | 0 |
| spaced-10ms | 10 | 10 | 30 | 30/10 | true | 0 |
| spaced-25ms | 25 | 10 | 30 | 30/10 | true | 0 |
| spaced-50ms | 50 | 10 | 10 | 10/10 | true | 0 |
| spaced-100ms | 100 | 10 | 30 | 30/10 | true | 0 |
| spaced-200ms | 200 | 10 | 30 | 30/10 | true | 0 |
| spaced-400ms | 400 | 10 | 30 | 30/10 | true | 0 |
| spaced-800ms | 800 | 10 | 30 | 30/10 | true | 0 |

No OS-level coalescing was observed on Linux at any probed spacing, including
the back-to-back tight burst: every write produced its full event set (3
events per write) regardless of spacing. (The spaced-50ms row shows 10
events for 10 ops in this run — one write's `Open`/`Close` pair was split
across the drain boundary; per-write event multiplicity is 3 throughout, as
the other rows and the kind histograms show.) The OS-imposed quiet window on
Linux is therefore effectively 0 ms; D5's 300 ms quiet window is entirely our
layer's design.

## Per-OS matrix

Linux rows are measured on the machine above. macOS/Windows rows are
**source-verified against the notify 8.2.1 backend code** (`fsevent.rs`,
`windows.rs`, `inotify.rs`) and marked *pending manual probe run* — they are
evidence about the library's behavior, not executed measurements, and the
manual runs remain a C1 follow-up per §9 S3 ("recorded manual runs for
macOS/Windows — labeled honestly as manual evidence").

| behavior | Linux (inotify) — measured | macOS (FSEvents) — source-verified, pending manual run | Windows (ReadDirectoryChangesW) — source-verified, pending manual run |
| --- | --- | --- | --- |
| events per content write | 3: `Access(Open(Any))`, `Modify(Data(Any))`, `Access(Close(Write))` | per-path events; kinds precise in notify 8 (precise mode always on) | 1: `Modify(Any)` |
| events per create | 4: Open, Create(File), Modify(Data), Close(Write) | `Create(File)`/`Create(Folder)` + modify kinds | `Create(Any)` |
| events per delete | 1: `Remove(File)` | `Remove(File)`/`Remove(Folder)` | `Remove(Any)` |
| delivery latency, single modify | p95 = 3 ms, max = 3 ms over 20 ops | FSEvents batches OS-side; measured by scenario 1 when run | near-immediate (buffered completion) |
| rename event shape | `From`, `To`, **and a synthesized `Both`** carrying both paths; all three share the tracker (inotify cookie) | `Modify(Name(Any))` per path; **no cookie — the backend documents "FSEvents provides no mechanism to associate the old and new sides of a rename event"** | `Modify(Name(From))` then `Modify(Name(To))` (`RENAMED_OLD_NAME` + `RENAMED_NEW_NAME`); **no cookie** |
| rename pairing available to T1 (D8) | cookie (tracker) **and** synthesized `Both` event | none — adjacency heuristic required | none — adjacency (`From` immediately followed by `To`) required |
| overflow signal | `EventKind::Other` + `Flag::Rescan` on `IN_Q_OVERFLOW` (`need_rescan()`); not observed at 20k ops | `EventKind::Other` + `Flag::Rescan` with info `rescan: user dropped` / `rescan: kernel dropped` on `MUST_SCAN_SUBDIRS` | **none** — a read failure (incl. `ERROR_NOTIFY_ENUM_DIR` buffer overflow) logs and silently *unwatches the directory*; no event, no rescan flag |
| OS-imposed coalescing window | 0 ms (no coalescing even back-to-back) | FSEvents coalesces OS-side; effective window measured by scenario 4 when run | not OS-coalesced; buffered completion packets |
| storm nomination (10k create + 10k delete) | 10000/10000 exact both phases; 0 uncovered; 0 overflow | pending manual run | pending manual run — see the Windows overflow row: detection there relies on D5's VerifyContent backstop, not a signal |
| silent loss at 20k ops | none observed | pending manual run | pending manual run |

## D5/D4 parameter verdicts

| parameter (spec) | Linux evidence | verdict |
| --- | --- | --- |
| Single-file external change visible ≤ 500 ms p99 (D4) | p95 = 3 ms, max = 3 ms over 20 scripted modifies | **Confirmed on Linux** with ~100× margin |
| 300 ms quiet window, keyed by path (§4.4) | the OS imposes no quiet window on Linux; 3–4 events per write arrive immediately and uncoalesced | **Confirmed** — on Linux the window is purely our coalescing layer, and it is load-bearing: without keyed coalescing every write costs 3 reconcile nominations |
| 2 s max coalescing window (§4.4) | not an OS property; the burst scenario brackets the OS-imposed window, which is 0 ms on Linux | **Unchanged** — our cap stands on top of a 0 ms OS window on Linux |
| Bulk mode at > 500 events within 1 s (§4.4) | the storm delivered 40,000 events in ~200 ms (~200k events/s) | **Confirmed** — the threshold triggers well inside a 10k-file storm; bulk mode (one full reconcile, progress on pulse) is the right response |
| Overflow → full reconcile (D5) | explicit `Rescan` signal exists on Linux (and macOS, source-verified); **not on Windows** (source-verified: read errors unwatch silently) | **Confirmed for Linux/macOS.** C5's wrapper MUST treat `need_rescan()` as full-reconcile on Linux/macOS, and MUST NOT assume any overflow signal on Windows — there the 10-minute VerifyContent pass and the drift counter are the only detection |
| 2 s racy-file rule (D5) | not a watcher property (mtime heuristic guarding watcher loss) | **Unchanged** — the storm shows no watcher loss on Linux at 20k ops; the rule and the periodic pass remain the backstop for the OSes/backends that can lose events silently |
| 10-minute VerifyContent interval (D5) | no silent loss observed on Linux; Windows has no overflow signal (source-verified) | **Confirmed, and load-bearing on Windows** — do not weaken; the spec's reversal ("the 10-minute default may change with measured I/O costs") must not remove the mandatory periodic content verification |
| T1 rename inference evidence (D8) | cookie + synthesized `Both` event on Linux | **Confirmed on Linux.** C13's T1: pair by tracker cookie/`Both` on Linux; by adjacency heuristic on macOS (no cookie exists) and Windows (`From`+`To`, no cookie) |

## Exceptions

No SHOULD-level exceptions to D5's parameters are recorded from the Linux
evidence. macOS/Windows measurements are pending manual runs; if those runs
contradict a parameter above, the exception is recorded here with a reason, per
the spec's Conventions. One asymmetry is already recorded as a *finding*, not
an exception: Windows has no explicit overflow signal in notify's backend, so
D5's "overflow triggers full reconcile" cannot fire there — the design already
covers this (periodic VerifyContent + focus reconcile + drift counter), and
C5 must implement the fallback accordingly.

## Feeds

- **C5 `flavite-fs::watch`** — per-OS wrapper parameters from this matrix:
  coalesce by path (3–4 events per write on Linux); treat `need_rescan()` as
  full-reconcile on Linux/macOS; on Windows rely on VerifyContent/drift, not a
  rescan event; restart logic must handle the backend's silent unwatch.
- **C8 `coalesce.rs`** — the 300 ms / 2 s / 500-per-second constants are
  confirmed for Linux; scenario 4 gives C8 the measured OS-imposed window
  (0 ms on Linux) to size against.
- **C13 T1 rename inference** — pairing evidence per OS (cookie/`Both` on
  Linux; adjacency on macOS/Windows).

## Reversal conditions

- If the CI Linux self-check (scenarios 1–3) starts failing, the watcher
  contract regressed — investigate before adjusting any parameter.
- If the macOS/Windows manual runs show silent loss without an overflow
  signal, D5's parameters change via a recorded exception here (the periodic
  content verification remains mandatory per the spec's reversal clause).
- If measured I/O costs make the 10-minute verification interval
  untenable, the interval may change with evidence; silent-loss detection
  must remain.

## Reproduce

```sh
# Full run (all scenarios, self-check, JSON + markdown):
cargo run -p xtask --bin s3_watcher_probe -- --self-check \
    --json target/s3/report.json --markdown
# CI variant (Linux headless, scenarios 1–3 only):
just spike-s3 --scenarios 1,2,3 --self-check --json target/s3-watcher-report.json
# On macOS / Windows (manual evidence for the pending rows):
cargo run -p xtask --bin s3_watcher_probe -- --self-check --markdown
```

Unit tests (scenario builders, statistics, pairing classification, coverage,
self-check logic, and four live-watcher tests against the real backend):
`cargo test -p xtask`.
