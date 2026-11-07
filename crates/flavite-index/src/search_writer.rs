//! The dedicated ordered Tantivy writer owner (RFC-0006 S06, D-07).
//!
//! The `IndexWriter` is owned by exactly one bounded worker thread. Every
//! committed SQL generation publishes an immutable per-generation envelope
//! (document upserts/deletes plus the `search_docs`-backed content hashes —
//! never references reread at a newer generation), and `Capture(G)` is an
//! ordered queue message processed after envelope G and before envelope
//! G+1. Commit and reader reload run on this thread without holding the SQL
//! actor or any Session/UI turn.
//!
//! Backpressure is explicit: envelope capacity is reserved *before* the
//! corresponding SQL generation commits; exceeding the outstanding/byte
//! budget refuses index admission (`SearchBackpressure`), which upstream
//! maps to an index-pending retry — the durable file save is never repeated.
//! A lost envelope or epoch reset invalidates pending captures (they receive
//! [`CaptureOutcome::ResetRequired`]) and triggers a cache-based rebuild,
//! never gap skipping.

use crate::search::{SearchFields, TantivyBackend};
use crossbeam::channel::{Receiver, Sender, bounded, unbounded};
use std::sync::{Arc, Mutex, atomic::Ordering};
use std::time::{Duration, Instant};
use tantivy::doc;
use thiserror::Error;

/// Maximum outstanding (reserved-but-uncommitted) envelopes (D-07/NFR-7).
pub const MAX_OUTSTANDING_ENVELOPES: usize = 16;

/// Maximum aggregate envelope payload bytes in flight (D-07/NFR-7).
pub const MAX_ENVELOPE_QUEUE_BYTES: usize = 64 * 1024 * 1024;

/// Safety bound for a parked capture: a capture(G) is enqueued after
/// envelope G by construction, so it completes as soon as the writer reaches
/// it. If the writer stops making progress (crashed queue), captures fail
/// with a typed reset instead of hanging a read session for its full
/// lifetime.
pub const CAPTURE_PARK_TIMEOUT: Duration = Duration::from_secs(30);

/// One immutable document mutation inside a generation envelope.
#[derive(Debug, Clone, PartialEq)]
pub enum SearchDocOp {
    /// Replace the note's full search document.
    Upsert {
        note_id: i64,
        /// `search_docs`-backed hash of the exact indexed body (D-07: the
        /// envelope carries the hash, not a reference reread later).
        content_hash: [u8; 32],
        title: String,
        aliases: Vec<String>,
        tags: Vec<String>,
        body: String,
    },
    /// Replace a degraded note's title-only search document.
    TitleOnly {
        note_id: i64,
        content_hash: [u8; 32],
        title: String,
    },
    /// Remove the note's search document.
    Delete { note_id: i64 },
}

impl SearchDocOp {
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        const FIXED: usize = 128;
        match self {
            Self::Upsert {
                content_hash: _,
                title,
                aliases,
                tags,
                body,
                ..
            } => {
                FIXED
                    + title.len()
                    + body.len()
                    + aliases.iter().map(String::len).sum::<usize>()
                    + tags.iter().map(String::len).sum::<usize>()
            }
            Self::TitleOnly { title, .. } => FIXED + title.len(),
            Self::Delete { .. } => FIXED,
        }
    }
}

/// The immutable search-update envelope for one committed SQL generation.
/// Envelopes are published in strict generation order; the writer processes
/// them in that order and commits each before observing the next.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchEnvelope {
    pub epoch: u64,
    pub generation: u64,
    /// Immutable full-document updates/deletions prepared at this
    /// generation. An empty list is the no-op watermark every committed
    /// generation still sends (D-07).
    pub ops: Arc<[SearchDocOp]>,
}

impl SearchEnvelope {
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        self.ops
            .iter()
            .map(SearchDocOp::approx_bytes)
            .sum::<usize>()
            + 32
    }
}

/// The reply to an ordered capture. A completed capture carries a searcher
/// stamped with exactly the generation whose envelope was last committed —
/// never a mixed G/G+1 bundle.
#[derive(Debug, Clone)]
pub enum CaptureOutcome {
    Searcher {
        epoch: u64,
        generation: u64,
        searcher: crate::search::CapturedSearcher,
    },
    /// The queue lost messages, an epoch reset, or a rebuild invalidated the
    /// requested generation. Pending captures are never silently skipped.
    ResetRequired,
}

