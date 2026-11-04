//! RFC-0004 S3 — seeded-drift fixtures (§6.2.5, exit: "doctor reports
//! seeded drift"). These tests exercise the exact primitives the
//! `flavite-api` doctor surfaces read-only (`resolver-parity` via
//! [`re_resolve_preview`], `change-log-health` via [`change_log_health`]),
//! plus the explicit full-verify backstop forcing (D13). The API-side
//! wiring itself is covered by the doctor suite in `flavite-api`.

use flavite_index::changelog::{ChangeLogHealth, MIN_LOG_RETENTION, change_log_health};
use flavite_index::resolver::{LinkParityMismatch, re_resolve_preview};
use flavite_index::{IndexActor, open_or_init};
use rusqlite::{Connection, params};
use tempfile::{NamedTempFile, tempdir};

fn preview(conn: &Connection) -> Vec<LinkParityMismatch> {
    re_resolve_preview(conn).expect("parity preview is read-only and must succeed")
}

fn health(conn: &Connection) -> ChangeLogHealth {
    change_log_health(conn).expect("health read must succeed")
}

/// Healthy index: the preview finds nothing and the window is contract-clean.
#[test]
fn healthy_index_passes_parity_and_health() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    {
        let mut actor = IndexActor::new(&db, 1).unwrap();
        actor
            .apply_saved_note(b"a.md", "a.md", b"# Alpha", 1, 1)
            .unwrap();
        actor
            .apply_saved_note(b"b.md", "b.md", b"See [[a]] ![[c.png]]", 2, 2)
            .unwrap();
        actor
            .apply_saved_attachment(
                b"c.png",
                "c.png",
                Some(b"\x89PNG".as_slice()),
                4,
                2,
                2,
                true,
            )
            .unwrap();
        assert!(preview(actor.writer().conn()).is_empty());
        let actor_health = health(actor.writer().conn());
        assert!(!actor_health.is_corrupt());
        assert!(!actor_health.retention_violated());
    }
    let conn = Connection::open(&db).unwrap();
    assert!(preview(&conn).is_empty());
    assert!(!health(&conn).is_corrupt());
}

/// Seeded drift case 1 (§6.2.5): corrupt exactly one `links.res_state`
/// via SQL; the parity preview must report exactly that row — no more,
/// no less — with the stored vs projected triples.
#[test]
fn seeded_res_state_drift_is_reported_exactly() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    {
        let mut actor = IndexActor::new(&db, 1).unwrap();
        actor
            .apply_saved_note(b"a.md", "a.md", b"# Alpha target", 1, 1)
            .unwrap();
        actor
            .apply_saved_note(b"b.md", "b.md", b"See [[a]]", 2, 2)
            .unwrap();
        assert!(preview(actor.writer().conn()).is_empty());
        let updated = actor
            .writer()
            .conn()
            .execute(
                "UPDATE links SET res_state='unresolved' WHERE res_state='resolved'",
                [],
            )
            .unwrap();
        assert_eq!(updated, 1, "fixture must corrupt exactly one row");
        let mismatches = preview(actor.writer().conn());
        assert_eq!(mismatches.len(), 1, "seeded drift reports exactly itself");
        let mismatch = &mismatches[0];
        assert_eq!(mismatch.target_key, "a");
        // The corruption touched only the state string: the destination
        // pair survived, so stored/projected differ in `res_state` alone.
        assert!(mismatch.stored.0.is_some(), "dst_note must be intact");
        assert_eq!(mismatch.stored.2, "unresolved");
        assert_eq!(mismatch.projected.2, "resolved");
        assert_eq!(mismatch.projected.0, mismatch.stored.0);
        assert_eq!(mismatch.projected.1, mismatch.stored.1);
    }
}

