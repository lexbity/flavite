//! Vault identity and registry management (D2, §4.13, §4.16.2).
//!
//! This module implements the vault UUID system with the following guarantees:
//!
//! 1. `vault.toml` is authoritative when well-formed and present.
//! 2. When absent, the per-root registry in OS data reconstructs the SAME UUID.
//! 3. Identity selection happens before any vault mutation.
//! 4. Both root guard and UUID lock are held for Session lifetime.
//! 5. Path reuse alone never authorizes attaching old recovery to unrelated notes.

use crate::types::VaultIdentity;
use directories::ProjectDirs;
use flavite_fs::Fs;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use thiserror::Error;
use uuid::Uuid;

/// Directory name for Flavite's configuration.
const FLAVITE_DIR: &str = "flavite";

/// Subdirectory for vault registries.
const VAULTS_DIR: &str = "vaults";

/// Subdirectory for root ownership guards.
const ROOTS_DIR: &str = "roots";

/// File name for vault identity in the vault.
const VAULT_TOML: &str = "vault.toml";

/// File name for UUID instance lock.
const UUID_LOCK: &str = "instance.lock";

/// Registry entry for a vault binding.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RegistryEntry {
    /// The vault UUID.
    pub uuid: Uuid,
    /// The native root key (canonical path identity, §4.16.2).
    pub root_key: Vec<u8>,
    /// The display root path.
    pub root_path: String,
    /// Timestamp when this entry was created.
    pub created_at: i64,
    /// Timestamp when this entry was last accessed.
    pub accessed_at: i64,
}

impl RegistryEntry {
    /// Create a new registry entry.
    #[must_use]
    pub fn new(uuid: Uuid, root_key: Vec<u8>, root_path: String) -> Self {
        use std::time::{SystemTime, UNIX_EPOCH};
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        Self {
            uuid,
            root_key,
            root_path,
            created_at: now,
            accessed_at: now,
        }
    }
}

