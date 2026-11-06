//! Isolated regex-matching helper process pool (RFC-0006 §6.2/D-14, S03).
//!
//! Regex evaluation runs in the `flavite-text-search-worker` sibling —
//! located next to THIS executable, never PATH, never an environment
//! override — with a 5 s CPU-operation wall deadline and process-group
//! kill. The parent owns time enforcement; the helper self-limits its
//! address space. A failed, timed-out, or protocol-violating helper
//! instance can never be reused: every operation spawns a fresh process
//! under a bounded permit, so a poisoned instance structurally cannot
//! re-enter the pool. Literal source operations never touch the helper
//! and stay available when it is missing (typed `RegexUnavailable`).
//!
//! The request flow is strictly ordered: spawn → `Handshake` → verified
//! `HandshakeReply` → `Begin` → (`Template` for replace) → source chunks →
//! `EndOfSource` → streamed pages → terminal `Done`/`Failed`. A helper that
//! skips the handshake cannot receive work, and a foreign engine identity
//! is refused before any frame ships (D-14). A separate reader thread
//! drains replies for the whole operation, so a helper that fails early
//! can never deadlock a parent that is still streaming input.
//!
//! Revision checks and `EditOp` construction live in `text_edit.rs`/the
//! Session: the helper returns match/expansion data, never an
//! authoritative prepared buffer candidate.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use flavite_text_search::matcher::MatchMode;
use flavite_text_search::protocol::{
    self, FRAME_MAX_BYTES, FailureCode, FrameError, FromHelper, RequestKind, SOURCE_MAX_BYTES,
    ToHelper, WireMatch, encode, read_reply, write_frame,
};
use flavite_text_search::template::{ReplacementTemplate, TemplateGrammar};

/// Wall-clock deadline for one helper operation (§6.2). The parent kills
/// and reaps the helper after this; indivisible OS scheduling may add a
/// small overshoot, which is honest visible delay, not false completion.
pub const HELPER_DEADLINE: Duration = Duration::from_secs(5);

/// Concurrent helper operations (NFR-10): one slot reserved for
/// interactive note commands, one shared slot usable by bulk work.
pub const HELPER_SLOTS: usize = 2;

/// Bulk source operations may never occupy more than this many concurrent
/// helper slots (§6.2: one slot stays free for interactive admission).
pub const HELPER_BULK_SLOTS: usize = 1;

/// Source chunks streamed per frame (well inside the frame bound).
const CHUNK_BYTES: usize = 32 * 1024;

/// Why regex work could not run in the helper. Typed, redacted — never
/// pattern/source text (§6.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextWorkerError {
    /// The installed sibling helper is missing or unusable. Literal
    /// matching remains available (RegexUnavailable state, §6.8).
    Unavailable { reason: &'static str },
    /// All helper slots this request class may use are occupied; admission
    /// is refused (Busy), never queued unbounded.
    Busy,
    /// The operation exceeded [`HELPER_DEADLINE`] and the helper was
    /// killed; only that request is invalidated (§6.2).
    Deadline,
    /// The caller's cancellation token fired.
    Cancelled,
    /// The helper died or violated the protocol mid-operation.
    HelperDied,
    /// A typed helper refusal (mirrors the protocol failure codes).
    Failure(FailureCode),
    /// A framed-message error.
    Frame(FrameError),
}

/// Which reservation a helper admission uses (§6.2 D-14: one slot and one
/// preparation-worker slot are reserved for interactive note commands;
/// bulk work cannot occupy the interactive reservation, accepting lower
/// bulk parallelism for bounded interactive admission).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelperPermitClass {
    /// Interactive note Find/Replace admission (both slots usable).
    Interactive,
    /// Bulk source operations (one slot ceiling).
    Bulk,
}

/// Bounded concurrency for helper operations. Acquisition is nonblocking;
/// failure means the class reservation is exhausted (Busy, not a wait).
#[derive(Debug, Default)]
pub struct HelperPermits {
    total: AtomicUsize,
    bulk: AtomicUsize,
}

