//! Shared api-test support (RFC-0003 W01 `tests/support/`): one gated
//! MemFs (per-path delay injection) and one plain shared MemFs wrapper,
//! used by the W05/W06 integration tests instead of per-file copies.

#![allow(dead_code)]

use flavite_fs::{DirEntry, Fs, MemFs, Metadata, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! impl_fs_delegates {
    ($outer:ident, $inner:ident) => {
        fn canonicalize(&self, path: &Path) -> Result<PathBuf> {
            self.$inner.canonicalize(path)
        }
        fn resolve_symlink(&self, path: &Path) -> Result<PathBuf> {
            self.$inner.resolve_symlink(path)
        }
        fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>> {
            self.$inner.encode_path_key(path)
        }
        fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf> {
            self.$inner.decode_path_key(key)
        }
        fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()> {
            self.$inner.validate_in_root(root, target)
        }
        fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>> {
            self.$inner.read_capped(path, max)
        }
        fn metadata(&self, path: &Path) -> Result<Metadata> {
            self.$inner.metadata(path)
        }
        fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>> {
            self.$inner.read_dir(path)
        }
        fn exists(&self, path: &Path) -> Result<bool> {
            self.$inner.exists(path)
        }
        fn is_placeholder(&self, path: &Path) -> Result<bool> {
            self.$inner.is_placeholder(path)
        }
        fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()> {
            self.$inner.write_new(path, bytes)
        }
        fn copy_mode(&self, from: &Path, to: &Path) -> Result<()> {
            self.$inner.copy_mode(from, to)
        }
        fn create_dir_all(&self, path: &Path) -> Result<()> {
            self.$inner.create_dir_all(path)
        }
        fn remove(&self, path: &Path) -> Result<()> {
            self.$inner.remove(path)
        }
        fn rename_replace(&self, from: &Path, to: &Path) -> Result<()> {
            self.$inner.rename_replace(from, to)
        }
        fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()> {
            self.$inner.rename_no_replace(from, to)
        }
        fn fsync_file(&self, path: &Path) -> Result<()> {
            self.$inner.fsync_file(path)
        }
        fn fsync_dir(&self, path: &Path) -> Result<()> {
            self.$inner.fsync_dir(path)
        }
    };
}

/// `MemFs` with a configurable delay on reads of one gated path, so cold
/// opens and query workers can be held in flight by tests.
pub struct GateFs {
    pub inner: std::sync::Arc<MemFs>,
    pub gated_path: PathBuf,
    pub read_delay_ms: AtomicU64,
}

impl GateFs {
    /// Build a gate around the given absolute path.
    pub fn new(gated_path: PathBuf) -> Self {
        Self::around(std::sync::Arc::new(MemFs::new()), gated_path)
    }

    /// Build a gate sharing an existing store (a `SharedFs` over the same
    /// `Arc<MemFs>` observes identical contents).
    pub fn around(inner: std::sync::Arc<MemFs>, gated_path: PathBuf) -> Self {
        Self {
            inner,
            gated_path,
            read_delay_ms: AtomicU64::new(0),
        }
    }

    /// Set the read delay for the gated path.
    pub fn set_delay(&self, delay_ms: u64) {
        self.read_delay_ms.store(delay_ms, Ordering::SeqCst);
    }
}

impl Fs for GateFs {
    fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        let delay = self.read_delay_ms.load(Ordering::SeqCst);
        if delay > 0 && path == self.gated_path {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
        self.inner.read_if_exists(path)
    }
    impl_fs_delegates!(GateFs, inner);
}

/// `Fs` view over a shared `MemFs`.
pub struct SharedFs {
    /// The shared store.
    pub inner: std::sync::Arc<MemFs>,
}

impl Fs for SharedFs {
    fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>> {
        self.inner.read_if_exists(path)
    }
    impl_fs_delegates!(SharedFs, inner);
}
