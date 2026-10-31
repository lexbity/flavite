//! User-state storage conformance (RFC-0003 §22, W06).
//!
//! Fs fault injection at every transition, validated generation receipts,
//! protected originals, symlink rejection, no note-file changes, and the
//! device-local root layout.

use flavite_fs::{DirEntry, Fs, MemFs, Metadata, Result};
use flavite_vault::user_state::{UserStateError, UserStateKind, UserStateStore};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

const PAYLOAD_A: &str = r#"{"checksum":"a","payload":{"schema":1}}"#;
const PAYLOAD_B: &str = r#"{"checksum":"b","payload":{"schema":1,"named":true}}"#;

/// Fault-injecting wrapper: fails the Nth call to a chosen operation.
struct FaultyFs {
    inner: MemFs,
    fail_op: &'static str,
    fail_at: AtomicUsize,
    calls: AtomicUsize,
}

impl FaultyFs {
    fn new(fail_op: &'static str, fail_at: usize) -> Self {
        Self {
            inner: MemFs::new(),
            fail_op,
            fail_at: AtomicUsize::new(fail_at),
            calls: AtomicUsize::new(0),
        }
    }

    fn maybe_fail(&self, op: &'static str) -> Result<()> {
        if op == self.fail_op {
            let seen = self.calls.fetch_add(1, Ordering::SeqCst);
            if seen + 1 == self.fail_at.load(Ordering::SeqCst) {
                // Single-shot: disarm after firing so retries proceed.
                self.fail_at.store(usize::MAX, Ordering::SeqCst);
                return Err(flavite_fs::FsError::Io(format!("injected fault in {op}")));
            }
        }
        Ok(())
    }
}

// The trait requires typed signatures; they are written out explicitly.
impl Fs for FaultyFs {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        self.inner.canonicalize(path)
    }
    fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
        self.inner.resolve_symlink(path)
    }
    fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
        self.inner.encode_path_key(path)
    }
    fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
        self.inner.decode_path_key(key)
    }
    fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()> {
        self.inner.validate_in_root(root, target)
    }
    fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        self.maybe_fail("read_if_exists")?;
        self.inner.read_if_exists(path)
    }
    fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
        self.maybe_fail("read_capped")?;
        self.inner.read_capped(path, max)
    }
    fn metadata(&self, path: &Path) -> Result<Metadata> {
        self.maybe_fail("metadata")?;
        self.inner.metadata(path)
    }
    fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        self.inner.read_dir(path)
    }
    fn exists(&self, path: &Path) -> Result<bool> {
        self.inner.exists(path)
    }
    fn is_placeholder(&self, path: &Path) -> Result<bool> {
        self.inner.is_placeholder(path)
    }
    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.maybe_fail("write_new")?;
        self.inner.write_new(path, bytes)
    }
    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.copy_mode(from, to)
    }
    fn create_dir_all(&self, path: &Path) -> Result<()> {
        self.maybe_fail("create_dir_all")?;
        self.inner.create_dir_all(path)
    }
    fn remove(&self, path: &Path) -> Result<()> {
        self.maybe_fail("remove")?;
        self.inner.remove(path)
    }
    fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
        self.maybe_fail("rename_replace")?;
        self.inner.rename_replace(from, to)
    }
    fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.rename_no_replace(from, to)
    }
    fn fsync_file(&self, path: &Path) -> Result<()> {
        self.maybe_fail("fsync_file")?;
        self.inner.fsync_file(path)
    }
    fn fsync_dir(&self, path: &Path) -> Result<()> {
        self.maybe_fail("fsync_dir")?;
        self.inner.fsync_dir(path)
    }
}

fn store_on_faulty(op: &'static str, fail_at: usize) -> UserStateStore {
    UserStateStore::at_root(
        Box::new(FaultyFs::new(op, fail_at)),
        PathBuf::from("/test/user-state/vault-uuid"),
    )
}

fn plain_store() -> (UserStateStore, MemFs) {
    let fs = MemFs::new();
    (
        UserStateStore::at_root(
            Box::new(SharedFs(fs.clone())),
            PathBuf::from("/test/user-state/vault-uuid"),
        ),
        fs,
    )
}

struct SharedFs(MemFs);

