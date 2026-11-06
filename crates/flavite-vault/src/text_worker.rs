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
//! Revision checks and `EditOp` construction live in `text_edit.rs`/the
//! Session: the helper returns match/expansion data, never an
//! authoritative prepared buffer candidate.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use flavite_text_search::matcher::MatchMode;
use flavite_text_search::protocol::{
    self, FailureCode, FromHelper, FrameError, FRAME_MAX_BYTES, SOURCE_MAX_BYTES, ToHelper,
    WireMatch, encode, read_reply, write_frame,
};
use flavite_text_search::template::ReplacementTemplate;

/// Wall-clock deadline for one helper operation (§6.2). The parent kills
/// and reaps the helper after this; indivisible OS scheduling may add a
/// small overshoot, which is honest visible delay, not false completion.
pub const HELPER_DEADLINE: Duration = Duration::from_secs(5);

/// Concurrent helper operations (NFR-10): one slot reserved for
/// interactive note commands, one for bulk source operations.
pub const HELPER_SLOTS: usize = 2;

/// Source chunks streamed per frame (well inside the frame bound).
const CHUNK_BYTES: usize = 32 * 1024;

/// Why regex work could not run in the helper. Typed, redacted — never
/// pattern/source text (§6.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextWorkerError {
    /// The installed sibling helper is missing or unusable. Literal
    /// matching remains available (RegexUnavailable state, §6.8).
    Unavailable {
        reason: &'static str,
    },
    /// All helper slots are occupied; admission is refused (Busy), never
    /// queued unbounded.
    Busy,
    /// The operation exceeded [`HELPER_DEADLINE`] and the helper was
    /// killed; only that request is invalidated.
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

/// Bounded concurrency for helper operations. Acquisition is nonblocking;
/// `None` means every slot is busy (Busy, not a wait).
#[derive(Debug, Default)]
pub struct HelperPermits {
    slots: Mutex<usize>,
}

impl HelperPermits {
    /// Try to take one slot; `false` when all [`HELPER_SLOTS`] are taken
    /// (Busy, never an unbounded wait).
    pub fn acquire(&self) -> bool {
        let mut slots = self.slots.lock().expect("helper permit lock");
        if *slots >= HELPER_SLOTS {
            return false;
        }
        *slots += 1;
        true
    }

    /// Return one slot; released slots are immediately reusable.
    pub fn release(&self) {
        let mut slots = self.slots.lock().expect("helper permit lock");
        *slots = slots.saturating_sub(1);
    }
}

/// The process-wide permit set (one per vault; helpers are independent
/// processes, so the bound is per vault instance).
#[must_use]
pub fn helper_permits() -> &'static HelperPermits {
    static PERMITS: std::sync::OnceLock<HelperPermits> = std::sync::OnceLock::new();
    PERMITS.get_or_init(HelperPermits::default)
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

/// One completed helper match run: ascending matches with per-match
/// expansions plus the checked totals the helper reported.
#[derive(Debug, Clone)]
pub struct HelperRun {
    pub matches: Vec<WireMatch>,
    pub total_matches: u64,
    pub output_len: u64,
}

/// Run one match request in a freshly spawned helper under a permit.
///
/// `template_raw` is validated locally AND in the helper; the parent
/// re-checks the reported output length against its own template math, so
/// a buggy or hostile helper cannot hand the session an unsound candidate.
///
/// # Errors
///
/// [`TextWorkerError`] — the helper missing/dead/late/over-queue, a typed
/// helper refusal, or a frame error. Zero partial results escape: pages
/// are returned only behind the terminal `Done` proof.
#[allow(clippy::too_many_arguments)] // the request fields mirror the wire Begin frame
pub fn run_match(
    source: &str,
    pattern: &str,
    mode: MatchMode,
    case_sensitive: bool,
    whole_word: bool,
    max_matches: usize,
    template_raw: &str,
    cancelled: &AtomicBool,
) -> Result<HelperRun, TextWorkerError> {
    let Some(helper) = sibling_helper_path() else {
        return Err(TextWorkerError::Unavailable {
            reason: "helper-not-installed",
        });
    };
    if !helper_permits().acquire() {
        return Err(TextWorkerError::Busy);
    }
    let outcome = run_match_locked(
        &helper,
        source,
        pattern,
        mode,
        case_sensitive,
        whole_word,
        max_matches,
        template_raw,
        cancelled,
    );
    helper_permits().release();
    outcome
}

