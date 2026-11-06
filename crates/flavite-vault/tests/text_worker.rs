//! Real-subprocess verification of the isolated matching helper
//! (RFC-0006 S03/§6.2, D-14): missing helper, real find/replace round
//! trips with frozen scopes and report caps, engine mismatch, blocking
//! helper deadline kill, mid-operation death, garbage frames, bounded
//! permits and cancellation. No component under test is mocked.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use flavite_text_search::matcher::MatchMode;
use flavite_text_search::protocol::{self, RequestKind, ToHelper, encode, read_request};
use flavite_vault::text_worker::{
    HELPER_DEADLINE, HelperPermitClass, HelperPermits, HelperRequest, TextWorkerError, run_match,
    run_match_bulk, sibling_helper_path,
};

/// Serialized sibling-path swaps: every test that installs a fake helper
/// replaces the file next to THIS test binary; tests run in one process.
static SIBLING_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// The scratch build directory for fallback builds. A `cargo build`
/// spawned INSIDE a `cargo test` invocation would deadlock on the shared
/// target-dir lock (cargo holds it until the outer command exits), so the
/// fallback builds into this private directory instead.
static SCRATCH_TARGET: OnceLock<PathBuf> = OnceLock::new();

fn sibling_lock() -> &'static Mutex<()> {
    SIBLING_LOCK.get_or_init(|| Mutex::new(()))
}

/// The sibling path WITHOUT an existence check (install targets the file
/// that does not exist yet, which is the point of installing).
fn sibling_install_target() -> PathBuf {
    let exe = std::env::current_exe().expect("current_exe");
    let mut sibling = exe.parent().expect("parent dir").to_path_buf();
    sibling.push("flavite-text-search-worker");
    #[cfg(windows)]
    {
        sibling.set_extension("exe");
    }
    sibling
}

/// Poison-tolerant acquisition: a panicked sibling swap must not cascade
/// failures into every other test in the binary.
struct SiblingGuard {
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl Drop for SiblingGuard {
    fn drop(&mut self) {
        // The sibling path is shared by every integration-test executable
        // in target/debug/deps. Restore the no-helper baseline after each
        // test so workspace test order cannot change unit/API outcomes.
        uninstall_helper();
    }
}

fn acquire_sibling_lock() -> SiblingGuard {
    let guard = match sibling_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    SiblingGuard { _guard: guard }
}

/// The repo root, resolved from this test's own location.
fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

/// The worker package's newest source mtime: any binary older than this
/// is a stale artifact from a previous build (and a stale helper means a
/// stale protocol — precisely the mismatch the parent must refuse).
fn worker_sources_mtime(root: &Path) -> std::time::SystemTime {
    let mut newest = std::time::SystemTime::UNIX_EPOCH;
    for dir in [
        root.join("crates/flavite-text-search-worker/src"),
        root.join("crates/flavite-text-search/src"),
        root.join("crates/flavite-syntax/src"),
    ] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Ok(meta) = entry.metadata()
                && let Ok(mtime) = meta.modified()
                && mtime > newest
            {
                newest = mtime;
            }
        }
    }
    newest
}

fn newer(binary: &Path, sources: std::time::SystemTime) -> bool {
    binary
        .metadata()
        .and_then(|meta| meta.modified())
        .is_ok_and(|mtime| mtime > sources)
}

