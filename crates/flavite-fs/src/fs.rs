//! The `Fs` trait — the filesystem seam the rest of the system codes against (§4.1).
//!
//! All methods return `Result<_, FsError>`; no panics on IO paths. Implementations:
//! [`MemFs`](crate::MemFs) (in-memory, for tests), `OsFs` (C5), `FaultFs` (fault injection).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Errors returned by [`Fs`] operations.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    #[error("file not found: {}", .0.display())]
    NotFound(PathBuf),

    #[error("file already exists: {}", .0.display())]
    AlreadyExists(PathBuf),

    #[error("path escapes vault root: {}", .0.display())]
    PathEscape(PathBuf),

    #[error("IO error: {0}")]
    Io(String),

    #[error("read exceeded cap of {max} bytes: {}", path.display())]
    ReadTooLarge { path: PathBuf, max: u64 },

    #[error("process is dead (crashed); a fresh Session/worker set is required")]
    ProcessDead,

    #[error("operation not supported: {0}")]
    Unsupported(String),

    #[error("invalid path: {0}")]
    InvalidPath(String),

    #[error("disk full")]
    DiskFull,

    #[error("short write: expected {expected} bytes, wrote {written}")]
    ShortWrite { expected: u64, written: u64 },

    #[error("fsync failed: {0}")]
    FsyncFailed(String),

    #[error("fsync not supported: {0}")]
    FsyncUnsupported(String),

    #[error("not a directory: {}", .0.display())]
    NotADirectory(PathBuf),

    #[error("directory not found: {}", .0.display())]
    DirNotFound(PathBuf),

    #[error("current bytes do not match the expected base")]
    BaseMismatch {
        expected: [u8; 32],
        actual: [u8; 32],
    },

    #[error("retained replacement staging protocol violated: {0}")]
    RetainedReplace(String),
}

/// Convenience `Result` alias for `Fs` operations.
pub type Result<T> = std::result::Result<T, FsError>;

/// File/directory metadata returned by [`Fs::metadata`].
#[derive(Debug, Clone)]
pub struct Metadata {
    /// File size in bytes.
    pub size: u64,
    /// Modification time in nanoseconds since the epoch.
    pub mtime_ns: i64,
    /// Native file identity *within its volume* (Unix: `st_ino`; Windows:
    /// file index). Combine [`Metadata::volume_id`] with this for a globally
    /// unique identity — inode numbers alone can collide across filesystems.
    pub native_id: u64,
    /// Volume/filesystem identity (Unix: `st_dev`; Windows: volume serial
    /// number). S3-2 evidence: the native root key (§4.13) and containment
    /// checks (§4.16.2) must key on the *(volume, file)* pair, never on the
    /// pathname — the same directory reached through a symlink alias reports
    /// an identical pair.
    pub volume_id: u64,
    /// Whether this path is a symlink.
    pub is_symlink: bool,
    /// Whether this path is a directory.
    pub is_dir: bool,
    /// Permission mode bits.
    pub mode: u32,
}

/// A directory entry returned by [`Fs::read_dir`].
#[derive(Debug, Clone)]
pub struct DirEntry {
    /// The file name (not the full path).
    pub name: OsString,
    /// The full path of the entry.
    pub path: PathBuf,
    /// Whether this entry is a directory.
    pub is_dir: bool,
    /// Whether this entry is a symlink.
    pub is_symlink: bool,
}

/// The filesystem trait (§4.1, §4.4).
///
/// The method surface is derived from the `save` and `reconcile` call sites in §4.4.
/// Object-safe; used as `&dyn Fs` throughout the system.
///
/// Durability model (§10.6.1): `fsync_file` persists file contents; `fsync_dir`
/// persists name creation/replacement/removal. The two are independent.
///
/// Per-OS semantics below cite the S3-2 spike evidence
/// (`devdocs/spikes/S3-fs.md`); rows not measured on the recording host are
/// marked source-verified/pending there.
pub trait Fs: Send + Sync {
    // --- Path operations ---

    /// Canonicalize a path (resolve `.` and `..` components, make absolute).
    fn canonicalize(&self, path: &Path) -> Result<PathBuf>;

