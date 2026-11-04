//! RFC-0005 S1 — the generation signal writer contract (FR-1, AC-1).
//!
//! Every public apply path on `IndexActor` that commits a structural
//! generation bump MUST publish exactly one `IndexVersion` on the
//! `GenerationSignal`, immediately after its transaction commits;
//! `FastPath` commits (stat refresh without a bump) and rolled-back applies
//! MUST publish nothing. These tests drive the real apply spine against a
//! real signal — the spy wraps the actual channel and atomics, so every
//! assertion also proves the production publish path end to end, not a
//! mock's opinion of it.
//!
//! The property test (AC-1) runs random op sequences through the public
//! apply functions and checks per op:
//!
//! - database truth first: the committed `meta.generation` equals exactly
//!   what the op returned (bump) or what it was before (silent commit);
//! - one bump ⇒ exactly one wake carrying the committed version;
//! - no bump ⇒ zero wakes, and the watermark did not move;
//! - wakes are monotone non-decreasing across the whole sequence;
//! - every published generation has at least one change-log row — the
//!   broadcast never advertises a window the typed feed cannot splice.
//!
//! Scenario tests cover the named S1 cases: a rename storm (several
//! `Applied` steps in one change-log window), id-conflict churn (the
//! S6/FR-1 flip rows travel inside the bump that already publishes), a
//! silent re-apply (`FastPath` stays invisible), and a database recreate
//! (the signal survives; the next publish carries the new epoch).

use std::path::Path;
use std::sync::{Arc, Mutex};

use flavite_index::{
    GenerationSignal, GenerationWatch, IndexActor, IndexVersion, MetaKey, get_meta_u64,
    open_or_init, set_meta,
};
use proptest::prelude::*;
use rusqlite::Connection;

const QUIET: std::time::Duration = std::time::Duration::ZERO;

/// Spy over the real broadcast: records every wake drained from the watch
/// into a shared log. The test drives applies and drains between ops, so
/// the bounded(1) queue never coalesces two observations and wake counts
/// are exact.
struct Spy {
    signal: GenerationSignal,
    watch: GenerationWatch,
    recorded: Arc<Mutex<Vec<IndexVersion>>>,
}