/// Locate (or build) one worker-package binary. Preference order: the
/// prebuilt binary in the workspace target dir when it is NEWER than the
/// helper's own sources, then a one-time fallback build into a scratch
/// target directory that cannot contend with the outer cargo's lock.
fn worker_binary(bin: &str) -> PathBuf {
    // EXE_EXTENSION is empty on Unix — never append a dangling dot.
    let extension = std::env::consts::EXE_EXTENSION;
    let exe_name = if extension.is_empty() {
        bin.to_owned()
    } else {
        format!("{bin}.{extension}")
    };
    let root = workspace_root();
    let sources = worker_sources_mtime(&root);
    for profile in ["debug", "release"] {
        let candidate = root.join("target").join(profile).join(&exe_name);
        if candidate.is_file() && newer(&candidate, sources) {
            return candidate;
        }
    }
    // Stale or absent: build into the private scratch target once per
    // process. The CALLER holds the sibling lock — never re-take it here.
    let scratch = SCRATCH_TARGET.get_or_init(|| {
        let dir =
            std::env::temp_dir().join(format!("flavite-text-worker-build-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch target dir");
        dir
    });
    let built = scratch.join(&exe_name);
    if built.is_file() && newer(&built, sources) {
        return built;
    }
    let output = Command::new(env!("CARGO"))
        .args(["build", "-p", "flavite-text-search-worker", "--bin", bin])
        .env("CARGO_TARGET_DIR", scratch)
        .current_dir(&root)
        .output()
        .expect("cargo build runs");
    assert!(
        output.status.success(),
        "helper build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Cargo may hardlink the binary into `deps/` rather than the profile
    // root; locate whichever spelling exists.
    let candidates = [
        built.clone(),
        scratch.join("debug").join(bin),
        scratch.join("debug").join("deps").join(&exe_name),
        scratch.join("release").join("deps").join(&exe_name),
    ];
    candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .unwrap_or_else(|| panic!("helper binary not found after build: {candidates:?}"))
}

/// Install the REAL helper at the sibling path next to this test binary,
/// exercising the exact discovery rule (`current_exe().parent()`), never
/// PATH. Returns the installed path (identity-checkable).
pub(crate) fn install_helper() -> PathBuf {
    // Caller holds the sibling lock (std Mutex is not re-entrant; the
    // tests hold it across their bodies, so never re-take it here).
    let target = sibling_install_target();
    let built = worker_binary("flavite-text-search-worker");
    std::fs::copy(&built, &target).expect("install helper beside the test binary");
    target
}

/// Remove any installed sibling so the next admission sees the honest
/// "not installed" state.
fn uninstall_helper() {
    let target = sibling_install_target();
    if target.is_file() {
        std::fs::remove_file(&target).expect("remove sibling");
    }
}

/// Install a fake helper that runs `misbehaving_helper <mode>`: a small
/// exec wrapper selects the misbehavior mode because the parent spawns
/// the sibling with no arguments (the discovery contract).
#[cfg(unix)]
fn install_fake(mode: &str) {
    use std::os::unix::fs::PermissionsExt;
    // Caller holds the sibling lock (never re-take it here).
    let built = worker_binary("misbehaving_helper");
    let target = sibling_install_target();
    let script = format!(
        "#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n",
        built.display(),
        mode
    );
    std::fs::write(&target, script).expect("write fake wrapper");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .expect("make wrapper executable");
}

fn replace_request<'a>(source: &'a str, pattern: &'a str, template: &'a str) -> HelperRequest<'a> {
    HelperRequest {
        kind: RequestKind::Replace,
        source,
        pattern,
        mode: MatchMode::Regex,
        case_sensitive: false,
        whole_word: false,
        scope: None,
        max_matches: 1_000_000,
        report_matches: usize::MAX,
        skip_matches: 0,
        max_output: 32 * 1024 * 1024,
        template_raw: template,
    }
}

fn find_request<'a>(source: &'a str, pattern: &'a str) -> HelperRequest<'a> {
    HelperRequest {
        kind: RequestKind::Find,
        report_matches: 10_000,
        skip_matches: 0,
        max_output: 0,
        template_raw: "",
        ..replace_request(source, pattern, "")
    }
}

fn run(
    request: &HelperRequest<'_>,
) -> Result<flavite_vault::text_worker::HelperRun, TextWorkerError> {
    run_match(request, &AtomicBool::new(false))
}

#[test]
fn missing_helper_is_typed_unavailable_and_literal_is_unaffected() {
    let _lock = acquire_sibling_lock();
    uninstall_helper();
    let error = run(&replace_request("abc", "b", "X")).expect_err("no helper installed");
    assert_eq!(
        error,
        TextWorkerError::Unavailable {
            reason: "helper-not-installed"
        }
    );
    // The literal path never touches the helper: proven at the
    // text_edit layer (in-process matcher) and by the S01 suites.
}

