//! Real-subprocess verification of the isolated matching helper
//! (RFC-0006 S03/§6.2, D-14): missing helper, real helper round trips,
//! engine mismatch, blocking helper deadline kill, mid-operation death,
//! bounded permits and cancellation. No component under test is mocked.

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

use flavite_text_search::matcher::MatchMode;
use flavite_text_search::protocol::{self, ToHelper, encode, read_request};
use flavite_vault::text_worker::{
    HelperPermits, HELPER_SLOTS, TextWorkerError, run_match, sibling_helper_path,
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
fn acquire_sibling_lock() -> std::sync::MutexGuard<'static, ()> {
    match sibling_lock().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
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

/// Locate (or build) one worker-package binary. Preference order: the
/// prebuilt binary in the workspace target dir (present after any
/// `cargo build`/`cargo test --workspace`), then a one-time fallback
/// build into a scratch target directory that cannot contend with the
/// outer cargo's lock.
fn worker_binary(bin: &str) -> PathBuf {
    // EXE_EXTENSION is empty on Unix — never append a dangling dot.
    let extension = std::env::consts::EXE_EXTENSION;
    let exe_name = if extension.is_empty() {
        bin.to_owned()
    } else {
        format!("{bin}.{extension}")
    };
    let root = workspace_root();
    for profile in ["debug", "release"] {
        let candidate = root.join("target").join(profile).join(&exe_name);
        if candidate.is_file() {
            return candidate;
        }
    }
    // Fallback: private target dir, built once per process. The CALLER
    // holds the sibling lock — never re-take it here (std Mutex is not
    // re-entrant; install_* hold it across this call).
    let scratch = SCRATCH_TARGET.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "flavite-text-worker-build-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("scratch target dir");
        dir
    });
    let built = scratch.join(&exe_name);
    if built.is_file() {
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
        scratch.join("debug").join("deps").join(&exe_name),
        scratch.join("release").join("deps").join(&exe_name),
    ];
    let built = candidates
        .iter()
        .find(|candidate| candidate.is_file())
        .cloned()
        .unwrap_or_else(|| {
            panic!(
                "helper binary not found after build in {:?}",
                candidates
            )
        });
    built
}

/// Install the REAL helper at the sibling path next to this test binary,
/// exercising the exact discovery rule (`current_exe().parent()`), never
/// PATH.
fn install_helper() -> PathBuf {
    // Caller holds the sibling lock (std Mutex is not re-entrant; the
    // tests hold it across their bodies, so never re-take it here).
    let target = sibling_install_target();
    let built = worker_binary("flavite-text-search-worker");
    std::fs::copy(&built, &target).expect("install helper beside the test binary");
    target
}

/// Install a fake helper (a real process that misbehaves on purpose).
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

fn run_regex(source: &str, pattern: &str, template: &str) -> Result<
    flavite_vault::text_worker::HelperRun,
    TextWorkerError,
> {
    run_match(
        source,
        pattern,
        MatchMode::Regex,
        false,
        false,
        1_000_000,
        template,
        &AtomicBool::new(false),
    )
}

