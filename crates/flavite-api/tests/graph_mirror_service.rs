//! RFC-0004 S6 — graph service integration over the incremental bridge.
//!
//! These tests drive the real `GraphService` thread against a real
//! `IndexActor` database: deltas must flow through the mirror (counters
//! prove the read path is incremental, not full-scan), an epoch change must
//! take the contractual reset path, a scope change over a quiet index must
//! re-publish from the cached snapshot with zero new saved reads, and a
//! seeded drift (an out-of-band row write that bypasses the apply spine)
//! must be caught by the §6.5.4 parity guard, counted, and repaired by the
//! forced full-read rebuild — with the corrected topology, never the
//! drifted one, in the published frame.
//!
//! The dirty-buffer overlay retirement on top of mirror-delta frames (the
//! third leg of the S6 checklist) is covered by `tests/graph_projection.rs`,
//! which drives the same buffers through this bridge and stays green
//! unmodified — the strongest compatibility assertion in the slice.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use flavite_api::graph_mirror::ParityPolicy;
use flavite_api::{GRAPH_SWEEP_INTERVAL, GraphService, GraphServiceStatus};
use flavite_graph::NodeKind;
use flavite_index::{IndexActor, open_or_init};
use rusqlite::Connection;

fn wait(service: &GraphService, predicate: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            std::time::Instant::now() < deadline,
            "service did not settle: {:?}; metrics {:?}",
            service.status(),
            service.metrics()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    actor: IndexActor,
    service: Arc<GraphService>,
}

impl Fixture {
    fn start(policy: ParityPolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.sqlite");
        drop(open_or_init(&db).unwrap());
        let actor = IndexActor::new(&db, 1).unwrap();
        let service = GraphService::start_with_parity_policy(db.clone(), None, policy);
        Self {
            _dir: dir,
            db,
            actor,
            service,
        }
    }

    fn settle_once(&self) {
        self.service.reevaluate();
        wait(&self.service, || self.service.frame().is_some());
    }

    fn apply(&mut self, path: &str, body: &[u8], stamp: i64) {
        self.actor
            .apply_saved_note(path.as_bytes(), path, body, stamp, stamp)
            .unwrap();
        self.service.reevaluate();
    }

    /// Wait until the service has published a frame at the index's current
    /// generation (the mirror consumed every delta through it).
    fn wait_current(&self) {
        let generation = self.actor.current_version().unwrap().1;
        wait(&self.service, || {
            self.service
                .frame()
                .is_some_and(|frame| frame.publication().generation >= generation)
        });
    }

    fn note_count(&self) -> usize {
        self.service
            .frame()
            .expect("frame")
            .publication()
            .snapshot
            .node_ids
            .iter()
            .filter(|id| id.kind == NodeKind::Note)
            .count()
    }

    fn read(&self) -> Connection {
        Connection::open(&self.db).unwrap()
    }
}

fn frame_has_edge(
    service: &GraphService,
    from: flavite_graph::NodeId,
    to: flavite_graph::NodeId,
) -> bool {
    service
        .frame()
        .expect("frame")
        .projection()
        .effective
        .edges
        .iter()
        .any(|edge| edge.from == from && edge.to == to)
}

/// The delta path is live: after the first full rebuild, every change must
/// arrive as mirror deltas (not resets), and the published frames must track
/// the index generation.
#[test]
fn service_flows_through_mirror_deltas() {
    let mut fixture = Fixture::start(ParityPolicy::default());
    fixture.settle_once();
    assert!(matches!(fixture.service.status(), GraphServiceStatus::Live));
    let baseline = fixture.service.metrics();
    assert_eq!(
        baseline.mirror_resets_total, 1,
        "service start is one reset"
    );
    assert_eq!(baseline.mirror_deltas_total, 0);

    fixture.apply("alpha.md", b"# Alpha", 1);
    fixture.apply("beta.md", b"links [[alpha]]", 2);
    fixture.wait_current();

    let metrics = fixture.service.metrics();
    assert!(
        metrics.mirror_deltas_total > baseline.mirror_deltas_total,
        "changes must arrive as deltas: {metrics:?}"
    );
    assert_eq!(
        metrics.mirror_resets_total, 1,
        "a content advance inside the epoch must not reset the mirror"
    );
    assert!(metrics.mirror_last_delta_entities >= 1, "{metrics:?}");
    assert_eq!(fixture.note_count(), 2);

    // Deleting a note must remove its node and flip the inbound link:
    // still delta-only.
    fixture.actor.remove_note_path(b"alpha.md").unwrap();
    fixture.service.reevaluate();
    fixture.wait_current();
    assert_eq!(fixture.note_count(), 1);
    assert_eq!(fixture.service.metrics().mirror_resets_total, 1);
    fixture.service.shutdown();
}

/// DB-recreate / epoch-change semantics (§6.5.2): the mirror reports a
/// reset, rebuilds from a full consistent read, and the publisher
/// re-installs the model with the new epoch.
#[test]
fn epoch_change_takes_the_reset_path() {
    let mut fixture = Fixture::start(ParityPolicy::default());
    fixture.apply("one.md", b"body", 1);
    fixture.wait_current();
    let before = fixture.service.metrics();

    // Out-of-band epoch bump simulates a recreated database.
    fixture
        .actor
        .writer()
        .conn()
        .execute("UPDATE meta SET value='9' WHERE key='epoch'", [])
        .unwrap();
    fixture.service.reevaluate();
    wait(&fixture.service, || {
        fixture
            .service
            .frame()
            .is_some_and(|frame| frame.publication().epoch == 9)
    });

    let after = fixture.service.metrics();
    assert_eq!(after.mirror_resets_total, before.mirror_resets_total + 1);
    let frame = fixture.service.frame().expect("frame");
    assert_eq!(frame.publication().epoch, 9);
    assert!(
        frame.publication().reset,
        "epoch change re-installs the model"
    );
    fixture.service.shutdown();
}

