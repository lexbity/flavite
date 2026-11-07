//! Session — the headless core that owns BufferManager (§4.2, §4.5, D7, D14).
//!
//! The Session is the single authority for document state in a vault.
//! It owns:
//! - BufferManager (one buffer per open note)
//! - Pending operations ledger
//! - Worker handles
//!
//! All document mutations execute as session commands and apply EditOps
//! through its BufferManager. Widgets receive read-only views.

use crate::actor::VaultActorHandle;
use crate::channels::{NoticeChannel, PulseChannel};
use crate::encoding::{CHOICE_REQUIRED_CODE, ChoiceReason, EncodingHintsState, Resolution};
use crate::replace::store::OpStoreIo;
use crate::save::{DiskPrecondition, DurabilityOutcome, SaveFailureStage, SaveRequest};
use crate::sync::MutexExt;
use crate::sync::WAIT_TICK;
use crate::types::{
    Epoch, Generation, ReconcileMode, ReconcileScope, ReconcileSummary, RenameStrategy,
    VaultConfig, VaultIdentity,
};
use flavite_buffer::manager::{BufferManager, BufferStateChanged};
use flavite_buffer::recovery::{RecoveryConfig, RecoveryWriter};
use flavite_buffer::revision::{BaseHash, BufferId, Revision};
use flavite_buffer::undo::EditOp;
use flavite_encoding::DecodePolicy;
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

fn located_patch(
    text: &str,
    target_key: &str,
    old_stem: &str,
    new_stem: &str,
) -> Result<Vec<EditOp>, String> {
    let snapshot = flavite_syntax::text::TextSnapshot::new(text.as_bytes());
    let snapshot = snapshot.map_err(|error| error.to_string())?;
    let mut edits = Vec::new();
    for scanned in flavite_syntax::scanner::scan_inline(&snapshot, 0..snapshot.text().len()) {
        let Some(payload) = scanned.payload else {
            continue;
        };
        let folded_target = flavite_syntax::text::fold_key(&payload.target_raw);
        let suffixed = folded_target.strip_prefix("id:").unwrap_or(&folded_target);
        let basename_stem = folded_target
            .rsplit('/')
            .next()
            .unwrap_or(&folded_target)
            .rsplit_once('.')
            .map_or_else(
                || folded_target.rsplit('/').next().unwrap_or(&folded_target),
                |(stem, _)| stem,
            );
        if folded_target != target_key
            && folded_target != format!("{target_key}.md")
            && !folded_target.ends_with(&format!("/{target_key}"))
            && !folded_target.ends_with(&format!("/{target_key}.md"))
            && basename_stem != target_key
            && suffixed != target_key
        {
            continue;
        }
        let start = scanned.span.start.0;
        let end = scanned.span.end.0;
        let raw = &snapshot.text()[start..end];
        let relative = raw
            .find(&payload.target_raw)
            .ok_or_else(|| "scanner target span is inconsistent".to_owned())?;
        let target_start = start + relative;
        let stem_offset = payload
            .target_raw
            .find(old_stem)
            .ok_or_else(|| "link-absent".to_owned())?;
        edits.push(EditOp::new(
            flavite_syntax::text::Span::new(
                flavite_syntax::text::TextByte(target_start + stem_offset),
                flavite_syntax::text::TextByte(target_start + stem_offset + old_stem.len()),
            ),
            new_stem.to_owned(),
        ));
    }
    Ok(edits)
}
use thiserror::Error;
use uuid::Uuid;

const MAX_PENDING_SAVE_OPERATIONS: usize = 4096;
/// RFC-0006 §6.9: at most 64 unconsumed replacement plans; a 30-second TTL.
const MAX_UNCONSUMED_TEXT_PLANS: usize = 64;
const TEXT_PLAN_TTL: Duration = Duration::from_secs(30);
/// Consumed results are replayable for 10 minutes; at most 2,048 retained.
const TEXT_PLAN_RESULT_TTL: Duration = Duration::from_secs(600);
const MAX_RETAINED_TEXT_RESULTS: usize = 2_048;
const TEXT_PREPARE_WORKERS: usize = 2;
/// NFR-10: at most this many queued text prepare/find admissions before
/// new admissions are refused with an explicit Busy (never unbounded).
const TEXT_QUEUE_MAX: usize = 16;
/// RFC-0006 S07: bounded vault source-find/preview workers (NFR-10:
/// replacement preparation workers 2).
const REPLACE_PREPARE_WORKERS: usize = 2;
/// NFR-10: queued replace admissions before explicit Busy.
const REPLACE_QUEUE_MAX: usize = 16;
/// S07: at most this many live source-finder sessions.
const MAX_LIVE_FINDERS: usize = 4;
/// S07: at most this many live sealed previews (plus NFR-10: two live
/// previews per vault).
const MAX_LIVE_PREVIEWS: usize = 2;
/// Per-buffer journal memo cap (C1). Entries are keyed `(txn_id, item_id)`
/// inside the buffer's own memo: total memo memory is bounded by
/// `open_buffers × JOURNAL_MEMO_CAP × entry`, roughly ≤ 800 KiB per open
/// buffer (NFR-1) instead of the unbounded process-lifetime growth of the
/// former global maps. Eviction is FIFO by insertion order; a replayed
/// patch after eviction re-derives safely through the actor's revision
/// guard (proven by the S5 replay test).
const JOURNAL_MEMO_CAP: usize = 4096;

/// Bounded pending cold opens (RFC-0003 §12.1: pending opens ≤128).
const MAX_PENDING_OPEN_NOTES: usize = 128;
/// Concurrent cold-open read flights (RFC-0003 §12.1).
const OPEN_READ_WORKERS: usize = 4;

/// Whether a cold open may create the note when it is absent.
///
/// `ExistingOnly` renders missing notes as recoverable missing states;
/// `CreateIfMissing` keeps the established absent-file creation guard
/// (RFC-0003 §8.3). The policy cannot be inferred from a display title.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OpenPolicy {
    /// Open the note; when the file is absent, open an empty buffer that is
    /// recorded in the absent-file creation guard and created on first save.
    #[default]
    CreateIfMissing,
    /// Open only when the file exists; otherwise the open fails.
    ExistingOnly,
    /// Explicit creation (W10): fail when the file already exists; otherwise
    /// create the empty file on disk through the vault actor and open it.
    /// Unlike [`OpenPolicy::CreateIfMissing`], the file exists immediately,
    /// so links and the index can resolve it without a save.
    CreateNew,
}

#[derive(Debug)]
struct SaveFlight {
    request: SaveRequest,
    response: crossbeam::channel::Receiver<crate::actor::VaultActorResp>,
    queued: Option<SaveRequest>,
}

/// One coalesced cold-open flight keyed by native path. Every admitted
/// caller records its request id and receives its own terminal result.
#[derive(Debug, Default)]
struct OpenFlight {
    request_ids: Vec<u64>,
}

/// A read job handed to the bounded open-worker pool. The blocking
/// `actor.read_file` and the recovery lookup run on the worker, never
/// under the Session driving turn.
#[derive(Debug)]
struct OpenJob {
    path: PathBuf,
    policy: OpenPolicy,
    note_key: Vec<u8>,
}

/// A completed read, awaiting installation on a short Session turn.
#[derive(Debug)]
struct OpenRead {
    path: PathBuf,
    policy: OpenPolicy,
    outcome: Result<Option<Vec<u8>>, String>,
    recovery: Option<flavite_buffer::recovery::RecoveryFile>,
}

/// Terminal result for an admitted asynchronous Session operation.
#[derive(Debug, Clone)]
pub struct OperationCompletion {
    /// Request ID returned by the initial Pending result.
    pub request_id: u64,
    /// The operation's terminal result.
    pub result: SessionResult,
}

/// Immutable revisioned buffer data exposed by Session to API hosts.
#[derive(Debug, Clone)]
pub struct BufferSnapshot {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub text: String,
    pub base_hash: Option<[u8; 32]>,
    pub state: flavite_buffer::state::BufferState,
    pub path: PathBuf,
}

/// Small source identity used to reject obsolete graph publications.
pub type GraphBufferStamp = (
    BufferId,
    Revision,
    PathBuf,
    flavite_buffer::state::BufferState,
    Option<[u8; 32]>,
);

/// Writer-side broadcast of buffer-stamp transitions (RFC-0005 §5.1/S6).
///
/// The sending half, owned by [`Session`] and installed into the buffer
/// manager (whose mutation choke points are the audit list from S5's
/// stamp-version inventory: revision bump, state dirty/clean, path change,
/// base-hash change, open/close). Lossy by design — `bounded(1)`,
/// `try_send`, drop-on-full, latest-wins coalescing, exactly the
/// [`flavite_index::GenerationSignal`] discipline (§4.2): a wake carries no
/// data, only "a stamp-relevant transition happened"; consumers re-read
/// [`Session::graph_buffer_stamp`] / the stamp version themselves and the
/// graph service applies its trailing-edge debounce policy (§4.4). The
/// manager keeps a sender clone, so the channel stays connected for the
/// session's lifetime; sends into a watch-less consumer are harmless drops.
#[derive(Clone, Debug)]
pub struct BufferSignal {
    tx: crossbeam::channel::Sender<()>,
}

/// The receiving half of a [`BufferSignal`]. One logical consumer (the
/// graph service; its restart attempts hand the receiver along, never
/// running two simultaneously). Test spies may take the watch — then the
/// service runs on its inert stub and converges via the sweep.
#[derive(Clone, Debug)]
pub struct BufferWatch {
    rx: crossbeam::channel::Receiver<()>,
}

impl BufferSignal {
    /// Queue capacity one: latest-wins coalescing (§4.2) — the graph never
    /// wants a backlog of keystroke pulses, only "something changed, and
    /// the newest stamp is what the rebuild will read".
    const QUEUE: usize = 1;

    /// Create a connected sender/watch pair.
    #[must_use]
    pub fn new() -> (Self, BufferWatch) {
        let (tx, rx) = crossbeam::channel::bounded(Self::QUEUE);
        (Self { tx }, BufferWatch { rx })
    }

    /// The sending-end clone for the buffer manager's choke points.
    #[must_use]
    pub fn notifier(&self) -> crossbeam::channel::Sender<()> {
        self.tx.clone()
    }

    /// Advertise a stamp-relevant transition. Never blocks, never errors:
    /// a full slot means a wake is already pending (coalescing), a gone
    /// receiver means no consumer (§4.10 — a send into a dead watch is a
    /// harmless drop).
    pub fn notify(&self) {
        let _ = self.tx.try_send(());
    }
}

impl Default for BufferSignal {
    fn default() -> Self {
        Self::new().0
    }
}

impl BufferWatch {
    /// The receiver handle the graph service multiplexes in its wake
    /// select. Cloneable — but only one consumer may actively recv at a
    /// time (a bounded channel dispatches each wake to one receiver).
    #[must_use]
    pub fn receiver(&self) -> crossbeam::channel::Receiver<()> {
        self.rx.clone()
    }

    /// Wait up to `timeout` for a wake (`None` on timeout). Test-spy side.
    #[must_use]
    pub fn recv_timeout(&self, timeout: std::time::Duration) -> Option<()> {
        self.rx.recv_timeout(timeout).ok()
    }
}

/// Immutable shared snapshot for multiple views of the same buffer revision.
#[derive(Debug, Clone)]
pub struct SharedBufferSnapshot {
    pub buffer_id: BufferId,
    pub revision: Revision,
    pub text: std::sync::Arc<str>,
    pub base_hash: Option<[u8; 32]>,
    pub state: flavite_buffer::state::BufferState,
    pub path: PathBuf,
    /// Authoritative save-format descriptor of this revision (§6.10).
    pub source_format: flavite_encoding::SourceFormat,
}

/// Failure while acquiring an immutable buffer snapshot.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BufferSnapshotError {
    #[error("buffer {0} is not open")]
    NotFound(u64),
    #[error("stale revision: expected {expected}, current {actual}")]
    Stale { expected: u64, actual: u64 },
}

/// Commands that can be executed by the Session.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Guarded property/task edit; one standalone Undo transaction.
    EditStructure {
        buffer_id: BufferId,
        revision: Revision,
        edit: crate::properties::StructureEdit,
    },
    /// Open a note.
    OpenNote { path: PathBuf, policy: OpenPolicy },
    /// Create a new empty note file and open it (W10). The command fails
    /// when the path already exists; creation runs on the open workers and
    /// the actor, never under the driving turn.
    CreateNote { path: PathBuf },
    /// Close a note.
    CloseNote {
        path: PathBuf,
        /// Retain a durable recovery snapshot covering the unsaved edits
        /// before the buffer is dropped (W12: close-with-recovery policy).
        retain_recovery: bool,
    },
    /// Apply edits to a note.
    ApplyEdits {
        buffer_id: BufferId,
        revision: Revision,
        ops: Vec<EditOp>,
        /// Undo-unit policy (RFC-0006 D-06): `Isolated` (default) installs
        /// the batch as one boundary unit; the editor's captured typing path
        /// requests `Typing` explicitly.
        undo_policy: flavite_buffer::undo::UndoPolicy,
    },
    /// Apply a user-reviewed three-way merge to the exact buffer and disk
    /// versions represented by its preview.
    ApplyMerge {
        path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        disk_hash: [u8; 32],
        merged_bytes: Vec<u8>,
    },
    /// Save a note.
    SaveNote { path: PathBuf },
    /// Trigger a reconcile.
    Reconcile {
        scope: ReconcileScope,
        mode: ReconcileMode,
    },
    /// Rename a note and journal inbound-link rewrites.
    RenameNote {
        from: PathBuf,
        to: PathBuf,
        strategy: Option<RenameStrategy>,
    },
    /// Undo an edit.
    Undo {
        buffer_id: BufferId,
        revision: Revision,
    },
    /// Redo an edit.
    Redo {
        buffer_id: BufferId,
        revision: Revision,
    },
    /// Resolve a conflict.
    ResolveConflict {
        path: PathBuf,
        resolution: ConflictResolution,
    },
    /// Restore the recovery snapshot for a note after explicit user choice.
    RestoreRecovery { path: PathBuf },
    /// Discard the recovery snapshot for a note after explicit user choice.
    DiscardRecovery { path: PathBuf },
    /// Durably checkpoint one dirty buffer's recovery snapshot now and
    /// return a revision-bound receipt (W12).
    FlushRecovery { path: PathBuf },
    /// Snapshot the buffer's unsaved edits into local history, drop them,
    /// and reload from disk as one explicit destructive action (W12).
    DiscardEdits { path: PathBuf },
    /// Replace an open buffer's content with a local-history snapshot as one
    /// guarded, undoable edit (W10). The note must already be open: restore
    /// never mutates disk and never opens notes implicitly.
    RestoreHistory {
        path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        hash: String,
    },
    /// Prepare a semantic link insertion plan (W05; expensive work runs on
    /// a bounded worker, admission happens here under the ingress turn).
    LinkPrepare {
        request: crate::link::LinkPrepareRequest,
        request_id: u64,
    },
    /// Consume a prepared exact occurrence-removal plan.
    LinkRemove {
        intent: crate::link::LinkIntent,
        plan_id: u128,
    },
    /// Commit a prepared semantic link with one guarded edit (W05).
    LinkInsert {
        intent: crate::link::LinkIntent,
        plan_id: u128,
    },
    /// Reopen a note under an explicit source-format interpretation
    /// (RFC-0006 §6.10). Refuses a dirty buffer or a pending save with a
    /// typed action; binds the confirmed hint on success.
    ReopenEncoding {
        path: PathBuf,
        expected_raw_hash: [u8; 32],
        encoding: flavite_encoding::FileEncoding,
        bom: crate::encoding::BomMode,
    },
    /// Convert an open buffer's save format (§6.10): preserves the current
    /// decoded text and its Undo lineage as one isolated boundary; disk is
    /// untouched until the next normal save.
    ConvertEncoding {
        buffer_id: BufferId,
        revision: Revision,
        target: flavite_encoding::SourceFormat,
    },
    /// Prepare a literal replacement plan (RFC-0006 S01/D-05/D-06). The
    /// match/computation work runs on a bounded worker; admission validates
    /// the buffer/revision and reserves a plan slot.
    PrepareReplace {
        request_id: u64,
        request: crate::text_edit::ReplaceRequest,
    },
    /// Find matches in the current buffer (RFC-0006 S03/D-05/FR-2): the
    /// same canonical matcher semantics as replacement, reported as
    /// revision-bound span pages. Literal matches run on a bounded worker;
    /// regex matches run in the isolated helper (D-14). Read-only: no plan,
    /// no mutation, and late completions are correlation-dropped by the
    /// caller when the query, revision or view has moved on.
    FindMatches {
        request_id: u64,
        request: crate::text_edit::FindRequest,
    },
    /// Install one prepared replacement as one isolated Undo unit (D-06).
    /// Consumed plans replay the same result during the retry window.
    ApplyReplace {
        buffer_id: BufferId,
        revision: Revision,
        plan_id: u128,
    },
    /// Start a vault source find (RFC-0006 S07): frozen scope, exact
    /// per-note matching (literal in-process; regex via the isolated
    /// helper, Bulk permits), spans streamed to a private spool. Read-only.
    SourceFindStart {
        request_id: u64,
        request: crate::replace::preview::SourceFindRequest,
    },
    /// One continuation page of an open finder session (S07).
    SourceFindPage {
        finder_id: u128,
        note_index: u64,
        match_offset: u64,
        page_len: usize,
    },
    /// Close a finder session and drop its spool (S07).
    SourceFindClose { finder_id: u128 },
    /// Prepare and seal a replacement preview (S07/D-09): full scan, exact
    /// serialized outputs, sealed manifest + content-addressed payloads.
    /// Read-only; nothing is applied.
    ReplacePreviewPrepare {
        request_id: u64,
        request: crate::replace::preview::PreviewRequest,
    },
    /// Record a selection revision on a sealed preview (S07). Produces an
    /// immutable selection digest; it never mutates notes.
    ReplaceSelect {
        preview_id: u128,
        selected: ReplaceSelection,
    },
    /// Report a sealed preview's status and totals (S07).
    ReplaceStatus { preview_id: u128 },
    /// Apply a sealed preview under a frozen selection revision
    /// (RFC-0006 S08/D-10). The first Apply freezes the selection; a
    /// different revision is SelectionLocked. `idempotency_key` is a
    /// caller-generated 128-bit key bound to job/selection/digest.
    ReplaceApply {
        request_id: u64,
        preview_id: u128,
        selection_revision: u64,
        idempotency_key: u128,
    },
    /// Stop admission of the next item; an item past its durable intent
    /// point finishes.
    ReplaceCancel { job_id: u128 },
    /// Explicitly resume a paused/partial/cancelled job (fresh grants are
    /// rechecked per item; untouched items run with unchanged guards).
    ReplaceResume {
        request_id: u64,
        job_id: u128,
        idempotency_key: u128,
    },
    /// Start a guarded Revert action epoch (RFC-0006 S09/D-11): restore the
    /// selected before-payloads of applied items under after-hash/lineage
    /// preconditions. `idempotency_key` is caller-generated and bound to
    /// the job/action.
    ReplaceRevert {
        request_id: u64,
        job_id: u128,
        idempotency_key: u128,
    },
    /// Current job status and totals.
    ReplaceJobStatus { job_id: u128 },
}

/// Which notes a preview selection names (S07/D-09). Selection is a local
/// revision/digest, not a mutation grant.
#[derive(Debug, Clone)]
pub enum ReplaceSelection {
    /// Select every eligible candidate.
    All,
    /// Select none.
    None,
    /// Select exactly the candidates whose native path (hex) is listed.
    Keys(Vec<String>),
}

/// Conflict resolution strategy.
#[derive(Debug, Clone)]
pub enum ConflictResolution {
    /// Keep the current version (ours).
    KeepMine,
    /// Reload from disk (theirs).
    Reload,
    /// Save as a new file.
    SaveAs { new_path: PathBuf },
    /// Merge the changes.
    Merge,
}

/// Results from Session commands.
#[derive(Debug, Clone)]
pub enum SessionResult {
    /// Command succeeded with data.
    Success(CommandResult),
    /// Command is pending.
    Pending { request_id: u64 },
    /// Operation was not admitted because the lossless completion ledger is full.
    Busy,
    /// Revision precondition did not match the current authoritative buffer.
    StaleRevision {
        buffer_id: BufferId,
        expected: Revision,
        actual: Revision,
    },
    /// An admitted save was replaced by a later request for the same buffer.
    Superseded { request_id: u64, by_request: u64 },
    /// Command failed.
    Error { message: String },
    /// Buffer is in conflict state.
    Conflict { path: PathBuf },
    /// Three-way merge preview. Conflicts are structured data and no markers
    /// are inserted into note content.
    MergeReady {
        path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        disk_hash: [u8; 32],
        result: crate::merge::MergeResult,
    },
    /// Action required from user.
    ActionRequired {
        code: &'static str,
        data: Option<String>,
    },
    /// Typed semantic-link rejection with its repair action (W05).
    LinkRejected {
        reason: crate::link::LinkInsertionError,
    },
}

/// Data returned from successful commands.
#[derive(Debug, Clone)]
pub enum CommandResult {
    /// Note was opened.
    NoteOpened {
        buffer_id: BufferId,
        revision: Revision,
    },
    /// A new empty note file was created and opened (W10 explicit creation).
    NoteCreated {
        buffer_id: BufferId,
        revision: Revision,
        path: PathBuf,
    },
    /// Note was closed.
    NoteClosed,
    /// Edits were applied.
    EditsApplied {
        buffer_id: BufferId,
        new_revision: Revision,
    },
    /// A semantic link plan was prepared (W05).
    LinkPrepared {
        /// The prepared plan exposed to the host.
        prepared: crate::link::PreparedLink,
    },
    /// A semantic link was inserted with one guarded edit (W05).
    LinkInserted {
        /// Source buffer.
        buffer_id: BufferId,
        /// New revision after the insertion.
        new_revision: Revision,
    },
    /// Note was saved.
    NoteSaved {
        /// Buffer instance whose immutable snapshot was saved.
        buffer_id: BufferId,
        /// Revision represented by the saved bytes.
        saved_revision: Revision,
        path: PathBuf,
        hash: Vec<u8>,
        epoch: Epoch,
        generation: Generation,
    },
    /// Reconcile completed.
    ReconcileComplete { summary: ReconcileSummary },
    /// A journaled rename and its indexed link patches completed.
    NoteRenamed {
        txn_id: String,
        from: PathBuf,
        to: PathBuf,
    },
    /// Undo was performed.
    UndoPerformed {
        buffer_id: BufferId,
        new_revision: Revision,
    },
    /// Redo was performed.
    RedoPerformed {
        buffer_id: BufferId,
        new_revision: Revision,
    },
    /// Conflict resolved.
    ConflictResolved,
    /// Recovery snapshot restored into the authoritative buffer.
    RecoveryRestored {
        buffer_id: BufferId,
        revision: Revision,
    },
    /// A durable recovery snapshot was written for `buffer_id` covering
    /// `revision` with content hash `hash` (W12 FlushRecovery receipt).
    RecoveryFlushed {
        buffer_id: BufferId,
        revision: Revision,
        hash: BaseHash,
    },
    /// Unsaved edits were snapshotted into local history and reloaded
    /// from disk (W12 explicit discard).
    EditsDiscarded { buffer_id: BufferId },
    /// A local-history snapshot replaced the open buffer content as one
    /// guarded, undoable edit (W10). Nothing was written to disk.
    HistoryRestored {
        buffer_id: BufferId,
        new_revision: Revision,
    },
    /// A note's bounded local-history listing (W10).
    NoteHistory {
        path: PathBuf,
        entries: Vec<crate::history::HistoryEntry>,
    },
    /// A note was reopened under an explicit source format (§6.10). All
    /// attached views observe the new revision and descriptor.
    EncodingReopened {
        buffer_id: BufferId,
        revision: Revision,
    },
    /// A buffer's save format was converted with one isolated Undo boundary
    /// (§6.10). Text is unchanged; disk is untouched until the next save.
    EncodingConverted {
        buffer_id: BufferId,
        new_revision: Revision,
    },
    /// A literal replacement plan is ready (RFC-0006 S01): core-issued plan
    /// id, exact match count and the preparation TTL.
    ReplacePrepared {
        plan_id: u128,
        match_count: u64,
        expires_in_ms: u64,
    },
    /// A note find page is ready (RFC-0006 S03/D-05): revision-bound
    /// ascending spans over the snapshot the request was admitted at, the
    /// exact match total, and the honest page-cap flag. Read-only — no
    /// mutation authority and no plan slot.
    Found {
        buffer_id: BufferId,
        revision: Revision,
        spans: Vec<(u64, u64)>,
        offset: u64,
        total_matches: u64,
        next_offset: Option<u64>,
        truncated: bool,
    },
    /// A vault source-find session started (RFC-0006 S07): frozen scope,
    /// exact totals, typed exclusions and the first page.
    SourceFindStarted {
        finder_id: u128,
        scope: crate::replace::ScopeVersion,
        truncated_scope: bool,
        total_notes: u64,
        total_matches: u64,
        exclusions: Vec<crate::replace::NoteExclusion>,
        page: SourceFindPageData,
    },
    /// One vault source-find continuation page (S07).
    SourceFindPaged {
        finder_id: u128,
        page: SourceFindPageData,
    },
    /// A source-find session was closed and its spool dropped (S07).
    SourceFindClosed,
    /// A replacement preview was sealed (S07/D-09): the plan is ready and
    /// non-applying; selection is a separate command.
    ReplacePreviewSealed {
        preview_id: u128,
        manifest: Arc<crate::replace::PreviewManifest>,
    },
    /// A selection revision was recorded on the sealed plan (S07).
    ReplaceSelected {
        selection_revision: u64,
        selection_digest: String,
    },
    /// The current status of a sealed preview (S07).
    ReplaceStatus {
        preview_id: u128,
        manifest: Arc<crate::replace::PreviewManifest>,
    },
    /// A durable replacement job applied/cancelled/resumed/finished (S08).
    ReplaceJobFinished {
        job_id: u128,
        phase: crate::replace::JobPhase,
        totals: crate::replace::JobTotals,
    },
    /// A durable replacement job was cancelled (S08).
    ReplaceCancelled { job_id: u128 },
    /// Current durable job status (S08).
    ReplaceJobStatus {
        job_id: u128,
        phase: crate::replace::JobPhase,
        totals: crate::replace::JobTotals,
        selection_revision: u64,
        selected_items: u64,
    },
}

/// One page of a vault source-find session (S07): note/match-ordered,
/// with exact continuation. Rows carry the note identity and bounded match
/// spans; pages never duplicate and never skip (ordinal-based).
#[derive(Debug, Clone)]
pub struct SourceFindPageData {
    /// Zero-based note ordinal this page begins at.
    pub note_index: u64,
    /// Total notes with matches in the finder.
    pub note_total: u64,
    /// Match offset within the first row.
    pub match_offset: u64,
    /// Rows in ascending note order.
    pub rows: Vec<MatchRow>,
    /// Next note ordinal, when more notes remain.
    pub next_note_index: Option<u64>,
    /// Next match offset within the last row, when more matches remain.
    pub next_match_offset: Option<u64>,
}

/// One find row: a note with a page of its matches.
#[derive(Debug, Clone)]
pub struct MatchRow {
    /// Note ordinal (ascending native order).
    pub ordinal: u64,
    /// Portable vault-relative path.
    pub path: String,
    /// Indexed title.
    pub title: String,
    /// Exact total matches for this note.
    pub note_matches: u64,
    /// Half-open `TextByte` spans on this page.
    pub matches: Vec<(u64, u64)>,
}

/// Errors from Session commands.
#[derive(Debug, Error)]
pub enum SessionError {
    #[error("Buffer not found: {0}")]
    BufferNotFound(PathBuf),
    #[error("Buffer already open: {0}")]
    BufferAlreadyOpen(PathBuf),
    #[error("Stale revision: expected {expected:?}, got {actual:?}")]
    StaleRevision {
        expected: Revision,
        actual: Revision,
    },
    #[error("Note not found on disk: {0}")]
    NoteNotFound(PathBuf),
    #[error("Save conflict: file changed on disk")]
    SaveConflict,
    #[error("Vault is read-only")]
    VaultReadOnly,
    #[error("Operation would exceed budget")]
    BudgetExceeded,
}

/// Event emitted by the Session.
#[derive(Debug, Clone)]
pub enum SessionEvent {
    /// Buffer state changed.
    BufferStateChanged(BufferStateChanged),
    /// Vault changed.
    VaultChanged { summary: ReconcileSummary },
    /// Conflict detected.
    ConflictDetected { path: PathBuf },
    /// Recovery available.
    RecoveryAvailable { path: PathBuf },
    /// An external writer changed or deleted a note on disk.
    ExternalChangeDetected { path: PathBuf, deleted: bool },
    /// The session caught a panic in a command transition or pump turn.
    /// The session remains operable; the host decides whether to continue
    /// or restart.
    Panicked { message: String, panics_total: u64 },
}

/// A correlated D9 rename flight: source, destination, and the rename
/// worker's reply channel.
type RenameFlight = (
    PathBuf,
    PathBuf,
    crossbeam::channel::Receiver<crate::actor::VaultActorResp>,
);

/// Per-buffer journal memo (C1). Lives exactly as long as the buffer:
/// created on first tracked journal interaction, removed by `close_note`,
/// FIFO-capped at `JOURNAL_MEMO_CAP` keys as a safety net.
#[derive(Debug, Default)]
struct BufferJournalMemo {
    /// Outcome of each journaled patch already applied to this buffer.
    applied: HashMap<(String, u64), Result<u64, String>>,
    /// Insertion order of keys held by either substructure below. One
    /// deque orders both: FIFO eviction of the front key removes it from
    /// whichever substructure still holds it, so `cancelled` retention is
    /// bounded too and never outlives the buffer (FR-3).
    order: VecDeque<(String, u64)>,
    /// Journal items cancelled by undo on this buffer (redo must refuse).
    cancelled: HashSet<(String, u64)>,
}

impl BufferJournalMemo {
    /// Make room for one fresh key: evict oldest-first until under the cap.
    fn make_room(&mut self) {
        while self.order.len() >= JOURNAL_MEMO_CAP {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            self.applied.remove(&oldest);
            self.cancelled.remove(&oldest);
        }
    }

    /// Record a patch outcome for this buffer. Updating a key that already
    /// has a slot keeps the slot (no re-ordering, no growth).
    fn memoize_applied(&mut self, key: (String, u64), result: Result<u64, String>) {
        if let std::collections::hash_map::Entry::Occupied(mut entry) =
            self.applied.entry(key.clone())
        {
            *entry.get_mut() = result;
            return;
        }
        if !self.cancelled.contains(&key) {
            self.make_room();
            self.order.push_back(key.clone());
        }
        self.applied.insert(key, result);
    }

    /// Record an undo-driven cancel for this buffer (redo refuses).
    fn memoize_cancelled(&mut self, key: (String, u64)) {
        if self.cancelled.contains(&key) {
            return;
        }
        if !self.applied.contains_key(&key) {
            self.make_room();
            self.order.push_back(key.clone());
        }
        self.cancelled.insert(key);
    }
}

/// Downcast a panic payload through the `&str` / `String` chain with a
/// debug-format backstop, for containment logging and events.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("{payload:?}")
    }
}

/// The Session itself.
///
/// This is the headless core that owns BufferManager and coordinates
/// all document operations.
#[derive(Debug)]
pub struct Session {
    /// Serializes every command transition and worker reply application.
    driving_turn: Mutex<()>,
    pub(crate) structure_worker: crate::properties::StructureWorker,
    /// Vault identity.
    identity: VaultIdentity,
    /// Vault actor handle.
    actor: VaultActorHandle,
    /// Lossless watcher observations from VaultActor, applied on Session's
    /// driving thread so buffers remain single-owner state.
    actor_events: crossbeam::channel::Receiver<crate::actor::VaultActorEvent>,
    actor_buffer_patches: crossbeam::channel::Receiver<crate::actor::ApplyBufferPatchRequest>,
    /// Session-level observations retained until the host drains them.
    session_events: Mutex<VecDeque<SessionEvent>>,
    /// Idle-save deadline state, keyed by the authoritative buffer path.
    last_edit: Mutex<HashMap<PathBuf, std::time::Instant>>,
    /// Prevent repeated autosave admission for an unchanged failed revision.
    autosave_attempted: Mutex<HashMap<PathBuf, Revision>>,
    autosave_idle_interval: Duration,
    rename_strategy: RenameStrategy,
    /// Buffer manager.
    buffer_manager: Arc<Mutex<BufferManager>>,
    /// RFC-0005 S5: monotonic graph-stamp version shared with the buffer
    /// manager (bumped at every stamp-relevant transition). The graph
    /// service's wake loop compares it against its last pump — a lock-free,
    /// allocation-free "buffer stamp unchanged" check (FR-3).
    graph_stamp_version: Arc<AtomicU64>,
    /// RFC-0005 S6: the buffer-transition watch (§5.1). Session constructs
    /// the pair — the sending end lives in the buffer manager's audited
    /// choke points — and hands the receiving end to the graph service's
    /// wake select via [`Self::graph_buffer_watch`].
    buffer_watch: BufferWatch,
    /// Latest immutable text snapshot per open buffer. Views/workers retain
    /// older revisions through Arc while the cache itself remains bounded to
    /// one revision per buffer.
    shared_snapshots: Mutex<HashMap<BufferId, (Revision, std::sync::Arc<str>)>>,
    /// Autosave failures observed by the host pump.
    autosave_failures_total: AtomicU64,
    /// Save requests admitted by autosave, so asynchronous failures are counted too.
    autosave_requests: Mutex<HashSet<u64>>,
    /// Paths that were absent when opened; creation uses an absent-file guard.
    absent_on_open: Arc<Mutex<HashSet<PathBuf>>>,
    /// Dirty-buffer recovery checkpoints. The host schedules `recovery_tick`
    /// using the configured buffer recovery interval.
    recovery_writer: Option<Arc<Mutex<RecoveryWriter>>>,
    /// Background recovery checkpoint driver owned by this Session.
    recovery_worker: Option<std::thread::JoinHandle<()>>,
    recovery_stop: Arc<std::sync::atomic::AtomicBool>,
    /// Request ID counter.
    request_counter: AtomicU64,
    /// Contained panics across this Session's turn entry points (`execute`,
    /// `pump`). Atomic so hosts can read the operator ground truth without
    /// locks (NFR-5).
    panics_total: AtomicU64,
    /// Poison-recovering lock acquisitions, mirrored (monotonic max) from
    /// `sync::POISON_RECOVERIES` at `drain_events` time. Advisory mirror;
    /// the counter stays the single observability surface across crates.
    poison_recoveries_total: AtomicU64,
    /// One active save and at most one coalesced successor per buffer.
    save_flights: Mutex<HashMap<u64, SaveFlight>>,
    /// Correlated reconcile workers.
    reconcile_flights:
        Mutex<HashMap<u64, crossbeam::channel::Receiver<crate::actor::VaultActorResp>>>,
    /// Correlated D9 rename operations.
    rename_flights: Mutex<HashMap<u64, RenameFlight>>,
    /// Coalesced cold-open flights keyed by native path.
    open_flights: Mutex<HashMap<PathBuf, OpenFlight>>,
    /// Job queue for the bounded open-read worker pool.
    open_jobs: Option<crossbeam::channel::Sender<OpenJob>>,
    /// Completed reads awaiting installation on a Session turn.
    open_results: crossbeam::channel::Receiver<OpenRead>,
    /// Per-buffer journal memos (C1): merged replacement of the former
    /// ownerless global maps. Keyed by
    /// `BufferId` so the memo's lifetime is provably the buffer's —
    /// removed whole by `close_note`, FIFO-capped per buffer at
    /// `JOURNAL_MEMO_CAP`.
    journal_memos: Mutex<HashMap<BufferId, BufferJournalMemo>>,
    /// Lossless terminal results. Admission is bounded by the pending-save cap.
    completions: Mutex<VecDeque<OperationCompletion>>,
    /// Wake edge for the headless wait loops, paired with `completions`
    /// (S6): `record_completion` is the only writer and notifies under no
    /// lock it holds past the push — waiters re-check under the same mutex
    /// they park on, so no wakeup can be lost by construction.
    completions_cv: Condvar,
    /// Pulse channel.
    pulse: PulseChannel,
    /// Notice channel.
    notice: NoticeChannel,
    /// Journal recovery findings waiting for the first host pump.
    recovery_notices: Mutex<VecDeque<&'static str>>,
    /// RFC-0006 §6.10: device-local confirmed source-format hints with their
    /// atomically published immutable view. Open/watch/reconcile workers
    /// acquire the view; confirmations go through the same Session turn.
    encoding_hints: EncodingHintsState,
    /// RFC-0006 S01: core-issued replacement-plan registry (D-06/§6.9).
    /// Unconsumed plans expire after [`TEXT_PLAN_TTL`]; consumed results are
    /// replayable for [`TEXT_PLAN_RESULT_TTL`]; both are reclaimed by the
    /// admission sweep, so long-running sessions never require a restart.
    text_plans: Mutex<HashMap<u128, TextPlanEntry>>,
    text_plan_nonce: u64,
    text_plan_counter: AtomicU64,
    /// Bounded prepare workers' job queue (D-06: preparation runs off the
    /// host turn).
    text_jobs: Option<crossbeam::channel::Sender<TextJob>>,
    /// Queue-depth view for bounded admission (NFR-10). Observes the same
    /// channel the workers pull from; len() counts pending jobs.
    text_queue: Option<crossbeam::channel::Receiver<TextJob>>,
    /// Completed preparations awaiting installation on a Session turn.
    text_results: crossbeam::channel::Receiver<TextOutcomeMessage>,
    /// RFC-0006 S07: bounded vault source-find/preview workers' job queue.
    replace_jobs: Option<crossbeam::channel::Sender<ReplaceJob>>,
    /// Queue-depth view for bounded admission (NFR-10).
    replace_queue: Option<crossbeam::channel::Receiver<ReplaceJob>>,
    /// Completed S07 scans awaiting installation on a Session turn.
    replace_results: crossbeam::channel::Receiver<ReplaceOutcomeMessage>,
    /// RFC-0006 S07: live source-finder sessions (bounded).
    finders: Mutex<HashMap<u128, SourceFinderEntry>>,
    finder_nonce: u64,
    finder_counter: AtomicU64,
    /// RFC-0006 S07: live sealed previews (bounded).
    previews: Mutex<HashMap<u128, PreviewEntry>>,
    preview_nonce: u64,
    preview_counter: AtomicU64,
    /// RFC-0006 S07: request ids of admitted-but-not-installed replace
    /// jobs (so `wait_for_operation` treats them as in-flight).
    pending_replace: Mutex<HashSet<u64>>,
    /// In-flight `text.find`/`text.prepare_replace` request ids (lost on
    /// drain: `wait_for_operation` needs them to stay reported as in
    /// flight until the text worker's completion lands).
    pending_text: Mutex<HashSet<u64>>,
    /// RFC-0006 S08: per-note source leases (non-replacing; D-10).
    leases: Arc<crate::source_leases::SourceLeaseRegistry>,
    /// RFC-0006 S08: durable replacement jobs by job id.
    replace_jobs_map: Mutex<HashMap<u128, JobEntry>>,
    /// RFC-0006 S08: one active apply per vault (D-10).
    active_apply: Mutex<Option<u128>>,
    /// Private OS data directory (recovery/operation key store).
    data_dir: Option<PathBuf>,
    /// Semantic link plan store (W05). Plans are process-local capabilities.
    link_plans: std::sync::Arc<Mutex<crate::link::LinkPlanStore>>,
    /// Bounded prepare workers' job queue.
    link_jobs: Option<crossbeam::channel::Sender<LinkJob>>,
    /// Completed prepare results awaiting installation on a Session turn.
    link_results: crossbeam::channel::Receiver<LinkOutcomeMessage>,
    /// RFC-0005 S6: writer-side broadcast of stamp-relevant buffer
    /// transitions (revision bump, state dirty/clean, path change,
    /// base-hash change). `None` when no consumer wired a watch (CLI/
    /// tests): transitions are still counted on `buffer_stamp_version`.
    buffer_signal: Option<BufferSignal>,
    /// Monotonic version of `graph_buffer_stamp()` output (RFC-0005 S5):
    /// incremented on every transition that can change the stamp, so the
    /// graph service can detect "stamp unchanged" without an actor hop.
    /// Advisory: a mutation that does not change the stamp (e.g. a save of
    /// an already-clean buffer) is a harmless false positive — it only
    /// arms a debounce.
    buffer_stamp_version: AtomicU64,
}