#[test]
fn real_helper_matches_regex_and_reports_sound_totals() {
    let _lock = acquire_sibling_lock();
    install_helper();
    // Rust regex (and D-05) has no backreferences: capture pairs exercise
    // group expansion instead.
    let run = run(&replace_request("one two one", "(o)(n)", "<$2$1>")).expect("helper run");
    assert_eq!(run.total_matches, 2);
    assert_eq!(run.output_len, 15, "11 - 2*2 span + 2*4 expansion");
    // Ascending, non-overlapping spans in original coordinates.
    assert_eq!(run.matches[0].start, 0);
    assert_eq!(run.matches[0].end, 2);
    assert_eq!(run.matches[1].start, 8);
    assert_eq!(run.matches[1].end, 10);
    // Group expansions travel for the parent's own cross-check.
    assert_eq!(run.matches[0].expansion, b"<no>".to_vec());
    assert_eq!(run.matches[1].expansion, b"<no>".to_vec());
}

#[test]
fn find_requests_ship_spans_never_expansions_and_prove_exact_totals() {
    let _lock = acquire_sibling_lock();
    install_helper();
    // `a*` on "aab": (0,2) "aa", (2,2) empty, (3,3) EOF-empty — the exact
    // D-05 empty-match contract through the real helper.
    let run = run(&find_request("aab", "a*")).expect("find run");
    assert_eq!(run.total_matches, 3);
    let spans: Vec<(u64, u64)> = run.matches.iter().map(|hit| (hit.start, hit.end)).collect();
    assert_eq!(spans, vec![(0, 2), (2, 2), (3, 3)]);
    assert!(
        run.matches.iter().all(|hit| hit.expansion.is_empty()),
        "a find never ships expansions"
    );
}

#[test]
fn empty_regex_query_is_valid_and_emits_each_scalar_boundary_once() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let run = run(&find_request("a😀", "")).expect("empty regex is valid");
    let spans: Vec<(u64, u64)> = run.matches.iter().map(|hit| (hit.start, hit.end)).collect();
    assert_eq!(spans, vec![(0, 0), (1, 1), (5, 5)]);
    assert_eq!(run.total_matches, 3);
}

#[test]
fn scoped_regex_find_uses_whole_text_boundaries_and_freezes_the_range() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let mut request = find_request("helloworld and world", "world");
    request.whole_word = true;
    // Frozen scope covering "world" inside "helloworld": whole-word
    // filtering consults the FULL text, so this match is not whole-word.
    request.scope = Some((5, 10));
    let scoped_run = run(&request).expect("scoped run");
    assert_eq!(scoped_run.total_matches, 0, "inside a larger word");
    // The second "world" is a real whole word; the scope excludes it.
    request.scope = Some((1, 12));
    let first_scope_run = run(&request).expect("first-scope run");
    assert_eq!(first_scope_run.total_matches, 0);
    request.scope = Some((15, 20));
    let second_scope_run = run(&request).expect("second-scope run");
    assert_eq!(second_scope_run.total_matches, 1);
    assert_eq!(
        second_scope_run.matches[0].start, 15,
        "spans are whole-text offsets"
    );
}

#[test]
fn report_cap_counts_every_match_and_ships_only_the_cap() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let mut request = find_request("abababab", "a");
    request.report_matches = 3;
    let run = run(&request).expect("capped find");
    assert_eq!(run.matches.len(), 3, "page cap shipped");
    assert_eq!(run.total_matches, 4, "exact total still proven");
}

#[test]
fn ordinal_continuation_ships_the_next_window_with_the_same_exact_total() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let mut request = find_request("a-a-a-a-a", "a");
    request.report_matches = 2;
    request.skip_matches = 2;
    let run = run(&request).expect("continued find");
    assert_eq!(run.total_matches, 5);
    assert_eq!(run.matches.len(), 2);
    let spans = run.matches.iter().map(|hit| hit.start).collect::<Vec<_>>();
    assert_eq!(spans, vec![4, 6]);
}

#[test]
fn large_single_expansion_streams_across_bounded_frames() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let source = "a".repeat(128 * 1024);
    let run = run(&replace_request(&source, "a+", "$0")).expect("large expansion");
    assert_eq!(run.total_matches, 1);
    assert_eq!(run.matches.len(), 1);
    assert_eq!(run.matches[0].expansion.len(), source.len());
    assert_eq!(run.matches[0].expansion, source.as_bytes());
    assert_eq!(run.output_len, source.len() as u64);
}