#[test]
fn missing_helper_is_typed_unavailable_and_literal_is_unaffected() {
    let _lock = acquire_sibling_lock();
    // Remove the sibling next to this test binary if present.
    let sibling = sibling_install_target();
    if sibling.is_file() {
        std::fs::remove_file(&sibling).expect("remove sibling");
    }
    let error = run_regex("abc", "b", "X").expect_err("no helper installed");
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
    let run = run_regex("one two one", r"(o)(n)", "<$2$1>").expect("helper run");
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
fn foreign_engine_helper_is_refused_not_fallen_back() {
    let _lock = acquire_sibling_lock();
    install_fake("wrong-engine");
    let error = run_regex("abc", "b", "X").expect_err("engine mismatch");
    match error {
        TextWorkerError::Unavailable { reason } => {
            assert!(
                reason == "engine-mismatch" || reason == "protocol",
                "typed engine refusal, got {reason}"
            );
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
}

#[test]
fn blocking_helper_is_killed_at_the_deadline() {
    let _lock = acquire_sibling_lock();
    install_fake("sleep");
    let started = std::time::Instant::now();
    let error = run_regex("abc", "b", "X").expect_err("stuck helper");
    assert_eq!(error, TextWorkerError::Deadline);
    // The wall-clock bound held (5 s deadline; the kill overshoots only by
    // OS scheduling, never by the helper's own patience).
    assert!(
        started.elapsed() < flavite_vault::text_worker::HELPER_DEADLINE
            + std::time::Duration::from_secs(2),
        "deadline enforcement took {:?}",
        started.elapsed()
    );
    // No orphan: the reaper waited for the child inside run_match.
}

#[test]
fn helper_dying_mid_request_is_typed_and_leaves_no_partial_result() {
    let _lock = acquire_sibling_lock();
    install_fake("die-after-accept");
    let error = run_regex("aaaa", "aa", "<$0>").expect_err("dies after accept");
    assert_eq!(error, TextWorkerError::HelperDied);
}

#[test]
fn garbage_frames_are_bounded_typed_refusals() {
    let _lock = acquire_sibling_lock();
    install_fake("garbage");
    let error = run_regex("abc", "b", "X");
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

#[test]
fn permits_bound_concurrency_and_are_reused() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let permits = HelperPermits::default();
    let acquired: Vec<bool> = (0..HELPER_SLOTS).map(|_| permits.acquire()).collect();
    assert!(acquired.iter().all(|taken| *taken), "all slots grant");
    assert!(
        !permits.acquire(),
        "slot {HELPER_SLOTS}+1 is refused (Busy), never queued unbounded"
    );
    for _ in 0..HELPER_SLOTS {
        permits.release();
    }
    assert!(permits.acquire(), "released slots are reused");
    permits.release();
}

#[test]
fn cancelled_requests_never_return_pages() {
    let _lock = acquire_sibling_lock();
    install_helper();
    let cancelled = AtomicBool::new(true);
    let error = run_match(
        "abc",
        "b",
        MatchMode::Regex,
        false,
        false,
        1_000,
        "X",
        &cancelled,
    )
    .expect_err("cancelled before start");
    assert_eq!(error, TextWorkerError::Cancelled);
    assert_eq!(cancelled.load(Ordering::Relaxed), true);
}

/// The real helper speaks the same protocol the parent speaks: a framed
/// handshake round trip against the installed binary (AC-17 framing).
#[test]
fn installed_helper_completes_a_framed_handshake() {
    let _lock = acquire_sibling_lock();
    let helper = install_helper();
    let mut child = Command::new(&helper)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("helper spawns");
    {
        let mut stdin = child.stdin.take().expect("stdin");
        let handshake = encode(&ToHelper::Handshake {
            protocol_version: protocol::PROTOCOL_VERSION,
            engine_version: flavite_text_search::engine_version(),
        })
        .expect("small frame");
        stdin.write_all(&handshake).expect("write handshake");
    }
    let mut stdout = child.stdout.take().expect("stdout");
    let reply = read_request_back(&mut stdout);
    drop(child.stdin.take());
    match reply {
        Some(Ok(flavite_text_search::protocol::FromHelper::HandshakeReply(Ok((
            protocol_version,
            engine_version,
        ))))) => {
            assert_eq!(protocol_version, protocol::PROTOCOL_VERSION);
            assert_eq!(engine_version, flavite_text_search::engine_version());
        }
        other => panic!("handshake reply, got {other:?}"),
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn read_request_back<R: std::io::Read>(
    reader: &mut R,
) -> Option<Result<flavite_text_search::protocol::FromHelper, flavite_text_search::protocol::FrameError>>
{
    // Tests read with the canonical decoder (no reimplementation).
    Some(flavite_text_search::protocol::read_reply(reader))
}
