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
use crate::save::{DiskPrecondition, DurabilityOutcome, SaveFailureStage, SaveRequest};
use crate::types::{
    Epoch, Generation, ReconcileMode, ReconcileScope, ReconcileSummary, RenameStrategy,
    VaultConfig, VaultIdentity,
};
use flavite_buffer::manager::{BufferManager, BufferStateChanged};
use flavite_buffer::recovery::{RecoveryConfig, RecoveryWriter};
use flavite_buffer::revision::{BaseHash, BufferId, Revision};
use flavite_buffer::undo::EditOp;
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
}

/// A correlated D9 rename flight: source, destination, and the rename
/// worker's reply channel.
type RenameFlight = (
    PathBuf,
    PathBuf,
    crossbeam::channel::Receiver<crate::actor::VaultActorResp>,
);

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
    journal_applied: Mutex<HashMap<(String, u64), Result<u64, String>>>,
    cancelled_journal_items: Mutex<HashSet<(String, u64)>>,
    /// Lossless terminal results. Admission is bounded by the pending-save cap.
    completions: Mutex<VecDeque<OperationCompletion>>,
    /// Pulse channel.
    pulse: PulseChannel,
    /// Notice channel.
    notice: NoticeChannel,
    /// Journal recovery findings waiting for the first host pump.
    recovery_notices: Mutex<VecDeque<&'static str>>,
    /// Semantic link plan store (W05). Plans are process-local capabilities.
    link_plans: std::sync::Arc<Mutex<crate::link::LinkPlanStore>>,
    /// Bounded prepare workers' job queue.
    link_jobs: Option<crossbeam::channel::Sender<LinkJob>>,
    /// Completed prepare results awaiting installation on a Session turn.
    link_results: crossbeam::channel::Receiver<LinkOutcomeMessage>,
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

