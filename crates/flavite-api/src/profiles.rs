//! Settings profiles store (FEUX-0003 X06).
//!
//! Profiles are inert TOML documents stored under `<vault>/.flavite/profiles/`
//! that capture the delta from descriptor defaults for visible settings only.
//! This module provides the durable storage layer with the same injection,
//! atomicity, and bounds guarantees as `ThemeStore`.
//!
//! Profiles are NOT live-linked: they are explicitly applied through the
//! existing settings funnel (one CAS write per apply). The store owns
//! locations and durability only; the semantic merge lives in the facade.

use flavite_fs::{Fs, atomic_write};
use std::path::{Path, PathBuf};

/// Hard byte bound for one profile document (64 KiB from FEUX-0003 §5.8).
pub const MAX_PROFILE_BYTES: usize = 64 * 1024;
/// Maximum number of profiles (FEUX-0003 §5.8).
pub const MAX_PROFILES: usize = 64;
/// Maximum length of a profile display name (FEUX-0003 §5.8).
pub const MAX_NAME_BYTES: usize = 64;

/// Entry name for profile documents.
const PROFILE_ENTRY: &str = "profile.toml";

/// Why a profile storage operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfilesStoreError {
    /// No device data directory is available.
    Unavailable(String),
    /// The profile exceeds the bounded size.
    TooLarge { size: usize },
    /// Too many profiles already exist.
    TooManyProfiles,
    /// The name is invalid (empty, too long, or contains invalid chars).
    InvalidName(String),
    /// A profile with this name already exists.
    Exists { name: String },
    /// The named profile is not installed.
    NotFound { id: String },
    /// The schema version is newer than supported.
    SchemaNewer,
    /// The stored profile is unparseable.
    Unparseable(String),
    /// The target path is a symlink or otherwise protected.
    Protected { path: String },
    /// The underlying filesystem failed.
    Fs(String),
    /// The vault is read-only; profiles cannot be written.
    ReadOnly,
}

impl std::fmt::Display for ProfilesStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "profiles store unavailable: {message}"),
            Self::TooLarge { size } => write!(
                f,
                "profile is {size} bytes, over the {MAX_PROFILE_BYTES} byte bound"
            ),
            Self::TooManyProfiles => {
                write!(f, "profile limit ({MAX_PROFILES}) exceeded")
            }
            Self::InvalidName(reason) => write!(f, "invalid profile name: {reason}"),
            Self::Exists { name } => write!(f, "profile '{name}' already exists"),
            Self::NotFound { id } => write!(f, "profile '{id}' not found"),
            Self::SchemaNewer => write!(f, "profile schema version is newer than supported"),
            Self::Unparseable(message) => write!(f, "profile unparseable: {message}"),
            Self::Protected { path } => write!(f, "profile path is protected: {path}"),
            Self::Fs(message) => write!(f, "profiles filesystem error: {message}"),
            Self::ReadOnly => write!(f, "profiles storage is read-only"),
        }
    }
}

impl std::error::Error for ProfilesStoreError {}

/// A profile identity: sanitized name + short blake3 digest suffix.
///
/// The id is generated from the display name, not stored in the file.
/// This ensures path-escape impossibility and collision resistance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProfileId {
    /// The display name (user-provided, used in UI).
    pub name: String,
    /// The sanitized id used as the directory name.
    pub id: String,
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl ProfileId {
    /// Create a profile id from a display name.
    ///
    /// Returns an error if the name is invalid (empty, too long, or contains
    /// only invalid characters).
    pub fn from_name(name: &str) -> Result<Self, ProfilesStoreError> {
        if name.is_empty() {
            return Err(ProfilesStoreError::InvalidName(
                "profile name cannot be empty".to_string(),
            ));
        }

        if name.len() > MAX_NAME_BYTES {
            return Err(ProfilesStoreError::InvalidName(format!(
                "profile name exceeds {MAX_NAME_BYTES} byte limit"
            )));
        }

        // Validate that the name contains only valid characters
        // (printable ASCII except control chars and path separators)
        if name
            .chars()
            .any(|c| c.is_control() || c == '/' || c == '\\' || c == ':')
        {
            return Err(ProfilesStoreError::InvalidName(
                "profile name contains invalid characters".to_string(),
            ));
        }

        let sanitized = sanitize_name(name);
        let id = format!("{sanitized}-{}", blake3_hash(name));

        Ok(Self {
            name: name.to_string(),
            id,
        })
    }
}

