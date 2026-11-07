//! Durable saved-note replacement coordinator (RFC-0006 S08/D-10/D-11).
//!
//! One sealed preview + selection revision becomes a job. A coordinator
//! worker runs the item pipeline: per-item `IntentDurable` log record
//! BEFORE any side effect, per-note source lease, actor-mediated retained
//! replacement + index apply, then a terminal per-item log record. Every
//! mutating request carries `(job, selection_revision, item, action_epoch)`
//! identity and is replayable: completing the same item with the same
//! action epoch returns the recorded receipt. The vault batch is NOT atomic:
//! canonical-order items have monotonic durable outcomes, Cancel stops
//! admission of the next item (finishing an item past its durable intent
//! point), and startup recovery classifies intents against durable disk
//! hashes without ever auto-continuing untouched items.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::log::{ItemAction, JobLog, LogError, PersistedItemOutcome};
use super::plan::NoteCandidateRecord;
use super::store::{OpStoreIo, ReplaceStore};
use crate::actor::VaultActorHandle;

/// Job phases (typed; never strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    /// Sealed and selectable, no effects started.
    Ready,
    /// Running (or paused mid-run awaiting explicit Resume).
    Applying,
    /// All selected items reached durable intended outcomes.
    Completed,
    /// At least one item refused/failed/conflicted.
    Partial,
    /// Cancel stopped admission before all items ran.
    Cancelled,
    /// Explicitly paused; untouched items await Resume.
    Paused,
    /// An unresolved effect or corrupt/unknown record requires inspection.
    NeedsRecovery,
}

impl JobPhase {
    /// Stable redacted label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Applying => "applying",
            Self::Completed => "completed",
            Self::Partial => "partial",
            Self::Cancelled => "cancelled",
            Self::Paused => "paused",
            Self::NeedsRecovery => "needs-recovery",
        }
    }
}

/// Per-item terminal/effect state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemEffect {
    /// Durable source outcome recorded.
    Saved,
    /// Durable source outcome recorded; index repair pending.
    IndexPending,
    /// Our bytes installed but an external writer's displaced bytes were
    /// retained (never presented as Saved).
    AppliedConflict,
    /// Guard mismatch (disk/buffer/path/grant changed) — final for this
    /// sealed plan; requires a new preview.
    Stale,
    /// The item was not admitted (busy lease / grant denied) — retryable.
    Busy,
    /// The filesystem refused retained replacement; the item cannot be
    /// applied on this host.
    Unsupported,
    /// Effect started but durability unconfirmed; requires reconciliation.
    NeedsRecovery,
    /// The disk already holds the exact after bytes (no marker).
    SatisfiedUnattributed,
}

impl ItemEffect {
    /// Persisted log projection.
    #[must_use]
    pub const fn persisted(self) -> PersistedItemOutcome {
        match self {
            Self::Saved => PersistedItemOutcome::Saved,
            Self::IndexPending => PersistedItemOutcome::IndexPending,
            Self::AppliedConflict => PersistedItemOutcome::AppliedConflict,
            Self::Stale => PersistedItemOutcome::Stale,
            Self::Busy => PersistedItemOutcome::Busy,
            Self::Unsupported => PersistedItemOutcome::Denied,
            Self::NeedsRecovery => PersistedItemOutcome::NeedsRecovery,
            Self::SatisfiedUnattributed => PersistedItemOutcome::SatisfiedUnattributed,
        }
    }
}

/// One queued/selected item.
#[derive(Debug, Clone)]
pub struct JobItem {
    pub record: NoteCandidateRecord,
    /// Current effect; `None` = not yet started.
    pub effect: Option<ItemEffect>,
    /// Action-request key mapped to the last result receipt for
    /// idempotent replay within this session.
    pub receipts: BTreeMap<u128, ItemEffect>,
}

/// Aggregate job status (derived counts; NFR: no count inflation).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct JobTotals {
    pub selected: u64,
    pub saved: u64,
    pub index_pending: u64,
    pub conflicts: u64,
    pub stale: u64,
    pub busy: u64,
    pub unsupported: u64,
    pub needs_recovery: u64,
    pub satisfied_unattributed: u64,
    pub not_started: u64,
}

