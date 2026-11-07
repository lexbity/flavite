//! Settings store for vault-anchored settings (RFC-0007).
//!
//! This module provides the durable storage layer for settings documents
//! in the vault, with CAS generations, atomic writes, and schema gating.
//!
//! # Design
//!
//! Settings live at `<vault>/.flavite/settings.toml` as a version-2 TOML
//! document. The store enforces:
//! - 256 KiB maximum document size (settings-specific bound)
//! - Symlink rejection on the settings file
//! - Atomic writes via the shared `flavite_fs::atomic::atomic_write` primitive
//! - Compare-and-set generation for conflict detection
//! - Schema gate: read-only mode for schema > current
//!
//! The codec authority lives in `flavite-settings`; this store owns the
//! IO boundary only.

use flavite_settings::codec::{parse, serialize};
use flavite_settings::document::SettingsDoc;
use flavite_settings::error::SettingsCodecError;
use flavite_settings::registry::SettingRegistry;
use flavite_settings::resolve::RegistryResolve;
use flavite_settings::value::SettingValue;
use flavite_vault::user_state::{UserStateError, VaultUserStateKind, VaultUserStateStore};
use std::sync::Arc;

/// Hard byte bound for a settings document (256 KiB from RFC-0007).
pub const MAX_SETTINGS_BYTES: usize = 256 * 1024;

/// Settings-specific error variants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingsStoreError {
    /// The settings document is too large (> 256 KiB).
    TooLarge { size: usize },
    /// The schema version is newer than supported; writes refused.
    SchemaNewer,
    /// The document is unparseable.
    Unparseable(String),
    /// A write conflict (CAS generation mismatch).
    Conflict {
        expected: u64,
        actual: u64,
    },
    /// The vault is read-only; settings cannot be written.
    ReadOnly,
    /// The underlying vault user state store failed.
    VaultStore(UserStateError),
    /// The settings codec failed.
    Codec(SettingsCodecError),
}

impl std::fmt::Display for SettingsStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLarge { size } => {
                write!(
                    f,
                    "settings document is {} bytes, over the {} byte bound",
                    size, MAX_SETTINGS_BYTES
                )
            }
            Self::SchemaNewer => write!(f, "settings schema version is newer than supported"),
            Self::Unparseable(msg) => write!(f, "settings document unparseable: {msg}"),
            Self::Conflict { expected, actual } => {
                write!(f, "settings CAS conflict: expected generation {expected}, got {actual}")
            }
            Self::ReadOnly => write!(f, "vault is read-only: settings are not writable"),
            Self::VaultStore(e) => write!(f, "vault user state error: {e}"),
            Self::Codec(e) => write!(f, "settings codec error: {e}"),
        }
    }
}

impl std::error::Error for SettingsStoreError {}

impl From<UserStateError> for SettingsStoreError {
    fn from(error: UserStateError) -> Self {
        match error {
            UserStateError::GenerationMismatch { expected, actual } => {
                Self::Conflict { expected, actual }
            }
            UserStateError::TooLarge { size } => {
                Self::TooLarge { size }
            }
            UserStateError::ReadOnly => {
                Self::ReadOnly
            }
            UserStateError::Protected { path } => {
                Self::VaultStore(UserStateError::Protected { path })
            }
            UserStateError::Corrupt(msg) => {
                Self::Unparseable(msg)
            }
            UserStateError::NotJson(msg) => {
                Self::Unparseable(msg)
            }
            other => Self::VaultStore(other),
        }
    }
}

impl From<SettingsCodecError> for SettingsStoreError {
    fn from(error: SettingsCodecError) -> Self {
        Self::Codec(error)
    }
}

/// Wire-compatible error codes (matching RFC-0007 constants).
impl SettingsStoreError {
    /// Returns the wire-compatible error code for this error.
    #[must_use]
    pub fn wire_code(&self) -> &'static str {
        use flavite_settings::error::*;
        match self {
            Self::TooLarge { .. } => SETTINGS_TOO_LARGE,
            Self::SchemaNewer => SETTINGS_SCHEMA_NEWER,
            Self::Unparseable(_) => SETTINGS_UNPARSEABLE,
            Self::Conflict { .. } => SETTINGS_CONFLICT,
            Self::ReadOnly => SETTINGS_READ_ONLY,
            Self::VaultStore(_) => SETTINGS_IO,
            Self::Codec(e) => e.wire_code(),
        }
    }
}

/// A stored settings document with metadata.
#[derive(Debug, Clone)]
pub struct StoredSettings {
    /// The parsed document.
    pub doc: SettingsDoc,
    /// Monotonic generation, starting at 1 for the first write.
    pub generation: u64,
    /// Warnings accumulated during parsing.
    pub warnings: Vec<String>,
    /// Whether the document is read-only (schema > current).
    pub read_only: bool,
}

impl StoredSettings {
    /// Check if this document is read-only due to schema version.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only || self.doc.is_read_only()
    }
}

/// Vault settings store.
///
/// Owns the settings document at `<vault>/.flavite/settings.toml` with
/// CAS generations and atomic writes.
#[derive(Debug)]
pub struct SettingsStore {
    vault_store: Arc<VaultUserStateStore>,
    allow_writes: bool,
}