impl Session {
    /// Create a new Session.
    pub fn new(
        identity: VaultIdentity,
        actor: VaultActorHandle,
        mut buffer_manager: BufferManager,
        pulse: PulseChannel,
        notice: NoticeChannel,
        config: VaultConfig,
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
        let recovery_writer = crate::identity::get_data_dir().map(|data| {
            let dir = data
                .join("flavite")
                .join("vaults")
                .join(identity.uuid.to_string())
                .join("recovery");
            Arc::new(Mutex::new(RecoveryWriter::new(RecoveryConfig::new(
                identity.uuid.to_string(),
                dir,
                manager.lock().unwrap().recovery_interval(),
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
                        let buffer_manager = manager.lock().unwrap();
                        let dirty = buffer_manager
                            .dirty_buffers()
                            .into_iter()
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
                            let due = writer.lock().unwrap().should_write_snapshot(&snapshot);
                            if due && actor.pin_merge_base(key.clone(), base).is_ok() {
                                writer.lock().unwrap().write_snapshot(&key, snapshot);
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
                            writer.lock().unwrap().read_for_note_key(&job.note_key)
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
                                        let mut store = link_plans.lock().unwrap();
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
                                    link_plans.lock().unwrap().fail_prepare(job.request.intent);
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
        Self {
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
            save_flights: Mutex::new(HashMap::new()),
            reconcile_flights: Mutex::new(HashMap::new()),
            rename_flights: Mutex::new(HashMap::new()),
            journal_applied: Mutex::new(HashMap::new()),
            cancelled_journal_items: Mutex::new(HashSet::new()),
            completions: Mutex::new(VecDeque::new()),
            pulse,
            notice,
            recovery_notices: Mutex::new(VecDeque::new()),
            link_plans,
            link_jobs: Some(link_job_sender),
            link_results: link_result_receiver,
        }
    }

    /// Generate a new request ID.
    fn next_request_id(&self) -> u64 {
        self.request_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Execute a command.
    pub fn execute(&self, command: SessionCommand) -> SessionResult {
        let _turn = self.driving_turn.lock().unwrap();
        self.pump_external_changes_inner();
        let edited_path = match &command {
            SessionCommand::EditStructure { buffer_id, .. }
            | SessionCommand::ApplyEdits { buffer_id, .. }
            | SessionCommand::Undo { buffer_id, .. }
            | SessionCommand::Redo { buffer_id, .. } => self
                .buffer_manager
                .lock()
                .unwrap()
                .path_for_id(*buffer_id)
                .cloned(),
            SessionCommand::LinkInsert { intent, plan_id }
            | SessionCommand::LinkRemove { intent, plan_id } => {
                let source = self
                    .link_plans
                    .lock()
                    .unwrap()
                    .source_for(*intent, *plan_id);
                source.and_then(|id| {
                    self.buffer_manager
                        .lock()
                        .unwrap()
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
                .lock()
                .unwrap()
                .insert(path.clone(), std::time::Instant::now());
            self.autosave_attempted.lock().unwrap().remove(&path);
        }
        result
    }

    /// Apply queued disk observations on the host pump. GUI hosts call this
    /// from their regular timer; command execution also drains before running.
    pub fn pump(&self) {
        let _turn = self.driving_turn.lock().unwrap();
        self.pump_external_changes_inner();
        self.pump_recovery_notices();
        self.pump_structure_completions();
        self.pump_link_completions();
        self.pump_open_completions();
        self.pump_save_completions();
        self.pump_reconcile_completions();
        self.pump_rename_completions();
        self.pump_autosave();
        // Recovery checkpoints run on the dedicated worker created with the
        // Session. Never perform recovery filesystem or actor I/O while this
        // host pump owns the Session driving-turn lock.
    }

    pub(crate) fn structure_completions_full(&self) -> bool {
        self.completions.lock().unwrap().len() >= MAX_PENDING_SAVE_OPERATIONS - 64
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
                    .lock()
                    .unwrap()
                    .path_for_id(prepared.buffer)
                    .cloned()
            {
                self.last_edit
                    .lock()
                    .unwrap()
                    .insert(path.clone(), std::time::Instant::now());
                self.autosave_attempted.lock().unwrap().remove(&path);
            }
            self.completions
                .lock()
                .unwrap()
                .push_back(OperationCompletion {
                    request_id: prepared.request,
                    result,
                });
            self.structure_worker.retired();
        }
    }

    fn pump_link_completions(&self) {
        while let Ok(message) = self.link_results.try_recv() {
            self.completions
                .lock()
                .unwrap()
                .push_back(OperationCompletion {
                    request_id: message.request_id,
                    result: message.result,
                });
        }
    }

    fn pump_recovery_notices(&self) {
        let codes = self
            .recovery_notices
            .lock()
            .unwrap()
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
            .lock()
            .unwrap()
            .dirty_buffers()
            .into_iter()
            .filter(|(_, buffer)| buffer.allows_autosave())
            .map(|(path, buffer)| (path.clone(), buffer.buffer_id(), buffer.revision()))
            .collect::<Vec<_>>();
        for (path, buffer_id, revision) in dirty {
            let last_edit = {
                let mut deadlines = self.last_edit.lock().unwrap();
                *deadlines.entry(path.clone()).or_insert(now)
            };
            if now.duration_since(last_edit) < self.autosave_idle_interval
                || self.autosave_attempted.lock().unwrap().get(&path) == Some(&revision)
                || self
                    .save_flights
                    .lock()
                    .unwrap()
                    .contains_key(&buffer_id.as_u64())
            {
                continue;
            }
            self.autosave_attempted
                .lock()
                .unwrap()
                .insert(path.clone(), revision);
            let result = self.save_note(path.clone());
            match result {
                SessionResult::Pending { request_id } => {
                    self.autosave_requests.lock().unwrap().insert(request_id);
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
                    self.session_events.lock().unwrap().push_back(
                        SessionEvent::ExternalChangeDetected {
                            path: path.clone(),
                            deleted: false,
                        },
                    );
                    let mut manager = self.buffer_manager.lock().unwrap();
                    if manager.get_buffer(&path).is_some_and(|buffer| {
                        buffer.state() == flavite_buffer::state::BufferState::Orphaned
                    }) {
                        manager.notify_file_recreated(&path, &bytes);
                    } else {
                        manager.notify_external_change(&path, &bytes);
                    }
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
                    self.session_events.lock().unwrap().push_back(
                        SessionEvent::ExternalChangeDetected {
                            path: path.clone(),
                            deleted: true,
                        },
                    );
                    let mut manager = self.buffer_manager.lock().unwrap();
                    if !manager.notify_file_deleted(&path).is_empty() {
                        self.notice.send(crate::channels::Notice::ActionRequired {
                            code: "external_delete",
                            note: Some(flavite_buffer::manager::NoteRef::new(path)),
                        });
                    }
                }
                crate::actor::VaultActorEvent::Renamed { from, to, .. } => {
                    let mut manager = self.buffer_manager.lock().unwrap();
                    if manager.rename_buffer(&from, to.clone()) {
                        drop(manager);
                        {
                            let mut last_edit = self.last_edit.lock().unwrap();
                            if let Some(edit_time) = last_edit.remove(&from) {
                                last_edit.insert(to.clone(), edit_time);
                            }
                        }
                        {
                            let mut attempted = self.autosave_attempted.lock().unwrap();
                            if let Some(revision) = attempted.remove(&from) {
                                attempted.insert(to.clone(), revision);
                            }
                        }
                        {
                            let mut absent = self.absent_on_open.lock().unwrap();
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
            if let Some(result) = self.journal_applied.lock().unwrap().get(&key).cloned() {
                let _ = request.reply.send(result);
                continue;
            }
            let result = self.apply_buffer_patch(&request);
            if let Ok(revision) = result {
                self.journal_applied
                    .lock()
                    .unwrap()
                    .insert(key, Ok(revision));
            } else {
                self.journal_applied
                    .lock()
                    .unwrap()
                    .insert(key, result.clone());
            }
            let _ = request.reply.send(result);
        }
    }

    fn apply_buffer_patch(
        &self,
        request: &crate::actor::ApplyBufferPatchRequest,
    ) -> Result<u64, String> {
        let buffer_id = BufferId::from(request.buffer_id);
        let mut manager = self.buffer_manager.lock().unwrap();
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
            } => self.apply_edits(buffer_id, revision, ops),
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
    pub fn note_catalog(&self, limit: usize) -> Result<flavite_index::NoteCatalogPage, String> {
        self.actor.note_catalog(limit)
    }

    /// Return the current indexed note-title and alias snapshot for UI
    /// autocomplete. This is a read-only request to the VaultActor.
    pub fn completion_candidates(
        &self,
    ) -> Result<Vec<flavite_index::types::CompletionCandidate>, String> {
        self.actor.completion_candidates()
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

    /// Execute a text or hybrid query on the index actor.
    pub fn search_notes(
        &self,
        query: flavite_index::query::Query,
        limit: usize,
    ) -> Result<flavite_index::SearchPage, String> {
        self.actor.search_notes(query, limit)
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
        self.link_plans.lock().unwrap().register_client(token);
        token
    }

    /// Cancel an uncommitted gesture; late worker results cannot revive it.
    pub fn cancel_link_intent(&self, intent: crate::link::LinkIntent) {
        self.link_plans.lock().unwrap().cancel(intent);
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
            .lock()
            .unwrap()
            .source_for(intent, plan_id)
            .map(BufferId::from)
    }
    /// Native path for an open buffer without cloning note contents.
    pub fn buffer_path(&self, buffer: BufferId) -> Option<PathBuf> {
        self.buffer_manager
            .lock()
            .unwrap()
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
            let mut store = self.link_plans.lock().unwrap();
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
                        self.link_plans.lock().unwrap().fail_prepare(intent);
                        return SessionResult::LinkRejected {
                            reason: crate::link::LinkInsertionError::Busy,
                        };
                    }
                    SessionResult::Pending { request_id }
                }
                None => {
                    self.link_plans.lock().unwrap().fail_prepare(request.intent);
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
            let mut store = self.link_plans.lock().unwrap();
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
            .lock()
            .unwrap()
            .get_buffer_by_id(BufferId::from(plan.source.buffer_id))
            .map(|buffer| (buffer.state(), buffer.revision().as_u64()));
        let Some((state, actual_revision)) = state else {
            let mut store = self.link_plans.lock().unwrap();
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
            self.link_plans.lock().unwrap().record_outcome(
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
            self.link_plans.lock().unwrap().record_outcome(
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
            self.link_plans.lock().unwrap().record_outcome(
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
            self.link_plans.lock().unwrap().record_outcome(
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
        self.link_plans.lock().unwrap().record_outcome(
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
        let _turn = self.driving_turn.lock().unwrap();
        self.pump_structure_completions();
        self.pump_link_completions();
        self.pump_open_completions();
        self.pump_save_completions();
        self.pump_reconcile_completions();
        self.pump_rename_completions();
        self.completions.lock().unwrap().drain(..).collect()
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
        let _turn = self.driving_turn.lock().unwrap();
        self.pump_external_changes_inner();
        self.session_events.lock().unwrap().drain(..).collect()
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
    pub fn wait_for_operation(&self, request_id: u64) -> SessionResult {
        let _turn = self.driving_turn.lock().unwrap();
        self.wait_for_operation_holding_turn(request_id)
    }

    /// Wait for an operation while the driving turn is already held (W12:
    /// `restore_recovery`'s implicit open runs inside `execute`, which
    /// holds the turn; re-acquiring here would self-deadlock).
    fn wait_for_operation_holding_turn(&self, request_id: u64) -> SessionResult {
        loop {
            self.pump_external_changes_inner();
            self.pump_open_completions();
            self.pump_save_completions();
            self.pump_reconcile_completions();
            self.pump_rename_completions();
            let completed = {
                let mut completions = self.completions.lock().unwrap();
                completions
                    .iter()
                    .position(|completion| completion.request_id == request_id)
                    .map(|position| completions.remove(position).unwrap().result)
            };
            if let Some(result) = completed {
                return result;
            }
            let active = self.save_flights.lock().unwrap().values().any(|flight| {
                flight.request.request_id == request_id
                    || flight
                        .queued
                        .as_ref()
                        .is_some_and(|request| request.request_id == request_id)
            }) || self
                .reconcile_flights
                .lock()
                .unwrap()
                .contains_key(&request_id)
                || self
                    .rename_flights
                    .lock()
                    .unwrap()
                    .contains_key(&request_id)
                || self
                    .open_flights
                    .lock()
                    .unwrap()
                    .values()
                    .any(|flight| flight.request_ids.contains(&request_id));
            if !active {
                return SessionResult::Error {
                    message: format!("unknown or already-drained operation {request_id}"),
                };
            }
            // The caller holds the driving turn for the whole wait; only
            // the sleep is released so concurrent pumps on other threads
            // can make progress against the actor.
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn pump_save_completions(&self) {
        let ready = {
            let mut flights = self.save_flights.lock().unwrap();
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
                .lock()
                .unwrap()
                .remove(&flight.request.request_id)
                && !matches!(&operation_result, SessionResult::Success(_))
            {
                self.record_autosave_failure(&flight.request.path, &operation_result);
            }
            self.completions
                .lock()
                .unwrap()
                .push_back(OperationCompletion {
                    request_id: flight.request.request_id,
                    result: operation_result,
                });

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
                        .lock()
                        .unwrap()
                        .get_buffer_by_id(BufferId::from(buffer_id))
                        .is_some();
                    if !buffer_is_live {
                        self.completions
                            .lock()
                            .unwrap()
                            .push_back(OperationCompletion {
                                request_id: queued.request_id,
                                result: SessionResult::Error {
                                    message: "queued save's buffer was closed before admission"
                                        .into(),
                                },
                            });
                        continue;
                    }
                    match self.actor.submit_save(queued.clone()) {
                        Ok(response) => {
                            self.save_flights.lock().unwrap().insert(
                                buffer_id,
                                SaveFlight {
                                    request: queued,
                                    response,
                                    queued: None,
                                },
                            );
                        }
                        Err(error) => {
                            self.completions
                                .lock()
                                .unwrap()
                                .push_back(OperationCompletion {
                                    request_id: queued.request_id,
                                    result: SessionResult::Error {
                                        message: error.to_string(),
                                    },
                                })
                        }
                    }
                }
                _ => self
                    .completions
                    .lock()
                    .unwrap()
                    .push_back(OperationCompletion {
                        request_id: queued.request_id,
                        result: SessionResult::Error {
                            message: format!(
                                "queued save was not admitted because save {} did not complete",
                                flight.request.request_id
                            ),
                        },
                    }),
            }
        }
    }

    fn pump_reconcile_completions(&self) {
        let ready = {
            let mut flights = self.reconcile_flights.lock().unwrap();
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
            self.completions
                .lock()
                .unwrap()
                .push_back(OperationCompletion { request_id, result });
        }
    }

    fn pump_rename_completions(&self) {
        let ready = {
            let mut flights = self.rename_flights.lock().unwrap();
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
            self.completions
                .lock()
                .unwrap()
                .push_back(OperationCompletion { request_id, result });
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
                let mut manager = self.buffer_manager.lock().unwrap();
                if let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) {
                    buffer.handle_save_completed_revision(revision, &request.bytes);
                }
                drop(manager);
                if let Some(writer) = &self.recovery_writer {
                    let mut writer = writer.lock().unwrap();
                    writer.retire_covered(buffer_id, revision);
                }
                self.absent_on_open.lock().unwrap().remove(&path);
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
                        .lock()
                        .unwrap()
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
                    let mut manager = self.buffer_manager.lock().unwrap();
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
                        self.absent_on_open.lock().unwrap().remove(&path);
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
            .lock()
            .unwrap()
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
            let flights = self.open_flights.lock().unwrap();
            if flights.len() >= MAX_PENDING_OPEN_NOTES {
                return SessionResult::Busy;
            }
        }
        let request_id = self.next_request_id();
        let mut flights = self.open_flights.lock().unwrap();
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
        let (buffer_id, revision) = {
            let mut buffer_manager_guard = self.buffer_manager.lock().unwrap();
            let (buffer, _transition) = buffer_manager_guard.open_note(read.path.clone(), &content);
            (buffer.buffer_id(), buffer.revision())
        };

        if let Some(recovery) = read.recovery {
            let recovered = recovery.to_source_bytes();
            if blake3::hash(&recovered).as_bytes() != blake3::hash(&content).as_bytes() {
                self.notice.send(crate::channels::Notice::RecoveryOffer {
                    note: flavite_buffer::manager::NoteRef::new(read.path.clone()),
                });
            }
        }
        if absent {
            self.absent_on_open
                .lock()
                .unwrap()
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

    fn pump_open_completions(&self) {
        let mut ready = Vec::new();
        while let Ok(read) = self.open_results.try_recv() {
            ready.push(read);
        }
        if ready.is_empty() {
            return;
        }
        let mut completions = self.completions.lock().unwrap();
        for read in ready {
            let Some(flight) = self.open_flights.lock().unwrap().remove(&read.path) else {
                continue;
            };
            let result = self.install_open_read(read);
            for request_id in flight.request_ids {
                completions.push_back(OperationCompletion {
                    request_id,
                    result: result.clone(),
                });
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
            .lock()
            .unwrap()
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
        // Get buffer ID
        let buffer_id = {
            self.buffer_manager
                .lock()
                .unwrap()
                .get_buffer(&path)
                .map(|buffer| buffer.buffer_id())
        };

        if let Some(_buffer_id) = buffer_id {
            if self
                .save_flights
                .lock()
                .unwrap()
                .contains_key(&_buffer_id.as_u64())
            {
                return SessionResult::ActionRequired {
                    code: "save-pending",
                    data: Some(path.display().to_string()),
                };
            }
            let unsaved = self
                .buffer_manager
                .lock()
                .unwrap()
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
            let mut buffer_manager = self.buffer_manager.lock().unwrap();
            buffer_manager.close_note(&path);
            drop(buffer_manager);
            self.shared_snapshots.lock().unwrap().remove(&_buffer_id);

            self.absent_on_open.lock().unwrap().remove(&path);

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
            let manager = self.buffer_manager.lock().unwrap();
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
        let _ = writer.lock().unwrap().write_snapshot_now(&key, snapshot);
        let stored = writer
            .lock()
            .unwrap()
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
            let manager = self.buffer_manager.lock().unwrap();
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
        let Some(buffer_id) = self.get_buffer_id(&path) else {
            return SessionResult::Error {
                message: "Buffer not found".into(),
            };
        };
        if self
            .save_flights
            .lock()
            .unwrap()
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
            let manager = self.buffer_manager.lock().unwrap();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => buffer.model().to_source_bytes(),
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
            let mut manager = self.buffer_manager.lock().unwrap();
            match manager
                .get_buffer_by_id_mut(buffer_id)
                .and_then(|buffer| buffer.reload_from_disk(&theirs))
            {
                Some(_) => {}
                None => {
                    return SessionResult::Error {
                        message: "disk bytes are not valid UTF-8".into(),
                    };
                }
            }
        }
        // The retained recovery copy holds the same discarded edits; keep
        // them from re-appearing as a recovery offer on the next open.
        if let Some(writer) = &self.recovery_writer {
            let key = native_note_key(&path, &self.identity.root);
            writer.lock().unwrap().delete_for_note_key(&key);
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
        let mut manager = self.buffer_manager.lock().unwrap();
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
    ) -> SessionResult {
        let mut buffer_manager = self.buffer_manager.lock().unwrap();

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
            // authoritative Buffer transition/undo unit.
            let Some((new_revision, _)) = buffer.apply_local_edits(&ops) else {
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
        let buffer_id = {
            self.buffer_manager
                .lock()
                .unwrap()
                .get_buffer(&path)
                .map(|buffer| buffer.buffer_id())
        };
        let Some(buffer_id) = buffer_id else {
            return SessionResult::Error {
                message: "Buffer not found".to_string(),
            };
        };
        let (bytes, revision, base_hash, base_bytes, state) = {
            let mut manager = self.buffer_manager.lock().unwrap();
            let Some(buffer) = manager.get_buffer_by_id_mut(buffer_id) else {
                return SessionResult::Error {
                    message: "Buffer not found".into(),
                };
            };
            buffer.model_mut().seal_undo_group();
            (
                buffer.model().to_source_bytes(),
                buffer.revision(),
                buffer.base_hash(),
                buffer.base_bytes().to_vec(),
                buffer.state(),
            )
        };
        if !state.allows_manual_save() {
            return SessionResult::Conflict { path };
        }
        let precondition = if self.absent_on_open.lock().unwrap().contains(&path) {
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

        let mut flights = self.save_flights.lock().unwrap();
        let outstanding = self.completions.lock().unwrap().len()
            + flights
                .values()
                .map(|flight| 1 + usize::from(flight.queued.is_some()))
                .sum::<usize>();
        if outstanding + self.reconcile_flights.lock().unwrap().len() >= MAX_PENDING_SAVE_OPERATIONS
        {
            return SessionResult::Busy;
        }
        if let Some(flight) = flights.get_mut(&buffer_id.as_u64()) {
            if let Some(superseded) = flight.queued.replace(request) {
                self.completions
                    .lock()
                    .unwrap()
                    .push_back(OperationCompletion {
                        request_id: superseded.request_id,
                        result: SessionResult::Superseded {
                            request_id: superseded.request_id,
                            by_request: request_id,
                        },
                    });
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
        let flights = self.save_flights.lock().unwrap();
        let outstanding = self.completions.lock().unwrap().len()
            + flights
                .values()
                .map(|flight| 1 + usize::from(flight.queued.is_some()))
                .sum::<usize>()
            + self.reconcile_flights.lock().unwrap().len();
        drop(flights);
        if outstanding >= MAX_PENDING_SAVE_OPERATIONS {
            return SessionResult::Busy;
        }
        match self.actor.submit_reconcile(scope, mode) {
            Ok(response) => {
                self.reconcile_flights
                    .lock()
                    .unwrap()
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
        if self.buffer_manager.lock().unwrap().has_buffer(&to) {
            return SessionResult::Error {
                message: format!("destination-open: {}", to.display()),
            };
        }
        let (buffer_ids, buffer_revisions) = {
            let manager = self.buffer_manager.lock().unwrap();
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
        let outstanding = self.completions.lock().unwrap().len()
            + self.save_flights.lock().unwrap().len()
            + self.reconcile_flights.lock().unwrap().len()
            + self.rename_flights.lock().unwrap().len();
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
                    .lock()
                    .unwrap()
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
        let mut buffer_manager = self.buffer_manager.lock().unwrap();

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
                    self.cancelled_journal_items
                        .lock()
                        .unwrap()
                        .insert((tag.txn_id, tag.item_id));
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
        let mut buffer_manager = self.buffer_manager.lock().unwrap();

        // Get the buffer and call redo on its model
        if let Some(buffer) = buffer_manager.get_buffer_by_id_mut(buffer_id) {
            if buffer.revision() != revision {
                return SessionResult::StaleRevision {
                    buffer_id,
                    expected: revision,
                    actual: buffer.revision(),
                };
            }
            if buffer
                .model()
                .next_redo_ops()
                .iter()
                .filter_map(|op| op.journal.as_ref())
                .any(|tag| {
                    self.cancelled_journal_items
                        .lock()
                        .unwrap()
                        .contains(&(tag.txn_id.clone(), tag.item_id))
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
        let mut manager = self.buffer_manager.lock().unwrap();
        match manager
            .get_buffer_by_id_mut(buffer_id)
            .and_then(|buffer| buffer.resolve_keep_local(&theirs).map(|_| ()))
        {
            Some(()) => SessionResult::Success(CommandResult::ConflictResolved),
            None => SessionResult::Error {
                message: "external bytes are not valid UTF-8".into(),
            },
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
            let manager = self.buffer_manager.lock().unwrap();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => buffer.model().to_source_bytes(),
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
        let mut manager = self.buffer_manager.lock().unwrap();
        match manager
            .get_buffer_by_id_mut(buffer_id)
            .and_then(|buffer| buffer.reload_from_disk(&theirs))
        {
            Some(_) => SessionResult::Success(CommandResult::ConflictResolved),
            None => SessionResult::Error {
                message: "external bytes are not valid UTF-8".into(),
            },
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
        let (base, mine, revision) = {
            let manager = self.buffer_manager.lock().unwrap();
            match manager.get_buffer_by_id(buffer_id) {
                Some(buffer) => (
                    buffer.base_bytes().to_vec(),
                    buffer.model().to_source_bytes(),
                    buffer.revision(),
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
        let (Ok(base), Ok(mine), Ok(theirs)) = (
            std::str::from_utf8(&base),
            std::str::from_utf8(&mine),
            std::str::from_utf8(&theirs),
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
            disk_hash: *blake3::hash(theirs.as_bytes()).as_bytes(),
            result: crate::merge::three_way_merge(base, mine, theirs),
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
        let Some(theirs) = self.actor.read_file(path.clone()).ok().flatten() else {
            return SessionResult::ActionRequired {
                code: "file-deleted",
                data: Some(path.display().to_string()),
            };
        };
        if blake3::hash(&theirs).as_bytes() != &expected_disk_hash {
            if let Some(buffer) = self
                .buffer_manager
                .lock()
                .unwrap()
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
            let manager = self.buffer_manager.lock().unwrap();
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
            buffer.model().to_source_bytes()
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
        let mut manager = self.buffer_manager.lock().unwrap();
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
        match buffer.apply_resolved_merge(&merged_bytes, &theirs) {
            Some(new_revision) => SessionResult::Success(CommandResult::EditsApplied {
                buffer_id,
                new_revision,
            }),
            None => SessionResult::Error {
                message: "merge output is invalid UTF-8 or exceeds the note size limit".into(),
            },
        }
    }

    fn restore_recovery(&self, path: PathBuf) -> SessionResult {
        let Some(writer) = &self.recovery_writer else {
            return SessionResult::Error {
                message: "recovery storage is unavailable".into(),
            };
        };
        let key = native_note_key(&path, &self.identity.root);
        let Some(recovery) = writer.lock().unwrap().read_for_note_key(&key) else {
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
        let recovered = recovery.to_source_bytes();
        let transition = self
            .buffer_manager
            .lock()
            .unwrap()
            .get_buffer_by_id_mut(buffer_id)
            .and_then(|buffer| buffer.restore_recovery(&recovered, &disk, recovery.base_hash));
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
            .lock()
            .unwrap()
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
        if writer.lock().unwrap().delete_for_note_key(&key) {
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
        // The buffer for `path` must be the exact buffer the caller saw:
        // restoring into a buffer belonging to another note is refused.
        {
            let manager = self.buffer_manager.lock().unwrap();
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
        let Ok(text) = String::from_utf8(snapshot) else {
            return SessionResult::Error {
                message: "history snapshot is not valid UTF-8".into(),
            };
        };
        let mut manager = self.buffer_manager.lock().unwrap();
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
        // surrounding typing.
        buffer.model_mut().seal_undo_group();
        let end = buffer.model().to_source_bytes().len();
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
        let manager = self.buffer_manager.lock().unwrap();
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
        let _turn = self.driving_turn.lock().unwrap();
        self.shared_buffer_snapshot_inner(buffer_id, revision)
    }
    pub(crate) fn shared_buffer_snapshot_inner(
        &self,
        buffer_id: BufferId,
        revision: Option<Revision>,
    ) -> Result<SharedBufferSnapshot, BufferSnapshotError> {
        let manager = self.buffer_manager.lock().unwrap();
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
            let mut cache = self.shared_snapshots.lock().unwrap();
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
        let manager = self.buffer_manager.lock().unwrap();
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
            let due = writer.lock().unwrap().should_write_snapshot(&snapshot);
            if due
                && self.actor.pin_merge_base(key.clone(), base).is_ok()
                && let Some(path) = writer.lock().unwrap().write_snapshot(&key, snapshot)
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
            let manager = self.buffer_manager.lock().unwrap();
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
                .lock()
                .unwrap()
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
            .lock()
            .unwrap()
            .has_buffer(&path.to_path_buf())
    }

    /// Get the buffer ID for an open path.
    #[must_use]
    pub fn get_buffer_id(&self, path: &Path) -> Option<BufferId> {
        self.buffer_manager
            .lock()
            .unwrap()
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

    /// Queue recovery findings for delivery on the first host pump.
    #[must_use]
    pub fn with_recovery_notices(mut self, notices: Vec<&'static str>) -> Self {
        self.recovery_notices = notices;
        self
    }

    /// Build the Session.
    pub fn build(self) -> Result<Session, String> {
        let session = Session::new(
            self.identity.ok_or("Missing identity")?,
            self.actor.ok_or("Missing actor")?,
            self.buffer_manager.unwrap_or_default(),
            self.pulse.unwrap_or_else(|| PulseChannel::new(1024)),
            self.notice
                .unwrap_or_else(|| NoticeChannel::new(Duration::from_millis(300))),
            self.config.unwrap_or_default(),
        );
        *session.recovery_notices.lock().unwrap() = self.recovery_notices.into();
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
        assert!(session.is_open(&to));
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

        assert_eq!(session.autosave_failures_total(), 1);
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
                .to_source_bytes(),
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
        writer
            .write_buffer(
                b"note.md",
                session
                    .buffer_manager()
                    .lock()
                    .unwrap()
                    .get_buffer_by_id(buffer_id)
                    .unwrap(),
            )
            .unwrap();
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
                .to_source_bytes(),
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
        });
        session
            .buffer_manager()
            .lock()
            .unwrap()
            .get_buffer_by_id_mut(buffer_id)
            .unwrap()
            .check_external_change(b"theirs");
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
