//! The typed change feed (§4.3, RFC-0004 W-B spine).
//!
//! The `changes` table is the index's invalidation log: every structural
//! mutation that can change any link row's resolution MUST, within its own
//! transaction and generation, record
//!
//! 1. a row for the mutated entity (`note`/`attachment`), and
//! 2. an `outgoing(src)` row for every source note whose link rows changed
//!    as a result of re-resolution.
//!
//! # Writer contract ([`ChangeLogWriter`])
//!
//! The index writer connection is the sole producer (one vault actor
//! thread). Writes go through [`ChangeLogWriter::record`] /
//! [`ChangeLogWriter::record_all`] only — raw `INSERT INTO changes` is a
//! gate-enforced error. Rows use `INSERT OR IGNORE`: `(generation, kind,
//! entity_id)` is the primary key, duplicates are legal when an affected
//! source *is* the applied note, and a plain `INSERT` of a duplicate would
//! abort the caller's transaction. The helpers commit nothing; the owning
//! apply path keeps its exact generation-bump discipline.
//!
//! # Consumer contract ([`ChangeLogReader`])
//!
//! [`ChangeLogReader::read_since`] returns either
//! [`ChangeLogRead::Delta`] (typed records in PK order plus a new cursor)
//! or [`ChangeLogRead::Reset`] — never an empty success on epoch mismatch.
//! Consumers MUST treat [`Reset`](ChangeLogRead::Reset) as "full rebuild
//! from a consistent read" (§4.14), never as "no changes". Consumers MUST
//! dedupe `(kind, entity_id)` per generation before acting, and MUST
//! process whole generations atomically per synchronize step: all rows of
//! generation `G` are causally closed once generation `G' > G` is visible
//! (SQLite writer-transaction atomicity).
//!
//! Reader isolation: consumers read on their own connections (WAL). The
//! meta triple (`epoch`, `generation`, `log_floor`) and the row range are
//! read in ONE read transaction, so a consumer can never observe a torn
//! view of the log.
//!
//! # Retention (§4.3)
//!
//! Pruning MUST retain at least [`MIN_LOG_RETENTION`] generations unless
//! the 64 MiB budget forces the floor higher (bulk-import storms); in that
//! case consumers receive [`ResetReason::BelowLogFloor`] and the event is
//! counted. The log is a *cache*, like tantivy: disposable, rebuildable.

use crate::schema::{MetaKey, get_meta_u64};
use crate::types::IndexVersion;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

/// Minimum generations retained by any sanctioned prune, unless budget
/// pressure forces the floor higher (§4.3; the escape hatch is counted,
/// never silent).
pub const MIN_LOG_RETENTION: u64 = 4096;

/// A change record in the invalidation log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeRecord {
    pub generation: i64,
    pub kind: ChangeKind,
    pub entity_id: i64,
}

/// Kind of change: note, attachment, or outgoing (links from a note).
/// The wire strings `note|attachment|outgoing` are the schema CHECK
/// constraint and MUST NOT change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    Note,
    Attachment,
    Outgoing,
}

impl ChangeKind {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Note => "note",
            Self::Attachment => "attachment",
            Self::Outgoing => "outgoing",
        }
    }
}

impl TryFrom<&str> for ChangeKind {
    type Error = String;

    /// Parses the stored wire string. An unknown kind is database
    /// corruption: surface it, never guess a replacement.
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "note" => Ok(Self::Note),
            "attachment" => Ok(Self::Attachment),
            "outgoing" => Ok(Self::Outgoing),
            other => Err(format!("unknown change-log kind: {other}")),
        }
    }
}

/// Sole sanctioned writer for the `changes` log (§4.3, FR-1).
///
/// All calls MUST run on the writer transaction of the apply path that
/// bumped the generation; the helpers commit nothing themselves.
pub struct ChangeLogWriter;

impl ChangeLogWriter {
    /// Record one `(generation, kind, entity_id)` row with `INSERT OR
    /// IGNORE` dedup semantics (the PK makes a plain duplicate `INSERT`
    /// abort the caller's transaction — today's failure mode).
    ///
    /// # Errors
    /// Returns the error text when the generation cannot be represented as
    /// `i64` or when SQLite fails.
    pub fn record(
        tx: &rusqlite::Transaction<'_>,
        generation: u64,
        kind: ChangeKind,
        entity_id: i64,
    ) -> Result<(), String> {
        Self::record_all(tx, generation, std::slice::from_ref(&(kind, entity_id)))
    }