    /// Resolve all symlinks in a path, returning the final target.
    ///
    /// The `save` path uses this before writing (D6): renaming a temp file
    /// over a *symlink path* replaces the link itself with a regular file
    /// (S3-2 scenario 2), so the atomic-write path MUST write the target.
    fn resolve_symlink(&self, path: &Path) -> Result<PathBuf>;

    /// Encode a vault-relative path to a reversible PathKey byte sequence (§4.16.2).
    ///
    /// Rejects absolute paths and non-normal components (`..`, root, prefix).
    fn encode_path_key(&self, path: &Path) -> Result<Vec<u8>>;

    /// Decode a PathKey byte sequence back to a vault-relative path.
    fn decode_path_key(&self, key: &[u8]) -> Result<PathBuf>;

    /// Validate that `target` is contained within `root` after canonicalization.
    fn validate_in_root(&self, root: &Path, target: &Path) -> Result<()>;

    // --- Read operations ---

    /// Read a file's full contents, or `None` if it does not exist.
    ///
    /// Symlinks are followed (a note that *is* a symlink reads its target's
    /// bytes; see the [`Fs::metadata`] symlink contract).
    fn read_if_exists(&self, path: &Path) -> Result<Option<Vec<u8>>>;

    /// Read a file's contents, erroring if the size exceeds `max` bytes.
    fn read_capped(&self, path: &Path, max: u64) -> Result<Vec<u8>>;

    /// Read through a directory capability, enforcing containment on the opened
    /// file and bounding allocation even when an external writer grows it.
    /// Custom filesystem adapters must implement this before serving embeds.
    fn read_capped_in_root(&self, _root: &Path, _relative: &Path, _max: u64) -> Result<Vec<u8>> {
        Err(FsError::Unsupported(
            "Contained bounded reads unavailable".into(),
        ))
    }

    /// Get metadata for a path.
    ///
    /// Symlink contract (finalized in S3-2, matching the `MemFs` model):
    /// `metadata` does **not** follow a symlink in the *final* component —
    /// it reports the link itself (`is_symlink: true`), like `lstat(2)`;
    /// intermediate symlink components are resolved as usual. Callers that
    /// need the target's identity call [`Fs::resolve_symlink`] first (the
    /// `save` path does, D6). The read methods follow symlinks. `OsFs` MUST
    /// implement this with `symlink_metadata`, not `metadata`.
    ///
    /// Native identity is the `(volume_id, native_id)` pair (S3-2 scenario 6):
    /// a symlink alias to the same file reports an identical pair, and a path
    /// reached *through* a symlinked parent resolves to the target's pair.
    /// Containment and root-key logic (§4.13, §4.16.2) validate the opened
    /// target's identity, never the path string alone.
    fn metadata(&self, path: &Path) -> Result<Metadata>;

    /// Enumerate the entries in a directory.
    fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>>;

    /// Check whether a path exists (file, directory, or symlink).
    fn exists(&self, path: &Path) -> Result<bool>;

    /// Check whether a path is a cloud placeholder (not downloaded locally).
    ///
    /// `MemFs` always returns `false`. `OsFs` detects per platform (§4.8,
    /// S3-2 scenario 4): Windows `FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS` /
    /// `_RECALL_ON_OPEN`; macOS `UF_DATALESS` in `st_flags`; Linux has no
    /// standard attribute and treats files as normal. A freshly created,
    /// fully local file MUST NOT report as a placeholder on any platform.
    fn is_placeholder(&self, path: &Path) -> Result<bool>;

    // --- Write operations ---

    /// Create a new file with the given contents. Fails if the file already exists.
    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<()>;

    /// Copy permission mode bits from one path to another.
    fn copy_mode(&self, from: &Path, to: &Path) -> Result<()>;

    /// Create a directory and all its parents.
    fn create_dir_all(&self, path: &Path) -> Result<()>;

    /// Remove a file. Fails if the path does not exist or is a directory.
    fn remove(&self, path: &Path) -> Result<()>;

    // --- Atomic operations ---