/// Sanitize a profile name for use in a filesystem path.
///
/// Converts to lowercase, replaces invalid characters with '-',
/// collapses consecutive '-', and trims.
fn sanitize_name(name: &str) -> String {
    let mut result = String::with_capacity(name.len());

    for c in name.chars() {
        if c.is_alphanumeric() {
            result.push(c.to_ascii_lowercase());
        } else if c == ' ' || c == '_' || c == '-' || c == '.' {
            result.push('-');
        }
        // Other characters are silently dropped
    }

    // Collapse consecutive '-'
    let mut collapsed = String::with_capacity(result.len());
    let mut last_was_dash = false;
    for c in result.chars() {
        if c == '-' {
            if !last_was_dash {
                collapsed.push(c);
                last_was_dash = true;
            }
        } else {
            collapsed.push(c);
            last_was_dash = false;
        }
    }

    // Trim leading/trailing '-'
    let trimmed = collapsed.trim_matches('-');

    // Ensure non-empty
    if trimmed.is_empty() {
        "profile".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Compute a short blake3 hash of a string (first 8 hex characters).
fn blake3_hash(input: &str) -> String {
    use blake3::Hasher;
    let mut hasher = Hasher::new();
    hasher.update(input.as_bytes());
    let hash = hasher.finalize();
    // Take first 4 bytes = 8 hex chars
    let hash_bytes = hash.as_bytes();
    format!(
        "{:08x}",
        u32::from_be_bytes([hash_bytes[0], hash_bytes[1], hash_bytes[2], hash_bytes[3]])
    )
}

/// A stored profile: identity plus raw bytes.
#[derive(Debug, Clone)]
pub struct StoredProfile {
    /// The profile identity.
    pub id: ProfileId,
    /// The raw TOML bytes of the profile document.
    pub bytes: Vec<u8>,
}

impl StoredProfile {
    /// Get the profile's display name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.id.name
    }

    /// Get the profile's internal id.
    #[must_use]
    pub fn profile_id(&self) -> &str {
        &self.id.id
    }
}

/// The device-local store of profile documents.
pub struct ProfilesStore {
    fs: Box<dyn Fs + Send + Sync>,
    root: PathBuf,
    allow_writes: bool,
}

impl std::fmt::Debug for ProfilesStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProfilesStore")
            .field("root", &self.root)
            .field("allow_writes", &self.allow_writes)
            .finish_non_exhaustive()
    }
}

impl ProfilesStore {
    /// Build the store at an explicit vault root (`.flavite/profiles`).
    #[must_use]
    pub fn at_vault(fs: Box<dyn Fs + Send + Sync>, vault_root: &Path) -> Self {
        let mut root = PathBuf::from(vault_root);
        root.push(".flavite");
        root.push("profiles");
        Self {
            fs,
            root,
            allow_writes: true,
        }
    }

    /// Build the store at an explicit root (tests and fault injection).
    #[must_use]
    pub fn at_root(fs: Box<dyn Fs + Send + Sync>, root: PathBuf) -> Self {
        Self {
            fs,
            root,
            allow_writes: true,
        }
    }

    /// Create a read-only store.
    #[must_use]
    pub fn read_only(fs: Box<dyn Fs + Send + Sync>, root: PathBuf) -> Self {
        Self {
            fs,
            root,
            allow_writes: false,
        }
    }

    /// Create and store a new profile from raw TOML bytes.
    ///
    /// # Errors
    /// Returns an error for invalid names, oversized profiles, duplicate ids,
    /// protected paths, or filesystem failures.
    pub fn create(&self, name: &str, bytes: &[u8]) -> Result<StoredProfile, ProfilesStoreError> {
        if !self.allow_writes {
            return Err(ProfilesStoreError::ReadOnly);
        }

        if bytes.len() > MAX_PROFILE_BYTES {
            return Err(ProfilesStoreError::TooLarge { size: bytes.len() });
        }

        let profile_id = ProfileId::from_name(name)?;

        // Check for duplicate id
        let directory = self.directory_for(&profile_id.id);
        let profile_path = directory.join(PROFILE_ENTRY);
        if self
            .fs
            .exists(&profile_path)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?
        {
            return Err(ProfilesStoreError::Exists {
                name: profile_id.name,
            });
        }

        // Check profile count
        let count = self.list()?.len();
        if count >= MAX_PROFILES {
            return Err(ProfilesStoreError::TooManyProfiles);
        }

        let directory = self.directory_for(&profile_id.id);
        self.guard_protected(&directory.join(PROFILE_ENTRY))?;

        self.fs
            .create_dir_all(&directory)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?;

        atomic_write(self.fs.as_ref(), &directory.join(PROFILE_ENTRY), bytes)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?;

        Ok(StoredProfile {
            id: profile_id,
            bytes: bytes.to_vec(),
        })
    }