/// Writer queue commands. `Capture` is a queue message, not a request to
/// whichever searcher happens to be newest (D-07).
pub enum WriterCommand {
    Envelope(SearchEnvelope),
    Capture {
        epoch: u64,
        generation: u64,
        reply: Sender<CaptureOutcome>,
    },
    /// Full rebuild from the authoritative `search_docs` cache, stamped at
    /// `generation` (startup catch-up, gap recovery, epoch reset).
    Rebuild {
        epoch: u64,
        generation: u64,
        docs: Arc<[SearchDocOp]>,
        reply: Sender<Result<(), String>>,
    },
    Shutdown,
}

/// Durability acknowledgement returned to the sole SQL owner after Tantivy
/// commit/reload. The SQL owner advances `tantivy_committed_generation`
/// metadata monotonically only after these acks (D-07).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchCommitAck {
    pub epoch: u64,
    pub generation: u64,
}

/// Why an index admission was refused. Upstream maps this to the existing
/// index-pending retry path; the durable file write is never repeated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SearchBackpressure {
    #[error(
        "search writer queue is full ({MAX_OUTSTANDING_ENVELOPES} outstanding envelopes); index application is pending"
    )]
    QueueFull,
    #[error("search envelope byte budget exceeded: {observed} of {limit}")]
    BudgetExceeded { observed: usize, limit: usize },
}

/// Admission accounting for reserved-but-uncommitted envelopes. Capacity
/// (count and aggregate bytes) is reserved before the corresponding SQL
/// generation commits (D-07); the writer releases the accounting when it
/// dequeues the envelope. The ordered channel is unbounded precisely so a
/// post-commit enqueue of a reserved envelope cannot fail — capacity was
/// already held.
#[derive(Default)]
struct Admission {
    state: Mutex<(usize, usize)>,
    capacity: std::sync::Condvar,
}

/// Shutdown grace for the writer thread before a last handle detaches it
/// instead of blocking the process (a stuck indivisible commit must not
/// hang shutdown; the OS reclaims the thread with the process).
const WRITER_SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// How long an admission waits for the writer to drain a batch before
/// refusing. The wait bounds bulk reconciliation to the writer's commit
/// rate instead of mass `IndexPending` churn; refusal (and the upstream
/// `IndexPending` retry) still happens when the writer cannot drain.
const ADMISSION_WAIT: Duration = Duration::from_secs(2);

impl Admission {
    fn try_reserve(&self, approx_bytes: usize) -> Result<(), SearchBackpressure> {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let outcome =
            self.capacity
                .wait_timeout_while(state, ADMISSION_WAIT, |(outstanding, bytes)| {
                    *outstanding >= MAX_OUTSTANDING_ENVELOPES
                        || *bytes + approx_bytes > MAX_ENVELOPE_QUEUE_BYTES
                });
        let (mut state, timed_out) = match outcome {
            Ok((state, timeout)) => (state, timeout.timed_out()),
            Err(poisoned) => {
                let (state, timeout) = poisoned.into_inner();
                (state, timeout.timed_out())
            }
        };
        let (outstanding, bytes) = *state;
        if outstanding >= MAX_OUTSTANDING_ENVELOPES {
            return Err(SearchBackpressure::QueueFull);
        }
        let observed = bytes.saturating_add(approx_bytes);
        if observed > MAX_ENVELOPE_QUEUE_BYTES {
            return Err(SearchBackpressure::BudgetExceeded {
                observed,
                limit: MAX_ENVELOPE_QUEUE_BYTES,
            });
        }
        let _ = timed_out;
        *state = (outstanding + 1, observed);
        Ok(())
    }

    fn release(&self, approx_bytes: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = state.0.saturating_sub(1);
        state.1 = state.1.saturating_sub(approx_bytes);
        drop(state);
        self.capacity.notify_all();
    }
}

/// Reserved capacity for one future envelope. Dropping it without
/// enqueuing releases the reservation.
pub struct EnvelopeReservation {
    admission: Arc<Admission>,
    approx_bytes: usize,
    consumed: bool,
}

