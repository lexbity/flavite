//! Device-local and vault-local user state storage (RFC-0003 §22, W06/W07,
//! D-11/D-12).
//!
//! Workspace layouts and device preferences are device-local user state in
//! the OS data directory, keyed by vault UUID: never note truth, never
//! synced through the vault. The store owns fixed, validated locations —
//! no arbitrary caller filenames — with atomic generations, a last-good
//! backup, and explicit fault surfaces for tests.
//!
//! Graph pin coordinates keep their existing vault-relative location
//! (`.flavite/graph.json`) and file format (D-11: "Graph pin coordinates
//! retain their existing vault-user-state location and semantics behind a
//! consolidated storage service"). `VaultUserStateStore` moves that storage
//! behind the API's `Fs` while the UI keeps only the in-memory codec and
//! identity rules (TD-11).

use crate::identity::resolve_data_dir;
use flavite_fs::Fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// App-launch memory (W10): the most recently opened vault root on this
    /// device. Lives in the app scope, not under any vault UUID.
    AppLaunch,
    /// RFC-0006 §6.10: device-local confirmed source-format hints, keyed by
    /// vault/root/native source identity and bound to the last confirmed raw
    /// hash. Never stored in the workspace layout; never note content.
    EncodingHints,
}

impl UserStateKind {
    /// The fixed file stem for this kind.
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::WorkspaceLayouts => "workspace-layouts.json",
            Self::DevicePreferences => "device-preferences.json",
            Self::AppLaunch => "app-launch.json",
            Self::EncodingHints => "encoding-hints.json",
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
    /// The operation mutates state that this vault mode must not write
    /// (RFC-0003 §11.2: a read-only vault still allows device-local layout
    /// writes, but graph pin storage modifies vault user state and is
    /// disabled).
    ReadOnly,
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
            Self::ReadOnly => write!(f, "vault is read-only: vault user state is not writable"),
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
        let data = resolve_data_dir(None).ok_or_else(|| {
            UserStateError::Unavailable("no OS data directory is available".into())
        })?;
        Ok(Self::at_root(
            fs,
            data.join("flavite").join("workspace").join(vault_uuid),
        ))
    }

    /// Build a store rooted at an explicit data directory (S8/M3):
    /// `<data>/flavite/workspace/<vault-uuid>`. Libraries never read the
    /// environment; tests and binaries pass the resolved directory here.
    ///
    /// # Errors
    /// Same `Unavailable` contract as [`UserStateStore::open`] when the OS
    /// data directory is unavailable.
    pub fn open_at(
        fs: Box<dyn Fs + Send + Sync>,
        data_dir: &Path,
        vault_uuid: &str,
    ) -> Result<Self, UserStateError> {
        Ok(Self::at_root(
            fs,
            data_dir.join("flavite").join("workspace").join(vault_uuid),
        ))
    }

    /// Build a store at an explicit root (tests and fault injection).
    #[must_use]
    pub fn at_root(fs: Box<dyn Fs + Send + Sync>, root: PathBuf) -> Self {
        Self { fs, root }
    }

    /// Build the device-wide app-scope store (W10): launch memory shared by
    /// every vault on this device, rooted at `<data>/flavite/app`.
    ///
    /// # Errors
    /// Returns [`UserStateError::Unavailable`] when no OS data directory
    /// exists.
    pub fn open_app_scope(fs: Box<dyn Fs + Send + Sync>) -> Result<Self, UserStateError> {
        let data = resolve_data_dir(None).ok_or_else(|| {
            UserStateError::Unavailable("no OS data directory is available".into())
        })?;
        Ok(Self::at_root(fs, data.join("flavite").join("app")))
    }

    /// Build the device-wide app-scope store at an explicit data directory
    /// (S8/M3): `<data>/flavite/app`.
    pub fn open_app_scope_at(
        fs: Box<dyn Fs + Send + Sync>,
        data_dir: &Path,
    ) -> Result<Self, UserStateError> {
        Ok(Self::at_root(fs, data_dir.join("flavite").join("app")))
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
        // Keep the last good copy before replacing it. The backup is
        // replaced atomically (not create-new): a save that follows an
        // earlier save must update the `.bak`, or the third save of any
        // kind would fail once a backup already exists.
        if let Some(previous) = self
            .fs
            .read_if_exists(&path)
            .map_err(|error| UserStateError::Fs(error.to_string()))?
        {
            flavite_fs::atomic::atomic_write(self.fs.as_ref(), &backup, &previous)
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

// ---------------------------------------------------------------------------
// Vault-local user state (graph pins; RFC-0003 TD-11, W07)
// ---------------------------------------------------------------------------

/// The fixed set of vault-relative user-state kinds. Like
/// [`UserStateKind`], callers cannot invent locations: the file name is
/// part of the kind, not an argument (§11.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultUserStateKind {
    /// Graph position pins — `<vault>/.flavite/graph.json`, existing
    /// format, existing semantics (D-11).
    GraphPins,
}

impl VaultUserStateKind {
    /// The fixed vault-relative path for this kind.
    #[must_use]
    pub const fn relative_path(self) -> &'static str {
        match self {
            Self::GraphPins => ".flavite/graph.json",
        }
    }
}

