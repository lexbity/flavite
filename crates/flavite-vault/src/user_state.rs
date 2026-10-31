//! Device-local user state storage (RFC-0003 §22, W06, D-11/D-12).
//!
//! Workspace layouts and device preferences are device-local user state in
//! the OS data directory, keyed by vault UUID: never note truth, never
//! synced through the vault. The store owns fixed, validated locations —
//! no arbitrary caller filenames — with atomic generations, a last-good
//! backup, and explicit fault surfaces for tests.

use crate::identity::get_data_dir;
use flavite_fs::Fs;
use std::path::{Path, PathBuf};

/// Hard byte bound for one user-state file (codec bound plus envelope).
pub const MAX_USER_STATE_BYTES: u64 = 5 * 1024 * 1024;

/// The fixed set of device-local state kinds. Adding a kind is a schema
/// decision; callers cannot invent locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserStateKind {
    /// The workspace layout file (W06 codec output).
    WorkspaceLayouts,
    /// Device UI preferences, split from vault safety settings.
    DevicePreferences,
}

impl UserStateKind {
    /// The fixed file stem for this kind.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::WorkspaceLayouts => "workspace-layouts.json",
            Self::DevicePreferences => "device-preferences.json",
        }
    }
}

/// Why a user-state operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserStateError {
    /// No device data directory is available.
    Unavailable(String),
    /// The file exceeds the bounded size.
    TooLarge {
        /// Actual byte length.
        size: usize,
    },
    /// The write's expected generation did not match the stored one.
    GenerationMismatch {
        /// The generation the writer expected.
        expected: u64,
        /// The generation actually stored.
        actual: u64,
    },
    /// The target path is a symlink or otherwise protected.
    Protected {
        /// The offending path (device-local, never a note path).
        path: String,
    },
    /// The stored document is not the expected envelope.
    Corrupt(String),
    /// The payload is not a JSON document.
    NotJson(String),
    /// The underlying filesystem failed.
    Fs(String),
}

impl std::fmt::Display for UserStateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "user state unavailable: {message}"),
            Self::TooLarge { size } => {
                write!(
                    f,
                    "user state file is {size} bytes, over the {MAX_USER_STATE_BYTES} byte bound"
                )
            }
            Self::GenerationMismatch { expected, actual } => {
                write!(
                    f,
                    "user state generation mismatch: expected {expected}, stored {actual}"
                )
            }
            Self::Protected { path } => write!(f, "user state path is protected: {path}"),
            Self::Corrupt(message) => write!(f, "user state file is corrupt: {message}"),
            Self::NotJson(message) => write!(f, "user state payload is not JSON: {message}"),
            Self::Fs(message) => write!(f, "user state filesystem error: {message}"),
        }
    }
}

impl std::error::Error for UserStateError {}

/// One stored user-state document: generation plus its codec payload bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredUserState {
    /// Monotonic generation, starting at 1 for the first write.
    pub generation: u64,
    /// The payload bytes (a codec envelope document).
    pub payload: Vec<u8>,
}

/// The device-local store for one vault's user state.
pub struct UserStateStore {
    // Manual `Debug`: the injected filesystem handle is not meaningful in
    // diagnostics and must not leak device paths.
    fs: Box<dyn Fs + Send + Sync>,
    root: PathBuf,
}

impl std::fmt::Debug for UserStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserStateStore").finish_non_exhaustive()
    }
}

impl UserStateStore {
    /// Build a store rooted at `<data>/flavite/workspace/<vault-uuid>`.
    ///
    /// # Errors
    /// Returns [`UserStateError::Unavailable`] when no OS data directory
    /// exists; the host then reports device persistence as unavailable
    /// instead of failing silently.
    pub fn open(fs: Box<dyn Fs + Send + Sync>, vault_uuid: &str) -> Result<Self, UserStateError> {
        let data = get_data_dir().ok_or_else(|| {
            UserStateError::Unavailable("no OS data directory is available".into())
        })?;
        Ok(Self::at_root(
            fs,
            data.join("flavite").join("workspace").join(vault_uuid),
        ))
    }

    /// Build a store at an explicit root (tests and fault injection).
    #[must_use]
    pub fn at_root(fs: Box<dyn Fs + Send + Sync>, root: PathBuf) -> Self {
        Self { fs, root }
    }

    /// The fixed file path for one kind.
    #[must_use]
    pub fn path_for(&self, kind: UserStateKind) -> PathBuf {
        self.root.join(kind.file_name())
    }