/// The §6.5.4 parity guard with a seeded drift: an out-of-band `links`
/// rewrite bypasses the apply spine, so the change log never describes it.
/// The next due parity check must count the mismatch and rebuild from the
/// full read — and the corrected topology (never the drifted mirror state)
/// is what reaches the published frame.
#[test]
fn seeded_drift_is_counted_repaired_and_never_shown() {
    let mut fixture = Fixture::start(ParityPolicy {
        generations: 1,
        interval: Duration::from_millis(50),
    });
    fixture.apply("target-one.md", b"# one", 1);
    fixture.apply("target-two.md", b"# two", 2);
    fixture.apply("source.md", b"points at [[target-one]]", 3);
    fixture.wait_current();
    let frame = fixture.service.frame().expect("frame");
    let id_of = |name: &str| {
        frame
            .publication()
            .records
            .iter()
            .find(|record| record.path == name)
            .unwrap_or_else(|| panic!("{name} missing from frame"))
            .id
    };
    let source = id_of("source.md");
    let one = id_of("target-one.md");
    let two = id_of("target-two.md");
    assert!(
        frame_has_edge(&fixture.service, source, one),
        "seed state: source -> one"
    );

    // Seed the drift OUT OF BAND: point the stored link at `two` without
    // bumping the generation and without writing change rows. The mirror
    // cannot see this through the feed — the parity guard is the detector.
    let conn = fixture.read();
    conn.execute(
        "UPDATE links SET dst_note = ?1, res_state = 'resolved' WHERE src = ?2",
        rusqlite::params![
            i64::try_from(two.id).unwrap(),
            i64::try_from(source.id).unwrap()
        ],
    )
    .unwrap();
    drop(conn);

    // One more legitimate change pumps a delta; the due parity check runs
    // BEFORE the projection is built/published, so the corrected (full-read)
    // state — not the drifted mirror — reaches the frame.
    fixture.apply("later.md", b"body", 4);
    fixture.wait_current();
    let metrics = fixture.service.metrics();
    assert!(
        metrics.mirror_parity_checks_total >= 1,
        "policy generations=1 must run the guard: {metrics:?}"
    );
    assert!(
        metrics.mirror_mismatches_total >= 1,
        "the seeded drift must be counted: {metrics:?}"
    );
    assert!(
        metrics.mirror_resets_total >= 2,
        "the mismatch forces a full-read rebuild: {metrics:?}"
    );
    assert!(
        frame_has_edge(&fixture.service, source, two),
        "the published frame must carry the true (seeded) topology"
    );
    assert!(
        !frame_has_edge(&fixture.service, source, one),
        "the stale edge must be gone"
    );
    // The mirror is healthy again and keeps flowing: further deltas are
    // served without another mismatch escalation.
    let mismatches = metrics.mirror_mismatches_total;
    fixture.apply("later.md", b"body v2", 5);
    fixture.wait_current();
    assert_eq!(
        fixture.service.metrics().mirror_mismatches_total,
        mismatches,
        "the repair must not leave the guard firing"
    );
    assert!(matches!(fixture.service.status(), GraphServiceStatus::Live));
    fixture.service.shutdown();
}

/// A scope request change over a quiet index must re-publish through the
/// CACHED mirror snapshot: zero new deltas, one new frame.
#[test]
fn scope_change_republishes_without_mirror_delta() {
    use flavite_api::{GraphViewToken, ScopeRequest};
    use flavite_graph::{GraphScope, NotePredicate};
    let mut fixture = Fixture::start(ParityPolicy::default());
    fixture.apply("scoped.md", b"---\ntags: [shown]\n---\nbody", 1);
    fixture.wait_current();
    let before = fixture.service.metrics();

    fixture
        .service
        .request_scope(ScopeRequest {
            token: GraphViewToken {
                tab: 7,
                incarnation: 1,
                sequence: 1,
            },
            scope: GraphScope {
                predicate: NotePredicate::Tag {
                    name: "shown".into(),
                },
                ..GraphScope::default()
            },
        })
        .expect("scope admitted");
    wait(&fixture.service, || {
        fixture.service.metrics().batches_total > before.batches_total
    });

    let after = fixture.service.metrics();
    assert_eq!(
        after.mirror_deltas_total, before.mirror_deltas_total,
        "an UpToDate pump with a new scope must not read the saved tables"
    );
    let frame = fixture.service.frame().expect("frame");
    let scope = frame
        .projection()
        .scopes
        .get(&7)
        .expect("scope result published")
        .result
        .as_ref()
        .expect("scope evaluates");
    assert_eq!(scope.matching_notes, 1);
    fixture.service.shutdown();
}

/// The on-demand parity API the doctor delegates to: matched on a healthy
/// mirror, findings after an out-of-band write.
#[test]
fn check_mirror_parity_surfaces_seeded_drift() {
    let mut fixture = Fixture::start(ParityPolicy::default());
    fixture.apply("p.md", b"# p", 1);
    fixture.wait_current();
    let conn = fixture.read();
    let outcome = fixture
        .service
        .check_mirror_parity(&conn)
        .expect("parity runs");
    assert!(outcome.matched(), "healthy mirror: {outcome:?}");

    conn.execute(
        "UPDATE notes SET title='drifted' WHERE path_native=?1",
        [b"p.md".as_slice()],
    )
    .unwrap();
    let outcome = fixture
        .service
        .check_mirror_parity(&conn)
        .expect("parity runs");
    assert!(
        !outcome.matched(),
        "the out-of-band title edit must be found: {outcome:?}"
    );
    drop(conn);
    fixture.service.shutdown();
}

/// Without a live service, the doctor's `graph-mirror-parity` entry is an
/// explicit, honest skip (never a vacuous pass). The database is planted at
/// the exact cache path the checker derives from the vault uuid so the
/// db-present branch of `run()` is exercised.
#[test]
fn doctor_reports_explicit_skip_without_a_live_mirror() {
    use flavite_api::ApiVaultRef;
    use flavite_api::doctor::{CheckStatus, DoctorChecker, cache_db_path};
    let root = tempfile::tempdir().unwrap();
    let uuid = uuid::Uuid::new_v4();
    let db = cache_db_path(uuid);
    std::fs::create_dir_all(db.parent().expect("cache dir")).unwrap();
    drop(open_or_init(&db).unwrap());
    let vault = ApiVaultRef::new(uuid, root.path().to_path_buf());
    let report = DoctorChecker::new(vault).run();
    let check = report
        .checks
        .iter()
        .find(|check| check.name == "graph-mirror-parity")
        .expect("the S6 check is part of the report");
    assert_eq!(check.status, CheckStatus::Ok);
    assert!(
        check.description.contains("No live graph mirror"),
        "the skip must name its reason: {:?}",
        check.description
    );
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
}