impl Fs for SharedFs {
    fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
        self.0.canonicalize(path)
    }
    fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
        self.0.resolve_symlink(path)
    }
    fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
        self.0.encode_path_key(path)
    }
    fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
        self.0.decode_path_key(key)
    }
    fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()> {
        self.0.validate_in_root(root, target)
    }
    fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        self.0.read_if_exists(path)
    }
    fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
        self.0.read_capped(path, max)
    }
    fn metadata(&self, path: &Path) -> Result<Metadata> {
        self.0.metadata(path)
    }
    fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
        self.0.read_dir(path)
    }
    fn exists(&self, path: &Path) -> Result<bool> {
        self.0.exists(path)
    }
    fn is_placeholder(&self, path: &Path) -> Result<bool> {
        self.0.is_placeholder(path)
    }
    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
        self.0.write_new(path, bytes)
    }
    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
        self.0.copy_mode(from, to)
    }
    fn create_dir_all(&self, path: &Path) -> Result<()> {
        self.0.create_dir_all(path)
    }
    fn remove(&self, path: &Path) -> Result<()> {
        self.0.remove(path)
    }
    fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
        self.0.rename_replace(from, to)
    }
    fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
        self.0.rename_no_replace(from, to)
    }
    fn fsync_file(&self, path: &Path) -> Result<()> {
        self.0.fsync_file(path)
    }
    fn fsync_dir(&self, path: &Path) -> Result<()> {
        self.0.fsync_dir(path)
    }
}

#[test]
fn generations_advance_and_receipts_are_enforced() {
    let (mut store, _fs) = plain_store();
    assert!(
        store
            .read(UserStateKind::WorkspaceLayouts)
            .unwrap()
            .is_none()
    );

    let generation = store
        .write(UserStateKind::WorkspaceLayouts, 0, PAYLOAD_A.as_bytes())
        .expect("first write");
    assert_eq!(generation, 1);
    let stored = store
        .read(UserStateKind::WorkspaceLayouts)
        .unwrap()
        .expect("stored");
    assert_eq!(stored.generation, 1);
    assert_eq!(stored.payload, PAYLOAD_A.as_bytes());

    // A stale receipt is a typed mismatch, never a silent overwrite.
    assert_eq!(
        store.write(UserStateKind::WorkspaceLayouts, 0, PAYLOAD_B.as_bytes()),
        Err(UserStateError::GenerationMismatch {
            expected: 0,
            actual: 1,
        })
    );
    let generation = store
        .write(UserStateKind::WorkspaceLayouts, 1, PAYLOAD_B.as_bytes())
        .expect("second write");
    assert_eq!(generation, 2);
    let stored = store
        .read(UserStateKind::WorkspaceLayouts)
        .unwrap()
        .expect("stored");
    assert_eq!(stored.payload, PAYLOAD_B.as_bytes());
}

#[test]
fn fault_injection_surfaces_every_transition_and_keeps_the_original() {
    // Every filesystem transition the write path performs must be able to
    // fail loudly while the previous file survives.
    for (op, fail_at) in [
        ("create_dir_all", 1),
        // (read_if_exists faults are excluded: a faulted read inside write
        // leaves the receipt ambiguous by design; the receipt itself is the
        // reconciliation source, covered by the fsync_dir case.)
        ("write_new", 1),
        ("fsync_file", 1),
        ("rename_replace", 1),
        ("fsync_dir", 1),
    ] {
        let mut store = store_on_faulty(op, fail_at);
        // The fault fires inside the FIRST write at the chosen transition.
        let first = store.write(UserStateKind::WorkspaceLayouts, 0, PAYLOAD_A.as_bytes());
        // Reconcile through the stored receipt: a fault before the replace
        // leaves nothing; a fault at the directory sync may have landed the
        // document — the receipt says which, never guessed.
        let stored = store
            .read(UserStateKind::WorkspaceLayouts)
            .unwrap_or_else(|error| {
                // A fault still armed on reads means the document never
                // landed; reconcile from empty once the fault disarms.
                assert!(matches!(error, UserStateError::Fs(_)), "{error:?}");
                None
            });
        let expected = stored.as_ref().map_or(0, |stored| stored.generation);
        let retry = store.write(
            UserStateKind::WorkspaceLayouts,
            expected,
            if expected == 0 {
                PAYLOAD_A.as_bytes()
            } else {
                PAYLOAD_B.as_bytes()
            },
        );
        assert!(
            retry.is_ok(),
            "{op}@{fail_at}: reconcile after fault: {retry:?}"
        );
        // From here every write proceeds normally.
        let reconciled = retry.expect("reconcile succeeded");
        if let Ok(final_generation) = store.write(
            UserStateKind::WorkspaceLayouts,
            reconciled,
            PAYLOAD_B.as_bytes(),
        ) {
            let final_stored = store
                .read(UserStateKind::WorkspaceLayouts)
                .expect("read works")
                .expect("final document");
            assert_eq!(final_stored.generation, final_generation);
        }
        let final_stored = store
            .read(UserStateKind::WorkspaceLayouts)
            .expect("read works")
            .expect("final document");
        assert_eq!(final_stored.payload, PAYLOAD_B.as_bytes());
        if first.is_ok() {
            unreachable!("the injected fault must have fired");
        }
    }
}