impl HelperPermits {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to take one slot for `class`; `false` when the class
    /// reservation is full (Busy, never an unbounded wait).
    #[must_use]
    pub fn acquire(&self, class: HelperPermitClass) -> bool {
        // Order matters: claim the class counter first so two racing bulk
        // admissions cannot both pass the total check and then strand one
        // slot. Release runs if the second claim fails.
        match class {
            HelperPermitClass::Interactive => {}
            HelperPermitClass::Bulk => {
                if self.bulk.fetch_add(1, Ordering::AcqRel) >= HELPER_BULK_SLOTS {
                    self.bulk.fetch_sub(1, Ordering::AcqRel);
                    return false;
                }
            }
        }
        if self.total.fetch_add(1, Ordering::AcqRel) >= HELPER_SLOTS {
            self.total.fetch_sub(1, Ordering::AcqRel);
            if class == HelperPermitClass::Bulk {
                self.bulk.fetch_sub(1, Ordering::AcqRel);
            }
            return false;
        }
        true
    }

    /// Return one slot of `class`; released slots are immediately reusable.
    pub fn release(&self, class: HelperPermitClass) {
        self.total.fetch_sub(1, Ordering::AcqRel);
        if class == HelperPermitClass::Bulk {
            self.bulk.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

/// The process-wide permit set for helper admissions.
#[must_use]
pub fn helper_permits() -> &'static HelperPermits {
    static PERMITS: OnceLock<HelperPermits> = OnceLock::new();
    PERMITS.get_or_init(HelperPermits::new)
}

/// The installed sibling helper, when present: the exact file next to the
/// running executable (§6.2 — never PATH, never a user-supplied override,
/// never derived from note content or preferences).
#[must_use]
pub fn sibling_helper_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.parent()?.join("flavite-text-search-worker");
    #[cfg(windows)]
    let sibling = sibling.with_extension("exe");
    sibling.is_file().then_some(sibling)
}

/// One helper admission: source, pattern/options, and the request kind.
/// Find requests stream spans with empty expansions; Replace requests also
/// carry the replacement template and its per-note output bound.
pub struct HelperRequest<'a> {
    /// Find (spans only) or Replace (spans + per-match expansions).
    pub kind: RequestKind,
    pub source: &'a str,
    pub pattern: &'a str,
    pub mode: MatchMode,
    pub case_sensitive: bool,
    pub whole_word: bool,
    /// Frozen scope in text-byte coordinates, validated by the caller
    /// against the same source before dispatch (helper re-validates).
    pub scope: Option<(usize, usize)>,
    /// Match-count budget (NFR-8). Exceeding it is a typed refusal.
    pub max_matches: usize,
    /// Matches reported back at most; `usize::MAX` means "report every
    /// match". The exact total is still counted and proven terminally.
    pub report_matches: usize,
    /// Zero-based ordinal of the first match to ship. The helper still scans
    /// from the beginning and proves the complete total for this source.
    pub skip_matches: u64,
    /// Expanded-output bound for Replace requests (NFR-8: per-note ≤ 32
    /// MiB). Checked before any page ships.
    pub max_output: usize,
    /// Replacement template; ignored for Find requests.
    pub template_raw: &'a str,
}

impl<'a> HelperRequest<'a> {
    fn grammar(&self) -> TemplateGrammar {
        match self.mode {
            MatchMode::Literal => TemplateGrammar::Literal,
            MatchMode::Regex => TemplateGrammar::Dollar,
        }
    }

    /// Replace admissions validate their template locally before any
    /// spawn; Find admissions carry no template.
    fn validate_locally(&self) -> Result<(), TextWorkerError> {
        if self.kind == RequestKind::Replace {
            ReplacementTemplate::new(self.template_raw, self.grammar())
                .map_err(|_| TextWorkerError::Failure(FailureCode::Template))?;
        }
        Ok(())
    }
}

/// One completed helper match run: ascending matches with per-match
/// expansions plus the exact totals the helper proved.
#[derive(Debug, Clone)]
pub struct HelperRun {
    pub matches: Vec<WireMatch>,
    pub total_matches: u64,
    pub output_len: u64,
}