impl EnvelopeReservation {
    /// Enqueue the prepared envelope against this reservation. Capacity was
    /// reserved before the SQL generation committed, so the only failure is
    /// a dead writer, which downstream observes as a capture reset.
    ///
    /// # Errors
    /// Returns [`SearchBackpressure::QueueFull`] when the writer is no
    /// longer running; the reservation is released either way.
    pub fn enqueue(
        mut self,
        handle: &SearchWriterHandle,
        envelope: SearchEnvelope,
    ) -> Result<(), SearchBackpressure> {
        self.consumed = true;
        handle
            .tx
            .send(WriterCommand::Envelope(envelope))
            .map_err(|_| {
                self.admission.release(self.approx_bytes);
                SearchBackpressure::QueueFull
            })
    }
}

impl Drop for EnvelopeReservation {
    fn drop(&mut self) {
        if !self.consumed {
            self.admission.release(self.approx_bytes);
        }
    }
}

/// Cloneable handle to the single ordered writer thread.
#[derive(Clone)]
pub struct SearchWriterHandle {
    tx: Sender<WriterCommand>,
    admission: Arc<Admission>,
    acks: Arc<Mutex<Receiver<SearchCommitAck>>>,
    /// Reports from the writer thread that require actor action (gap
    /// detection / epoch reset). Drained by the SQL owner, which answers
    /// with a rebuild — never gap skipping.
    reports: Arc<Mutex<Receiver<WriterReport>>>,
    frontier: Arc<Mutex<u64>>,
    epoch: Arc<Mutex<u64>>,
    running: Arc<std::sync::atomic::AtomicBool>,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
}

impl Drop for SearchWriterHandle {
    fn drop(&mut self) {
        // The last handle drops the writer thread and waits for it, so a
        // reopened index never races the previous writer for the Tantivy
        // lockfile. Intermediate clones keep the worker alive.
        if Arc::strong_count(&self.worker) == 1 {
            let _ = self.tx.send(WriterCommand::Shutdown);
            let worker = self
                .worker
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take();
            if let Some(worker) = worker {
                // Bounded wait: a writer stalled inside an indivisible
                // Tantivy operation must not hang the process on shutdown.
                // The queue disconnects when this handle finishes dropping,
                // so the thread exits at its next command boundary; a
                // subsequent handle on this index degrades to secondary
                // mode rather than hanging on the lockfile.
                let deadline = Instant::now() + WRITER_SHUTDOWN_GRACE;
                while !worker.is_finished() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                if worker.is_finished() {
                    let _ = worker.join();
                } else {
                    tracing::warn!(
                        "search writer did not exit within its shutdown grace; detaching"
                    );
                    std::thread::spawn(move || {
                        let _ = worker.join();
                    });
                }
            }
        }
    }
}

/// Asynchronous writer report consumed by the SQL owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterReport {
    /// The next envelope did not continue the frontier: the queue lost
    /// messages or the database was recreated. All pending captures are
    /// already reset; the owner must rebuild from the authoritative cache.
    Desynced,
}

impl std::fmt::Debug for SearchWriterHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchWriterHandle")
            .field("frontier", &self.committed_frontier())
            .finish_non_exhaustive()
    }
}

impl SearchWriterHandle {
    /// Reserve capacity for one future envelope. The reservation must be
    /// taken *before* the corresponding SQL generation commits (D-07).
    ///
    /// # Errors
    /// Returns backpressure when the byte budget is exhausted.
    pub fn reserve(&self, approx_bytes: usize) -> Result<EnvelopeReservation, SearchBackpressure> {
        self.admission.try_reserve(approx_bytes)?;
        Ok(EnvelopeReservation {
            admission: Arc::clone(&self.admission),
            approx_bytes,
            consumed: false,
        })
    }

    /// Enqueue an ordered `Capture(G)`. The writer completes the reply after
    /// envelope G has committed and before any later envelope is processed,
    /// so the returned searcher is stamped exactly at G.
    ///
    /// # Errors
    /// Returns backpressure when the ordered queue is full; retry is
    /// explicit upstream.
    pub fn enqueue_capture(
        &self,
        epoch: u64,
        generation: u64,
        reply: Sender<CaptureOutcome>,
    ) -> Result<(), SearchBackpressure> {
        self.tx
            .send(WriterCommand::Capture {
                epoch,
                generation,
                reply,
            })
            .map_err(|_| SearchBackpressure::QueueFull)
    }

