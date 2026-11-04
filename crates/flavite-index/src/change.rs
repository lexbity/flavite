//! Change-log retention primitives (§4.3, RFC-0004 W-B spine).
//!
//! The typed feed contract lives in [`crate::changelog`]: one sanctioned
//! writer and one typed reader with [`Reset`](crate::changelog::ChangeLogRead::Reset)
//! semantics. This module owns only the *budget* side of the same contract:
//! the log is a bounded cache, so pruning advances `log_floor` and readers
//! that fall below the floor receive a typed `Reset` instead of silently
//! stale success.
//!
//! Retention rule (§4.3): pruning MUST retain at least
//! [`MIN_LOG_RETENTION`] generations
//! unless the [`DEFAULT_LOG_BUDGET`] estimate forces the floor higher
//! (bulk-import storms). When the budget forces a deeper prune, consumers
//! reset and the event is counted — never silent.

use crate::changelog::MIN_LOG_RETENTION;
use crate::schema::{MetaKey, get_meta_u64, set_meta};
use rusqlite::{Connection, Result};

/// Default invalidation log storage budget (64 MiB, §4.14).
pub const DEFAULT_LOG_BUDGET: usize = 64 * 1024 * 1024;

/// Prune the invalidation log down to `max_retention` generations.
///
/// The requested retention is clamped to at least [`MIN_LOG_RETENTION`]:
/// a forgetful caller cannot cut the feed below the contractual floor
/// through this helper. The floor never moves backwards. Deletion and the
/// `log_floor` meta update share one transaction, so readers either see
/// the old floor with all rows or the new floor with the surviving rows —
/// torn retention is impossible.
///
/// # Errors
/// Returns an error when the transaction cannot start, the `DELETE`
/// fails, or metadata cannot be read or written.
pub fn prune(conn: &Connection, max_retention: u64) -> Result<()> {
    let retention = max_retention.max(MIN_LOG_RETENTION);
    let current_generation = get_meta_u64(conn, MetaKey::Generation)?.unwrap_or(0);
    let log_floor = get_meta_u64(conn, MetaKey::LogFloor)?.unwrap_or(0);

    let new_floor = current_generation.saturating_sub(retention).max(log_floor);

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "DELETE FROM changes WHERE generation < ?",
        [new_floor.cast_signed()],
    )?;
    set_meta(&tx, MetaKey::LogFloor, &new_floor.to_string())?;
    tx.commit()?;

    Ok(())
}

/// Estimate whether the log exceeds `max_size` bytes.
///
/// The estimate is deliberately conservative: 32 bytes per row, which is
/// cheap and errs toward pruning. Budget-forced pruning beyond
/// [`MIN_LOG_RETENTION`] is a consumer-visible reset event and MUST be
/// counted by its caller (§4.3).
///
/// # Errors
/// Returns an error when the row count cannot be read.
pub fn needs_prune(conn: &Connection, max_size: usize) -> Result<bool> {
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))?;

    let estimated_size = count.cast_unsigned() * 32;

    Ok(estimated_size > max_size as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changelog::{ChangeKind, ChangeLogWriter};
    use crate::schema::create_schema;
    use tempfile::NamedTempFile;

    fn seeded(generations: u64) -> (tempfile::NamedTempFile, Connection) {
        let temp = NamedTempFile::new().unwrap();
        let mut conn = Connection::open(temp.path()).unwrap();
        create_schema(&mut conn).unwrap();
        let tx = conn.transaction().unwrap();
        for generation in 1..=generations {
            ChangeLogWriter::record(
                &tx,
                generation,
                ChangeKind::Note,
                i64::try_from(generation).unwrap(),
            )
            .unwrap();
        }
        set_meta(&tx, MetaKey::Generation, &generations.to_string()).unwrap();
        tx.commit().unwrap();
        (temp, conn)
    }

    #[test]
    fn prune_clamps_retention_to_the_contractual_floor() {
        let (_temp, conn) = seeded(MIN_LOG_RETENTION + 1000);
        prune(&conn, 10).unwrap();
        let floor = get_meta_u64(&conn, MetaKey::LogFloor).unwrap().unwrap();
        assert_eq!(floor, 1000);
        let retained: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM changes WHERE generation >= ?1",
                [floor.cast_signed()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(u64::try_from(retained).unwrap(), MIN_LOG_RETENTION + 1);
    }

    #[test]
    fn prune_never_moves_the_floor_backwards() {
        let (_temp, conn) = seeded(MIN_LOG_RETENTION + 1000);
        prune(&conn, 10).unwrap();
        let first_floor = get_meta_u64(&conn, MetaKey::LogFloor).unwrap().unwrap();
        prune(&conn, MIN_LOG_RETENTION * 4).unwrap();
        let second_floor = get_meta_u64(&conn, MetaKey::LogFloor).unwrap().unwrap();
        assert_eq!(second_floor, first_floor);
    }

    #[test]
    fn needs_prune_uses_the_conservative_row_estimate() {
        let (_temp, conn) = seeded(10);
        assert!(needs_prune(&conn, 0).unwrap());
        assert!(!needs_prune(&conn, DEFAULT_LOG_BUDGET).unwrap());
    }
}