/// Run one match request in a freshly spawned helper under an interactive
/// permit (note Find/Replace).
///
/// The template is validated locally before any spawn and re-validated in
/// the helper; the parent re-derives the output length from the returned
/// spans and cross-checks the helper's terminal proof, so a buggy or
/// hostile helper cannot hand the session an unsound candidate.
///
/// # Errors
///
/// [`TextWorkerError`] — the helper missing/dead/late/over-queue, a typed
/// helper refusal, or a frame error. Zero partial results escape: pages
/// are returned only behind the terminal `Done` proof.
pub fn run_match(
    request: &HelperRequest<'_>,
    cancelled: &AtomicBool,
) -> Result<HelperRun, TextWorkerError> {
    request.validate_locally()?;
    admit(request, cancelled, HelperPermitClass::Interactive)
}

/// Bulk-class admission (vault source operations, S07+): capped to
/// [`HELPER_BULK_SLOTS`] so interactive note commands always keep a
/// reserved slot even while a pathological bulk regex runs.
pub fn run_match_bulk(
    request: &HelperRequest<'_>,
    cancelled: &AtomicBool,
) -> Result<HelperRun, TextWorkerError> {
    request.validate_locally()?;
    admit(request, cancelled, HelperPermitClass::Bulk)
}

fn admit(
    request: &HelperRequest<'_>,
    cancelled: &AtomicBool,
    class: HelperPermitClass,
) -> Result<HelperRun, TextWorkerError> {
    let Some(helper) = sibling_helper_path() else {
        return Err(TextWorkerError::Unavailable {
            reason: "helper-not-installed",
        });
    };
    if !helper_permits().acquire(class) {
        return Err(TextWorkerError::Busy);
    }
    let outcome = run_match_locked(&helper, request, cancelled);
    helper_permits().release(class);
    outcome
}