/// Vault-relative user state stored behind the injected `Fs`.
///
/// The graph-pins document keeps its established on-disk bytes — the
/// `{"version":1,"pins":[…]}` shape with unknown-field preservation is the
/// UI codec's contract, not this store's. The store therefore treats the
/// file as an opaque JSON document: bounded size, symlink protection, one
/// shared atomic-replace primitive (§4.4/D6), and a compare-and-set
/// generation.
///
/// The generation counter is process-scoped by design: the vault's single
/// writer lock already serializes across processes (§11.2), and the CAS
/// exists to reject stale in-process writes after a re-read — never to
/// rewrite the established file format with a new envelope.
pub struct VaultUserStateStore {
    fs: Box<dyn Fs + Send + Sync>,
    vault_root: PathBuf,
    graph_pins_generation: AtomicU64,
}

impl std::fmt::Debug for VaultUserStateStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VaultUserStateStore")
            .finish_non_exhaustive()
    }
}

impl VaultUserStateStore {
    /// Build a store rooted at the open vault's root.
    #[must_use]
    pub fn new(fs: Box<dyn Fs + Send + Sync>, vault_root: &Path) -> Self {
        Self {
            fs,
            vault_root: vault_root.to_path_buf(),
            graph_pins_generation: AtomicU64::new(0),
        }
    }

    /// The fixed path for one vault-relative kind.
    #[must_use]
    pub fn path_for(&self, kind: VaultUserStateKind) -> PathBuf {
        let mut path = self.vault_root.clone();
        for component in Path::new(kind.relative_path()) {
            path.push(component);
        }
        path
    }

