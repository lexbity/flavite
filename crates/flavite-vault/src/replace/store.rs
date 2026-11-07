//! Fs-backed private replacement-preview store (RFC-0006 S07/D-09).
//!
//! Sealed previews live under `<vault>/.flavite/operations/replace/<job-id>/`
//! with content-addressed payloads in `payloads/` (raw BLAKE3 names). The
//! whole `.flavite` tree is already excluded from note indexing, watcher
//! nominations and reconcile scans, so operation files structurally never
//! appear as notes or graph nodes. I/O is expressed through [`OpStoreIo`]
//! so the vault actor provides bounded contained access and tests can use a
//! plain `MemFs`; UI code never touches `std::fs` (gated).

use std::path::{Path, PathBuf};

use super::plan::{OPERATIONS_DIR, PREVIEW_SCHEMA_VERSION, PreviewManifest, REPLACE_DIR};

/// Bounded private-operation-store I/O. Implemented by the vault actor
/// (contained within the vault root) and by test Fs wrappers.
pub trait OpStoreIo: Send + Sync {
    /// Create a directory and its parents.
    fn mkdir_all(&self, path: &Path) -> Result<(), String>;
    /// Create a new file exclusively; fails when the path already exists.
    fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<(), String>;
    /// Atomically replace a file's contents (staging + rename_replace);
    /// used for the private high-water head.
    fn write_replace(&self, path: &Path, bytes: &[u8]) -> Result<(), String>;
    /// Read a file; `None` when absent.
    fn read(&self, path: &Path) -> Result<Option<Vec<u8>>, String>;
    /// Read a file bounded by `max` bytes (§6.4 decoder bounds: operation
    /// store files are untrusted input validated before parsing). `None`
    /// when absent; an error when the file exceeds the bound.
    fn read_capped(&self, path: &Path, max: u64) -> Result<Option<Vec<u8>>, String> {
        match self.read(path)? {
            None => Ok(None),
            Some(bytes) if bytes.len() as u64 > max => Err(format!(
                "operation-store file exceeds its {} byte bound",
                max
            )),
            Some(bytes) => Ok(Some(bytes)),
        }
    }
    /// List plain file names in a directory (sorted; `None` when absent).
    fn list_dir(&self, path: &Path) -> Result<Option<Vec<String>>, String>;
    /// Fsync one file.
    fn fsync_file(&self, path: &Path) -> Result<(), String>;
    /// Fsync one directory.
    fn fsync_dir(&self, path: &Path) -> Result<(), String>;
    /// Remove one file (idempotent for absent files).
    fn remove(&self, path: &Path) -> Result<(), String>;
}