/// Seeded drift case 2: the comparison covers the full stored triple — a
/// destination corruption (not just the state string) is reported too.
#[test]
fn seeded_destination_drift_is_reported() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    {
        let mut actor = IndexActor::new(&db, 1).unwrap();
        actor
            .apply_saved_note(b"a.md", "a.md", b"# Alpha", 1, 1)
            .unwrap();
        actor
            .apply_saved_note(b"b.md", "b.md", b"See [[a]]", 2, 2)
            .unwrap();
        actor
            .writer()
            .conn()
            .execute(
                "UPDATE links SET dst_note=NULL, res_state='unresolved' WHERE dst_note IS NOT NULL",
                [],
            )
            .unwrap();
        let mismatches = preview(actor.writer().conn());
        assert_eq!(mismatches.len(), 1);
        assert_eq!(mismatches[0].stored, (None, None, "unresolved".into()));
        assert_eq!(mismatches[0].projected.2, "resolved");
        assert!(mismatches[0].projected.0.is_some());
    }
}

/// Seeded drift case 3 (§6.2.5 `change-log-health`): a `log_floor`
/// corrupted past the generation (window nonsense) and below-retention
/// advances are both flagged; rows outside the window are flagged.
#[test]
fn change_log_window_and_retention_violations_are_reported() {
    let temp = NamedTempFile::new().unwrap();
    let db = temp.path().to_path_buf();
    let mut conn = open_or_init(&db).unwrap();
    // Long enough log that the retention contract binds.
    let top = MIN_LOG_RETENTION + 1000;
    {
        let tx = conn.transaction().unwrap();
        {
            let mut statement = tx
                .prepare("INSERT INTO changes(generation,kind,entity_id) VALUES(?1,'note',?2)")
                .unwrap();
            for generation in 1..=top {
                statement
                    .execute(params![i64::try_from(generation).unwrap(), 1_i64])
                    .unwrap();
            }
        }
        flavite_index::schema::set_meta(
            &tx,
            flavite_index::schema::MetaKey::Generation,
            &top.to_string(),
        )
        .unwrap();
        tx.commit().unwrap();
    }
    // Below-retention advance: the floor keeps only 106 generations.
    {
        let tx = conn.transaction().unwrap();
        flavite_index::schema::set_meta(
            &tx,
            flavite_index::schema::MetaKey::LogFloor,
            &(top - 106).to_string(),
        )
        .unwrap();
        tx.execute(
            "DELETE FROM changes WHERE generation < ?1",
            [i64::try_from(top - 106).unwrap()],
        )
        .unwrap();
        tx.commit().unwrap();
    }
    let snapshot = health(&conn);
    assert!(!snapshot.is_corrupt());
    assert_eq!(snapshot.generation, top);
    assert_eq!(snapshot.retention_generations, 106);
    assert!(
        snapshot.retention_violated(),
        "106-generation retention below {MIN_LOG_RETENTION} must surface"
    );

    // Floor ahead of generation: hard corruption, rows stranded below it.
    flavite_index::schema::set_meta(&conn, flavite_index::schema::MetaKey::LogFloor, "999999")
        .unwrap();
    let snapshot = health(&conn);
    assert!(snapshot.is_corrupt());
    assert!(snapshot.floor_above_generation);
    assert!(snapshot.rows_below_floor > 0);
}