    /// Atomically rename `from` to `to`, replacing any existing file at `to`.
    ///
    /// Contract (S3-2 scenario 1): on Unix, `rename(2)` — the destination
    /// takes the *source's* inode, so its mode and mtime travel with the
    /// content; readers see old or new bytes, never partial. If `to` is a
    /// symlink, the link itself is replaced (S3-2 scenario 2), which is why
    /// callers MUST [`Fs::resolve_symlink`] first (D6). Windows (source
    /// verified, pending manual run): `ReplaceFileW` when the destination
    /// exists, falling back to `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`
    /// when it does not — `ReplaceFileW` alone fails over a missing target.
    fn rename_replace(&self, from: &Path, to: &Path) -> Result<()>;

    /// Atomically rename `from` to `to`, failing if `to` already exists.
    ///
    /// On failure the destination is untouched and [`FsError::AlreadyExists`]
    /// is returned. Contract (S3-2 scenario 7): plain `rename(2)` *clobbers*,
    /// so implementations need a dedicated primitive — Linux
    /// `renameat2(RENAME_NOREPLACE)` (measured), macOS
    /// `renamex_np(RENAME_EXCL)` and Windows `MoveFileExW` without
    /// `MOVEFILE_REPLACE_EXISTING` (source-verified, pending manual runs).
    fn rename_no_replace(&self, from: &Path, to: &Path) -> Result<()>;

    // --- Durability ---

    /// Persist file contents to stable storage.
    ///
    /// Durability strength is a property of the backing filesystem
    /// (§10.6.1): on memory-backed filesystems (tmpfs) success is a no-op
    /// guarantee (S3-2 scenario 3 measured ~0 µs there versus ~600 µs on
    /// ext4), and network filesystems are best-effort (§4.9). A caller MUST
    /// NOT infer a durability tier from success alone — the persistence model
    /// is recorded per filesystem by C2/C5, and unsupported/lying fsync is an
    /// explicitly weaker mode with visible status.
    fn fsync_file(&self, path: &Path) -> Result<()>;

    /// Persist directory entries (name creation/replacement/removal) to stable storage.
    ///
    /// Unix: `fsync` on an opened directory descriptor (measured on Linux for
    /// tmpfs and ext4). Windows: best-effort — `FlushFileBuffers` cannot flush
    /// a directory handle, so implementations return
    /// [`FsError::FsyncUnsupported`] and callers fall back to the weaker
    /// documented durability mode (§4.9).
    fn fsync_dir(&self, path: &Path) -> Result<()>;

    // --- Retained replacement (RFC-0006 D-10/§6.5) ---

    /// Atomically exchange the two existing files at `a` and `b`.
    ///
    /// After a successful exchange, `a` contains `b`'s previous bytes and
    /// `b` contains `a`'s previous bytes; both paths continue to exist.
    /// This is the kernel primitive behind [`Fs::replace_preserving_displaced`]:
    /// Linux implements it with `renameat2(RENAME_EXCHANGE)`, macOS with
    /// `renamex_np(RENAME_SWAP)`. Implementations that cannot guarantee an
    /// atomic swap MUST return [`FsError::Unsupported`] — never an emulation
    /// by unlinking one side (a crash then loses a file name).
    fn exchange(&self, _a: &Path, _b: &Path) -> Result<()> {
        Err(FsError::Unsupported(
            "atomic file exchange is not available on this filesystem".into(),
        ))
    }

    /// Whether [`Fs::replace_preserving_displaced`] is available on this
    /// filesystem (RFC-0006 D-10/§6.5). `false` means the stronger
    /// displacement guarantee cannot be kept; callers MUST refuse retained
    /// replacement with a typed `Unsupported` rather than silently falling
    /// back to an overwrite.
    fn supports_retained_replace(&self) -> Result<bool> {
        Ok(false)
    }