/// One queued semantic-link prepare job (W05).
struct LinkJob {
    request_id: u64,
    request: crate::link::LinkPrepareRequest,
}

/// One finished prepare awaiting installation (W05).
struct LinkOutcomeMessage {
    request_id: u64,
    result: SessionResult,
}

/// One queued replacement-prepare or find job (RFC-0006 S01/S03/D-06). The
/// immutable snapshot text rides as a cheap `Arc` clone; the worker never
/// touches the buffer manager.
struct TextJob {
    request_id: u64,
    text: std::sync::Arc<str>,
    work: TextWork,
}

/// Which prepare computation a queued job performs.
enum TextWork {
    Prepare(crate::text_edit::ReplaceRequest),
    Find(crate::text_edit::FindRequest),
}

/// One finished preparation awaiting registry installation or completion
/// routing.
enum TextOutcomeMessage {
    Prepare {
        request_id: u64,
        buffer_id: BufferId,
        revision: Revision,
        outcome: Result<(Vec<EditOp>, u64, u64), crate::text_edit::PrepareError>,
    },
    Find {
        request_id: u64,
        buffer_id: BufferId,
        revision: Revision,
        outcome: Result<crate::text_edit::FindOutcome, crate::text_edit::PrepareError>,
    },
}

/// One queued vault source-find or replacement-preview job (RFC-0006
/// S07). The worker owns the scan; the Session installs the result.
struct ReplaceJob {
    request_id: u64,
    work: ReplaceWork,
}

/// Which S07/S08 computation a queued job performs.
#[allow(clippy::large_enum_variant)]
enum ReplaceWork {
    Find {
        finder_id: u128,
        request: crate::replace::preview::SourceFindRequest,
    },
    Preview {
        preview_id: u128,
        request: crate::replace::preview::PreviewRequest,
    },
    /// RFC-0006 S08: run one durable apply (or resume) loop.
    Apply {
        job_id: u128,
        ctx: crate::replace::CoordinatorCtx,
        key: crate::replace::OperationKey,
        store: crate::replace::ReplaceStore,
        state: Arc<Mutex<JobState>>,
        cancel: Arc<AtomicBool>,
    },
    /// RFC-0006 S09/D-11: run one guarded Revert action epoch.
    Revert {
        job_id: u128,
        ctx: crate::replace::CoordinatorCtx,
        key: crate::replace::OperationKey,
        store: crate::replace::ReplaceStore,
        state: Arc<Mutex<JobState>>,
        cancel: Arc<AtomicBool>,
    },
}

/// One finished S07 scan / S08 apply awaiting installation.
enum ReplaceOutcomeMessage {
    Find {
        request_id: u64,
        finder_id: u128,
        outcome: Result<
            Box<crate::replace::preview::SourceFindOutcome>,
            crate::replace::preview::PreviewError,
        >,
    },
    Preview {
        request_id: u64,
        preview_id: u128,
        outcome: Result<
            Box<crate::replace::preview::PreviewManifest>,
            crate::replace::preview::PreviewError,
        >,
    },
    ApplyDone {
        request_id: u64,
        job_id: u128,
        phase: crate::replace::JobPhase,
        totals: crate::replace::JobTotals,
    },
    ApplyFailed {
        request_id: u64,
        job_id: u128,
        message: String,
    },
}

/// Which action epoch a job is currently running (RFC-0006 S09/D-11):
/// apply runs the sealed forward replacement; revert restores the sealed
/// before-payloads. The mode is persisted implicitly by the action records
/// in the authenticated log and reconstructed on restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobMode {
    Apply,
    Revert,
}

/// Shared job state (RFC-0006 S08/S09): the worker mutates items; the
/// Session reads phase/totals for status and idempotent receipts.
struct JobState {
    phase: crate::replace::JobPhase,
    mode: JobMode,
    items: Vec<crate::replace::JobItem>,
    totals: crate::replace::JobTotals,
}

impl JobState {
    fn recompute(&mut self) {
        self.totals = crate::replace::JobTotals::from_items(&self.items);
    }
}

/// A live durable replacement job entry (RFC-0006 S08).
struct JobEntry {
    state: Arc<Mutex<JobState>>,
    cancel: Arc<AtomicBool>,
    /// The preview this job applied (freeze identity).
    preview_id: u128,
    selection_revision: u64,
    /// The action-request key of the last admitted command (idempotent
    /// replay within this session returns the same receipt).
    idempotency_key: Option<u128>,
}

impl std::fmt::Debug for JobEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let phase = self.state.lock_recovering().phase;
        f.debug_struct("JobEntry")
            .field("phase", &phase)
            .field("selection_revision", &self.selection_revision)
            .field("cancelled", &self.cancel.load(Ordering::Relaxed))
            .finish()
    }
}

/// A live source-finder session (S07): spool location plus the in-memory
/// per-note index; spans live in the private spool.
#[derive(Debug, Clone)]
struct SourceFinderEntry {
    /// Finder id (128-bit hex on the wire).
    id: u128,
    /// Unix-ms expiry (30-minute total lifetime).
    expires_at_unix_ms: u64,
    /// Frozen scope.
    scope: crate::replace::ScopeVersion,
    /// Scope truncation flag.
    truncated_scope: bool,
    /// Per-note records.
    notes: Vec<crate::replace::preview::FindNoteRecord>,
    /// Typed exclusions.
    exclusions: Vec<crate::replace::NoteExclusion>,
    /// Total scoped notes.
    total_notes: u64,
    /// Exact total matches.
    total_matches: u64,
}

/// A live sealed preview (S07): the manifest is authoritative on disk; the
/// entry caches identity/expiry for access-time checks.
#[derive(Debug)]
struct PreviewEntry {
    /// Unix-ms expiry.
    expires_at_unix_ms: u64,
    /// Cached manifest (loaded once at seal time).
    manifest: Arc<crate::replace::PreviewManifest>,
    /// Currently selected item ids (canonical order; S08 freezes these on
    /// the first Apply).
    selected_ids: Vec<u64>,
    /// Monotonic selection revision counter (S07/D-09).
    selection_revision: u64,
}

/// One core-issued replacement plan (D-06/§6.9). `consumed` records the
/// replayable receipt for the 10-minute retry window.
struct TextPlanEntry {
    ops: Vec<EditOp>,
    match_count: u64,
    options_digest: u64,
    buffer_id: BufferId,
    revision: Revision,
    created: std::time::Instant,
    consumed: Option<(std::time::Instant, SessionResult)>,
}

impl std::fmt::Debug for TextPlanEntry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The receipt content is never part of diagnostics (§6.7: no
        // note/query text in logs); only presence is reported.
        f.debug_struct("TextPlanEntry")
            .field("match_count", &self.match_count)
            .field("options_digest", &self.options_digest)
            .field("buffer_id", &self.buffer_id)
            .field("revision", &self.revision)
            .field("consumed", &self.consumed.is_some())
            .finish()
    }
}

impl Session {
    /// Create a new Session without a buffer-transition broadcast.
    pub fn new(
        identity: VaultIdentity,
        actor: VaultActorHandle,
        buffer_manager: BufferManager,
        pulse: PulseChannel,
        notice: NoticeChannel,
        config: VaultConfig,
    ) -> Self {
        Self::new_inner(identity, actor, buffer_manager, pulse, notice, config, None)
    }