/// D13 explicit trigger (S3): `force_resolver_backstop` makes the *next*
/// structural apply run the whole-vault walk regardless of cadence, the
/// demand is consumed on commit, and equality holds across the forcing.
#[test]
fn forced_backstop_fires_on_next_apply_and_is_consumed() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let mut actor = IndexActor::new(&db, 1).unwrap();
    actor
        .apply_saved_note(b"n0.md", "n0.md", b"# Zero", 1, 1)
        .unwrap();
    let baseline = actor.resolver_diagnostics();
    assert_eq!(baseline.resolve_full_total, 0);
    assert_eq!(baseline.resolve_targeted_total, 1);

    // A normal apply without forcing stays targeted.
    actor
        .apply_saved_note(b"n1.md", "n1.md", b"# One [[n0]]", 2, 2)
        .unwrap();
    let calm = actor.resolver_diagnostics();
    assert_eq!(calm.resolve_full_total, 0);
    assert_eq!(calm.resolve_targeted_total, 2);

    // Forcing (full-verify path) promotes exactly the next apply.
    actor.force_resolver_backstop();
    actor
        .apply_saved_note(b"n2.md", "n2.md", b"# Two [[n1]]", 3, 3)
        .unwrap();
    let forced = actor.resolver_diagnostics();
    assert_eq!(forced.resolve_full_total, 1);
    assert_eq!(
        forced.resolve_targeted_total, 2,
        "forced apply skips the targeted pass"
    );
    assert_eq!(forced.generations_since_full, 1);

    // Consumed: the following apply is targeted again.
    actor
        .apply_saved_note(b"n3.md", "n3.md", b"# Three [[n2]]", 4, 4)
        .unwrap();
    let after = actor.resolver_diagnostics();
    assert_eq!(after.resolve_full_total, 1);
    assert_eq!(after.resolve_targeted_total, 3);
    assert!(preview(actor.writer().conn()).is_empty());

    // §6.7 feed counters track committed applies and submitted rows.
    assert!(after.apply_generations_total >= 4);
    assert!(after.changes_rows_total >= 4);
}

/// S3 repair semantics: a forced demand (full-verify job) rides even an
/// unchanged-content apply. With drift present the walk converts that
/// save into a logged repair apply (generation bump + `outgoing` rows);
/// with a clean vault it consumes the demand without touching the
/// generation at all.
#[test]
fn forced_backstop_repairs_drift_on_unchanged_content() {
    let dir = tempdir().unwrap();
    let db = dir.path().join("index.sqlite3");
    drop(open_or_init(&db).unwrap());
    let mut actor = IndexActor::new(&db, 1).unwrap();
    actor
        .apply_saved_note(b"a.md", "a.md", b"# Alpha target", 1, 1)
        .unwrap();
    actor
        .apply_saved_note(b"b.md", "b.md", b"See [[a]]", 2, 2)
        .unwrap();
    assert!(preview(actor.writer().conn()).is_empty());

    // Seeded drift that no content hash would reveal.
    actor
        .writer()
        .conn()
        .execute(
            "UPDATE links SET res_state='unresolved', dst_note=NULL WHERE target_key='a'",
            [],
        )
        .unwrap();
    assert_eq!(preview(actor.writer().conn()).len(), 1);
    let version_before = actor.current_version().unwrap();

    // Verify-style: forced demand + an unchanged-content re-save.
    actor.force_resolver_backstop();
    let (epoch, generation) = actor
        .apply_saved_note(b"b.md", "b.md", b"See [[a]]", 3, 3)
        .unwrap();
    assert!(
        generation > version_before.1,
        "the repair must own a generation: drift flips are logged (FR-1)"
    );
    assert_eq!(epoch, version_before.0);
    assert!(preview(actor.writer().conn()).is_empty(), "drift repaired");
    let (outgoing, entity_rows): (i64, i64) = actor
        .writer()
        .conn()
        .query_row(
            "SELECT (SELECT COUNT(*) FROM changes WHERE generation=?1 AND kind='outgoing'), \
                    (SELECT COUNT(*) FROM changes WHERE generation=?1 AND kind='note')",
            params![i64::try_from(generation).unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(outgoing, 1, "the repaired source is logged once");
    assert_eq!(entity_rows, 1, "the touched note is refreshed");

    // Clean vault + forced demand: consumed without a generation bump.
    let diagnostics = actor.resolver_diagnostics();
    assert_eq!(diagnostics.resolve_full_total, 1);
    actor.force_resolver_backstop();
    let before = actor.current_version().unwrap();
    let (epoch, generation) = actor
        .apply_saved_note(b"b.md", "b.md", b"See [[a]]", 4, 4)
        .unwrap();
    assert_eq!(
        (epoch, generation),
        before,
        "a clean forced walk must not invent a generation"
    );
    let after = actor.resolver_diagnostics();
    assert_eq!(after.resolve_full_total, 2, "demand consumed");
    assert_eq!(
        after.generations_since_full, 0,
        "cadence restarts from the forced walk"
    );
}