    /// Replace `request.target` with `request.bytes` while retaining the
    /// actual displaced object, per RFC-0006 D-10/§6.5.
    ///
    /// Steps: exclusive sibling staging directory `.flavite-op-<id>` next to
    /// the target; stage and fsync the desired output; read the current
    /// target bytes and compare against `expected_base_hash` (when `Some`;
    /// `None` asserts the target is absent — no-clobber creation); atomically
    /// exchange the staged file with the target so the previous bytes land on
    /// the staged path; fsync the parent directory (both names durable);
    /// then compare the displaced bytes against the expected base. A
    /// mismatch at that final compare means an external writer raced between
    /// the base check and the exchange — the displaced bytes are still
    /// retained and reported, never discarded and never auto-rolled-back.
    ///
    /// The default implementation is a generic composition over the rest of
    /// the trait surface; platform backends supply [`Fs::exchange`] and
    /// [`Fs::supports_retained_replace`]. Fault-injecting wrappers override
    /// it to orchestrate the same algorithm through their own injected
    /// methods, so every sub-operation boundary is exercisable.
    fn replace_preserving_displaced(
        &self,
        request: &RetainedReplaceRequest,
    ) -> Result<RetainedReplaceOutcome> {
        retained_replace(self, request)
    }
}

/// Generic retained-replacement algorithm (RFC-0006 D-10/§6.5), composed
/// over the [`Fs`] surface. Every sub-step is a trait call, so wrapper
/// implementations (fault injection, tracing) observe each boundary.
pub(crate) fn retained_replace<F: Fs + ?Sized>(
    fs: &F,
    request: &RetainedReplaceRequest,
) -> Result<RetainedReplaceOutcome> {
    if !fs.supports_retained_replace()? {
        return Err(FsError::Unsupported(
            "retained replacement is not available on this filesystem".into(),
        ));
    }
    let real = fs.resolve_symlink(&request.target)?;
    let Some(parent) = real.parent() else {
        return Err(FsError::RetainedReplace(
            "destination has no parent".to_owned(),
        ));
    };
    let name = real.file_name().and_then(|n| n.to_str()).unwrap_or("note");
    let staging = parent.join(format!(".flavite-op-{id}", id = uuid::Uuid::now_v7()));
    let staged = staging.join(format!(".{name}.staged"));
    let cleanup = || {
        let _ = fs.remove(&staged);
        let _ = fs.remove(&staging);
    };

    // 1. Exclusive sibling staging directory + staged output.
    fs.create_dir_all(&staging)?;
    if let Err(error) = fs.write_new(&staged, &request.bytes) {
        cleanup();
        return Err(error);
    }
    if let Ok(metadata) = fs.metadata(&real)
        && !metadata.is_dir
    {
        // Copying mode bits is best-effort; failure must not fail the
        // replacement (permissions are inherited where possible).
        let _ = fs.copy_mode(&real, &staged);
    }
    if let Err(error) = fs.fsync_file(&staged) {
        cleanup();
        return Err(error);
    }
    match fs.read_if_exists(&staged) {
        Ok(Some(staged_bytes)) if staged_bytes == request.bytes => {}
        Ok(_) => {
            cleanup();
            return Err(FsError::ShortWrite {
                expected: request.bytes.len() as u64,
                written: 0,
            });
        }
        Err(error) => {
            cleanup();
            return Err(error);
        }
    }

    // 2. Current target bytes + base check.
    let prior = match fs.read_if_exists(&real) {
        Ok(value) => value,
        Err(error) => {
            cleanup();
            return Err(error);
        }
    };
    let Some(prior_bytes) = prior else {
        // Creation: no-clobber atomic install, no displaced object.
        if request.expected_base_hash.is_some() {
            cleanup();
            return Err(FsError::NotFound(real));
        }
        if let Err(error) = fs.rename_no_replace(&staged, &real) {
            cleanup();
            return Err(error);
        }
        fs.fsync_dir(parent)?;
        let _ = fs.remove(&staging);
        return Ok(RetainedReplaceOutcome::Created);
    };
    let prior_hash = *blake3::hash(&prior_bytes).as_bytes();
    match request.expected_base_hash {
        None => {
            // No-clobber creation: a target that exists is a collision.
            cleanup();
            return Err(FsError::AlreadyExists(real));
        }
        Some(expected) if prior_hash != expected => {
            cleanup();
            return Err(FsError::BaseMismatch {
                expected,
                actual: prior_hash,
            });
        }
        Some(_) => {}
    }

    // 3. Atomic exchange: real gets the new bytes; the staged path now
    // holds the ACTUAL prior object (the displaced bytes).
    if let Err(error) = fs.exchange(&staged, &real) {
        cleanup();
        return Err(error);
    }
    // The exchange already happened and the displaced object remains on
    // the staged path. The caller reconciles durability — the displaced
    // bytes must NEVER be deleted here.
    fs.fsync_dir(parent)?;
    fs.fsync_dir(&staging)?;

    // 4. Verify the retained displaced object against the expected base.
    let displaced_bytes = match fs.read_if_exists(&staged) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => {
            return Err(FsError::RetainedReplace(
                "displaced object is missing after exchange".to_owned(),
            ));
        }
        Err(error) => return Err(error),
    };
    let displaced_hash = *blake3::hash(&displaced_bytes).as_bytes();
    let displaced_matches_base = request
        .expected_base_hash
        .is_none_or(|expected| displaced_hash == expected);
    Ok(RetainedReplaceOutcome::Replaced {
        displaced: DisplacedObject {
            path: staged,
            hash: displaced_hash,
            len: displaced_bytes.len() as u64,
        },
        displaced_matches_base,
    })
}