    /// Create a new Session with an explicit OS data directory (S8/M3).
    ///
    /// Library code never reads the data-directory override; binaries and
    /// tests pass the resolved directory here. `None` selects the resolved
    /// platform default. The recovery checkpoint store roots under this
    /// directory.
    pub fn new_with_data_dir(
        identity: VaultIdentity,
        actor: VaultActorHandle,
        buffer_manager: BufferManager,
        pulse: PulseChannel,
        notice: NoticeChannel,
        config: VaultConfig,
        data_dir: Option<PathBuf>,
    ) -> Self {
        Self::new_inner(
            identity,
            actor,
            buffer_manager,
            pulse,
            notice,
            config,
            data_dir,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_inner(
        identity: VaultIdentity,
        actor: VaultActorHandle,
        mut buffer_manager: BufferManager,
        pulse: PulseChannel,
        notice: NoticeChannel,
        config: VaultConfig,
        data_dir: Option<PathBuf>,
    ) -> Self {
        let actor_events = actor.events();
        let actor_buffer_patches = actor.buffer_patch_requests();
        // RFC-0005 S5: capture the stamp-version handle before the manager
        // moves into its mutex — the graph service reads it lock-free.
        let graph_stamp_version = buffer_manager.graph_stamp_handle();
        // RFC-0005 S6: the buffer-transition broadcast. Session constructs
        // the pair (§5.1); the manager's audited choke points carry the
        // sending end, and the graph service takes the watch (its wake
        // select's buffer arm). A taken-by-spy watch (tests) leaves the
        // service on its inert stub — lossy, recovered by the sweep, never
        // incorrect.
        let (buffer_signal, buffer_watch) = BufferSignal::new();
        buffer_manager.set_graph_stamp_notifier(buffer_signal.notifier());
        let manager = Arc::new(Mutex::new(buffer_manager));
        let recovery_writer = crate::identity::resolve_data_dir(data_dir.as_deref()).map(|data| {
            let dir = data
                .join("flavite")
                .join("vaults")
                .join(identity.uuid.to_string())
                .join("recovery");
            Arc::new(Mutex::new(RecoveryWriter::new(RecoveryConfig::new(
                identity.uuid.to_string(),
                dir,
                manager.lock_recovering().recovery_interval(),
            ))))
        });
        let recovery_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let recovery_worker = recovery_writer.as_ref().map(|writer| {
            let manager = manager.clone();
            let writer = writer.clone();
            let stop = recovery_stop.clone();
            let root = identity.root.clone();
            let actor = actor.clone();
            std::thread::Builder::new()
                .name("vault-recovery".into())
                .spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        // Decide admission from tiny stamps before copying
                        // text or hashing a document. Never wait for the
                        // recovery writer's disk work while holding the
                        // buffer manager needed by interactive edits.
                        let candidates = {
                            let manager = manager.lock_recovering();
                            manager
                                .dirty_buffers()
                                .into_iter()
                                .map(|(_, buffer)| (buffer.buffer_id(), buffer.revision()))
                                .collect::<Vec<_>>()
                        };
                        let due = {
                            let writer = writer.lock_recovering();
                            candidates
                                .into_iter()
                                .filter(|(id, revision)| {
                                    writer.should_write_revision(*id, *revision)
                                })
                                .map(|(id, _)| id)
                                .collect::<Vec<_>>()
                        };
                        let buffer_manager = manager.lock_recovering();
                        let dirty = due
                            .into_iter()
                            .filter_map(|id| {
                                let buffer = buffer_manager.get_buffer_by_id(id)?;
                                if !buffer.is_dirty() {
                                    return None;
                                }
                                Some((buffer_manager.path_for_id(id)?, buffer))
                            })
                            .map(|(path, buffer)| {
                                (
                                    native_note_key(path, &root),
                                    buffer.base_bytes().to_vec(),
                                    flavite_buffer::recovery::RecoveryFile::from_buffer(
                                        buffer, 0, false,
                                    ),
                                )
                            })
                            .collect::<Vec<_>>();
                        drop(buffer_manager);
                        for (key, base, snapshot) in dirty {
                            let due = writer.lock_recovering().should_write_snapshot(&snapshot);
                            if due && actor.pin_merge_base(key.clone(), base).is_ok() {
                                writer.lock_recovering().write_snapshot(&key, snapshot);
                            }
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                })
                .expect("recovery worker thread creation failed")
        });
        let (open_job_sender, open_job_receiver) = crossbeam::channel::unbounded::<OpenJob>();
        let (open_result_sender, open_result_receiver) =
            crossbeam::channel::unbounded::<OpenRead>();
        // RFC-0006 §6.10: device-local encoding hints. The store lives under
        // the resolved OS data root, bound to this vault's identity; a store
        // or record failure is a typed pending state surfaced through the
        // returned issue string, never a silent reset of confirmed choices.
        let encoding_hints = {
            let store = crate::identity::resolve_data_dir(data_dir.as_deref()).and_then(|data| {
                crate::user_state::UserStateStore::open_at(
                    Box::new(flavite_fs::OsFs::new()),
                    &data,
                    &identity.uuid.to_string(),
                )
                .ok()
            });
            let (state, issue) = EncodingHintsState::open(store, identity.uuid.to_string());
            if let Some(issue) = issue {
                tracing::warn!(vault = %identity.uuid, %issue, "encoding hints unavailable");
            }
            state
        };
        for worker_index in 0..OPEN_READ_WORKERS {
            let jobs = open_job_receiver.clone();
            let results = open_result_sender.clone();
            let actor = actor.clone();
            let recovery_writer = recovery_writer.clone();
            std::thread::Builder::new()
                .name(format!("vault-open-{worker_index}"))
                .spawn(move || {
                    // Workers exit when the Session drops the job sender.
                    while let Ok(job) = jobs.recv() {
                        let outcome = match job.policy {
                            // Explicit creation (W10): refuse collisions, ask
                            // the actor to create the empty file, then re-read
                            // so installation sees the on-disk snapshot.
                            OpenPolicy::CreateNew => match actor.read_file(job.path.clone()) {
                                Ok(Some(_)) => {
                                    Err(format!("note already exists: {}", job.path.display()))
                                }
                                Ok(None) => actor
                                    .create_empty_note(job.path.clone())
                                    .and_then(|()| {
                                        actor
                                            .read_file(job.path.clone())
                                            .map_err(|error| error.to_string())
                                    })
                                    .and_then(|content| {
                                        content.ok_or_else(|| {
                                            "created note disappeared before open".to_owned()
                                        })
                                    })
                                    .map(Some),
                                Err(error) => Err(error.to_string()),
                            },
                            OpenPolicy::CreateIfMissing | OpenPolicy::ExistingOnly => actor
                                .read_file(job.path.clone())
                                .map_err(|error| error.to_string()),
                        };
                        let recovery = recovery_writer.as_ref().and_then(|writer| {
                            writer.lock_recovering().read_for_note_key(&job.note_key)
                        });
                        // Sending only fails when the Session is gone; the
                        // read result is then unobservable by construction.
                        let _ = results.send(OpenRead {
                            path: job.path,
                            policy: job.policy,
                            outcome,
                            recovery,
                        });
                    }
                })
                .expect("open worker thread creation failed");
        }
        let link_plans = std::sync::Arc::new(Mutex::new(crate::link::LinkPlanStore::default()));
        let (link_job_sender, link_job_receiver) = crossbeam::channel::unbounded::<LinkJob>();
        let (link_result_sender, link_result_receiver) =
            crossbeam::channel::unbounded::<LinkOutcomeMessage>();
        {
            for worker_index in 0..2 {
                let root = identity.root.clone();
                let manager = manager.clone();
                let actor = actor.clone();
                let link_plans = link_plans.clone();
                let jobs = link_job_receiver.clone();
                let results = link_result_sender.clone();
                std::thread::Builder::new()
                    .name(format!("vault-link-{worker_index}"))
                    .spawn(move || {
                        // Workers exit when the Session drops the job sender.
                        while let Ok(job) = jobs.recv() {
                            let context = crate::link::PrepareContext {
                                manager: &manager,
                                actor: &actor,
                                root: &root,
                            };
                            let outcome = match crate::link::prepare_work(&context, &job.request) {
                                Ok((plan, mut prepared)) => {
                                    let plan_id = {
                                        let mut store = link_plans.lock_recovering();
                                        let plan_id = store.allocate_plan_id();
                                        prepared.plan_id = plan_id;
                                        if store.complete_prepare(plan_id, plan) {
                                            Some(plan_id)
                                        } else {
                                            None
                                        }
                                    };
                                    match plan_id {
                                        Some(plan_id) => {
                                            SessionResult::Success(CommandResult::LinkPrepared {
                                                prepared: prepared.with_plan_id(plan_id),
                                            })
                                        }
                                        None => SessionResult::LinkRejected {
                                            reason: crate::link::LinkInsertionError::IntentExpired,
                                        },
                                    }
                                }
                                Err(reason) => {
                                    link_plans
                                        .lock_recovering()
                                        .fail_prepare(job.request.intent);
                                    SessionResult::LinkRejected { reason }
                                }
                            };
                            // Sending only fails when the Session is gone;
                            // the result is then unobservable by construction.
                            let _ = results.send(LinkOutcomeMessage {
                                request_id: job.request_id,
                                result: outcome,
                            });
                        }
                    })
                    .expect("link worker thread creation failed");
            }
        }
        let (text_job_sender, text_job_receiver) = crossbeam::channel::unbounded::<TextJob>();
        let (text_result_sender, text_result_receiver) =
            crossbeam::channel::unbounded::<TextOutcomeMessage>();
        {
            for worker_index in 0..TEXT_PREPARE_WORKERS {
                let jobs = text_job_receiver.clone();
                let results = text_result_sender.clone();
                std::thread::Builder::new()
                    .name(format!("vault-text-{worker_index}"))
                    .spawn(move || {
                        // Workers exit when the Session drops the job sender.
                        while let Ok(job) = jobs.recv() {
                            let message = match job.work {
                                TextWork::Prepare(request) => TextOutcomeMessage::Prepare {
                                    request_id: job.request_id,
                                    buffer_id: request.buffer_id,
                                    revision: request.revision,
                                    outcome: crate::text_edit::prepare_ops(&job.text, &request),
                                },
                                TextWork::Find(request) => TextOutcomeMessage::Find {
                                    request_id: job.request_id,
                                    buffer_id: request.buffer_id,
                                    revision: request.revision,
                                    outcome: crate::text_edit::prepare_find(&job.text, &request),
                                },
                            };
                            // Sending only fails when the Session is gone;
                            // the result is then unobservable by construction.
                            let _ = results.send(message);
                        }
                    })
                    .expect("text prepare worker thread creation failed");
            }
        }
        let text_plan_nonce = u64::from_le_bytes(
            blake3::hash(identity.uuid.as_bytes()).as_bytes()[..8]
                .try_into()
                .expect("fixed-size slice"),
        );
        let (replace_job_sender, replace_job_receiver) =
            crossbeam::channel::unbounded::<ReplaceJob>();
        let (replace_result_sender, replace_result_receiver) =
            crossbeam::channel::unbounded::<ReplaceOutcomeMessage>();
        {
            let ctx = crate::replace::preview::PreviewCtx {
                actor: actor.clone(),
                manager: manager.clone(),
                root: identity.root.clone(),
                vault_uuid: identity.uuid,
            };
            for worker_index in 0..REPLACE_PREPARE_WORKERS {
                let jobs = replace_job_receiver.clone();
                let results = replace_result_sender.clone();
                let ctx = ctx.clone();
                std::thread::Builder::new()
                    .name(format!("vault-replace-{worker_index}"))
                    .spawn(move || {
                        // Workers exit when the Session drops the job sender.
                        while let Ok(job) = jobs.recv() {
                            // Bound the working set: a cancelled scan exits
                            // at its next checkpoint.
                            let cancelled = std::sync::atomic::AtomicBool::new(false);
                            let message = match job.work {
                                ReplaceWork::Find { finder_id, request } => {
                                    let outcome = crate::replace::preview::run_find(
                                        &ctx,
                                        &request,
                                        &hex::encode(finder_id.to_be_bytes()),
                                        &crate::replace::ActorOpStoreIo::new(ctx.actor.clone()),
                                        &cancelled,
                                    );
                                    ReplaceOutcomeMessage::Find {
                                        request_id: job.request_id,
                                        finder_id,
                                        outcome: outcome.map(Box::new),
                                    }
                                }
                                ReplaceWork::Preview {
                                    preview_id,
                                    request,
                                } => {
                                    let outcome = crate::replace::preview::run_preview(
                                        &ctx,
                                        &request,
                                        &hex::encode(preview_id.to_be_bytes()),
                                        &crate::replace::ReplaceStore::new(&ctx.root),
                                        &crate::replace::ActorOpStoreIo::new(ctx.actor.clone()),
                                        &cancelled,
                                    );
                                    ReplaceOutcomeMessage::Preview {
                                        request_id: job.request_id,
                                        preview_id,
                                        outcome: outcome.map(Box::new),
                                    }
                                }
                                ReplaceWork::Apply {
                                    job_id,
                                    ctx,
                                    key,
                                    store,
                                    state,
                                    cancel,
                                }
                                | ReplaceWork::Revert {
                                    job_id,
                                    ctx,
                                    key,
                                    store,
                                    state,
                                    cancel,
                                } => {
                                    let job_hex = hex::encode(job_id.to_be_bytes());
                                    let io = crate::replace::ActorOpStoreIo::new(ctx.actor.clone());
                                    let data_dir =
                                        crate::identity::resolve_data_dir(ctx.data_dir.as_deref());
                                    let run = || -> Result<
                                        (crate::replace::JobPhase, crate::replace::JobTotals),
                                        String,
                                    > {
                                        let data_dir = data_dir.ok_or_else(|| {
                                            "private data directory is unavailable".to_owned()
                                        })?;
                                        let mut log = crate::replace::JobLog::open(
                                            Box::new(io.clone()),
                                            &store,
                                            &job_hex,
                                            key,
                                            &data_dir,
                                            &ctx.vault_uuid,
                                        )
                                        .map_err(|error| error.to_string())?;
                                        // Release the state mutex for the
                                        // whole run: Cancel/Status/Resume must
                                        // stay observable while items process
                                        // (D-10, NFR-12). The durable outcome
                                        // is published back afterwards.
                                        let (mode, mut items) = {
                                            let mut guard = state.lock_recovering();
                                            guard.recompute();
                                            (guard.mode, std::mem::take(&mut guard.items))
                                        };
                                        let (phase, totals) = match mode {
                                            JobMode::Revert => crate::replace::run_revert(
                                                &ctx,
                                                &job_hex,
                                                &mut items,
                                                &cancel,
                                                &mut log,
                                                &store,
                                                &io,
                                            ),
                                            JobMode::Apply => crate::replace::run_apply(
                                                &ctx,
                                                &job_hex,
                                                &mut items,
                                                &cancel,
                                                &mut log,
                                                &store,
                                                &io,
                                            ),
                                        };
                                        let mut guard = state.lock_recovering();
                                        guard.items = items;
                                        guard.phase = phase;
                                        guard.totals = totals.clone();
                                        Ok((phase, totals))
                                    };
                                    // A panicking worker must not brick the
                                    // job: without this the worker thread
                                    // dies mid-recv and `active_apply` stays
                                    // set until restart (§6.7 bounded
                                    // shutdown). Report the run as failed.
                                    let outcome = match std::panic::catch_unwind(
                                        std::panic::AssertUnwindSafe(run),
                                    ) {
                                        Ok(outcome) => outcome,
                                        Err(panic) => {
                                            Err(format!("replace worker panicked: {panic:?}"))
                                        }
                                    };
                                    match outcome {
                                        Ok((phase, totals)) => ReplaceOutcomeMessage::ApplyDone {
                                            request_id: job.request_id,
                                            job_id,
                                            phase,
                                            totals,
                                        },
                                        Err(message) => {
                                            let mut guard = state.lock_recovering();
                                            guard.phase = crate::replace::JobPhase::NeedsRecovery;
                                            guard.recompute();
                                            ReplaceOutcomeMessage::ApplyFailed {
                                                request_id: job.request_id,
                                                job_id,
                                                message,
                                            }
                                        }
                                    }
                                }
                            };
                            // Sending only fails when the Session is gone.
                            let _ = results.send(message);
                        }
                    })
                    .expect("replace worker thread creation failed");
            }
        }
        let recovered = Self {
            text_jobs: Some(text_job_sender),
            text_queue: Some(text_job_receiver),
            text_results: text_result_receiver,
            text_plan_nonce,
            text_plan_counter: AtomicU64::new(0),
            text_plans: Mutex::new(HashMap::new()),
            data_dir,
            replace_jobs: Some(replace_job_sender),
            replace_queue: Some(replace_job_receiver),
            replace_results: replace_result_receiver,
            finders: Mutex::new(HashMap::new()),
            finder_nonce: text_plan_nonce,
            finder_counter: AtomicU64::new(0),
            previews: Mutex::new(HashMap::new()),
            preview_nonce: text_plan_nonce,
            preview_counter: AtomicU64::new(0),
            pending_replace: Mutex::new(HashSet::new()),
            pending_text: Mutex::new(HashSet::new()),
            leases: Arc::new(crate::source_leases::SourceLeaseRegistry::new()),
            replace_jobs_map: Mutex::new(HashMap::new()),
            active_apply: Mutex::new(None),
            driving_turn: Mutex::new(()),
            structure_worker: crate::properties::StructureWorker::new(),
            identity,
            actor,
            actor_events,
            actor_buffer_patches,
            open_jobs: Some(open_job_sender),
            open_results: open_result_receiver,
            open_flights: Mutex::new(HashMap::new()),
            session_events: Mutex::new(VecDeque::new()),
            last_edit: Mutex::new(HashMap::new()),
            autosave_attempted: Mutex::new(HashMap::new()),
            autosave_idle_interval: config.autosave_idle_interval,
            rename_strategy: config.rename_strategy,
            buffer_manager: manager,
            graph_stamp_version,
            buffer_watch,
            shared_snapshots: Mutex::new(HashMap::new()),
            autosave_failures_total: AtomicU64::new(0),
            autosave_requests: Mutex::new(HashSet::new()),
            absent_on_open: Arc::new(Mutex::new(HashSet::new())),
            recovery_writer,
            recovery_worker,
            recovery_stop,
            request_counter: AtomicU64::new(0),
            panics_total: AtomicU64::new(0),
            poison_recoveries_total: AtomicU64::new(0),
            save_flights: Mutex::new(HashMap::new()),
            reconcile_flights: Mutex::new(HashMap::new()),
            rename_flights: Mutex::new(HashMap::new()),
            journal_memos: Mutex::new(HashMap::new()),
            completions: Mutex::new(VecDeque::new()),
            completions_cv: Condvar::new(),
            pulse,
            notice,
            recovery_notices: Mutex::new(VecDeque::new()),
            encoding_hints,
            link_plans,
            link_jobs: Some(link_job_sender),
            link_results: link_result_receiver,
            buffer_signal: Some(buffer_signal),
            buffer_stamp_version: AtomicU64::new(0),
        };
        // Recover durable replacement jobs from the private store
        // (RFC-0006 D-11). Runs once at Session construction; never
        // auto-continues untouched items.
        recovered.recover_replace_jobs();
        recovered
    }

    /// The current version of `graph_buffer_stamp()` output (RFC-0005
    /// S5). Monotonic within this process; the graph service compares it
    /// across wakes to skip database work when nothing changed (FR-3).
    #[must_use]
    pub fn buffer_stamp_version(&self) -> u64 {
        self.buffer_stamp_version.load(Ordering::Acquire)
    }

    /// Record one stamp-relevant buffer transition: bump the version
    /// atomic and, when a signal is wired, publish it (lossy; RFC-0005
    /// S6). Callers are all buffer-mutation sites in this module.
    fn note_buffer_transition(&self) {
        self.buffer_stamp_version.fetch_add(1, Ordering::Release);
        if let Some(signal) = &self.buffer_signal {
            signal.notify();
        }
    }

    /// Generate a new request ID.
    fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Execute a command.
    ///
    /// The turn body runs inside an unwinding-catch boundary: a panic in a
    /// command
    /// transition is contained at this boundary, logged, counted in
    /// [`Session::panics_total`], and surfaced as
    /// [`SessionEvent::Panicked`]; the caller receives an honest
    /// [`SessionResult::Error`] naming the panic instead of an unwinding
    /// host. The Session stays operable (locks recover per `sync.rs`;
    /// collections are memory-safe after unwind) — the host decides
    /// whether to continue or restart.
    pub fn execute(&self, command: SessionCommand) -> SessionResult {
        let _turn = self.driving_turn.lock_recovering();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.execute_turn(command)))
        {
            Ok(result) => result,
            Err(payload) => self.record_contained_panic(&payload, "execute"),
        }
    }

    fn execute_turn(&self, command: SessionCommand) -> SessionResult {
        self.pump_external_changes_inner();
        let edited_path = match &command {
            SessionCommand::EditStructure { buffer_id, .. }
            | SessionCommand::ApplyEdits { buffer_id, .. }
            | SessionCommand::Undo { buffer_id, .. }
            | SessionCommand::Redo { buffer_id, .. } => self
                .buffer_manager
                .lock_recovering()
                .path_for_id(*buffer_id)
                .cloned(),
            SessionCommand::LinkInsert { intent, plan_id }
            | SessionCommand::LinkRemove { intent, plan_id } => {
                let source = self
                    .link_plans
                    .lock_recovering()
                    .source_for(*intent, *plan_id);
                source.and_then(|id| {
                    self.buffer_manager
                        .lock_recovering()
                        .path_for_id(id.into())
                        .cloned()
                })
            }
            SessionCommand::ApplyMerge { path, .. }
            | SessionCommand::ResolveConflict { path, .. }
            | SessionCommand::RestoreRecovery { path }
            | SessionCommand::RestoreHistory { path, .. } => Some(path.clone()),
            _ => None,
        };
        let result = self.execute_inner(command);
        if let Some(path) = edited_path
            && matches!(
                &result,
                SessionResult::Success(_) | SessionResult::Pending { .. }
            )
        {
            self.last_edit
                .lock_recovering()
                .insert(path.clone(), std::time::Instant::now());
            self.autosave_attempted.lock_recovering().remove(&path);
        }
        result
    }

    /// Apply queued disk observations on the host pump. GUI hosts call this
    /// from their regular timer; command execution also drains before running.
    ///
    /// Like [`Session::execute`], the whole turn is panic-contained: a panic
    /// in any pump helper is recorded (log + counter +
    /// [`SessionEvent::Panicked`]) and the turn is released. The pump has no
    /// result to fail, so the event is the signal.
    pub fn pump(&self) {
        let _turn = self.driving_turn.lock_recovering();
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.pump_turn()))
        {
            // The pump has no result to fail: the recorded event is the
            // signal and the turn is released when this frame returns.
            self.record_contained_panic(&payload, "pump");
        }
    }

    fn pump_turn(&self) {
        self.pump_external_changes_inner();
        self.pump_recovery_notices();
        self.pump_structure_completions();
        self.pump_link_completions();
        self.pump_text_completions();
        self.pump_replace_completions();
        self.pump_open_completions();
        self.pump_save_completions();
        self.pump_reconcile_completions();
        self.pump_rename_completions();
        self.pump_autosave();
        // Recovery checkpoints run on the dedicated worker created with the
        // Session. Never perform recovery filesystem or actor I/O while this
        // host pump owns the Session driving-turn lock.
    }

    /// Shared containment core called by both turn wrappers.
    ///
    /// Behaviour proven by tests (the six-line catch plumbing in the two
    /// turn wrappers is reviewed by eye): (a) logs at error level, (b) increments
    /// [`Session::panics_total`], (c) emits [`SessionEvent::Panicked`]
    /// through the event channel, (d) returns the honest error result
    /// naming the panic for `execute` (the pump wrapper discards it — the
    /// event is its signal). Never re-panics, never aborts.
    fn record_contained_panic(
        &self,
        payload: &(dyn std::any::Any + Send),
        turn: &'static str,
    ) -> SessionResult {
        let message = panic_message(payload);
        tracing::error!(
            target: "vault::session",
            turn,
            panic = %message,
            "session turn panicked",
        );
        // SeqCst: this counter is the operator's ground truth and is read
        // cross-thread.
        let panics_total = self.panics_total.fetch_add(1, Ordering::SeqCst) + 1;
        self.session_events
            .lock_recovering()
            .push_back(SessionEvent::Panicked {
                message: message.clone(),
                panics_total,
            });
        SessionResult::Error {
            message: format!("session turn panicked during {turn}: {message}"),
        }
    }

    pub(crate) fn structure_completions_full(&self) -> bool {
        self.completions.lock_recovering().len() >= MAX_PENDING_SAVE_OPERATIONS - 64
    }
    pub(crate) fn next_structure_request(&self) -> u64 {
        self.next_request_id()
    }
    fn pump_structure_completions(&self) {
        while let Ok(prepared) = self.structure_worker.results.try_recv() {
            let result = match prepared.patch {
                Ok(op) => self.apply_structure_patch(prepared.buffer, prepared.revision, op),
                Err(message) => SessionResult::ActionRequired {
                    code: "structure.source",
                    data: Some(message),
                },
            };
            if matches!(result, SessionResult::Success(_))
                && let Some(path) = self
                    .buffer_manager
                    .lock_recovering()
                    .path_for_id(prepared.buffer)
                    .cloned()
            {
                self.last_edit
                    .lock_recovering()
                    .insert(path.clone(), std::time::Instant::now());
                self.autosave_attempted.lock_recovering().remove(&path);
            }
            self.record_completion(prepared.request, result);
            self.structure_worker.retired();
        }
    }

    fn pump_link_completions(&self) {
        while let Ok(message) = self.link_results.try_recv() {
            self.record_completion(message.request_id, message.result);
        }
    }

    fn pump_recovery_notices(&self) {
        let codes = self
            .recovery_notices
            .lock_recovering()
            .drain(..)
            .collect::<Vec<_>>();
        for code in codes {
            self.notice
                .send(crate::channels::Notice::ActionRequired { code, note: None });
        }
    }

    fn pump_autosave(&self) {
        let now = std::time::Instant::now();
        let dirty = self
            .buffer_manager
            .lock_recovering()
            .dirty_buffers()
            .into_iter()
            .filter(|(_, buffer)| buffer.allows_autosave())
            // S09/D-11: a draft replacement holds the source lease through
            // its durable checkpoint; autosave must not save the
            // after-text mid-effect. The lease refuses it anyway; skipping
            // avoids noisy failure records for a normal policy pause.
            .filter(|(path, _)| !self.leased_by_replace(path))
            .map(|(path, buffer)| (path.clone(), buffer.buffer_id(), buffer.revision()))
            .collect::<Vec<_>>();
        for (path, buffer_id, revision) in dirty {
            let last_edit = {
                let mut deadlines = self.last_edit.lock_recovering();
                *deadlines.entry(path.clone()).or_insert(now)
            };
            if now.duration_since(last_edit) < self.autosave_idle_interval
                || self.autosave_attempted.lock_recovering().get(&path) == Some(&revision)
                || self
                    .save_flights
                    .lock_recovering()
                    .contains_key(&buffer_id.as_u64())
            {
                continue;
            }
            self.autosave_attempted
                .lock_recovering()
                .insert(path.clone(), revision);
            let result = self.save_note(path.clone());
            match result {
                SessionResult::Pending { request_id } => {
                    self.autosave_requests.lock_recovering().insert(request_id);
                }
                SessionResult::Success(_) => {}
                failure => self.record_autosave_failure(&path, &failure),
            }
        }
    }

    fn record_autosave_failure(&self, path: &Path, result: &SessionResult) {
        self.autosave_failures_total.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(path = %path.display(), result = ?result, "autosave failed");
        if let Err(error) = self.actor.record_autosave_failure() {
            tracing::error!(path = %path.display(), %error, "failed to persist autosave counter");
        }
    }

    fn pump_external_changes_inner(&self) {
        self.pump_buffer_patch_requests();
        while let Ok(event) = self.actor_events.try_recv() {
            match event {
                crate::actor::VaultActorEvent::ExternalChange { path, bytes } => {
                    self.session_events.lock_recovering().push_back(
                        SessionEvent::ExternalChangeDetected {
                            path: path.clone(),
                            deleted: false,
                        },
                    );
                    let mut manager = self.buffer_manager.lock_recovering();
                    // External bytes are strictly decoded under Auto: an
                    // unambiguous new format may be adopted only while the
                    // buffer is clean, and a stale hint never reinterprets
                    // them silently (§6.10). A decode failure leaves the
                    // existing text intact and requests an explicit choice.
                    let decoded = if manager.get_buffer(&path).is_some_and(|buffer| {
                        buffer.state() == flavite_buffer::state::BufferState::Orphaned
                    }) {
                        manager.notify_file_recreated(&path, &bytes, DecodePolicy::Auto)
                    } else {
                        manager.notify_external_change(&path, &bytes, DecodePolicy::Auto)
                    };
                    if decoded.is_err() {
                        self.notice.send(crate::channels::Notice::ActionRequired {
                            code: CHOICE_REQUIRED_CODE,
                            note: Some(flavite_buffer::manager::NoteRef::new(path.clone())),
                        });
                    }
                    self.note_buffer_transition();
                    if manager.get_buffer(&path).is_some_and(|buffer| {
                        buffer.state() == flavite_buffer::state::BufferState::Conflicted
                    }) {
                        self.notice.send(crate::channels::Notice::Conflict {
                            note: flavite_buffer::manager::NoteRef::new(path.clone()),
                        });
                    }
                    let _ = self.pulse.send(crate::channels::Pulse::Invalidate {
                        note: flavite_buffer::manager::NoteRef::new(path),
                    });
                }
                crate::actor::VaultActorEvent::Deleted { path } => {
                    self.session_events.lock_recovering().push_back(
                        SessionEvent::ExternalChangeDetected {
                            path: path.clone(),
                            deleted: true,
                        },
                    );
                    let mut manager = self.buffer_manager.lock_recovering();
                    if !manager.notify_file_deleted(&path).is_empty() {
                        self.note_buffer_transition();
                        self.notice.send(crate::channels::Notice::ActionRequired {
                            code: "external_delete",
                            note: Some(flavite_buffer::manager::NoteRef::new(path)),
                        });
                    }
                }
                crate::actor::VaultActorEvent::Renamed { from, to, .. } => {
                    let mut manager = self.buffer_manager.lock_recovering();
                    if manager.rename_buffer(&from, to.clone()) {
                        drop(manager);
                        {
                            let mut last_edit = self.last_edit.lock_recovering();
                            if let Some(edit_time) = last_edit.remove(&from) {
                                last_edit.insert(to.clone(), edit_time);
                            }
                        }
                        {
                            let mut attempted = self.autosave_attempted.lock_recovering();
                            if let Some(revision) = attempted.remove(&from) {
                                attempted.insert(to.clone(), revision);
                            }
                        }
                        {
                            let mut absent = self.absent_on_open.lock_recovering();
                            if absent.remove(&from) {
                                absent.insert(to.clone());
                            }
                        }
                        let _ = self.pulse.send(crate::channels::Pulse::Invalidate {
                            note: flavite_buffer::manager::NoteRef::new(to),
                        });
                    }
                }
                crate::actor::VaultActorEvent::WatcherLost => {
                    self.notice.send(crate::channels::Notice::ActionRequired {
                        code: "watcher_lost",
                        note: None,
                    });
                }
                crate::actor::VaultActorEvent::RenameSuggested {
                    from,
                    to,
                    tier,
                    inbound,
                } => {
                    self.notice.send(crate::channels::Notice::RenameSuggested {
                        from,
                        to,
                        tier,
                        inbound,
                    });
                }
                crate::actor::VaultActorEvent::LinksBroken { from, count } => {
                    self.notice
                        .send(crate::channels::Notice::LinksBroken { from, count });
                }
            }
        }
        self.pump_buffer_patch_requests();
    }

    fn pump_buffer_patch_requests(&self) {
        while let Ok(request) = self.actor_buffer_patches.try_recv() {
            let key = (request.txn_id.clone(), request.item_id);
            let buffer_id = BufferId::from(request.buffer_id);
            // One lock on the memoized path (the former code took three).
            let memoized = self
                .journal_memos
                .lock_recovering()
                .entry(buffer_id)
                .or_default()
                .applied
                .get(&key)
                .cloned();
            let result = match memoized {
                // FR-2: replay the recorded outcome without re-applying.
                Some(result) => result,
                None => {
                    let result = self.apply_buffer_patch(&request);
                    self.memo_insert(buffer_id, key, result.clone());
                    result
                }
            };
            let _ = request.reply.send(result);
        }
    }

    /// Record a patch outcome into the buffer's memo, FIFO-evicting at
    /// `JOURNAL_MEMO_CAP` (C1).
    fn memo_insert(&self, buffer_id: BufferId, key: (String, u64), result: Result<u64, String>) {
        self.journal_memos
            .lock_recovering()
            .entry(buffer_id)
            .or_default()
            .memoize_applied(key, result);
    }

    /// Record an undo-driven journal cancel into the buffer's memo; redo
    /// consults the same per-buffer scope.
    fn memo_cancel(&self, buffer_id: BufferId, key: (String, u64)) {
        self.journal_memos
            .lock_recovering()
            .entry(buffer_id)
            .or_default()
            .memoize_cancelled(key);
    }

    fn apply_buffer_patch(
        &self,
        request: &crate::actor::ApplyBufferPatchRequest,
    ) -> Result<u64, String> {
        let buffer_id = BufferId::from(request.buffer_id);
        let mut manager = self.buffer_manager.lock_recovering();
        if manager
            .get_buffer_by_id(buffer_id)
            .is_some_and(|buffer| buffer.state() == flavite_buffer::state::BufferState::Conflicted)
        {
            if let Some(path) = manager.path_for_id(buffer_id).cloned() {
                self.notice.send(crate::channels::Notice::Conflict {
                    note: flavite_buffer::manager::NoteRef::new(path),
                });
            }
            return Err("conflicted".into());
        }
        let buffer = manager
            .get_buffer_by_id_mut(buffer_id)
            .ok_or("buffer is not open")?;
        if buffer.revision().as_u64() != request.expected_revision {
            return Err("StaleRevision".into());
        }
        if *blake3::hash(buffer.base_bytes()).as_bytes() != request.indexed_hash {
            return Err("StaleRevision: indexed base changed".into());
        }
        if request.kind == crate::journal::TransactionItemKind::Alias {
            let snapshot = flavite_syntax::text::TextSnapshot::new(&request.new_base_bytes)
                .map_err(|error| error.to_string())?;
            let revision = buffer.model_mut().apply_journal_snapshot_boundary(
                snapshot,
                flavite_buffer::undo::JournalTag {
                    txn_id: request.txn_id.clone(),
                    item_id: request.item_id,
                },
            );
            buffer.set_base_snapshot(request.new_base_bytes.clone());
            let recovery = flavite_buffer::recovery::RecoveryFile::from_buffer(buffer, 0, false);
            drop(manager);
            self.persist_journal_recovery_snapshot(request, recovery)?;
            return Ok(revision.as_u64());
        }
        let edits = located_patch(
            buffer.text(),
            &request.target_key,
            &request.old_stem,
            &request.new_stem,
        )?;
        if edits.is_empty() {
            return Err("link-absent".into());
        }
        let revision = buffer.model_mut().apply_journal_edit_ops(
            &edits,
            flavite_buffer::undo::JournalTag {
                txn_id: request.txn_id.clone(),
                item_id: request.item_id,
            },
        );
        if revision.as_u64() == request.expected_revision {
            return Err("journal patch failed validation".into());
        }
        buffer.set_base_snapshot(request.new_base_bytes.clone());
        let recovery = flavite_buffer::recovery::RecoveryFile::from_buffer(buffer, 0, false);
        drop(manager);
        self.persist_journal_recovery_snapshot(request, recovery)?;
        Ok(revision.as_u64())
    }

    fn persist_journal_recovery_snapshot(
        &self,
        request: &crate::actor::ApplyBufferPatchRequest,
        recovery: flavite_buffer::recovery::RecoveryFile,
    ) -> Result<(), String> {
        let key = request
            .recovery_key
            .as_deref()
            .ok_or("journal buffer item has no recovery key")?;
        let writer = self
            .recovery_writer
            .as_ref()
            .ok_or("recovery writer is unavailable for journal buffer patch")?;
        writer
            .lock()
            .map_err(|_| "recovery writer lock poisoned")?
            .write_snapshot_now(key, recovery)
            .ok_or_else(|| "could not persist journal buffer recovery snapshot".to_owned())?;
        Ok(())
    }

    fn execute_inner(&self, command: SessionCommand) -> SessionResult {
        match command {
            SessionCommand::EditStructure {
                buffer_id,
                revision,
                edit,
            } => self.edit_structure(buffer_id, revision, edit),
            SessionCommand::OpenNote { path, policy } => self.open_note(path, policy),
            SessionCommand::CreateNote { path } => self.create_note(path),
            SessionCommand::CloseNote {
                path,
                retain_recovery,
            } => self.close_note(path, retain_recovery),
            SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops,
                undo_policy,
            } => self.apply_edits(buffer_id, revision, ops, undo_policy),
            SessionCommand::ApplyMerge {
                path,
                buffer_id,
                revision,
                disk_hash,
                merged_bytes,
            } => self.apply_merge(path, buffer_id, revision, disk_hash, merged_bytes),
            SessionCommand::SaveNote { path } => self.save_note(path),
            SessionCommand::Reconcile { scope, mode } => self.reconcile(scope, mode),
            SessionCommand::RenameNote { from, to, strategy } => {
                self.rename_note(from, to, strategy.unwrap_or(self.rename_strategy))
            }
            SessionCommand::Undo {
                buffer_id,
                revision,
            } => self.undo(buffer_id, revision),
            SessionCommand::Redo {
                buffer_id,
                revision,
            } => self.redo(buffer_id, revision),
            SessionCommand::ResolveConflict { path, resolution } => {
                self.resolve_conflict(path, resolution)
            }
            SessionCommand::RestoreRecovery { path } => self.restore_recovery(path),
            SessionCommand::ReopenEncoding {
                path,
                expected_raw_hash,
                encoding,
                bom,
            } => self.reopen_encoding(path, expected_raw_hash, encoding, bom),
            SessionCommand::ConvertEncoding {
                buffer_id,
                revision,
                target,
            } => self.convert_encoding(buffer_id, revision, target),
            SessionCommand::PrepareReplace {
                request_id,
                request,
            } => self.prepare_replace(request_id, request),
            SessionCommand::FindMatches {
                request_id,
                request,
            } => self.text_find(request_id, request),
            SessionCommand::ApplyReplace {
                buffer_id,
                revision,
                plan_id,
            } => self.apply_replace(buffer_id, revision, plan_id),
            SessionCommand::SourceFindStart {
                request_id,
                request,
            } => self.source_find_start(request_id, request),
            SessionCommand::SourceFindPage {
                finder_id,
                note_index,
                match_offset,
                page_len,
            } => self.source_find_page(finder_id, note_index, match_offset, page_len),
            SessionCommand::SourceFindClose { finder_id } => self.source_find_close(finder_id),
            SessionCommand::ReplacePreviewPrepare {
                request_id,
                request,
            } => self.replace_preview_prepare(request_id, request),
            SessionCommand::ReplaceSelect {
                preview_id,
                selected,
            } => self.replace_select(preview_id, selected),
            SessionCommand::ReplaceStatus { preview_id } => self.replace_status(preview_id),
            SessionCommand::ReplaceApply {
                request_id,
                preview_id,
                selection_revision,
                idempotency_key,
            } => self.replace_apply(request_id, preview_id, selection_revision, idempotency_key),
            SessionCommand::ReplaceCancel { job_id } => self.replace_cancel(job_id),
            SessionCommand::ReplaceResume {
                request_id,
                job_id,
                idempotency_key,
            } => self.replace_resume(request_id, job_id, idempotency_key),
            SessionCommand::ReplaceRevert {
                request_id,
                job_id,
                idempotency_key,
            } => self.replace_revert(request_id, job_id, idempotency_key),
            SessionCommand::ReplaceJobStatus { job_id } => self.replace_job_status(job_id),
            SessionCommand::RestoreHistory {
                path,
                buffer_id,
                revision,
                hash,
            } => self.restore_history(path, buffer_id, revision, hash),
            SessionCommand::DiscardRecovery { path } => self.discard_recovery(path),
            SessionCommand::FlushRecovery { path } => match self.flush_recovery_note(&path) {
                Ok(receipt) => SessionResult::Success(receipt),
                Err(message) => SessionResult::Error { message },
            },
            SessionCommand::DiscardEdits { path } => self.discard_edits(path),
            SessionCommand::LinkPrepare {
                request,
                request_id,
            } => self.prepare_link(request, request_id),
            SessionCommand::LinkRemove { intent, plan_id } => {
                self.commit_link(intent, plan_id, true)
            }
            SessionCommand::LinkInsert { intent, plan_id } => {
                self.commit_link(intent, plan_id, false)
            }
        }
    }

    /// Read a bounded native-identity note catalog snapshot for workspace
    /// shells and pickers. Read-only; errors surface as retryable states.
    pub fn note_catalog(
        &self,
        limit: usize,
        after: Option<flavite_index::NoteCatalogCursor>,
    ) -> Result<flavite_index::NoteCatalogPage, String> {
        self.actor.note_catalog(limit, after)
    }

    /// Resolve a target through the shared index resolver for editor
    /// decorations or navigation previews.
    pub fn resolve_target(
        &self,
        target_key: String,
        raw_target: String,
        is_embed: bool,
        source_path_key: Option<String>,
    ) -> Result<flavite_index::resolver::Resolution, String> {
        self.actor
            .resolve_target(target_key, raw_target, is_embed, source_path_key)
    }

    /// Read the saved bytes of one note directly from the vault actor.
    ///
    /// This is a read-only bounded snapshot of saved content for query
    /// workers; it never touches open-buffer state or the driving turn.
    ///
    /// # Errors
    /// Returns the actor error text when the read cannot be served.
    pub fn read_saved_note(&self, path: &Path) -> Result<Option<Vec<u8>>, String> {
        self.actor
            .read_file(path.to_path_buf())
            .map_err(|error| error.to_string())
    }

    pub fn validate_attachment_open(&self, relative: &Path) -> Result<PathBuf, String> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("Invalid vault path".into());
        }
        self.actor
            .validate_attachment_open(self.root().join(relative))
    }
    /// Worker-only capped, contained vault content read.
    pub fn read_content_bounded(
        &self,
        relative: &Path,
        max: u64,
    ) -> Result<Option<Vec<u8>>, String> {
        if relative.is_absolute()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("Invalid vault path".into());
        }
        self.actor.read_bounded(self.root().join(relative), max)
    }

    /// Evaluate one saved-content search against a coherent captured
    /// snapshot (synchronous seam for graph filter membership; search
    /// sessions use the async capture handshake).
    ///
    /// # Errors
    /// Returns the actor error text when evaluation fails.
    pub fn evaluate_saved_search(
        &self,
        query: flavite_index::query::Query,
        limit: usize,
    ) -> Result<flavite_index::SearchPage, String> {
        self.actor.evaluate_saved_search(query, limit)
    }

    /// Begin the ordered search-snapshot capture handshake (D-07): the
    /// caller establishes its own SQL snapshot when signalled, acks the
    /// generation it read, and then waits asynchronously for the searcher.
    ///
    /// # Errors
    /// Returns the actor error text when the handshake cannot begin.
    pub fn begin_search_capture(
        &self,
    ) -> Result<crate::search_capture::SearchCaptureHandshake, String> {
        self.actor.begin_search_capture()
    }

    /// The vault identity this session serves.
    #[must_use]
    pub fn vault_uuid(&self) -> uuid::Uuid {
        self.identity.uuid
    }

    /// Execute a structural-only query page (§6.1: text predicates are
    /// rejected explicitly by the index, never an empty success).
    ///
    /// # Errors
    /// Returns the typed text rejection or the actor error text.
    pub fn structural_page(
        &self,
        query: flavite_index::query::Query,
        limit: usize,
    ) -> Result<flavite_index::SearchPage, String> {
        self.actor.structural_query(query, limit)
    }

    /// List a note's bounded local-history records, newest first (W10).
    ///
    /// # Errors
    /// Returns the actor error text when history is unavailable.
    pub fn note_history(
        &self,
        path: &Path,
        limit: usize,
    ) -> Result<Vec<crate::history::HistoryEntry>, String> {
        self.actor
            .history_for_note(native_note_key(path, &self.identity.root), limit)
    }

    /// Read one content-addressed history snapshot, bounded (W10).
    ///
    /// # Errors
    /// Returns the actor error text when the snapshot cannot be served.
    pub fn read_history_snapshot(&self, hash: &str) -> Result<Vec<u8>, String> {
        self.actor
            .read_history_snapshot(hash.to_owned(), crate::history::MAX_HISTORY_READ_BYTES)
    }

    /// Issue a core-semantic-link client namespace token (W05 §9.1). The
    /// token is process-local and never persisted (FR-26).
    #[must_use]
    pub fn new_link_client(&self) -> crate::link::ClientToken {
        let token = crate::link::ClientToken(u128::from(uuid::Uuid::now_v7().as_u64_pair().1));
        self.link_plans.lock_recovering().register_client(token);
        token
    }

    /// Cancel an uncommitted gesture; late worker results cannot revive it.
    pub fn cancel_link_intent(&self, intent: crate::link::LinkIntent) {
        self.link_plans.lock_recovering().cancel(intent);
    }

    /// Read the immutable canonical publication without actor IO.
    pub fn resolver_catalog(
        &self,
    ) -> Result<Arc<flavite_index::catalog::ResolverCatalog>, &'static str> {
        self.actor.resolver_catalog()
    }

    /// Resolve source authority from a semantic plan, never a caller path.
    pub fn link_plan_source(
        &self,
        intent: crate::link::LinkIntent,
        plan_id: u128,
    ) -> Option<BufferId> {
        self.link_plans
            .lock_recovering()
            .source_for(intent, plan_id)
            .map(BufferId::from)
    }
    /// Native path for an open buffer without cloning note contents.
    pub fn buffer_path(&self, buffer: BufferId) -> Option<PathBuf> {
        self.buffer_manager
            .lock_recovering()
            .path_for_id(buffer)
            .cloned()
    }

    /// Admit one semantic-link prepare request under the ingress turn; the
    /// expensive proof work runs on the bounded link worker pool (W05 §9.2).
    fn prepare_link(
        &self,
        request: crate::link::LinkPrepareRequest,
        _caller_request_id: u64,
    ) -> SessionResult {
        // Every accepted asynchronous operation shares the Session namespace;
        // a caller request number must not collide with an open/save/structure
        // receipt and overwrite the facade's correlation map.
        let request_id = self.next_request_id();
        let admission = {
            let mut store = self.link_plans.lock_recovering();
            store.admit_prepare(&request)
        };
        match admission {
            Err(reason) => SessionResult::LinkRejected { reason },
            Ok(crate::link::PrepareAdmission::Proceed) => match &self.link_jobs {
                Some(jobs) => {
                    let intent = request.intent;
                    if jobs
                        .send(LinkJob {
                            request_id,
                            request,
                        })
                        .is_err()
                    {
                        self.link_plans.lock_recovering().fail_prepare(intent);
                        return SessionResult::LinkRejected {
                            reason: crate::link::LinkInsertionError::Busy,
                        };
                    }
                    SessionResult::Pending { request_id }
                }
                None => {
                    self.link_plans
                        .lock_recovering()
                        .fail_prepare(request.intent);
                    SessionResult::LinkRejected {
                        reason: crate::link::LinkInsertionError::Busy,
                    }
                }
            },
        }
    }

    /// Commit one prepared semantic link with one guarded edit under the
    /// serialized mutation authority (W05 §9.2: TTL, revision, epoch, and
    /// target identity are revalidated; the plan is consumed exactly once
    /// and the terminal outcome is recorded atomically with the edit).
    fn commit_link(
        &self,
        intent: crate::link::LinkIntent,
        plan_id: u128,
        removal: bool,
    ) -> SessionResult {
        use crate::link::{CommitAdmission, LinkOutcome};
        let admission = {
            let mut store = self.link_plans.lock_recovering();
            store.admit_commit(&intent, plan_id, removal)
        };
        let plan = match admission {
            Err(reason) => return SessionResult::LinkRejected { reason },
            Ok(CommitAdmission::Cached(outcome)) => {
                return match outcome {
                    Ok(LinkOutcome::Applied {
                        buffer_id,
                        new_revision,
                    }) => SessionResult::Success(if removal {
                        CommandResult::EditsApplied {
                            buffer_id: BufferId::from(buffer_id),
                            new_revision: Revision::new(new_revision),
                        }
                    } else {
                        CommandResult::LinkInserted {
                            buffer_id: BufferId::from(buffer_id),
                            new_revision: Revision::new(new_revision),
                        }
                    }),
                    Err(reason) => SessionResult::LinkRejected { reason },
                };
            }
            Ok(CommitAdmission::Proceed(plan)) => plan,
        };
        // Buffer state and revision guards.
        let state = self
            .buffer_manager
            .lock_recovering()
            .get_buffer_by_id(BufferId::from(plan.source.buffer_id))
            .map(|buffer| (buffer.state(), buffer.revision().as_u64()));
        let Some((state, actual_revision)) = state else {
            let mut store = self.link_plans.lock_recovering();
            store.record_outcome(
                &intent,
                crate::link::fingerprint_insert_public(plan_id),
                Err(crate::link::LinkInsertionError::StaleSource {
                    expected: plan.source.revision,
                    actual: 0,
                }),
            );
            return SessionResult::LinkRejected {
                reason: crate::link::LinkInsertionError::StaleSource {
                    expected: plan.source.revision,
                    actual: 0,
                },
            };
        };
        if actual_revision != plan.source.revision {
            self.link_plans.lock_recovering().record_outcome(
                &intent,
                crate::link::fingerprint_insert_public(plan_id),
                Err(crate::link::LinkInsertionError::StaleSource {
                    expected: plan.source.revision,
                    actual: actual_revision,
                }),
            );
            return SessionResult::LinkRejected {
                reason: crate::link::LinkInsertionError::StaleSource {
                    expected: plan.source.revision,
                    actual: actual_revision,
                },
            };
        }
        if !matches!(
            state,
            flavite_buffer::state::BufferState::Clean | flavite_buffer::state::BufferState::Dirty
        ) {
            let name = match state {
                flavite_buffer::state::BufferState::Conflicted => "Conflicted",
                flavite_buffer::state::BufferState::Orphaned => "Orphaned",
                _ => "Unavailable",
            };
            self.link_plans.lock_recovering().record_outcome(
                &intent,
                crate::link::fingerprint_insert_public(plan_id),
                Err(crate::link::LinkInsertionError::BufferStateNeedsDecision { state: name }),
            );
            return SessionResult::LinkRejected {
                reason: crate::link::LinkInsertionError::BufferStateNeedsDecision { state: name },
            };
        }
        // Canonical catalog revalidation: same epoch, target still resolves
        // to the exact planned entity (never retarget by title).
        let catalog = match self.actor.resolver_catalog() {
            Ok(catalog) => catalog,
            Err(_) => {
                return SessionResult::LinkRejected {
                    reason: crate::link::LinkInsertionError::Busy,
                };
            }
        };
        if catalog.epoch != plan.validated_epoch || catalog.generation < plan.validated_generation {
            self.link_plans.lock_recovering().record_outcome(
                &intent,
                crate::link::fingerprint_insert_public(plan_id),
                Err(crate::link::LinkInsertionError::StaleEpoch),
            );
            return SessionResult::LinkRejected {
                reason: crate::link::LinkInsertionError::StaleEpoch,
            };
        }
        let resolution = catalog.resolve_at_path(
            &flavite_syntax::text::fold_key(&plan.target_raw),
            &plan.target_raw,
            plan.target_is_embed,
            plan.source_path_key.as_deref().map(std::path::Path::new),
        );
        let current_id = if plan.target_is_attachment {
            resolution.attachment.as_ref().map(|a| a.att_id)
        } else {
            resolution.note.as_ref().map(|n| n.note_id)
        };
        if if plan.target_is_unresolved {
            !resolution.is_unresolved
        } else {
            current_id != Some(plan.target_entity_id)
        } {
            self.link_plans.lock_recovering().record_outcome(
                &intent,
                crate::link::fingerprint_insert_public(plan_id),
                Err(crate::link::LinkInsertionError::TargetRetargeted),
            );
            return SessionResult::LinkRejected {
                reason: crate::link::LinkInsertionError::TargetRetargeted,
            };
        }
        // One guarded edit: a single-op batch is one undo-tree unit.
        let (start, end, replacement) = plan.edit.clone();
        let mut edit = flavite_buffer::EditOp::new(
            flavite_syntax::text::Span::new(
                flavite_syntax::text::TextByte(start),
                flavite_syntax::text::TextByte(end),
            ),
            replacement,
        );
        edit.boundary = true;
        let applied = self.apply_edits(
            BufferId::from(plan.source.buffer_id),
            Revision::new(plan.source.revision),
            vec![edit],
            flavite_buffer::undo::UndoPolicy::Isolated,
        );
        let result = match applied {
            SessionResult::Success(CommandResult::EditsApplied {
                buffer_id,
                new_revision,
            }) => SessionResult::Success(CommandResult::LinkInserted {
                buffer_id,
                new_revision,
            }),
            SessionResult::StaleRevision {
                buffer_id: _,
                expected,
                actual,
            } => SessionResult::LinkRejected {
                reason: crate::link::LinkInsertionError::StaleSource {
                    expected: expected.as_u64(),
                    actual: actual.as_u64(),
                },
            },
            SessionResult::Error { message } => SessionResult::LinkRejected {
                reason: crate::link::LinkInsertionError::EditRejected(message),
            },
            other => SessionResult::LinkRejected {
                reason: crate::link::LinkInsertionError::EditRejected(format!("{other:?}")),
            },
        };
        // The terminal outcome is recorded atomically with the edit result:
        // a retry of the same gesture replays exactly this result.
        let cached: crate::link::CachedOutcome = match &result {
            SessionResult::Success(CommandResult::LinkInserted {
                buffer_id,
                new_revision,
            }) => Ok(LinkOutcome::Applied {
                buffer_id: buffer_id.as_u64(),
                new_revision: new_revision.as_u64(),
            }),
            SessionResult::LinkRejected { reason } => Err(reason.clone()),
            other => Err(crate::link::LinkInsertionError::EditRejected(format!(
                "{other:?}"
            ))),
        };
        self.link_plans.lock_recovering().record_outcome(
            &intent,
            crate::link::fingerprint_insert_public(plan_id),
            cached,
        );
        if removal
            && let SessionResult::Success(CommandResult::LinkInserted {
                buffer_id,
                new_revision,
            }) = result
        {
            return SessionResult::Success(CommandResult::EditsApplied {
                buffer_id,
                new_revision,
            });
        }
        result
    }

    /// Process ready worker replies and return every terminal result that has
    /// not yet been presented to the host. This is lossless: completions stay
    /// queued until the host drains them.
    pub fn drain_operation_completions(&self) -> Vec<OperationCompletion> {
        let _turn = self.driving_turn.lock_recovering();
        self.pump_structure_completions();
        self.pump_link_completions();
        self.pump_text_completions();
        self.pump_replace_completions();
        self.pump_open_completions();
        self.pump_save_completions();
        self.pump_reconcile_completions();
        self.pump_rename_completions();
        self.completions.lock_recovering().drain(..).collect()
    }

    /// Drain lossy vault pulses for a host adapter.
    pub fn drain_pulses(&self) -> Vec<crate::channels::Pulse> {
        let mut events = Vec::new();
        while let Ok(event) = self.pulse.try_recv() {
            events.push(event);
        }
        events
    }

    /// Drain lossless, coalesced notices for a host adapter.
    pub fn drain_notices(&self) -> Vec<crate::channels::Notice> {
        self.drain_notices_limit(usize::MAX)
    }

    /// Drain at most `limit` lossless notices so an API host can apply
    /// acknowledgement credits without losing the producer's backlog.
    pub fn drain_notices_limit(&self, limit: usize) -> Vec<crate::channels::Notice> {
        let mut events = Vec::new();
        while events.len() < limit
            && let Ok(event) = self.notice.try_recv()
        {
            events.push(event);
        }
        events
    }

    /// Drain session observations, including external disk changes.
    pub fn drain_events(&self) -> Vec<SessionEvent> {
        let _turn = self.driving_turn.lock_recovering();
        self.pump_external_changes_inner();
        // Advisory mirror (NFR-5): fold process-local poison recoveries
        // recorded outside this Session (`sync::POISON_RECOVERIES`) into the
        // observable counter, monotonic-max so the mirror never rewinds.
        let live = crate::sync::POISON_RECOVERIES.load(Ordering::Relaxed);
        self.poison_recoveries_total
            .fetch_max(live, Ordering::Relaxed);
        self.session_events.lock_recovering().drain(..).collect()
    }

    /// Request a throttled verification after the host window gains focus.
    pub fn focus(&self) -> Result<(), String> {
        self.actor.focus().map_err(|error| error.to_string())
    }

    /// Return the cumulative number of disk drifts found by reconciliation.
    pub fn drift_found_total(&self) -> u64 {
        self.actor.drift_found_total()
    }

    /// Current index version from the single-writer actor.
    pub fn version(&self) -> Result<crate::types::Version, String> {
        self.actor.get_version().map_err(|error| error.to_string())
    }

    /// Acknowledge notice presentation. This never changes operation state.
    pub fn acknowledge_notice(&self, notice: &crate::channels::Notice) {
        self.notice.ack(notice);
    }

    /// CLI/headless convenience for commands that promise a disk mutation.
    /// GUI hosts use `drain_operation_completions` from their normal pump.
    /// Wait for a headless operation to reach its terminal result.
    ///
    /// Turn-free by contract (S6/Q4): the pumps are turn-free and command
    /// transitions self-serialize under `driving_turn` inside `execute`, so
    /// a waiting observer must not hold the turn that the producer needs to
    /// deliver its result. The one path that already holds the turn — the
    /// W12 `restore_recovery` implicit open inside `execute` — uses the
    /// dedicated [`Session::wait_for_operation_holding_turn`] entry, which
    /// must not re-acquire it (self-deadlock).
    pub fn wait_for_operation(&self, request_id: u64) -> SessionResult {
        self.wait_loop(request_id)
    }

    /// Wait while the driving turn is already held (W12: `restore_recovery`'s
    /// implicit open runs inside `execute`, which holds the turn;
    /// re-acquiring here would self-deadlock).
    fn wait_for_operation_holding_turn(&self, request_id: u64) -> SessionResult {
        self.wait_loop(request_id)
    }

    /// Shared park-and-pump wait body for both entry points.
    ///
    /// Every pass pumps this thread's installers (worker replies become
    /// completions only inside the pump functions, so a lone waiter must
    /// pump itself), takes its own result losslessly, checks liveness, then
    /// parks on `completions_cv` until a recorded completion or the
    /// `WAIT_TICK` ceiling. No sleeping poll, no `driving_turn`.
    /// Worst-case lone-waiter added latency is 25 ms versus the old 1 ms
    /// busy-poll — accepted for headless paths: CPU per idle waiter drops
    /// from ~1,000 wakeups/s to ≤ 40/s (NFR-3/NFR-4).
    fn wait_loop(&self, request_id: u64) -> SessionResult {
        loop {
            self.pump_external_changes_inner();
            self.pump_open_completions();
            self.pump_save_completions();
            self.pump_reconcile_completions();
            self.pump_rename_completions();
            self.pump_replace_completions();
            self.pump_text_completions();
            if let Some(result) = self.take_completion(request_id) {
                return result;
            }
            if !self.operation_in_flight(request_id) {
                return SessionResult::Error {
                    message: format!("unknown or already-drained operation {request_id}"),
                };
            }
            // No lost wakeups by construction: the completion check above
            // and the parking happen under the same `completions` mutex,
            // and `record_completion` holds that mutex while pushing, so a
            // completion recorded between the check and the park is visible
            // to the condition predicate before the waiter ever sleeps.
            let completions = self.completions.lock_recovering();
            crate::sync::observe_park(request_id);
            let _waited = crate::sync::wait_result_recovering(
                self.completions_cv
                    .wait_timeout_while(completions, WAIT_TICK, |queue| {
                        queue
                            .iter()
                            .all(|completion| completion.request_id != request_id)
                    }),
            );
        }
    }

    /// Remove and return the recorded result for `request_id`, leaving all
    /// other completions in place (the ledger stays lossless for other
    /// consumers such as `drain_operation_completions`).
    fn take_completion(&self, request_id: u64) -> Option<SessionResult> {
        let mut completions = self.completions.lock_recovering();
        completions
            .iter()
            .position(|completion| completion.request_id == request_id)
            .map(|position| completions.remove(position).unwrap().result)
    }

    /// True while any flight can still deliver the terminal result for this
    /// request: a save (primary or coalesced successor), reconcile, rename,
    /// or open flight.
    fn operation_in_flight(&self, request_id: u64) -> bool {
        self.save_flights.lock_recovering().values().any(|flight| {
            flight.request.request_id == request_id
                || flight
                    .queued
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
        }) || self
            .reconcile_flights
            .lock_recovering()
            .contains_key(&request_id)
            || self
                .rename_flights
                .lock_recovering()
                .contains_key(&request_id)
            || self
                .open_flights
                .lock_recovering()
                .values()
                .any(|flight| flight.request_ids.contains(&request_id))
            || self.pending_replace.lock_recovering().contains(&request_id)
            || self.pending_text.lock_recovering().contains(&request_id)
    }

    /// Record a terminal operation result and wake any parked waiters (S6).
    ///
    /// The ONLY writer of `OperationCompletion` into the queue: the ten
    /// former inline push sites all routed through here, which is what makes
    /// the notify edge auditable — one site, not ten. Admission remains
    /// bounded by the pending-save cap (unchanged).
    fn record_completion(&self, request_id: u64, result: SessionResult) {
        self.completions
            .lock_recovering()
            .push_back(OperationCompletion { request_id, result });
        self.completions_cv.notify_all();
    }

    fn pump_save_completions(&self) {
        let ready = {
            let mut flights = self.save_flights.lock_recovering();
            let ready_ids = flights
                .iter()
                .filter_map(|(buffer, flight)| match flight.response.try_recv() {
                    Ok(response) => Some((*buffer, response)),
                    Err(crossbeam::channel::TryRecvError::Empty) => None,
                    Err(crossbeam::channel::TryRecvError::Disconnected) => Some((
                        *buffer,
                        crate::actor::VaultActorResp::Error(
                            "save worker disconnected before returning a terminal result".into(),
                        ),
                    )),
                })
                .collect::<Vec<_>>();
            ready_ids
                .into_iter()
                .filter_map(|(buffer_id, response)| {
                    flights
                        .remove(&buffer_id)
                        .map(|flight| (buffer_id, flight, response))
                })
                .collect::<Vec<_>>()
        };

        for (buffer_id, flight, response) in ready {
            let operation_result = self.finish_save(&flight.request, &response);
            if self
                .autosave_requests
                .lock_recovering()
                .remove(&flight.request.request_id)
                && !matches!(&operation_result, SessionResult::Success(_))
            {
                self.record_autosave_failure(&flight.request.path, &operation_result);
            }
            self.record_completion(flight.request.request_id, operation_result);

            let Some(mut queued) = flight.queued else {
                continue;
            };
            match response {
                crate::actor::VaultActorResp::Saved(Ok(receipt)) => {
                    // The first save established a new disk base. Bind the
                    // queued immutable snapshot to those exact bytes before
                    // admitting its worker request.
                    queued.precondition = DiskPrecondition::Existing(receipt.hash);
                    queued.merge_base = flight.request.bytes.clone();
                    let buffer_is_live = self
                        .buffer_manager
                        .lock_recovering()
                        .get_buffer_by_id(BufferId::from(buffer_id))
                        .is_some();
                    if !buffer_is_live {
                        self.record_completion(
                            queued.request_id,
                            SessionResult::Error {
                                message: "queued save's buffer was closed before admission".into(),
                            },
                        );
                        continue;
                    }
                    match self.actor.submit_save(queued.clone()) {
                        Ok(response) => {
                            self.save_flights.lock_recovering().insert(
                                buffer_id,
                                SaveFlight {
                                    request: queued,
                                    response,
                                    queued: None,
                                },
                            );
                        }
                        Err(error) => self.record_completion(
                            queued.request_id,
                            SessionResult::Error {
                                message: error.to_string(),
                            },
                        ),
                    }
                }
                _ => self.record_completion(
                    queued.request_id,
                    SessionResult::Error {
                        message: format!(
                            "queued save was not admitted because save {} did not complete",
                            flight.request.request_id
                        ),
                    },
                ),
            }
        }
    }

    fn pump_reconcile_completions(&self) {
        let ready = {
            let mut flights = self.reconcile_flights.lock_recovering();
            let ids = flights
                .iter()
                .filter_map(|(id, receiver)| match receiver.try_recv() {
                    Ok(response) => Some((*id, response)),
                    Err(crossbeam::channel::TryRecvError::Empty) => None,
                    Err(crossbeam::channel::TryRecvError::Disconnected) => Some((
                        *id,
                        crate::actor::VaultActorResp::Error(
                            "reconcile worker disconnected before returning a terminal result"
                                .into(),
                        ),
                    )),
                })
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|(id, response)| flights.remove(&id).map(|_| (id, response)))
                .collect::<Vec<_>>()
        };
        for (request_id, response) in ready {
            let result = match response {
                crate::actor::VaultActorResp::ReconcileComplete { summary, .. } => {
                    SessionResult::Success(CommandResult::ReconcileComplete { summary })
                }
                crate::actor::VaultActorResp::ReconcileSuperseded { by_request } => {
                    SessionResult::Superseded {
                        request_id,
                        by_request,
                    }
                }
                crate::actor::VaultActorResp::Error(message) => SessionResult::Error { message },
                _ => SessionResult::Error {
                    message: "reconcile worker returned an invalid response".into(),
                },
            };
            self.record_completion(request_id, result);
        }
    }

    fn pump_rename_completions(&self) {
        let ready = {
            let mut flights = self.rename_flights.lock_recovering();
            let ids = flights
                .iter()
                .filter_map(|(id, (_, _, receiver))| match receiver.try_recv() {
                    Ok(response) => Some((*id, response)),
                    Err(crossbeam::channel::TryRecvError::Empty) => None,
                    Err(crossbeam::channel::TryRecvError::Disconnected) => Some((
                        *id,
                        crate::actor::VaultActorResp::Error(
                            "rename worker disconnected before returning a terminal result".into(),
                        ),
                    )),
                })
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|(id, response)| {
                    flights
                        .remove(&id)
                        .map(|(from, to, _)| (id, from, to, response))
                })
                .collect::<Vec<_>>()
        };
        for (request_id, from, to, response) in ready {
            let result = match response {
                crate::actor::VaultActorResp::Renamed(Ok(txn_id)) => {
                    SessionResult::Success(CommandResult::NoteRenamed { txn_id, from, to })
                }
                crate::actor::VaultActorResp::Renamed(Err(message))
                | crate::actor::VaultActorResp::Error(message) => SessionResult::Error { message },
                _ => SessionResult::Error {
                    message: "rename worker returned an invalid response".into(),
                },
            };
            self.record_completion(request_id, result);
        }
    }

    fn finish_save(
        &self,
        request: &SaveRequest,
        response: &crate::actor::VaultActorResp,
    ) -> SessionResult {
        let path = request.path.clone();
        let buffer_id = BufferId::from(request.buffer_id);
        match response {
            crate::actor::VaultActorResp::Saved(Ok(receipt)) => {
                let revision = Revision::new(request.revision);
                let mut manager = self.buffer_manager.lock_recovering();
                if let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) {
                    buffer.handle_save_completed_revision(revision, &request.bytes);
                }
                let source_format = manager
                    .get_buffer_by_id(buffer_id)
                    .map(|buffer| buffer.model().source_format());
                drop(manager);
                if let Some(writer) = &self.recovery_writer {
                    let mut writer = writer.lock_recovering();
                    writer.retire_covered(buffer_id, revision);
                }
                self.absent_on_open.lock_recovering().remove(&path);
                // §6.10: an own save updates the hint binding with its exact
                // receipt hash. Plain BOM-less UTF-8 content binds nothing.
                if let Some(format) = source_format {
                    self.bind_save_hint(
                        &path,
                        format,
                        *blake3::hash(&request.bytes).as_bytes(),
                        &request.bytes,
                    );
                }
                SessionResult::Success(CommandResult::NoteSaved {
                    buffer_id,
                    saved_revision: revision,
                    path,
                    hash: receipt.hash.to_vec(),
                    epoch: Epoch::new(receipt.epoch),
                    generation: Generation::new(receipt.generation),
                })
            }
            crate::actor::VaultActorResp::Saved(Err(failure)) => {
                if failure.stage == SaveFailureStage::Compare {
                    if let Some(buffer) = self
                        .buffer_manager
                        .lock_recovering()
                        .get_buffer_by_id_mut(buffer_id)
                        && let Some(hash) = failure.current_hash
                    {
                        buffer.mark_conflicted_disk_hash(hash);
                    }
                    self.notice.send(crate::channels::Notice::Conflict {
                        note: flavite_buffer::manager::NoteRef::new(path.clone()),
                    });
                }
                if matches!(
                    failure.outcome,
                    DurabilityOutcome::ReplacedIndexPending
                        | DurabilityOutcome::ReplacedDurabilityUncertain
                        | DurabilityOutcome::OutcomeUnknown
                ) {
                    let mut manager = self.buffer_manager.lock_recovering();
                    if let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) {
                        let saved_hash = *blake3::hash(&request.bytes).as_bytes();
                        if failure.current_hash == Some(saved_hash) {
                            buffer.record_replaced_disk_snapshot(&request.bytes);
                        } else if let Some(hash) = failure.current_hash {
                            buffer.mark_conflicted_disk_hash(hash);
                        } else {
                            // No observed hash means replacement outcome
                            // remains unknown. The old precondition is kept,
                            // so another save cannot pass compare without a
                            // fresh worker observation.
                        }
                    }
                    drop(manager);
                    if failure.current_hash.is_some() {
                        self.absent_on_open.lock_recovering().remove(&path);
                    }
                }
                if failure.stage == SaveFailureStage::Compare {
                    SessionResult::Conflict { path }
                } else if failure.stage == SaveFailureStage::IndexApply
                    && failure.outcome == DurabilityOutcome::ReplacedIndexPending
                {
                    // W12: the bytes are durably on disk; only the index
                    // application is pending. Surfaced separately from the
                    // generic error so hosts can report save success and
                    // indexing failure apart (the UI reconcile clears it).
                    SessionResult::ActionRequired {
                        code: "index-pending",
                        data: Some(path.display().to_string()),
                    }
                } else {
                    SessionResult::Error {
                        message: failure.to_string(),
                    }
                }
            }
            crate::actor::VaultActorResp::Error(message) => SessionResult::Error {
                message: message.clone(),
            },
            _ => SessionResult::Error {
                message: "save worker returned an invalid response".into(),
            },
        }
    }

    /// Admit a cold open. An already-open note returns its existing
    /// identity immediately; otherwise the request joins (or creates) a
    /// coalesced open flight and the read runs on a bounded worker, never
    /// under this driving turn (RFC-0003 §8.3).
    fn open_note(&self, path: PathBuf, policy: OpenPolicy) -> SessionResult {
        if let Some(buffer_id) = self
            .buffer_manager
            .lock_recovering()
            .get_buffer(&path)
            .map(|buffer| (buffer.buffer_id(), buffer.revision()))
        {
            return SessionResult::Success(CommandResult::NoteOpened {
                buffer_id: buffer_id.0,
                revision: buffer_id.1,
            });
        }
        self.open_note_inner(path, policy)
    }

    /// Shared open-flight admission for [`Self::open_note`] and
    /// [`Self::create_note`]: reserve or join a flight keyed by path and
    /// dispatch the read/create job to a bounded worker.
    fn open_note_inner(&self, path: PathBuf, policy: OpenPolicy) -> SessionResult {
        {
            let flights = self.open_flights.lock_recovering();
            if flights.len() >= MAX_PENDING_OPEN_NOTES {
                return SessionResult::Busy;
            }
        }
        let request_id = self.next_request_id();
        let mut flights = self.open_flights.lock_recovering();
        match flights.entry(path.clone()) {
            std::collections::hash_map::Entry::Occupied(mut flight) => {
                flight.get_mut().request_ids.push(request_id);
                SessionResult::Pending { request_id }
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(OpenFlight {
                    request_ids: vec![request_id],
                });
                let job = OpenJob {
                    note_key: native_note_key(&path, &self.identity.root),
                    path: path.clone(),
                    policy,
                };
                let admitted = self
                    .open_jobs
                    .as_ref()
                    .is_some_and(|jobs| jobs.send(job).is_ok());
                if admitted {
                    SessionResult::Pending { request_id }
                } else {
                    flights.remove(&path);
                    SessionResult::Error {
                        message: "vault open workers are unavailable".into(),
                    }
                }
            }
        }
    }

    /// Install one completed read: a single Session turn turns the exact
    /// immutable snapshot into a buffer and publishes every correlated
    /// caller's terminal result once (RFC-0003 §8.3).
    fn install_open_read(&self, read: OpenRead) -> SessionResult {
        let Some(open_jobs) = &self.open_jobs else {
            return SessionResult::Error {
                message: "vault is shutting down".into(),
            };
        };
        let content = match read.outcome {
            Ok(content) => content,
            Err(message) => return SessionResult::Error { message },
        };
        let absent = content.is_none();
        if absent
            && matches!(
                read.policy,
                OpenPolicy::ExistingOnly | OpenPolicy::CreateNew
            )
        {
            let message = if read.policy == OpenPolicy::CreateNew {
                // The worker created the file moments ago; losing it again is
                // an honest error, never a silently empty note.
                format!("created note vanished before open: {}", read.path.display())
            } else {
                SessionError::NoteNotFound(read.path.clone()).to_string()
            };
            return SessionResult::Error { message };
        }
        let content = content.unwrap_or_default();
        // §6.10: decode once under the central codec contract. Every outcome
        // is either a strictly decoded snapshot with its authoritative
        // descriptor, or a typed encoding-choice request — installing the
        // manager's empty fallback would present a blank editor as a clean,
        // saved note (and an autosave could later write that emptiness over
        // the real bytes).
        let native_key = hex::encode(native_note_key(&read.path, &self.identity.root));
        let limits = flavite_encoding::DecodeLimits::default();
        // An explicitly created note is new UTF-8 content by construction;
        // a stale hint for a previously deleted path must not block it.
        let mut explicit = (absent || read.policy == OpenPolicy::CreateNew)
            .then_some(flavite_syntax::SourceFormat::utf8());
        let raw_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        let hints = self.encoding_hints.view();
        // §6.10: an unresolved pre-save format-binding intent whose output
        // hash matches the observed bytes completes here — the source effect
        // reached disk durably but its metadata binding did not. This is
        // EncodingMetadataPending resolution, never NotReplaced.
        let pending_intent = hints
            .record
            .intents
            .get(&native_key)
            .filter(|intent| intent.output_raw_hash == raw_hash);
        if explicit.is_none()
            && let Some(intent) = pending_intent
        {
            let intent_format = intent.format();
            self.bind_save_hint(&read.path, intent_format, raw_hash, &content);
            explicit = Some(intent_format);
        }
        let (snapshot, _format, stamp) = match crate::encoding::resolve_source(
            native_key, &content, &hints, explicit, &limits,
        ) {
            Resolution::Resolved(resolved) => (resolved.snapshot, resolved.format, resolved.stamp),
            Resolution::NeedsChoice { reason, .. } => {
                return self.encoding_choice_required(&read.path, &reason);
            }
        };
        let (buffer_id, revision) = {
            let mut buffer_manager_guard = self.buffer_manager.lock_recovering();
            let (buffer, _transition) =
                buffer_manager_guard.open_note_snapshot(read.path.clone(), snapshot);
            // The format stamp's hint generation is provenance metadata on
            // the model (§6.10): it rides along with every serialized guard.
            buffer
                .model_mut()
                .set_format_stamp_generation(stamp.hint_generation);
            (buffer.buffer_id(), buffer.revision())
        };

        if let Some(recovery) = read.recovery {
            // The recorded draft differs from the disk bytes: offer it. An
            // unencodable recorded draft (strict-ASCII violation) can never
            // prove equality, so it is offered rather than dropped silently.
            let offer = match recovery.to_source_bytes() {
                Ok(recovered) => {
                    blake3::hash(&recovered).as_bytes() != blake3::hash(&content).as_bytes()
                }
                Err(_) => true,
            };
            if offer {
                self.notice.send(crate::channels::Notice::RecoveryOffer {
                    note: flavite_buffer::manager::NoteRef::new(read.path.clone()),
                });
            }
        }
        if absent {
            self.absent_on_open
                .lock_recovering()
                .insert(read.path.clone());
        }
        // The flight slot is already consumed by the pump; keep the jobs
        // sender referenced so the field stays alive for later flights.
        let _ = open_jobs;
        if read.policy == OpenPolicy::CreateNew {
            SessionResult::Success(CommandResult::NoteCreated {
                buffer_id,
                revision,
                path: read.path,
            })
        } else {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            })
        }
    }

    /// Typed outcome for a source that needs an explicit encoding decision
    /// before it can be attached, indexed or saved (§6.10). The notice and
    /// the returned result share one stable code; the data payload carries
    /// the sanitized reason kind — never note content or raw bytes.
    fn encoding_choice_required(&self, path: &Path, reason: &ChoiceReason) -> SessionResult {
        self.notice.send(crate::channels::Notice::ActionRequired {
            code: CHOICE_REQUIRED_CODE,
            note: Some(flavite_buffer::manager::NoteRef::new(path.to_path_buf())),
        });
        SessionResult::ActionRequired {
            code: CHOICE_REQUIRED_CODE,
            data: Some(crate::encoding::choice_payload_json(reason)),
        }
    }

    /// The published device-local encoding-hints view (§6.10). Workers and
    /// facades acquire this immutable snapshot instead of querying a label.
    #[must_use]
    pub fn encoding_hints(&self) -> Arc<crate::encoding::HintsView> {
        self.encoding_hints.view()
    }

    /// Confirm one source's interpretation: stores the hint bound to the
    /// observed raw hash and publishes the new generation atomically. The
    /// caller must have verified the descriptor decodes the current bytes.
    ///
    /// # Errors
    ///
    /// [`crate::encoding::HintsWriteError`] when the device-local store
    /// cannot durably protect the confirmation.
    pub fn confirm_encoding_hint(
        &self,
        path: &Path,
        format: flavite_syntax::SourceFormat,
        observed_raw_hash: [u8; 32],
        preserve_leading_signature: bool,
        nul_content_confirmed: bool,
    ) -> Result<u64, crate::encoding::HintsWriteError> {
        let native_key = hex::encode(native_note_key(path, &self.identity.root));
        self.encoding_hints.update(|record| {
            record.entries.insert(
                native_key,
                crate::encoding::HintEntry {
                    encoding: format.encoding(),
                    bom: format.bom(),
                    bound_raw_hash: observed_raw_hash,
                    preserve_leading_signature,
                    nul_content_confirmed,
                },
            );
        })
    }

    /// Core-issued 128-bit plan id: vault-nonce prefix plus a monotonic
    /// counter (§6.1: unguessable, core-issued).
    fn next_text_plan_id(&self) -> u128 {
        let counter = self.text_plan_counter.fetch_add(1, Ordering::Relaxed);
        (u128::from(self.text_plan_nonce) << 64) | u128::from(counter)
    }

    /// Reclaim expired plans and over-cap retained receipts (§6.9). Called
    /// on every admission/apply so years of operation need no restart.
    fn sweep_text_plans(&self) {
        let now = std::time::Instant::now();
        let mut plans = self.text_plans.lock_recovering();
        plans.retain(|_, entry| match &entry.consumed {
            Some((at, _)) => now.duration_since(*at) < TEXT_PLAN_RESULT_TTL,
            None => now.duration_since(entry.created) < TEXT_PLAN_TTL,
        });
        let mut consumed_ids: Vec<(std::time::Instant, u128)> = plans
            .iter()
            .filter_map(|(id, entry)| entry.consumed.as_ref().map(|(at, _)| (*at, *id)))
            .collect();
        if consumed_ids.len() > MAX_RETAINED_TEXT_RESULTS {
            consumed_ids.sort();
            let excess = consumed_ids.len() - MAX_RETAINED_TEXT_RESULTS;
            for (_, id) in consumed_ids.into_iter().take(excess) {
                plans.remove(&id);
            }
        }
    }

    /// Admit one replacement preparation (D-05/D-06): validate the
    /// buffer/revision, reserve a plan slot, and dispatch the immutable
    /// snapshot to a bounded prepare worker. Empty queries are Idle, not a
    /// match-everywhere plan.
    fn prepare_replace(
        &self,
        request_id: u64,
        request: crate::text_edit::ReplaceRequest,
    ) -> SessionResult {
        self.sweep_text_plans();
        if self.text_plans_full() {
            return SessionResult::Busy;
        }
        let buffer_id = request.buffer_id;
        let text = match self.snapshot_for_revision(buffer_id, request.revision) {
            Ok(text) => text,
            Err(result) => return result,
        };
        if self.text_queue_full() {
            return SessionResult::Busy;
        }
        match &self.text_jobs {
            Some(jobs)
                if jobs
                    .send(TextJob {
                        request_id,
                        text,
                        work: TextWork::Prepare(request),
                    })
                    .is_ok() =>
            {
                self.pending_text.lock_recovering().insert(request_id);
                SessionResult::Pending { request_id }
            }
            _ => SessionResult::Error {
                message: "replacement prepare workers are unavailable".into(),
            },
        }
    }

    /// Admit one buffer find (S03): the same snapshot admission as
    /// replacement, the same bounded worker pool, no plan slot (a find is
    /// read-only). An empty query is Idle and never dispatched.
    fn text_find(&self, request_id: u64, request: crate::text_edit::FindRequest) -> SessionResult {
        if request.query.is_empty()
            && request.mode == flavite_text_search::matcher::MatchMode::Literal
        {
            return SessionResult::Error {
                message: crate::text_edit::IDLE_QUERY.to_owned(),
            };
        }
        let buffer_id = request.buffer_id;
        let text = match self.snapshot_for_revision(buffer_id, request.revision) {
            Ok(text) => text,
            Err(result) => return result,
        };
        if self.text_queue_full() {
            return SessionResult::Busy;
        }
        match &self.text_jobs {
            Some(jobs)
                if jobs
                    .send(TextJob {
                        request_id,
                        text,
                        work: TextWork::Find(request),
                    })
                    .is_ok() =>
            {
                self.pending_text.lock_recovering().insert(request_id);
                SessionResult::Pending { request_id }
            }
            _ => SessionResult::Error {
                message: "find workers are unavailable".into(),
            },
        }
    }

    /// Shared admission guard: at most [`TEXT_QUEUE_MAX`] text jobs may be
    /// queued across both workers (NFR-10); beyond that the caller gets an
    /// explicit Busy instead of unbounded queue growth.
    fn text_queue_full(&self) -> bool {
        self.text_queue
            .as_ref()
            .is_some_and(|queue| queue.len() >= TEXT_QUEUE_MAX)
    }

    fn text_plans_full(&self) -> bool {
        let plans = self.text_plans.lock_recovering();
        plans
            .values()
            .filter(|entry| entry.consumed.is_none())
            .count()
            >= MAX_UNCONSUMED_TEXT_PLANS
    }

    /// Fetch (or populate) the shared immutable snapshot for a revision,
    /// with the buffer/revision guards both text admissions require.
    #[allow(clippy::result_large_err)] // SessionResult is the existing typed facade outcome.
    fn snapshot_for_revision(
        &self,
        buffer_id: BufferId,
        revision: Revision,
    ) -> Result<std::sync::Arc<str>, SessionResult> {
        let manager = self.buffer_manager.lock_recovering();
        let Some(buffer) = manager.get_buffer_by_id(buffer_id) else {
            return Err(SessionResult::Error {
                message: "Buffer not found".into(),
            });
        };
        if buffer.revision() != revision {
            return Err(SessionResult::StaleRevision {
                buffer_id,
                expected: revision,
                actual: buffer.revision(),
            });
        }
        let mut cache = self.shared_snapshots.lock_recovering();
        Ok(match cache.get(&buffer_id) {
            Some((cached, text)) if *cached == revision => text.clone(),
            _ => {
                let text: std::sync::Arc<str> = buffer.text().to_owned().into();
                cache.insert(buffer_id, (revision, text.clone()));
                text
            }
        })
    }

    /// Install one prepared replacement (D-06): the plan id names a
    /// core-issued candidate; buffer/revision guards re-run at install;
    /// one isolated Undo unit is recorded; the consumed result replays for
    /// the retry window. Expired/unknown plans return Gone without effect.
    fn apply_replace(
        &self,
        buffer_id: BufferId,
        revision: Revision,
        plan_id: u128,
    ) -> SessionResult {
        self.sweep_text_plans();
        let now = std::time::Instant::now();
        let entry = {
            let plans = self.text_plans.lock_recovering();
            plans.get(&plan_id).map(|entry| {
                (
                    entry.ops.clone(),
                    entry.match_count,
                    entry.options_digest,
                    entry.buffer_id,
                    entry.revision,
                    entry.consumed.as_ref().map(|(at, _)| *at),
                )
            })
        };
        let Some((ops, _match_count, _digest, entry_buffer, entry_revision, consumed_at)) = entry
        else {
            return SessionResult::ActionRequired {
                code: "text-plan-expired",
                data: None,
            };
        };
        if let Some(receipt) = consumed_at
            .filter(|at| now.duration_since(*at) < TEXT_PLAN_RESULT_TTL)
            .and_then(|_| {
                let plans = self.text_plans.lock_recovering();
                plans
                    .get(&plan_id)
                    .and_then(|entry| entry.consumed.as_ref().map(|(_, receipt)| receipt.clone()))
            })
        {
            return receipt;
        }
        let _ = _match_count;
        let _ = _digest;
        if entry_buffer != buffer_id {
            return SessionResult::Error {
                message: "replacement plan belongs to another buffer".into(),
            };
        }
        if entry_revision != revision {
            return SessionResult::StaleRevision {
                buffer_id,
                expected: entry_revision,
                actual: revision,
            };
        }
        let result = {
            let mut manager = self.buffer_manager.lock_recovering();
            let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
                return SessionResult::Error {
                    message: "Buffer no longer open".into(),
                };
            };
            if buffer.revision() != entry_revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: entry_revision,
                    actual: buffer.revision(),
                };
            }
            match buffer
                .apply_local_edits_with_policy(&ops, flavite_buffer::undo::UndoPolicy::Isolated)
            {
                Some((new_revision, _)) => SessionResult::Success(CommandResult::EditsApplied {
                    buffer_id,
                    new_revision,
                }),
                None => SessionResult::Error {
                    message: "prepared batch no longer applies".into(),
                },
            }
        };
        {
            let mut plans = self.text_plans.lock_recovering();
            if let Some(entry) = plans.get_mut(&plan_id) {
                entry.consumed = Some((now, result.clone()));
            }
        }
        result
    }

    /// Core-issued finder id (128-bit; hex on the wire).
    fn next_finder_id(&self) -> u128 {
        let counter = self.finder_counter.fetch_add(1, Ordering::Relaxed);
        (u128::from(self.finder_nonce) << 64) | u128::from(counter)
    }

    /// Core-issued preview id (128-bit; hex on the wire).
    fn next_preview_id(&self) -> u128 {
        let counter = self.preview_counter.fetch_add(1, Ordering::Relaxed);
        (u128::from(self.preview_nonce) << 64) | u128::from(counter)
    }

    /// The read surface vault scan workers use (RFC-0006 S07).
    #[must_use]
    pub fn preview_ctx(&self) -> crate::replace::preview::PreviewCtx {
        crate::replace::preview::PreviewCtx {
            actor: self.actor.clone(),
            manager: Arc::clone(&self.buffer_manager),
            root: self.identity.root.clone(),
            vault_uuid: self.identity.uuid,
        }
    }

    /// The durable-replacement coordinator context (RFC-0006 S08/S09).
    #[must_use]
    pub fn coordinator_ctx(&self) -> crate::replace::CoordinatorCtx {
        crate::replace::CoordinatorCtx {
            actor: self.actor.clone(),
            root: self.identity.root.clone(),
            data_dir: self.data_dir.clone(),
            vault_uuid: self.identity.uuid,
            leases: Arc::clone(&self.leases),
            manager: Arc::clone(&self.buffer_manager),
            recovery_writer: self.recovery_writer.clone(),
        }
    }

    /// The resolved private OS data directory.
    #[must_use]
    pub fn resolved_data_dir(&self) -> Option<PathBuf> {
        crate::identity::resolve_data_dir(self.data_dir.as_deref())
    }

    /// Shared per-note source leases (RFC-0006 S08/D-10).
    #[must_use]
    pub fn source_leases(&self) -> Arc<crate::source_leases::SourceLeaseRegistry> {
        Arc::clone(&self.leases)
    }

    /// Load (or create) the device/vault operation key (RFC-0006 §6.9).
    /// The key store lives outside the vault; inability to protect it
    /// refuses applicable replacement operations.
    pub fn operation_key(&self) -> Result<crate::replace::OperationKey, String> {
        let data_dir = self
            .resolved_data_dir()
            .ok_or_else(|| "private data directory is unavailable".to_owned())?;
        crate::operation_key::load_or_create_key(&data_dir, &self.identity.uuid)
            .map_err(|error| error.to_string())
    }

    /// Startup recovery of durable replacement jobs (RFC-0006 D-11/§6.4):
    /// classify intents against durable disk hashes; never auto-continue
    /// untouched items. Jobs whose authenticated log cannot be verified are
    /// quarantined (inspection only), never guessed or silently discarded.
    fn recover_replace_jobs(&self) {
        let store = crate::replace::ReplaceStore::new(&self.identity.root);
        let io = crate::replace::ActorOpStoreIo::new(self.actor.clone());
        let Ok(Some(dirs)) = io.list_dir(store.base()) else {
            return;
        };
        let Some(data_dir) = self.resolved_data_dir() else {
            return;
        };
        let Ok(key) = self.operation_key() else {
            return;
        };
        for dir in dirs {
            if dir.len() != 32 || !dir.chars().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let job_dir = store.base().join(&dir);
            let Some(selection_bytes) = io.read(&job_dir.join("selection.bin")).ok().flatten()
            else {
                continue; // sealed previews without an apply have no selection
            };
            if selection_bytes.len() < 8 || (selection_bytes.len() - 8) % 8 != 0 {
                continue;
            }
            let selection_count =
                u64::from_le_bytes(selection_bytes[..8].try_into().expect("8 bytes"));
            let mut selected_ids = Vec::new();
            for chunk in selection_bytes[8..].as_chunks::<8>().0 {
                selected_ids.push(u64::from_le_bytes(*chunk));
            }
            if u64::try_from(selected_ids.len()).unwrap_or(u64::MAX) != selection_count {
                continue;
            }
            let Ok(Some(manifest)) = store.read_manifest(&io, &dir) else {
                continue;
            };
            let Ok(log) = crate::replace::JobLog::open(
                Box::new(io.clone()),
                &store,
                &dir,
                key.clone(),
                &data_dir,
                &self.identity.uuid,
            ) else {
                // Corrupt/unknown logs quarantine the operation: leave it
                // out of the resumable set (inspection-only via doctor).
                continue;
            };
            let mut record_effects: std::collections::HashMap<u64, crate::replace::ItemEffect> =
                std::collections::HashMap::new();
            let mut saw_revert = false;
            {
                let records = log.records().ok();
                if let Some(records) = records {
                    for record in records {
                        match record.action {
                            crate::replace::ItemAction::Outcome { outcome } => {
                                let effect = match outcome {
                                    crate::replace::PersistedItemOutcome::Saved => {
                                        crate::replace::ItemEffect::Saved
                                    }
                                    crate::replace::PersistedItemOutcome::IndexPending => {
                                        crate::replace::ItemEffect::IndexPending
                                    }
                                    crate::replace::PersistedItemOutcome::AppliedConflict => {
                                        crate::replace::ItemEffect::AppliedConflict
                                    }
                                    crate::replace::PersistedItemOutcome::AppliedDraft => {
                                        crate::replace::ItemEffect::AppliedDraft
                                    }
                                    crate::replace::PersistedItemOutcome::Stale => {
                                        crate::replace::ItemEffect::Stale
                                    }
                                    crate::replace::PersistedItemOutcome::Busy => {
                                        crate::replace::ItemEffect::Busy
                                    }
                                    crate::replace::PersistedItemOutcome::Denied => {
                                        crate::replace::ItemEffect::Unsupported
                                    }
                                    crate::replace::PersistedItemOutcome::NeedsRecovery => {
                                        crate::replace::ItemEffect::NeedsRecovery
                                    }
                                    crate::replace::PersistedItemOutcome::SatisfiedUnattributed => {
                                        crate::replace::ItemEffect::SatisfiedUnattributed
                                    }
                                };
                                record_effects.insert(record.item_id, effect);
                            }
                            crate::replace::ItemAction::RevertOutcome { outcome } => {
                                saw_revert = true;
                                let effect = match outcome {
                                    crate::replace::PersistedRevertOutcome::Reverted => {
                                        crate::replace::ItemEffect::Reverted
                                    }
                                    crate::replace::PersistedRevertOutcome::RevertedDraft => {
                                        crate::replace::ItemEffect::RevertedDraft
                                    }
                                    crate::replace::PersistedRevertOutcome::Stale => {
                                        crate::replace::ItemEffect::Stale
                                    }
                                    crate::replace::PersistedRevertOutcome::NeedsRecovery => {
                                        crate::replace::ItemEffect::NeedsRecovery
                                    }
                                };
                                record_effects.insert(record.item_id, effect);
                            }
                            _ => {}
                        }
                    }
                }
            }
            let draft_state =
                crate::replace::read_draft_state(&io, &store, &dir).unwrap_or_default();
            let mut items: Vec<crate::replace::JobItem> = Vec::new();
            for note in &manifest.notes {
                if !selected_ids.contains(&note.item_id) {
                    continue;
                }
                let effect = match record_effects.get(&note.item_id) {
                    Some(effect) => Some(*effect),
                    None => {
                        if note.source_kind == crate::replace::SourceKind::Draft {
                            // Draft classification (S09/D-11): the recovery
                            // checkpoint is the marker. Recorded text equal
                            // to the after-payload = satisfied (never
                            // re-applied); equal to the before-payload =
                            // not started; otherwise divergence.
                            let note_key = crate::session::native_note_key(
                                &self.identity.root.join(&note.path),
                                &self.identity.root,
                            );
                            let recovery = self.recovery_writer.as_ref().and_then(|writer| {
                                writer.lock_recovering().read_for_note_key(&note_key)
                            });
                            match recovery {
                                Some(file) => {
                                    let after_text = store
                                        .read_payload(&io, &dir, &note.after)
                                        .ok()
                                        .flatten()
                                        .and_then(|bytes| {
                                            flavite_syntax::text::TextSnapshot::new(&bytes).ok()
                                        })
                                        .map(|snapshot| snapshot.text().to_owned());
                                    let before_text = store
                                        .read_payload(&io, &dir, &note.before)
                                        .ok()
                                        .flatten()
                                        .and_then(|bytes| {
                                            flavite_syntax::text::TextSnapshot::new(&bytes).ok()
                                        })
                                        .map(|snapshot| snapshot.text().to_owned());
                                    match after_text {
                                        Some(after) if file.text == after => {
                                            Some(crate::replace::ItemEffect::SatisfiedUnattributed)
                                        }
                                        _ if before_text.as_deref() == Some(file.text.as_str()) => {
                                            None
                                        }
                                        _ => Some(crate::replace::ItemEffect::NeedsRecovery),
                                    }
                                }
                                None => None,
                            }
                        } else {
                            // Classify against durable disk hashes (D-11):
                            // before-hash = not yet applied; after-hash =
                            // satisfied (never overwritten again); neither =
                            // divergence.
                            let full = self.identity.root.join(&note.path);
                            let current = self
                                .actor
                                .read_bounded(full, flavite_index::MAX_NOTE_BYTES as u64);
                            match current {
                                Ok(Some(bytes)) => {
                                    let hash = hex::encode(blake3::hash(&bytes).as_bytes());
                                    if hash == note.disk_hash {
                                        None
                                    } else if hash == note.after.hash {
                                        Some(crate::replace::ItemEffect::SatisfiedUnattributed)
                                    } else {
                                        Some(crate::replace::ItemEffect::NeedsRecovery)
                                    }
                                }
                                Ok(None) => Some(crate::replace::ItemEffect::Stale),
                                Err(_) => Some(crate::replace::ItemEffect::NeedsRecovery),
                            }
                        }
                    }
                };
                let mut record = note.clone();
                if let Some(applied) = record.draft_lineage.as_mut()
                    && let Some(revision) = draft_state.get(&note.item_id).copied()
                {
                    applied.applied_revision = Some(revision);
                }
                items.push(crate::replace::JobItem { record, effect });
            }
            let mut phase = crate::replace::JobPhase::Paused;
            for record in log.records().ok().into_iter().flatten() {
                if let crate::replace::ItemAction::JobTerminal { phase: phase_label } =
                    record.action
                {
                    phase = match phase_label.as_str() {
                        "completed" => crate::replace::JobPhase::Completed,
                        "partial" => crate::replace::JobPhase::Partial,
                        "cancelled" => crate::replace::JobPhase::Cancelled,
                        "paused" => crate::replace::JobPhase::Paused,
                        "needs-recovery" => crate::replace::JobPhase::NeedsRecovery,
                        _ => crate::replace::JobPhase::NeedsRecovery,
                    };
                }
            }
            if phase == crate::replace::JobPhase::Paused
                && items.iter().any(|item| {
                    matches!(item.effect, Some(crate::replace::ItemEffect::NeedsRecovery))
                })
            {
                phase = crate::replace::JobPhase::NeedsRecovery;
            }
            let totals = crate::replace::JobTotals::from_items(&items);
            let state = Arc::new(Mutex::new(JobState {
                phase,
                mode: if saw_revert {
                    JobMode::Revert
                } else {
                    JobMode::Apply
                },
                items,
                totals,
            }));
            let job_id = match u128::from_str_radix(&dir, 16) {
                Ok(id) => id,
                Err(_) => continue,
            };
            self.replace_jobs_map.lock_recovering().insert(
                job_id,
                JobEntry {
                    state,
                    cancel: Arc::new(AtomicBool::new(false)),
                    preview_id: 0,
                    selection_revision: 0,
                    idempotency_key: None,
                },
            );
        }
    }

    /// Whether a source path is leased by a replacement job (RFC-0006
    /// D-10: input to a leased note is refused, never buffered/replayed).
    fn leased_by_replace(&self, path: &Path) -> bool {
        let relative = path.strip_prefix(&self.identity.root).unwrap_or(path);
        let key = relative.as_os_str().as_encoded_bytes();
        self.leases
            .is_leased_by(key, crate::source_leases::LeaseOwner::Replace)
    }

    /// Typed D-10 refusal for a source whose durable replacement lease is
    /// live: input is refused with "Replacement in progress", never
    /// buffered for replay after the lease.
    fn replacement_in_progress(&self) -> SessionResult {
        SessionResult::ActionRequired {
            code: "replacement-in-progress",
            data: None,
        }
    }

    /// Shared admission guard: at most [`REPLACE_QUEUE_MAX`] replace jobs
    /// may be queued before explicit Busy (NFR-10).
    fn replace_queue_full(&self) -> bool {
        self.replace_queue
            .as_ref()
            .is_some_and(|queue| queue.len() >= REPLACE_QUEUE_MAX)
    }

    /// Reclaim expired source sessions on access.
    fn sweep_source_sessions(&self) {
        let now = crate::replace::preview::unix_ms_now();
        let mut finders = self.finders.lock_recovering();
        finders.retain(|_, entry| entry.expires_at_unix_ms > now);
        let mut previews = self.previews.lock_recovering();
        previews.retain(|_, entry| entry.expires_at_unix_ms > now);
    }

    /// Admit one vault source find (S07): read-only, frozen scope, exact
    /// per-note matches on a bounded worker. Read-only; no plan slot.
    fn source_find_start(
        &self,
        request_id: u64,
        request: crate::replace::preview::SourceFindRequest,
    ) -> SessionResult {
        if request.query.is_empty()
            && request.mode == flavite_text_search::matcher::MatchMode::Literal
        {
            return SessionResult::Error {
                message: crate::text_edit::IDLE_QUERY.to_owned(),
            };
        }
        self.sweep_source_sessions();
        if self.finders.lock_recovering().len() >= MAX_LIVE_FINDERS {
            return SessionResult::Busy;
        }
        if self.replace_queue_full() {
            return SessionResult::Busy;
        }
        let finder_id = self.next_finder_id();
        self.pending_replace.lock_recovering().insert(request_id);
        match &self.replace_jobs {
            Some(jobs)
                if jobs
                    .send(ReplaceJob {
                        request_id,
                        work: ReplaceWork::Find { finder_id, request },
                    })
                    .is_ok() =>
            {
                SessionResult::Pending { request_id }
            }
            _ => {
                self.pending_replace.lock_recovering().remove(&request_id);
                SessionResult::Error {
                    message: "source find workers are unavailable".into(),
                }
            }
        }
    }

    /// One continuation page of an open finder (S07). Pages slice the
    /// private spool by (note ordinal, match ordinal); replay is
    /// idempotent and gaps are impossible by construction.
    fn source_find_page(
        &self,
        finder_id: u128,
        note_index: u64,
        match_offset: u64,
        page_len: usize,
    ) -> SessionResult {
        self.sweep_source_sessions();
        let entry = {
            let finders = self.finders.lock_recovering();
            finders.get(&finder_id).cloned()
        };
        let Some(entry) = entry else {
            return SessionResult::ActionRequired {
                code: "source-find-expired",
                data: None,
            };
        };
        let page_len = page_len.clamp(1, 200);
        let page = match self.find_page(&entry, note_index, match_offset, page_len) {
            Ok(page) => page,
            Err(message) => {
                return SessionResult::Error { message };
            }
        };
        SessionResult::Success(CommandResult::SourceFindPaged { finder_id, page })
    }

    /// Close a finder and drop its spool (best effort; expiry sweeps any
    /// remainder).
    fn source_find_close(&self, finder_id: u128) -> SessionResult {
        let removed = self.finders.lock_recovering().remove(&finder_id);
        if removed.is_some() {
            let base = crate::replace::preview::source_find_base(&self.identity.root)
                .join(hex::encode(finder_id.to_be_bytes()));
            let io = crate::replace::ActorOpStoreIo::new(self.actor.clone());
            let _ = io.remove(&base.join("spans"));
            let _ = io.remove(&base);
        }
        SessionResult::Success(CommandResult::SourceFindClosed)
    }

    /// Admit one replacement-preview prepare (S07/S09): full scan and seal
    /// on a bounded worker; nothing is applied. `include_drafts` (S09)
    /// explicitly adds Dirty buffers as draft candidates; conflicted and
    /// orphaned buffers remain excluded until resolved.
    fn replace_preview_prepare(
        &self,
        request_id: u64,
        request: crate::replace::preview::PreviewRequest,
    ) -> SessionResult {
        self.sweep_source_sessions();
        if self.previews.lock_recovering().len() >= MAX_LIVE_PREVIEWS {
            return SessionResult::Busy;
        }
        if self.replace_queue_full() {
            return SessionResult::Busy;
        }
        let preview_id = self.next_preview_id();
        self.pending_replace.lock_recovering().insert(request_id);
        match &self.replace_jobs {
            Some(jobs)
                if jobs
                    .send(ReplaceJob {
                        request_id,
                        work: ReplaceWork::Preview {
                            preview_id,
                            request,
                        },
                    })
                    .is_ok() =>
            {
                SessionResult::Pending { request_id }
            }
            _ => {
                self.pending_replace.lock_recovering().remove(&request_id);
                SessionResult::Error {
                    message: "replacement preview workers are unavailable".into(),
                }
            }
        }
    }

    /// Record a selection revision on a sealed preview (S07/D-09): a local
    /// identity, never a mutation grant. Selection never mutates the sealed
    /// plan or any note.
    fn replace_select(
        &self,
        preview_id: u128,
        selected: crate::session::ReplaceSelection,
    ) -> SessionResult {
        self.sweep_source_sessions();
        let (revision, digest) = {
            let mut previews = self.previews.lock_recovering();
            let Some(entry) = previews.get_mut(&preview_id) else {
                return SessionResult::ActionRequired {
                    code: "preview-expired",
                    data: None,
                };
            };
            let manifest = entry.manifest.clone();
            let mut selected_ids: Vec<u64> = match selected {
                crate::session::ReplaceSelection::All => manifest
                    .notes
                    .iter()
                    .filter(|n| !n.no_change)
                    .map(|n| n.item_id)
                    .collect(),
                crate::session::ReplaceSelection::None => Vec::new(),
                crate::session::ReplaceSelection::Keys(keys) => {
                    let wanted: std::collections::BTreeSet<&str> =
                        keys.iter().map(String::as_str).collect();
                    let mut ids = Vec::new();
                    for note in &manifest.notes {
                        if note.no_change {
                            continue;
                        }
                        if wanted.contains(note.path_native.as_str()) {
                            ids.push(note.item_id);
                        }
                    }
                    if ids.len() != wanted.len() {
                        return SessionResult::Error {
                            message: "selection names a note outside this preview".into(),
                        };
                    }
                    ids
                }
            };
            selected_ids.sort_unstable();
            selected_ids.dedup();
            entry.selected_ids = selected_ids.clone();
            entry.selection_revision = entry.selection_revision.saturating_add(1);
            let revision = entry.selection_revision;
            let digest =
                crate::replace::coordinator::selection_digest(preview_id, revision, &selected_ids);
            (revision, digest)
        };
        SessionResult::Success(CommandResult::ReplaceSelected {
            selection_revision: revision,
            selection_digest: digest,
        })
    }

    /// Report the current status of a sealed preview (S07).
    fn replace_status(&self, preview_id: u128) -> SessionResult {
        self.sweep_source_sessions();
        let entry = {
            let previews = self.previews.lock_recovering();
            previews
                .get(&preview_id)
                .map(|entry| entry.manifest.clone())
        };
        let Some(manifest) = entry else {
            return SessionResult::ActionRequired {
                code: "preview-expired",
                data: None,
            };
        };
        SessionResult::Success(CommandResult::ReplaceStatus {
            preview_id,
            manifest,
        })
    }

    /// Admit a durable replacement apply (RFC-0006 S08/D-10).
    ///
    /// The first Apply freezes the selection revision; a different revision
    /// is `SelectionLocked`. One active apply runs per vault; further
    /// admissions are Busy. The job is persisted (selection file + sealed
    /// manifest) before the worker starts.
    #[allow(clippy::too_many_lines)]
    fn replace_apply(
        &self,
        request_id: u64,
        preview_id: u128,
        selection_revision: u64,
        idempotency_key: u128,
    ) -> SessionResult {
        self.sweep_source_sessions();
        if self.active_apply.lock_recovering().is_some() {
            return SessionResult::Busy;
        }
        // The operation key must be usable BEFORE any job start.
        let key = match self.operation_key() {
            Ok(key) => key,
            Err(message) => {
                return SessionResult::ActionRequired {
                    code: "replacement-key-unavailable",
                    data: Some(message),
                };
            }
        };
        let preview = {
            let previews = self.previews.lock_recovering();
            previews.get(&preview_id).map(|entry| {
                (
                    entry.manifest.clone(),
                    entry.selected_ids.clone(),
                    entry.selection_revision,
                )
            })
        };
        let Some((manifest, selected_ids, current_revision)) = preview else {
            return SessionResult::ActionRequired {
                code: "preview-expired",
                data: None,
            };
        };
        if current_revision == 0 {
            return SessionResult::Error {
                message: "select notes before applying".into(),
            };
        }
        if selection_revision != current_revision {
            return SessionResult::Error {
                message: "selection locked: the preview has a newer selection revision".into(),
            };
        }
        if selected_ids.is_empty() {
            return SessionResult::Error {
                message: "the selection names no notes".into(),
            };
        }
        // Idempotent replay: an existing completed job for the same frozen
        // selection returns its receipt; a live one is Busy.
        {
            let jobs = self.replace_jobs_map.lock_recovering();
            if let Some((existing_id, entry)) = jobs.iter().find(|(_, entry)| {
                entry.preview_id == preview_id && entry.selection_revision == selection_revision
            }) {
                let state = entry.state.lock_recovering();
                return match state.phase {
                    // A finished run replays its recorded receipt (D-11:
                    // repeated Apply requests return the same receipt);
                    // NeedsRecovery's recorded state IS its receipt.
                    crate::replace::JobPhase::Completed
                    | crate::replace::JobPhase::Partial
                    | crate::replace::JobPhase::Cancelled
                    | crate::replace::JobPhase::NeedsRecovery => {
                        SessionResult::Success(CommandResult::ReplaceJobFinished {
                            job_id: *existing_id,
                            phase: state.phase,
                            totals: state.totals.clone(),
                        })
                    }
                    // A live run or a paused job awaiting explicit Resume is
                    // busy, not a receipt.
                    crate::replace::JobPhase::Ready
                    | crate::replace::JobPhase::Applying
                    | crate::replace::JobPhase::Paused => SessionResult::Busy,
                };
            }
        }
        if self.replace_queue_full() {
            return SessionResult::Busy;
        }
        // The job reuses the preview id: payload objects and the sealed
        // manifest already live under this job dir (content-addressed), so
        // Apply references the same store objects.
        let job_id = preview_id;
        let job_hex = hex::encode(job_id.to_be_bytes());
        // Persist the frozen selection under the job dir before starting.
        let store = crate::replace::ReplaceStore::new(&self.identity.root);
        let io = crate::replace::ActorOpStoreIo::new(self.actor.clone());
        if let Err(message) =
            crate::replace::coordinator::write_selection(&io, &store, &job_hex, &selected_ids)
        {
            return SessionResult::Error { message };
        }
        // Stamp the frozen selection onto the sealed manifest (D-09/§6.4):
        // after restart a resumed job can prove the on-disk selection
        // matches the revision the caller authorized.
        let selection_digest =
            crate::replace::coordinator::selection_digest(job_id, current_revision, &selected_ids);
        if let Err(error) =
            store.stamp_selection(&io, &job_hex, current_revision, &selection_digest)
        {
            return SessionResult::Error {
                message: error.to_string(),
            };
        }
        let selected_set: std::collections::BTreeSet<u64> = selected_ids.iter().copied().collect();
        let items: Vec<crate::replace::JobItem> = manifest
            .notes
            .iter()
            .filter(|note| !note.no_change && selected_set.contains(&note.item_id))
            .map(|note| crate::replace::JobItem {
                record: note.clone(),
                effect: None,
            })
            .collect();
        let state = Arc::new(Mutex::new(JobState {
            phase: crate::replace::JobPhase::Applying,
            mode: JobMode::Apply,
            items,
            totals: crate::replace::JobTotals::default(),
        }));
        let cancel = Arc::new(AtomicBool::new(false));
        let ctx = self.coordinator_ctx();
        self.pending_replace.lock_recovering().insert(request_id);
        let admitted = self.replace_jobs.as_ref().is_some_and(|jobs| {
            jobs.send(ReplaceJob {
                request_id,
                work: ReplaceWork::Apply {
                    job_id,
                    ctx,
                    key,
                    store,
                    state: Arc::clone(&state),
                    cancel: Arc::clone(&cancel),
                },
            })
            .is_ok()
        });
        if !admitted {
            self.pending_replace.lock_recovering().remove(&request_id);
            return SessionResult::Error {
                message: "replacement apply workers are unavailable".into(),
            };
        }
        self.replace_jobs_map.lock_recovering().insert(
            job_id,
            JobEntry {
                state,
                cancel,
                preview_id,
                selection_revision,
                idempotency_key: Some(idempotency_key),
            },
        );
        *self.active_apply.lock_recovering() = Some(job_id);
        SessionResult::Pending { request_id }
    }

    /// Cancel a durable job: stop admission of the next item; an item past
    /// its durable intent point finishes. Idempotent.
    fn replace_cancel(&self, job_id: u128) -> SessionResult {
        let mut jobs = self.replace_jobs_map.lock_recovering();
        let Some(entry) = jobs.get_mut(&job_id) else {
            return SessionResult::ActionRequired {
                code: "job-unknown",
                data: None,
            };
        };
        let phase = entry.state.lock_recovering().phase;
        if matches!(
            phase,
            crate::replace::JobPhase::Completed
                | crate::replace::JobPhase::Cancelled
                | crate::replace::JobPhase::NeedsRecovery
        ) {
            return SessionResult::Success(CommandResult::ReplaceCancelled { job_id });
        }
        entry.cancel.store(true, Ordering::Relaxed);
        SessionResult::Success(CommandResult::ReplaceCancelled { job_id })
    }

    /// Explicitly resume a paused/partial/cancelled job. Untouched items run
    /// with their unchanged guards; a fresh grant is rechecked per item.
    /// Idempotent within the session.
    fn replace_resume(
        &self,
        request_id: u64,
        job_id: u128,
        idempotency_key: u128,
    ) -> SessionResult {
        self.sweep_source_sessions();
        if self.active_apply.lock_recovering().is_some() {
            return SessionResult::Busy;
        }
        let entry = {
            let jobs = self.replace_jobs_map.lock_recovering();
            jobs.get(&job_id).map(|entry| {
                (
                    Arc::clone(&entry.state),
                    Arc::clone(&entry.cancel),
                    entry.selection_revision,
                )
            })
        };
        let Some((state, cancel, _revision)) = entry else {
            return SessionResult::ActionRequired {
                code: "job-unknown",
                data: None,
            };
        };
        let phase = state.lock_recovering().phase;
        if matches!(phase, crate::replace::JobPhase::Completed) {
            return SessionResult::Success(CommandResult::ReplaceJobFinished {
                job_id,
                phase,
                totals: state.lock_recovering().totals.clone(),
            });
        }
        // A NeedsRecovery job is crossable by an explicit Resume (§6.4:
        // NeedsRecovery --inspect--> Paused | Partial). The coordinator
        // reconciles every nonterminal item through the guarded apply —
        // before-hash guard plus after-hash satisfaction — so nothing is
        // recomputed against changed content and no uncertain overwrite is
        // repeated. Unverifiable authorization still fails at the first
        // authenticated append, BEFORE any effect (§6.9).
        let key = match self.operation_key() {
            Ok(key) => key,
            Err(message) => {
                return SessionResult::ActionRequired {
                    code: "replacement-key-unavailable",
                    data: Some(message),
                };
            }
        };
        let mode = state.lock_recovering().mode;
        let prior_phase = state.lock_recovering().phase;
        let state_for_restore = Arc::clone(&state);
        state.lock_recovering().phase = crate::replace::JobPhase::Applying;
        cancel.store(false, Ordering::Relaxed);
        let store = crate::replace::ReplaceStore::new(&self.identity.root);
        let ctx = self.coordinator_ctx();
        self.pending_replace.lock_recovering().insert(request_id);
        let work = match mode {
            JobMode::Revert => ReplaceWork::Revert {
                job_id,
                ctx,
                key,
                store,
                state,
                cancel,
            },
            JobMode::Apply => ReplaceWork::Apply {
                job_id,
                ctx,
                key,
                store,
                state,
                cancel,
            },
        };
        let admitted = self
            .replace_jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send(ReplaceJob { request_id, work }).is_ok());
        if !admitted {
            // The phase was set optimistically before admission; restore it
            // so status does not report a phantom running job.
            state_for_restore.lock_recovering().phase = prior_phase;
            self.pending_replace.lock_recovering().remove(&request_id);
            return SessionResult::Error {
                message: "replacement apply workers are unavailable".into(),
            };
        }
        if let Some(entry) = self.replace_jobs_map.lock_recovering().get_mut(&job_id) {
            entry.idempotency_key = Some(idempotency_key);
        }
        *self.active_apply.lock_recovering() = Some(job_id);
        SessionResult::Pending { request_id }
    }

    /// Start a guarded Revert action epoch (RFC-0006 S09/D-11). Eligible
    /// items are those with a durable applied outcome (Saved/IndexPending/
    /// AppliedConflict/SatisfiedUnattributed/AppliedDraft); unapplied items
    /// are left untouched. Guards are rechecked per item on the worker: a
    /// later edit/rename/external write skips that note (Stale) and offers
    /// compare/manual recovery. Idempotent within the session.
    fn replace_revert(
        &self,
        request_id: u64,
        job_id: u128,
        idempotency_key: u128,
    ) -> SessionResult {
        self.sweep_source_sessions();
        if self.active_apply.lock_recovering().is_some() {
            return SessionResult::Busy;
        }
        let entry = {
            let jobs = self.replace_jobs_map.lock_recovering();
            jobs.get(&job_id).map(|entry| {
                (
                    Arc::clone(&entry.state),
                    Arc::clone(&entry.cancel),
                    entry.selection_revision,
                )
            })
        };
        let Some((state, cancel, _revision)) = entry else {
            return SessionResult::ActionRequired {
                code: "job-unknown",
                data: None,
            };
        };
        {
            let guard = state.lock_recovering();
            if matches!(
                guard.phase,
                crate::replace::JobPhase::Applying | crate::replace::JobPhase::Ready
            ) {
                return SessionResult::Busy;
            }
            if guard.mode == JobMode::Revert
                && matches!(
                    guard.phase,
                    crate::replace::JobPhase::Completed
                        | crate::replace::JobPhase::Partial
                        | crate::replace::JobPhase::Cancelled
                )
            {
                // Idempotent replay of an already-run revert epoch.
                return SessionResult::Success(CommandResult::ReplaceJobFinished {
                    job_id,
                    phase: guard.phase,
                    totals: guard.totals.clone(),
                });
            }
        }
        let key = match self.operation_key() {
            Ok(key) => key,
            Err(message) => {
                return SessionResult::ActionRequired {
                    code: "replacement-key-unavailable",
                    data: Some(message),
                };
            }
        };
        state.lock_recovering().mode = JobMode::Revert;
        let prior_phase = state.lock_recovering().phase;
        let state_for_restore = Arc::clone(&state);
        state.lock_recovering().phase = crate::replace::JobPhase::Applying;
        cancel.store(false, Ordering::Relaxed);
        let store = crate::replace::ReplaceStore::new(&self.identity.root);
        let ctx = self.coordinator_ctx();
        self.pending_replace.lock_recovering().insert(request_id);
        let admitted = self.replace_jobs.as_ref().is_some_and(|jobs| {
            jobs.send(ReplaceJob {
                request_id,
                work: ReplaceWork::Revert {
                    job_id,
                    ctx,
                    key,
                    store,
                    state,
                    cancel,
                },
            })
            .is_ok()
        });
        if !admitted {
            // Restore the pre-admission phase: no phantom running job.
            state_for_restore.lock_recovering().phase = prior_phase;
            self.pending_replace.lock_recovering().remove(&request_id);
            return SessionResult::Error {
                message: "replacement revert workers are unavailable".into(),
            };
        }
        if let Some(entry) = self.replace_jobs_map.lock_recovering().get_mut(&job_id) {
            entry.idempotency_key = Some(idempotency_key);
        }
        *self.active_apply.lock_recovering() = Some(job_id);
        SessionResult::Pending { request_id }
    }

    /// Current durable job status.
    fn replace_job_status(&self, job_id: u128) -> SessionResult {
        let entry = self
            .replace_jobs_map
            .lock_recovering()
            .get(&job_id)
            .map(|entry| {
                let state = entry.state.lock_recovering();
                (
                    state.phase,
                    state.totals.clone(),
                    entry.selection_revision,
                    u64::try_from(state.items.len()).unwrap_or(u64::MAX),
                )
            });
        let Some((phase, totals, selection_revision, selected_items)) = entry else {
            return SessionResult::ActionRequired {
                code: "job-unknown",
                data: None,
            };
        };
        SessionResult::Success(CommandResult::ReplaceJobStatus {
            job_id,
            phase,
            totals,
            selection_revision,
            selected_items,
        })
    }

    /// Build one page of a finder session from the spool.
    fn find_page(
        &self,
        entry: &SourceFinderEntry,
        note_index: u64,
        match_offset: u64,
        page_len: usize,
    ) -> Result<SourceFindPageData, String> {
        let io = crate::replace::ActorOpStoreIo::new(self.actor.clone());
        let base = crate::replace::preview::source_find_base(&self.identity.root)
            .join(hex::encode(entry.id.to_be_bytes()))
            .join("spans");
        let mut rows = Vec::new();
        let mut collected = 0usize;
        let mut cursor_note = note_index;
        let mut cursor_match = match_offset;
        let start = entry
            .notes
            .iter()
            .position(|n| n.ordinal >= cursor_note)
            .unwrap_or(entry.notes.len());
        for record in &entry.notes[start..] {
            if collected >= page_len {
                break;
            }
            let bytes = io
                .read(&base.join(&record.spans_file))?
                .ok_or_else(|| "find spool file is missing".to_owned())?;
            if bytes.len() < 8 || (bytes.len() - 8) % 16 != 0 {
                return Err("find spool file is corrupt".to_owned());
            }
            let total = u64::from_le_bytes(bytes[..8].try_into().expect("8 bytes"));
            if ((bytes.len() - 8) / 16) as u64 != total {
                return Err("find spool count mismatch".to_owned());
            }
            if cursor_match >= total {
                cursor_note = record.ordinal + 1;
                cursor_match = 0;
                continue;
            }
            let remaining = (page_len - collected) as u64;
            let take = (total - cursor_match).min(remaining) as usize;
            let mut matches = Vec::with_capacity(take);
            for chunk in bytes[8..]
                .as_chunks::<16>()
                .0
                .iter()
                .skip(cursor_match as usize)
                .take(take)
            {
                let start = u64::from_le_bytes(chunk[..8].try_into().expect("8 bytes"));
                let end = u64::from_le_bytes(chunk[8..].try_into().expect("8 bytes"));
                matches.push((start, end));
            }
            rows.push(MatchRow {
                ordinal: record.ordinal,
                path: record.path.clone(),
                title: record.title.clone(),
                note_matches: record.match_count,
                matches,
            });
            collected += take;
            cursor_match += take as u64;
            if cursor_match >= total {
                cursor_note = record.ordinal + 1;
                cursor_match = 0;
            }
        }
        let next_note_index = if collected >= page_len {
            if cursor_match > 0 {
                Some(cursor_note)
            } else {
                entry
                    .notes
                    .iter()
                    .find(|n| n.ordinal >= cursor_note)
                    .map(|n| n.ordinal)
            }
        } else {
            None
        };
        Ok(SourceFindPageData {
            note_index: cursor_note,
            note_total: u64::try_from(entry.notes.len()).unwrap_or(u64::MAX),
            match_offset: cursor_match,
            rows,
            next_note_index,
            next_match_offset: (cursor_match > 0).then_some(cursor_match),
        })
    }

    /// Install finished S07 scans into their destinations on a Session turn,
    /// completing every correlated caller once.
    fn pump_replace_completions(&self) {
        while let Ok(message) = self.replace_results.try_recv() {
            let (request_id, result) = match message {
                ReplaceOutcomeMessage::Find {
                    request_id,
                    finder_id,
                    outcome,
                } => {
                    let result = match outcome {
                        Ok(found) => {
                            let found = *found;
                            // One entry construction: the first page is cut
                            // from the entry that gets inserted, instead of
                            // cloning all matched notes twice.
                            let entry = SourceFinderEntry {
                                id: finder_id,
                                expires_at_unix_ms: crate::replace::preview::unix_ms_now()
                                    .saturating_add(30 * 60 * 1000),
                                scope: found.scope,
                                truncated_scope: found.truncated_scope,
                                notes: found.notes,
                                exclusions: found.exclusions,
                                total_notes: found.total_notes,
                                total_matches: found.total_matches,
                            };
                            let page = match self.find_page(&entry, 0, 0, 50) {
                                Ok(page) => page,
                                Err(message) => {
                                    self.pending_replace.lock_recovering().remove(&request_id);
                                    return self.record_completion(
                                        request_id,
                                        SessionResult::Error { message },
                                    );
                                }
                            };
                            let mut finders = self.finders.lock_recovering();
                            if finders.len() >= MAX_LIVE_FINDERS {
                                SessionResult::Busy
                            } else {
                                let inner = CommandResult::SourceFindStarted {
                                    finder_id,
                                    scope: entry.scope,
                                    truncated_scope: entry.truncated_scope,
                                    total_notes: entry.total_notes,
                                    total_matches: entry.total_matches,
                                    exclusions: entry.exclusions.clone(),
                                    page,
                                };
                                finders.insert(finder_id, entry);
                                SessionResult::Success(inner)
                            }
                        }
                        Err(error) => SessionResult::Error {
                            message: error.message(),
                        },
                    };
                    (request_id, result)
                }
                ReplaceOutcomeMessage::Preview {
                    request_id,
                    preview_id,
                    outcome,
                } => {
                    let result = match outcome {
                        Ok(manifest) => {
                            let manifest = *manifest;
                            let mut previews = self.previews.lock_recovering();
                            if previews.len() >= MAX_LIVE_PREVIEWS {
                                SessionResult::Busy
                            } else {
                                let entry = PreviewEntry {
                                    expires_at_unix_ms: manifest.expires_at_unix_ms,
                                    manifest: Arc::new(manifest),
                                    selected_ids: Vec::new(),
                                    selection_revision: 0,
                                };
                                let inner = CommandResult::ReplacePreviewSealed {
                                    preview_id,
                                    manifest: entry.manifest.clone(),
                                };
                                previews.insert(preview_id, entry);
                                SessionResult::Success(inner)
                            }
                        }
                        Err(error) => {
                            // Best-effort cleanup of any partially written
                            // payloads (no manifest was sealed on failure).
                            let store = crate::replace::ReplaceStore::new(&self.identity.root);
                            let io = crate::replace::ActorOpStoreIo::new(self.actor.clone());
                            let _ = store.remove_job(&io, &hex::encode(preview_id.to_be_bytes()));
                            SessionResult::Error {
                                message: error.message(),
                            }
                        }
                    };
                    (request_id, result)
                }
                ReplaceOutcomeMessage::ApplyDone {
                    request_id,
                    job_id,
                    phase,
                    totals,
                } => {
                    {
                        let mut jobs = self.replace_jobs_map.lock_recovering();
                        if let Some(entry) = jobs.get_mut(&job_id) {
                            let mut state = entry.state.lock_recovering();
                            state.phase = phase;
                            state.totals = totals.clone();
                        }
                    }
                    *self.active_apply.lock_recovering() = None;
                    (
                        request_id,
                        SessionResult::Success(CommandResult::ReplaceJobFinished {
                            job_id,
                            phase,
                            totals,
                        }),
                    )
                }
                ReplaceOutcomeMessage::ApplyFailed {
                    request_id,
                    job_id,
                    message,
                } => {
                    {
                        let mut jobs = self.replace_jobs_map.lock_recovering();
                        if let Some(entry) = jobs.get_mut(&job_id) {
                            entry.state.lock_recovering().phase =
                                crate::replace::JobPhase::NeedsRecovery;
                        }
                    }
                    *self.active_apply.lock_recovering() = None;
                    (request_id, SessionResult::Error { message })
                }
            };
            self.record_completion(request_id, result);
            self.pending_replace.lock_recovering().remove(&request_id);
        }
    }

    /// Install finished preparations and read-only finds into their
    /// destinations on a Session turn, completing every correlated caller
    /// once.
    fn pump_text_completions(&self) {
        while let Ok(message) = self.text_results.try_recv() {
            let (request_id, result) = match message {
                TextOutcomeMessage::Prepare {
                    request_id,
                    buffer_id,
                    revision,
                    outcome,
                } => {
                    let result = match outcome {
                        Ok((ops, match_count, digest)) => {
                            let plan_id = self.next_text_plan_id();
                            let entry = TextPlanEntry {
                                ops,
                                match_count,
                                options_digest: digest,
                                buffer_id,
                                revision,
                                created: std::time::Instant::now(),
                                consumed: None,
                            };
                            let admitted = {
                                let mut plans = self.text_plans.lock_recovering();
                                if !self.text_plans_full_locked(&plans) {
                                    plans.insert(plan_id, entry);
                                    true
                                } else {
                                    false
                                }
                            };
                            if admitted {
                                SessionResult::Success(CommandResult::ReplacePrepared {
                                    plan_id,
                                    match_count,
                                    expires_in_ms: u64::try_from(TEXT_PLAN_TTL.as_millis())
                                        .unwrap_or(u64::MAX),
                                })
                            } else {
                                SessionResult::Busy
                            }
                        }
                        // Typed, redacted refusal: never `{:?}` — a regex
                        // parse error's Debug text can quote pattern
                        // fragments (NFR-17/§6.7).
                        Err(error) => SessionResult::Error {
                            message: error.message(),
                        },
                    };
                    (request_id, result)
                }
                TextOutcomeMessage::Find {
                    request_id,
                    buffer_id,
                    revision,
                    outcome,
                } => {
                    let result = match outcome {
                        Ok(found) => {
                            let spans = found
                                .spans
                                .into_iter()
                                .map(|(start, end)| {
                                    (
                                        u64::try_from(start).unwrap_or(u64::MAX),
                                        u64::try_from(end).unwrap_or(u64::MAX),
                                    )
                                })
                                .collect();
                            SessionResult::Success(CommandResult::Found {
                                buffer_id,
                                revision,
                                spans,
                                offset: found.offset,
                                total_matches: found.total_matches,
                                next_offset: found.next_offset,
                                truncated: found.truncated,
                            })
                        }
                        Err(error) => SessionResult::Error {
                            message: error.message(),
                        },
                    };
                    (request_id, result)
                }
            };
            self.pending_text.lock_recovering().remove(&request_id);
            self.record_completion(request_id, result);
        }
    }

    /// Plan-cap check against an already-held lock (the pump must not
    /// re-take the mutex it holds).
    fn text_plans_full_locked(
        &self,
        plans: &std::collections::HashMap<u128, TextPlanEntry>,
    ) -> bool {
        plans
            .values()
            .filter(|entry| entry.consumed.is_none())
            .count()
            >= MAX_UNCONSUMED_TEXT_PLANS
    }

    fn pump_open_completions(&self) {
        let mut ready = Vec::new();
        while let Ok(read) = self.open_results.try_recv() {
            ready.push(read);
        }
        if ready.is_empty() {
            return;
        }
        for read in ready {
            let Some(flight) = self.open_flights.lock_recovering().remove(&read.path) else {
                continue;
            };
            let result = self.install_open_read(read);
            // The former code held the completions lock across the open-flights
            // removal and the install; every lock now lives inside its owner
            // (S6: completions is only ever touched by `record_completion`).
            for request_id in flight.request_ids {
                self.record_completion(request_id, result.clone());
            }
        }
    }

    /// Close a note.
    /// Create a new empty note file and open it (W10 explicit creation).
    ///
    /// The flight machinery of [`Self::open_note`] is reused with
    /// [`OpenPolicy::CreateNew`]: the collision check and the empty-file
    /// write run on an open worker and the vault actor, never under the
    /// driving turn. Callers correlate the terminal result through the
    /// returned request id.
    fn create_note(&self, path: PathBuf) -> SessionResult {
        if self
            .buffer_manager
            .lock_recovering()
            .get_buffer(&path)
            .is_some()
        {
            return SessionResult::Error {
                message: format!("note already exists: {}", path.display()),
            };
        }
        // Path validation: the note must live inside this vault and carry
        // no traversal or separator tricks. Absolute paths within the root
        // are canonicalized to vault-relative, matching `note.open`.
        let relative = match path.strip_prefix(&self.identity.root) {
            Ok(relative) => relative.to_path_buf(),
            Err(_) => {
                return SessionResult::Error {
                    message: "note path is outside this vault".into(),
                };
            }
        };
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return SessionResult::Error {
                message: "Invalid vault path".into(),
            };
        }
        // The buffer key keeps the caller's exact path form (absolute),
        // matching `note.open`.
        self.open_note_inner(path, OpenPolicy::CreateNew)
    }

    /// The one core close policy (W12 consolidation): a pending save
    /// flight refuses every close; a dirty buffer refuses a plain close
    /// but may close while retaining a durable recovery snapshot that
    /// provably covers its current revision and bytes.
    fn close_note(&self, path: PathBuf, retain_recovery: bool) -> SessionResult {
        // Closing a leased draft without keeping its recovery would destroy
        // the evidence a running job pinned (D-10); a retained close stays
        // available for inspection paths.
        if !retain_recovery && self.leased_by_replace(&path) {
            return self.replacement_in_progress();
        }
        // Get buffer ID
        let buffer_id = {
            self.buffer_manager
                .lock_recovering()
                .get_buffer(&path)
                .map(|buffer| buffer.buffer_id())
        };

        if let Some(_buffer_id) = buffer_id {
            if self
                .save_flights
                .lock_recovering()
                .contains_key(&_buffer_id.as_u64())
            {
                return SessionResult::ActionRequired {
                    code: "save-pending",
                    data: Some(path.display().to_string()),
                };
            }
            let unsaved = self
                .buffer_manager
                .lock_recovering()
                .get_buffer_by_id(_buffer_id)
                .is_some_and(|buffer| buffer.is_dirty() || buffer.needs_resolution());
            if unsaved {
                if !retain_recovery {
                    return SessionResult::ActionRequired {
                        code: "dirty-buffer-close",
                        data: Some(path.display().to_string()),
                    };
                }
                // The receipt must provably cover the buffer being dropped:
                // durable snapshot, revision-bound, hash-verified.
                if let Err(message) = self.flush_recovery_note(&path) {
                    return SessionResult::Error { message };
                }
            }
            // Close the buffer. The recovery file is intentionally not
            // deleted here: retain-recovery means the snapshot outlives
            // the buffer and is offered again on reopen.
            let mut buffer_manager = self.buffer_manager.lock_recovering();
            buffer_manager.close_note(&path);
            drop(buffer_manager);
            self.shared_snapshots.lock_recovering().remove(&_buffer_id);

            self.absent_on_open.lock_recovering().remove(&path);
            // The buffer's journal memo dies with the buffer (C1): a
            // re-opened buffer (new id) starts cold, and the close tier
            // keeps total memo memory bounded by open buffers.
            self.journal_memos.lock_recovering().remove(&_buffer_id);

            SessionResult::Success(CommandResult::NoteClosed)
        } else {
            SessionResult::Error {
                message: format!("Buffer not found: {}", path.display()),
            }
        }
    }

    /// Durably checkpoint one buffer's recovery snapshot and verify the
    /// receipt covers the current revision and content (W12). Returns the
    /// revision-bound receipt.
    fn flush_recovery_note(&self, path: &PathBuf) -> Result<CommandResult, String> {
        let (key, base, snapshot, revision) = {
            let manager = self.buffer_manager.lock_recovering();
            let Some(buffer) = manager.get_buffer(path) else {
                return Err(format!("Buffer not found: {}", path.display()));
            };
            if !buffer.is_dirty() && !buffer.needs_resolution() {
                return Err(format!(
                    "Buffer has no unsaved changes to retain: {}",
                    path.display()
                ));
            }
            (
                native_note_key(path, &self.identity.root),
                buffer.base_bytes().to_vec(),
                flavite_buffer::recovery::RecoveryFile::from_buffer(buffer, 0, false),
                buffer.revision(),
            )
        };
        let Some(writer) = &self.recovery_writer else {
            return Err("local recovery storage is unavailable".into());
        };
        self.actor
            .pin_merge_base(key.clone(), base)
            .map_err(|error| format!("could not persist recovery merge base: {error}"))?;
        // `None` means an equally-covering snapshot is already durable;
        // the read-back verification below is the receipt check either way.
        let _ = writer.lock_recovering().write_snapshot_now(&key, snapshot);
        let stored = writer
            .lock_recovering()
            .read_for_note_key(&key)
            .ok_or_else(|| "no durable recovery snapshot exists for this buffer".to_owned())?;
        if stored.header.revision < revision {
            return Err(format!(
                "durable recovery snapshot covers revision {} but the buffer is at {}",
                stored.header.revision.as_u64(),
                revision.as_u64()
            ));
        }
        let hash = {
            let manager = self.buffer_manager.lock_recovering();
            manager
                .get_buffer(path)
                .map(|buffer| buffer.content_hash())
                .ok_or_else(|| format!("Buffer not found: {}", path.display()))?
        };
        if !stored.matches_hash(hash) {
            return Err("durable recovery snapshot does not match the buffer content".into());
        }
        Ok(CommandResult::RecoveryFlushed {
            buffer_id: stored.header.buffer_id,
            revision: stored.header.revision,
            hash: stored.content_hash,
        })
    }

    /// Explicit discard (W12): snapshot the unsaved edits into local
    /// history, drop the retained recovery copy of the same edits, and
    /// reload the buffer from disk as one guarded destructive action.
    fn discard_edits(&self, path: PathBuf) -> SessionResult {
        if self.leased_by_replace(&path) {
            return self.replacement_in_progress();
        }
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "Buffer not found".into(),
            };
        };
        if self
            .save_flights
            .lock_recovering()
            .contains_key(&buffer_id.as_u64())
        {
            return SessionResult::ActionRequired {
                code: "save-pending",
                data: Some(path.display().to_string()),
            };
        }
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        let mine = {
            let manager = self.buffer_manager.lock_recovering();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => history_bytes(buffer),
                None => {
                    return SessionResult::Error {
                        message: "Buffer not found".into(),
                    };
                }
            }
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            mine,
            crate::history::HistoryReason::ConflictResolution,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot discarded bytes: {error}"),
            };
        }
        {
            let mut manager = self.buffer_manager.lock_recovering();
            match manager
                .get_buffer_by_id_mut(buffer_id)
                .and_then(|buffer| buffer.reload_from_disk(&theirs, DecodePolicy::Auto))
            {
                Some(_) => {}
                None => {
                    // Malformed/undecodable disk bytes are never adopted
                    // silently; the buffer is untouched and the repair is an
                    // explicit encoding choice (§6.10).
                    return self.encoding_choice_required(
                        &path,
                        &ChoiceReason::InvalidInput {
                            kind: crate::encoding::InvalidKind::Other,
                            observed_raw_hash: *blake3::hash(&theirs).as_bytes(),
                        },
                    );
                }
            }
        }
        // The retained recovery copy holds the same discarded edits; keep
        // them from re-appearing as a recovery offer on the next open.
        if let Some(writer) = &self.recovery_writer {
            let key = native_note_key(&path, &self.identity.root);
            writer.lock_recovering().delete_for_note_key(&key);
        }
        SessionResult::Success(CommandResult::EditsDiscarded { buffer_id })
    }

    /// Apply edits to a buffer.
    pub(crate) fn apply_structure_patch(
        &self,
        buffer_id: BufferId,
        revision: Revision,
        op: EditOp,
    ) -> SessionResult {
        let mut manager = self.buffer_manager.lock_recovering();
        let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
            return SessionResult::Error {
                message: "Buffer no longer open".into(),
            };
        };
        if buffer.revision() != revision {
            return SessionResult::StaleRevision {
                buffer_id,
                expected: revision,
                actual: buffer.revision(),
            };
        }
        if !matches!(
            buffer.state(),
            flavite_buffer::state::BufferState::Clean | flavite_buffer::state::BufferState::Dirty
        ) {
            return SessionResult::ActionRequired {
                code: "structure.state",
                data: None,
            };
        }
        buffer.model_mut().seal_undo_group();
        let result = buffer.apply_local_edits(&[op]);
        buffer.model_mut().seal_undo_group();
        match result {
            Some((new_revision, _)) => SessionResult::Success(CommandResult::EditsApplied {
                buffer_id,
                new_revision,
            }),
            None => SessionResult::Error {
                message: "Invalid structure edit".into(),
            },
        }
    }

    fn apply_edits(
        &self,
        buffer_id: BufferId,
        revision: Revision,
        ops: Vec<EditOp>,
        undo_policy: flavite_buffer::undo::UndoPolicy,
    ) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock_recovering();
        if let Some(path) = buffer_manager.path_for_id(buffer_id)
            && self.leased_by_replace(path)
        {
            return SessionResult::ActionRequired {
                code: "replacement-in-progress",
                data: Some(path.display().to_string()),
            };
        }

        // Get the buffer and apply all edit ops
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            // Check revision first
            if buffer.revision() != revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }

            // Validate the command once and apply all positions as one
            // authoritative Buffer transition/undo unit. `Isolated` (the
            // default) prepares and installs one boundary candidate; the
            // explicit typing path keeps ordinary coalescing (D-06/TD-8).
            let Some((new_revision, _)) = buffer.apply_local_edits_with_policy(&ops, undo_policy)
            else {
                return SessionResult::Error {
                    message: "edit ranges overlap, are out of bounds, or split UTF-8".into(),
                };
            };

            SessionResult::Success(CommandResult::EditsApplied {
                buffer_id,
                new_revision,
            })
        } else {
            SessionResult::Error {
                message: format!("Buffer not found: {:?}", buffer_id),
            }
        }
    }

    /// Save a note.
    fn save_note(&self, path: PathBuf) -> SessionResult {
        if self.leased_by_replace(&path) {
            return SessionResult::ActionRequired {
                code: "replacement-in-progress",
                data: Some(path.display().to_string()),
            };
        }
        let buffer_id = {
            self.buffer_manager
                .lock_recovering()
                .get_buffer(&path)
                .map(|buffer| buffer.buffer_id())
        };
        let Some(buffer_id) = buffer_id else {
            return SessionResult::Error {
                message: "Buffer not found".to_string(),
            };
        };
        let (
            bytes,
            revision,
            base_hash,
            base_bytes,
            state,
            source_format,
            leading_signature,
            nul_content,
        ) = {
            let mut manager = self.buffer_manager.lock_recovering();
            let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
                return SessionResult::Error {
                    message: "Buffer not found".into(),
                };
            };
            buffer.model_mut().seal_undo_group();
            // Strict descriptor serialization before any stage exists: a
            // strict-ASCII draft holding non-ASCII content fails here with
            // zero filesystem effects, the draft stays intact/recoverable,
            // and autosave pauses with one coalesced actionable error
            // (§6.10, FR-22). Explicit conversion clears the condition.
            let bytes = match buffer.model().try_to_source_bytes() {
                Ok(bytes) => bytes,
                Err(error) => {
                    let unencodable =
                        matches!(error, flavite_encoding::CodecError::Unencodable { .. });
                    let code = if unencodable {
                        crate::encoding::UNENCODABLE_CODE
                    } else {
                        crate::encoding::OUTPUT_LIMIT_CODE
                    };
                    self.notice.send(crate::channels::Notice::ActionRequired {
                        code,
                        note: Some(flavite_buffer::manager::NoteRef::new(path.clone())),
                    });
                    return SessionResult::ActionRequired {
                        code,
                        data: Some(crate::encoding::codec_limit_kind_code(&error).to_owned()),
                    };
                }
            };
            // The FEFF/NUL probes are computed under the lock from the
            // borrowed text: an owned copy of a 32 MiB note is not needed
            // for two boolean answers.
            let leading_signature = buffer.text().starts_with('\u{FEFF}');
            let nul_content = buffer.text().contains('\0');
            (
                bytes,
                buffer.revision(),
                buffer.base_hash(),
                buffer.base_bytes().to_vec(),
                buffer.state(),
                buffer.model().source_format(),
                leading_signature,
                nul_content,
            )
        };
        if !state.allows_manual_save() {
            return SessionResult::Conflict { path };
        }
        let precondition = if self.absent_on_open.lock_recovering().contains(&path) {
            DiskPrecondition::Absent
        } else if let Some(hash) = base_hash {
            DiskPrecondition::Existing(hash.as_bytes())
        } else {
            return SessionResult::Error {
                message: "buffer has no disk base; explicit create resolution is required".into(),
            };
        };
        let request_id = self.next_request_id();
        let note_key = native_note_key(&path, &self.identity.root);
        // §6.10: persist the format-binding intent BEFORE a source effect
        // whose interpretation depends on hint metadata (BOM-less UTF-16,
        // explicit ASCII, leading signature-shaped content, NUL-bearing
        // content). The intent cannot be persisted -> the save is refused
        // typed: an ambiguous source is never advertised as durably
        // configured, and a green Saved receipt always implies a durably
        // reachable descriptor.
        let needs_hint = source_format.encoding() != flavite_encoding::FileEncoding::Utf8
            || source_format.bom()
            || leading_signature
            || nul_content;
        if needs_hint {
            let hex_key = hex::encode(&note_key);
            let intent = crate::encoding::BindingIntent {
                native_key: hex_key.clone(),
                old_raw_hash: base_hash.map(|hash| hash.as_bytes()).unwrap_or([0u8; 32]),
                output_raw_hash: *blake3::hash(&bytes).as_bytes(),
                encoding: source_format.encoding(),
                bom: source_format.bom(),
                preserve_leading_signature: leading_signature && !source_format.bom(),
                nul_content_confirmed: nul_content,
            };
            if let Err(error) = self.encoding_hints.update(|record| {
                record.intents.insert(hex_key, intent);
            }) {
                tracing::warn!(path = %path.display(), ?error, "format-binding intent refused");
                return SessionResult::ActionRequired {
                    code: crate::encoding::HINT_UNAVAILABLE_CODE,
                    data: None,
                };
            }
        }
        let request = SaveRequest {
            request_id,
            path,
            note_key,
            bytes,
            merge_base: base_bytes,
            precondition,
            buffer_id: buffer_id.as_u64(),
            revision: revision.as_u64(),
        };

        let mut flights = self.save_flights.lock_recovering();
        let outstanding = self.completions.lock_recovering().len()
            + flights
                .values()
                .map(|flight| 1 + usize::from(flight.queued.is_some()))
                .sum::<usize>();
        if outstanding + self.reconcile_flights.lock_recovering().len()
            >= MAX_PENDING_SAVE_OPERATIONS
        {
            return SessionResult::Busy;
        }
        if let Some(flight) = flights.get_mut(&buffer_id.as_u64()) {
            if let Some(superseded) = flight.queued.replace(request) {
                self.record_completion(
                    superseded.request_id,
                    SessionResult::Superseded {
                        request_id: superseded.request_id,
                        by_request: request_id,
                    },
                );
            }
            return SessionResult::Pending { request_id };
        }
        match self.actor.submit_save(request.clone()) {
            Ok(response) => {
                flights.insert(
                    buffer_id.as_u64(),
                    SaveFlight {
                        request,
                        response,
                        queued: None,
                    },
                );
                SessionResult::Pending { request_id }
            }
            Err(error) => SessionResult::Error {
                message: error.to_string(),
            },
        }
    }

    /// Trigger a reconcile.
    fn reconcile(&self, scope: ReconcileScope, mode: ReconcileMode) -> SessionResult {
        let request_id = self.next_request_id();
        let flights = self.save_flights.lock_recovering();
        let outstanding = self.completions.lock_recovering().len()
            + flights
                .values()
                .map(|flight| 1 + usize::from(flight.queued.is_some()))
                .sum::<usize>()
            + self.reconcile_flights.lock_recovering().len();
        drop(flights);
        if outstanding >= MAX_PENDING_SAVE_OPERATIONS {
            return SessionResult::Busy;
        }
        match self.actor.submit_reconcile(scope, mode) {
            Ok(response) => {
                self.reconcile_flights
                    .lock_recovering()
                    .insert(request_id, response);
                SessionResult::Pending { request_id }
            }
            Err(crate::actor::VaultActorError::Busy) => SessionResult::Busy,
            Err(error) => SessionResult::Error {
                message: error.to_string(),
            },
        }
    }

    fn rename_note(&self, from: PathBuf, to: PathBuf, strategy: RenameStrategy) -> SessionResult {
        if self.leased_by_replace(&from) || self.leased_by_replace(&to) {
            return SessionResult::ActionRequired {
                code: "replacement-in-progress",
                data: Some(from.display().to_string()),
            };
        }
        let from = if from.is_absolute() {
            from
        } else {
            self.identity.root.join(from)
        };
        let to = if to.is_absolute() {
            to
        } else {
            self.identity.root.join(to)
        };
        if self.buffer_manager.lock_recovering().has_buffer(&to) {
            return SessionResult::Error {
                message: format!("destination-open: {}", to.display()),
            };
        }
        let (buffer_ids, buffer_revisions) = {
            let manager = self.buffer_manager.lock_recovering();
            let mut buffer_ids = HashMap::new();
            let mut buffer_revisions = HashMap::new();
            for (path, buffer_id) in manager.open_paths() {
                if let Ok(relative) = path.strip_prefix(&self.identity.root) {
                    buffer_ids.insert(relative.to_path_buf(), buffer_id.as_u64());
                    if let Some(buffer) = manager.get_buffer_by_id(buffer_id) {
                        buffer_revisions.insert(buffer_id.as_u64(), buffer.revision().as_u64());
                    }
                }
            }
            (buffer_ids, buffer_revisions)
        };
        let outstanding = self.completions.lock_recovering().len()
            + self.save_flights.lock_recovering().len()
            + self.reconcile_flights.lock_recovering().len()
            + self.rename_flights.lock_recovering().len();
        if outstanding >= MAX_PENDING_SAVE_OPERATIONS {
            return SessionResult::Busy;
        }
        let request_id = self.next_request_id();
        match self.actor.submit_rename(
            from.clone(),
            to.clone(),
            strategy,
            buffer_ids,
            buffer_revisions,
        ) {
            Ok(response) => {
                self.rename_flights
                    .lock_recovering()
                    .insert(request_id, (from, to, response));
                SessionResult::Pending { request_id }
            }
            Err(crate::actor::VaultActorError::Busy) => SessionResult::Busy,
            Err(error) => SessionResult::Error {
                message: error.to_string(),
            },
        }
    }

    /// Undo an edit.
    fn undo(&self, buffer_id: BufferId, revision: Revision) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock_recovering();
        if buffer_manager
            .path_for_id(buffer_id)
            .is_some_and(|path| self.leased_by_replace(path))
        {
            return self.replacement_in_progress();
        }

        // Get the buffer and call undo on its model
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            if buffer.revision() != revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }
            if let Some(new_revision) = buffer.model_mut().undo() {
                let tags = buffer
                    .model()
                    .last_transition_ops()
                    .iter()
                    .filter_map(|op| op.journal.clone())
                    .collect::<Vec<_>>();
                for tag in tags {
                    if let Err(message) = self
                        .actor
                        .cancel_journal_item(tag.txn_id.clone(), tag.item_id)
                    {
                        return SessionResult::Error { message };
                    }
                    self.memo_cancel(buffer_id, (tag.txn_id, tag.item_id));
                }
                SessionResult::Success(CommandResult::UndoPerformed {
                    buffer_id,
                    new_revision,
                })
            } else {
                SessionResult::Error {
                    message: "Nothing to undo".to_string(),
                }
            }
        } else {
            SessionResult::Error {
                message: "Buffer not found".to_string(),
            }
        }
    }

    /// Redo an edit.
    fn redo(&self, buffer_id: BufferId, revision: Revision) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock_recovering();
        if buffer_manager
            .path_for_id(buffer_id)
            .is_some_and(|path| self.leased_by_replace(path))
        {
            return self.replacement_in_progress();
        }

        // Get the buffer and call redo on its model
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            if buffer.revision() != revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }
            let cancelled_tags: Vec<(String, u64)> = buffer
                .model()
                .next_redo_ops()
                .iter()
                .filter_map(|op| op.journal.as_ref())
                .map(|tag| (tag.txn_id.clone(), tag.item_id))
                .collect();
            if self
                .journal_memos
                .lock_recovering()
                .get(&buffer_id)
                .is_some_and(|memo| {
                    cancelled_tags
                        .iter()
                        .any(|key| memo.cancelled.contains(key))
                })
            {
                return SessionResult::Error {
                    message: "redo-after-cancel".into(),
                };
            }
            if let Some(new_revision) = buffer.model_mut().redo() {
                SessionResult::Success(CommandResult::RedoPerformed {
                    buffer_id,
                    new_revision,
                })
            } else {
                SessionResult::Error {
                    message: "Nothing to redo".to_string(),
                }
            }
        } else {
            SessionResult::Error {
                message: "Buffer not found".to_string(),
            }
        }
    }

    /// Resolve a conflict.
    fn resolve_conflict(&self, path: PathBuf, resolution: ConflictResolution) -> SessionResult {
        match resolution {
            ConflictResolution::KeepMine => self.resolve_keep_mine(path),
            ConflictResolution::Reload => self.resolve_reload(path),
            ConflictResolution::SaveAs { new_path } => {
                // Save as new file
                SessionResult::ActionRequired {
                    code: "save-as-confirm",
                    data: Some(new_path.display().to_string()),
                }
            }
            ConflictResolution::Merge => self.prepare_merge(path),
        }
    }

    fn resolve_keep_mine(&self, path: PathBuf) -> SessionResult {
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "Buffer not found".into(),
            };
        };
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            theirs.clone(),
            crate::history::HistoryReason::ConflictResolution,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot conflict bytes: {error}"),
            };
        }
        let mut manager = self.buffer_manager.lock_recovering();
        match manager.get_buffer_by_id_mut(buffer_id).and_then(|buffer| {
            buffer
                .resolve_keep_local(&theirs, DecodePolicy::Auto)
                .map(|_| ())
        }) {
            Some(()) => SessionResult::Success(CommandResult::ConflictResolved),
            None => self.encoding_choice_required(
                &path,
                &ChoiceReason::InvalidInput {
                    kind: crate::encoding::InvalidKind::Other,
                    observed_raw_hash: *blake3::hash(&theirs).as_bytes(),
                },
            ),
        }
    }

    fn resolve_reload(&self, path: PathBuf) -> SessionResult {
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "Buffer not found".into(),
            };
        };
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        let mine = {
            let manager = self.buffer_manager.lock_recovering();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => history_bytes(buffer),
                None => {
                    return SessionResult::Error {
                        message: "Buffer not found".into(),
                    };
                }
            }
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            mine,
            crate::history::HistoryReason::ConflictResolution,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot discarded bytes: {error}"),
            };
        }
        let mut manager = self.buffer_manager.lock_recovering();
        match manager
            .get_buffer_by_id_mut(buffer_id)
            .and_then(|buffer| buffer.reload_from_disk(&theirs, DecodePolicy::Auto))
        {
            Some(_) => SessionResult::Success(CommandResult::ConflictResolved),
            None => self.encoding_choice_required(
                &path,
                &ChoiceReason::InvalidInput {
                    kind: crate::encoding::InvalidKind::Other,
                    observed_raw_hash: *blake3::hash(&theirs).as_bytes(),
                },
            ),
        }
    }

    fn prepare_merge(&self, path: PathBuf) -> SessionResult {
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "Buffer not found".into(),
            };
        };
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        let (base, mine, revision, base_format) = {
            let manager = self.buffer_manager.lock_recovering();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => (
                    buffer.base_bytes().to_vec(),
                    buffer.model().decoded_text().into_bytes(),
                    buffer.revision(),
                    buffer.base_format(),
                ),
                None => {
                    return SessionResult::Error {
                        message: "Buffer not found".into(),
                    };
                }
            }
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            theirs.clone(),
            crate::history::HistoryReason::Merge,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot merge input: {error}"),
            };
        }
        // §6.10: decode base/mine/theirs independently under their own
        // captured descriptors, then merge the decoded layers (physical EOLs
        // preserved, no BOM). A decode failure is a typed choice request,
        // never a partial merge or a lossy reinterpretation.
        let limits = flavite_encoding::DecodeLimits::default();
        // Owned inputs move into the decode map without a source copy, and
        // the decoded layer moves out without a text copy. The remote
        // layer's raw hash is taken before the move.
        let theirs_disk_hash = *blake3::hash(&theirs).as_bytes();
        let decode_layer = |bytes: Vec<u8>,
                            policy: flavite_encoding::DecodePolicy|
         -> Result<String, flavite_encoding::CodecError> {
            flavite_encoding::decode_vec(bytes, policy, &limits, &flavite_encoding::NOT_CANCELLED)
                .map(flavite_encoding::DecodedContent::into_text)
        };
        let base_policy = base_format
            .map(flavite_encoding::DecodePolicy::Explicit)
            .unwrap_or(flavite_encoding::DecodePolicy::Auto);
        let (Ok(base), Ok(mine), Ok(theirs_text)) = (
            decode_layer(base, base_policy),
            String::from_utf8(mine)
                .map_err(|_| flavite_encoding::CodecError::InvalidUtf8 { source_byte: 0 }),
            decode_layer(theirs, flavite_encoding::DecodePolicy::Auto),
        ) else {
            return SessionResult::ActionRequired {
                code: "merge-unsupported-encoding",
                data: Some(path.display().to_string()),
            };
        };
        SessionResult::MergeReady {
            path,
            buffer_id,
            revision,
            disk_hash: theirs_disk_hash,
            result: crate::merge::three_way_merge(&base, &mine, &theirs_text),
        }
    }

    fn apply_merge(
        &self,
        path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        expected_disk_hash: [u8; 32],
        merged_bytes: Vec<u8>,
    ) -> SessionResult {
        if self.leased_by_replace(&path) {
            return self.replacement_in_progress();
        }
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        if blake3::hash(&theirs).as_bytes() != &expected_disk_hash {
            if let Some(buffer) = self
                .buffer_manager
                .lock_recovering()
                .get_buffer_by_id_mut(buffer_id)
            {
                buffer.mark_conflicted_disk_hash(*blake3::hash(&theirs).as_bytes());
            }
            return SessionResult::Conflict { path };
        }
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            theirs.clone(),
            crate::history::HistoryReason::Merge,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot merge disk input: {error}"),
            };
        }
        let mine = {
            let manager = self.buffer_manager.lock_recovering();
            let Some(buffer) = manager.get_buffer_by_id(buffer_id) else {
                return SessionResult::Error {
                    message: "merge preview buffer was closed".into(),
                };
            };
            if buffer.revision() != revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }
            history_bytes(buffer)
        };
        if let Err(error) = self.actor.snapshot_history(
            native_note_key(&path, &self.identity.root),
            mine,
            crate::history::HistoryReason::ConflictResolution,
        ) {
            return SessionResult::Error {
                message: format!("could not snapshot merge buffer: {error}"),
            };
        }
        let mut manager = self.buffer_manager.lock_recovering();
        let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
            return SessionResult::Error {
                message: "merge preview buffer was closed".into(),
            };
        };
        if buffer.revision() != revision {
            return SessionResult::StaleRevision {
                buffer_id,
                expected: revision,
                actual: buffer.revision(),
            };
        }
        match buffer.apply_resolved_merge(&merged_bytes, &theirs, DecodePolicy::Auto) {
            Some(new_revision) => SessionResult::Success(CommandResult::EditsApplied {
                buffer_id,
                new_revision,
            }),
            None => SessionResult::Error {
                message: "merge output is invalid UTF-8 or exceeds the note size limit".into(),
            },
        }
    }

    /// Reopen one note under an explicit source-format interpretation
    /// (§6.10). Guards: exact current raw hash, no pending save, and — for
    /// an open buffer — no unsaved text. A clean reopen reinterprets the
    /// shared buffer (all views observe the new revision/descriptor), starts
    /// a fresh Undo root, and binds the confirmed hint to the observed hash.
    fn reopen_encoding(
        &self,
        path: PathBuf,
        expected_raw_hash: [u8; 32],
        encoding: flavite_encoding::FileEncoding,
        bom: crate::encoding::BomMode,
    ) -> SessionResult {
        let Some(bytes) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        let observed = *blake3::hash(&bytes).as_bytes();
        if observed != expected_raw_hash {
            // A stale precondition is a re-read, never a decode under bytes
            // the caller never saw.
            return SessionResult::ActionRequired {
                code: crate::encoding::STALE_SOURCE_CODE,
                data: None,
            };
        }
        // Resolve the concrete descriptor from the family + BOM intent
        // against the actual bytes (§6.10): `Auto` strips a matching
        // signature for the requested family, `Present` requires it,
        // `Absent` preserves signature-shaped leading bytes as content.
        let signature: &[u8] = encoding.signature();
        let format = match bom {
            crate::encoding::BomMode::Auto
                if !signature.is_empty() && bytes.starts_with(signature) =>
            {
                flavite_encoding::SourceFormat::new(encoding, true)
            }
            crate::encoding::BomMode::Present => {
                flavite_encoding::SourceFormat::new(encoding, true)
            }
            _ => flavite_encoding::SourceFormat::new(encoding, false),
        };
        let format = match format {
            Ok(format) => format,
            // ASCII+BOM is an invalid descriptor combination (§6.10).
            Err(_) => {
                return SessionResult::Error {
                    message: "ascii cannot be combined with a BOM".into(),
                };
            }
        };
        // Decode once under the explicit descriptor before touching state.
        let snapshot = match flavite_syntax::text::TextSnapshot::with_policy(
            &bytes,
            DecodePolicy::Explicit(format),
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let reason = match &error {
                    flavite_syntax::text::TextError::Codec(
                        flavite_encoding::CodecError::BomConflict,
                    ) => ChoiceReason::BomConflict {
                        observed_raw_hash: observed,
                    },
                    flavite_syntax::text::TextError::Codec(
                        flavite_encoding::CodecError::UnsupportedOrAmbiguousBom,
                    ) => ChoiceReason::UnsupportedOrAmbiguousBom {
                        observed_raw_hash: observed,
                    },
                    flavite_syntax::text::TextError::Codec(codec) => ChoiceReason::InvalidInput {
                        kind: crate::encoding::InvalidKind::from_codec(codec),
                        observed_raw_hash: observed,
                    },
                };
                return self.encoding_choice_required(&path, &reason);
            }
        };
        let (buffer_id, revision) = {
            let mut manager = self.buffer_manager.lock_recovering();
            match manager.get_buffer(&path).map(|buffer| buffer.buffer_id()) {
                Some(buffer_id) => {
                    if self
                        .save_flights
                        .lock_recovering()
                        .contains_key(&buffer_id.as_u64())
                    {
                        return SessionResult::ActionRequired {
                            code: crate::encoding::SAVE_PENDING_CODE,
                            data: None,
                        };
                    }
                    let is_dirty = manager
                        .get_buffer(&path)
                        .is_some_and(flavite_buffer::state::Buffer::is_dirty);
                    if is_dirty {
                        // DirtyBufferActionRequired: save with the current
                        // format, convert-then-save, or cancel — never an
                        // implicit discard of unsaved text (§6.10).
                        return SessionResult::ActionRequired {
                            code: crate::encoding::DIRTY_BUFFER_CODE,
                            data: None,
                        };
                    }
                    manager
                        .get_buffer_mut(&path)
                        .expect("buffer presence just checked")
                        .reopen_with_snapshot(snapshot, &bytes);
                    let buffer = manager.get_buffer(&path).expect("buffer present");
                    (buffer.buffer_id(), buffer.revision())
                }
                None => {
                    let (buffer, _transition) = manager.open_note_snapshot(path.clone(), snapshot);
                    (buffer.buffer_id(), buffer.revision())
                }
            }
        };
        self.note_buffer_transition();
        self.bind_save_hint(&path, format, observed, &bytes);
        let _ = self.pulse.send(crate::channels::Pulse::Invalidate {
            note: flavite_buffer::manager::NoteRef::new(path),
        });
        SessionResult::Success(CommandResult::EncodingReopened {
            buffer_id,
            revision,
        })
    }

    /// Convert an open buffer's save format (§6.10): validates whole-note
    /// representability/output limits before installing any metadata, keeps
    /// the decoded text and Undo lineage, and records one isolated boundary.
    fn convert_encoding(
        &self,
        buffer_id: BufferId,
        revision: Revision,
        target: flavite_encoding::SourceFormat,
    ) -> SessionResult {
        let mut manager = self.buffer_manager.lock_recovering();
        if manager
            .path_for_id(buffer_id)
            .is_some_and(|path| self.leased_by_replace(path))
        {
            return self.replacement_in_progress();
        }
        let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
            return SessionResult::Error {
                message: "Buffer no longer open".into(),
            };
        };
        if buffer.revision() != revision {
            return SessionResult::StaleRevision {
                buffer_id,
                expected: revision,
                actual: buffer.revision(),
            };
        }
        match buffer.model_mut().try_convert_source_format(target) {
            Ok(new_revision) => SessionResult::Success(CommandResult::EncodingConverted {
                buffer_id,
                new_revision,
            }),
            Err(error) => {
                let code = if matches!(error, flavite_encoding::CodecError::Unencodable { .. }) {
                    crate::encoding::UNENCODABLE_CODE
                } else {
                    crate::encoding::OUTPUT_LIMIT_CODE
                };
                SessionResult::ActionRequired {
                    code,
                    data: Some(crate::encoding::codec_limit_kind_code(&error).to_owned()),
                }
            }
        }
    }

    /// Bind the confirmed save-format hint for one source when its
    /// interpretation depends on more than plain BOM-less UTF-8 (§6.10).
    /// Write failure is logged — the next open re-asks instead of silently
    /// advertising a durably configured choice.
    fn bind_save_hint(
        &self,
        path: &Path,
        format: flavite_encoding::SourceFormat,
        observed_raw_hash: [u8; 32],
        bytes: &[u8],
    ) {
        let leading_signature = bytes.starts_with("\u{FEFF}".as_bytes());
        let nul_content = bytes.contains(&0);
        let needs_hint = format.encoding() != flavite_encoding::FileEncoding::Utf8
            || format.bom()
            || leading_signature
            || nul_content;
        if !needs_hint {
            return;
        }
        match self.confirm_encoding_hint(
            path,
            format,
            observed_raw_hash,
            leading_signature,
            nul_content,
        ) {
            Ok(_) => {
                // The binding is durable: retire any pre-save intent for
                // this source (§6.10 "retire the intent only after that
                // metadata is durable").
                let native_key = hex::encode(native_note_key(path, &self.identity.root));
                let _ = self.encoding_hints.update(|record| {
                    record.intents.remove(&native_key);
                });
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), ?error, "encoding hint binding failed");
            }
        }
    }

    fn restore_recovery(&self, path: PathBuf) -> SessionResult {
        let Some(writer) = &self.recovery_writer else {
            return SessionResult::Error {
                message: "recovery storage is unavailable".into(),
            };
        };
        let key = native_note_key(&path, &self.identity.root);
        let Some(recovery) = writer.lock_recovering().read_for_note_key(&key) else {
            return SessionResult::Error {
                message: "no valid recovery snapshot exists for this note".into(),
            };
        };
        let disk = match self.actor.read_file(path.clone()) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => Vec::new(),
            Err(error) => {
                return SessionResult::Error {
                    message: error.to_string(),
                };
            }
        };
        if !self.is_open(&path) {
            match self.open_note(path.clone(), OpenPolicy::CreateIfMissing) {
                SessionResult::Success(_) => {}
                SessionResult::Pending { request_id } => {
                    match self.wait_for_operation_holding_turn(request_id) {
                        SessionResult::Success(_) => {}
                        other => return other,
                    }
                }
                other => return other,
            }
        }
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "buffer disappeared while restoring recovery".into(),
            };
        };
        let recovered = match recovery.to_source_bytes() {
            Ok(bytes) => bytes,
            // The recorded draft is not representable under its own
            // descriptor; refusing is honest, the draft stays recoverable.
            Err(error) => {
                return self.encoding_choice_required(
                    &path,
                    &ChoiceReason::InvalidInput {
                        kind: crate::encoding::InvalidKind::from_codec(&error),
                        observed_raw_hash: [0u8; 32],
                    },
                );
            }
        };
        let transition = self
            .buffer_manager
            .lock_recovering()
            .get_buffer_by_id_mut(buffer_id)
            .and_then(|buffer| {
                buffer.restore_recovery(
                    &recovered,
                    DecodePolicy::Explicit(recovery.source_format),
                    &disk,
                    recovery.base_hash,
                    DecodePolicy::Auto,
                )
            });
        if transition.is_none() {
            return SessionResult::Error {
                message: "recovery snapshot could not be applied".into(),
            };
        }
        if recovery.base_hash.is_some_and(|base| {
            base != flavite_buffer::revision::BaseHash::from_bytes(*blake3::hash(&disk).as_bytes())
        }) {
            self.notice.send(crate::channels::Notice::Conflict {
                note: flavite_buffer::manager::NoteRef::new(path.clone()),
            });
        }
        let revision = self
            .buffer_manager
            .lock_recovering()
            .get_buffer_by_id(buffer_id)
            .map(|buffer| buffer.revision())
            .unwrap_or(Revision::INITIAL);
        SessionResult::Success(CommandResult::RecoveryRestored {
            buffer_id,
            revision,
        })
    }

    fn discard_recovery(&self, path: PathBuf) -> SessionResult {
        let Some(writer) = &self.recovery_writer else {
            return SessionResult::Error {
                message: "recovery storage is unavailable".into(),
            };
        };
        let key = native_note_key(&path, &self.identity.root);
        if writer.lock_recovering().delete_for_note_key(&key) {
            SessionResult::Success(CommandResult::ConflictResolved)
        } else {
            SessionResult::Error {
                message: "recovery snapshot could not be removed".into(),
            }
        }
    }

    /// Replace an open buffer's content with a local-history snapshot (W10).
    ///
    /// The replacement is one guarded, undoable buffer edit: revision is
    /// checked, disk is never touched here, and the user can undo. The note
    /// must already be open; this command never opens notes implicitly, so
    /// it cannot replay the recovery path's not-open branch.
    fn restore_history(
        &self,
        path: PathBuf,
        buffer_id: BufferId,
        revision: Revision,
        hash: String,
    ) -> SessionResult {
        if self.leased_by_replace(&path) {
            return self.replacement_in_progress();
        }
        // The buffer for `path` must be the exact buffer the caller saw:
        // restoring into a buffer belonging to another note is refused.
        {
            let manager = self.buffer_manager.lock_recovering();
            let path_buffer = manager.get_buffer(&path).map(|buffer| buffer.buffer_id());
            if path_buffer != Some(buffer_id) {
                return SessionResult::Error {
                    message: "buffer not found; open the note before restoring history".into(),
                };
            }
        }
        let snapshot = match self
            .actor
            .read_history_snapshot(hash, crate::history::MAX_HISTORY_READ_BYTES)
        {
            Ok(bytes) => bytes,
            Err(message) => return SessionResult::Error { message },
        };
        // §6.10: history blobs are raw serialized source bytes with their
        // recorded provenance. Decode strictly under Auto — UTF-8 and BOM'd
        // UTF-16 decode; anything ambiguous is a typed choice request, never
        // a lossy reinterpretation. The BOM is descriptor-owned and is not
        // re-inserted as content; restored lines inherit the buffer's EOL
        // policy through the normal edit path.
        let text = match flavite_syntax::text::TextSnapshot::new(&snapshot) {
            Ok(decoded) => decoded.text().to_owned(),
            Err(error) => {
                return self.encoding_choice_required(
                    &path,
                    &ChoiceReason::InvalidInput {
                        kind: match error {
                            flavite_syntax::text::TextError::Codec(codec) => {
                                crate::encoding::InvalidKind::from_codec(&codec)
                            }
                        },
                        observed_raw_hash: *blake3::hash(&snapshot).as_bytes(),
                    },
                );
            }
        };
        let mut manager = self.buffer_manager.lock_recovering();
        let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
            return SessionResult::Error {
                message: "Buffer no longer open".into(),
            };
        };
        if buffer.revision() != revision {
            return SessionResult::StaleRevision {
                buffer_id,
                expected: revision,
                actual: buffer.revision(),
            };
        }
        if !matches!(
            buffer.state(),
            flavite_buffer::state::BufferState::Clean | flavite_buffer::state::BufferState::Dirty
        ) {
            return SessionResult::ActionRequired {
                code: "history.state",
                data: None,
            };
        }
        // One undo unit: the full-content replacement never merges with
        // surrounding typing. The range is normalized-text (TextByte)
        // coordinates; the replacement is normalized UTF-8.
        buffer.model_mut().seal_undo_group();
        let end = buffer.model().text().len();
        let mut op = EditOp::new(
            flavite_syntax::text::Span::new(
                flavite_syntax::text::TextByte(0),
                flavite_syntax::text::TextByte(end),
            ),
            text,
        );
        op.boundary = true;
        let result = buffer.apply_local_edits(&[op]);
        buffer.model_mut().seal_undo_group();
        match result {
            Some((new_revision, _)) => SessionResult::Success(CommandResult::HistoryRestored {
                buffer_id,
                new_revision,
            }),
            None => SessionResult::Error {
                message: "history snapshot could not be applied".into(),
            },
        }
    }

    /// Get an immutable snapshot of an open buffer.
    pub fn buffer_snapshot(
        &self,
        buffer_id: BufferId,
        revision: Option<Revision>,
    ) -> Result<BufferSnapshot, BufferSnapshotError> {
        let snapshot = self.shared_buffer_snapshot(buffer_id, revision)?;
        Ok(BufferSnapshot {
            buffer_id: snapshot.buffer_id,
            revision: snapshot.revision,
            text: snapshot.text.to_string(),
            base_hash: snapshot.base_hash,
            state: snapshot.state,
            path: snapshot.path,
        })
    }

    /// Worker-only decode through the exact vault filesystem identity codec.
    pub fn decode_graph_paths(&self, keys: Vec<Vec<u8>>) -> Result<Vec<PathBuf>, String> {
        self.actor.decode_graph_paths(keys)
    }

    /// RFC-0005 S5 (FR-3): the lock-free buffer-stamp version. Bumped at
    /// every transition that can change [`Self::graph_buffer_stamp`] output
    /// (revision, state, path, base hash, the open set). A consumer compares
    /// it against the value observed at its last rebuild: `==` proves no
    /// buffer transition happened in between, so a wake may skip the stamp
    /// read entirely; `!=` is conservative (one extra rebuild). Read without
    /// the manager lock and without an actor hop.
    #[must_use]
    pub fn graph_stamp_version(&self) -> u64 {
        self.graph_stamp_version
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// RFC-0005 S6: the buffer-transition watch handed to consumers of the
    /// wake channel — the graph service clones its receiver into the wake
    /// select (the trailing-edge debounce policy lives there, §4.4). One
    /// logical receiver may actively consume wakes at a time (a channel
    /// dispatches each wake to exactly one receiver); restart attempts hand
    /// the receiver along without overlap, and the graph service is the
    /// only consumer in production. Tests may take a spy watch (§S6 seam)
    /// — a stolen wake only delays a rebuild to the next wake or the
    /// self-verifying sweep; correctness never rides the channel.
    #[must_use]
    pub fn graph_buffer_watch(&self) -> BufferWatch {
        self.buffer_watch.clone()
    }

    /// Lightweight current buffer stamp for rejecting obsolete graph publications.
    /// No text copies, database access, or filesystem work.
    pub fn graph_buffer_stamp(&self) -> Vec<GraphBufferStamp> {
        let manager = self.buffer_manager.lock_recovering();
        let mut stamp: Vec<_> = manager
            .open_paths()
            .into_iter()
            .filter_map(|(path, id)| {
                manager.get_buffer_by_id(id).map(|buffer| {
                    (
                        id,
                        buffer.revision(),
                        path,
                        buffer.state(),
                        buffer.base_hash().map(|hash| hash.as_bytes()),
                    )
                })
            })
            .collect();
        stamp.sort_by_key(|(id, ..)| id.as_u64());
        stamp
    }

    /// Worker-only immutable graph inputs, shared with editor snapshots.
    pub fn graph_buffer_snapshots(&self) -> Vec<SharedBufferSnapshot> {
        self.graph_buffer_stamp()
            .into_iter()
            .filter_map(|(id, revision, ..)| self.shared_buffer_snapshot(id, Some(revision)).ok())
            .collect()
    }

    /// Get a cached immutable snapshot shared by all views of this revision.
    pub fn shared_buffer_snapshot(
        &self,
        buffer_id: BufferId,
        revision: Option<Revision>,
    ) -> Result<SharedBufferSnapshot, BufferSnapshotError> {
        let _turn = self.driving_turn.lock_recovering();
        self.shared_buffer_snapshot_inner(buffer_id, revision)
    }
    pub(crate) fn shared_buffer_snapshot_inner(
        &self,
        buffer_id: BufferId,
        revision: Option<Revision>,
    ) -> Result<SharedBufferSnapshot, BufferSnapshotError> {
        let manager = self.buffer_manager.lock_recovering();
        let buffer = manager
            .get_buffer_by_id(buffer_id)
            .ok_or(BufferSnapshotError::NotFound(buffer_id.as_u64()))?;
        if let Some(expected) = revision
            && buffer.revision() != expected
        {
            return Err(BufferSnapshotError::Stale {
                expected: expected.as_u64(),
                actual: buffer.revision().as_u64(),
            });
        }
        let path = manager
            .path_for_id(buffer_id)
            .cloned()
            .ok_or(BufferSnapshotError::NotFound(buffer_id.as_u64()))?;
        let current_revision = buffer.revision();
        let text = {
            let mut cache = self.shared_snapshots.lock_recovering();
            match cache.get(&buffer_id) {
                Some((cached_revision, text)) if *cached_revision == current_revision => {
                    text.clone()
                }
                _ => {
                    let text: std::sync::Arc<str> = buffer.text().to_owned().into();
                    cache.insert(buffer_id, (current_revision, text.clone()));
                    text
                }
            }
        };
        Ok(SharedBufferSnapshot {
            buffer_id,
            revision: current_revision,
            text,
            base_hash: buffer.base_hash().map(|hash| hash.as_bytes()),
            state: buffer.state(),
            path,
            source_format: buffer.model().source_format(),
        })
    }

    /// Internal access for Session-owned integrations and in-crate tests.
    #[must_use]
    #[cfg(test)]
    pub(crate) fn buffer_manager(&self) -> Arc<Mutex<BufferManager>> {
        self.buffer_manager.clone()
    }

    /// Write recovery files for dirty buffers whose configured interval is
    /// due. This explicit maintenance hook is used by tests and recovery
    /// tooling; normal host operation uses the dedicated background worker.
    /// Errors remain best-effort while the dirty buffer remains present.
    pub fn recovery_tick(&self) -> Vec<PathBuf> {
        let Some(writer) = &self.recovery_writer else {
            return Vec::new();
        };
        let manager = self.buffer_manager.lock_recovering();
        let dirty = manager
            .dirty_buffers()
            .into_iter()
            .map(|(path, buffer)| {
                (
                    native_note_key(path, &self.identity.root),
                    buffer.base_bytes().to_vec(),
                    flavite_buffer::recovery::RecoveryFile::from_buffer(buffer, 0, false),
                )
            })
            .collect::<Vec<_>>();
        drop(manager);
        let mut written = Vec::new();
        for (key, base, snapshot) in dirty {
            let due = writer.lock_recovering().should_write_snapshot(&snapshot);
            if due
                && self.actor.pin_merge_base(key.clone(), base).is_ok()
                && let Some(path) = writer.lock_recovering().write_snapshot(&key, snapshot)
            {
                written.push(path);
            }
        }
        written
    }

    /// Force a durable checkpoint for every dirty buffer before a host exits.
    /// Returns failures rather than claiming that the timer interval alone
    /// protects edits made shortly before shutdown.
    pub fn flush_recovery(&self) -> Result<Vec<PathBuf>, String> {
        let snapshots = {
            let manager = self.buffer_manager.lock_recovering();
            manager
                .dirty_buffers()
                .into_iter()
                .map(|(path, buffer)| {
                    (
                        native_note_key(path, &self.identity.root),
                        buffer.base_bytes().to_vec(),
                        flavite_buffer::recovery::RecoveryFile::from_buffer(buffer, 0, false),
                    )
                })
                .collect::<Vec<_>>()
        };
        if snapshots.is_empty() {
            return Ok(Vec::new());
        }
        let Some(writer) = &self.recovery_writer else {
            return Err("local recovery storage is unavailable".into());
        };
        let mut written = Vec::with_capacity(snapshots.len());
        for (key, base, snapshot) in snapshots {
            self.actor
                .pin_merge_base(key.clone(), base)
                .map_err(|error| format!("could not persist recovery merge base: {error}"))?;
            writer
                .lock_recovering()
                .write_snapshot_now(&key, snapshot)
                .ok_or_else(|| {
                    "could not durably write a dirty-buffer recovery snapshot".to_owned()
                })
                .map(|path| written.push(path))?;
        }
        Ok(written)
    }

    /// Get the vault identity.
    #[must_use]
    pub fn identity(&self) -> &VaultIdentity {
        &self.identity
    }

    /// Get the UUID.
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.identity.uuid
    }

    /// Get the vault root.
    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.identity.root
    }

    /// Get the pulse channel.
    #[must_use]
    pub fn pulse_channel(&self) -> &PulseChannel {
        &self.pulse
    }

    /// Get the notice channel.
    #[must_use]
    pub fn notice_channel(&self) -> &NoticeChannel {
        &self.notice
    }

    /// Check if a buffer is open for a path.
    #[must_use]
    pub fn is_open(&self, path: &Path) -> bool {
        self.buffer_manager
            .lock_recovering()
            .has_buffer(&path.to_path_buf())
    }

    /// Get the buffer ID for an open path.
    #[must_use]
    pub fn get_buffer_id(&self, path: &Path) -> Option<BufferId> {
        self.buffer_manager
            .lock_recovering()
            .get_buffer(&path.to_path_buf())
            .map(|buffer| buffer.buffer_id())
    }

    /// Return the number of autosave attempts that failed.
    #[must_use]
    pub fn autosave_failures_total(&self) -> u64 {
        self.autosave_failures_total
            .load(Ordering::Relaxed)
            .max(self.actor.autosave_failures_total())
    }

    /// Cumulative rename items skipped by the journal executor.
    #[must_use]
    pub fn rename_items_skipped_total(&self) -> u64 {
        self.actor.rename_items_skipped_total()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.flush_recovery() {
            tracing::error!(error = %error, "failed to flush dirty buffers during session shutdown");
        }
        self.recovery_stop.store(true, Ordering::Release);
        if let Some(worker) = self.recovery_worker.take() {
            let _ = worker.join();
        }
    }
}