    /// Request a full rebuild stamped at `generation`. Blocks the caller
    /// until the rebuild has committed — legitimate only at startup or on
    /// the SQL owner's recovery path, never on a UI turn.
    ///
    /// # Errors
    /// Returns the writer's error text when the rebuild commit fails.
    pub fn rebuild_blocking(
        &self,
        epoch: u64,
        generation: u64,
        docs: Arc<[SearchDocOp]>,
    ) -> Result<(), String> {
        let (tx, rx) = bounded::<Result<(), String>>(1);
        self.tx
            .send(WriterCommand::Rebuild {
                epoch,
                generation,
                docs,
                reply: tx,
            })
            .map_err(|_| "search writer is not running".to_owned())?;
        rx.recv_timeout(Duration::from_secs(120))
            .map_err(|_| "search writer rebuild did not complete".to_owned())?
    }

    /// Synchronous capture used by startup and tests: enqueue and block for
    /// the ordered reply. Callers must never wait on this from the SQL
    /// actor or a UI turn while writes are in flight.
    ///
    /// # Errors
    /// Returns an error text when the writer is not running.
    pub fn capture_blocking(&self, epoch: u64, generation: u64) -> Result<CaptureOutcome, String> {
        let (tx, rx) = bounded::<CaptureOutcome>(1);
        self.enqueue_capture(epoch, generation, tx)
            .map_err(|_| "search writer queue is full".to_owned())?;
        rx.recv_timeout(CAPTURE_PARK_TIMEOUT)
            .map_err(|_| "search writer did not answer the capture".to_owned())
    }

    /// Drain committed-version acknowledgements. The SQL owner calls this
    /// and advances `tantivy_committed_generation` metadata monotonically.
    pub fn drain_acks(&self) -> Vec<SearchCommitAck> {
        let acks = self
            .acks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut drained = Vec::new();
        while let Ok(ack) = acks.try_recv() {
            drained.push(ack);
        }
        drained
    }

    /// Drain writer reports requiring owner action.
    pub fn drain_reports(&self) -> Vec<WriterReport> {
        let reports = self
            .reports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut drained = Vec::new();
        while let Ok(report) = reports.try_recv() {
            drained.push(report);
        }
        drained
    }