impl JobTotals {
    /// Recompute totals from item effects (never trusted over the log).
    #[must_use]
    pub fn from_items(items: &[JobItem]) -> Self {
        let mut totals = JobTotals {
            selected: u64::try_from(items.len()).unwrap_or(u64::MAX),
            ..JobTotals::default()
        };
        for item in items {
            match item.effect {
                None => totals.not_started = totals.not_started.saturating_add(1),
                Some(ItemEffect::Saved) => totals.saved = totals.saved.saturating_add(1),
                Some(ItemEffect::IndexPending) => {
                    totals.index_pending = totals.index_pending.saturating_add(1);
                }
                Some(ItemEffect::AppliedConflict) => {
                    totals.conflicts = totals.conflicts.saturating_add(1);
                }
                Some(ItemEffect::Stale) => totals.stale = totals.stale.saturating_add(1),
                Some(ItemEffect::Busy) => totals.busy = totals.busy.saturating_add(1),
                Some(ItemEffect::Unsupported) => {
                    totals.unsupported = totals.unsupported.saturating_add(1);
                }
                Some(ItemEffect::NeedsRecovery) => {
                    totals.needs_recovery = totals.needs_recovery.saturating_add(1);
                }
                Some(ItemEffect::SatisfiedUnattributed) => {
                    totals.satisfied_unattributed = totals.satisfied_unattributed.saturating_add(1);
                }
            }
        }
        totals
    }
}

/// The actor-side per-item apply request.
#[derive(Debug, Clone)]
pub struct ReplaceItemRequest {
    pub job_id: String,
    pub item_id: u64,
    pub path: PathBuf,
    pub expected_before_hash: [u8; 32],
    pub after_bytes: Vec<u8>,
    pub note_key: Vec<u8>,
}

/// The actor-side per-item outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceItemOutcome {
    /// Durable source replaced; `conflict` reports a displaced mismatch.
    Replaced { conflict: bool },
    /// The disk already holds the exact after bytes (no overwrite).
    SatisfiedUnattributed,
    /// Guard mismatch before any effect.
    Stale,
    /// The note is missing.
    Missing,
    /// The filesystem refuses retained replacement.
    Unsupported,
    /// Durable source ok; index apply pending repair.
    IndexPending,
}

/// Coordinator context: everything a job worker needs beyond the Session
/// driving-turn object.
#[derive(Clone)]
pub struct CoordinatorCtx {
    pub actor: VaultActorHandle,
    pub root: PathBuf,
    pub data_dir: Option<PathBuf>,
    pub vault_uuid: uuid::Uuid,
    pub leases: Arc<crate::source_leases::SourceLeaseRegistry>,
}

impl CoordinatorCtx {
    /// Build from a live Session.
    #[must_use]
    pub fn from_session(session: &crate::session::Session) -> Self {
        session.coordinator_ctx()
    }
}

/// Selection persistence: the first Apply freezes the selection; the ids
/// are stored under the job directory so Resume can restore them.
pub fn write_selection(
    io: &dyn OpStoreIo,
    store: &ReplaceStore,
    job_id: &str,
    ids: &[u64],
) -> Result<(), String> {
    let dir = store.base().join(job_id);
    io.mkdir_all(&dir)?;
    let mut bytes = Vec::with_capacity(8 + ids.len() * 8);
    bytes.extend_from_slice(&(ids.len() as u64).to_le_bytes());
    for id in ids {
        bytes.extend_from_slice(&id.to_le_bytes());
    }
    let path = dir.join("selection.bin");
    io.write_new(&path, &bytes)?;
    io.fsync_file(&path)?;
    io.fsync_dir(&dir)
}

/// Read a persisted selection (`None` when absent).
pub fn read_selection(
    io: &dyn OpStoreIo,
    store: &ReplaceStore,
    job_id: &str,
) -> Result<Option<Vec<u64>>, String> {
    let path = store.base().join(job_id).join("selection.bin");
    let Some(bytes) = io.read(&path)? else {
        return Ok(None);
    };
    if bytes.len() < 8 || (bytes.len() - 8) % 8 != 0 {
        return Err("selection record is corrupt".to_owned());
    }
    let count = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
    let mut ids = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for chunk in bytes[8..].as_chunks::<8>().0 {
        ids.push(u64::from_le_bytes(*chunk));
    }
    if u64::try_from(ids.len()).unwrap_or(u64::MAX) != count {
        return Err("selection count mismatch".to_owned());
    }
    Ok(Some(ids))
}

