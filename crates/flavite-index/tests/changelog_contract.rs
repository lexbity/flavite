//! RFC-0004 S1 — the change-feed contract (§4.3, FR-1, FR-4).
//!
//! These tests are the spine's acceptance gate: the `changes` log must be a
//! complete, typed feed. The retired feed returned an empty success on an
//! epoch mismatch (RFC-0004 C6 — the trap); test 1 asserts the trap is
//! gone. Test 7 asserts the C4 gap is closed: re-resolution flips
//! of *other* notes' links now emit `outgoing(src)` rows.
//!
//! Fixtures 4–8 seed the table with direct SQL on purpose: seeding the
//! state-under-test is a legitimate exception to the writer-helper rule,
//! and the grep gate scopes to `src/`.

use flavite_index::change::{DEFAULT_LOG_BUDGET, needs_prune, prune};
use flavite_index::changelog::{
    ChangeKind, ChangeLogRead, ChangeLogReader, ChangeLogWriter, MIN_LOG_RETENTION, ResetReason,
};
use flavite_index::schema::{MetaKey, get_meta_u64, set_meta};
use flavite_index::{IndexActor, IndexVersion, open_or_init};
use rusqlite::{Connection, params};
use tempfile::{NamedTempFile, tempdir};

fn fresh_db() -> (NamedTempFile, Connection) {
    let temp = NamedTempFile::new().unwrap();
    let conn = open_or_init(temp.path()).unwrap();
    (temp, conn)
}

/// Case 1: epoch mismatch must be a typed `Reset` — never an empty success.
#[test]
fn epoch_mismatch_returns_reset_not_empty_delta() {
    let (_temp, mut conn) = fresh_db();
    {
        let tx = conn.transaction().unwrap();
        set_meta(&tx, MetaKey::Generation, "1").unwrap();
        ChangeLogWriter::record(&tx, 1, ChangeKind::Note, 7).unwrap();
        tx.commit().unwrap();
    }
    let reader = ChangeLogReader::new(&conn);
    // Even with rows present in the current epoch, a foreign-epoch cursor
    // is a reset instruction, not "no changes".
    let read = reader
        .read_since(IndexVersion::new(99, 1))
        .expect("typed reset is not an error");
    assert_eq!(
        read,
        ChangeLogRead::Reset {
            reason: ResetReason::EpochChanged,
        }
    );
}

/// Case 2: a cursor below the retained log floor must be a typed `Reset`.
#[test]
fn floor_overrun_returns_reset() {
    let (_temp, conn) = fresh_db();
    set_meta(&conn, MetaKey::Generation, "5").unwrap();
    set_meta(&conn, MetaKey::LogFloor, "3").unwrap();
    let read = ChangeLogReader::new(&conn)
        .read_since(IndexVersion::new(1, 2))
        .unwrap();
    assert_eq!(
        read,
        ChangeLogRead::Reset {
            reason: ResetReason::BelowLogFloor,
        }
    );
}

/// Case 3: a cursor ahead of the current generation is an error, not a reset.
#[test]
fn future_cursor_is_an_error() {
    let (_temp, conn) = fresh_db();
    set_meta(&conn, MetaKey::Generation, "1").unwrap();
    let error = ChangeLogReader::new(&conn)
        .read_since(IndexVersion::new(1, 7))
        .expect_err("a future cursor must not read as a delta or reset");
    assert!(
        error.contains("cursor from the future"),
        "unexpected error text: {error}"
    );
}

/// Case 4: duplicate `(generation, kind, entity_id)` writes are
/// suppressed, not fatal — a plain `INSERT` of a duplicate would abort the transaction.
#[test]
fn duplicate_records_collapse_to_one_row() {
    let (_temp, mut conn) = fresh_db();
    let tx = conn.transaction().unwrap();
    ChangeLogWriter::record(&tx, 1, ChangeKind::Note, 42).unwrap();
    ChangeLogWriter::record(&tx, 1, ChangeKind::Note, 42).unwrap();
    ChangeLogWriter::record_all(&tx, 1, &[(ChangeKind::Note, 42), (ChangeKind::Note, 42)]).unwrap();
    let count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM changes WHERE generation=1 AND kind='note' AND entity_id=42",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    tx.commit().unwrap();
}

/// Case 5: `'note'` and `'outgoing'` rows for the same id and
/// generation coexist (§4.6: same id, different kind).
#[test]
fn note_and_outgoing_same_id_coexist() {
    let (_temp, mut conn) = fresh_db();
    let tx = conn.transaction().unwrap();
    set_meta(&tx, MetaKey::Generation, "3").unwrap();
    ChangeLogWriter::record(&tx, 3, ChangeKind::Note, 9).unwrap();
    ChangeLogWriter::record(&tx, 3, ChangeKind::Outgoing, 9).unwrap();
    let count: i64 = tx
        .query_row(
            "SELECT COUNT(*) FROM changes WHERE generation=3 AND entity_id=9",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 2);
    tx.commit().unwrap();
    let read = ChangeLogReader::new(&conn)
        .read_since(IndexVersion::new(1, 0))
        .unwrap();
    match read {
        ChangeLogRead::Delta { records, to } => {
            assert_eq!(
                records
                    .iter()
                    .map(|record| (record.kind, record.entity_id))
                    .collect::<Vec<_>>(),
                vec![(ChangeKind::Note, 9), (ChangeKind::Outgoing, 9)]
            );
            assert_eq!(to, IndexVersion::new(1, 3));
        }
        ChangeLogRead::Reset { reason } => panic!("expected delta, got reset: {reason:?}"),
    }
}