fn run_match_locked(
    helper: &Path,
    request: &HelperRequest<'_>,
    cancelled: &AtomicBool,
) -> Result<HelperRun, TextWorkerError> {
    if request.source.len() as u64 > SOURCE_MAX_BYTES {
        return Err(TextWorkerError::Failure(FailureCode::Source));
    }
    // Scope endpoints are validated against the source the helper will
    // receive; a frozen selection that no longer fits is refused here and
    // never silently widened (D-05).
    if let Some((start, end)) = request.scope
        && (end < start
            || end > request.source.len()
            || !request.source.is_char_boundary(start)
            || !request.source.is_char_boundary(end))
    {
        return Err(TextWorkerError::Failure(FailureCode::Source));
    }
    let request_id = next_request_id();
    let started = Instant::now();

    // Restricted inheritance: piped stdin/stdout, discarded stderr, own
    // process group (Unix) so no orphaned descendants outlive a kill.
    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn_with_group()
        .map_err(|_| TextWorkerError::Unavailable {
            reason: "helper-spawn-failed",
        })?;
    let result = drive_helper(&mut child, request_id, started, request, cancelled);
    // Whatever happened, the helper instance never serves again: this is
    // what makes "no poisoned helper re-enters the pool" structural.
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn next_request_id() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Drive one full helper operation: handshake, admission, input streaming,
/// reply reading with the shared wall deadline. The reader thread runs for
/// the whole operation so helper replies never block parent writes.
fn drive_helper(
    child: &mut Child,
    request_id: u64,
    started: Instant,
    request: &HelperRequest<'_>,
    cancelled: &AtomicBool,
) -> Result<HelperRun, TextWorkerError> {
    let mut stdin = child.stdin.take().ok_or(TextWorkerError::HelperDied)?;
    let stdout = child.stdout.take().ok_or(TextWorkerError::HelperDied)?;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("text-worker-read".into())
        .spawn(move || {
            let mut reader = stdout;
            loop {
                match read_reply(&mut reader) {
                    Ok(message) => {
                        if reply_tx.send(Ok(message)).is_err() {
                            break;
                        }
                    }
                    Err(FrameError::Truncated) => {
                        // Stream closed: the helper exited (death or the
                        // natural end after Done/Failed).
                        break;
                    }
                    Err(error) => {
                        let _ = reply_tx.send(Err(error));
                        break;
                    }
                }
            }
        })
        .map_err(|_| TextWorkerError::HelperDied)?;

    // 1. Handshake first: the helper refuses Begin without it, and the
    //    engine identity must match before any request is admitted.
    send_frame(
        &mut stdin,
        &ToHelper::Handshake {
            protocol_version: protocol::PROTOCOL_VERSION,
            engine_version: flavite_text_search::engine_version(),
        },
    )?;
    match recv_frame(&reply_rx, started, cancelled)? {
        FromHelper::HandshakeReply(Ok((protocol_version, engine_version))) => {
            if protocol_version != protocol::PROTOCOL_VERSION
                || engine_version != flavite_text_search::engine_version()
            {
                // A foreign engine is refused, never fallen back from.
                return Err(TextWorkerError::Unavailable {
                    reason: "engine-mismatch",
                });
            }
        }
        FromHelper::HandshakeReply(Err(failure)) => {
            return Err(TextWorkerError::Unavailable {
                reason: failure.code(),
            });
        }
        other => {
            let _ = other;
            return Err(TextWorkerError::HelperDied);
        }
    }

    // 2. Begin with kind, scope, report cap and output bound.
    let report_matches = u64::try_from(request.report_matches).unwrap_or(u64::MAX);
    send_frame(
        &mut stdin,
        &ToHelper::Begin {
            request_id,
            kind: request.kind,
            mode: request.mode,
            case_sensitive: request.case_sensitive,
            whole_word: request.whole_word,
            max_matches: u64::try_from(request.max_matches).unwrap_or(u64::MAX),
            // The budget bound must not drop reported matches a page short of
            // what the parent asked for: cap at the budget inside the helper.
            report_matches: if request.report_matches == usize::MAX {
                0
            } else {
                report_matches
            },
            skip_matches: request.skip_matches,
            max_output: u64::try_from(request.max_output).unwrap_or(u64::MAX),
            scope: request.scope.map(|(start, end)| {
                (
                    u64::try_from(start).unwrap_or(u64::MAX),
                    u64::try_from(end).unwrap_or(u64::MAX),
                )
            }),
            source_total: request.source.len() as u64,
            pattern: request.pattern.as_bytes().to_vec(),
        },
    )?;

    // 3. Template for replace requests (validated locally and in the
    //    helper against its own copy).
    if request.kind == RequestKind::Replace {
        send_frame(
            &mut stdin,
            &ToHelper::Template {
                request_id,
                template: request.template_raw.as_bytes().to_vec(),
            },
        )?;
    }

    // 4. Stream the declared source, checking cancellation and the wall
    //    deadline between chunks (an early helper failure reply surfaces
    //    through the reader channel; the writes unblock on pipe death).
    for (offset, chunk) in request.source.as_bytes().chunks(CHUNK_BYTES).enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            let _ = send_frame(&mut stdin, &ToHelper::Cancel { request_id });
            return Err(TextWorkerError::Cancelled);
        }
        if started.elapsed() >= HELPER_DEADLINE {
            return Err(TextWorkerError::Deadline);
        }
        send_frame(
            &mut stdin,
            &ToHelper::SourceChunk {
                request_id,
                offset: (offset * CHUNK_BYTES) as u64,
                bytes: chunk.to_vec(),
            },
        )?;
    }
    send_frame(&mut stdin, &ToHelper::EndOfSource { request_id })?;
    drop(stdin);

    // 5. Accepted, pages and the terminal proof. Pages alone are not a
    //    result: every exit path except Done returns an error.
    match recv_frame(&reply_rx, started, cancelled)? {
        FromHelper::Accepted { request_id: id } if id == request_id => {}
        _ => return Err(TextWorkerError::HelperDied),
    }
    let mut matches = Vec::new();
    let mut pending_match: Option<(u64, WireMatch, usize)> = None;
    let mut expansion_bytes = 0usize;
    let totals;
    loop {
        match recv_frame(&reply_rx, started, cancelled)? {
            FromHelper::Page {
                request_id: id,
                matches: page,
            } if id == request_id && pending_match.is_none() => {
                if request.kind == RequestKind::Find
                    && page.iter().any(|matched| !matched.expansion.is_empty())
                {
                    return Err(TextWorkerError::HelperDied);
                }
                if request.kind == RequestKind::Replace {
                    for matched in &page {
                        expansion_bytes = expansion_bytes
                            .checked_add(matched.expansion.len())
                            .ok_or(TextWorkerError::HelperDied)?;
                        if expansion_bytes > request.max_output {
                            return Err(TextWorkerError::HelperDied);
                        }
                    }
                }
                matches.extend(page);
            }
            FromHelper::MatchStart {
                request_id: id,
                index,
                matched,
                expansion_len,
            } if id == request_id && request.kind == RequestKind::Replace => {
                if pending_match.is_some()
                    || index != u64::try_from(matches.len()).unwrap_or(u64::MAX)
                    || !matched.expansion.is_empty()
                {
                    return Err(TextWorkerError::HelperDied);
                }
                let len =
                    usize::try_from(expansion_len).map_err(|_| TextWorkerError::HelperDied)?;
                let next_expansion_total = expansion_bytes
                    .checked_add(len)
                    .ok_or(TextWorkerError::HelperDied)?;
                if next_expansion_total > request.max_output {
                    return Err(TextWorkerError::HelperDied);
                }
                expansion_bytes = next_expansion_total;
                if len == 0 {
                    matches.push(matched);
                } else {
                    let mut matched = matched;
                    matched.expansion.reserve(len);
                    pending_match = Some((index, matched, len));
                }
            }
            FromHelper::ExpansionChunk {
                request_id: id,
                index,
                offset,
                bytes,
            } if id == request_id && request.kind == RequestKind::Replace => {
                let Some((pending_index, matched, expected_len)) = pending_match.as_mut() else {
                    return Err(TextWorkerError::HelperDied);
                };
                if index != *pending_index
                    || offset != u64::try_from(matched.expansion.len()).unwrap_or(u64::MAX)
                    || bytes.is_empty()
                    || matched.expansion.len().saturating_add(bytes.len()) > *expected_len
                {
                    return Err(TextWorkerError::HelperDied);
                }
                matched.expansion.extend_from_slice(&bytes);
                if matched.expansion.len() == *expected_len {
                    let (_, complete, _) =
                        pending_match.take().ok_or(TextWorkerError::HelperDied)?;
                    matches.push(complete);
                }
            }
            FromHelper::Done {
                request_id: id,
                total_matches,
                output_len,
            } if id == request_id => {
                if pending_match.is_some() {
                    return Err(TextWorkerError::HelperDied);
                }
                totals = (total_matches, output_len);
                break;
            }
            FromHelper::Failed {
                request_id: id,
                failure,
            } if id == request_id => {
                return Err(TextWorkerError::Failure(failure));
            }
            _ => return Err(TextWorkerError::HelperDied),
        }
    }
    let (total_matches, output_len) = totals;
    // Count cross-check: pages shipped must equal the reported total when
    // unbounded, or exactly the report cap when the total exceeds it.
    let capped = request.report_matches != usize::MAX;
    let remaining = total_matches.saturating_sub(request.skip_matches);
    let expected = if capped {
        remaining.min(u64::try_from(request.report_matches).unwrap_or(u64::MAX))
    } else {
        remaining
    };
    if u64::try_from(matches.len()).unwrap_or(u64::MAX) != expected {
        return Err(TextWorkerError::HelperDied);
    }
    Ok(HelperRun {
        matches,
        total_matches,
        output_len,
    })
}

