//! Durable saved-note replacement coordinator (RFC-0006 S08/D-10/D-11,
//! extended by S09 for drafts and guarded Revert).
//!
//! One sealed preview + selection revision becomes a job. A coordinator
//! worker runs the item pipeline: per-item `IntentDurable` log record
//! BEFORE any side effect, per-note source lease, actor-mediated retained
//! replacement + index apply (saved notes) or an isolated buffer edit +
//! durable recovery checkpoint (explicit drafts), then a terminal per-item
//! log record. Every mutating request carries `(job, selection_revision,
//! item, action_epoch)` identity and is replayable: completing the same
//! item with the same action epoch returns the recorded receipt. The vault
//! batch is NOT atomic: canonical-order items have monotonic durable
//! outcomes, Cancel stops admission of the next item (finishing an item
//! past its durable intent point), and startup recovery classifies intents
//! against durable disk hashes / checkpoint markers without ever
//! auto-continuing untouched items.
//!
//! S09 adds: `SourceKind::Draft` items (applied as an isolated buffer edit
//! sealed by a durable recovery checkpoint, never a forced save —
//! `AppliedDraft`), and a guarded `Revert` action epoch that restores the
//! sealed before-payloads under after-hash/lineage preconditions, skipping
//! any note whose guard broke (later edit/rename/external write) and
//! offering compare/manual recovery instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use flavite_buffer::revision::BufferId;
use flavite_buffer::undo::UndoPolicy;
use flavite_syntax::text::TextByte;

use super::log::{ItemAction, JobLog, LogError, PersistedItemOutcome};
use super::plan::{NoteCandidateRecord, SealedPayloadRef, SourceKind};
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
    /// A draft buffer edit was installed and durably checkpointed (S09);
    /// the draft is not a file save.
    AppliedDraft,
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
    /// A saved note's before-payload was restored durably (S09 revert).
    Reverted,
    /// A draft's before text was restored with a durable checkpoint (S09).
    RevertedDraft,
}

impl ItemEffect {
    /// Persisted log projection for the apply epoch.
    #[must_use]
    pub fn persisted(self) -> PersistedItemOutcome {
        match self {
            Self::Saved => PersistedItemOutcome::Saved,
            Self::IndexPending => PersistedItemOutcome::IndexPending,
            Self::AppliedConflict => PersistedItemOutcome::AppliedConflict,
            Self::AppliedDraft => PersistedItemOutcome::AppliedDraft,
            Self::Stale => PersistedItemOutcome::Stale,
            Self::Busy => PersistedItemOutcome::Busy,
            Self::Unsupported => PersistedItemOutcome::Denied,
            Self::NeedsRecovery => PersistedItemOutcome::NeedsRecovery,
            Self::SatisfiedUnattributed => PersistedItemOutcome::SatisfiedUnattributed,
            Self::Reverted | Self::RevertedDraft => {
                unreachable!("revert outcomes persist through PersistedRevertOutcome")
            }
        }
    }