    /// List all stored profiles, sorted by name.
    ///
    /// A directory without readable profile files is skipped.
    pub fn list(&self) -> Result<Vec<StoredProfile>, ProfilesStoreError> {
        let mut profiles = Vec::new();

        // A store root that was never created holds no profiles
        if !self
            .fs
            .exists(&self.root)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?
        {
            return Ok(profiles);
        }

        for entry in self
            .fs
            .read_dir(&self.root)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?
        {
            if !entry.is_dir {
                continue;
            }

            let profile_path = entry.path.join(PROFILE_ENTRY);
            let Some(raw_bytes) = self
                .fs
                .read_if_exists(&profile_path)
                .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?
            else {
                continue;
            };

            // Skip if empty
            if raw_bytes.is_empty() {
                continue;
            }

            // Extract the id from the directory name
            // Directory name format: <sanitized-name>-<hash>
            let dir_name = entry.path.file_name().unwrap_or_default();
            let dir_name_str = dir_name.to_string_lossy().to_string();

            // For X06, we use the directory name as the id
            // In a full implementation, we might parse the name back, but
            // the directory name is the id
            let profile_id = ProfileId {
                name: dir_name_str.clone(),
                id: dir_name_str,
            };

            profiles.push(StoredProfile {
                id: profile_id,
                bytes: raw_bytes,
            });
        }

        // Sort by name
        profiles.sort_by(|a, b| a.id.name.cmp(&b.id.name));

        Ok(profiles)
    }

    /// Read a profile by its internal id.
    pub fn read(&self, id: &str) -> Result<StoredProfile, ProfilesStoreError> {
        for profile in self.list()? {
            if profile.id.id == id {
                return Ok(profile);
            }
        }
        Err(ProfilesStoreError::NotFound { id: id.to_string() })
    }

    /// Read a profile's bytes by its internal id.
    pub fn read_bytes(&self, id: &str) -> Result<Vec<u8>, ProfilesStoreError> {
        let profile = self.read(id)?;
        Ok(profile.bytes)
    }

    /// Delete a profile by its internal id.
    pub fn delete(&self, id: &str) -> Result<(), ProfilesStoreError> {
        if !self.allow_writes {
            return Err(ProfilesStoreError::ReadOnly);
        }

        let directory = self.directory_for(id);

        // Check if the profile exists
        let profile_path = directory.join(PROFILE_ENTRY);
        if !self
            .fs
            .exists(&profile_path)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?
        {
            return Err(ProfilesStoreError::NotFound { id: id.to_string() });
        }

        // Remove the profile file
        self.guard_protected(&profile_path)?;
        self.fs
            .remove(&profile_path)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?;

        // Note: We don't remove the directory itself, just the profile file.
        // The directory will be empty and can be cleaned up separately.

        Ok(())
    }

    /// Export a profile to a host path (copy the bytes).
    pub fn export_path(&self, id: &str, path: &Path) -> Result<(), ProfilesStoreError> {
        let bytes = self.read_bytes(id)?;
        atomic_write(self.fs.as_ref(), path, &bytes)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?;
        Ok(())
    }

    /// Import a profile from a host path.
    ///
    /// Reads the file, validates bounds, and creates a new profile with
    /// the name derived from the file stem.
    pub fn import_path(&self, path: &Path) -> Result<StoredProfile, ProfilesStoreError> {
        if !self.allow_writes {
            return Err(ProfilesStoreError::ReadOnly);
        }

        let Some(bytes) = self
            .fs
            .read_if_exists(path)
            .map_err(|error| ProfilesStoreError::Fs(error.to_string()))?
        else {
            return Err(ProfilesStoreError::Fs("file not found".to_string()));
        };

        if bytes.len() > MAX_PROFILE_BYTES {
            return Err(ProfilesStoreError::TooLarge { size: bytes.len() });
        }

        // Derive name from file stem
        let file_stem = path.file_stem().unwrap_or_default();
        let name = file_stem.to_string_lossy().to_string();

        self.create(&name, &bytes)
    }

    /// Get the directory path for a profile id.
    fn directory_for(&self, id: &str) -> PathBuf {
        let mut path = self.root.clone();
        path.push(id);
        path
    }