fn send_frame(stdin: &mut impl Write, frame: &ToHelper) -> Result<(), TextWorkerError> {
    let bytes = encode(frame).map_err(TextWorkerError::Frame)?;
    write_frame(stdin, &bytes).map_err(|_| TextWorkerError::HelperDied)
}

use std::io::Write;

fn recv_frame(
    replies: &std::sync::mpsc::Receiver<Result<FromHelper, FrameError>>,
    started: Instant,
    cancelled: &AtomicBool,
) -> Result<FromHelper, TextWorkerError> {
    loop {
        if cancelled.load(Ordering::Relaxed) {
            return Err(TextWorkerError::Cancelled);
        }
        let remaining = HELPER_DEADLINE.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(TextWorkerError::Deadline);
        }
        match replies.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(Ok(message)) => return Ok(message),
            Ok(Err(error)) => return Err(TextWorkerError::Frame(error)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(TextWorkerError::HelperDied);
            }
        }
    }
}

/// Convenience used by tests and diagnostics: verify a helper is present
/// and answers the handshake with the pinned engine identity.
///
/// # Errors
///
/// [`TextWorkerError`] when the helper is missing, dies, or mismatches.
pub fn probe_helper() -> Result<(), TextWorkerError> {
    let Some(helper) = sibling_helper_path() else {
        return Err(TextWorkerError::Unavailable {
            reason: "helper-not-installed",
        });
    };
    let mut child = Command::new(&helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn_with_group()
        .map_err(|_| TextWorkerError::Unavailable {
            reason: "helper-spawn-failed",
        })?;
    let outcome = (|| {
        let started = Instant::now();
        let mut stdin = child.stdin.take().ok_or(TextWorkerError::HelperDied)?;
        let handshake = encode(&ToHelper::Handshake {
            protocol_version: protocol::PROTOCOL_VERSION,
            engine_version: flavite_text_search::engine_version(),
        })
        .map_err(TextWorkerError::Frame)?;
        write_frame(&mut stdin, &handshake).map_err(|_| TextWorkerError::HelperDied)?;
        drop(stdin);
        let stdout = child.stdout.take().ok_or(TextWorkerError::HelperDied)?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = stdout;
            loop {
                match read_reply(&mut reader) {
                    Ok(message) => {
                        if tx.send(Ok(message)).is_err() {
                            break;
                        }
                    }
                    Err(FrameError::Truncated) => break,
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        });
        let cancelled = AtomicBool::new(false);
        match recv_frame(&rx, started, &cancelled) {
            Ok(FromHelper::HandshakeReply(Ok((p, e))))
                if p == protocol::PROTOCOL_VERSION
                    && e == flavite_text_search::engine_version() =>
            {
                Ok(())
            }
            Ok(FromHelper::HandshakeReply(Ok(_))) => Err(TextWorkerError::Unavailable {
                reason: "engine-mismatch",
            }),
            Ok(FromHelper::HandshakeReply(Err(failure))) => Err(TextWorkerError::Unavailable {
                reason: failure.code(),
            }),
            Ok(_) => Err(TextWorkerError::HelperDied),
            Err(error) => Err(error),
        }
    })();
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

/// Spawn with the platform's process-group isolation (Unix `process_group`;
/// the Windows Job-object equivalent lands with its platform slice, §6.2).
trait SpawnWithGroup {
    fn spawn_with_group(&mut self) -> std::io::Result<Child>;
}

#[cfg(unix)]
impl SpawnWithGroup for Command {
    fn spawn_with_group(&mut self) -> std::io::Result<Child> {
        use std::os::unix::process::CommandExt as _;
        self.process_group(0).spawn()
    }
}

#[cfg(not(unix))]
impl SpawnWithGroup for Command {
    fn spawn_with_group(&mut self) -> std::io::Result<Child> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "regex process isolation is unavailable on this platform",
        ))
    }
}

