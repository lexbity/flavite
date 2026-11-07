//! Retained replacement primitive tests (RFC-0006 S08/D-10/§6.5).
//!
//! The stronger save primitive MUST retain the actual displaced object,
//! never emulate exchange by unlinking, refuse NonSupported filesystems, and
//! classify an external-writer race at the exchange (displaced mismatch)
//! without discarding the racing bytes.

use flavite_fs::{
    CrashMode, FaultBoundary, FaultFs, FaultKind, FaultScript, Fs, FsError, MemFs,
    RetainedReplaceOutcome, RetainedReplaceRequest,
};
use std::path::{Path, PathBuf};

fn setup(path: &str, prior: &[u8]) -> (MemFs, PathBuf) {
    let fs = MemFs::new();
    let root = Path::new("/vault");
    fs.create_dir_all(root).unwrap();
    let full = root.join(path);
    if !prior.is_empty() || path.ends_with(".md") {
        fs.write_new(&full, prior).unwrap();
        fs.fsync_file(&full).unwrap();
        fs.fsync_dir(root).unwrap();
    }
    (fs, full)
}

fn replace_request(
    target: PathBuf,
    expected: Option<&[u8]>,
    bytes: &[u8],
) -> RetainedReplaceRequest {
    RetainedReplaceRequest {
        target,
        expected_base_hash: expected.map(|b| *blake3::hash(b).as_bytes()),
        bytes: bytes.to_vec(),
    }
}

#[test]
fn mem_replaces_and_retains_the_exact_prior_object() {
    let (fs, target) = setup("a.md", b"prior");
    let outcome = fs
        .replace_preserving_displaced(&replace_request(target.clone(), Some(b"prior"), b"after"))
        .expect("replace");
    let RetainedReplaceOutcome::Replaced {
        displaced,
        displaced_matches_base,
    } = outcome
    else {
        panic!("expected Replaced");
    };
    assert!(displaced_matches_base);
    assert_eq!(fs.read_if_exists(&target).unwrap().unwrap(), b"after");
    let displaced_bytes = fs.read_if_exists(&displaced.path).unwrap().unwrap();
    assert_eq!(displaced_bytes, b"prior");
    assert_eq!(displaced.hash, *blake3::hash(b"prior").as_bytes());
    assert_eq!(displaced.len, 5);
    // The staging sibling directory is private and distinct.
    let parent = target.parent().unwrap();
    let entries = fs.read_dir(parent).unwrap();
    assert!(
        entries
            .iter()
            .filter(|e| e.is_dir)
            .all(|e| { e.name.to_string_lossy().starts_with(".flavite-op-") })
    );
}

#[test]
fn base_mismatch_refuses_without_touching_target_or_leaving_staging() {
    let (fs, target) = setup("a.md", b"prior");
    let err = fs
        .replace_preserving_displaced(&replace_request(target.clone(), Some(b"other"), b"after"))
        .expect_err("expected base mismatch");
    assert!(matches!(err, FsError::BaseMismatch { .. }));
    assert_eq!(fs.read_if_exists(&target).unwrap().unwrap(), b"prior");
    let parent = target.parent().unwrap();
    assert!(
        fs.read_dir(parent)
            .unwrap()
            .iter()
            .all(|e| e.name.to_string_lossy() != ".flavite-op-"),
        "no staging residue after refusal"
    );
}

#[test]
fn absent_expected_base_is_a_typed_missing_refusal() {
    let (fs, target) = setup("a.md", b"prior");
    let err = fs
        .replace_preserving_displaced(&replace_request(target.clone(), Some(b"x"), b"after"))
        .expect_err("mismatch");
    let _ = err;
    // remove then expect NotFound when expected base still Some
    fs.remove(&target).unwrap();
    let err = fs
        .replace_preserving_displaced(&replace_request(target.clone(), Some(b"x"), b"after"))
        .expect_err("missing");
    assert!(matches!(err, FsError::NotFound(_)));
}

#[test]
fn creation_is_no_clobber_and_reports_created() {
    let (fs, target) = setup("new.md", b"");
    fs.remove(&target).unwrap();
    let outcome = fs
        .replace_preserving_displaced(&replace_request(target.clone(), None, b"fresh"))
        .expect("create");
    assert!(matches!(outcome, RetainedReplaceOutcome::Created));
    assert_eq!(fs.read_if_exists(&target).unwrap().unwrap(), b"fresh");
    let err = fs
        .replace_preserving_displaced(&replace_request(target.clone(), None, b"again"))
        .expect_err("no-clobber");
    assert!(matches!(err, FsError::AlreadyExists(_)));
    assert_eq!(fs.read_if_exists(&target).unwrap().unwrap(), b"fresh");
}

