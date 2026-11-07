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
#![allow(clippy::collapsible_if)]

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
    Conflict { expected: u64, actual: u64 },
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
                write!(
                    f,
                    "settings CAS conflict: expected generation {expected}, got {actual}"
                )
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
            UserStateError::TooLarge { size } => Self::TooLarge { size },
            UserStateError::ReadOnly => Self::ReadOnly,
            UserStateError::Protected { path } => {
                Self::VaultStore(UserStateError::Protected { path })
            }
            UserStateError::Corrupt(msg) => Self::Unparseable(msg),
            UserStateError::NotJson(msg) => Self::Unparseable(msg),
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
            return Err(SettingsStoreError::TooLarge { size: text.len() });
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

    /// Load settings with migration from legacy stores (RFC-0007 S3/D-09).
    ///
    /// If `.flavite/settings.toml` doesn't exist in the vault:
    /// 1. Create the defaults document
    /// 2. Overlay the legacy v1 `.flavite/settings.toml` if it exists
    /// 3. Overlay the legacy `device-preferences.json` from the data dir if
    ///    it exists
    /// 4. Write the migrated v2 document atomically
    ///
    /// This is the entry point for vault open; it ensures exactly one
    /// document with idempotent migration. Runs at every open; re-runs are
    /// no-ops once the v2 document exists.
    ///
    /// # Arguments
    /// * `registry` - The setting registry
    /// * `vault_root` - The vault root path (for reading v1 settings.toml)
    /// * `data_dir` - The OS data directory (for reading device-preferences.json)
    /// * `vault_uuid` - The vault UUID (for finding device-preferences.json)
    ///
    /// # Errors
    /// Same as `read`, `write`, plus filesystem errors for legacy files.
    pub fn load_or_migrate(
        &self,
        registry: &SettingRegistry,
        vault_root: &std::path::Path,
        data_dir: Option<&std::path::Path>,
        vault_uuid: &str,
    ) -> Result<StoredSettings, SettingsStoreError> {
        // Try to read the v2 settings document
        match self.read()? {
            Some(stored) => Ok(stored),
            None => {
                // v2 document doesn't exist - migrate from legacy
                let mut doc = crate::settings_store::defaults_document(registry);
                let mut warnings = Vec::new();

                // Step 1: Overlay v1 settings.toml from vault
                // V1 format: flat TOML with vault-specific settings
                let v1_settings_path = vault_root.join(".flavite").join("settings.toml");
                #[allow(clippy::collapsible_if)]
                if v1_settings_path.exists() {
                    if let Ok(contents) = std::fs::read_to_string(&v1_settings_path) {
                        if let Ok(value) = toml::from_str::<toml::Value>(&contents) {
                            if let Some(table) = value.as_table() {
                                // Map v1 fields to v2 settings
                                self.overlay_v1_vault_settings(registry, &mut doc, table);
                            }
                        }
                        warnings.push(format!(
                            "Migrated from v1 settings at {}",
                            v1_settings_path.display()
                        ));
                    }
                }

                // Step 2: Overlay device-preferences.json from data dir
                // V1 format: JSON with user preferences
                if let Some(data_dir) = data_dir {
                    let device_prefs_path = data_dir
                        .join("flavite")
                        .join("workspace")
                        .join(vault_uuid)
                        .join("device-preferences.json");
                    #[allow(clippy::collapsible_if)]
                    if device_prefs_path.exists() {
                        if let Ok(contents) = std::fs::read_to_string(&device_prefs_path) {
                            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&contents)
                            {
                                if let Some(obj) = value.as_object() {
                                    // Map v1 device preferences to v2 settings
                                    self.overlay_v1_device_preferences(
                                        registry,
                                        &mut doc,
                                        &mut warnings,
                                        obj,
                                    );
                                }
                            }
                            warnings.push(format!(
                                "Migrated from device-preferences at {}",
                                device_prefs_path.display()
                            ));
                        }
                    }
                }

                // Write the migrated document
                let next = self.write(&doc, 0)?;
                Ok(StoredSettings {
                    doc,
                    generation: next,
                    warnings,
                    read_only: false,
                })
            }
        }
    }

    /// Overlay v1 vault settings (from .flavite/settings.toml) onto the v2 document.
    fn overlay_v1_vault_settings(
        &self,
        registry: &SettingRegistry,
        doc: &mut flavite_settings::document::SettingsDoc,
        table: &toml::value::Table,
    ) {
        use flavite_settings::value::SettingValue;

        // Map v1 fields to v2 setting IDs according to D-09
        // v1: rename_behavior -> v2: vault.rename_behavior
        if let Some(value) = table.get("rename_behavior") {
            if let Some(text) = value.as_str() {
                let setting_value = match text {
                    "rewrite" => SettingValue::Str("rewrite".to_owned()),
                    "alias" => SettingValue::Str("alias".to_owned()),
                    "none" => SettingValue::Str("none".to_owned()),
                    _ => SettingValue::Str("rewrite".to_owned()),
                };
                let _ = doc.set("vault.rename_behavior", setting_value, registry);
            }
        }

        // v1: recovery_interval_secs -> v2: vault.recovery_interval_secs
        if let Some(value) = table.get("recovery_interval_secs") {
            if let Some(int_val) = value.as_integer() {
                let _ = doc.set(
                    "vault.recovery_interval_secs",
                    SettingValue::Int(int_val),
                    registry,
                );
            }
        }

        // v1: verification_interval_mins -> v2: vault.verification_interval_mins
        if let Some(value) = table.get("verification_interval_mins") {
            if let Some(int_val) = value.as_integer() {
                let _ = doc.set(
                    "vault.verification_interval_mins",
                    SettingValue::Int(int_val),
                    registry,
                );
            }
        }

        // v1: change_log_budget_mib -> v2: vault.change_log_budget_mib
        if let Some(value) = table.get("change_log_budget_mib") {
            if let Some(int_val) = value.as_integer() {
                let _ = doc.set(
                    "vault.change_log_budget_mib",
                    SettingValue::Int(int_val),
                    registry,
                );
            }
        }

        // v1: ignore_globs -> v2: vault.ignore_globs
        if let Some(value) = table.get("ignore_globs") {
            if let Some(array) = value.as_array() {
                let list: Vec<String> = array
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_owned()))
                    .collect();
                let _ = doc.set("vault.ignore_globs", SettingValue::List(list), registry);
            }
        }
    }

    /// Overlay v1 device preferences (from device-preferences.json) onto the v2 document.
    fn overlay_v1_device_preferences(
        &self,
        registry: &SettingRegistry,
        doc: &mut flavite_settings::document::SettingsDoc,
        warnings: &mut Vec<String>,
        obj: &serde_json::Map<String, serde_json::Value>,
    ) {
        use flavite_settings::value::SettingValue;

        // Map v1 device preferences fields to v2 setting IDs according to D-09

        // theme -> appearance.theme
        if let Some(value) = obj.get("theme") {
            if let Some(text) = value.as_str() {
                let theme = match text {
                    "system" => "system",
                    "light" => "light",
                    "dark" => "dark",
                    _ => "system",
                };
                let _ = doc.set(
                    "appearance.theme",
                    SettingValue::Str(theme.to_owned()),
                    registry,
                );
            }
        }

        // theme_package -> appearance.theme_package
        if let Some(value) = obj.get("theme_package") {
            if let Some(text) = value.as_str() {
                let _ = doc.set(
                    "appearance.theme_package",
                    SettingValue::Str(text.to_owned()),
                    registry,
                );
            } else {
                let _ = doc.set(
                    "appearance.theme_package",
                    SettingValue::Str("".to_owned()),
                    registry,
                );
            }
        }

        // interface_scale_pct -> appearance.scale_pct
        if let Some(value) = obj.get("interface_scale_pct") {
            if let Some(int_val) = value.as_u64() {
                let _ = doc.set(
                    "appearance.scale_pct",
                    SettingValue::Int(int_val as i64),
                    registry,
                );
            }
        }

        // reduced_motion -> appearance.reduced_motion
        if let Some(value) = obj.get("reduced_motion") {
            if let Some(bool_val) = value.as_bool() {
                let _ = doc.set(
                    "appearance.reduced_motion",
                    SettingValue::Bool(bool_val),
                    registry,
                );
            }
        }

        // editor_font_size -> editor.font_size
        if let Some(value) = obj.get("editor_font_size") {
            if let Some(int_val) = value.as_u64() {
                let _ = doc.set(
                    "editor.font_size",
                    SettingValue::Int(int_val as i64),
                    registry,
                );
            }
        }

        // line_width -> editor.line_width
        if let Some(value) = obj.get("line_width") {
            if let Some(obj_map) = value.as_object() {
                if let Some(columns) = obj_map.get("columns").and_then(|v| v.as_u64()) {
                    let _ = doc.set(
                        "editor.line_width",
                        SettingValue::Int(columns as i64),
                        registry,
                    );
                } else if let Some(text) = obj_map.get("type").and_then(|v| v.as_str()) {
                    if text == "full" {
                        let _ = doc.set(
                            "editor.line_width",
                            SettingValue::Str("full".to_owned()),
                            registry,
                        );
                    }
                }
            }
        }

        // prefer_stable_links -> links.prefer_stable_links
        if let Some(value) = obj.get("prefer_stable_links") {
            if let Some(bool_val) = value.as_bool() {
                let _ = doc.set(
                    "links.prefer_stable_links",
                    SettingValue::Bool(bool_val),
                    registry,
                );
            }
        }

        // sidebar_open -> sidebar.open
        if let Some(value) = obj.get("sidebar_open") {
            if let Some(bool_val) = value.as_bool() {
                let _ = doc.set("sidebar.open", SettingValue::Bool(bool_val), registry);
            }
        }

        // sidebar_width -> sidebar.width
        if let Some(value) = obj.get("sidebar_width") {
            if let Some(int_val) = value.as_u64() {
                let _ = doc.set("sidebar.width", SettingValue::Int(int_val as i64), registry);
            }
        }

        // graph_activation_acknowledged -> state.graph_activation_acknowledged
        if let Some(value) = obj.get("graph_activation_acknowledged") {
            if let Some(bool_val) = value.as_bool() {
                let _ = doc.set(
                    "state.graph_activation_acknowledged",
                    SettingValue::Bool(bool_val),
                    registry,
                );
            }
        }

        // shortcuts -> [shortcuts]
        if let Some(value) = obj.get("shortcuts") {
            if let Some(shortcuts_obj) = value.as_object() {
                let mut shortcuts_map = std::collections::BTreeMap::new();
                for (command_id, chord_str) in shortcuts_obj {
                    if let Some(chord_text) = chord_str.as_str() {
                        // Validate the chord using flavite-settings
                        if flavite_settings::chord::parse_chord(chord_text).is_some() {
                            shortcuts_map.insert(command_id.clone(), chord_text.to_owned());
                        } else {
                            warnings
                                .push(format!("Invalid chord for {}: {}", command_id, chord_text));
                        }
                    }
                }
                let _ = doc.set(
                    "shortcuts",
                    SettingValue::ShortcutMap(shortcuts_map),
                    registry,
                );
            }
        }
    }

    /// The last-good backup document, when one exists (RFC-0007 FR-10).
    ///
    /// The vault store keeps the pre-write bytes at `<name>.bak`; when the
    /// live document is torn or unparseable, this is the one-click restore
    /// source. The store never restores silently — the caller decides.
    ///
    /// # Errors
    /// Same as `read`, plus codec failures if the backup itself is broken.
    pub fn backup(&self) -> Result<Option<StoredSettings>, SettingsStoreError> {
        let Some(bytes) = self.vault_store.read_backup(VaultUserStateKind::Settings)? else {
            return Ok(None);
        };
        if bytes.len() > MAX_SETTINGS_BYTES {
            return Err(SettingsStoreError::TooLarge { size: bytes.len() });
        }
        let text =
            String::from_utf8(bytes).map_err(|e| SettingsStoreError::Unparseable(e.to_string()))?;
        let parsed = parse(Some(&text)).map_err(SettingsStoreError::Codec)?;
        let generation = self
            .vault_store
            .read(VaultUserStateKind::Settings)?
            .map_or(0, |stored| stored.generation);
        Ok(Some(StoredSettings {
            doc: parsed.doc,
            generation,
            warnings: parsed.warnings,
            read_only: parsed.read_only,
        }))
    }

    /// Restore the last-good backup over the current document under the
    /// store's current generation receipt (RFC-0007 FR-10). Returns the
    /// new generation.
    ///
    /// # Errors
    /// `Unparseable("no backup exists")` when there is nothing to restore;
    /// otherwise the same as `backup` and `write`.
    pub fn restore_backup(&self) -> Result<u64, SettingsStoreError> {
        let Some(stored) = self.backup()? else {
            return Err(SettingsStoreError::Unparseable("no backup exists".into()));
        };
        let generation = self
            .vault_store
            .read(VaultUserStateKind::Settings)?
            .map_or(0, |raw| raw.generation);
        self.write(&stored.doc, generation)
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
            return Err(SettingsStoreError::Codec(
                SettingsCodecError::ReservedNamespace(id.to_owned()),
            ));
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
        Ok((
            resolved.all().values().cloned().collect(),
            resolved.warnings().to_vec(),
        ))
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
    use flavite_fs::MemFs;
    use flavite_fs::fs::Fs;
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

        let stored = store
            .read()
            .expect("read should succeed")
            .expect("should have doc");
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
        assert!(matches!(
            result,
            Err(SettingsStoreError::Conflict {
                expected: 0,
                actual: 1
            })
        ));
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
        doc.unknown_tables.insert(
            "huge".to_owned(),
            BTreeMap::from([("value".to_owned(), toml::Value::String(huge_value))]),
        );

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
            PathBuf::from(dir.path())
                .join(".flavite")
                .join("settings.toml")
        );
    }
}