impl SettingsStore {
    /// Create a new settings store for a vault.
    #[must_use]
    pub fn new(vault_store: Arc<VaultUserStateStore>, allow_writes: bool) -> Self {
        Self {
            vault_store,
            allow_writes,
        }
    }

    /// Read the settings document from the vault.
    ///
    /// Returns `None` if no settings document exists (caller should create defaults).
    /// Returns read-only documents when schema > current.
    ///
    /// # Errors
    /// Returns typed errors for parse failures, bounds, and filesystem issues.
    pub fn read(&self) -> Result<Option<StoredSettings>, SettingsStoreError> {
        let stored = self.vault_store.read(VaultUserStateKind::Settings)?;
        
        let Some(stored) = stored else {
            return Ok(None);
        };

        // Check size bound
        if stored.payload.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsStoreError::TooLarge {
                size: stored.payload.len(),
            });
        }

        // Parse the document
        let text = String::from_utf8(stored.payload.clone())
            .map_err(|e| SettingsStoreError::Unparseable(e.to_string()))?;
        
        let parsed = parse(Some(&text)).map_err(SettingsStoreError::Codec)?;

        Ok(Some(StoredSettings {
            doc: parsed.doc,
            generation: stored.generation,
            warnings: parsed.warnings,
            read_only: parsed.read_only,
        }))
    }

    /// Write the settings document to the vault under CAS.
    ///
    /// Returns the new generation on success.
    ///
    /// # Errors
    /// - `SettingsStoreError::ReadOnly` if the vault is read-only
    /// - `SettingsStoreError::SchemaNewer` if the document schema > current
    /// - `SettingsStoreError::Conflict` if the generation doesn't match
    /// - `SettingsStoreError::TooLarge` if the serialized document exceeds 256 KiB
    pub fn write(
        &self,
        doc: &SettingsDoc,
        expected_generation: u64,
    ) -> Result<u64, SettingsStoreError> {
        // Reject writes to newer schema documents
        if doc.is_read_only() {
            return Err(SettingsStoreError::SchemaNewer);
        }

        // Check size bound
        let text = serialize(doc).map_err(SettingsStoreError::Codec)?;
        if text.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsStoreError::TooLarge {
                size: text.len(),
            });
        }

        // Write through the vault store
        let next = self.vault_store.write(
            VaultUserStateKind::Settings,
            expected_generation,
            text.as_bytes(),
            self.allow_writes,
        )?;

        Ok(next)
    }

    /// Read or initialize settings with defaults.
    ///
    /// If no settings document exists, creates one from the registry defaults
    /// and writes it atomically. This is the entry point for vault open.
    ///
    /// # Errors
    /// Same as `read` and `write`, plus initialization failures.
    pub fn load_or_initialize(
        &self,
        registry: &SettingRegistry,
    ) -> Result<StoredSettings, SettingsStoreError> {
        match self.read()? {
            Some(stored) => Ok(stored),
            None => {
                // Create defaults document
                let doc = crate::settings_store::defaults_document(registry);
                let next = self.write(&doc, 0)?;
                Ok(StoredSettings {
                    doc,
                    generation: next,
                    warnings: Vec::new(),
                    read_only: false,
                })
            }
        }
    }

    /// Get a value by ID from the current document.
    ///
    /// This is a convenience method that combines read + lookup.
    /// Returns `None` if no document exists or the key doesn't exist.
    pub fn get_value(
        &self,
        registry: &SettingRegistry,
        id: &str,
    ) -> Result<Option<SettingValue>, SettingsStoreError> {
        let stored = self.load_or_initialize(registry)?;
        let resolved = registry.resolve(&stored.doc);
        Ok(resolved.get(id).cloned())
    }

    /// Set a value by ID and persist.
    ///
    /// This is a convenience method that:
    /// 1. Reads or initializes the document
    /// 2. Validates and sets the value
    /// 3. Writes with CAS
    ///
    /// # Errors
    /// Same as `load_or_initialize`, `write`, plus validation errors.
    pub fn set_value(
        &self,
        registry: &SettingRegistry,
        id: &str,
        value: SettingValue,
    ) -> Result<u64, SettingsStoreError> {
        let mut stored = self.load_or_initialize(registry)?;
        
        // Validate ID
        if SettingRegistry::is_reserved_namespace(id) {
            return Err(SettingsStoreError::Codec(SettingsCodecError::ReservedNamespace(id.to_owned())));
        }

        // Set the value
        stored.doc.set(id, value, registry)?;

        // Write with CAS
        self.write(&stored.doc, stored.generation)
    }

    /// Resolve the current document against the registry.
    ///
    /// This is a convenience method for getting all effective values.
    pub fn resolve(
        &self,
        registry: &SettingRegistry,
    ) -> Result<(Vec<SettingValue>, Vec<String>), SettingsStoreError> {
        let stored = self.load_or_initialize(registry)?;
        let resolved = registry.resolve(&stored.doc);
        Ok((resolved.all().values().cloned().collect(), resolved.warnings().to_vec()))
    }
}