impl Spy {
    fn new() -> Self {
        let (signal, watch) = GenerationSignal::new();
        Self {
            signal,
            watch,
            recorded: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Drain the queue into the record and return what arrived.
    fn drain(&self) -> Vec<IndexVersion> {
        let mut wakes = Vec::new();
        while let Some(version) = self.watch.recv_timeout(QUIET) {
            wakes.push(version);
        }
        if !wakes.is_empty() {
            self.recorded
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .extend(wakes.iter().copied());
        }
        wakes
    }

    fn watermark(&self) -> IndexVersion {
        self.watch.watermark()
    }

    fn recorded(&self) -> Vec<IndexVersion> {
        self.recorded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Database truth read outside any writer transaction (WAL: a committed
    /// generation is visible to a fresh read, RFC-0005 §4.9).
    fn db_version(actor: &IndexActor) -> IndexVersion {
        let conn = actor.writer().conn();
        let epoch = get_meta_u64(conn, MetaKey::Epoch)
            .expect("epoch is readable")
            .expect("schema seeds the epoch");
        let generation = get_meta_u64(conn, MetaKey::Generation)
            .expect("generation is readable")
            .unwrap_or(0);
        IndexVersion::new(epoch, generation)
    }
}

/// Run one op and assert the FR-1 contract on its effect: exactly one wake
/// carrying the committed version when the op bumped the generation, zero
/// wakes and a still watermark when it committed silently (`FastPath`) or
/// rolled back.
fn check_op(
    spy: &Spy,
    actor: &mut IndexActor,
    op: impl FnOnce(&mut IndexActor) -> Result<(), String>,
) {
    let before = Spy::db_version(actor);
    op(actor).expect("the op must commit or roll back cleanly, not error");
    let after = Spy::db_version(actor);
    let wakes = spy.drain();
    let bumped = after != before;
    assert_eq!(
        wakes.len(),
        usize::from(bumped),
        "one bump must carry exactly one wake, a silent commit none: {before:?} -> {after:?}, wakes {wakes:?}"
    );
    if bumped {
        // Applied: the wake advertises exactly the committed version and
        // the watermark matches it (the advisory lower bound is equal here
        // because this test thread is ordered after every commit).
        assert_eq!(wakes[0], after);
        assert_eq!(spy.watermark(), after);
    } else {
        // FastPath/RolledBack: silent commit — the watermark never moves.
        let expected = spy
            .recorded()
            .last()
            .copied()
            .unwrap_or(IndexVersion::new(0, 0));
        assert_eq!(
            spy.watermark(),
            expected,
            "a silent commit must not advertise"
        );
    }
    // Wakes are monotone non-decreasing across the whole sequence.
    let recorded = spy.recorded();
    for pair in recorded.windows(2) {
        assert!(
            pair[1].epoch > pair[0].epoch
                || (pair[1].epoch == pair[0].epoch && pair[1].generation >= pair[0].generation),
            "non-monotone publishes: {pair:?}"
        );
    }
}

fn fresh_actor(db_path: &Path, spy: &Spy) -> IndexActor {
    IndexActor::with_generation_signal(db_path, 1, spy.signal.clone())
        .expect("index actor opens with the signal wired")
}

fn apply_note(actor: &mut IndexActor, path: &str, body: &[u8], stamp: i64) -> Result<(), String> {
    actor
        .apply_saved_note(path.as_bytes(), path, body, stamp, stamp + 1)
        .map(|_| ())
}

fn read_logged_generations(conn: &Connection) -> Vec<i64> {
    let mut statement = conn
        .prepare("SELECT DISTINCT generation FROM changes ORDER BY generation")
        .expect("the changes table is part of the schema");
    statement
        .query_map([], |row| row.get::<_, i64>(0))
        .expect("distinct generation scan")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows are readable")
}

// ---------------------------------------------------------------------------
// AC-1: the property test over random op sequences.
// ---------------------------------------------------------------------------

const NOTE_PATHS: [&str; 4] = ["alpha.md", "beta/gamma.md", "delta.md", "epsilon/zeta.md"];
const NOTE_BODIES: [&str; 3] = [
    "# One\nPlain body.\n",
    "# Two\nA link to [[alpha]] and a #tag.\n",
    "# Three\nAlias body with [[beta/gamma|a label]].\n",
];
const ATTACHMENT_PATHS: [&str; 2] = ["img/one.png", "img/two.pdf"];

#[derive(Debug, Clone)]
enum Op {
    /// apply_saved_note(path, body) with a per-step stamp.
    Save(usize, usize),
    /// apply_unparsed_note(path, size) — large/teardown rows.
    Unparsed(usize, u64),
    /// apply_saved_attachment(path).
    Attachment(usize),
    /// remove_note_path(path).
    RemoveNote(usize),
    /// remove_attachment_path(path).
    RemoveAttachment(usize),
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0..NOTE_PATHS.len(), 0..NOTE_BODIES.len()).prop_map(|(path, body)| Op::Save(path, body)),
        (0..NOTE_PATHS.len(), 1u64..8).prop_map(|(path, size)| Op::Unparsed(path, size)),
        (0..ATTACHMENT_PATHS.len()).prop_map(Op::Attachment),
        (0..NOTE_PATHS.len()).prop_map(Op::RemoveNote),
        (0..ATTACHMENT_PATHS.len()).prop_map(Op::RemoveAttachment),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 500,
        ..ProptestConfig::default()
    })]

    /// Every `Applied` step publishes exactly one wake carrying the
    /// committed version; every `FastPath`/rolled-back step publishes zero;
    /// the wakes stay monotone; and every published generation exists in
    /// the change log (bump-and-rows-in-one-transaction, §4.3).
    #[test]
    fn applied_ops_publish_exactly_once_and_silent_commits_never_publish(
        ops in prop::collection::vec(op_strategy(), 1..=30),
    ) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.sqlite3");
        drop(open_or_init(&db).unwrap());
        let spy = Spy::new();
        let mut actor = fresh_actor(&db, &spy);

        for (stamp, op) in (2i64..).zip(ops.into_iter()) {
            match op {
                Op::Save(path, body) => {
                    check_op(&spy, &mut actor, move |actor| {
                        apply_note(actor, NOTE_PATHS[path], NOTE_BODIES[body].as_bytes(), stamp)
                    });
                }
                Op::Unparsed(path, size) => {
                    check_op(&spy, &mut actor, move |actor| {
                        actor
                            .apply_unparsed_note(
                                NOTE_PATHS[path].as_bytes(),
                                NOTE_PATHS[path],
                                size,
                                stamp,
                                stamp + 1,
                            )
                            .map(|_| ())
                    });
                }
                Op::Attachment(path) => {
                    check_op(&spy, &mut actor, move |actor| {
                        actor
                            .apply_saved_attachment(
                                ATTACHMENT_PATHS[path].as_bytes(),
                                ATTACHMENT_PATHS[path],
                                Some(b"bytes".as_slice()),
                                5,
                                stamp,
                                stamp + 1,
                                true,
                            )
                            .map(|_| ())
                    });
                }
                Op::RemoveNote(path) => {
                    check_op(&spy, &mut actor, move |actor| {
                        actor.remove_note_path(NOTE_PATHS[path].as_bytes()).map(|_| ())
                    });
                }
                Op::RemoveAttachment(path) => {
                    check_op(&spy, &mut actor, move |actor| {
                        actor
                            .remove_attachment_path(ATTACHMENT_PATHS[path].as_bytes())
                            .map(|_| ())
                    });
                }
            }
        }