    /// Record many rows in one pass, reusing a single prepared statement.
    /// Duplicate `(generation, kind, entity_id)` tuples collapse into one
    /// row via `INSERT OR IGNORE`.
    ///
    /// # Errors
    /// Returns the error text when the generation cannot be represented as
    /// `i64` or when SQLite fails.
    pub fn record_all(
        tx: &rusqlite::Transaction<'_>,
        generation: u64,
        rows: &[(ChangeKind, i64)],
    ) -> Result<(), String> {
        if rows.is_empty() {
            return Ok(());
        }
        let generation = i64::try_from(generation)
            .map_err(|error| format!("change generation overflow: {error}"))?;
        let mut statement = tx
            .prepare("INSERT OR IGNORE INTO changes(generation,kind,entity_id) VALUES(?1,?2,?3)")
            .map_err(|error| error.to_string())?;
        for (kind, entity_id) in rows {
            statement
                .execute(params![generation, kind.as_str(), entity_id])
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

/// Why a consumer must rebuild from a full consistent read instead of
/// applying a delta (§4.3, §4.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// The cursor's epoch differs from the current epoch (database
    /// recreated).
    EpochChanged,
    /// The cursor's generation predates the retained log floor (pruning
    /// outran the consumer).
    BelowLogFloor,
}

/// A typed change-log read outcome: either an incremental delta plus the
/// new cursor, or an instruction to reset. Never an empty success on an
/// invalid cursor — the trap that retired the pre-RFC-0004 feed (§4.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChangeLogRead {
    /// Rows in `(cursor.generation, current.generation]`, PK order.
    Delta {
        records: Vec<ChangeRecord>,
        to: IndexVersion,
    },
    /// The cursor cannot be advanced by deltas; rebuild from a consistent
    /// full read (§4.14 consumer-reset semantics).
    Reset { reason: ResetReason },
}

/// Typed consumer reader over the change feed. Reads happen on the
/// consumer's own connection (WAL), never on the writer.
pub struct ChangeLogReader<'a> {
    conn: &'a Connection,
}

impl<'a> ChangeLogReader<'a> {
    /// Build a reader bound to one read connection.
    #[must_use]
    pub const fn new(conn: &'a Connection) -> Self {
        Self { conn }
    }

    /// Read everything after `cursor` in ONE read transaction: the meta
    /// triple (`epoch`, `generation`, `log_floor`) plus all rows with
    /// `generation > cursor.generation AND generation <= generation`, in
    /// PK order.
    ///
    /// An epoch mismatch or `cursor.generation < log_floor` returns
    /// `Reset` — never empty records presented as success. A cursor ahead
    /// of the current generation is an error, not a reset.
    ///
    /// # Errors
    /// Returns the error text when metadata or rows cannot be read, or
    /// when the cursor comes from the future relative to the current
    /// epoch.
    pub fn read_since(&self, cursor: IndexVersion) -> Result<ChangeLogRead, String> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let epoch = get_meta_u64(&tx, MetaKey::Epoch)
            .map_err(|error| error.to_string())?
            .unwrap_or(1);
        let generation = get_meta_u64(&tx, MetaKey::Generation)
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        let log_floor = get_meta_u64(&tx, MetaKey::LogFloor)
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        if cursor.epoch != epoch {
            return Ok(ChangeLogRead::Reset {
                reason: ResetReason::EpochChanged,
            });
        }
        if cursor.generation < log_floor {
            return Ok(ChangeLogRead::Reset {
                reason: ResetReason::BelowLogFloor,
            });
        }
        if cursor.generation > generation {
            return Err(format!(
                "cursor from the future: {} > {generation}",
                cursor.generation
            ));
        }
        let records = {
            let mut statement = tx
                .prepare(
                    "SELECT generation, kind, entity_id FROM changes \
                     WHERE generation > ?1 AND generation <= ?2 ORDER BY generation ASC",
                )
                .map_err(|error| error.to_string())?;
            let rows = statement
                .query_map(
                    params![
                        i64::try_from(cursor.generation).map_err(|error| error.to_string())?,
                        i64::try_from(generation).map_err(|error| error.to_string())?,
                    ],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, i64>(2)?,
                        ))
                    },
                )
                .map_err(|error| error.to_string())?;
            let mut records = Vec::new();
            for row in rows {
                let (row_generation, kind, entity_id) = row.map_err(|error| error.to_string())?;
                records.push(ChangeRecord {
                    generation: row_generation,
                    kind: ChangeKind::try_from(kind.as_str())?,
                    entity_id,
                });
            }
            records
        };
        tx.commit().map_err(|error| error.to_string())?;
        Ok(ChangeLogRead::Delta {
            records,
            to: IndexVersion { epoch, generation },
        })
    }

    /// The current consumer cursor `(epoch, generation)`, read atomically.
    ///
    /// # Errors
    /// Returns the error text when metadata cannot be read.
    pub fn current_cursor(&self) -> Result<IndexVersion, String> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|error| error.to_string())?;
        let epoch = get_meta_u64(&tx, MetaKey::Epoch)
            .map_err(|error| error.to_string())?
            .unwrap_or(1);
        let generation = get_meta_u64(&tx, MetaKey::Generation)
            .map_err(|error| error.to_string())?
            .unwrap_or(0);
        tx.commit().map_err(|error| error.to_string())?;
        Ok(IndexVersion { epoch, generation })
    }
}