    /// The writer's committed frontier (last envelope generation applied
    /// and committed).
    #[must_use]
    pub fn committed_frontier(&self) -> u64 {
        *self
            .frontier
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The epoch the writer last saw.
    #[must_use]
    pub fn epoch(&self) -> u64 {
        *self
            .epoch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the writer thread is still accepting commands.
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.running.load(Ordering::Acquire)
    }

    /// Stop the writer thread after draining queued commands.
    pub fn shutdown(&self) {
        let _ = self.tx.send(WriterCommand::Shutdown);
    }
}

struct PendingCapture {
    generation: u64,
    reply: Sender<CaptureOutcome>,
}

/// Spawn the single ordered writer thread over `backend`'s index.
///
/// # Errors
/// Returns an error when the Tantivy writer cannot be created.
pub fn spawn_search_writer(
    backend: &TantivyBackend,
    initial_epoch: u64,
    initial_frontier: u64,
) -> Result<SearchWriterHandle, String> {
    let writer = backend.create_writer().map_err(|error| error.to_string())?;
    let reader = backend.reader();
    let fields = backend.fields();
    let (tx, rx) = unbounded::<WriterCommand>();
    let (ack_tx, ack_rx) = crossbeam::channel::unbounded::<SearchCommitAck>();
    let (report_tx, report_rx) = crossbeam::channel::unbounded::<WriterReport>();
    let admission = Arc::new(Admission::default());
    let frontier = Arc::new(Mutex::new(initial_frontier));
    let epoch_cell = Arc::new(Mutex::new(initial_epoch));
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let worker_frontier = Arc::clone(&frontier);
    let worker_epoch = Arc::clone(&epoch_cell);
    let worker_admission = Arc::clone(&admission);
    let worker_running = Arc::clone(&running);
    let worker_cell: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let handle = std::thread::Builder::new()
        .name("flavite-search-writer".into())
        .spawn(move || {
            writer_loop(
                writer,
                reader,
                fields,
                rx,
                &ack_tx,
                &report_tx,
                &worker_admission,
                &worker_frontier,
                &worker_epoch,
                &worker_running,
            );
        })
        .map_err(|error| error.to_string())?;
    *worker_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(handle);
    Ok(SearchWriterHandle {
        tx,
        admission,
        acks: Arc::new(Mutex::new(ack_rx)),
        reports: Arc::new(Mutex::new(report_rx)),
        frontier,
        epoch: epoch_cell,
        running,
        worker: worker_cell,
    })
}

#[allow(
    clippy::needless_pass_by_value,
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one ordered thread's enqueue/commit/capture discipline; splitting it would only relocate the ordering argument"
)]
fn writer_loop(
    mut writer: tantivy::IndexWriter,
    reader: tantivy::IndexReader,
    fields: SearchFields,
    rx: Receiver<WriterCommand>,
    ack_tx: &Sender<SearchCommitAck>,
    report_tx: &Sender<WriterReport>,
    admission: &Admission,
    frontier_cell: &Mutex<u64>,
    epoch_cell: &Mutex<u64>,
    running: &std::sync::atomic::AtomicBool,
) {
    let mut frontier: u64 = *frontier_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut desynced = false;
    let mut parked: Vec<PendingCapture> = Vec::new();
    let mut replayed: std::collections::VecDeque<WriterCommand> = std::collections::VecDeque::new();

    let complete_parked = |parked: &mut Vec<PendingCapture>,
                           frontier: u64,
                           epoch: u64,
                           reader: &tantivy::IndexReader,
                           fields: &SearchFields| {
        let searcher = crate::search::CapturedSearcher {
            searcher: reader.searcher(),
            fields: *fields,
        };
        parked.retain(|pending| {
            if pending.generation <= frontier {
                // The reply carries the writer's actual frontier, not the
                // requested generation: a capture parked for an older
                // generation than the batch committed must fail the
                // worker's coherence check (D-07: never a mixed G/G+1
                // bundle).
                let _ = pending.reply.try_send(CaptureOutcome::Searcher {
                    epoch,
                    generation: frontier,
                    searcher: searcher.clone(),
                });
                false
            } else {
                true
            }
        });
    };

    // Envelope batching: consecutive leading envelopes drain into ONE
    // commit stamped at the last generation, so bulk reconcile throughput
    // is bounded by apply work, not commit count. The drain always stops at
    // the first non-envelope command — in particular at a Capture — so a
    // parked capture(G) still observes a commit stamped exactly at G (D-07:
    // the capture is ordered after envelope G and before envelope G+1).
    'outer: loop {
        let Ok(command) = rx.recv() else {
            break;
        };
        match command {
            WriterCommand::Envelope(first) => {
                admission.release(first.approx_bytes());
                admission.capacity.notify_all();
                if desynced {
                    continue;
                }
                if first.generation != frontier.saturating_add(1) {
                    desynced = true;
                    for pending in parked.drain(..) {
                        let _ = pending.reply.try_send(CaptureOutcome::ResetRequired);
                    }
                    let _ = report_tx.send(WriterReport::Desynced);
                    continue;
                }
                let SearchEnvelope {
                    epoch: first_epoch,
                    generation: first_generation,
                    ops: first_ops,
                } = first;
                // Envelopes already hold immutable `Arc` slices: the batch
                // carries the slices, never a deep clone of their bodies.
                let mut ops: Vec<Arc<[SearchDocOp]>> = vec![first_ops];
                let mut last_epoch = first_epoch;
                let mut last_generation = first_generation;
                while let Ok(next) = rx.try_recv() {
                    match next {
                        WriterCommand::Envelope(envelope) => {
                            admission.release(envelope.approx_bytes());
                            admission.capacity.notify_all();
                            if desynced {
                                continue;
                            }
                            if envelope.generation != last_generation.saturating_add(1) {
                                desynced = true;
                                for pending in parked.drain(..) {
                                    let _ = pending.reply.try_send(CaptureOutcome::ResetRequired);
                                }
                                let _ = report_tx.send(WriterReport::Desynced);
                                continue 'outer;
                            }
                            let SearchEnvelope {
                                epoch,
                                generation,
                                ops: envelope_ops,
                            } = envelope;
                            ops.push(envelope_ops);
                            last_epoch = epoch;
                            last_generation = generation;
                        }
                        other => {
                            // Process the straddling command after this
                            // batch's commit by pushing it back through a
                            // replay queue.
                            replayed.push_back(other);
                            break;
                        }
                    }
                }
                if writer_commit_and_stamp(
                    &mut writer,
                    &reader,
                    fields,
                    &ops,
                    last_epoch,
                    last_generation,
                    ack_tx,
                    frontier_cell,
                    epoch_cell,
                )
                .is_err()
                {
                    desynced = true;
                    for pending in parked.drain(..) {
                        let _ = pending.reply.try_send(CaptureOutcome::ResetRequired);
                    }
                    let _ = report_tx.send(WriterReport::Desynced);
                    continue;
                }
                frontier = last_generation;
                complete_parked(&mut parked, frontier, last_epoch, &reader, &fields);
            }
            other => replayed.push_back(other),
        }
        // Replay any straddled commands in order.
        while let Some(command) = replayed.pop_front() {
            match command {
                WriterCommand::Envelope(_) => {
                    replayed.push_front(command);
                    continue 'outer;
                }
                WriterCommand::Capture {
                    epoch: _requested_epoch,
                    generation,
                    reply,
                } => {
                    if desynced {
                        let _ = reply.try_send(CaptureOutcome::ResetRequired);
                        continue;
                    }
                    if generation <= frontier {
                        // The reply carries the writer's own frontier and
                        // epoch, never the requested ones: only a reply
                        // stamped at the caller's exact snapshot passes its
                        // coherence check (D-07: never a mixed G/G+1
                        // bundle).
                        let _ = reply.try_send(CaptureOutcome::Searcher {
                            epoch: *epoch_cell
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                            generation: frontier,
                            searcher: crate::search::CapturedSearcher {
                                searcher: reader.searcher(),
                                fields,
                            },
                        });
                    } else if generation == frontier.saturating_add(1) {
                        parked.push(PendingCapture { generation, reply });
                    } else {
                        // The envelope for this generation can never arrive.
                        let _ = reply.try_send(CaptureOutcome::ResetRequired);
                    }
                }
                WriterCommand::Rebuild {
                    epoch,
                    generation,
                    docs,
                    reply,
                } => {
                    let outcome = (|| -> Result<(), String> {
                        writer
                            .delete_all_documents()
                            .map_err(|error| error.to_string())?;
                        apply_ops(&mut writer, fields, &docs);
                        writer.commit().map_err(|error| error.to_string())?;
                        reader.reload().map_err(|error| error.to_string())?;
                        Ok(())
                    })();
                    match outcome {
                        Ok(()) => {
                            frontier = generation;
                            desynced = false;
                            *frontier_cell
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = frontier;
                            *epoch_cell
                                .lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner) = epoch;
                            let _ = ack_tx.send(SearchCommitAck { epoch, generation });
                            complete_parked(&mut parked, frontier, epoch, &reader, &fields);
                            let _ = reply.try_send(Ok(()));
                        }
                        Err(message) => {
                            desynced = true;
                            for pending in parked.drain(..) {
                                let _ = pending.reply.try_send(CaptureOutcome::ResetRequired);
                            }
                            let _ = reply.try_send(Err(message));
                        }
                    }
                }
                WriterCommand::Shutdown => break 'outer,
            }
        }
    }
    running.store(false, Ordering::Release);
}