#[allow(clippy::too_many_arguments)]
fn run_match_locked(
    helper: &Path,
    source: &str,
    pattern: &str,
    mode: MatchMode,
    case_sensitive: bool,
    whole_word: bool,
    max_matches: usize,
    template_raw: &str,
    cancelled: &AtomicBool,
) -> Result<HelperRun, TextWorkerError> {
    if source.len() as u64 > SOURCE_MAX_BYTES {
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
    let result = drive_helper(
        &mut child,
        request_id,
        started,
        source,
        pattern,
        mode,
        case_sensitive,
        whole_word,
        max_matches,
        template_raw,
        cancelled,
    );
    // Whatever happened, the helper instance never serves again.
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn next_request_id() -> u64 {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[allow(clippy::too_many_arguments)]
fn drive_helper(
    child: &mut Child,
    request_id: u64,
    started: Instant,
    source: &str,
    pattern: &str,
    mode: MatchMode,
    case_sensitive: bool,
    whole_word: bool,
    max_matches: usize,
    template_raw: &str,
    cancelled: &AtomicBool,
) -> Result<HelperRun, TextWorkerError> {
    let Some(mut stdin) = child.stdin.take() else {
        return Err(TextWorkerError::HelperDied);
    };
    let mut send = |frame: &[u8]| -> Result<(), TextWorkerError> {
        write_frame(&mut stdin, frame).map_err(|_| TextWorkerError::HelperDied)
    };
    let begin = encode(&ToHelper::Begin {
        request_id,
        mode,
        case_sensitive,
        whole_word,
        max_matches: max_matches as u64,
        source_total: source.len() as u64,
        pattern: pattern.as_bytes().to_vec(),
    })
    .map_err(TextWorkerError::Frame)?;
    send(&begin)?;
    // Template validation happens locally first: a local refusal is typed
    // and exact, and the helper re-validates against its own copy.
    let grammar = match mode {
        MatchMode::Literal => flavite_text_search::template::TemplateGrammar::Literal,
        MatchMode::Regex => flavite_text_search::template::TemplateGrammar::Dollar,
    };
    ReplacementTemplate::new(template_raw, grammar).map_err(|_| {
        TextWorkerError::Failure(FailureCode::Template)
    })?;
    let template = encode(&ToHelper::Template {
        request_id,
        template: template_raw.as_bytes().to_vec(),
    })
    .map_err(TextWorkerError::Frame)?;
    send(&template)?;
    for (offset, chunk) in source.as_bytes().chunks(CHUNK_BYTES).enumerate() {
        if cancelled.load(Ordering::Relaxed) {
            let cancel = encode(&ToHelper::Cancel { request_id })
                .map_err(TextWorkerError::Frame)?;
            let _ = send(&cancel);
            return Err(TextWorkerError::Cancelled);
        }
        let frame = encode(&ToHelper::SourceChunk {
            request_id,
            offset: (offset * CHUNK_BYTES) as u64,
            bytes: chunk.to_vec(),
        })
        .map_err(TextWorkerError::Frame)?;
        send(&frame)?;
    }
    let end = encode(&ToHelper::EndOfSource { request_id })
        .map_err(TextWorkerError::Frame)?;
    send(&end)?;
    drop(stdin);

    // Reader side: bounded decoder + deadline. The helper is killed and
    // reaped on expiry; only this request is invalidated (§6.2).
    let Some(stdout) = child.stdout.take() else {
        return Err(TextWorkerError::HelperDied);
    };
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

    let mut matches = Vec::new();
    let mut handshaken = false;
    let mut accepted = false;
    let totals;
    loop {
        if cancelled.load(Ordering::Relaxed) {
            // Stdin is already closed at this stage; the caller kills and
            // reaps the instance in `run_match_locked`, so no orphan and
            // no partial result escapes the cancelled request.
            return Err(TextWorkerError::Cancelled);
        }
        let remaining = HELPER_DEADLINE.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(TextWorkerError::Deadline);
        }
        let message = match reply_rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
            Ok(message) => message,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(TextWorkerError::HelperDied);
            }
        };
        let message = message.map_err(TextWorkerError::Frame)?;
        match message {
            FromHelper::HandshakeReply(Ok((protocol_version, engine_version))) => {
                if protocol_version != protocol::PROTOCOL_VERSION
                    || engine_version != flavite_text_search::engine_version()
                {
                    // A foreign engine is refused, never fallen back from.
                    return Err(TextWorkerError::Unavailable {
                        reason: "engine-mismatch",
                    });
                }
                handshaken = true;
            }
            FromHelper::HandshakeReply(Err(failure)) => {
                return Err(TextWorkerError::Unavailable {
                    reason: failure.code(),
                });
            }
            FromHelper::Accepted { request_id: id } if id == request_id => {
                if !handshaken {
                    return Err(TextWorkerError::HelperDied);
                }
                accepted = true;
            }
            FromHelper::Accepted { .. } => return Err(TextWorkerError::HelperDied),
            FromHelper::Page {
                request_id: id,
                matches: page,
            } => {
                if !accepted || id != request_id {
                    return Err(TextWorkerError::HelperDied);
                }
                matches.extend(page);
            }
            FromHelper::Done {
                request_id: id,
                total_matches,
                output_len,
            } => {
                if !accepted || id != request_id {
                    return Err(TextWorkerError::HelperDied);
                }
                totals = Some((total_matches, output_len));
                break;
            }
            FromHelper::Failed {
                request_id: id,
                failure,
            } => {
                if !handshaken || (!accepted && failure != FailureCode::Cancelled) || id != request_id
                {
                    return Err(TextWorkerError::HelperDied);
                }
                return Err(TextWorkerError::Failure(failure));
            }
        }
    }
    let Some((total_matches, output_len)) = totals else {
        return Err(TextWorkerError::HelperDied);
    };
    if total_matches as usize != matches.len() {
        return Err(TextWorkerError::HelperDied);
    }
    Ok(HelperRun {
        matches,
        total_matches,
        output_len,
    })
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
        loop {
            let remaining = HELPER_DEADLINE.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(TextWorkerError::Deadline);
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(50))) {
                Ok(Ok(FromHelper::HandshakeReply(Ok((p, e)))))
                    if p == protocol::PROTOCOL_VERSION
                        && e == flavite_text_search::engine_version() =>
                {
                    return Ok(());
                }
                Ok(Ok(FromHelper::HandshakeReply(Ok(_)))) => {
                    return Err(TextWorkerError::Unavailable {
                        reason: "engine-mismatch",
                    });
                }
                Ok(Ok(FromHelper::HandshakeReply(Err(failure)))) => {
                    return Err(TextWorkerError::Unavailable {
                        reason: failure.code(),
                    });
                }
                Ok(Ok(_)) => return Err(TextWorkerError::HelperDied),
                Ok(Err(error)) => return Err(TextWorkerError::Frame(error)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(TextWorkerError::HelperDied);
                }
            }
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
        self.spawn()
    }
}

// The frame bound comfortably admits two chunks of overhead.
const _: () = assert!(FRAME_MAX_BYTES >= CHUNK_BYTES * 2, "chunks must fit frames");