/// Run the apply loop for one job. `items` are already filtered to the
/// frozen selection in canonical order; items with a recorded effect are
/// skipped (idempotent resume). Returns the terminal phase plus recomputed
/// totals.
#[allow(clippy::too_many_arguments)]
pub fn run_apply(
    ctx: &CoordinatorCtx,
    job_id: &str,
    items: &mut [JobItem],
    cancel: &AtomicBool,
    log: &mut JobLog,
    store: &ReplaceStore,
    io: &dyn OpStoreIo,
) -> (JobPhase, JobTotals) {
    let mut phase = JobPhase::Applying;
    for item in items.iter_mut() {
        if item.effect.is_some() {
            continue;
        }
        if cancel.load(Ordering::Relaxed) {
            phase = JobPhase::Cancelled;
            break;
        }
        let item_id = item.record.item_id;
        // 1. Lease (non-replacing). Busy is a deferred attempt, not an
        // effect; the item stays not-started (`effect: None`) and is
        // retryable by Resume.
        let relative = Path::new(&item.record.path);
        let lease_key = relative.as_os_str().as_encoded_bytes().to_vec();
        let Ok(lease) = ctx
            .leases
            .acquire(lease_key, crate::source_leases::LeaseOwner::Replace)
        else {
            continue;
        };
        // 2. Durable intent BEFORE any side effect.
        if let Err(error) = log.append(item_id, ItemAction::IntentDurable) {
            ctx.leases.release(&lease);
            item.effect = Some(ItemEffect::NeedsRecovery);
            tracing::warn!(job_id, item_id, ?error, "replace intent log failed");
            phase = JobPhase::NeedsRecovery;
            break;
        }
        // 3. Effect through the actor (single-threaded with saves/renames).
        let request = ReplaceItemRequest {
            job_id: job_id.to_owned(),
            item_id,
            path: ctx.root.join(&item.record.path),
            expected_before_hash: decode_hash(&item.record.disk_hash),
            after_bytes: store
                .read_payload(io, job_id, &item.record.after)
                .ok()
                .flatten()
                .unwrap_or_default(),
            note_key: hex::decode(&item.record.path_native).unwrap_or_default(),
        };
        let effect = match ctx.actor.apply_replace_item(request) {
            Ok(outcome) => match outcome {
                ReplaceItemOutcome::Replaced { conflict } => {
                    if conflict {
                        ItemEffect::AppliedConflict
                    } else {
                        ItemEffect::Saved
                    }
                }
                ReplaceItemOutcome::SatisfiedUnattributed => ItemEffect::SatisfiedUnattributed,
                ReplaceItemOutcome::Stale | ReplaceItemOutcome::Missing => ItemEffect::Stale,
                ReplaceItemOutcome::Unsupported => ItemEffect::Unsupported,
                ReplaceItemOutcome::IndexPending => ItemEffect::IndexPending,
            },
            Err(_) => ItemEffect::NeedsRecovery,
        };
        // 4. Terminal record BEFORE the in-memory effect is visible. A
        // record failure leaves an unresolved effect (never a claim of a
        // durable terminal).
        if let Err(error) = log.append(
            item_id,
            ItemAction::Outcome {
                outcome: effect.persisted(),
            },
        ) {
            tracing::warn!(job_id, item_id, ?error, "replace outcome log failed");
            item.effect = Some(ItemEffect::NeedsRecovery);
            phase = JobPhase::NeedsRecovery;
            ctx.leases.release(&lease);
            break;
        }
        item.effect = Some(effect);
        ctx.leases.release(&lease);
        if effect == ItemEffect::NeedsRecovery {
            phase = JobPhase::NeedsRecovery;
            break;
        }
    }
    let totals = JobTotals::from_items(items);
    if phase != JobPhase::NeedsRecovery {
        let any_nonterminal = items
            .iter()
            .any(|item| matches!(item.effect, Some(ItemEffect::NeedsRecovery) | None));
        phase = if any_nonterminal {
            JobPhase::Partial
        } else if totals.saved + totals.index_pending + totals.satisfied_unattributed
            == totals.selected
            && totals.conflicts == 0
        {
            JobPhase::Completed
        } else {
            JobPhase::Partial
        };
        // Record the terminal job marker.
        let _ = log.append(
            0,
            ItemAction::JobTerminal {
                phase: phase.label().to_owned(),
            },
        );
    }
    (phase, totals)
}

fn decode_hash(hex_hash: &str) -> [u8; 32] {
    hex::decode(hex_hash)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .unwrap_or([0u8; 32])
}

/// Log error message (redacted).
#[must_use]
pub fn log_error_message(error: &LogError) -> String {
    error.to_string()
}