/// Apply one batch of ops, commit, reload, and record the durability ack
/// with the stamped generation.
#[allow(clippy::too_many_arguments)]
fn writer_commit_and_stamp(
    writer: &mut tantivy::IndexWriter,
    reader: &tantivy::IndexReader,
    fields: SearchFields,
    ops: &[Arc<[SearchDocOp]>],
    epoch: u64,
    generation: u64,
    ack_tx: &Sender<SearchCommitAck>,
    frontier_cell: &Mutex<u64>,
    epoch_cell: &Mutex<u64>,
) -> Result<(), ()> {
    for batch in ops {
        apply_ops(writer, fields, batch);
    }
    // The actual error is logged here: the caller only converts the failure
    // into `WriterReport::Desynced`, which carries no error text.
    if let Err(error) = writer.commit() {
        tracing::error!(%error, "search writer commit failed; waiting for the desync rebuild");
        return Err(());
    }
    if let Err(error) = reader.reload() {
        tracing::error!(%error, "search writer reload failed; waiting for the desync rebuild");
        return Err(());
    }
    *frontier_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = generation;
    *epoch_cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = epoch;
    let _ = ack_tx.send(SearchCommitAck { epoch, generation });
    Ok(())
}

fn apply_ops(writer: &mut tantivy::IndexWriter, fields: SearchFields, ops: &[SearchDocOp]) {
    for op in ops {
        match op {
            SearchDocOp::Upsert {
                note_id,
                title,
                aliases,
                tags,
                body,
                ..
            } => {
                writer.delete_term(tantivy::Term::from_field_u64(
                    fields.id,
                    note_id.cast_unsigned(),
                ));
                let mut document = tantivy::doc!(
                    fields.id => note_id.cast_unsigned(),
                    fields.title => title.as_str(),
                    fields.body => body.as_str(),
                );
                for alias in aliases {
                    document.add_text(fields.aliases, alias.as_str());
                }
                for tag in tags {
                    document.add_text(fields.tags, tag.as_str());
                }
                let _ = writer.add_document(document);
            }
            SearchDocOp::TitleOnly { note_id, title, .. } => {
                writer.delete_term(tantivy::Term::from_field_u64(
                    fields.id,
                    note_id.cast_unsigned(),
                ));
                let _ = writer.add_document(tantivy::doc!(
                    fields.id => note_id.cast_unsigned(),
                    fields.title => title.as_str(),
                ));
            }
            SearchDocOp::Delete { note_id } => {
                writer.delete_term(tantivy::Term::from_field_u64(
                    fields.id,
                    note_id.cast_unsigned(),
                ));
            }
        }
    }
}