// The frame bound comfortably admits two chunks of overhead.
const _: () = assert!(FRAME_MAX_BYTES >= CHUNK_BYTES * 2, "chunks must fit frames");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permits_reserve_one_slot_for_interactive_work() {
        let permits = HelperPermits::new();
        // One bulk admission succeeds; a second is refused even while a
        // slot remains free — bulk can never occupy the interactive
        // reservation (§6.2 D-14).
        assert!(permits.acquire(HelperPermitClass::Bulk));
        assert!(
            !permits.acquire(HelperPermitClass::Bulk),
            "bulk is capped at its own reservation"
        );
        // The interactive-exclusive slot still admits interactive work.
        assert!(permits.acquire(HelperPermitClass::Interactive));
        // Both HELPER_SLOTS are now taken (one bulk, one interactive).
        assert!(!permits.acquire(HelperPermitClass::Interactive));
        // Releasing the bulk slot lets interactive borrow it — the total
        // HELPER_SLOTS bound is what caps concurrency, never bulk hoarding.
        permits.release(HelperPermitClass::Bulk);
        assert!(permits.acquire(HelperPermitClass::Interactive));
        assert!(
            !permits.acquire(HelperPermitClass::Bulk),
            "all slots are interactive now; bulk waits"
        );
        permits.release(HelperPermitClass::Interactive);
        permits.release(HelperPermitClass::Interactive);
        assert!(permits.acquire(HelperPermitClass::Bulk));
        permits.release(HelperPermitClass::Bulk);
    }
}