/// Create a defaults document from the registry (RFC-0007 D-04 table).
#[must_use]
pub fn defaults_document(registry: &SettingRegistry) -> SettingsDoc {
    let mut doc = SettingsDoc::new(flavite_settings::descriptor::SCHEMA_VERSION);
    
    for descriptor in registry.all() {
        let parts: Vec<&str> = descriptor.id.split('.').collect();
        if parts.len() == 2 {
            let (section, key) = (parts[0], parts[1]);
            doc.sections
                .entry(section.to_owned())
                .or_default()
                .insert(key.to_owned(), descriptor.default.clone());
        }
    }
    
    doc
}

/// A resolved settings view with all effective values and metadata.
///
/// This is the result of `settings_read()` — a fully resolved view of all
/// settings with defaults filled in, suitable for UI consumption.
#[derive(Debug, Clone)]
pub struct ResolvedSettings {
    /// The resolved values.
    pub resolved: flavite_settings::resolve::Resolved,
    /// The document generation.
    pub generation: u64,
    /// Whether the document is read-only.
    pub read_only: bool,
    /// Any warnings from parsing.
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_fs::fs::Fs;
    use flavite_fs::MemFs;
    use flavite_settings::registry::SettingRegistry;
    use flavite_vault::user_state::VaultUserStateStore;
    use std::collections::BTreeMap;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn vault_store() -> (Arc<VaultUserStateStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("vault dir");
        let fs: Box<dyn Fs + Send + Sync> = Box::new(MemFs::new());
        let store = VaultUserStateStore::new(fs, dir.path());
        (Arc::new(store), dir)
    }

    fn settings_store() -> (SettingsStore, tempfile::TempDir) {
        let (vault_store, dir) = vault_store();
        let store = SettingsStore::new(vault_store, true);
        (store, dir)
    }

    #[test]
    fn read_nonexistent_returns_none() {
        let (store, _dir) = settings_store();
        let result = store.read().expect("read should succeed");
        assert!(result.is_none());
    }

    #[test]
    fn write_and_read_round_trip() {
        let (store, _dir) = settings_store();
        let registry = SettingRegistry::with_builtins();
        
        let doc = defaults_document(&registry);
        let next = store.write(&doc, 0).expect("write should succeed");
        assert_eq!(next, 1);
        
        let stored = store.read().expect("read should succeed").expect("should have doc");
        assert_eq!(stored.generation, 1);
        assert!(!stored.is_read_only());
    }

    #[test]
    fn cas_conflict() {
        let (store, _dir) = settings_store();
        let registry = SettingRegistry::with_builtins();
        
        let doc = defaults_document(&registry);
        store.write(&doc, 0).expect("first write");
        
        // Second write with stale generation should fail
        let result = store.write(&doc, 0);
        assert!(matches!(result, Err(SettingsStoreError::Conflict { expected: 0, actual: 1 })));
    }

    #[test]
    fn read_only_vault_rejects_writes() {
        let (vault_store, _dir) = vault_store();
        let store = SettingsStore::new(vault_store, false);
        let registry = SettingRegistry::with_builtins();
        
        let doc = defaults_document(&registry);
        let result = store.write(&doc, 0);
        assert!(matches!(result, Err(SettingsStoreError::ReadOnly)));
    }

    #[test]
    fn oversized_document_rejected() {
        let (store, _dir) = settings_store();
        let registry = SettingRegistry::with_builtins();
        
        let mut doc = defaults_document(&registry);
        // Add a huge unknown section to exceed the bound
        let huge_value = "x".repeat(MAX_SETTINGS_BYTES + 1);
        doc.unknown_tables.insert("huge".to_owned(), BTreeMap::from([("value".to_owned(), toml::Value::String(huge_value))]));
        
        let result = store.write(&doc, 0);
        assert!(matches!(result, Err(SettingsStoreError::TooLarge { .. })));
    }

    #[test]
    fn load_or_initialize_creates_defaults() {
        let (store, _dir) = settings_store();
        let registry = SettingRegistry::with_builtins();
        
        let stored = store.load_or_initialize(&registry).expect("load or init");
        assert_eq!(stored.generation, 1);
        assert!(!stored.is_read_only());
    }

    #[test]
    fn load_or_initialize_returns_existing() {
        let (store, _dir) = settings_store();
        let registry = SettingRegistry::with_builtins();
        
        // Write a document first
        let doc = defaults_document(&registry);
        store.write(&doc, 0).expect("write");
        
        // Load should return the existing document
        let stored = store.load_or_initialize(&registry).expect("load or init");
        assert_eq!(stored.generation, 1);
    }

    #[test]
    fn settings_paths_under_vault_root() {
        let dir = tempfile::tempdir().expect("vault dir");
        let fs: Box<dyn Fs + Send + Sync> = Box::new(MemFs::new());
        let vault_store = VaultUserStateStore::new(fs, dir.path());
        
        let path = vault_store.path_for(VaultUserStateKind::Settings);
        assert_eq!(
            path,
            PathBuf::from(dir.path()).join(".flavite").join("settings.toml")
        );
    }
}