        // The feed leg: each advertised generation owns at least one change
        // row, and the logged set equals the published set exactly — the
        // spine writes bump + rows + commit as one transaction, and FR-1
        // forbids a bumped generation without a publish.
        let published: Vec<u64> = spy.recorded().iter().map(|version| version.generation).collect();
        let logged: Vec<u64> = read_logged_generations(actor.writer().conn())
            .into_iter()
            .map(|generation| {
                u64::try_from(generation).expect("generations are non-negative counters")
            })
            .collect();
        prop_assert_eq!(
            &published,
            &logged,
            "published generations must equal the logged generations"
        );
    }
}

// ---------------------------------------------------------------------------
// Named S1 scenarios.
// ---------------------------------------------------------------------------

/// A rename storm: several `Applied` steps inside one change-log window.
/// Each bump publishes exactly once; the whole window is splicable from the
/// typed feed (every published generation owns rows), which is what the
/// mirror relies on when a coalesced wake skips intermediate values.
#[test]
fn rename_storm_publishes_every_applied_generation() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let spy = Spy::new();
    let mut actor = fresh_actor(&db, &spy);

    // old -> new rename: apply the new path, remove the old, re-add the old
    // — three consecutive Applied commits at generations 1, 2, 3.
    check_op(&spy, &mut actor, |actor| {
        apply_note(actor, "old.md", b"# Old\nBody.\n", 10)
    });
    check_op(&spy, &mut actor, |actor| {
        apply_note(actor, "new.md", b"# Old\nBody.\n", 11)
    });
    check_op(&spy, &mut actor, |actor| {
        actor.remove_note_path(b"old.md").map(|_| ())
    });

    let published = spy.recorded();
    assert_eq!(
        published,
        vec![
            IndexVersion::new(1, 1),
            IndexVersion::new(1, 2),
            IndexVersion::new(1, 3),
        ],
        "the rename storm publishes each committed generation once, in order"
    );
    let logged = read_logged_generations(actor.writer().conn());
    assert_eq!(
        logged,
        vec![1, 2, 3],
        "the whole published window is present in the change feed"
    );
}

/// Id-conflict churn: the FR-1 flip rows (another note's `id_conflict`
/// flag changing as a group winner moves) ride inside the transaction of
/// the generation that already publishes. They never add a second wake and
/// never swallow the bump's wake.
#[test]
fn id_conflict_churn_still_publishes_once_per_bump() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let spy = Spy::new();
    let mut actor = fresh_actor(&db, &spy);

    let with_id = |id: &str| format!("---\nid: {id}\n---\n# Body\nLink to [[missing]].\n");

    check_op(&spy, &mut actor, |actor| {
        apply_note(actor, "first.md", with_id("dup").as_bytes(), 10)
    });
    // The duplicate id flips `first.md`'s id_conflict flag (FR-1 rows) as
    // part of this same bump: exactly one wake, not two.
    check_op(&spy, &mut actor, |actor| {
        apply_note(actor, "second.md", with_id("dup").as_bytes(), 11)
    });
    // Conflict clears when the second note moves off the shared id.
    check_op(&spy, &mut actor, |actor| {
        apply_note(actor, "second.md", with_id("other").as_bytes(), 12)
    });
    // And the flip rows on removal of the conflicting sibling.
    check_op(&spy, &mut actor, |actor| {
        actor.remove_note_path(b"second.md").map(|_| ())
    });

    let published = spy.recorded();
    assert_eq!(
        published,
        vec![
            IndexVersion::new(1, 1),
            IndexVersion::new(1, 2),
            IndexVersion::new(1, 3),
            IndexVersion::new(1, 4),
        ],
        "flip-row commits publish exactly once, with their bump"
    );
    assert_eq!(
        read_logged_generations(actor.writer().conn()),
        vec![1, 2, 3, 4]
    );
}