// ---------------------------------------------------------------------------
// RFC-0005 S3 — the snapshot handoff (F1): cache discipline, exact rebuild
// accounting, and the doctor's bounded wait.
// ---------------------------------------------------------------------------

#[path = "support/links.rs"]
mod links_fixture;

/// (a) Snapshot reuse: buffer-stamp-only republishes over a quiet index —
/// the generalized shape of the S6 scope-change test — must reuse the
/// cursor-keyed handoff: zero new deltas, zero new snapshot builds, while
/// frames genuinely republish (batches climb and the stamp converges).
/// This is the keystroke path the F1 cache exists to make clone-free.
#[test]
fn stamp_only_republishes_reuse_the_snapshot_handoff() {
    use flavite_api::{ExecutionResult, Invocation, LocalResult, Origin};

    let f = links_fixture::fixture(&[("keep.md", "# Keep\nbase body\n")]);
    // Settle: first frame published, every index delta of the reconcile
    // consumed (the frame's generation has reached the index head), and
    // counters published by that same pump — the baseline is then
    // immovable while only buffers change.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        f.flavite.pump();
        f.flavite.graph_reevaluate();
        let head = f.flavite.vault_info().expect("vault info").generation;
        let caught_up = f.flavite.graph_frame().is_some_and(|frame| {
            frame.publication().generation >= head && f.flavite.graph_is_current()
        });
        if caught_up {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the first current, caught-up frame never arrived: {:?}",
            f.flavite.graph_status()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let before = f.flavite.graph_metrics();
    assert!(before.mirror_snapshots_built_total >= 1, "{before:?}");
    // The handoff invariant holds at quiescence (see test (b)).
    assert_eq!(
        before.mirror_snapshots_built_total,
        before.mirror_deltas_total + before.mirror_resets_total,
        "settled metrics must satisfy snapshots == deltas + resets: {before:?}"
    );

    let (buffer_id, _revision) = f.open_note("keep.md");
    let mut last_digest = f
        .flavite
        .graph_frame()
        .expect("settled frame")
        .projection()
        .digest;
    for step in 1..=5u64 {
        let view = f
            .flavite
            .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
            .expect("buffer view");
        let result = f.flavite.execute(Invocation::new(
            links_fixture::next_id(&f.flavite),
            "apply_edits",
            serde_json::json!({
                "buffer_id": buffer_id,
                "revision": view.revision,
                "ops": [{ "range": [0, view.text.len()],
                          "replacement": format!("# Keep\nstamped edit {step}\n") }],
            }),
            Origin::Gui,
        ));
        assert!(
            matches!(result, ExecutionResult::Local(LocalResult::Success(_))),
            "edit {step} must apply locally: {result:?}"
        );
        // Wait for the republished frame to carry this stamp
        // (`graph_is_current` compares the frame's buffer stamp against
        // the session's) and to be genuinely new: the digest moves every
        // step because the overlay content changed. S5 has no buffer wake
        // yet (S6 adds the debounced one), so the self-verifying sweep is
        // the convergence driver here — one stamp-only republish per sweep
        // pass, well inside the deadline.
        let deadline = std::time::Instant::now() + Duration::from_millis(11_000);
        loop {
            f.flavite.pump();
            f.flavite.graph_reevaluate();
            let settled = f.flavite.graph_frame().filter(|frame| {
                f.flavite.graph_is_current()
                    && frame.projection().buffer_stamp.len() == 1
                    && frame.projection().digest != last_digest
            });
            if let Some(frame) = settled {
                last_digest = frame.projection().digest;
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "stamp-only republish stalled at step {step}: {:?}",
                f.flavite.graph_metrics()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let after = f.flavite.graph_metrics();
    assert_eq!(
        after.mirror_deltas_total, before.mirror_deltas_total,
        "buffer edits never move the index cursor"
    );
    assert_eq!(
        after.mirror_snapshots_built_total, before.mirror_snapshots_built_total,
        "the keystroke path must reuse the cached Arc — no view clone"
    );
    assert!(
        after.batches_total > before.batches_total,
        "frames must genuinely republish over the quiet index: {before:?} -> {after:?}"
    );
}

/// (b) Cache rebuild accounting: every delta window that reaches a pump
/// rebuilds the snapshot exactly once, and idle cadence pumps hit the
/// cache. The invariant `snapshots_built == deltas + resets` is the
/// machine-checkable statement of §4.6's "clone cost moves from per
/// publish to per index delta that reaches a publish".
#[test]
fn each_delta_window_rebuilds_the_snapshot_exactly_once() {
    let mut fixture = Fixture::start(ParityPolicy::default());
    fixture.settle_once();
    let cold = fixture.service.metrics();
    assert_eq!(cold.mirror_resets_total, 1, "service start is one reset");
    assert_eq!(
        cold.mirror_snapshots_built_total,
        cold.mirror_deltas_total + cold.mirror_resets_total,
        "the cold pump builds exactly its one snapshot: {cold:?}"
    );

    for step in 0..6u64 {
        fixture.apply(
            &format!("w{step}.md"),
            format!("# Window {step}\n[[w{}]]\n", (step + 1) % 6).as_bytes(),
            100 + i64::try_from(step).unwrap(),
        );
        fixture.wait_current();
        let metrics = fixture.service.metrics();
        assert_eq!(
            metrics.mirror_snapshots_built_total,
            metrics.mirror_deltas_total + metrics.mirror_resets_total,
            "handoff invariant violated at step {step}: {metrics:?}"
        );
        assert_eq!(
            metrics.mirror_resets_total, 1,
            "no surprise resets while the feed advances: {metrics:?}"
        );
    }
    let busy = fixture.service.metrics();
    assert!(
        busy.mirror_snapshots_built_total > busy.mirror_deltas_total,
        "the cold reset window adds its own build on top of the delta set: {busy:?}"
    );

    // Quiet pumps (the reevaluate kick stands in for the 5 s recovery
    // sweep — RFC-0005 S5 replaced the cadence, and with no watch this
    // fixture can only pump on control kicks and the sweep) are UpToDate:
    // every one is a cache hit on the handoff — the snapshot counter, the
    // delta set, and the reset set all stay flat.
    let before_idle = fixture.service.metrics().mirror_snapshots_built_total;
    for _ in 0..3 {
        fixture.service.reevaluate();
        std::thread::sleep(Duration::from_millis(30));
    }
    let idle = fixture.service.metrics();
    assert_eq!(
        idle.wakes_total, busy.wakes_total,
        "no index events exist for this fixture — nothing to wake on: {busy:?} -> {idle:?}"
    );
    assert_eq!(
        idle.watermark_skips_total, 0,
        "skips belong to index wakes only: {idle:?}"
    );
    assert!(
        idle.sweeps_total - busy.sweeps_total <= 1,
        "the sweep is the only cadence (5 s — at most one pass in 600 ms): {busy:?} -> {idle:?}"
    );
    assert_eq!(
        idle.mirror_snapshots_built_total, before_idle,
        "quiet pumps must reuse the cached snapshot: {busy:?} -> {idle:?}"
    );
    assert_eq!(
        idle.mirror_snapshots_built_total,
        idle.mirror_deltas_total + idle.mirror_resets_total,
        "the invariant survives idle: {idle:?}"
    );
    fixture.service.shutdown();
}

/// (c) NFR-7 / FR-8 (AC-5): the service pumps a scripted delta stream
/// while another thread calls `check_mirror_parity` in a loop. Every
/// parity call completes within 200 ms wall — the mutex is never held
/// across overlay assembly anymore, so the worst wait is one
/// synchronize/parity cycle — and every verdict settles matched (a
/// legitimately behind mirror catches up inside the call itself).
#[test]
fn doctor_parity_never_waits_behind_overlay_assembly() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let mut fixture = Fixture::start(ParityPolicy::default());
    // Seed real topology so each pump's build does genuine overlay work.
    for step in 0..24u64 {
        let stamp = i64::try_from(10 + step).unwrap();
        fixture.apply(
            &format!("seed{step}.md"),
            format!("# Seed {step}\nlinks [[seed{}]]\n", (step + 1) % 24).as_bytes(),
            stamp,
        );
    }
    fixture.wait_current();

    let service = Arc::clone(&fixture.service);
    let conn = fixture.read();
    let stop = Arc::new(AtomicBool::new(false));
    let worst_micros = Arc::new(AtomicU64::new(0));
    let calls = Arc::new(AtomicU64::new(0));
    let matched = Arc::new(AtomicU64::new(0));
    let (stop_t, worst_t, calls_t, matched_t) = (
        Arc::clone(&stop),
        Arc::clone(&worst_micros),
        Arc::clone(&calls),
        Arc::clone(&matched),
    );
    let driver = std::thread::spawn(move || {
        let mut max_wall = Duration::ZERO;
        while !stop_t.load(Ordering::Acquire) {
            let started = std::time::Instant::now();
            let call = service.check_mirror_parity(&conn);
            let wall = started.elapsed();
            if wall > max_wall {
                max_wall = wall;
            }
            worst_t.fetch_max(
                u64::try_from(wall.as_micros()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            calls_t.fetch_add(1, Ordering::Relaxed);
            match call {
                Ok(outcome) => {
                    assert!(
                        outcome.matched(),
                        "the pump and the guard must keep the mirror true: {outcome:?}"
                    );
                    matched_t.fetch_add(1, Ordering::Relaxed);
                }
                // Legitimate stale-under-load verdict (§6.5.4): the bounded
                // catch-up loop declines to render a verdict while the
                // writer continuously outruns it. That refusal — never a
                // mismatch, never a hang — is the honest outcome here.
                Err(error) => assert!(
                    error.contains("could not settle"),
                    "unexpected parity failure: {error}"
                ),
            }
        }
        max_wall
    });

    // A live delta stream while the doctor loops.
    for step in 0..40u64 {
        let stamp = i64::try_from(200 + step).unwrap();
        fixture.apply(
            &format!("live{step}.md"),
            format!("# Live {step}\n[[seed{}]]\n", step % 24).as_bytes(),
            stamp,
        );
    }
    fixture.wait_current();
    // Quiescent landing: with the writer done, the guard settles matched
    // — the "stale-then-matched" verdict the stream deliberately raced.
    let settled = fixture
        .service
        .check_mirror_parity(&fixture.read())
        .expect("quiescent parity settles");
    assert!(settled.matched(), "quiescent verdict: {settled:?}");
    // Let the driver land a few more calls on the settled state, then stop.
    let settle_deadline = std::time::Instant::now() + Duration::from_secs(10);
    while matched.load(Ordering::Relaxed) < 2 && std::time::Instant::now() < settle_deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    stop.store(true, Ordering::Release);
    let max_wall = driver.join().expect("parity driver finished cleanly");

    println!(
        "doctor parity under pump load: worst wall {max_wall:?}, {} calls, {} matched verdicts",
        calls.load(Ordering::Relaxed),
        matched.load(Ordering::Relaxed)
    );
    assert!(
        calls.load(Ordering::Relaxed) >= 3,
        "the parity loop must overlap the pump stream"
    );
    assert!(
        matched.load(Ordering::Relaxed) >= 2,
        "every settled verdict must be matched once the stream lands"
    );
    assert!(
        max_wall <= Duration::from_millis(200),
        "NFR-7 violated: a parity call waited {max_wall:?} for the mirror mutex"
    );
    let metrics = fixture.service.metrics();
    assert_eq!(metrics.mirror_mismatches_total, 0, "{metrics:?}");
    fixture.service.shutdown();
}

// ---------------------------------------------------------------------------
// RFC-0005 S5 — the event spine: index wakes, watermark skips, sweep
// recovery, burst coalescing, connection longevity, and DB recreate.
// ---------------------------------------------------------------------------

/// A fixture wired to the real writer-side broadcast: the actor publishes
/// every committed generation, the service watches. No test here ever
/// forces a pump through the control channel unless it is deliberately
/// exercising the host-kick leg — the point is that events carry freshness.
struct EventFixture {
    _dir: tempfile::TempDir,
    db: PathBuf,
    signal: flavite_index::GenerationSignal,
    actor: IndexActor,
    service: Arc<GraphService>,
}

impl EventFixture {
    fn start(policy: ParityPolicy) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.sqlite");
        drop(open_or_init(&db).unwrap());
        let (signal, watch) = flavite_index::GenerationSignal::new();
        let actor = IndexActor::with_generation_signal(&db, 1, signal.clone()).unwrap();
        let service = GraphService::start_with_parity_policy_and_watch(
            db.clone(),
            None,
            policy,
            Some(Arc::new(watch)),
        );
        Self {
            _dir: dir,
            db,
            signal,
            actor,
            service,
        }
    }

    fn apply(&mut self, path: &str, body: &[u8], stamp: i64) {
        self.actor
            .apply_saved_note(path.as_bytes(), path, body, stamp, stamp)
            .unwrap();
    }

    fn head(&self) -> (u64, u64) {
        self.actor.current_version().unwrap()
    }

    /// Wait, poll-free (no control kick — the spine must do this), until
    /// the published frame has caught up with the index head.
    fn wait_current(&self) {
        let generation = self.head().1;
        wait(&self.service, || {
            self.service
                .frame()
                .is_some_and(|frame| frame.publication().generation >= generation)
        });
    }
}

/// AC-2 (test scale): one commit through the spine reaches a published
/// frame within the 100 ms event budget — measured from before the commit
/// to after the frame swap, with no polling assistance anywhere. The vault
/// is seeded at 1k notes per the S5 test bound; the design-bound number
/// lands in the scale bench (AC-2/S7).
#[test]
fn index_commit_reaches_a_published_frame_within_the_event_budget() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("graph.sqlite");
    drop(open_or_init(&db).unwrap());
    let (signal, watch) = flavite_index::GenerationSignal::new();
    let mut actor = IndexActor::with_generation_signal(&db, 1, signal.clone()).unwrap();
    for step in 0..1_000u64 {
        let path = format!("n{step}.md");
        let stamp = i64::try_from(step).unwrap();
        actor
            .apply_saved_note(
                path.as_bytes(),
                &path,
                format!("# N{step}\n[[n{}]]\n", (step + 1) % 1_000).as_bytes(),
                stamp,
                stamp,
            )
            .unwrap();
    }
    let service = GraphService::start_with_parity_policy_and_watch(
        db.clone(),
        None,
        ParityPolicy::default(),
        Some(Arc::new(watch)),
    );
    // Cold boot: the t=0 bootstrap sweep reads the full truth. The one
    // queued seed wake is then legitimately skipped (watermark == cursor).
    wait(&service, || service.frame().is_some());
    let generation = actor.current_version().unwrap().1;
    wait(&service, || {
        service
            .frame()
            .is_some_and(|frame| frame.publication().generation >= generation)
    });

    let path = "late.md".to_string();
    let started = std::time::Instant::now();
    let stamp = 9_999;
    actor
        .apply_saved_note(path.as_bytes(), &path, b"[[n7.md]]", stamp, stamp)
        .unwrap();
    let target = actor.current_version().unwrap().1;
    // Poll-free wait on the published frame (reading an atomic — no kicks).
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if service
            .frame()
            .is_some_and(|frame| frame.publication().generation >= target)
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the event never delivered the commit: {:?}",
            service.metrics()
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let latency = started.elapsed();
    let metrics = service.metrics();
    println!("index-commit -> published frame latency: {latency:?} ({metrics:?})");
    assert!(
        latency < Duration::from_millis(100),
        "AC-2 test bound violated: {latency:?} at a 1k-note vault"
    );
    assert!(
        metrics.wakes_total >= 1,
        "the timed commit woke the pump: {metrics:?}"
    );
    service.shutdown();
}

/// FR-3 / AC-4: a wake whose watermark equals the cursor (and whose buffer
/// stamp is unchanged) skips the pump entirely — no DB open/read, no mirror
/// lock, no republish. Quiet stretches themselves must not pump: zero
/// wakes, zero skips, one long-lived connection (no reopens — the cadence
/// reuse is proven by the counters staying flat while the sweep runs).
#[test]
fn watermark_equal_wakes_skip_all_work() {
    let mut fixture = EventFixture::start(ParityPolicy::default());
    fixture.apply("w.md", b"# W", 1);
    fixture.wait_current();
    // Let any coalesced wake land and be processed first: a wake arriving
    // after the frame is current is legitimately skipped (FR-3), so the
    // quiet window is measured from a drained baseline.
    std::thread::sleep(Duration::from_millis(300));
    let settled = fixture.service.metrics();
    assert!(settled.mirror_snapshots_built_total >= 1, "{settled:?}");

    // A quiet stretch: nothing is coming, so nothing may happen.
    std::thread::sleep(Duration::from_millis(600));
    let quiet = fixture.service.metrics();
    assert_eq!(
        quiet.wakes_total, settled.wakes_total,
        "no events ⇒ no wakes"
    );
    assert_eq!(
        quiet.watermark_skips_total, settled.watermark_skips_total,
        "no events ⇒ no new skips"
    );
    assert_eq!(quiet.batches_total, settled.batches_total, "no republish");
    assert_eq!(
        quiet.connection_reopens_total, 0,
        "the handle is long-lived"
    );

    // A wake at the current watermark: the writer re-advertises the head
    // (legal, harmless — FR-1 says nothing about uniqueness). The pump
    // must observe the wake and skip without touching the database.
    let frame_before = fixture.service.frame().expect("settled frame");
    let head = fixture.head();
    fixture
        .signal
        .publish(flavite_index::types::IndexVersion::new(head.0, head.1));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while fixture.service.metrics().watermark_skips_total <= settled.watermark_skips_total {
        assert!(
            std::time::Instant::now() < deadline,
            "the watermark-equal wake was never skipped: {:?}",
            fixture.service.metrics()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let after = fixture.service.metrics();
    assert_eq!(
        after.wakes_total, settled.wakes_total,
        "a skipped wake must never count as a pump: {after:?}"
    );
    assert_eq!(
        after.mirror_deltas_total, settled.mirror_deltas_total,
        "skip: the mirror must not synchronize: {after:?}"
    );
    assert_eq!(
        after.mirror_parity_checks_total,
        settled.mirror_parity_checks_total
    );
    assert_eq!(after.connection_reopens_total, 0, "skip: no DB open");
    assert!(
        Arc::ptr_eq(
            &frame_before,
            &fixture
                .service
                .frame()
                .expect("frame stable across the skip")
        ),
        "skip: the frame must not be republished"
    );
    fixture.service.shutdown();
}

/// §4.2 coalescing through the real spine: a burst of 300 commits delivers
/// far fewer wakes than generations, the change-log window is spanned in
/// far fewer deltas, and the final frame sits at the final generation.
/// (The bounded(1) drop-on-full channel semantics are proven unit-side in
/// `signal.rs`; this is the service-level contract.)
// Timing-sensitive coalescing ratio (wakes < 150 for 300 commits depends on
// pump-vs-commit scheduling; failed on the loaded CI runner, run
// 42551960216). Runs in release under `just bench-scale`.
#[test]
#[ignore = "timing-sensitive; runs under just bench-scale (release, --ignored)"]
fn burst_commits_coalesce_into_windows_not_per_generation_wakes() {
    let mut fixture = EventFixture::start(ParityPolicy {
        // Parity every delta pump: pumps cost more than commits during the
        // burst, so wakes must pile up in the single coalescing slot.
        generations: 1,
        interval: Duration::ZERO,
    });
    for step in 0..40u64 {
        let stamp = i64::try_from(step).unwrap();
        fixture.apply(
            &format!("c{step}.md"),
            format!("# C{step}\n[[c{}]]\n", (step + 1) % 40).as_bytes(),
            stamp,
        );
    }
    fixture.wait_current();

    let before = fixture.service.metrics();
    for step in 0..300u64 {
        let stamp = i64::try_from(1_000 + step).unwrap();
        fixture.apply(&format!("b{step}.md"), b"burst body", stamp);
    }
    let target = fixture.head().1;
    wait(&fixture.service, || {
        fixture
            .service
            .frame()
            .is_some_and(|frame| frame.publication().generation >= target)
    });
    let after = fixture.service.metrics();
    let wakes = after.wakes_total - before.wakes_total;
    let deltas = after.mirror_deltas_total - before.mirror_deltas_total;
    println!("burst 300 commits: {wakes} wakes, {deltas} delta windows");
    assert!(
        wakes < 150,
        "at least 2:1 coalescing expected while pumps are running: {wakes}"
    );
    assert!(
        deltas < 300,
        "the change-log window must span bursts (one synchronize per wake): {deltas}"
    );
    assert_eq!(
        fixture
            .service
            .frame()
            .expect("frame")
            .publication()
            .generation,
        target,
        "the final frame must sit at the final generation"
    );
    fixture.service.shutdown();
}

/// AC-8 / FR-6: a DB recreate (new inode, new `vault_uuid`, fresh feed)
/// under a live service forces the reopen and the existing reset path —
/// never a silent read of the ghost database. The recreated vault's actor
/// carries no signal, so this also exercises the disconnected-watch leg
/// (the index arm retires onto the stub without spinning).
#[test]
fn db_recreate_forces_reopen_and_the_reset_path() {
    let mut fixture = EventFixture::start(ParityPolicy::default());
    fixture.actor.ensure_vault_uuid("uuid-before").unwrap();
    // Two ghost notes: the recreated database holds exactly one, so a
    // frame matching the NEW topology can never match the stale one.
    fixture.apply("ghost.md", b"# ghost", 1);
    fixture.apply("ghost2.md", b"# ghost2", 2);
    fixture.wait_current();
    let before = fixture.service.metrics();
    assert_eq!(
        fixture
            .service
            .frame()
            .expect("ghost frame")
            .publication()
            .snapshot
            .node_ids
            .len(),
        2,
        "{before:?}"
    );

    // Recreate out of band: drop every handle to the old file, delete it,
    // and build a fresh database (different uuid) under the same path.
    let EventFixture {
        _dir,
        db,
        signal,
        actor,
        service,
    } = fixture;
    drop(actor);
    drop(signal);
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    drop(open_or_init(&db).unwrap());
    let mut external = IndexActor::new(&db, 1).unwrap();
    external.ensure_vault_uuid("uuid-after").unwrap();
    external
        .apply_saved_note(b"after.md", "after.md", b"# after", 1, 1)
        .unwrap();
    let head_epoch = external.current_version().unwrap().0;
    let head_generation = external.current_version().unwrap().1;

    // A host kick drives the first pump (the watch is disconnected now;
    // the sweep would have gotten there within one interval regardless).
    service.reevaluate();
    wait(&service, || {
        service.frame().is_some_and(|frame| {
            let publication = frame.publication();
            publication.epoch == head_epoch
                && publication.generation >= head_generation
                && publication.snapshot.node_ids.len() == 1
        })
    });
    let after = service.metrics();
    assert!(
        after.connection_reopens_total > before.connection_reopens_total,
        "the identity swap must reopen the long-lived handle: before {before:?} after {after:?}"
    );
    assert!(
        after.mirror_resets_total >= 2,
        "the recreated database must take the reset path: {after:?}"
    );
    let frame = service.frame().expect("frame");
    assert_eq!(frame.publication().generation, head_generation);
    // The ghost note is gone — the frame describes the NEW database only.
    let names: Vec<String> = frame
        .projection()
        .saved
        .nodes
        .iter()
        .map(|node| node.path.clone())
        .collect();
    assert_eq!(names, vec!["after.md".to_string()], "{names:?}");
    service.shutdown();
}

/// AC-4 recovery leg / §4.1: an external writer (second index handle, no
/// signal) is picked up by the 5 s sweep — no events existed to wake on,
/// no control kick was sent, and the wake rate stays at the 0.2 Hz budget.
#[test]
fn sweep_recovers_external_commits_without_events() {
    let mut fixture = EventFixture::start(ParityPolicy::default());
    fixture.apply("seed.md", b"# seed", 1);
    fixture.wait_current();
    let before = fixture.service.metrics();

    let mut external = IndexActor::new(&fixture.db, 1).unwrap();
    external
        .apply_saved_note(b"ext.md", "ext.md", b"links [[seed.md]]", 9, 9)
        .unwrap();
    let target = external.current_version().unwrap().1;
    let started = std::time::Instant::now();
    wait(&fixture.service, || {
        fixture
            .service
            .frame()
            .is_some_and(|frame| frame.publication().generation >= target)
    });
    let converged = started.elapsed();
    let after = fixture.service.metrics();
    println!("external commit converged via sweep in {converged:?}");
    assert!(
        converged <= GRAPH_SWEEP_INTERVAL + Duration::from_millis(750),
        "the sweep is the recovery bound: {converged:?}"
    );
    assert_eq!(
        after.wakes_total, before.wakes_total,
        "no events existed — only the sweep may have pumped: {before:?} -> {after:?}"
    );
    assert!(
        after.sweeps_total > before.sweeps_total,
        "{before:?} -> {after:?}"
    );
    assert!(
        after.sweeps_total - before.sweeps_total <= 2,
        "the sweep cadence is 0.2 Hz (NFR-1): {before:?} -> {after:?}"
    );
    let frame = fixture.service.frame().expect("converged frame");
    let id_of = |name: &str| {
        frame
            .projection()
            .saved
            .nodes
            .iter()
            .find(|node| node.path == name)
            .unwrap_or_else(|| panic!("{name} missing from the converged frame"))
            .id
    };
    assert!(
        frame_has_edge(&fixture.service, id_of("ext.md"), id_of("seed.md")),
        "the converged frame must carry the external writer's edge"
    );
    fixture.service.shutdown();
}

// ---------------------------------------------------------------------------
// RFC-0005 S6 — buffer events + trailing-edge debounce (FR-4, AC-3).
// Real sessions, real stamp wakes: no relay, no host kick, no cadence.
// ---------------------------------------------------------------------------

/// AC-3: a burst of ten stamp changes inside one window over a quiet index
/// rebuilds the dirty overlay ONCE, within 150 ms + build of the LAST
/// transition — driven purely by the buffer watch and the debounce
/// deadline. The counters prove the shape: deadline fire(s) happened, no
/// index wake participated, no delta was read, the sweep stayed idle, and
/// the frames published are the fires' (≤ the fires, dedup swallowing any
/// trailing identical rebuild).
#[test]
fn stamp_burst_rebuilds_the_overlay_once_within_the_debounce_budget() {
    use flavite_api::{ExecutionResult, GRAPH_BUFFER_DEBOUNCE, Invocation, LocalResult, Origin};

    let f = links_fixture::fixture(&[("keep.md", "# Keep\nbase body\n")]);
    // Settle on the event spine: a frame at the index head, current stamp.
    let head = f.flavite.vault_info().expect("vault info").generation;
    let settle_deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        f.flavite.pump();
        if f.flavite.graph_frame().is_some_and(|frame| {
            frame.publication().generation >= head && f.flavite.graph_is_current()
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < settle_deadline,
            "the first current frame never arrived: {:?}",
            f.flavite.graph_status()
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    let (buffer_id, _revision) = f.open_note("keep.md");
    let before = f.flavite.graph_metrics();

    let burst_start = std::time::Instant::now();
    for step in 1..=10u64 {
        let view = f
            .flavite
            .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
            .expect("buffer view");
        let result = f.flavite.execute(Invocation::new(
            links_fixture::next_id(&f.flavite),
            "apply_edits",
            serde_json::json!({
                "buffer_id": buffer_id,
                "revision": view.revision,
                "ops": [{ "range": [0, view.text.len()],
                          "replacement": format!("# Keep\nstamped edit {step}\n") }],
            }),
            Origin::Gui,
        ));
        assert!(
            matches!(result, ExecutionResult::Local(LocalResult::Success(_))),
            "edit {step} must apply locally: {result:?}"
        );
    }
    let burst = burst_start.elapsed();
    // Normally ten in-process edits complete well inside one 150 ms window;
    // a starved test thread may span two or three, costing exactly one
    // extra fire. Derive the budget from the windows actually spanned.
    let windows = {
        let burst_ms = u64::try_from(burst.as_millis()).unwrap_or(u64::MAX);
        let window_ms = u64::try_from(GRAPH_BUFFER_DEBOUNCE.as_millis()).unwrap_or(1);
        burst_ms.div_ceil(window_ms).max(1)
    };

    // The ONLY requirement: the frame carries the final stamp within the
    // budget measured from the last transition. No kicks — the loop just
    // reads the published frame.
    let since_last = std::time::Instant::now();
    loop {
        f.flavite.pump();
        if f.flavite.graph_is_current() {
            break;
        }
        let elapsed = since_last.elapsed();
        assert!(
            elapsed <= GRAPH_BUFFER_DEBOUNCE + Duration::from_millis(600),
            "AC-3: the debounced rebuild never landed ({elapsed:?} since the last change): {:?}",
            f.flavite.graph_metrics()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let wall = since_last.elapsed();
    let after = f.flavite.graph_metrics();
    println!(
        "last stamp change -> current overlay frame: {wall:?} (window {GRAPH_BUFFER_DEBOUNCE:?})"
    );
    assert!(
        wall <= GRAPH_BUFFER_DEBOUNCE + Duration::from_millis(500),
        "AC-3 budget violated: {wall:?}"
    );
    let fires = after.debounced_rebuilds_total - before.debounced_rebuilds_total;
    let publishes = after.batches_total - before.batches_total;
    assert!(
        (1..=windows).contains(&fires),
        "one burst collapses to a fire per spanned window: {burst:?} spans {windows} window(s), {after:?}"
    );
    assert!(
        (1..=fires).contains(&publishes),
        "every publish rides a rebuild: {after:?}"
    );
    assert_eq!(
        after.wakes_total, before.wakes_total,
        "no index commits — no wakes"
    );
    assert_eq!(
        after.mirror_deltas_total, before.mirror_deltas_total,
        "the index stayed quiet: buffer work never reads deltas"
    );
    // (No sweep-equality assert: a 5 s boundary crossing under parallel
    // load is timing noise — the wall budget above already excludes a
    // sweep-driven convergence, which would take seconds.)
    assert_eq!(
        after.mirror_snapshots_built_total, before.mirror_snapshots_built_total,
        "stamp-only fires reuse the cached snapshot handoff (S3/S4 unchanged)"
    );
}

/// §4.4 "Index-delta rebuilds are never debounced": a commit landing while
/// a stamp window is armed pumps immediately (writer wake), and the pending
/// window is consumed by that rebuild — never a trailing second fire. The
/// typed stamp and the new generation reach the same published frame.
#[test]
fn index_commit_during_typing_pumps_immediately_and_consumes_the_window() {
    use flavite_api::{GRAPH_BUFFER_DEBOUNCE, Invocation, Origin};

    let f = links_fixture::fixture(&[("a.md", "# A\nsee [[b]]\n"), ("b.md", "# B\n")]);
    let head = f.flavite.vault_info().expect("vault info").generation;
    let settle_deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        f.flavite.pump();
        if f.flavite.graph_frame().is_some_and(|frame| {
            frame.publication().generation >= head && f.flavite.graph_is_current()
        }) {
            break;
        }
        assert!(
            std::time::Instant::now() < settle_deadline,
            "the first current frame never arrived: {:?}",
            f.flavite.graph_status()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let (buffer_id, _revision) = f.open_note("a.md");
    let before = f.flavite.graph_metrics();

    // Type (arms the trailing window)...
    let view = f
        .flavite
        .get_buffer_view(flavite_buffer::BufferId::from(buffer_id), None)
        .expect("view");
    let typed = f.flavite.execute(Invocation::new(
        links_fixture::next_id(&f.flavite),
        "apply_edits",
        serde_json::json!({
            "buffer_id": buffer_id,
            "revision": view.revision,
            "ops": [{ "range": [0, view.text.len()],
                      "replacement": "# A typed mid-flight\nsee [[b]]\n" }],
        }),
        Origin::Gui,
    ));
    assert!(typed.is_success(), "edit applies: {typed:?}");

    // ...and an index commit lands inside the window: an out-of-band file
    // written straight onto the shared database through the spine.
    fixture_apply_note(&f, "late.md", "links [[b]]", 77_777);
    let target = f.flavite.vault_info().expect("info").generation;
    assert!(target > head, "the commit advanced the generation");

    let started = std::time::Instant::now();
    loop {
        f.flavite.pump();
        if f.flavite
            .graph_frame()
            .is_some_and(|frame| frame.publication().generation >= target)
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the index commit never reached a frame: {:?}",
            f.flavite.graph_metrics()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let commit_wall = started.elapsed();

    // The wake path pumped the commit (never debounced): an index-driven
    // pump happened.
    let after_commit = f.flavite.graph_metrics();
    assert!(
        after_commit.wakes_total > before.wakes_total,
        "the commit woke an index pump: {after_commit:?}"
    );

    // §4.4 immediacy: the commit reached a published frame well before a
    // pure stamp path could (the debounce floor is 150 ms after the last
    // typing wake; measured from after the commit returned).
    assert!(
        commit_wall < GRAPH_BUFFER_DEBOUNCE,
        "the index commit waited for a debounce window: {commit_wall:?}"
    );

    // Let the state machine fully settle (buffer refresh paths may fire
    // late stamp wakes around a reconcile), then require counter silence:
    // pending windows expire, nothing refires without new transitions —
    // and the whole typing-plus-commit episode rebuilds at most twice,
    // never once per keystroke (the storm FR-4 exists to prevent).
    let mut settled = after_commit;
    let quiet_deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        std::thread::sleep(Duration::from_millis(300));
        let probe = f.flavite.graph_metrics();
        if probe.debounced_rebuilds_total == settled.debounced_rebuilds_total {
            settled = probe;
            break;
        }
        settled = probe;
        assert!(
            std::time::Instant::now() < quiet_deadline,
            "debounce fires never went quiet: {settled:?}"
        );
    }
    let fires = settled.debounced_rebuilds_total - before.debounced_rebuilds_total;
    println!(
        "typing + one index commit episode: {fires} debounced fire(s), commit wall {commit_wall:?}"
    );
    assert!(
        fires <= 2,
        "a ten-transition burst plus a commit must collapse into windows, not per-change rebuilds: {settled:?}"
    );
    assert!(
        f.flavite.graph_is_current(),
        "typed stamp is live: {settled:?}"
    );
    let frame = f.flavite.graph_frame().expect("frame");
    assert_eq!(frame.publication().generation, target);
    // The typed content actually reached the overlay (not just "current"):
    assert!(
        frame
            .projection()
            .effective
            .nodes
            .iter()
            .any(|node| node.path == "late.md"),
        "the committed note is in the frame"
    );
    assert!(
        frame
            .projection()
            .overlays
            .values()
            .any(|overlay| matches!(overlay, flavite_api::OverlayState::Ready { .. })),
        "the typed overlay is live in the frame: {:?}",
        frame.projection().overlays
    );
}

/// Push one note through the vault's index spine with no UI relay in sight.
fn fixture_apply_note(f: &links_fixture::Fixture, name: &str, body: &str, _stamp: i64) {
    use flavite_api::{ExecutionResult, Invocation, Origin};
    use flavite_fs::Fs;
    f.fs.write_new(&f.root.join(name), body.as_bytes())
        .expect("fs write");
    let result = f.flavite.execute(Invocation::new(
        links_fixture::next_id(&f.flavite),
        "vault.reconcile",
        serde_json::json!({ "scope": "full", "mode": "fast" }),
        Origin::Gui,
    ));
    if let ExecutionResult::Pending { request_id } = result {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            f.flavite.pump();
            let done = f.flavite.wait_for_operation(request_id);
            if !matches!(done, ExecutionResult::Pending { .. }) {
                assert!(done.is_success(), "reconcile: {done:?}");
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "reconcile never finished"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