/// Serialized bytes for a history archive of a buffer's current content.
///
/// History archives draft content, not a disk claim: a strict-ASCII draft
/// holding non-ASCII characters is stored as its decoded UTF-8 layer (its
/// descriptor forbids the encoded form, §6.10), and saving it still fails
/// with `Unencodable` before any file effect.
fn history_bytes(buffer: &flavite_buffer::state::Buffer) -> Vec<u8> {
    buffer
        .model()
        .try_to_source_bytes()
        .unwrap_or_else(|_| buffer.model().decoded_text().into_bytes())
}

pub(crate) fn native_note_key(path: &Path, root: &Path) -> Vec<u8> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        relative.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        relative
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        relative.to_string_lossy().as_bytes().to_vec()
    }
}

/// Builder for Session.
#[derive(Default)]
pub struct SessionBuilder {
    identity: Option<VaultIdentity>,
    actor: Option<VaultActorHandle>,
    buffer_manager: Option<BufferManager>,
    pulse: Option<PulseChannel>,
    notice: Option<NoticeChannel>,
    config: Option<VaultConfig>,
    recovery_notices: Vec<&'static str>,
    /// Explicit OS data directory (S8/M3); `None` = resolved platform
    /// default. Libraries never read the environment.
    data_dir: Option<PathBuf>,
}