#[test]
fn replace_output_bound_refuses_before_any_page_ships() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let mut request = replace_request("aaaa", "a", "$0$0$0$0");
    request.max_output = 8; // 4 matches × expansion 4 = 16 > 8
    let error = run(&request).expect_err("over output bound");
    assert_eq!(
        error,
        TextWorkerError::Failure(protocol::FailureCode::Limit)
    );
}

#[test]
fn foreign_engine_helper_is_refused_not_fallen_back() {
    let _lock = acquire_sibling_lock();
    install_fake("wrong-engine");
    let error = run(&replace_request("abc", "b", "X")).expect_err("engine mismatch");
    match error {
        TextWorkerError::Unavailable { reason } => {
            assert_eq!(reason, "engine-mismatch", "typed engine refusal");
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn blocking_helper_is_killed_at_the_deadline() {
    let _lock = acquire_sibling_lock();
    install_fake("sleep");
    let started = std::time::Instant::now();
    let error = run(&replace_request("abc", "b", "X")).expect_err("stuck helper");
    assert_eq!(error, TextWorkerError::Deadline);
    // The wall-clock bound held (5 s deadline; the kill overshoots only by
    // OS scheduling, never by the helper's own patience).
    assert!(
        started.elapsed() < HELPER_DEADLINE + std::time::Duration::from_secs(2),
        "deadline enforcement took {:?}",
        started.elapsed()
    );
    // No orphan: the reaper waited for the child inside run_match.
}

#[test]
fn helper_dying_mid_request_is_typed_and_leaves_no_partial_result() {
    let _lock = acquire_sibling_lock();
    install_fake("die-after-accept");
    let error = run(&replace_request("aaaa", "aa", "<$0>")).expect_err("dies after accept");
    assert_eq!(error, TextWorkerError::HelperDied);
}

#[test]
fn garbage_frames_are_bounded_typed_refusals() {
    let _lock = acquire_sibling_lock();
    install_fake("garbage");
    let error = run(&replace_request("abc", "b", "X"));
    assert!(
        matches!(
            error,
            Err(TextWorkerError::HelperDied)
                | Err(TextWorkerError::Frame(
                    flavite_text_search::protocol::FrameError::PeerOversize { .. }
                ))
        ),
        "bounded decoder refuses garbage, got {error:?}"
    );
}

/// Editor-shaped message kinds (cursor movement, Markdown commands, edits,
/// file operations) have NO protocol kind at all: an unknown frame tag is
/// a typed protocol death, not an executed operation. This is the helper
/// half of the S03 "inject unsupported kinds and assert rejection" gate.
#[test]
fn unsupported_message_kinds_are_structurally_impossible() {
    let _lock = acquire_sibling_lock();
    let helper = install_helper();
    let mut child = Command::new(&helper)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("helper spawns");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        // A frame tagged 0x07 — a kind this protocol does not define
        // ("MoveCursor" shaped). The helper must exit on the typed decode
        // refusal rather than act on a guess.
        let forged = encode(&ToHelper::Cancel { request_id: 42 })
            .expect("frame")
            .as_slice()
            .to_vec();
        let mut tagged = forged.clone();
        // Replace the payload's first byte (the tag) with the unknown 0x07
        // while keeping the length prefix coherent.
        tagged[4] = 0x07;
        // Also send one well-formed cancel after it is consumed by death;
        // the process must already be exiting from the unknown tag.
        let _ = std::io::Write::write_all(stdin, &tagged);
        let _ = std::io::Write::write_all(stdin, &forged);
    }
    let status = child.wait().expect("helper exits");
    assert!(!status.success(), "unknown kinds die as typed failures");
    // And a fresh real helper still answers correctly afterwards: the
    // poisoned instance never re-enters service (it is killed and reaped
    // by run_match; here we re-probe with a new admission).
    let run = run(&find_request("abc", "b")).expect("fresh instance serves");
    assert_eq!(run.total_matches, 1);
}

#[test]
fn permits_bound_concurrency_per_class_and_are_reused() {
    let _lock = acquire_sibling_lock();
    let permits = HelperPermits::new();
    assert!(permits.acquire(HelperPermitClass::Bulk));
    assert!(
        !permits.acquire(HelperPermitClass::Bulk),
        "bulk cannot occupy the interactive reservation"
    );
    assert!(permits.acquire(HelperPermitClass::Interactive));
    assert!(
        !permits.acquire(HelperPermitClass::Interactive),
        "all slots busy: Busy, never queued unbounded"
    );
    permits.release(HelperPermitClass::Bulk);
    permits.release(HelperPermitClass::Interactive);
    assert!(permits.acquire(HelperPermitClass::Interactive));
    permits.release(HelperPermitClass::Interactive);
}

#[test]
fn bulk_admission_uses_the_bulk_class() {
    let _lock = acquire_sibling_lock();
    install_helper();
    // One bulk operation runs to completion through the bulk entry point;
    // its class reservation is released afterwards (proven by a second
    // bulk admission succeeding).
    let first =
        run_match_bulk(&find_request("abc", "b"), &AtomicBool::new(false)).expect("first bulk run");
    assert_eq!(first.total_matches, 1);
    let second = run_match_bulk(&find_request("abc", "c"), &AtomicBool::new(false))
        .expect("bulk slot was released");
    assert_eq!(second.total_matches, 1);
}

#[test]
fn cancelled_requests_never_return_pages() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let cancelled = AtomicBool::new(true);
    let error =
        run_match(&find_request("abc", "b"), &cancelled).expect_err("cancelled before completion");
    assert_eq!(error, TextWorkerError::Cancelled);
    assert!(cancelled.load(Ordering::Relaxed));
}

#[test]
fn stale_scope_is_refused_locally_before_the_helper_sees_it() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let mut request = find_request("abc", "b");
    request.scope = Some((2, 1)); // reversed: not a legal frozen range
    let error = run(&request).expect_err("reversed scope");
    assert_eq!(
        error,
        TextWorkerError::Failure(protocol::FailureCode::Source)
    );
    // A mid-scalar endpoint is refused the same way (é starts at 0 here).
    let mut request = find_request("é", ".");
    request.scope = Some((1, 1));
    let error = run(&request).expect_err("mid-scalar scope start");
    assert_eq!(
        error,
        TextWorkerError::Failure(protocol::FailureCode::Source)
    );
}