/// Store error (typed, redacted).
#[derive(Debug, Clone)]
pub enum StoreError {
    /// An I/O operation failed with the underlying message.
    Io(String),
    /// The store layout/header is corrupt or unknown (never guessed).
    Corrupt(String),
    /// The referenced object is missing or has the wrong length.
    MissingObject { hash: String, expected: u64 },
    /// A payload object's bytes do not match its content-addressed name.
    ChecksumMismatch { hash: String },
    /// The manifest schema/engine is not supported by this build.
    UnsupportedVersion { schema: u32, engine: u32 },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "operation store I/O failed: {message}"),
            Self::Corrupt(message) => write!(f, "operation store is corrupt: {message}"),
            Self::MissingObject { hash, expected } => {
                write!(
                    f,
                    "operation store object {hash} missing (expected {expected} bytes)"
                )
            }
            Self::ChecksumMismatch { hash } => {
                write!(f, "operation store object {hash} failed its checksum")
            }
            Self::UnsupportedVersion { schema, engine } => write!(
                f,
                "operation store schema {schema}/engine {engine} is not supported"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

/// Hex-encode a hash for object naming.
#[must_use]
pub fn hex_hash(bytes: &[u8; 32]) -> String {
    hex::encode(bytes)
}

/// The replace-preview store: fixed private namespace, content-addressed
/// payloads, exclusive writes, manifest-last sealing.
#[derive(Debug, Clone)]
pub struct ReplaceStore {
    /// `<vault>/.flavite/operations/replace`.
    base: PathBuf,
}

impl ReplaceStore {
    /// The store rooted at a vault root.
    #[must_use]
    pub fn new(vault_root: &Path) -> Self {
        Self {
            base: vault_root.join(OPERATIONS_DIR).join(REPLACE_DIR),
        }
    }

    /// The private base directory.
    #[must_use]
    pub fn base(&self) -> &Path {
        &self.base
    }

    /// The `payloads/` directory for one job.
    #[must_use]
    pub fn payload_dir(&self, job_id: &str) -> PathBuf {
        self.base.join(job_id).join("payloads")
    }

    /// The manifest path for one job.
    #[must_use]
    pub fn manifest_path(&self, job_id: &str) -> PathBuf {
        self.base.join(job_id).join("manifest.json")
    }

    /// The object path for one content-addressed payload.
    #[must_use]
    pub fn payload_path(&self, job_id: &str, hash: &str) -> PathBuf {
        self.payload_dir(job_id).join(format!("{hash}.bin"))
    }

    /// Write one content-addressed payload (exclusive; an existing object
    /// with the same name is already the same bytes, so it is accepted).
    pub fn write_payload(
        &self,
        io: &dyn OpStoreIo,
        job_id: &str,
        bytes: &[u8],
    ) -> Result<super::plan::SealedPayloadRef, StoreError> {
        let hash = blake3::hash(bytes);
        let name = hex_hash(hash.as_bytes());
        let path = self.payload_path(job_id, &name);
        io.mkdir_all(&self.payload_dir(job_id))
            .map_err(StoreError::Io)?;
        if io.read(&path).map_err(StoreError::Io)?.is_none() {
            io.write_new(&path, bytes).map_err(StoreError::Io)?;
            io.fsync_file(&path).map_err(StoreError::Io)?;
        }
        Ok(super::plan::SealedPayloadRef {
            hash: name,
            len: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        })
    }

    /// Read one content-addressed payload, verifying byte length and
    /// checksum. `None` when the object is absent.
    pub fn read_payload(
        &self,
        io: &dyn OpStoreIo,
        job_id: &str,
        reference: &super::plan::SealedPayloadRef,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.payload_path(job_id, &reference.hash);
        let Some(bytes) = io
            .read_capped(&path, reference.len)
            .map_err(StoreError::Io)?
        else {
            return Ok(None);
        };
        if bytes.len() as u64 != reference.len {
            return Err(StoreError::MissingObject {
                hash: reference.hash.clone(),
                expected: reference.len,
            });
        }
        if hex_hash(blake3::hash(&bytes).as_bytes()) != reference.hash {
            return Err(StoreError::ChecksumMismatch {
                hash: reference.hash.clone(),
            });
        }
        Ok(Some(bytes))
    }

    /// Seal a preview: payloads are written first, then the manifest as the
    /// last atomically visible record (manifest-last sealing). A torn write
    /// never produces a readable ready manifest.
    pub fn seal_preview(
        &self,
        io: &dyn OpStoreIo,
        manifest: &PreviewManifest,
    ) -> Result<(), StoreError> {
        if manifest.schema != PREVIEW_SCHEMA_VERSION {
            return Err(StoreError::UnsupportedVersion {
                schema: manifest.schema,
                engine: manifest.engine_version,
            });
        }
        let job_dir = self.base.join(&manifest.job_id);
        io.mkdir_all(&job_dir).map_err(StoreError::Io)?;
        io.mkdir_all(&self.payload_dir(&manifest.job_id))
            .map_err(StoreError::Io)?;
        let json =
            serde_json::to_vec(manifest).map_err(|error| StoreError::Corrupt(error.to_string()))?;
        let path = self.manifest_path(&manifest.job_id);
        // Exclusive create: a collision is a leaked job id, not silently
        // acceptable (the id is core-issued and unguessable).
        io.write_new(&path, &json).map_err(StoreError::Io)?;
        io.fsync_file(&path).map_err(StoreError::Io)?;
        io.fsync_dir(&job_dir).map_err(StoreError::Io)?;
        io.fsync_dir(&self.base).map_err(StoreError::Io)?;
        Ok(())
    }

    /// Hard decoder bound for one manifest header (§6.4).
    pub const MANIFEST_MAX_BYTES: u64 = 1024 * 1024;

    /// Load a sealed manifest; `None` when the job is not present.
    pub fn read_manifest(
        &self,
        io: &dyn OpStoreIo,
        job_id: &str,
    ) -> Result<Option<PreviewManifest>, StoreError> {
        let path = self.manifest_path(job_id);
        let Some(bytes) = io
            .read_capped(&path, Self::MANIFEST_MAX_BYTES)
            .map_err(StoreError::Io)?
        else {
            return Ok(None);
        };
        let manifest: PreviewManifest = serde_json::from_slice(&bytes)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        if manifest.schema != PREVIEW_SCHEMA_VERSION
            || manifest.engine_version > super::plan::ENGINE_VERSION
        {
            return Err(StoreError::UnsupportedVersion {
                schema: manifest.schema,
                engine: manifest.engine_version,
            });
        }
        if manifest.job_id != job_id {
            return Err(StoreError::Corrupt(
                "manifest job id does not match its directory".to_owned(),
            ));
        }
        Ok(Some(manifest))
    }

    /// Stamp the frozen selection revision/digest onto the sealed manifest
    /// at first Apply (D-09/§6.4): after restart, a resumed job can prove
    /// the on-disk selection matches the revision the caller authorized.
    ///
    /// # Errors
    ///
    /// Typed [`StoreError`] for absent/unknown manifests and IO failure.
    pub fn stamp_selection(
        &self,
        io: &dyn OpStoreIo,
        job_id: &str,
        selection_revision: u64,
        selection_digest: &str,
    ) -> Result<(), StoreError> {
        let path = self.manifest_path(job_id);
        let Some(bytes) = io
            .read_capped(&path, Self::MANIFEST_MAX_BYTES)
            .map_err(StoreError::Io)?
        else {
            return Err(StoreError::MissingObject {
                hash: job_id.to_owned(),
                expected: 0,
            });
        };
        let mut manifest: PreviewManifest = serde_json::from_slice(&bytes)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        if manifest.schema != PREVIEW_SCHEMA_VERSION
            || manifest.engine_version > super::plan::ENGINE_VERSION
        {
            return Err(StoreError::UnsupportedVersion {
                schema: manifest.schema,
                engine: manifest.engine_version,
            });
        }
        manifest.selection_revision = Some(selection_revision);
        manifest.selection_digest = Some(selection_digest.to_owned());
        let json = serde_json::to_vec(&manifest)
            .map_err(|error| StoreError::Corrupt(error.to_string()))?;
        io.write_replace(&path, &json).map_err(StoreError::Io)?;
        io.fsync_file(&path).map_err(StoreError::Io)?;
        Ok(())
    }

    /// Remove a preview job tree (expiry/forget). Idempotent.
    pub fn remove_job(&self, io: &dyn OpStoreIo, job_id: &str) -> Result<(), StoreError> {
        // S07 removes only the known files of a sealed preview; directory
        // removal is best-effort and never touches unknown content.
        let dir = self.base.join(job_id);
        let manifest = self.manifest_path(job_id);
        let _ = io.remove(&manifest);
        if let Ok(Some(manifest)) = self.read_manifest(io, job_id) {
            for note in &manifest.notes {
                let _ = io.remove(&self.payload_path(job_id, &note.before.hash));
                let _ = io.remove(&self.payload_path(job_id, &note.after.hash));
            }
        }
        let _ = io.remove(&dir.join("payloads"));
        let _ = io.remove(&dir);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replace::plan;
    use crate::replace::plan::{
        NoteCandidateRecord, PreviewTotals, ScopeVersion, SealedPayloadRef, SourceKind,
    };
    use flavite_fs::{Fs, MemFs};

    /// `OpStoreIo` directly over a MemFs (test harness; production uses the
    /// actor's contained implementation).
    struct MemIo(MemFs);

    impl OpStoreIo for MemIo {
        fn mkdir_all(&self, path: &Path) -> Result<(), String> {
            self.0.create_dir_all(path).map_err(|e| e.to_string())
        }
        fn write_new(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
            self.0.write_new(path, bytes).map_err(|e| e.to_string())
        }
        fn write_replace(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
            let parent = path.parent().ok_or_else(|| "no parent".to_owned())?;
            self.0.create_dir_all(parent).map_err(|e| e.to_string())?;
            let temp = parent.join(format!(".ophead-{}.tmp", uuid::Uuid::now_v7()));
            self.0.write_new(&temp, bytes).map_err(|e| e.to_string())?;
            self.0.fsync_file(&temp).map_err(|e| e.to_string())?;
            self.0
                .rename_replace(&temp, path)
                .map_err(|e| e.to_string())?;
            self.0.fsync_dir(parent).map_err(|e| e.to_string())
        }
        fn read(&self, path: &Path) -> Result<Option<Vec<u8>>, String> {
            self.0.read_if_exists(path).map_err(|e| e.to_string())
        }
        fn list_dir(&self, path: &Path) -> Result<Option<Vec<String>>, String> {
            match self.0.read_dir(path) {
                Ok(entries) => {
                    let mut names = entries
                        .into_iter()
                        .filter(|entry| !entry.is_dir)
                        .map(|entry| entry.name.into_string().map_err(|_| "not UTF-8".to_owned()))
                        .collect::<Result<Vec<_>, String>>()?;
                    names.sort();
                    Ok(Some(names))
                }
                Err(flavite_fs::FsError::DirNotFound(_)) => Ok(None),
                Err(error) => Err(error.to_string()),
            }
        }
        fn fsync_file(&self, path: &Path) -> Result<(), String> {
            self.0.fsync_file(path).map_err(|e| e.to_string())
        }
        fn fsync_dir(&self, path: &Path) -> Result<(), String> {
            self.0.fsync_dir(path).map_err(|e| e.to_string())
        }
        fn remove(&self, path: &Path) -> Result<(), String> {
            self.0.remove(path).map_err(|e| e.to_string())
        }
    }

    fn sample_manifest(job: &str) -> PreviewManifest {
        PreviewManifest {
            schema: PREVIEW_SCHEMA_VERSION,
            engine_version: plan::ENGINE_VERSION,
            job_id: job.to_owned(),
            vault_uuid: "uu".to_owned(),
            plan_digest: "p".to_owned(),
            scope_version: ScopeVersion {
                epoch: 1,
                generation: 1,
            },
            source_policy: "disk_with_explicit_drafts".to_owned(),
            state: "ready".to_owned(),
            totals: PreviewTotals::default(),
            payload_bytes: 0,
            created_at_unix_ms: 1,
            expires_at_unix_ms: 2,
            selection_revision: None,
            selection_digest: None,
            include_drafts: false,
            notes: vec![NoteCandidateRecord {
                item_id: 0,
                path: "a.md".to_owned(),
                path_native: "61".to_owned(),
                source_kind: SourceKind::Saved,
                disk_hash: "d".to_owned(),
                content_hash: "c".to_owned(),
                format: plan::PersistedSourceFormat::from_format(
                    flavite_encoding::SourceFormat::utf8(),
                ),
                codec_version: 1,
                hint_generation: None,
                before: SealedPayloadRef {
                    hash: "b".to_owned(),
                    len: 1,
                },
                after: SealedPayloadRef {
                    hash: "a".to_owned(),
                    len: 1,
                },
                match_count: 1,
                output_bytes: 1,
                no_change: false,
                open_lineage: None,
                draft_lineage: None,
            }],
            exclusions: vec![],
        }
    }

    #[test]
    fn payloads_are_content_addressed_and_deduped() {
        let fs = MemFs::new();
        let io = MemIo(fs.clone());
        let store = ReplaceStore::new(Path::new("/vault"));
        let a = store.write_payload(&io, "job1", b"hello").unwrap();
        let b = store.write_payload(&io, "job1", b"hello").unwrap();
        assert_eq!(a, b, "identical bytes produce the same object");
        let different = store.write_payload(&io, "job1", b"world").unwrap();
        assert_ne!(a.hash, different.hash);
        let read = store.read_payload(&io, "job1", &a).unwrap();
        assert_eq!(read.as_deref(), Some(&b"hello"[..]));
    }

    #[test]
    fn seal_then_read_roundtrips_and_removal_is_idempotent() {
        let fs = MemFs::new();
        let io = MemIo(fs.clone());
        let store = ReplaceStore::new(Path::new("/vault"));
        let manifest = sample_manifest("f".repeat(32).as_str());
        store.seal_preview(&io, &manifest).unwrap();
        let loaded = store.read_manifest(&io, "f".repeat(32).as_str()).unwrap();
        let loaded = loaded.expect("manifest present");
        assert_eq!(loaded.totals, manifest.totals);
        assert_eq!(loaded.notes.len(), 1);
        assert_eq!(loaded.record_digest(), manifest.record_digest());
        store.remove_job(&io, "f".repeat(32).as_str()).unwrap();
        assert!(
            store
                .read_manifest(&io, "f".repeat(32).as_str())
                .unwrap()
                .is_none()
        );
        store.remove_job(&io, "f".repeat(32).as_str()).unwrap();
    }

    #[test]
    fn manifest_rejects_wrong_job_id_and_unknown_schema() {
        let fs = MemFs::new();
        let io = MemIo(fs.clone());
        let store = ReplaceStore::new(Path::new("/vault"));
        let mut manifest = sample_manifest("0".repeat(32).as_str());
        manifest.schema = 99;
        let err = store.seal_preview(&io, &manifest).unwrap_err();
        assert!(matches!(err, StoreError::UnsupportedVersion { .. }));
        manifest.schema = PREVIEW_SCHEMA_VERSION;
        store.seal_preview(&io, &manifest).unwrap();
        let absent = store.read_manifest(&io, &"1".repeat(32)).unwrap();
        assert!(
            absent.is_none(),
            "absent job is None, never a corrupt guess"
        );
    }
}