impl SessionBuilder {
    /// Create a new builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the vault identity.
    #[must_use]
    pub fn with_identity(mut self, identity: VaultIdentity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Set the vault actor handle.
    #[must_use]
    pub fn with_actor(mut self, actor: VaultActorHandle) -> Self {
        self.actor = Some(actor);
        self
    }

    /// Set the buffer manager.
    #[must_use]
    pub fn with_buffer_manager(mut self, buffer_manager: BufferManager) -> Self {
        self.buffer_manager = Some(buffer_manager);
        self
    }

    /// Set the pulse channel.
    #[must_use]
    pub fn with_pulse(mut self, pulse: PulseChannel) -> Self {
        self.pulse = Some(pulse);
        self
    }

    /// Set the notice channel.
    #[must_use]
    pub fn with_notice(mut self, notice: NoticeChannel) -> Self {
        self.notice = Some(notice);
        self
    }

    /// Set the configuration.
    #[must_use]
    pub fn with_config(mut self, config: VaultConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Set the explicit OS data directory (S8/M3). The Session's recovery
    /// checkpoint store roots here instead of the ambient platform default.
    /// Libraries never read the data-directory override; binaries resolve
    /// it before spawning threads and pass it through this builder.
    #[must_use]
    pub fn with_data_dir(mut self, data_dir: impl Into<PathBuf>) -> Self {
        self.data_dir = Some(data_dir.into());
        self
    }

    /// Queue recovery findings for delivery on the first host pump.
    #[must_use]
    pub fn with_recovery_notices(mut self, notices: Vec<&'static str>) -> Self {
        self.recovery_notices = notices;
        self
    }

    /// Build the Session.
    pub fn build(self) -> Result<Session, String> {
        let session = Session::new_with_data_dir(
            self.identity.ok_or("Missing identity")?,
            self.actor.ok_or("Missing actor")?,
            self.buffer_manager.unwrap_or_default(),
            self.pulse.unwrap_or_else(|| PulseChannel::new(1024)),
            self.notice
                .unwrap_or_else(|| NoticeChannel::new(Duration::from_millis(300))),
            self.config.unwrap_or_default(),
            self.data_dir,
        );
        *session.recovery_notices.lock_recovering() = self.recovery_notices.into();
        Ok(session)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::VaultActorBuilder;
    use crate::channels::ChannelSet;
    use crate::coalesce::WatcherEvent;
    use flavite_buffer::state::BufferState;
    use flavite_fs::{Fs, MemFs};
    use flavite_index::IndexActor;
    use flavite_syntax::text::{Span, TextByte};
    use std::path::PathBuf;

    fn test_session() -> (Session, MemFs, tempfile::TempDir) {
        let fs = MemFs::new();
        let root = PathBuf::from("/test/vault");
        fs.create_dir_all(&root).unwrap();
        fs.write_new(&root.join("note.md"), b"hello").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root, vec![1, 2, 3]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let channels = ChannelSet::new();
        let session = Session::new(
            identity,
            actor,
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        (session, fs, temp)
    }

    /// Open through the async flight and wait, for tests that need the
    /// buffer identity immediately.
    fn open_to_terminal(session: &Session, path: PathBuf) -> SessionResult {
        match session.execute(SessionCommand::OpenNote {
            path,
            policy: OpenPolicy::CreateIfMissing,
        }) {
            SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
            result => result,
        }
    }

    fn save_to_terminal(session: &Session, path: PathBuf) -> SessionResult {
        match session.execute(SessionCommand::SaveNote { path }) {
            SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
            result => result,
        }
    }

    #[test]
    fn test_session_builder() {
        let identity = VaultIdentity::new(
            uuid::Uuid::now_v7(),
            PathBuf::from("/test/vault"),
            vec![1, 2, 3, 4],
        );
        let fs = MemFs::new();
        fs.create_dir_all(&identity.root).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("builder.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let channels = ChannelSet::new();
        let session = SessionBuilder::new()
            .with_identity(identity)
            .with_actor(actor)
            .with_buffer_manager(BufferManager::new())
            .with_pulse(channels.pulse)
            .with_notice(channels.notice)
            .with_config(VaultConfig::default())
            .build()
            .unwrap();
        assert!(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .all_buffers()
                .is_empty()
        );
    }

    #[test]
    fn open_recovery_notices_are_emitted_once_and_ackable() {
        let (session, _fs, _temp) = test_session();
        *session.recovery_notices.lock().unwrap() =
            vec!["rename-diverged", "journal-quarantined"].into();
        session.pump();
        let first = session.notice.try_recv().unwrap();
        let second = session.notice.try_recv().unwrap();
        assert!(matches!(
            first,
            crate::channels::Notice::ActionRequired {
                code: "rename-diverged",
                ..
            }
        ));
        assert!(matches!(
            second,
            crate::channels::Notice::ActionRequired {
                code: "journal-quarantined",
                ..
            }
        ));
        session.notice.ack(&first);
        session.notice.ack(&second);
        session.pump();
        assert!(session.notice.try_recv().is_err());
    }

    #[test]
    fn test_request_counter() {
        let (session, _, _) = test_session();
        assert_eq!(session.next_request_id(), 0);
        assert_eq!(session.next_request_id(), 1);
    }

    /// S4/FR-7 core contract, proven on the shared containment core that
    /// both turn wrappers call: log + `panics_total` + `SessionEvent::Panicked`
    /// + honest error result naming the panic. Removing any leg fails it.
    #[test]
    fn contained_panic_records_event_counter_and_error() {
        let (session, _fs, _temp) = test_session();

        let result = session
            .record_contained_panic(&String::from("disk write tripped an assert"), "execute");
        match result {
            SessionResult::Error { message } => {
                assert!(
                    message.contains("session turn panicked during execute"),
                    "error must name the turn honestly: {message}"
                );
                assert!(message.contains("disk write tripped an assert"));
            }
            other => panic!("expected contained-panic error, got {other:?}"),
        }
        assert_eq!(session.panics_total.load(Ordering::SeqCst), 1);

        let events = session.drain_events();
        let panicked: Vec<&SessionEvent> = events
            .iter()
            .filter(|e| matches!(e, SessionEvent::Panicked { .. }))
            .collect();
        assert_eq!(panicked.len(), 1, "contained panic emits exactly one event");
        assert!(matches!(
            panicked[0],
            SessionEvent::Panicked { message, panics_total: 1 }
                if message == "disk write tripped an assert"
        ));

        // A second containment accumulates: the event carries the running
        // total so hosts can alert on `panics_total > 0` (R1 signal).
        session.record_contained_panic(&"again", "pump");
        assert_eq!(session.panics_total.load(Ordering::SeqCst), 2);
        let second = session
            .drain_events()
            .into_iter()
            .find(|e| matches!(e, SessionEvent::Panicked { .. }));
        assert!(matches!(
            second,
            Some(SessionEvent::Panicked {
                panics_total: 2,
                ..
            })
        ));
    }

    /// Payload downcast chain locked by Q3: `&str` → `String` → debug
    /// backstop.
    #[test]
    fn panic_message_downcasts_through_the_chain() {
        assert_eq!(panic_message(&"static slice"), "static slice");
        assert_eq!(panic_message(&String::from("owned string")), "owned string");
        // Non-string payloads fall back to the Debug representation;
        // `dyn Any`'s Debug is opaque ("Any { .. }") — the message is still
        // honest about being a panic without claiming payload detail.
        assert_eq!(panic_message(&42u8), "Any { .. }");
    }

    /// Wrapper transparency: no happy-path result is swallowed by the
    /// containment. `execute` returns exactly what the turn returns and
    /// `pump` completes; `panics_total` stays 0 across real traffic.
    #[test]
    fn execute_and_pump_wrappers_pass_results_through() {
        let (session, _fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");

        let result = session.execute(SessionCommand::SaveNote { path: path.clone() });
        assert!(
            !matches!(&result, SessionResult::Error { message }
                if message.contains("session turn panicked")),
            "happy path must not be reported as a contained panic: {result:?}"
        );
        session.pump();
        let again = session.execute(SessionCommand::SaveNote { path });
        assert!(!matches!(&again, SessionResult::Error { message }
                if message.contains("session turn panicked")),);
        assert_eq!(session.panics_total.load(Ordering::SeqCst), 0);
    }

    /// NFR-2 at the turn level: one panic-while-held poisons a Session
    /// mutex, and afterwards `execute`, `pump`, and `drain_events` must
    /// complete (with recovery counted), not cascade into a wedged vault.
    /// This test can fail: revert any S3 site and later turns panic.
    #[test]
    fn poisoned_mutex_does_not_wedge_execute_pump_or_drain() {
        let _serial = crate::sync::poison_test_serial()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (session, _fs, _temp) = test_session();

        crate::sync::poison_mutex_for_test(&session.save_flights);
        let before = crate::sync::POISON_RECOVERIES.load(Ordering::Relaxed);

        // Every later turn touches the poisoned mutex through recovering
        // locks; none of this may panic.
        let path = PathBuf::from("/test/vault/note.md");
        let _ = session.execute(SessionCommand::SaveNote { path });
        session.pump();
        let _ = session.drain_events();

        let live = crate::sync::POISON_RECOVERIES.load(Ordering::Relaxed);
        assert!(live > before, "recoveries must be counted, not hidden");
        assert_eq!(
            session.poison_recoveries_total.load(Ordering::Relaxed),
            live,
            "drain_events must mirror the process counter into the Session"
        );
        assert_eq!(session.panics_total.load(Ordering::SeqCst), 0);
    }

    /// Keep-alive in-flight save so `operation_in_flight` answers `true`
    /// without a live worker: the sender stays with the caller, the
    /// receiver parks in the flight table until the test delivers.
    fn fabricate_inflight_save(
        session: &Session,
        request_id: u64,
    ) -> crossbeam::channel::Sender<crate::actor::VaultActorResp> {
        let (keep_tx, keep_rx) = crossbeam::channel::unbounded();
        session.save_flights.lock_recovering().insert(
            request_id,
            SaveFlight {
                request: SaveRequest {
                    request_id,
                    path: PathBuf::from("/test/vault/note.md"),
                    note_key: b"fabricated-key".to_vec(),
                    bytes: Vec::new(),
                    merge_base: Vec::new(),
                    precondition: crate::save::DiskPrecondition::Existing([7u8; 32]),
                    buffer_id: request_id,
                    revision: 0,
                },
                response: keep_rx,
                queued: None,
            },
        );
        keep_tx
    }

    /// S6/Q4: a headless wait must NOT hold the driving turn. Thread A
    /// waits an in-flight save while thread B executes unrelated commands
    /// for a fixed window. Under the former turn-holding wait B is starved
    /// for the entire window (it blocks on the first `execute`), which is
    /// exactly what this test shows red against the old design.
    #[test]
    fn wait_for_operation_returns_without_turn_and_terminates() {
        let (session, _fs, _temp) = test_session();
        let request_id = 7_777u64;
        let _keep = fabricate_inflight_save(&session, request_id);
        let session = std::sync::Arc::new(session);

        std::thread::scope(|scope| {
            let waiter = {
                let session = std::sync::Arc::clone(&session);
                scope.spawn(move || session.wait_for_operation(request_id))
            };
            let driver = {
                let session = std::sync::Arc::clone(&session);
                scope.spawn(move || {
                    let started = std::time::Instant::now();
                    let mut completed = 0u64;
                    while started.elapsed() < Duration::from_millis(250) {
                        // Cheap unrelated command: closing a never-opened
                        // path is a preflight error and mutates nothing.
                        session.execute(SessionCommand::CloseNote {
                            path: PathBuf::from("/test/vault/not-open.md"),
                            retain_recovery: false,
                        });
                        completed += 1;
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    completed
                })
            };
            std::thread::sleep(Duration::from_millis(300));
            // Deliver through the production writer. This test's subject is
            // the turn, not the wake latency (the wake-edge test pins that).
            session.record_completion(
                request_id,
                SessionResult::Error {
                    message: "test-delivered".into(),
                },
            );
            let b_count = driver.join().expect("driver thread");
            let result = waiter.join().expect("waiter thread");
            assert!(
                matches!(&result, SessionResult::Error { message } if message == "test-delivered"),
                "{result:?}"
            );
            assert!(
                b_count >= 20,
                "waiter starved the driving turn: B completed only {b_count} commands in 250ms"
            );
        });
    }

    /// S6 wake-edge proof: a completion recorded on another thread wakes a
    /// parked waiter materially BEFORE its 25 ms tick ceiling. If
    /// `record_completion`'s `notify_all` regressed to a silent push, the
    /// wake would wait out the remaining tick — near-certainly > 5 ms.
    #[test]
    fn wait_wakes_on_completion_from_another_thread() {
        let (session, _fs, _temp) = test_session();
        let request_id = 8_888u64;
        let _keep = fabricate_inflight_save(&session, request_id);
        let session = std::sync::Arc::new(session);

        std::thread::scope(|scope| {
            let waiter = {
                let session = std::sync::Arc::clone(&session);
                scope.spawn(move || session.wait_for_operation(request_id))
            };
            // Guarantee the waiter is parked before delivery is timed.
            std::thread::sleep(Duration::from_millis(30));
            let started = std::time::Instant::now();
            session.record_completion(
                request_id,
                SessionResult::Success(CommandResult::NoteClosed),
            );
            let result = waiter.join().expect("waiter thread");
            let woke_in = started.elapsed();
            assert!(matches!(
                result,
                SessionResult::Success(CommandResult::NoteClosed)
            ));
            assert!(
                woke_in < Duration::from_millis(5),
                "wake came on the tick, not the notify edge: {woke_in:?} (WAIT_TICK is {WAIT_TICK:?})"
            );
        });
    }

    /// R4/NFR-3: forward progress is bounded without any notify —
    /// (a) an unknown request id returns the honest
    /// "unknown or already-drained" error immediately, never parking;
    /// (b) an in-flight id that never completes parks at the tick ceiling:
    /// over a 600 ms window the observed waiter takes ~24 parks (bound
    /// 40/s), proving neither a 1 ms spin nor a hang; a delivery still
    /// terminates it afterwards.
    ///
    /// The park counter observes only a designated request id (and the test
    /// serializes that designation) so concurrent waiters in other tests
    /// cannot contaminate the budget.
    #[test]
    fn unknown_and_parked_waits_make_bounded_progress() {
        let _serial = crate::sync::poison_test_serial()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (session, _fs, _temp) = test_session();

        // (a) Unknown id: immediate, no park.
        let before_parks = crate::sync::WAIT_PARKS.load(Ordering::Relaxed);
        let unknown = session.wait_for_operation(9_999);
        assert!(
            matches!(&unknown, SessionResult::Error { message }
                if message == "unknown or already-drained operation 9999"),
            "{unknown:?}"
        );
        assert_eq!(
            crate::sync::WAIT_PARKS.load(Ordering::Relaxed),
            before_parks,
            "the unknown-id exit must not park"
        );

        // (b) Never-completed in-flight id: tick-bounded parks, then a
        // delivery ends the wait.
        let request_id = 6_666u64;
        let _keep = fabricate_inflight_save(&session, request_id);
        crate::sync::OBSERVED_WAIT_ID.store(request_id, Ordering::Relaxed);
        crate::sync::WAIT_PARKS.store(0, Ordering::Relaxed);
        let session = std::sync::Arc::new(session);
        std::thread::scope(|scope| {
            let waiter = {
                let session = std::sync::Arc::clone(&session);
                scope.spawn(move || session.wait_for_operation(request_id))
            };
            std::thread::sleep(Duration::from_millis(600));
            assert!(
                !waiter.is_finished(),
                "a never-completed wait must stay parked, not exit silently"
            );
            let parks = crate::sync::WAIT_PARKS.load(Ordering::Relaxed);
            session.record_completion(request_id, SessionResult::Busy);
            let result = waiter.join().expect("waiter thread");
            crate::sync::OBSERVED_WAIT_ID.store(u64::MAX, Ordering::Relaxed);
            assert!(matches!(result, SessionResult::Busy));
            // ≤ 40/s budget over 600 ms = 24, plus scheduling slack; a 1 ms
            // spin would show ~600 parks. ≥ 10 proves timeout-driven
            // forward progress rather than a lost-wakeup hang.
            assert!(
                (10..=40).contains(&parks),
                "idle waiter parked {parks} times in ~600ms — expected ~24 (25ms tick), \
                 the old 1ms poll would show ~600"
            );
        });
    }

    /// FR-10 unit leg: a recorded completion is visible to `take_completion`
    /// and `drain_operation_completions` with FIFO order; taking one id is
    /// lossless for the others (the ledger contract of the old inline scan).
    #[test]
    fn recorded_completions_are_lossless_and_fifo() {
        let (session, _fs, _temp) = test_session();
        session.record_completion(1, SessionResult::Busy);
        session.record_completion(
            2,
            SessionResult::Error {
                message: "second".into(),
            },
        );
        session.record_completion(3, SessionResult::Success(CommandResult::NoteClosed));

        let taken = session.take_completion(2);
        match &taken {
            Some(SessionResult::Error { message }) => assert_eq!(message, "second"),
            other => panic!("expected the recorded error for id 2, got {other:?}"),
        }
        assert!(session.take_completion(2).is_none(), "take is consuming");

        let drained = session.drain_operation_completions();
        assert_eq!(
            drained
                .iter()
                .map(|completion| completion.request_id)
                .collect::<Vec<_>>(),
            vec![1, 3],
            "untouched completions keep FIFO order after a mid-queue take"
        );
    }

    #[test]
    fn reconcile_and_save_completions_keep_request_identity_and_revision() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let (buffer_id, revision) = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected opened note, got {other:?}"),
        };
        let first_revision = match session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(5), TextByte(5)),
                " one".into(),
            )],
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
        }) {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("expected first edit, got {other:?}"),
        };

        let first_save = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("expected first save to be admitted, got {other:?}"),
        };
        let second_revision = match session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision: first_revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(9), TextByte(9)),
                " two".into(),
            )],
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
        }) {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("expected second edit, got {other:?}"),
        };
        let superseded = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("expected queued save to be admitted, got {other:?}"),
        };
        let final_revision = match session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision: second_revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(13), TextByte(13)),
                " three".into(),
            )],
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
        }) {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("expected third edit, got {other:?}"),
        };
        let final_save = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("expected coalesced save to be admitted, got {other:?}"),
        };

        assert!(matches!(
            session.wait_for_operation(first_save),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert!(matches!(
            session.wait_for_operation(superseded),
            SessionResult::Superseded {
                request_id,
                by_request
            } if request_id == superseded && by_request == final_save
        ));
        assert!(matches!(
            session.wait_for_operation(final_save),
            SessionResult::Success(CommandResult::NoteSaved { saved_revision, .. })
                if saved_revision == final_revision
        ));
        assert_eq!(
            fs.read_if_exists(&path).unwrap().unwrap(),
            b"hello one two three"
        );

        let reconcile = match session.execute(SessionCommand::Reconcile {
            scope: ReconcileScope::Full,
            mode: ReconcileMode::VerifyContent,
        }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("expected reconcile to be admitted, got {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(reconcile),
            SessionResult::Success(CommandResult::ReconcileComplete { .. })
        ));
        assert!(session.drain_operation_completions().is_empty());
        assert!(matches!(
            session.wait_for_operation(reconcile),
            SessionResult::Error { .. }
        ));
    }

    #[test]
    fn session_open_edit_undo_redo_and_close_tracks_buffer_state() {
        let (session, _fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let opened = open_to_terminal(&session, path.clone());
        let (buffer_id, revision) = match opened {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected opened note, got {other:?}"),
        };
        assert!(session.is_open(&path));
        assert_eq!(session.get_buffer_id(&path), Some(buffer_id));
        let shared = session.shared_buffer_snapshot(buffer_id, None).unwrap();
        assert_eq!(shared.text.as_ref(), "hello");
        assert!(
            session
                .shared_snapshots
                .lock()
                .unwrap()
                .contains_key(&buffer_id)
        );
        assert!(matches!(
            open_to_terminal(&session, path.clone()),
            SessionResult::Success(CommandResult::NoteOpened { .. })
        ));

        let edit = EditOp::new(Span::new(TextByte(5), TextByte(5)), " world".into());
        let edited = session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![edit],
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
        });
        assert!(matches!(
            edited,
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::StaleRevision { .. }
        ));
        let edited_revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::Undo {
                buffer_id,
                revision,
            }),
            SessionResult::StaleRevision { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Undo {
                buffer_id,
                revision: edited_revision,
            }),
            SessionResult::Success(CommandResult::UndoPerformed { .. })
        ));
        let undone_revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision: edited_revision,
            }),
            SessionResult::StaleRevision { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision: undone_revision,
            }),
            SessionResult::Success(CommandResult::RedoPerformed { .. })
        ));
        let request_id = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("save must be admitted asynchronously: {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(request_id),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(_fs.read_if_exists(&path).unwrap().unwrap(), b"hello world");
        assert!(matches!(
            session.execute(SessionCommand::CloseNote {
                path: path.clone(),
                retain_recovery: false,
            }),
            SessionResult::Success(CommandResult::NoteClosed)
        ));
        assert!(!session.is_open(&path));
        assert!(
            !session
                .shared_snapshots
                .lock()
                .unwrap()
                .contains_key(&buffer_id)
        );
        assert!(matches!(
            session.execute(SessionCommand::CloseNote {
                path,
                retain_recovery: false,
            }),
            SessionResult::Error { .. }
        ));
    }

    #[test]
    fn watcher_change_conflicts_with_dirty_buffer_through_session_pump() {
        let fs = MemFs::new();
        let root = PathBuf::from("/watch/vault");
        fs.create_dir_all(&root).unwrap();
        let path = root.join("note.md");
        fs.write_new(&path, b"hello").unwrap();
        let clean_path = root.join("clean.md");
        fs.write_new(&clean_path, b"clean before").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("session-watch.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![1, 2, 3]);
        let (event_tx, event_rx) = crate::actor::event_source_channel();
        let channels = ChannelSet::new();
        let pulse_rx = channels.pulse.receiver();
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()))
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .with_event_source(event_rx)
            .with_pulse_sender(channels.pulse.sender())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let mut session = Session::new(
            identity,
            actor,
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        session.autosave_idle_interval = Duration::from_secs(60);
        let (buffer_id, revision) = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected open, got {other:?}"),
        };
        let _ = session.drain_pulses();
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![EditOp::new(
                    Span::new(TextByte(5), TextByte(5)),
                    " mine".into()
                )],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        let clean_id = match open_to_terminal(&session, clean_path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected clean note open, got {other:?}"),
        };
        fs.write_new(&root.join(".external"), b"theirs").unwrap();
        fs.rename_replace(&root.join(".external"), &path).unwrap();
        // Bulk event pressure must still produce the conflict notice through
        // the lossless Session event path.
        for _ in 0..500 {
            event_tx.send(WatcherEvent::Modified(path.clone())).unwrap();
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut conflicted = false;
        while std::time::Instant::now() < deadline {
            while pulse_rx.try_recv().is_ok() {}
            session.pump();
            conflicted = session
                .buffer_manager
                .lock()
                .unwrap()
                .get_buffer(&path)
                .unwrap()
                .state()
                == BufferState::Conflicted;
            if conflicted {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            conflicted,
            "watcher change did not conflict the dirty buffer"
        );
        assert!(session.drain_notices().iter().any(|notice| matches!(notice, crate::channels::Notice::Conflict { note } if note.path == path)));
        fs.write_new(&root.join(".external-clean"), b"clean after")
            .unwrap();
        fs.rename_replace(&root.join(".external-clean"), &clean_path)
            .unwrap();
        event_tx
            .send(WatcherEvent::Modified(clean_path.clone()))
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            session.pump();
            if session
                .buffer_manager
                .lock()
                .unwrap()
                .get_buffer_by_id(clean_id)
                .unwrap()
                .text()
                == "clean after"
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let manager = session.buffer_manager.lock().unwrap();
        let clean = manager.get_buffer_by_id(clean_id).unwrap();
        assert_eq!(clean.state(), BufferState::Clean);
        assert_eq!(clean.text(), "clean after");
    }

    #[test]
    fn host_clock_pump_autosaves_an_idle_dirty_revision() {
        let (mut session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        session.autosave_idle_interval = Duration::from_millis(5);
        let (buffer_id, revision) = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected opened note, got {other:?}"),
        };
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![EditOp::new(Span::new(TextByte(5), TextByte(5)), "!".into())],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        std::thread::sleep(Duration::from_millis(10));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            session.pump();
            if fs.read_if_exists(&path).unwrap().as_deref() == Some(b"hello!") {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("idle dirty revision was not autosaved by the Session clock pump");
    }

    fn rename_to_terminal(session: &Session, from: PathBuf, to: PathBuf) -> SessionResult {
        match session.execute(SessionCommand::RenameNote {
            from,
            to,
            strategy: Some(RenameStrategy::Rewrite),
        }) {
            SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
            result => result,
        }
    }

    fn edit_append(session: &Session, buffer_id: BufferId, revision: Revision, suffix: &str) {
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![EditOp::new(
                    Span::new(TextByte(5), TextByte(5)),
                    suffix.into()
                )],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
    }

    #[test]
    fn renamed_open_note_remains_savable() {
        let (session, fs, _temp) = test_session();
        let from = PathBuf::from("/test/vault/note.md");
        let to = PathBuf::from("/test/vault/renamed.md");
        let (buffer_id, revision) = match open_to_terminal(&session, from.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected open, got {other:?}"),
        };
        edit_append(&session, buffer_id, revision, "!");

        assert!(matches!(
            rename_to_terminal(&session, from.clone(), to.clone()),
            SessionResult::Success(CommandResult::NoteRenamed { .. })
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && !session.is_open(&to) {
            session.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
        // M2 paydown (bounded to loops in this file, S6): on exhaustion the
        // failure message carries the Session backlog, not just the bool.
        if !session.is_open(&to) {
            panic!(
                "renamed buffer never surfaced open; events={:?} notices={:?}",
                session.drain_events(),
                session.drain_notices()
            );
        }
        assert_eq!(session.get_buffer_id(&to), Some(buffer_id));
        let save_result = save_to_terminal(&session, to.clone());
        assert!(
            matches!(&save_result, SessionResult::Success(CommandResult::NoteSaved { path, .. }) if *path == to),
            "renamed note save failed: {save_result:?}"
        );
        assert_eq!(
            fs.read_if_exists(&to).unwrap().as_deref(),
            Some(&b"hello!"[..])
        );
    }

    #[test]
    fn open_inbound_buffer_is_patched_as_one_journal_undo_and_cancelled_durably() {
        let (session, fs, _temp) = test_session();
        let source = PathBuf::from("/test/vault/links.md");
        let from = PathBuf::from("/test/vault/note.md");
        let to = PathBuf::from("/test/vault/renamed.md");
        fs.write_new(&source, b"See [[note]] / [[note]] / [[other]]")
            .unwrap();
        let scan = session
            .actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap();
        scan.recv_timeout(Duration::from_secs(2)).unwrap();
        let (buffer_id, revision) = match open_to_terminal(&session, source.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected inbound buffer to open: {other:?}"),
        };

        assert!(matches!(
            rename_to_terminal(&session, from, to),
            SessionResult::Success(CommandResult::NoteRenamed { .. })
        ));
        let current = session.buffer_manager.lock().unwrap();
        let buffer = current.get_buffer_by_id(buffer_id).unwrap();
        assert_eq!(buffer.text(), "See [[renamed]] / [[renamed]] / [[other]]");
        let patched_revision = buffer.revision();
        drop(current);

        let undo = session.execute(SessionCommand::Undo {
            buffer_id,
            revision: patched_revision,
        });
        assert!(
            matches!(
                undo,
                SessionResult::Success(CommandResult::UndoPerformed { .. })
            ),
            "{undo:?}"
        );
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision: patched_revision.next(),
            }),
            SessionResult::Error { message } if message == "redo-after-cancel"
        ));
        let current = session.buffer_manager.lock().unwrap();
        assert_eq!(
            current.get_buffer_by_id(buffer_id).unwrap().text(),
            "See [[note]] / [[note]] / [[other]]"
        );
        drop(current);
        assert!(matches!(
            save_to_terminal(&session, source.clone()),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(
            fs.read_if_exists(&source).unwrap().unwrap(),
            b"See [[note]] / [[note]] / [[other]]"
        );
        let _ = revision;
    }

    /// C1 scoping proof: a journal patch memo must never cross buffers.
    /// Both requests carry the same `(txn_id, item_id)` (the global-memo
    /// bug: the second buffer gets the first buffer's memoized reply and is
    /// never patched). On the per-buffer memo each buffer patches itself.
    #[test]
    fn journal_memo_is_scoped_per_buffer() {
        let (session, fs, _temp) = test_session();
        let path_a = PathBuf::from("/test/vault/links-a.md");
        let path_b = PathBuf::from("/test/vault/links-b.md");
        fs.write_new(&path_a, b"See [[note]] and [[other]].")
            .unwrap();
        fs.write_new(&path_b, b"Ref [[note]] here.").unwrap();
        let scan = session
            .actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap();
        scan.recv_timeout(Duration::from_secs(2)).unwrap();
        let a_id = match open_to_terminal(&session, path_a.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected buffer A to open: {other:?}"),
        };
        let b_id = match open_to_terminal(&session, path_b.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected buffer B to open: {other:?}"),
        };

        fn patch_state(
            session: &Session,
            buffer_id: flavite_buffer::revision::BufferId,
        ) -> (u64, [u8; 32], String) {
            let manager = session.buffer_manager.lock().unwrap();
            let buffer = manager.get_buffer_by_id(buffer_id).unwrap();
            (
                buffer.revision().as_u64(),
                *blake3::hash(buffer.base_bytes()).as_bytes(),
                buffer.text().to_string(),
            )
        }
        let request = |txn: &str,
                       item: u64,
                       buffer_id: flavite_buffer::revision::BufferId,
                       expected: u64,
                       hash: [u8; 32],
                       _base: &str,
                       patched: &str,
                       recovery: &[u8]| {
            let (reply, receipt) = crossbeam::channel::bounded(1);
            (
                crate::actor::ApplyBufferPatchRequest {
                    txn_id: txn.to_owned(),
                    item_id: item,
                    kind: crate::journal::TransactionItemKind::Patch,
                    buffer_id: buffer_id.as_u64(),
                    expected_revision: expected,
                    old_stem: "note".into(),
                    new_stem: "renamed".into(),
                    target_key: "note".into(),
                    indexed_hash: hash,
                    recovery_key: Some(recovery.to_vec()),
                    new_base_bytes: patched.as_bytes().to_vec(),
                    reply,
                },
                receipt,
            )
        };

        let (rev_a, hash_a, _text_a) = patch_state(&session, a_id);
        let (req_a, receipt_a) = request(
            "memo-scope",
            7,
            a_id,
            rev_a,
            hash_a,
            "See [[note]] and [[other]].",
            "See [[renamed]] and [[other]].",
            b"memo-scope-a",
        );
        session
            .actor
            .buffer_patch_sender_for_test()
            .send(req_a)
            .unwrap();
        session.pump();
        let reply_a = receipt_a.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(reply_a.is_ok(), "buffer A patch must succeed: {reply_a:?}");

        // Same (txn_id, item_id), different buffer: must patch B itself.
        let (rev_b, hash_b, _text_b) = patch_state(&session, b_id);
        let (req_b, receipt_b) = request(
            "memo-scope",
            7,
            b_id,
            rev_b,
            hash_b,
            "Ref [[note]] here.",
            "Ref [[renamed]] here.",
            b"memo-scope-b",
        );
        session
            .actor
            .buffer_patch_sender_for_test()
            .send(req_b)
            .unwrap();
        session.pump();
        let reply_b = receipt_b.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(reply_b.is_ok(), "buffer B patch must succeed: {reply_b:?}");
        assert_eq!(
            patch_state(&session, b_id).2,
            "Ref [[renamed]] here.",
            "a shared (txn_id, item_id) key must not mask buffer B's patch"
        );
        assert_eq!(
            patch_state(&session, a_id).2,
            "See [[renamed]] and [[other]].",
            "buffer A keeps its own patched text"
        );
    }

    /// S5/Q1 proof obligation — decided by test, not by assumption. Before
    /// eviction a replayed `(txn_id, item_id)` MUST return the memoized
    /// outcome without re-applying (FR-2); after FIFO-cap eviction the
    /// replay MUST go through the actor's revision guard and come back as
    /// an honest error with text and revision untouched. This is what makes
    /// the FIFO cap safe to ship; if the final leg failed, the Q1 fallback
    /// (reject-and-log) would activate instead of trusting eviction.
    #[test]
    fn journaled_patch_replay_is_memoized_then_safe_after_eviction() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/links.md");
        fs.write_new(&path, b"See [[note]] and [[other]].").unwrap();
        let scan = session
            .actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap();
        scan.recv_timeout(Duration::from_secs(2)).unwrap();
        let buffer_id = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected buffer to open: {other:?}"),
        };

        let state = |session: &Session| -> (u64, [u8; 32], String) {
            let manager = session.buffer_manager.lock().unwrap();
            let buffer = manager.get_buffer_by_id(buffer_id).unwrap();
            (
                buffer.revision().as_u64(),
                *blake3::hash(buffer.base_bytes()).as_bytes(),
                buffer.text().to_string(),
            )
        };

        // Capture the pristine pre-patch expectation once; every request
        // below carries it, exactly as a journal replay would.
        let (rev0, hash0, _text0) = state(&session);
        let make = || {
            let (tx, receipt) = crossbeam::channel::bounded(1);
            (
                crate::actor::ApplyBufferPatchRequest {
                    txn_id: "memo-proof".into(),
                    item_id: 9,
                    kind: crate::journal::TransactionItemKind::Patch,
                    buffer_id: buffer_id.as_u64(),
                    expected_revision: rev0,
                    old_stem: "note".into(),
                    new_stem: "renamed".into(),
                    target_key: "note".into(),
                    indexed_hash: hash0,
                    recovery_key: Some(b"memo-proof".to_vec()),
                    new_base_bytes: b"See [[renamed]] and [[other]].".to_vec(),
                    reply: tx,
                },
                receipt,
            )
        };
        let sender = session.actor.buffer_patch_sender_for_test();

        // 1) First application goes through the real patch path.
        let (req1, receipt1) = make();
        sender.send(req1).unwrap();
        session.pump();
        let reply1 = receipt1.recv_timeout(Duration::from_secs(2)).unwrap();
        let applied_revision = *reply1.as_ref().expect("first patch must succeed");
        assert_eq!(state(&session).2, "See [[renamed]] and [[other]].");
        assert_eq!(state(&session).0, applied_revision);

        // 2) FR-2: identical key replays the memoized outcome without
        //    re-applying. A re-apply here would hit the revision guard and
        //    return Err — the identical Ok proves the memo answered.
        for _ in 0..4 {
            let (req, receipt) = make();
            sender.send(req).unwrap();
            session.pump();
            let replay = receipt.recv_timeout(Duration::from_secs(2)).unwrap();
            assert_eq!(replay, reply1, "replay must return the memoized outcome");
        }
        assert_eq!(state(&session).2, "See [[renamed]] and [[other]].");

        // 3) Evict the recorded key via the FIFO cap: push exactly
        //    JOURNAL_MEMO_CAP newer keys, the oldest (ours) must go, and the
        //    memo must respect its bound throughout (NFR-1 invariant).
        {
            let mut memos = session.journal_memos.lock_recovering();
            let memo = memos.entry(buffer_id).or_default();
            for i in 0..JOURNAL_MEMO_CAP {
                memo.memoize_applied((format!("evict-{i}"), i as u64), Ok(0));
                assert!(memo.applied.len() <= JOURNAL_MEMO_CAP);
                assert!(memo.order.len() <= JOURNAL_MEMO_CAP);
            }
            assert!(
                !memo.applied.contains_key(&("memo-proof".to_string(), 9u64)),
                "FIFO eviction must drop the oldest recorded key"
            );
        }

        // 4) Post-eviction replay: memo cold, patch re-attempted against the
        //    actor's revision guard — honest error, zero corruption, the
        //    revision did not advance, and no duplicated text.
        let (req3, receipt3) = make();
        sender.send(req3).unwrap();
        session.pump();
        let after_eviction = receipt3.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            after_eviction.is_err(),
            "evicted replay must not fabricate success: {after_eviction:?}"
        );
        let (rev_now, _, text_now) = state(&session);
        assert_eq!(
            text_now, "See [[renamed]] and [[other]].",
            "no duplicated patch text"
        );
        assert_eq!(
            rev_now, applied_revision,
            "replay must not advance the revision"
        );
    }

    /// NFR-1: per-buffer memo memory is bounded under sustained synthetic
    /// load — `applied`, `cancelled`, and `order` never exceed the cap, the
    /// newest entries survive, and buffers stay independent entries.
    #[test]
    fn memo_is_bounded_under_sustained_load() {
        let (session, _fs, _temp) = test_session();
        let hot = BufferId::new();
        for i in 0..(2 * JOURNAL_MEMO_CAP) {
            let key = (format!("txn-{i}"), i as u64);
            session.memo_insert(hot, key.clone(), Ok(i as u64));
            session.memo_cancel(hot, key);
        }
        let memos = session.journal_memos.lock_recovering();
        let memo = memos.get(&hot).expect("hot buffer memo exists");
        assert!(memo.order.len() <= JOURNAL_MEMO_CAP, "order cap");
        assert!(
            memo.applied.len() <= JOURNAL_MEMO_CAP,
            "applied cap: {}",
            memo.applied.len()
        );
        assert!(
            memo.cancelled.len() <= JOURNAL_MEMO_CAP,
            "cancelled cap: {}",
            memo.cancelled.len()
        );
        let newest = (
            format!("txn-{}", 2 * JOURNAL_MEMO_CAP - 1),
            (2 * JOURNAL_MEMO_CAP - 1) as u64,
        );
        assert!(
            memo.applied.contains_key(&newest),
            "FIFO keeps newest entries"
        );
        let oldest = ("txn-0".to_string(), 0u64);
        assert!(
            !memo.applied.contains_key(&oldest) && !memo.cancelled.contains(&oldest),
            "FIFO drops oldest from both substructures"
        );
    }

    /// Close tier (exact): `close_note` removes the buffer's whole memo —
    /// the memo's lifetime is now provably the buffer's lifetime (FR-1).
    #[test]
    fn close_note_clears_the_buffer_memo() {
        let (session, _fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let buffer_id = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected buffer to open: {other:?}"),
        };
        session.memo_insert(buffer_id, ("txn-close".into(), 1), Ok(3));
        session.memo_cancel(buffer_id, ("txn-close".into(), 2));
        assert!(
            session
                .journal_memos
                .lock_recovering()
                .contains_key(&buffer_id)
        );

        let closed = session.execute(SessionCommand::CloseNote {
            path: path.clone(),
            retain_recovery: true,
        });
        assert!(
            matches!(closed, SessionResult::Success(CommandResult::NoteClosed)),
            "{closed:?}"
        );
        assert!(
            session
                .journal_memos
                .lock_recovering()
                .get(&buffer_id)
                .is_none(),
            "close_note must evict the buffer's journal memo (C1)"
        );
    }

    /// Cancel scope is the buffer that undid, not the journal key's global
    /// neighborhood: a cancel recorded on one buffer can never make another
    /// buffer's redo refuse (the false-positive latent in the former
    /// ownerless set).
    #[test]
    fn cancelled_entries_are_scoped_to_their_buffer() {
        let (session, _fs, _temp) = test_session();
        let a = BufferId::new();
        let b = BufferId::new();
        let key = ("txn-shared".to_string(), 3u64);

        session.memo_cancel(a, key.clone());

        let memos = session.journal_memos.lock_recovering();
        assert!(
            memos
                .get(&a)
                .is_some_and(|memo| memo.cancelled.contains(&key))
        );
        assert!(
            memos
                .get(&b)
                .is_none_or(|memo| !memo.cancelled.contains(&key)),
            "cancel must never cross buffers"
        );
    }

    #[test]
    fn alias_strategy_updates_open_buffer_and_appends_frontmatter() {
        let (session, fs, _temp) = test_session();
        let from = PathBuf::from("/test/vault/note.md");
        let to = PathBuf::from("/test/vault/renamed.md");
        let (buffer_id, _) = match open_to_terminal(&session, from.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected source buffer open: {other:?}"),
        };
        assert!(matches!(
            session.execute(SessionCommand::RenameNote {
                from,
                to: to.clone(),
                strategy: Some(RenameStrategy::Alias),
            }),
            SessionResult::Pending { request_id } if matches!(session.wait_for_operation(request_id), SessionResult::Success(CommandResult::NoteRenamed { .. }))
        ));
        let bytes = fs.read_if_exists(&to).unwrap().unwrap();
        let text = std::str::from_utf8(&bytes).unwrap();
        assert!(text.starts_with("---\naliases: \"note\"\n---\nhello"));
        let manager = session.buffer_manager.lock().unwrap();
        assert_eq!(
            manager.get_buffer_by_id(buffer_id).unwrap().text(),
            text.trim_end_matches('\n')
        );
    }

    #[test]
    fn none_strategy_keeps_links_and_emits_counted_notice() {
        let (session, fs, _temp) = test_session();
        let source = PathBuf::from("/test/vault/links.md");
        let from = PathBuf::from("/test/vault/note.md");
        let to = PathBuf::from("/test/vault/renamed.md");
        fs.write_new(&source, b"See [[note]]").unwrap();
        let reconcile = session
            .actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap();
        reconcile.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(
            session.execute(SessionCommand::RenameNote {
                from,
                to,
                strategy: Some(RenameStrategy::None),
            }),
            SessionResult::Pending { request_id } if matches!(session.wait_for_operation(request_id), SessionResult::Success(CommandResult::NoteRenamed { .. }))
        ));
        assert_eq!(
            fs.read_if_exists(&source).unwrap().unwrap(),
            b"See [[note]]"
        );
        assert!(matches!(
            session.notice.try_recv(),
            Ok(crate::channels::Notice::LinksBroken { count: 1, .. })
        ));
    }

    #[test]
    fn skipped_rename_patch_increments_persisted_counter() {
        let (session, fs, temp) = test_session();
        let source = PathBuf::from("/test/vault/links.md");
        fs.write_new(&source, b"See [[note]]").unwrap();
        session
            .actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap()
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let (buffer_id, revision) = match open_to_terminal(&session, source.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            result => panic!("expected inbound note to open: {result:?}"),
        };
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![flavite_buffer::undo::EditOp::new(
                    Span::new(TextByte(0), TextByte("See [[note]]".len())),
                    "No link".into(),
                )],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        let rename = session.execute(SessionCommand::RenameNote {
            from: PathBuf::from("/test/vault/note.md"),
            to: PathBuf::from("/test/vault/renamed.md"),
            strategy: Some(RenameStrategy::Rewrite),
        });
        let result = match rename {
            SessionResult::Pending { request_id } => session.wait_for_operation(request_id),
            result => result,
        };
        assert!(matches!(result, SessionResult::Error { .. }));
        assert_eq!(session.rename_items_skipped_total(), 1);
        let conn = flavite_index::open_or_init(&temp.path().join("index.sqlite3")).unwrap();
        let persisted: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key='rename.items_skipped_total'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(persisted, "1");
    }

    #[test]
    fn attachment_rename_rewrites_open_embed_buffer_through_journal() {
        let (session, fs, _temp) = test_session();
        let old = PathBuf::from("/test/vault/image.png");
        let new = PathBuf::from("/test/vault/renamed-image.png");
        let source = PathBuf::from("/test/vault/embeds.md");
        fs.write_new(&old, b"image bytes").unwrap();
        fs.write_new(&source, b"![[image.png]] and ![[other.png]]")
            .unwrap();
        let reconcile = session
            .actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap();
        reconcile.recv_timeout(Duration::from_secs(2)).unwrap();
        let (buffer_id, _) = match open_to_terminal(&session, source.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected embedding note to open: {other:?}"),
        };
        let result = session.execute(SessionCommand::RenameNote {
            from: old.clone(),
            to: new.clone(),
            strategy: Some(RenameStrategy::Rewrite),
        });
        assert!(
            matches!(result, SessionResult::Pending { request_id } if matches!(session.wait_for_operation(request_id), SessionResult::Success(CommandResult::NoteRenamed { .. })))
        );
        assert!(!fs.exists(&old).unwrap());
        assert!(fs.exists(&new).unwrap());
        assert_eq!(
            session
                .buffer_manager
                .lock()
                .unwrap()
                .get_buffer_by_id(buffer_id)
                .unwrap()
                .text(),
            "![[renamed-image.png]] and ![[other.png]]"
        );
        assert_eq!(
            fs.read_if_exists(&source).unwrap().unwrap(),
            b"![[renamed-image.png]] and ![[other.png]]"
        );
    }

    #[test]
    fn renamed_open_note_autosaves_after_idle_tick() {
        let (mut session, fs, _temp) = test_session();
        let from = PathBuf::from("/test/vault/note.md");
        let to = PathBuf::from("/test/vault/autosaved.md");
        session.autosave_idle_interval = Duration::from_millis(5);
        let (buffer_id, revision) = match open_to_terminal(&session, from.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected open, got {other:?}"),
        };
        edit_append(&session, buffer_id, revision, "!");
        assert!(matches!(
            rename_to_terminal(&session, from, to.clone()),
            SessionResult::Success(CommandResult::NoteRenamed { .. })
        ));

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            session.pump();
            if fs.read_if_exists(&to).unwrap().as_deref() == Some(&b"hello!"[..]) {
                return;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        panic!("renamed open note was not autosaved under its new path");
    }

    #[test]
    fn t2_reconcile_rekeys_open_buffer_and_emits_suggestion() {
        let fs = MemFs::new();
        let root = PathBuf::from("/test/vault");
        fs.create_dir_all(&root).unwrap();
        let old_path = root.join("old-name.md");
        let new_path = root.join("new-name.md");
        fs.write_new(&old_path, b"---\nid: Stable-Note\n---\n# Original\n")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity = VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![1, 2, 3]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        actor.get_version().unwrap();
        let channels = ChannelSet::new();
        let session = Session::new(
            identity,
            actor.clone(),
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        let buffer_id = match open_to_terminal(&session, old_path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected open, got {other:?}"),
        };

        fs.remove(&old_path).unwrap();
        fs.write_new(&new_path, b"---\nid: stable-note\n---\n# Original\n")
            .unwrap();
        let response = actor
            .submit_reconcile(ReconcileScope::Full, ReconcileMode::VerifyContent)
            .unwrap()
            .recv_timeout(Duration::from_secs(5))
            .unwrap();
        assert!(matches!(
            response,
            crate::actor::VaultActorResp::ReconcileComplete { .. }
        ));
        session.pump();

        assert_eq!(session.get_buffer_id(&new_path), Some(buffer_id));
        assert_eq!(session.get_buffer_id(&old_path), None);
        assert!(session.drain_notices().iter().any(|notice| matches!(
            notice,
            crate::channels::Notice::RenameSuggested {
                from,
                to,
                tier: crate::channels::RenameTier::T2,
                ..
            } if from == &old_path && to == &new_path
        )));
        let _ = actor.send(crate::actor::VaultActorMsg::Shutdown);
    }

    #[test]
    fn autosave_failure_is_counted() {
        let (mut session, fs, _temp) = test_session();
        session.autosave_idle_interval = Duration::ZERO;
        let path = PathBuf::from("/test/vault/note.md");
        let (buffer_id, revision) = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected open, got {other:?}"),
        };
        edit_append(&session, buffer_id, revision, "!");
        let external = PathBuf::from("/test/vault/.external-save.md");
        fs.write_new(&external, b"external bytes").unwrap();
        fs.rename_replace(&external, &path).unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline && session.autosave_failures_total() == 0 {
            session.pump();
            std::thread::sleep(Duration::from_millis(2));
        }
        // M2 paydown (bounded, S6): include the Session backlog on stall.
        assert_eq!(
            session.autosave_failures_total(),
            1,
            "autosave failure never surfaced; events={:?} notices={:?}",
            session.drain_events(),
            session.drain_notices()
        );
    }

    #[test]
    fn rename_onto_open_destination_is_refused_preflight() {
        let (session, _fs, _temp) = test_session();
        let from = PathBuf::from("/test/vault/note.md");
        let to = PathBuf::from("/test/vault/destination.md");
        assert!(matches!(
            open_to_terminal(&session, to.clone()),
            SessionResult::Success(CommandResult::NoteOpened { .. })
        ));

        let result = session.execute(SessionCommand::RenameNote {
            from,
            to,
            strategy: Some(RenameStrategy::Rewrite),
        });

        assert!(
            matches!(result, SessionResult::Error { message } if message.starts_with("destination-open:"))
        );
    }

    #[test]
    fn session_rejects_invalid_lifecycle_operations_and_reports_unavailable_recovery() {
        let (mut session, _fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let missing = PathBuf::from("/test/vault/missing.md");
        let never_opened = PathBuf::from("/test/vault/never-opened.md");

        assert!(matches!(
            open_to_terminal(&session, missing.clone()),
            SessionResult::Success(CommandResult::NoteOpened { .. })
        ));
        assert!(matches!(
            session.execute(SessionCommand::CloseNote {
                path: never_opened.clone(),
                retain_recovery: false,
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::SaveNote { path: never_opened }),
            SessionResult::Error { .. }
        ));

        let (buffer_id, revision) = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("unexpected open result: {other:?}"),
        };
        assert!(matches!(
            open_to_terminal(&session, path.clone()),
            SessionResult::Success(CommandResult::NoteOpened { buffer_id: existing, .. }) if existing == buffer_id
        ));

        let bad_edit = session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(99), TextByte(99)),
                "x".into(),
            )],
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
        });
        assert!(matches!(bad_edit, SessionResult::Error { .. }));
        assert!(matches!(
            session.execute(SessionCommand::Undo {
                buffer_id,
                revision
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision
            }),
            SessionResult::Error { .. }
        ));

        let changed = session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(Span::new(TextByte(5), TextByte(5)), "!".into())],
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
        });
        let dirty_revision = match changed {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("unexpected edit result: {other:?}"),
        };
        assert!(matches!(
            session.execute(SessionCommand::CloseNote {
                path: path.clone(),
                retain_recovery: false,
            }),
            SessionResult::ActionRequired {
                code: "dirty-buffer-close",
                ..
            }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::StaleRevision { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::SaveAs {
                    new_path: PathBuf::from("copy.md")
                }
            }),
            SessionResult::ActionRequired {
                code: "save-as-confirm",
                ..
            }
        ));
        assert!(matches!(
            session.execute(SessionCommand::RestoreRecovery { path: path.clone() }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::DiscardRecovery { path }),
            SessionResult::Error { .. }
        ));
        session.recovery_stop.store(true, Ordering::Release);
        if let Some(worker) = session.recovery_worker.take() {
            worker.join().unwrap();
        }
        session.recovery_writer = None;
        assert!(session.recovery_tick().is_empty());
        assert!(session.flush_recovery().is_err());
        assert!(matches!(
            session.wait_for_operation(u64::MAX),
            SessionResult::Error { .. }
        ));
        assert!(dirty_revision > revision);
    }

    #[test]
    fn session_detects_external_save_conflict_and_supports_merge_reload_and_save_as() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let buffer_id = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected opened note, got {other:?}"),
        };
        let revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![EditOp::new(
                    Span::new(TextByte(5), TextByte(5)),
                    " mine".into()
                )],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::Success(_)
        ));

        fs.remove(&path).unwrap();
        fs.write_new(&path, b"hello theirs").unwrap();
        let saved = save_to_terminal(&session, path.clone());
        assert!(matches!(saved, SessionResult::Conflict { .. }), "{saved:?}");
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::Merge,
            }),
            SessionResult::MergeReady { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::SaveAs {
                    new_path: PathBuf::from("/test/vault/copy.md"),
                },
            }),
            SessionResult::ActionRequired {
                code: "save-as-confirm",
                ..
            }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::Reload,
            }),
            SessionResult::Success(CommandResult::ConflictResolved)
        ));
        assert_eq!(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .get_buffer_by_id(buffer_id)
                .unwrap()
                .model()
                .try_to_source_bytes()
                .unwrap(),
            b"hello theirs"
        );
    }

    #[test]
    fn session_restores_then_discards_note_keyed_recovery() {
        let (mut session, _fs, temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        let buffer_id = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected opened note, got {other:?}"),
        };
        session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id_mut(buffer_id)
            .unwrap()
            .apply_local_edit(EditOp::new(
                Span::new(TextByte(5), TextByte(5)),
                " recovered".into(),
            ));

        let recovery_dir = temp.path().join("recovery");
        let mut writer = RecoveryWriter::new(RecoveryConfig::new("test".into(), recovery_dir, 0));
        let recovery_file = flavite_buffer::RecoveryFile::from_buffer(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .get_buffer_by_id(buffer_id)
                .unwrap(),
            0,
            false,
        );
        writer.write_snapshot_now(b"note.md", recovery_file);
        session.recovery_writer = Some(Arc::new(Mutex::new(writer)));

        assert!(matches!(
            session.execute(SessionCommand::RestoreRecovery { path: path.clone() }),
            SessionResult::Success(CommandResult::RecoveryRestored { .. })
        ));
        assert_eq!(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .get_buffer_by_id(buffer_id)
                .unwrap()
                .model()
                .try_to_source_bytes()
                .unwrap(),
            b"hello recovered"
        );
        assert!(matches!(
            session.execute(SessionCommand::DiscardRecovery { path: path.clone() }),
            SessionResult::Success(CommandResult::ConflictResolved)
        ));
        assert!(matches!(
            session.execute(SessionCommand::DiscardRecovery { path }),
            SessionResult::Error { .. }
        ));
    }

    #[test]
    fn session_handles_absent_notes_stale_edits_and_empty_undo_redo() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/new.md");
        let buffer_id = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected opened absent note, got {other:?}"),
        };
        let revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::Undo {
                buffer_id,
                revision
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::Redo {
                buffer_id,
                revision
            }),
            SessionResult::Error { .. }
        ));
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision: Revision::new(revision.as_u64() + 1),
                ops: vec![],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::StaleRevision { .. }
        ));
        let request_id = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("save must be admitted asynchronously: {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(request_id),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(fs.read_if_exists(&path).unwrap().unwrap(), b"");
        assert!(matches!(
            session.execute(SessionCommand::SaveNote {
                path: PathBuf::from("/test/vault/closed.md")
            }),
            SessionResult::Error { .. }
        ));
    }

    #[test]
    fn conflict_resolution_handles_missing_buffers_deleted_files_and_keep_mine() {
        let (session, fs, _temp) = test_session();
        let path = PathBuf::from("/test/vault/note.md");
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::KeepMine,
            }),
            SessionResult::Error { .. }
        ));
        let buffer_id = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened { buffer_id, .. }) => buffer_id,
            other => panic!("expected opened note, got {other:?}"),
        };
        fs.remove(&path).unwrap();
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::Reload,
            }),
            SessionResult::ActionRequired {
                code: "file-deleted",
                ..
            }
        ));
        fs.write_new(&path, b"external").unwrap();
        let revision = session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(0), TextByte(0)),
                "mine ".into(),
            )],
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
        });
        session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id_mut(buffer_id)
            .unwrap()
            .check_external_change(b"theirs", DecodePolicy::Auto);
        assert!(matches!(
            session.execute(SessionCommand::ResolveConflict {
                path: path.clone(),
                resolution: ConflictResolution::KeepMine,
            }),
            SessionResult::Success(CommandResult::ConflictResolved)
        ));
        assert_eq!(
            session
                .buffer_manager()
                .lock()
                .unwrap()
                .get_buffer_by_id(buffer_id)
                .unwrap()
                .state(),
            BufferState::Dirty
        );
    }

    #[test]
    fn save_command_is_pending_until_disk_and_index_complete() {
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("/vault/notes")).unwrap();
        fs.write_new(Path::new("/vault/notes/a.md"), b"# Base")
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("index.sqlite3");
        drop(flavite_index::open_or_init(&db_path).unwrap());
        let index = IndexActor::new(&db_path, 1).unwrap();
        let identity =
            VaultIdentity::new(Uuid::now_v7(), PathBuf::from("/vault"), vec![1, 2, 3, 4]);
        let actor = crate::actor::VaultActorBuilder::new()
            .with_fs(Box::new(fs.clone()) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(index)
            .build()
            .unwrap()
            .start()
            .unwrap();
        let session = Session::new(
            identity,
            actor.clone(),
            BufferManager::default(),
            PulseChannel::new(8),
            NoticeChannel::new(Duration::from_millis(10)),
            VaultConfig::default(),
        );
        let path = PathBuf::from("/vault/notes/a.md");
        let (buffer_id, revision) = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("unexpected open result: {other:?}"),
        };
        let applied = session.execute(SessionCommand::ApplyEdits {
            buffer_id,
            revision,
            ops: vec![EditOp::new(
                Span::new(TextByte(6), TextByte(6)),
                " edited".into(),
            )],
            undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
        });
        let new_revision = match applied {
            SessionResult::Success(CommandResult::EditsApplied { new_revision, .. }) => {
                new_revision
            }
            other => panic!("unexpected edit result: {other:?}"),
        };
        assert!(new_revision > revision);
        assert!(matches!(
            session.execute(SessionCommand::CloseNote {
                path: path.clone(),
                retain_recovery: false,
            }),
            SessionResult::ActionRequired {
                code: "dirty-buffer-close",
                ..
            }
        ));
        let request_id = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("save must be admitted asynchronously: {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(request_id),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(
            actor.read_file(path.clone()).unwrap().unwrap(),
            b"# Base edited"
        );
        let current_revision = session
            .buffer_manager
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .revision();
        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision: current_revision,
                ops: vec![EditOp::new(
                    Span::new(TextByte(13), TextByte(13)),
                    " mine".into(),
                )],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        fs.remove(&path).unwrap();
        fs.write_new(&path, b"# External").unwrap();
        let (merge_revision, merge_disk_hash) = match session.prepare_merge(path.clone()) {
            SessionResult::MergeReady {
                buffer_id: preview_buffer,
                revision,
                disk_hash,
                result,
                ..
            } => {
                assert_eq!(preview_buffer, buffer_id);
                assert!(result.merged.is_none());
                (revision, disk_hash)
            }
            other => panic!("unexpected merge preview: {other:?}"),
        };
        assert!(matches!(
            session.execute(SessionCommand::ApplyMerge {
                path: path.clone(),
                buffer_id,
                revision: merge_revision,
                disk_hash: merge_disk_hash,
                merged_bytes: b"# Merged".to_vec(),
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        let merged_base = session
            .buffer_manager
            .lock()
            .unwrap()
            .get_buffer_by_id(buffer_id)
            .unwrap()
            .base_bytes()
            .to_vec();
        assert_eq!(merged_base, b"# External");
        let merge_save_id = match session.execute(SessionCommand::SaveNote { path: path.clone() }) {
            SessionResult::Pending { request_id } => request_id,
            other => panic!("merged buffer save must be pending: {other:?}"),
        };
        assert!(matches!(
            session.wait_for_operation(merge_save_id),
            SessionResult::Success(CommandResult::NoteSaved { .. })
        ));
        assert_eq!(actor.read_file(path.clone()).unwrap().unwrap(), b"# Merged");
        assert!(matches!(
            session.execute(SessionCommand::CloseNote {
                path,
                retain_recovery: false,
            }),
            SessionResult::Success(CommandResult::NoteClosed)
        ));
        assert!(session.drain_operation_completions().is_empty());
    }

    #[test]
    fn graph_buffer_watch_advertises_stamp_transitions() {
        // RFC-0005 S6 seam: the session watch is the handle the graph
        // service's debounced arm reads. The session's internal receiver
        // never actively waits, so a spy's recv deterministically gets the
        // next wake.
        let (session, _fs, _temp) = test_session();
        let watch = session.graph_buffer_watch();
        let version = session.graph_stamp_version();
        assert_eq!(watch.recv_timeout(Duration::from_millis(20)), None);

        let path = PathBuf::from("/test/vault/note.md");
        let (buffer_id, revision) = match open_to_terminal(&session, path.clone()) {
            SessionResult::Success(CommandResult::NoteOpened {
                buffer_id,
                revision,
            }) => (buffer_id, revision),
            other => panic!("expected open, got {other:?}"),
        };
        assert!(session.graph_stamp_version() > version, "open bumps");
        assert_eq!(
            watch.recv_timeout(Duration::from_secs(1)),
            Some(()),
            "open wakes"
        );

        assert!(matches!(
            session.execute(SessionCommand::ApplyEdits {
                buffer_id,
                revision,
                ops: vec![EditOp::new(Span::new(TextByte(5), TextByte(5)), "!".into())],
                undo_policy: flavite_buffer::undo::UndoPolicy::Typing,
            }),
            SessionResult::Success(CommandResult::EditsApplied { .. })
        ));
        assert_eq!(
            watch.recv_timeout(Duration::from_secs(1)),
            Some(()),
            "the edit wake lands inside the debounce window"
        );
    }
}