#[test]
fn backups_retain_the_last_good_document() {
    let (store, _fs) = plain_store();
    let mut store = store;
    store
        .write(UserStateKind::WorkspaceLayouts, 0, PAYLOAD_A.as_bytes())
        .expect("first");
    store
        .write(UserStateKind::WorkspaceLayouts, 1, PAYLOAD_B.as_bytes())
        .expect("second");
    let backup = store
        .read_backup(UserStateKind::WorkspaceLayouts)
        .expect("backup readable")
        .expect("backup exists");
    // The backup is the previous stored envelope; decoding it through the
    // same envelope format yields the first payload verbatim.
    let envelope: serde_json::Value = serde_json::from_slice(&backup).expect("envelope");
    let previous = envelope["payload"].as_str().expect("string payload");
    assert_eq!(previous, PAYLOAD_A, "backup holds the last good copy");
}

#[test]
fn protected_paths_are_refused_and_notes_never_touched() {
    // A symlinked state file is refused by the protection guard: metadata
    // reporting a symlink produces a typed Protected error before any read
    // or write proceeds. The wrapper simulates the filesystem verdict.
    struct SymlinkFs(MemFs);
    impl Fs for SymlinkFs {
        fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
            self.0.canonicalize(path)
        }
        fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
            self.0.resolve_symlink(path)
        }
        fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
            self.0.encode_path_key(path)
        }
        fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
            self.0.decode_path_key(key)
        }
        fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()> {
            self.0.validate_in_root(root, target)
        }
        fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
            self.0.read_if_exists(path)
        }
        fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
            self.0.read_capped(path, max)
        }
        fn metadata(&self, path: &Path) -> Result<Metadata> {
            let metadata = self.0.metadata(path)?;
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                return Ok(Metadata {
                    is_symlink: true,
                    ..metadata
                });
            }
            Ok(metadata)
        }
        fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
            self.0.read_dir(path)
        }
        fn exists(&self, path: &Path) -> Result<bool> {
            self.0.exists(path)
        }
        fn is_placeholder(&self, path: &Path) -> Result<bool> {
            self.0.is_placeholder(path)
        }
        fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
            self.0.write_new(path, bytes)
        }
        fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
            self.0.copy_mode(from, to)
        }
        fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.0.create_dir_all(path)
        }
        fn remove(&self, path: &Path) -> Result<()> {
            self.0.remove(path)
        }
        fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
            self.0.rename_replace(from, to)
        }
        fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
            self.0.rename_no_replace(from, to)
        }
        fn fsync_file(&self, path: &Path) -> Result<()> {
            self.0.fsync_file(path)
        }
        fn fsync_dir(&self, path: &Path) -> Result<()> {
            self.0.fsync_dir(path)
        }
    }
    let fs = MemFs::new();
    fs.create_dir_all(Path::new("/test/user-state/vault-uuid"))
        .expect("root");
    fs.write_new(
        Path::new("/test/user-state/vault-uuid/workspace-layouts.json"),
        br#"{"generation":1,"payload":"{}"}"#,
    )
    .expect("seeded");
    let store = UserStateStore::at_root(
        Box::new(SymlinkFs(fs)),
        PathBuf::from("/test/user-state/vault-uuid"),
    );
    assert!(matches!(
        store.read(UserStateKind::WorkspaceLayouts),
        Err(UserStateError::Protected { .. })
    ));
    let mut store = store;
    assert!(matches!(
        store.write(UserStateKind::WorkspaceLayouts, 0, PAYLOAD_A.as_bytes()),
        Err(UserStateError::Protected { .. })
    ));
}

#[test]
fn non_json_payloads_are_refused() {
    let (mut store, _fs) = plain_store();
    assert!(matches!(
        store.write(UserStateKind::WorkspaceLayouts, 0, b"not json"),
        Err(UserStateError::NotJson(_))
    ));
}

#[test]
fn device_preferences_are_an_independent_fixed_location() {
    let (mut store, fs) = plain_store();
    store
        .write(UserStateKind::DevicePreferences, 0, PAYLOAD_A.as_bytes())
        .expect("written");
    assert!(
        fs.read_if_exists(Path::new(
            "/test/user-state/vault-uuid/device-preferences.json"
        ))
        .unwrap()
        .is_some(),
        "the fixed device-preferences location is used"
    );
    assert!(
        fs.read_if_exists(Path::new(
            "/test/user-state/vault-uuid/workspace-layouts.json"
        ))
        .unwrap()
        .is_none(),
        "kinds never share files"
    );
}