/// A retained displaced object (RFC-0006 D-10/§6.5): the exact previous
/// bytes of a replaced target, still present on disk at [`DisplacedObject::path`]
/// until the caller archives them durably and retires the staging directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplacedObject {
    /// Path of the retained previous-target object (inside the exclusive
    /// sibling staging directory).
    pub path: PathBuf,
    /// BLAKE3 of the displaced bytes.
    pub hash: [u8; 32],
    /// Byte length of the displaced object.
    pub len: u64,
}

/// Outcome of [`Fs::replace_preserving_displaced`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetainedReplaceOutcome {
    /// The target was replaced; `displaced` is the actual prior object and
    /// `displaced_matches_base` reports whether it equaled the requested
    /// base (false ⇒ an external writer raced and its bytes were retained).
    Replaced {
        displaced: DisplacedObject,
        displaced_matches_base: bool,
    },
    /// The target was created (no-clobber): there was no prior object.
    Created,
}

/// Request for [`Fs::replace_preserving_displaced`].
#[derive(Debug, Clone)]
pub struct RetainedReplaceRequest {
    /// The target path, already validated/anchored by the caller.
    pub target: PathBuf,
    /// Expected current bytes hash. `Some` replaces an existing file and is
    /// checked before the exchange; `None` asserts the target is absent
    /// (no-clobber creation).
    pub expected_base_hash: Option<[u8; 32]>,
    /// The exact desired bytes.
    pub bytes: Vec<u8>,
}

/// Compute the BLAKE3 hash of a file's contents (convenience helper).
pub fn hash_file<F: Fs + ?Sized>(fs: &F, path: &Path) -> Result<[u8; 32]> {
    match fs.read_if_exists(path)? {
        Some(bytes) => Ok(*blake3::hash(&bytes).as_bytes()),
        None => Err(FsError::NotFound(path.to_path_buf())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fs_error_display() {
        let err = FsError::NotFound(PathBuf::from("/missing"));
        assert!(err.to_string().contains("file not found"));
        assert!(err.to_string().contains("/missing"));

        let err = FsError::ReadTooLarge {
            path: PathBuf::from("/big"),
            max: 1024,
        };
        assert!(err.to_string().contains("1024"));

        let err = FsError::ProcessDead;
        assert!(err.to_string().contains("process is dead"));

        let err = FsError::ShortWrite {
            expected: 100,
            written: 50,
        };
        assert!(err.to_string().contains("100"));
        assert!(err.to_string().contains("50"));
    }

    #[test]
    fn metadata_clone() {
        let md = Metadata {
            size: 42,
            mtime_ns: 1000,
            native_id: 7,
            volume_id: 1,
            is_symlink: false,
            is_dir: false,
            mode: 0o644,
        };
        let md2 = md.clone();
        assert_eq!(md2.size, 42);
        assert_eq!(md2.native_id, 7);
        assert_eq!(md2.volume_id, 1);
    }

    #[test]
    fn dir_entry_clone() {
        let entry = DirEntry {
            name: OsString::from("file.md"),
            path: PathBuf::from("/vault/file.md"),
            is_dir: false,
            is_symlink: false,
        };
        let entry2 = entry.clone();
        assert_eq!(entry2.name, OsString::from("file.md"));
        assert!(!entry2.is_dir);
    }
}