/// Case 6: generation atomicity — rows become visible only when the
/// writer transaction commits, and the meta bump travels with them.
#[test]
fn rows_are_invisible_until_the_writer_commits() {
    let temp = NamedTempFile::new().unwrap();
    let path = temp.path().to_path_buf();
    let mut writer = open_or_init(&path).unwrap();
    let reader_conn = Connection::open(&path).unwrap();
    let reader = ChangeLogReader::new(&reader_conn);
    let cursor = reader.current_cursor().unwrap();
    assert_eq!(cursor, IndexVersion::new(1, 0));
    {
        let tx = writer.transaction().unwrap();
        set_meta(&tx, MetaKey::Generation, "1").unwrap();
        ChangeLogWriter::record(&tx, 1, ChangeKind::Note, 42).unwrap();
        let pending = reader.read_since(cursor).unwrap();
        assert_eq!(
            pending,
            ChangeLogRead::Delta {
                records: Vec::new(),
                to: IndexVersion::new(1, 0),
            },
            "uncommitted generations must not leak to readers"
        );
        tx.commit().unwrap();
    }
    let after = reader.read_since(cursor).unwrap();
    match after {
        ChangeLogRead::Delta { records, to } => {
            assert_eq!(
                records,
                vec![flavite_index::ChangeRecord {
                    generation: 1,
                    kind: ChangeKind::Note,
                    entity_id: 42,
                }]
            );
            assert_eq!(to, IndexVersion::new(1, 1));
        }
        ChangeLogRead::Reset { reason } => panic!("expected delta, got reset: {reason:?}"),
    }
}

/// Case 7: the C4 gap, closed. Deleting note A flips B's link and MUST
/// log exactly one `'outgoing'(B)` row in the new generation.
#[test]
fn deleted_note_flip_logs_the_inbound_source() {
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
    let id_of = |actor: &IndexActor, path: &[u8]| -> i64 {
        actor
            .writer()
            .conn()
            .query_row(
                "SELECT note_id FROM notes WHERE path_native=?1",
                [path],
                |row| row.get(0),
            )
            .unwrap()
    };
    let a_id = id_of(&actor, b"a.md");
    let b_id = id_of(&actor, b"b.md");
    let state: String = actor
        .writer()
        .conn()
        .query_row("SELECT res_state FROM links WHERE src=?1", [b_id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(
        state, "resolved",
        "B's link must target A before the delete"
    );

    let cursor = ChangeLogReader::new(actor.writer().conn())
        .current_cursor()
        .unwrap();
    assert!(actor.remove_note_path(b"a.md").unwrap());

    let read = ChangeLogReader::new(actor.writer().conn())
        .read_since(cursor)
        .unwrap();
    let ChangeLogRead::Delta { records, to } = read else {
        panic!("a delete on the current epoch is a delta, not a reset");
    };
    assert_eq!(to.epoch, cursor.epoch);
    assert_eq!(to.generation, cursor.generation + 1);
    assert_eq!(
        records
            .iter()
            .map(|record| (record.kind, record.entity_id))
            .collect::<Vec<_>>(),
        vec![(ChangeKind::Note, a_id), (ChangeKind::Outgoing, b_id)],
        "the flipped source B must be logged exactly once in the delete generation"
    );
    assert_eq!(
        records
            .iter()
            .filter(|record| record.kind == ChangeKind::Outgoing)
            .count(),
        1
    );
    let state: String = actor
        .writer()
        .conn()
        .query_row("SELECT res_state FROM links WHERE src=?1", [b_id], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(state, "unresolved");
}

/// Case 8: pruning honors `MIN_LOG_RETENTION` — a small requested
/// retention still keeps the contractual floor, the floor never moves backwards,
/// and readers below the new floor get a typed `Reset`.
#[test]
fn prune_honors_min_log_retention() {
    let (_temp, mut conn) = fresh_db();
    {
        // Fixture seeding writes generations directly (see module header).
        let tx = conn.transaction().unwrap();
        let top = MIN_LOG_RETENTION + 904;
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
        set_meta(&tx, MetaKey::Generation, &top.to_string()).unwrap();
        tx.commit().unwrap();
    }
    prune(&conn, 10).unwrap();
    let floor = get_meta_u64(&conn, MetaKey::LogFloor).unwrap().unwrap();
    assert_eq!(
        floor, 904,
        "requested 10 must be clamped to {MIN_LOG_RETENTION}"
    );
    let removed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM changes WHERE generation < ?1",
            [floor.cast_signed()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(removed, 0);
    let retained: i64 = conn
        .query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(u64::try_from(retained).unwrap(), MIN_LOG_RETENTION + 1);

    // A below-floor cursor is now a typed reset through the real reader.
    let read = ChangeLogReader::new(&conn)
        .read_since(IndexVersion::new(1, floor - 1))
        .unwrap();
    assert_eq!(
        read,
        ChangeLogRead::Reset {
            reason: ResetReason::BelowLogFloor,
        }
    );

    // A generous retention must not move the floor backwards.
    prune(&conn, MIN_LOG_RETENTION * 4).unwrap();
    assert_eq!(
        get_meta_u64(&conn, MetaKey::LogFloor).unwrap().unwrap(),
        floor
    );

    // The budget estimate keeps its conservative row-count semantics.
    assert!(needs_prune(&conn, 0).unwrap());
    assert!(!needs_prune(&conn, DEFAULT_LOG_BUDGET).unwrap());
}