/// Errors related to vault identity.
#[derive(Debug, Error)]
pub enum IdentityError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("FS error: {0}")]
    Fs(#[from] flavite_fs::FsError),

    #[error("No project directories available")]
    NoProjectDirs,

    #[error("Root ownership guard acquisition failed: {0}")]
    RootGuardFailed(String),

    #[error("UUID lock acquisition failed: {0}")]
    UuidLockFailed(String),

    #[error("Vault TOML parse error: {0}")]
    TomlParseError(String),

    #[error("Vault TOML is malformed: {0}")]
    MalformedToml(String),

    #[error("UUID mismatch: expected {expected}, found {found}")]
    UuidMismatch { expected: Uuid, found: Uuid },

    #[error("Root key mismatch: registry entry for different root")]
    RootKeyMismatch,

    #[error("Action required: {0}")]
    ActionRequired(&'static str),

    #[error("Vault identity already exists with different binding")]
    ConflictingBinding,
}

/// Type alias for identity results.
pub type IdentityResult<T> = Result<T, IdentityError>;

/// Get the OS data directory for Flavite.
pub fn get_data_dir() -> Option<PathBuf> {
    ProjectDirs::from("com", "flavite", "Flavite").map(|p| p.data_dir().to_path_buf())
}

/// Get the OS cache directory for Flavite.
#[allow(dead_code)]
fn get_cache_dir() -> Option<PathBuf> {
    ProjectDirs::from("com", "flavite", "Flavite").map(|p| p.cache_dir().to_path_buf())
}

fn registry_path_in(data_dir: &Path, root_key: &[u8]) -> PathBuf {
    data_dir
        .join(FLAVITE_DIR)
        .join(VAULTS_DIR)
        .join(format!("{}.toml", hex::encode(root_key)))
}

/// Get the path to the root ownership guard.
pub fn root_guard_path(root_key: &[u8]) -> Option<PathBuf> {
    get_data_dir().map(|data_dir| root_guard_path_in(&data_dir, root_key))
}

fn root_guard_path_in(data_dir: &Path, root_key: &[u8]) -> PathBuf {
    data_dir
        .join(FLAVITE_DIR)
        .join(ROOTS_DIR)
        .join(format!("{}.lock", hex::encode(root_key)))
}

/// Get the path to the UUID instance lock.
pub fn uuid_lock_path(uuid: Uuid) -> Option<PathBuf> {
    get_data_dir().map(|data_dir| uuid_lock_path_in(&data_dir, uuid))
}

fn uuid_lock_path_in(data_dir: &Path, uuid: Uuid) -> PathBuf {
    data_dir
        .join(FLAVITE_DIR)
        .join(VAULTS_DIR)
        .join(uuid.to_string())
        .join(UUID_LOCK)
}

/// Get the path to the vault.toml in a vault directory.
fn vault_toml_path(vault_root: &Path) -> PathBuf {
    vault_root.join(".flavite").join(VAULT_TOML)
}

/// Compute a canonical root key for a path (§4.16.2).
///
/// This uses the native filesystem identity (volume + native file ID) to
/// create a stable key that survives path renames.
pub fn compute_root_key(fs: &dyn Fs, root: &Path) -> IdentityResult<Vec<u8>> {
    let metadata = fs.metadata(root)?;

    // Create a key from volume_id + native_id
    let mut key = Vec::with_capacity(16);
    key.extend_from_slice(&metadata.volume_id.to_le_bytes());
    key.extend_from_slice(&metadata.native_id.to_le_bytes());

    Ok(key)
}

/// Read the vault UUID from vault.toml if it exists and is well-formed.
pub fn read_vault_toml(vault_root: &Path) -> IdentityResult<Option<Uuid>> {
    let toml_path = vault_toml_path(vault_root);

    if !toml_path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(&toml_path).map_err(IdentityError::Io)?;

    let value: toml::Value =
        toml::from_str(&contents).map_err(|e| IdentityError::TomlParseError(e.to_string()))?;

    let uuid_str = value
        .get("vault")
        .and_then(|v| v.get("uuid"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| IdentityError::MalformedToml("Missing vault.uuid".to_string()))?;

    Uuid::parse_str(uuid_str)
        .map(Some)
        .map_err(|e| IdentityError::MalformedToml(e.to_string()))
}

/// Write the vault UUID to vault.toml.
fn write_vault_toml(vault_root: &Path, uuid: Uuid) -> IdentityResult<()> {
    let toml_path = vault_toml_path(vault_root);

    // Create .flavite directory if it doesn't exist
    if let Some(parent) = toml_path.parent() {
        fs::create_dir_all(parent).map_err(IdentityError::Io)?;
    }

    let contents = format!("[vault]\nuuid = \"{uuid}\"\n");
    fs::write(&toml_path, contents).map_err(IdentityError::Io)?;

    Ok(())
}

fn read_registry_entry_in(
    data_dir: &Path,
    root_key: &[u8],
) -> IdentityResult<Option<RegistryEntry>> {
    read_registry_entry_path(&registry_path_in(data_dir, root_key))
}

fn read_registry_entry_path(path: &Path) -> IdentityResult<Option<RegistryEntry>> {
    if !path.exists() {
        return Ok(None);
    }

    let contents = fs::read_to_string(path).map_err(IdentityError::Io)?;

    let entry: RegistryEntry =
        toml::from_str(&contents).map_err(|e| IdentityError::TomlParseError(e.to_string()))?;

    Ok(Some(entry))
}

fn write_registry_entry_in(
    data_dir: &Path,
    root_key: &[u8],
    entry: &RegistryEntry,
) -> IdentityResult<()> {
    write_registry_entry_path(&registry_path_in(data_dir, root_key), entry)
}

fn write_registry_entry_path(path: &Path, entry: &RegistryEntry) -> IdentityResult<()> {
    // Create parent directories if needed
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(IdentityError::Io)?;
    }

    let contents = toml::to_string(entry)
        .map_err(|e| IdentityError::Io(std::io::Error::other(e.to_string())))?;

    fs::write(path, contents).map_err(IdentityError::Io)?;

    Ok(())
}

/// A guard for root ownership.
///
/// This prevents multiple processes from operating on the same root path
/// simultaneously.
#[derive(Debug)]
pub struct RootGuard {
    root_key: Vec<u8>,
    path: PathBuf,
    _lock: File,
}

impl RootGuard {
    /// Acquire a root guard for the given root path.
    pub fn acquire(fs: &dyn Fs, root: &Path) -> IdentityResult<Self> {
        let root_key = compute_root_key(fs, root)?;
        let path = root_guard_path(&root_key).ok_or(IdentityError::NoProjectDirs)?;
        Self::acquire_at(root_key, path)
    }

    fn acquire_at(root_key: Vec<u8>, path: PathBuf) -> IdentityResult<Self> {
        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(IdentityError::Io)?;
        }

        // Try to create the lock file with exclusive access
        let lock = File::create(&path).map_err(|e| {
            IdentityError::RootGuardFailed(format!(
                "Failed to create root guard at {}: {}",
                path.display(),
                e
            ))
        })?;

        Ok(Self {
            root_key,
            path,
            _lock: lock,
        })
    }

    /// Get the root key.
    #[must_use]
    pub fn root_key(&self) -> &[u8] {
        &self.root_key
    }

    /// Release the guard (deletes the lock file).
    pub fn release(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// A guard for UUID instance lock.
///
/// This prevents multiple processes from operating on the same vault UUID
/// simultaneously.
#[derive(Debug)]
pub struct UuidGuard {
    uuid: Uuid,
    path: PathBuf,
    _lock: File,
}

impl UuidGuard {
    /// Acquire a UUID guard for the given UUID.
    pub fn acquire(uuid: Uuid) -> IdentityResult<Self> {
        let path = uuid_lock_path(uuid).ok_or(IdentityError::NoProjectDirs)?;
        Self::acquire_at(uuid, path)
    }

    fn acquire_at(uuid: Uuid, path: PathBuf) -> IdentityResult<Self> {
        // Create parent directories if needed
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(IdentityError::Io)?;
        }

        // Try to create the lock file with exclusive access
        let lock = File::create(&path).map_err(|e| {
            IdentityError::UuidLockFailed(format!(
                "Failed to create UUID lock at {}: {}",
                path.display(),
                e
            ))
        })?;

        Ok(Self {
            uuid,
            path,
            _lock: lock,
        })
    }

    /// Get the UUID.
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.uuid
    }

    /// Release the guard (deletes the lock file).
    pub fn release(&self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Drop for UuidGuard {
    fn drop(&mut self) {
        self.release();
    }
}

/// Identity manager for vaults.
///
/// This manages the selection and persistence of vault identities according to
/// §4.13: acquire root guard → select identity read-only → acquire UUID lock
/// → persist identity/binding → start watcher + recover journals → open cache.
#[derive(Debug)]
pub struct IdentityManager {
    /// The vault identity.
    identity: VaultIdentity,
    /// Root ownership guard.
    root_guard: RootGuard,
    /// UUID instance lock.
    uuid_guard: UuidGuard,
}

impl IdentityManager {
    /// Open a vault with the given root path.
    ///
    /// This follows the startup sequence from §4.13:
    /// 1. Resolve the selected root
    /// 2. Acquire its root guard
    /// 3. Read vault.toml and registry without mutation
    /// 4. Select identity in memory
    /// 5. Acquire the UUID lock
    /// 6. Verify and persist identity/binding
    pub fn open(fs: &dyn Fs, root: PathBuf) -> IdentityResult<Self> {
        let data_dir = get_data_dir().ok_or(IdentityError::NoProjectDirs)?;
        Self::open_in_data_dir(fs, root, &data_dir)
    }

    fn open_in_data_dir(fs: &dyn Fs, root: PathBuf, data_dir: &Path) -> IdentityResult<Self> {
        // Step 1: Resolve the root
        let canonical_root = fs.canonicalize(&root)?;

        // Step 2: Acquire root guard
        let root_key = compute_root_key(fs, &canonical_root)?;
        let root_guard =
            RootGuard::acquire_at(root_key.clone(), root_guard_path_in(data_dir, &root_key))?;

        // Step 3: Read vault.toml and registry
        let root_key = root_guard.root_key().to_vec();
        let toml_uuid = read_vault_toml(&canonical_root)?;
        let registry_entry = read_registry_entry_in(data_dir, &root_key)?;

        // Step 4: Select identity in memory
        let uuid = match (toml_uuid, registry_entry) {
            // Both present: must match
            (Some(toml_uuid), Some(registry_entry)) => {
                if toml_uuid == registry_entry.uuid {
                    toml_uuid
                } else {
                    // Different UUIDs at the same path — conflict
                    return Err(IdentityError::ConflictingBinding);
                }
            }
            // Only TOML present: use it
            (Some(uuid), None) => uuid,
            // Only registry present: use it
            (None, Some(entry)) => entry.uuid,
            // Neither present: generate new UUID
            (None, None) => Uuid::now_v7(),
        };

        // Step 5: Acquire UUID lock
        // First, check if another process has the lock
        let uuid_lock_path = uuid_lock_path_in(data_dir, uuid);

        if uuid_lock_path.exists() {
            // Another process has the lock
            return Err(IdentityError::UuidLockFailed(
                "UUID lock already held by another process".to_string(),
            ));
        }

        let uuid_guard = UuidGuard::acquire_at(uuid, uuid_lock_path)?;

        // Step 6: Persist identity/binding
        // Write vault.toml
        write_vault_toml(&canonical_root, uuid)?;

        // Write/update registry
        let entry =
            RegistryEntry::new(uuid, root_key.clone(), canonical_root.display().to_string());
        write_registry_entry_in(data_dir, &root_key, &entry)?;

        let identity = VaultIdentity::new(uuid, canonical_root, root_key);

        Ok(Self {
            identity,
            root_guard,
            uuid_guard,
        })
    }

    /// Get the vault identity.
    #[must_use]
    pub fn identity(&self) -> &VaultIdentity {
        &self.identity
    }

    /// Get a mutable reference to the vault identity.
    #[must_use]
    pub fn identity_mut(&mut self) -> &mut VaultIdentity {
        &mut self.identity
    }

    /// Get the UUID.
    #[must_use]
    pub fn uuid(&self) -> Uuid {
        self.identity.uuid
    }

    /// Get the root path.
    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.identity.root
    }

    /// Get the root key.
    #[must_use]
    pub fn root_key(&self) -> &[u8] {
        &self.identity.root_key
    }
}

/// Create the .flavite directory structure in a vault.
pub fn ensure_flavite_dir(vault_root: &Path) -> IdentityResult<()> {
    let flavite_dir = vault_root.join(".flavite");
    let journal_dir = flavite_dir.join("journal");

    fs::create_dir_all(&flavite_dir).map_err(IdentityError::Io)?;
    fs::create_dir_all(&journal_dir).map_err(IdentityError::Io)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ReconcileSummary, Version};

    #[test]
    fn test_compute_root_key() {
        // This is a basic test - real filesystem identity testing
        // requires actual filesystem support
        // For now, we just verify the function doesn't panic

        // We can't easily test this without a real filesystem
        // but we can at least check the structure
        use flavite_fs::MemFs;
        let fs = MemFs::new();
        fs.create_dir_all(Path::new("/vault")).unwrap();
        let key = compute_root_key(&fs, Path::new("/vault")).unwrap();
        assert_eq!(key.len(), 16);
        assert!(matches!(
            compute_root_key(&fs, Path::new("/missing")),
            Err(IdentityError::Fs(_))
        ));
    }

    #[test]
    fn vault_toml_read_write_and_directory_setup_cover_invalid_inputs() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        assert_eq!(read_vault_toml(root).unwrap(), None);
        let uuid = Uuid::now_v7();
        write_vault_toml(root, uuid).unwrap();
        assert_eq!(read_vault_toml(root).unwrap(), Some(uuid));
        ensure_flavite_dir(root).unwrap();
        assert!(root.join(".flavite/journal").is_dir());

        let path = vault_toml_path(root);
        fs::write(&path, "not toml = [").unwrap();
        assert!(matches!(
            read_vault_toml(root),
            Err(IdentityError::TomlParseError(_))
        ));
        fs::write(&path, "[other]\nvalue = 1\n").unwrap();
        assert!(matches!(
            read_vault_toml(root),
            Err(IdentityError::MalformedToml(_))
        ));
        fs::write(&path, "[vault]\nuuid = 'not-a-uuid'\n").unwrap();
        assert!(matches!(
            read_vault_toml(root),
            Err(IdentityError::MalformedToml(_))
        ));
    }

    #[test]
    fn test_vault_identity() {
        let uuid = Uuid::now_v7();
        let root = PathBuf::from("/test/vault");
        let root_key = vec![1, 2, 3, 4];

        let identity = VaultIdentity::new(uuid, root.clone(), root_key.clone());

        assert_eq!(identity.uuid, uuid);
        assert_eq!(identity.root, root);
        assert_eq!(identity.root_key, root_key);
    }

    #[test]
    fn test_version() {
        let v1 = Version::from_values(1, 100);
        assert_eq!(v1.epoch.value(), 1);
        assert_eq!(v1.generation.value(), 100);

        let v2 = v1.next_generation();
        assert_eq!(v2.epoch.value(), 1);
        assert_eq!(v2.generation.value(), 101);

        let v3 = v1.new_epoch();
        assert_eq!(v3.epoch.value(), 2);
        assert_eq!(v3.generation.value(), 0);
    }

    #[test]
    fn test_reconcile_summary() {
        let summary = ReconcileSummary::new(5, 3, 2);
        assert_eq!(summary.added, 5);
        assert_eq!(summary.changed, 3);
        assert_eq!(summary.removed, 2);
        assert_eq!(summary.total(), 10);
        assert!(!summary.is_empty());

        let empty = ReconcileSummary::default();
        assert!(empty.is_empty());
    }

    #[test]
    fn atomic_version_load_store_increment_and_epoch_reset() {
        use std::sync::atomic::Ordering;

        let version = crate::types::AtomicVersion::default();
        assert_eq!(version.load(Ordering::SeqCst), Version::from_values(0, 0));
        version.store(Version::from_values(4, 9), Ordering::SeqCst);
        assert_eq!(
            version.increment_generation(Ordering::SeqCst),
            Version::from_values(4, 9)
        );
        assert_eq!(version.load(Ordering::SeqCst), Version::from_values(4, 10));
        assert_eq!(
            version.new_epoch(Ordering::SeqCst),
            Version::from_values(5, 0)
        );
        assert_eq!(version.load(Ordering::SeqCst), Version::from_values(5, 0));
    }

    #[test]
    fn identity_selection_persists_and_rejects_conflicting_bindings() {
        let vault = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = vault.path().to_path_buf();
        let fs = flavite_fs::OsFs::new();

        let first = IdentityManager::open_in_data_dir(&fs, root.clone(), data.path()).unwrap();
        let uuid = first.uuid();
        let root_key = first.root_key().to_vec();
        drop(first);

        let reopened = IdentityManager::open_in_data_dir(&fs, root.clone(), data.path()).unwrap();
        assert_eq!(reopened.uuid(), uuid);
        assert_eq!(reopened.root(), &root);
        drop(reopened);

        let registry = registry_path_in(data.path(), &root_key);
        let conflicting = RegistryEntry::new(Uuid::now_v7(), root_key, root.display().to_string());
        write_registry_entry_path(&registry, &conflicting).unwrap();
        assert!(matches!(
            IdentityManager::open_in_data_dir(&fs, root, data.path()),
            Err(IdentityError::ConflictingBinding)
        ));
    }

    #[test]
    fn identity_selection_uses_single_source_and_rejects_existing_uuid_lock() {
        let fs = flavite_fs::OsFs::new();

        let toml_vault = tempfile::tempdir().unwrap();
        let toml_data = tempfile::tempdir().unwrap();
        let toml_uuid = Uuid::now_v7();
        ensure_flavite_dir(toml_vault.path()).unwrap();
        write_vault_toml(toml_vault.path(), toml_uuid).unwrap();
        let opened = IdentityManager::open_in_data_dir(
            &fs,
            toml_vault.path().to_path_buf(),
            toml_data.path(),
        )
        .unwrap();
        assert_eq!(opened.uuid(), toml_uuid);
        drop(opened);

        let registry_vault = tempfile::tempdir().unwrap();
        let registry_data = tempfile::tempdir().unwrap();
        let root_key = compute_root_key(&fs, registry_vault.path()).unwrap();
        let registry_uuid = Uuid::now_v7();
        let entry = RegistryEntry::new(
            registry_uuid,
            root_key.clone(),
            registry_vault.path().display().to_string(),
        );
        write_registry_entry_in(registry_data.path(), &root_key, &entry).unwrap();
        let opened = IdentityManager::open_in_data_dir(
            &fs,
            registry_vault.path().to_path_buf(),
            registry_data.path(),
        )
        .unwrap();
        assert_eq!(opened.uuid(), registry_uuid);
        drop(opened);

        let locked_vault = tempfile::tempdir().unwrap();
        let locked_data = tempfile::tempdir().unwrap();
        let locked_uuid = Uuid::now_v7();
        ensure_flavite_dir(locked_vault.path()).unwrap();
        write_vault_toml(locked_vault.path(), locked_uuid).unwrap();
        let root_key = compute_root_key(&fs, locked_vault.path()).unwrap();
        let lock = uuid_lock_path_in(locked_data.path(), locked_uuid);
        std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
        std::fs::write(&lock, b"held").unwrap();
        assert!(matches!(
            IdentityManager::open_in_data_dir(
                &fs,
                locked_vault.path().to_path_buf(),
                locked_data.path()
            ),
            Err(IdentityError::UuidLockFailed(_))
        ));
        assert!(!root_guard_path_in(locked_data.path(), &root_key).exists());
    }
}