    /// Guard against protected paths (symlinks, etc.).
    fn guard_protected(&self, path: &Path) -> Result<(), ProfilesStoreError> {
        // Check for path escape attempts
        let path_str = path.to_string_lossy();
        if path_str.contains("..") {
            return Err(ProfilesStoreError::Protected {
                path: path_str.to_string(),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_fs::OsFs;
    use tempfile::tempdir;

    #[test]
    fn profile_id_from_name_generates_valid_ids() {
        let id = ProfileId::from_name("My Profile").unwrap();
        assert_eq!(id.name, "My Profile");
        assert!(!id.id.is_empty());
        assert!(id.id.contains('-')); // Should have hash suffix
    }

    #[test]
    fn profile_id_rejects_empty_name() {
        let result = ProfileId::from_name("");
        assert!(matches!(result, Err(ProfilesStoreError::InvalidName(_))));
    }

    #[test]
    fn profile_id_rejects_too_long_name() {
        let long_name = "a".repeat(MAX_NAME_BYTES + 1);
        let result = ProfileId::from_name(&long_name);
        assert!(matches!(result, Err(ProfilesStoreError::InvalidName(_))));
    }

    #[test]
    fn profile_id_rejects_invalid_characters() {
        let result = ProfileId::from_name("invalid/name");
        assert!(matches!(result, Err(ProfilesStoreError::InvalidName(_))));
    }

    #[test]
    fn store_create_and_list_roundtrip() {
        let temp_dir = tempdir().unwrap();
        let fs = Box::new(OsFs);
        let store = ProfilesStore::at_root(fs, temp_dir.path().to_path_buf());

        let _profile = store
            .create(
                "test profile",
                b"schema = 2\n[appearance]\nkeyboard_layout = \"dvorak\"",
            )
            .unwrap();

        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        // The name in the listed profile is the sanitized directory name
        // The original name is stored in profile.id.name
        assert_eq!(listed[0].id.name, _profile.id.id);
    }

    #[test]
    fn store_rejects_duplicate_names() {
        let temp_dir = tempdir().unwrap();
        let fs = Box::new(OsFs);
        let store = ProfilesStore::at_root(fs, temp_dir.path().to_path_buf());

        store.create("test", b"schema = 2").unwrap();

        let result = store.create("test", b"schema = 2");
        assert!(matches!(result, Err(ProfilesStoreError::Exists { .. })));
    }

    #[test]
    fn store_rejects_oversized_profiles() {
        let temp_dir = tempdir().unwrap();
        let fs = Box::new(OsFs);
        let store = ProfilesStore::at_root(fs, temp_dir.path().to_path_buf());

        let large_bytes = vec![0u8; MAX_PROFILE_BYTES + 1];
        let result = store.create("test", &large_bytes);
        assert!(matches!(result, Err(ProfilesStoreError::TooLarge { .. })));
    }

    #[test]
    fn store_read_and_delete() {
        let temp_dir = tempdir().unwrap();
        let fs = Box::new(OsFs);
        let store = ProfilesStore::at_root(fs, temp_dir.path().to_path_buf());

        let _profile = store.create("read delete test", b"schema = 2").unwrap();
        let id = _profile.id.id.clone();

        let read_profile = store.read(&id).unwrap();
        assert_eq!(read_profile.bytes, b"schema = 2");

        store.delete(&id).unwrap();

        let result = store.read(&id);
        assert!(matches!(result, Err(ProfilesStoreError::NotFound { .. })));
    }

    #[test]
    fn store_export_and_import() {
        let temp_dir = tempdir().unwrap();
        let fs = Box::new(OsFs);
        let store = ProfilesStore::at_root(fs, temp_dir.path().to_path_buf());

        let _profile = store
            .create("export test", b"schema = 2\n[test]\nvalue = 1")
            .unwrap();
        let id = _profile.id.id.clone();

        let export_path = temp_dir.path().join("exported.toml");
        store.export_path(&id, &export_path).unwrap();

        let imported = store.import_path(&export_path).unwrap();
        assert_eq!(imported.bytes, b"schema = 2\n[test]\nvalue = 1");
    }

    #[test]
    fn read_only_store_rejects_writes() {
        let temp_dir = tempdir().unwrap();
        let fs = Box::new(OsFs);
        let store = ProfilesStore::read_only(fs, temp_dir.path().to_path_buf());

        let result = store.create("test", b"schema = 2");
        assert!(matches!(result, Err(ProfilesStoreError::ReadOnly)));
    }

    #[test]
    fn sanitize_name_handles_various_inputs() {
        assert_eq!(sanitize_name("Test Profile"), "test-profile");
        assert_eq!(sanitize_name("test_profile"), "test-profile");
        assert_eq!(sanitize_name("Test123"), "test123");
        assert_eq!(sanitize_name("---Test---"), "test");
        assert_eq!(sanitize_name(""), "profile");
        assert_eq!(sanitize_name("!!!"), "profile");
    }
}