#[test]
fn external_writer_between_check_and_exchange_is_retained_and_reported() {
    // The interleaver fires at the second `read_if_exists` (the displaced
    // verification read inside the composite): it replaces the staged
    // displaced object with external bytes. The primitive must still
    // retain those bytes and report `displaced_matches_base: false`.
    let memory = MemFs::new();
    memory.create_dir_all(Path::new("/vault")).unwrap();
    let target = PathBuf::from("/vault/a.md");
    memory.write_new(&target, b"prior").unwrap();
    memory.fsync_file(&target).unwrap();
    memory.fsync_dir(Path::new("/vault")).unwrap();

    let script = FaultScript::new().fault_at(
        "read_if_exists",
        3,
        FaultBoundary::Before,
        FaultKind::ExternalWrite,
    );
    let displaced_path = std::sync::Arc::new(std::sync::Mutex::new(None));
    let displaced_path2 = displaced_path.clone();
    let fs = FaultFs::new_mem(memory, script).with_interleaver(move |inner, _ctx| {
        // Occurs before the displaced-verification read: the staged file now
        // holds the displaced prior object. Simulate an external writer
        // modifying that retained object by overwriting it with racing bytes.
        let parent = Path::new("/vault");
        let staging = inner
            .read_dir(parent)
            .unwrap()
            .into_iter()
            .find(|e| e.name.to_string_lossy().starts_with(".flavite-op-"))
            .map(|e| e.path)
            .expect("staging dir exists");
        let staged = inner
            .read_dir(&staging)
            .unwrap()
            .into_iter()
            .find(|e| e.name.to_string_lossy().ends_with(".staged"))
            .map(|e| e.path)
            .expect("staged file exists");
        *displaced_path2.lock().unwrap() = Some(staged.clone());
        inner.remove(&staged).unwrap();
        inner.write_new(&staged, b"external-race").unwrap();
    });

    let outcome = fs
        .replace_preserving_displaced(&replace_request(target.clone(), Some(b"prior"), b"after"))
        .expect("replace succeeded despite race");
    let RetainedReplaceOutcome::Replaced {
        displaced,
        displaced_matches_base,
    } = outcome
    else {
        panic!("expected Replaced");
    };
    assert!(!displaced_matches_base, "external race must be visible");
    assert_eq!(
        fs.read_if_exists(&target).unwrap().unwrap(),
        b"after",
        "our output remains installed"
    );
    let displaced_bytes = fs.read_if_exists(&displaced.path).unwrap().unwrap();
    assert_eq!(displaced_bytes, b"external-race", "racing bytes retained");
    assert_eq!(displaced.len, 13);
}

#[test]
fn crash_before_parent_dir_sync_leaves_new_complete_bytes_and_displaced() {
    // The staged output was fsynced BEFORE the exchange, so after the
    // exchange the target's bytes are already durable. The parent-directory
    // sync persists the exchange itself; crashing before it still leaves the
    // target complete (new bytes) and the displaced object present — never
    // partial bytes, never a lost old object.
    for mode in [CrashMode::ProcessCrash, CrashMode::PowerLoss] {
        let memory = MemFs::new();
        memory.create_dir_all(Path::new("/vault")).unwrap();
        let target = PathBuf::from("/vault/a.md");
        memory.write_new(&target, b"prior").unwrap();
        memory.fsync_file(&target).unwrap();
        memory.fsync_dir(Path::new("/vault")).unwrap();
        memory.fsync_dir(Path::new("/")).unwrap();
        let kind = match mode {
            CrashMode::ProcessCrash => FaultKind::ProcessCrash,
            CrashMode::PowerLoss => FaultKind::PowerLoss,
        };
        // fsync_dir #1 through the wrapper is the parent-dir sync after the
        // exchange (setup used the unwrapped memory).
        let script = FaultScript::new().fault_at("fsync_dir", 1, FaultBoundary::Before, kind);
        let fs = FaultFs::new_mem(memory, script);
        let result = fs.replace_preserving_displaced(&replace_request(
            target.clone(),
            Some(b"prior"),
            b"after",
        ));
        assert!(!fs.is_alive(), "{mode:?} crash was not reached");
        let revived = fs.revive();
        let installed = revived.read_if_exists(&target).unwrap().unwrap();
        assert_eq!(installed, b"after", "{mode:?}: complete new bytes");
        let staging_entries = revived.read_dir(Path::new("/vault")).unwrap();
        let staging_count = staging_entries
            .iter()
            .filter(|e| e.name.to_string_lossy().starts_with(".flavite-op-"))
            .count();
        match mode {
            // Process crash: volatile state retained; the displaced object is
            // still reachable at its staged path.
            CrashMode::ProcessCrash => {
                assert_eq!(staging_count, 1, "process crash retains displaced object");
            }
            // Power loss: the staging namespace was never durably synced, so
            // the displaced object is not reachable by name — the durable
            // target (after bytes) is the only surviving complete source.
            CrashMode::PowerLoss => {
                assert_eq!(staging_count, 0, "undurable staging vanishes");
            }
        }
        assert!(result.is_err(), "{mode:?}: durability was not confirmed");
    }
}

#[test]
fn whole_operation_fault_at_boundary_fails_without_effect() {
    let (fs, target) = setup("a.md", b"prior");
    let fs = FaultFs::new_mem(
        fs,
        FaultScript::new().fault_at(
            "replace_preserving_displaced",
            1,
            FaultBoundary::Before,
            FaultKind::ProcessCrash,
        ),
    );
    let err = fs
        .replace_preserving_displaced(&replace_request(target.clone(), Some(b"prior"), b"after"))
        .unwrap_err();
    assert!(matches!(err, FsError::ProcessDead));
    let revived = fs.revive();
    assert_eq!(revived.read_if_exists(&target).unwrap().unwrap(), b"prior");
}

#[test]
fn supports_retained_replace_reports_honestly() {
    let fs = MemFs::new();
    assert!(fs.supports_retained_replace().unwrap());
    let fault_fs = FaultFs::new_mem(MemFs::new(), FaultScript::new());
    assert!(fault_fs.supports_retained_replace().unwrap());
    #[cfg(target_os = "linux")]
    {
        use flavite_fs::OsFs;
        assert!(OsFs::new().supports_retained_replace().unwrap());
    }
    #[cfg(not(target_os = "linux"))]
    {
        use flavite_fs::OsFs;
        assert!(!OsFs::new().supports_retained_replace().unwrap());
        let fs = OsFs::new();
        let err = fs
            .replace_preserving_displaced(&replace_request(
                PathBuf::from("/tmp/noexchange"),
                Some(b"x"),
                b"y",
            ))
            .unwrap_err();
        assert!(matches!(err, FsError::Unsupported(_)));
    }
}
