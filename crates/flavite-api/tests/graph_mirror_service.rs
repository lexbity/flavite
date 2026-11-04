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
use flavite_api::{GraphService, GraphServiceStatus};
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
        self.service.notify_changed();
        wait(&self.service, || self.service.frame().is_some());
    }

    fn apply(&mut self, path: &str, body: &[u8], stamp: i64) {
        self.actor
            .apply_saved_note(path.as_bytes(), path, body, stamp, stamp)
            .unwrap();
        self.service.notify_changed();
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
    fixture.service.notify_changed();
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
    fixture.service.notify_changed();
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