/// Read the full document set for a rebuild from the authoritative SQLite
/// cache (notes joined with `search_docs`, aliases, tags). Three ordered
/// queries total — one per table, merged by `note_id` — so startup does not
/// pay one aliases/tags statement pair per note on the SQL owner thread.
///
/// # Errors
/// Returns an error when the cache read fails.
pub fn read_rebuild_docs(conn: &rusqlite::Connection) -> Result<Vec<SearchDocOp>, String> {
    let mut docs_stmt = conn
        .prepare(
            "SELECT n.note_id,n.title,COALESCE(s.body_text,''), \
             COALESCE(s.content_hash,x''), n.parse_ok \
             FROM notes n LEFT JOIN search_docs s USING(note_id) ORDER BY n.note_id",
        )
        .map_err(|error| error.to_string())?;
    let doc_rows = docs_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, bool>(4)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    let alias_rows = grouped_strings(
        conn,
        "SELECT note_id, alias FROM aliases ORDER BY note_id, alias_key",
    )?;
    let tag_rows = grouped_strings(conn, "SELECT note_id, tag FROM tags ORDER BY note_id, tag")?;
    let (mut alias_index, mut tag_index) = (0usize, 0usize);
    let mut docs = Vec::new();
    for row in doc_rows {
        let (id, title, body, hash, parse_ok) = row.map_err(|error| error.to_string())?;
        let mut content_hash = [0u8; 32];
        if hash.len() == 32 {
            content_hash.copy_from_slice(&hash);
        }
        if !parse_ok {
            docs.push(SearchDocOp::TitleOnly {
                note_id: id,
                content_hash,
                title,
            });
            continue;
        }
        // Merge-scan: both side queries are ordered by note_id, so each
        // note's group is the leading run of its cursor.
        let mut aliases = Vec::new();
        while let Some((group_note_id, alias)) = alias_rows.get(alias_index) {
            match group_note_id.cmp(&id) {
                std::cmp::Ordering::Less => alias_index += 1,
                std::cmp::Ordering::Equal => {
                    aliases.push(alias.clone());
                    alias_index += 1;
                }
                std::cmp::Ordering::Greater => break,
            }
        }
        let mut tags = Vec::new();
        while let Some((group_note_id, tag)) = tag_rows.get(tag_index) {
            match group_note_id.cmp(&id) {
                std::cmp::Ordering::Less => tag_index += 1,
                std::cmp::Ordering::Equal => {
                    tags.push(tag.clone());
                    tag_index += 1;
                }
                std::cmp::Ordering::Greater => break,
            }
        }
        docs.push(SearchDocOp::Upsert {
            note_id: id,
            content_hash,
            title,
            aliases,
            tags,
            body,
        });
    }
    Ok(docs)
}

/// All `(note_id, value)` rows of one ordered side query.
fn grouped_strings(conn: &rusqlite::Connection, sql: &str) -> Result<Vec<(i64, String)>, String> {
    let mut stmt = conn.prepare(sql).map_err(|error| error.to_string())?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())
}