    /// Read the stored document for a kind, `None` when nothing was ever
    /// written.
    ///
    /// # Errors
    /// Returns typed errors for bounds, protection, and corruption.
    pub fn read(&self, kind: UserStateKind) -> Result<Option<StoredUserState>, UserStateError> {
        let path = self.path_for(kind);
        self.guard_protected(&path)?;
        let bytes = match self.fs.read_if_exists(&path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            Err(error) => return Err(UserStateError::Fs(error.to_string())),
        };
        if bytes.len() as u64 > MAX_USER_STATE_BYTES {
            return Err(UserStateError::TooLarge { size: bytes.len() });
        }
        let envelope: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|error| UserStateError::Corrupt(error.to_string()))?;
        let generation = envelope
            .get("generation")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| UserStateError::Corrupt("missing generation".into()))?;
        let payload_text = envelope
            .get("payload")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| UserStateError::Corrupt("missing payload".into()))?;
        let payload = payload_text.as_bytes().to_vec();
        Ok(Some(StoredUserState {
            generation,
            payload,
        }))
    }

    /// Write the payload under a compare-and-set generation receipt. The
    /// previous file is kept as `<name>.bak` until the new write is durable.
    ///
    /// # Errors
    /// Returns [`UserStateError::GenerationMismatch`] when another writer
    /// advanced the file first; matching receipts only.
    pub fn write(
        &mut self,
        kind: UserStateKind,
        expected_generation: u64,
        payload: &[u8],
    ) -> Result<u64, UserStateError> {
        if payload.len() as u64 > MAX_USER_STATE_BYTES {
            return Err(UserStateError::TooLarge {
                size: payload.len(),
            });
        }
        let _payload_is_json: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|error| UserStateError::NotJson(error.to_string()))?;
        let path = self.path_for(kind);
        let backup = path.with_extension("bak");
        self.guard_protected(&path)?;
        self.guard_protected(&backup)?;
        self.ensure_root()?;
        let actual = self.read(kind)?.map_or(0, |stored| stored.generation);
        if actual != expected_generation {
            return Err(UserStateError::GenerationMismatch {
                expected: expected_generation,
                actual,
            });
        }
        let next = expected_generation.saturating_add(1);
        // The payload travels as a JSON string so the stored bytes are
        // verbatim: reads return exactly what the codec wrote.
        let payload_text = String::from_utf8(payload.to_vec())
            .map_err(|error| UserStateError::NotJson(error.to_string()))?;
        let envelope = serde_json::json!({ "generation": next, "payload": payload_text });
        let bytes = serde_json::to_vec_pretty(&envelope)
            .map_err(|error| UserStateError::Corrupt(error.to_string()))?;
        // Keep the last good copy before replacing it.
        if let Some(previous) = self
            .fs
            .read_if_exists(&path)
            .map_err(|error| UserStateError::Fs(error.to_string()))?
        {
            self.fs
                .write_new(&backup, &previous)
                .map_err(|error| UserStateError::Fs(error.to_string()))?;
        }
        // One shared durability primitive (§4.4/D6, consolidated rather than
        // reimplemented): temp file, permission copy, fsync, rename, dir fsync.
        flavite_fs::atomic::atomic_write(self.fs.as_ref(), &path, &bytes)
            .map_err(|error| UserStateError::Fs(error.to_string()))?;
        Ok(next)
    }

    /// The last-good backup bytes, when one exists (diagnosis path; the
    /// store never silently restores from it).
    ///
    /// # Errors
    /// Returns filesystem errors and protection violations.
    pub fn read_backup(&self, kind: UserStateKind) -> Result<Option<Vec<u8>>, UserStateError> {
        let backup = self.path_for(kind).with_extension("bak");
        self.guard_protected(&backup)?;
        self.fs
            .read_if_exists(&backup)
            .map_err(|error| UserStateError::Fs(error.to_string()))
    }

    fn ensure_root(&mut self) -> Result<(), UserStateError> {
        self.fs
            .create_dir_all(&self.root)
            .map_err(|error| UserStateError::Fs(error.to_string()))
    }

    fn guard_protected(&self, path: &Path) -> Result<(), UserStateError> {
        if let Ok(metadata) = self.fs.metadata(path)
            && metadata.is_symlink
        {
            return Err(UserStateError::Protected {
                path: path.display().to_string(),
            });
        }
        Ok(())
    }
}