    /// Whether the effect means the item's durable intended source outcome
    /// (or draft checkpoint) succeeded, making it revertible.
    #[must_use]
    pub const fn is_applied(self) -> bool {
        matches!(
            self,
            Self::Saved
                | Self::IndexPending
                | Self::AppliedConflict
                | Self::AppliedDraft
                | Self::SatisfiedUnattributed
        )
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
    pub applied_drafts: u64,
    pub stale: u64,
    pub busy: u64,
    pub unsupported: u64,
    pub needs_recovery: u64,
    pub satisfied_unattributed: u64,
    pub not_started: u64,
    /// S09 revert epoch outcomes.
    pub reverted: u64,
    pub reverted_drafts: u64,
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
                Some(ItemEffect::AppliedDraft) => {
                    totals.applied_drafts = totals.applied_drafts.saturating_add(1);
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
                Some(ItemEffect::Reverted) => totals.reverted = totals.reverted.saturating_add(1),
                Some(ItemEffect::RevertedDraft) => {
                    totals.reverted_drafts = totals.reverted_drafts.saturating_add(1);
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
    /// Shared buffer manager (draft install, S09).
    pub manager: Arc<std::sync::Mutex<flavite_buffer::manager::BufferManager>>,
    /// Shared recovery writer (durable draft checkpoints, S09).
    pub recovery_writer: Option<Arc<std::sync::Mutex<flavite_buffer::recovery::RecoveryWriter>>>,
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

/// The sidecar recording each draft item's post-install revision (S09).
/// Written atomically by the apply worker after a draft edit installs, and
/// read at restart/revert time. Missing entries are tolerated: the revert
/// guard then falls back to exact-text equality.
fn draft_state_path(store: &ReplaceStore, job_id: &str) -> PathBuf {
    store.base().join(job_id).join("draft-state.bin")
}

/// Persist `(item_id, applied_revision)` entries (best-effort; callers
/// continue when the sidecar write fails — the durable checkpoint is the
/// authority).
pub fn write_draft_state(
    io: &dyn OpStoreIo,
    store: &ReplaceStore,
    job_id: &str,
    entries: &[(u64, u64)],
) -> Result<(), String> {
    let mut bytes = Vec::with_capacity(8 + entries.len() * 16);
    bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
    for (item_id, revision) in entries {
        bytes.extend_from_slice(&item_id.to_le_bytes());
        bytes.extend_from_slice(&revision.to_le_bytes());
    }
    let path = draft_state_path(store, job_id);
    io.mkdir_all(&store.base().join(job_id))?;
    io.write_replace(&path, &bytes)?;
    io.fsync_dir(&store.base().join(job_id))
}

/// Read the sidecar `(item_id, applied_revision)` map (empty when absent).
pub fn read_draft_state(
    io: &dyn OpStoreIo,
    store: &ReplaceStore,
    job_id: &str,
) -> Result<BTreeMap<u64, u64>, String> {
    let Some(bytes) = io.read(&draft_state_path(store, job_id))? else {
        return Ok(BTreeMap::new());
    };
    if bytes.len() < 8 || (bytes.len() - 8) % 16 != 0 {
        return Err("draft-state record is corrupt".to_owned());
    }
    let count = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
    let mut map = BTreeMap::new();
    for chunk in bytes[8..].as_chunks::<16>().0 {
        let item_id = u64::from_le_bytes(chunk[..8].try_into().expect("8 bytes"));
        let revision = u64::from_le_bytes(chunk[8..].try_into().expect("8 bytes"));
        map.insert(item_id, revision);
    }
    if u64::try_from(map.len()).unwrap_or(u64::MAX) != count {
        return Err("draft-state count mismatch".to_owned());
    }
    Ok(map)
}

fn decode_hash(hex_hash: &str) -> [u8; 32] {
    hex::decode(hex_hash)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .unwrap_or([0u8; 32])
}

/// Decode one sealed payload to normalized text.
fn payload_text(
    store: &ReplaceStore,
    io: &dyn OpStoreIo,
    job_id: &str,
    reference: &SealedPayloadRef,
) -> Result<String, String> {
    let bytes = store
        .read_payload(io, job_id, reference)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "sealed payload is missing".to_owned())?;
    let snapshot = flavite_syntax::text::TextSnapshot::new(&bytes)
        .map_err(|_| "sealed payload decode failed".to_owned())?;
    Ok(snapshot.text().to_owned())
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
        // 3. Effect.
        let effect = match item.record.source_kind {
            SourceKind::Saved => {
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
                match ctx.actor.apply_replace_item(request) {
                    Ok(outcome) => match outcome {
                        ReplaceItemOutcome::Replaced { conflict } => {
                            if conflict {
                                ItemEffect::AppliedConflict
                            } else {
                                ItemEffect::Saved
                            }
                        }
                        ReplaceItemOutcome::SatisfiedUnattributed => {
                            ItemEffect::SatisfiedUnattributed
                        }
                        ReplaceItemOutcome::Stale | ReplaceItemOutcome::Missing => {
                            ItemEffect::Stale
                        }
                        ReplaceItemOutcome::Unsupported => ItemEffect::Unsupported,
                        ReplaceItemOutcome::IndexPending => ItemEffect::IndexPending,
                    },
                    Err(_) => ItemEffect::NeedsRecovery,
                }
            }
            SourceKind::Draft => match apply_draft_item(ctx, job_id, item, store, io) {
                Ok(effect) => effect,
                Err(DraftApplyError::Guard) => ItemEffect::Stale,
                Err(DraftApplyError::Checkpoint) => {
                    tracing::warn!(job_id, item_id, "draft checkpoint failed");
                    ItemEffect::NeedsRecovery
                }
                Err(DraftApplyError::Install) => {
                    tracing::warn!(job_id, item_id, "draft install failed");
                    ItemEffect::NeedsRecovery
                }
                Err(DraftApplyError::Payload(message)) => {
                    tracing::warn!(job_id, item_id, %message, "draft payload failed");
                    ItemEffect::NeedsRecovery
                }
            },
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
        } else if totals.saved
            + totals.index_pending
            + totals.satisfied_unattributed
            + totals.applied_drafts
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

/// Why a draft item could not be applied.
enum DraftApplyError {
    /// A guard (revision/state/hash) no longer matches: final skip.
    Guard,
    /// The isolated edit install failed internally.
    Install,
    /// The durable recovery checkpoint could not be written.
    Checkpoint,
    /// Payload read/decode failed.
    Payload(String),
}

/// Apply one draft item: an isolated buffer edit producing the sealed after
/// payload, a sidecar revision record, and a durable recovery checkpoint.
/// Never force-saves the draft; the buffer stays Dirty and ordinary
/// autosave policy resumes after the checkpoint is durable.
fn apply_draft_item(
    ctx: &CoordinatorCtx,
    job_id: &str,
    item: &JobItem,
    store: &ReplaceStore,
    io: &dyn OpStoreIo,
) -> Result<ItemEffect, DraftApplyError> {
    let record = &item.record;
    let lineage = record
        .draft_lineage
        .as_ref()
        .ok_or(DraftApplyError::Guard)?;
    let before_text =
        payload_text(store, io, job_id, &record.before).map_err(DraftApplyError::Payload)?;
    let after_text =
        payload_text(store, io, job_id, &record.after).map_err(DraftApplyError::Payload)?;
    // Exact draft preconditions (D-09): buffer revision, source hash, state
    // and disk base must still match the sealed record. The manager lock is
    // held through the durable checkpoint so the effect cannot interleave
    // with an edit or save (the lease also refuses both).
    let applied_revision;
    let checkpointed;
    {
        let mut manager = ctx
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(buffer) = manager.get_buffer_by_id_mut(BufferId::from(lineage.buffer_id)) else {
            return Err(DraftApplyError::Guard);
        };
        if buffer.state() != flavite_buffer::state::BufferState::Dirty {
            return Err(DraftApplyError::Guard);
        }
        if buffer.revision().as_u64() != lineage.preview_revision {
            return Err(DraftApplyError::Guard);
        }
        if buffer.text() != before_text {
            return Err(DraftApplyError::Guard);
        }
        let base_ok = match buffer.base_hash() {
            Some(hash) => hex::encode(hash.as_bytes()) == lineage.disk_base_hash,
            None => lineage.disk_base_hash.is_empty(),
        };
        if !base_ok {
            return Err(DraftApplyError::Guard);
        }
        let span = flavite_syntax::Span::new(TextByte(0), TextByte(buffer.len()));
        let op = flavite_buffer::undo::EditOp::new(span, after_text);
        let Some((new_revision, _)) =
            buffer.apply_local_edits_with_policy(&[op], UndoPolicy::Isolated)
        else {
            return Err(DraftApplyError::Install);
        };
        applied_revision = new_revision.as_u64();
        // Durable checkpoint: the draft effect is only terminal once this
        // marker is durable (D-11).
        let note_key = crate::session::native_note_key(&ctx.root.join(&record.path), &ctx.root);
        let mut guard = ctx.recovery_writer.as_ref().map(|writer| {
            writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        });
        checkpointed = match guard.as_mut() {
            Some(writer) => writer.write_checkpoint_durable(&note_key, buffer).is_some(),
            None => false,
        };
    }
    if !checkpointed {
        return Err(DraftApplyError::Checkpoint);
    }
    // Sidecar (best-effort). The durable checkpoint is the authority; a
    // failed sidecar only weakens the later revert guard to text equality.
    if let Err(error) = write_draft_state(io, store, job_id, &[(record.item_id, applied_revision)])
    {
        tracing::warn!(job_id, item_id = record.item_id, %error, "draft sidecar failed");
    }
    Ok(ItemEffect::AppliedDraft)
}

/// Run one guarded Revert action epoch (RFC-0006 S09/D-11): restore the
/// sealed before-payloads of every applied item under after-hash/lineage
/// preconditions. Items whose guard broke are skipped (Stale) and remain
/// offered for compare/manual recovery; items that were never applied are
/// left untouched. Idempotent: re-running skips completed items.
#[allow(clippy::too_many_arguments)]
pub fn run_revert(
    ctx: &CoordinatorCtx,
    job_id: &str,
    items: &mut [JobItem],
    cancel: &AtomicBool,
    log: &mut JobLog,
    store: &ReplaceStore,
    io: &dyn OpStoreIo,
) -> (JobPhase, JobTotals) {
    let applied_before: Vec<bool> = items
        .iter()
        .map(|item| item.effect.is_some_and(|effect| effect.is_applied()))
        .collect();
    let mut phase = JobPhase::Applying;
    for (index, item) in items.iter_mut().enumerate() {
        if !applied_before[index] {
            continue; // never applied: nothing to revert
        }
        if matches!(
            item.effect,
            Some(
                ItemEffect::Reverted
                    | ItemEffect::RevertedDraft
                    | ItemEffect::Stale
                    | ItemEffect::NeedsRecovery
            )
        ) {
            continue; // already handled by a previous (idempotent) run
        }
        if cancel.load(Ordering::Relaxed) {
            phase = JobPhase::Cancelled;
            break;
        }
        let item_id = item.record.item_id;
        let relative = Path::new(&item.record.path);
        let lease_key = relative.as_os_str().as_encoded_bytes().to_vec();
        let Ok(lease) = ctx
            .leases
            .acquire(lease_key, crate::source_leases::LeaseOwner::Replace)
        else {
            continue;
        };
        if let Err(error) = log.append(item_id, ItemAction::RevertIntentDurable) {
            ctx.leases.release(&lease);
            item.effect = Some(ItemEffect::NeedsRecovery);
            tracing::warn!(job_id, item_id, ?error, "revert intent log failed");
            phase = JobPhase::NeedsRecovery;
            break;
        }
        let effect = match item.record.source_kind {
            SourceKind::Saved => match revert_saved_item(ctx, job_id, item, store, io) {
                Ok(effect) => effect,
                Err(message) => {
                    tracing::warn!(job_id, item_id, %message, "revert item failed");
                    ItemEffect::NeedsRecovery
                }
            },
            SourceKind::Draft => match revert_draft_item(ctx, job_id, item, store, io) {
                Ok(effect) => effect,
                Err(RevertDraftError::Guard) => ItemEffect::Stale,
                Err(RevertDraftError::Checkpoint) => {
                    tracing::warn!(job_id, item_id, "revert draft checkpoint failed");
                    ItemEffect::NeedsRecovery
                }
                Err(RevertDraftError::Payload(message)) => {
                    tracing::warn!(job_id, item_id, %message, "revert draft payload failed");
                    ItemEffect::NeedsRecovery
                }
            },
        };
        let persisted = match effect {
            ItemEffect::Reverted => super::log::PersistedRevertOutcome::Reverted,
            ItemEffect::RevertedDraft => super::log::PersistedRevertOutcome::RevertedDraft,
            ItemEffect::Stale => super::log::PersistedRevertOutcome::Stale,
            _ => super::log::PersistedRevertOutcome::NeedsRecovery,
        };
        if let Err(error) = log.append(item_id, ItemAction::RevertOutcome { outcome: persisted }) {
            ctx.leases.release(&lease);
            item.effect = Some(ItemEffect::NeedsRecovery);
            tracing::warn!(job_id, item_id, ?error, "revert outcome log failed");
            phase = JobPhase::NeedsRecovery;
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
        let any_nonterminal = items.iter().enumerate().any(|(index, item)| {
            applied_before[index] && matches!(item.effect, Some(ItemEffect::NeedsRecovery))
        });
        let reverted_all = items.iter().enumerate().all(|(index, item)| {
            !applied_before[index]
                || matches!(
                    item.effect,
                    Some(ItemEffect::Reverted | ItemEffect::RevertedDraft)
                )
        });
        phase = if any_nonterminal {
            JobPhase::NeedsRecovery
        } else if reverted_all && applied_before.iter().any(|applied| *applied) {
            JobPhase::Completed
        } else {
            JobPhase::Partial
        };
        let _ = log.append(
            0,
            ItemAction::JobTerminal {
                phase: phase.label().to_owned(),
            },
        );
    }
    (phase, totals)
}

/// Restore one saved note: the inverse retained replacement (expected base
/// = the sealed after-hash, bytes = the sealed before-payload).
fn revert_saved_item(
    ctx: &CoordinatorCtx,
    job_id: &str,
    item: &JobItem,
    store: &ReplaceStore,
    io: &dyn OpStoreIo,
) -> Result<ItemEffect, String> {
    let record = &item.record;
    let before_bytes = store
        .read_payload(io, job_id, &record.before)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "before payload is missing".to_owned())?;
    let request = ReplaceItemRequest {
        job_id: job_id.to_owned(),
        item_id: record.item_id,
        path: ctx.root.join(&record.path),
        expected_before_hash: decode_hash(&record.after.hash),
        after_bytes: before_bytes,
        note_key: hex::decode(&record.path_native).unwrap_or_default(),
    };
    match ctx.actor.revert_replace_item(request) {
        Ok(ReplaceItemOutcome::Replaced { conflict }) => {
            if conflict {
                // An external writer raced at the exchange: its bytes are
                // retained, our inverse effect is uncertain — not a green
                // Reverted.
                Ok(ItemEffect::NeedsRecovery)
            } else {
                Ok(ItemEffect::Reverted)
            }
        }
        Ok(ReplaceItemOutcome::SatisfiedUnattributed) => Ok(ItemEffect::Reverted),
        Ok(ReplaceItemOutcome::Stale | ReplaceItemOutcome::Missing) => Ok(ItemEffect::Stale),
        Ok(ReplaceItemOutcome::Unsupported) => Ok(ItemEffect::Unsupported),
        Ok(ReplaceItemOutcome::IndexPending) => Ok(ItemEffect::IndexPending),
        Err(message) => Err(message),
    }
}

/// Why a draft revert could not complete.
enum RevertDraftError {
    Guard,
    Checkpoint,
    Payload(String),
}

/// Restore one draft item: inverse isolated buffer edit (after → before)
/// with a durable checkpoint, or a disk revert when the buffer has since
/// been saved at the after state.
fn revert_draft_item(
    ctx: &CoordinatorCtx,
    job_id: &str,
    item: &JobItem,
    store: &ReplaceStore,
    io: &dyn OpStoreIo,
) -> Result<ItemEffect, RevertDraftError> {
    let record = &item.record;
    let lineage = record
        .draft_lineage
        .as_ref()
        .ok_or(RevertDraftError::Guard)?;
    let before_text =
        payload_text(store, io, job_id, &record.before).map_err(RevertDraftError::Payload)?;
    let after_text =
        payload_text(store, io, job_id, &record.after).map_err(RevertDraftError::Payload)?;
    // A disk revert is preferred when the draft has since been saved at the
    // exact after state (ordinary whole-draft autosave after checkpoint):
    // the source is disk-backed then.
    let (buffer_id, revision, state, text, disk_hash) = {
        let manager = ctx
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(buffer) = manager.get_buffer_by_id(BufferId::from(lineage.buffer_id)) else {
            return Err(RevertDraftError::Guard);
        };
        (
            buffer.buffer_id(),
            buffer.revision().as_u64(),
            buffer.state(),
            buffer.text().to_owned(),
            buffer.disk_hash().map(|h| hex::encode(h.as_bytes())),
        )
    };
    let applied_revision = read_draft_state(io, store, job_id)
        .map_err(RevertDraftError::Payload)?
        .get(&record.item_id)
        .copied();
    let guard_ok = match applied_revision {
        Some(expected) => revision == expected,
        // No sidecar (crash between install and sidecar): fall back to
        // exact text equality — the revert only ever rewrites bytes that
        // ARE the sealed after content, so it cannot erase newer edits.
        None => text == after_text,
    };
    if !guard_ok {
        return Err(RevertDraftError::Guard);
    }
    // Saved-at-after state → disk revert path.
    if state == flavite_buffer::state::BufferState::Clean
        && disk_hash.as_deref() == Some(record.after.hash.as_str())
    {
        let effect =
            revert_saved_item(ctx, job_id, item, store, io).map_err(RevertDraftError::Payload)?;
        return Ok(effect);
    }
    if state != flavite_buffer::state::BufferState::Dirty {
        return Err(RevertDraftError::Guard);
    }
    let checkpointed;
    {
        let mut manager = ctx
            .manager
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
            return Err(RevertDraftError::Guard);
        };
        if buffer.revision().as_u64() != revision {
            return Err(RevertDraftError::Guard);
        }
        let span = flavite_syntax::Span::new(TextByte(0), TextByte(buffer.len()));
        let op = flavite_buffer::undo::EditOp::new(span, before_text);
        let Some((_, _)) = buffer.apply_local_edits_with_policy(&[op], UndoPolicy::Isolated) else {
            return Err(RevertDraftError::Guard);
        };
        let note_key = crate::session::native_note_key(&ctx.root.join(&record.path), &ctx.root);
        let mut guard = ctx.recovery_writer.as_ref().map(|writer| {
            writer
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        });
        checkpointed = match guard.as_mut() {
            Some(writer) => writer.write_checkpoint_durable(&note_key, buffer).is_some(),
            None => false,
        };
    }
    if !checkpointed {
        return Err(RevertDraftError::Checkpoint);
    }
    Ok(ItemEffect::RevertedDraft)
}

/// Log error message (redacted).
#[must_use]
pub fn log_error_message(error: &LogError) -> String {
    error.to_string()
}