/// `FastPath` commits stay silent: a re-apply whose content hash matches
/// the indexed row commits stat columns only — no bump, no change rows, no
/// publish (graph-invisible by contract, pool.rs `FastPath`).
#[test]
fn fast_path_commits_stay_silent() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let spy = Spy::new();
    let mut actor = fresh_actor(&db, &spy);

    check_op(&spy, &mut actor, |actor| {
        apply_note(actor, "quiet.md", b"# Quiet\nBody.\n", 10)
    });
    assert_eq!(spy.watermark(), IndexVersion::new(1, 1));
    // Same bytes, fresh stamps: hash matches, so the commit is a FastPath.
    check_op(&spy, &mut actor, |actor| {
        apply_note(actor, "quiet.md", b"# Quiet\nBody.\n", 99)
    });
    // And again — an unparsed row already matching size+mtime is silent too.
    check_op(&spy, &mut actor, |actor| {
        actor
            .apply_unparsed_note(b"big.bin", "big.bin", 40_000_000, 50, 51)
            .map(|_| ())
    });
    check_op(&spy, &mut actor, |actor| {
        actor
            .apply_unparsed_note(b"big.bin", "big.bin", 40_000_000, 50, 77)
            .map(|_| ())
    });

    assert_eq!(
        spy.recorded(),
        vec![IndexVersion::new(1, 1), IndexVersion::new(1, 2)],
        "only the two bumping commits (the note apply and the unparsed insert) publish"
    );
    assert_eq!(spy.watermark(), IndexVersion::new(1, 2));
    assert_eq!(Spy::db_version(&actor), IndexVersion::new(1, 2));
    // Removing an absent path rolls back: silent, like FastPath.
    check_op(&spy, &mut actor, |actor| {
        actor.remove_note_path(b"never-indexed.md").map(|_| ())
    });
    assert_eq!(
        spy.recorded().len(),
        2,
        "a rolled-back apply publishes nothing"
    );
}

/// Database recreate: the `GenerationSignal` itself survives (it is the
/// same atomics and channel behind the clone the new actor receives), and
/// the first publish from the recreated database carries the new epoch —
/// which is what makes consumers take the reset path instead of skipping
/// (RFC-0005 §4.9).
#[test]
fn db_recreate_survives_with_the_new_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let spy = Spy::new();

    {
        let mut actor = fresh_actor(&db, &spy);
        check_op(&spy, &mut actor, |actor| {
            apply_note(actor, "era-one.md", b"# Era one\nBody.\n", 10)
        });
        assert_eq!(spy.recorded(), vec![IndexVersion::new(1, 1)]);
    }
    // Recreate the database under the same path: the file (and its WAL
    // sidecars) and the derived search cache are discarded, a fresh schema
    // is seeded, and the host gives it a new epoch — the CLI/doctor
    // recreate shape the graph service must react to (AC-8 later).
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(dir.path().join(format!("index.sqlite3{suffix}")));
    }
    let _ = std::fs::remove_dir_all(dir.path().join("tantivy"));
    drop(open_or_init(&db).unwrap());
    let conn = Connection::open(&db).unwrap();
    set_meta(&conn, MetaKey::Epoch, "7").unwrap();
    drop(conn);

    {
        let mut actor = fresh_actor(&db, &spy);
        check_op(&spy, &mut actor, |actor| {
            apply_note(actor, "era-two.md", b"# Era two\nBody.\n", 20)
        });
        assert_eq!(
            spy.recorded(),
            vec![IndexVersion::new(1, 1), IndexVersion::new(7, 1)],
            "the signal survives the recreate and the next publish carries the new epoch"
        );
        assert_eq!(spy.watermark(), IndexVersion::new(7, 1));
    }
}

/// A pool constructed without a signal (CLI/test default) commits
/// normally — the broadcast is additive and never required for durability.
#[test]
fn signal_less_pool_still_commits() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let mut actor = IndexActor::new(&db, 1).expect("signal-less construction");
    actor
        .apply_saved_note(b"plain.md", "plain.md", b"# Plain\nBody.\n", 10, 11)
        .expect("apply commits");
    assert_eq!(Spy::db_version(&actor), IndexVersion::new(1, 1));
    assert_eq!(actor.current_version().unwrap(), (1, 1));
}

/// A burst of commits with a busy consumer coalesces into at most one
/// queued wake while the watermark keeps the newest committed version —
/// the latest-wins discipline (§4.2) the graph service relies on, proved
/// through the real spine rather than hand-built publishes.
#[test]
fn burst_commits_coalesce_but_the_watermark_stays_current() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let spy = Spy::new();
    let mut actor = fresh_actor(&db, &spy);

    // 50 saves with the queue left undrained (a busy pump).
    let mut bumped = 0u64;
    for step in 0..50u64 {
        let before = Spy::db_version(&actor);
        let body = format!("# Storm {step}\nUnique body {step}.\n");
        apply_note(&mut actor, "storm.md", body.as_bytes(), 100 + step as i64).unwrap();
        let after = Spy::db_version(&actor);
        assert_eq!(
            after.generation,
            before.generation + 1,
            "every save bumps once"
        );
        bumped += 1;
    }
    assert_eq!(bumped, 50);
    // The queue holds at most one coalesced wake…
    let wakes = spy.drain();
    let wake_count = wakes.len();
    assert!(
        wake_count <= 1,
        "bounded(1) coalescing, got {wake_count} wakes"
    );
    // …and the watermark carries the newest committed generation.
    assert_eq!(spy.watermark(), IndexVersion::new(1, 50));
}