/// A snapshot of change-log integrity for the doctor's `change-log-health`
/// check (§6.2.5, S3): the meta triple, the retention window, and row
/// placement relative to the window — all read atomically in one read
/// transaction, all read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChangeLogHealth {
    pub epoch: u64,
    pub generation: u64,
    pub log_floor: u64,
    /// Generations retained between `log_floor` and `generation`.
    pub retention_generations: u64,
    /// `log_floor > generation` — the window is nonsense (corruption).
    pub floor_above_generation: bool,
    /// Retention below [`MIN_LOG_RETENTION`] while the log is long enough
    /// to honor it: either a budget-forced advance (a counted, legal-but-
    /// surfaced event, §4.3) or a corrupted floor.
    pub retention_below_contract: bool,
    /// Rows below the floor: `prune` deletes them, so any are leftovers
    /// from out-of-band writes (corruption signal).
    pub rows_below_floor: i64,
    /// Rows ahead of the current generation: impossible under writer
    /// discipline (corruption signal).
    pub rows_ahead_of_generation: i64,
    pub rows_total: i64,
}

impl ChangeLogHealth {
    /// Whether the log violates its hard invariants (window sanity and row
    /// placement). A contract-retention shortfall alone is a warning:
    /// §4.3 makes it legal only as a counted budget event, never silent.
    #[must_use]
    pub const fn is_corrupt(&self) -> bool {
        self.floor_above_generation
            || self.rows_below_floor > 0
            || self.rows_ahead_of_generation > 0
    }

    #[must_use]
    pub const fn retention_violated(&self) -> bool {
        self.retention_below_contract
    }
}

/// Compute [`ChangeLogHealth`] on a read connection in ONE read
/// transaction (§6.2.5: doctor `change-log-health` — floor/generation
/// sanity, `MIN_LOG_RETENTION` honored, row-count sanity).
///
/// # Errors
/// Returns the error text when metadata or row counts cannot be read.
pub fn change_log_health(conn: &Connection) -> Result<ChangeLogHealth, String> {
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| error.to_string())?;
    let epoch = get_meta_u64(&tx, MetaKey::Epoch)
        .map_err(|error| error.to_string())?
        .unwrap_or(1);
    let generation = get_meta_u64(&tx, MetaKey::Generation)
        .map_err(|error| error.to_string())?
        .unwrap_or(0);
    let log_floor = get_meta_u64(&tx, MetaKey::LogFloor)
        .map_err(|error| error.to_string())?
        .unwrap_or(0);
    let count = |where_clause: &str, bound: i64| -> Result<i64, String> {
        tx.query_row(
            &format!("SELECT COUNT(*) FROM changes WHERE {where_clause}"),
            params![bound],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())
    };
    let rows_below_floor = count(
        "generation < ?1",
        i64::try_from(log_floor).map_err(|e| e.to_string())?,
    )?;
    let rows_ahead_of_generation = count(
        "generation > ?1",
        i64::try_from(generation).map_err(|e| e.to_string())?,
    )?;
    let rows_total: i64 = tx
        .query_row("SELECT COUNT(*) FROM changes", [], |row| row.get(0))
        .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())?;
    Ok(ChangeLogHealth {
        epoch,
        generation,
        log_floor,
        retention_generations: generation.saturating_sub(log_floor),
        floor_above_generation: log_floor > generation,
        retention_below_contract: generation >= MIN_LOG_RETENTION
            && generation.saturating_sub(log_floor) < MIN_LOG_RETENTION,
        rows_below_floor,
        rows_ahead_of_generation,
        rows_total,
    })
}