/// The real helper speaks the same protocol the parent speaks: a framed
/// handshake round trip against the installed binary (AC-17 framing).
#[test]
fn installed_helper_completes_a_framed_handshake() {
    let _lock = acquire_sibling_lock();
    let helper = install_helper();
    assert_eq!(
        sibling_helper_path().as_deref(),
        Some(helper.as_path()),
        "discovery finds exactly the installed sibling"
    );
    let mut child = Command::new(&helper)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("helper spawns");
    {
        let stdin = child.stdin.as_mut().expect("stdin");
        let handshake = encode(&ToHelper::Handshake {
            protocol_version: protocol::PROTOCOL_VERSION,
            engine_version: flavite_text_search::engine_version(),
        })
        .expect("small frame");
        std::io::Write::write_all(stdin, &handshake).expect("write handshake");
    }
    let mut stdout = child.stdout.take().expect("stdout");
    let reply = flavite_text_search::protocol::read_reply(&mut stdout);
    drop(child.stdin.take());
    match reply {
        Ok(flavite_text_search::protocol::FromHelper::HandshakeReply(Ok((
            protocol_version,
            engine_version,
        )))) => {
            assert_eq!(protocol_version, protocol::PROTOCOL_VERSION);
            assert_eq!(engine_version, flavite_text_search::engine_version());
        }
        other => panic!("handshake reply, got {other:?}"),
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Helper-side read of a request frame (used only to build the forged
/// frames above); the canonical decoder is imported, never reimplemented.
#[allow(dead_code)]
fn decode_a_request(bytes: &[u8]) -> Result<ToHelper, flavite_text_search::protocol::FrameError> {
    let mut cursor = std::io::Cursor::new(bytes.to_vec());
    read_request(&mut cursor)
}