    /// Read the graph-pins document; `None` when nothing was ever written.
    ///
    /// # Errors
    /// Typed bounds, protection, and filesystem failures.
    pub fn read_graph_pins(&self) -> Result<Option<StoredUserState>, UserStateError> {
        let path = self.path_for(VaultUserStateKind::GraphPins);
        self.guard_protected(&path)?;
        let bytes = match self.fs.read_if_exists(&path) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(None),
            Err(error) => return Err(UserStateError::Fs(error.to_string())),
        };
        if bytes.len() as u64 > MAX_USER_STATE_BYTES {
            return Err(UserStateError::TooLarge { size: bytes.len() });
        }
        Ok(Some(StoredUserState {
            generation: self.graph_pins_generation.load(Ordering::Acquire),
            payload: bytes,
        }))
    }

    /// Write the graph-pins document under a compare-and-set generation
    /// receipt; returns the new generation.
    ///
    /// The payload must be a JSON document (the UI codec's
    /// `{"version":1,…}` shape) and the write goes through the same
    /// `flavite_fs::atomic::atomic_write` durability primitive as device
    /// user state — no separate temp/replace helper survives (TD-11).
    ///
    /// # Errors
    /// [`UserStateError::GenerationMismatch`] on a stale receipt,
    /// [`UserStateError::NotJson`] for a non-document payload,
    /// [`UserStateError::ReadOnly`] when `allow_writes` is false, and typed
    /// protection/filesystem failures.
    pub fn write_graph_pins(
        &self,
        expected_generation: u64,
        payload: &[u8],
        allow_writes: bool,
    ) -> Result<u64, UserStateError> {
        if !allow_writes {
            return Err(UserStateError::ReadOnly);
        }
        if payload.len() as u64 > MAX_USER_STATE_BYTES {
            return Err(UserStateError::TooLarge {
                size: payload.len(),
            });
        }
        let document: serde_json::Value = serde_json::from_slice(payload)
            .map_err(|error| UserStateError::NotJson(error.to_string()))?;
        if !document.is_object() {
            return Err(UserStateError::NotJson(
                "graph pins must be a JSON object".into(),
            ));
        }
        let path = self.path_for(VaultUserStateKind::GraphPins);
        let parent = path
            .parent()
            .ok_or_else(|| UserStateError::Fs("graph pins path has no parent".into()))?;
        self.guard_protected(&path)?;
        self.guard_protected(parent)?;
        let actual = self.graph_pins_generation.load(Ordering::Acquire);
        if actual != expected_generation {
            return Err(UserStateError::GenerationMismatch {
                expected: expected_generation,
                actual,
            });
        }
        if let Err(error) = self.fs.create_dir_all(parent) {
            return Err(UserStateError::Fs(error.to_string()));
        }
        if let Err(error) = flavite_fs::atomic::atomic_write(self.fs.as_ref(), &path, payload) {
            return Err(UserStateError::Fs(error.to_string()));
        }
        let next = expected_generation.saturating_add(1);
        self.graph_pins_generation.store(next, Ordering::Release);
        Ok(next)
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

#[cfg(test)]
mod vault_store_tests {
    use super::*;
    use flavite_fs::MemFs;

    fn store() -> (VaultUserStateStore, tempfile::TempDir) {
        // A real temp root with MemFs is the established fault-surface
        // pattern: paths exist for metadata, contents live in memory.
        let dir = tempfile::tempdir().expect("vault dir");
        let fs: Box<dyn Fs + Send + Sync> = Box::new(MemFs::new());
        (VaultUserStateStore::new(fs, dir.path()), dir)
    }

    #[test]
    fn graph_pins_round_trip_under_generation_cas() {
        let (store, _dir) = store();
        assert!(store.read_graph_pins().expect("read").is_none());
        let payload = br#"{"version":1,"pins":[]}"#;
        let next = store
            .write_graph_pins(0, payload, true)
            .expect("first write");
        assert_eq!(next, 1);
        let stored = store.read_graph_pins().expect("read").expect("present");
        assert_eq!(stored.generation, 1);
        assert_eq!(stored.payload, payload);
        // A stale receipt is a typed mismatch, never a silent overwrite.
        let stale = store.write_graph_pins(0, br#"{"version":1,"pins":[]}"#, true);
        assert_eq!(
            stale,
            Err(UserStateError::GenerationMismatch {
                expected: 0,
                actual: 1
            })
        );
    }

    #[test]
    fn read_only_vaults_reject_graph_pin_writes() {
        let (store, _dir) = store();
        assert_eq!(
            store.write_graph_pins(0, b"{}", false),
            Err(UserStateError::ReadOnly)
        );
    }

    #[test]
    fn graph_pins_payload_must_be_a_json_object() {
        let (store, _dir) = store();
        assert!(matches!(
            store.write_graph_pins(0, b"not json", true),
            Err(UserStateError::NotJson(_))
        ));
        assert!(matches!(
            store.write_graph_pins(0, b"[1,2]", true),
            Err(UserStateError::NotJson(_))
        ));
    }

    #[test]
    fn graph_pins_path_is_fixed_under_the_vault_root() {
        let dir = tempfile::tempdir().expect("vault dir");
        let fs: Box<dyn Fs + Send + Sync> = Box::new(MemFs::new());
        let store = VaultUserStateStore::new(fs, dir.path());
        let path = store.path_for(VaultUserStateKind::GraphPins);
        assert_eq!(path, dir.path().join(".flavite").join("graph.json"));
    }
}
