//! Installed theme packages: vault-local storage and the typed command
//! payloads.
//!
//! Themes are vault state: a converted package is stored under the vault
//! directory (`<vault>/.flavite/themes/<id>/`), one directory per theme
//! holding the converted `.flavite.xpi` plus a small metadata document.
//! The store follows the user-state pattern — injected [`Fs`], atomic
//! writes, symlink guards, hard size bounds — and the pure conversion
//! lives in flavite-theme, so this module only owns locations and durability.

use crate::FlaviteError;
use flavite_fs::{Fs, atomic_write};
use flavite_theme::{ConvertedTheme, ThemeError};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Hard byte bound for one stored theme package (the conversion's own
/// package bound, enforced again at the storage boundary).
pub const MAX_THEME_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
/// Hard byte bound for one metadata document.
pub const MAX_THEME_META_BYTES: u64 = 64 * 1024;

/// The fixed entry names inside one theme directory. Callers cannot
/// invent locations; the file names are part of the kind.
pub const PACKAGE_ENTRY: &str = "theme.flavite.xpi";
pub const META_ENTRY: &str = "theme.json";

/// Why a theme storage operation failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThemeStoreError {
    /// No device data directory is available.
    Unavailable(String),
    /// The package or metadata exceeds the bounded size.
    TooLarge { size: usize },
    /// The target path is a symlink or otherwise protected.
    Protected { path: String },
    /// The package failed to convert.
    InvalidPackage(ThemeError),
    /// The named theme is not installed.
    NotFound { id: String },
    /// The stored metadata is unreadable.
    Corrupt(String),
    /// The underlying filesystem failed.
    Fs(String),
    /// The vault is read-only, theme writes are not allowed.
    ReadOnly,
    /// The source package at a host-authorized path could not be read.
    SourceRead(String),
}

impl std::fmt::Display for ThemeStoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(message) => write!(f, "theme store unavailable: {message}"),
            Self::TooLarge { size } => write!(
                f,
                "theme package is {size} bytes, over the {MAX_THEME_PACKAGE_BYTES} byte bound"
            ),
            Self::Protected { path } => write!(f, "theme path is protected: {path}"),
            Self::InvalidPackage(error) => write!(f, "theme package is invalid: {error}"),
            Self::NotFound { id } => write!(f, "theme {id:?} is not installed"),
            Self::Corrupt(message) => write!(f, "theme metadata is corrupt: {message}"),
            Self::Fs(message) => write!(f, "theme filesystem error: {message}"),
            Self::SourceRead(message) => write!(f, "theme source unreadable: {message}"),
            Self::ReadOnly => write!(f, "theme storage is read-only"),
        }
    }
}

impl std::error::Error for ThemeStoreError {}

impl From<ThemeStoreError> for FlaviteError {
    fn from(error: ThemeStoreError) -> Self {
        FlaviteError::ThemeStore(error.to_string())
    }
}

/// One installed theme's metadata document (`theme.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledTheme {
    /// The resolved theme identity.
    pub id: String,
    /// The theme display name.
    pub name: String,
    /// The theme-package format version.
    pub format_version: u32,
    /// The `theme_frame` is a multi-frame APNG.
    pub animated_frame: bool,
    /// A `dark_theme` variant was present.
    pub supports_dark_variant: bool,
}

/// A loaded theme: the converted package plus its decoded artifacts.
#[derive(Debug, Clone)]
pub struct LoadedTheme {
    /// The metadata document.
    pub installed: InstalledTheme,
    /// The full conversion result (semantic mapping, report, images).
    pub converted: ConvertedTheme,
}

/// The device-local store of installed theme packages.
pub struct ThemeStore {
    fs: Box<dyn Fs + Send + Sync>,
    root: PathBuf,
}

impl std::fmt::Debug for ThemeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThemeStore").finish_non_exhaustive()
    }
}

impl ThemeStore {
    /// Build the store at an explicit vault root (`.flavite/themes`).
    #[must_use]
    pub fn at_vault(fs: Box<dyn Fs + Send + Sync>, vault_root: &Path) -> Self {
        let mut root = PathBuf::from(vault_root);
        root.push(".flavite");
        root.push("themes");
        Self { fs, root }
    }

    /// Build the store at an explicit root (tests and fault injection).
    #[must_use]
    pub fn at_root(fs: Box<dyn Fs + Send + Sync>, root: PathBuf) -> Self {
        Self { fs, root }
    }

    /// Convert and install a standard theme package. Re-installing a
    /// package with the same identity replaces its files atomically.
    ///
    /// # Errors
    /// Typed failures for invalid packages, bounds, protection, and
    /// filesystem errors.
    pub fn install(&self, package: &[u8]) -> Result<InstalledTheme, ThemeStoreError> {
        if package.len() as u64 > MAX_THEME_PACKAGE_BYTES {
            return Err(ThemeStoreError::TooLarge {
                size: package.len(),
            });
        }
        let converted =
            flavite_theme::convert_xpi(package).map_err(ThemeStoreError::InvalidPackage)?;
        let mut package_bytes = std::io::Cursor::new(Vec::new());
        flavite_theme::convert::write_package(&converted.files, &mut package_bytes)
            .map_err(ThemeStoreError::InvalidPackage)?;
        let package_bytes = package_bytes.into_inner();
        let installed = InstalledTheme {
            id: converted.id.clone(),
            name: converted.report.name.clone(),
            format_version: converted.report.format_version,
            animated_frame: converted.report.animated_frame,
            supports_dark_variant: converted.report.supports_dark_variant,
        };
        let metadata = serde_json::to_vec_pretty(&installed)
            .map_err(|error| ThemeStoreError::Corrupt(error.to_string()))?;
        let directory = self.directory_for(&installed.id);
        self.guard_protected(&directory.join(PACKAGE_ENTRY))?;
        self.guard_protected(&directory.join(META_ENTRY))?;
        self.fs
            .create_dir_all(&directory)
            .map_err(|error| ThemeStoreError::Fs(error.to_string()))?;
        atomic_write(
            self.fs.as_ref(),
            &directory.join(PACKAGE_ENTRY),
            &package_bytes,
        )
        .map_err(|error| ThemeStoreError::Fs(error.to_string()))?;
        atomic_write(self.fs.as_ref(), &directory.join(META_ENTRY), &metadata)
            .map_err(|error| ThemeStoreError::Fs(error.to_string()))?;
        Ok(installed)
    }

    /// The installed themes, sorted by id. A directory without readable
    /// metadata is skipped (an interrupted install leaves nothing to list).
    ///
    /// # Errors
    /// Filesystem failures.
    pub fn list(&self) -> Result<Vec<InstalledTheme>, ThemeStoreError> {
        let mut themes = Vec::new();
        // A store root that was never created holds no themes; the empty
        // list is the honest answer, not a filesystem failure.
        if !self
            .fs
            .exists(&self.root)
            .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
        {
            return Ok(themes);
        }
        for entry in self
            .fs
            .read_dir(&self.root)
            .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
        {
            if !entry.is_dir {
                continue;
            }
            let metadata_path = entry.path.join(META_ENTRY);
            let Some(bytes) = self
                .fs
                .read_if_exists(&metadata_path)
                .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
            else {
                continue;
            };
            if bytes.len() as u64 > MAX_THEME_META_BYTES {
                return Err(ThemeStoreError::TooLarge { size: bytes.len() });
            }
            let installed: InstalledTheme = serde_json::from_slice(&bytes)
                .map_err(|error| ThemeStoreError::Corrupt(error.to_string()))?;
            themes.push(installed);
        }
        themes.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(themes)
    }

    /// Load one installed theme with its full conversion (semantic
    /// mapping, report, decoded images).
    ///
    /// # Errors
    /// [`ThemeStoreError::NotFound`] when the id is not installed;
    /// conversion and filesystem failures otherwise.
    pub fn load(&self, id: &str) -> Result<LoadedTheme, ThemeStoreError> {
        let installed = self
            .list()?
            .into_iter()
            .find(|theme| theme.id == id)
            .ok_or_else(|| ThemeStoreError::NotFound { id: id.to_owned() })?;
        let directory = self.directory_for(id);
        let bytes = self
            .fs
            .read_if_exists(&directory.join(PACKAGE_ENTRY))
            .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
            .ok_or_else(|| ThemeStoreError::NotFound { id: id.to_owned() })?;
        if bytes.len() as u64 > MAX_THEME_PACKAGE_BYTES {
            return Err(ThemeStoreError::TooLarge { size: bytes.len() });
        }
        let converted =
            flavite_theme::convert_xpi(&bytes).map_err(ThemeStoreError::InvalidPackage)?;
        Ok(LoadedTheme {
            installed,
            converted,
        })
    }

    /// Remove one installed theme's files. An empty theme directory may
    /// remain behind; it is invisible to [`ThemeStore::list`].
    ///
    /// # Errors
    /// [`ThemeStoreError::NotFound`] when the id is not installed;
    /// protection and filesystem failures otherwise.
    pub fn uninstall(&self, id: &str) -> Result<(), ThemeStoreError> {
        if !self.list()?.iter().any(|theme| theme.id == id) {
            return Err(ThemeStoreError::NotFound { id: id.to_owned() });
        }
        let directory = self.directory_for(id);
        for entry in [META_ENTRY, PACKAGE_ENTRY] {
            let path = directory.join(entry);
            self.guard_protected(&path)?;
            if self
                .fs
                .exists(&path)
                .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
            {
                self.fs
                    .remove(&path)
                    .map_err(|error| ThemeStoreError::Fs(error.to_string()))?;
            }
        }
        Ok(())
    }

    /// The directory for one theme id: a sanitized, collision-resistant
    /// stem so hostile identities (`../x`, empty, 10 KiB long) can never
    /// escape the store.
    fn directory_for(&self, id: &str) -> PathBuf {
        self.root.join(sanitize_id(id))
    }

    fn guard_protected(&self, path: &Path) -> Result<(), ThemeStoreError> {
        if let Ok(metadata) = self.fs.metadata(path)
            && metadata.is_symlink
        {
            return Err(ThemeStoreError::Protected {
                path: path.display().to_string(),
            });
        }
        Ok(())
    }

    /// Migrate themes from the legacy device store to the vault store.
    /// Copies theme directories from `<data>/flavite/themes/` to `<vault>/.flavite/themes/`,
    /// skipping ids already present. Runs once per vault, idempotent.
    ///
    /// # Errors
    /// Filesystem failures during copy operations.
    pub fn migrate_from_device(&self, device_themes_root: &Path) -> Result<usize, ThemeStoreError> {
        // Ensure vault themes directory exists
        self.fs
            .create_dir_all(&self.root)
            .map_err(|error| ThemeStoreError::Fs(error.to_string()))?;

        // Check if device themes directory exists
        if !self
            .fs
            .exists(device_themes_root)
            .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
        {
            return Ok(0);
        }

        let mut copied = 0;
        for entry in self
            .fs
            .read_dir(device_themes_root)
            .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
        {
            if !entry.is_dir {
                continue;
            }
            // Extract theme id from directory name (reverse the sanitization is not needed
            // since we're just copying the directory as-is)
            let theme_dir_name = entry
                .path
                .file_name()
                .ok_or_else(|| ThemeStoreError::Fs("invalid theme directory name".into()))?;
            let theme_dir_name = theme_dir_name
                .to_str()
                .ok_or_else(|| ThemeStoreError::Fs("non-utf8 theme directory name".into()))?;

            let device_theme_dir = device_themes_root.join(theme_dir_name);
            let vault_theme_dir = self.root.join(theme_dir_name);

            // Skip if already exists in vault
            if self
                .fs
                .exists(&vault_theme_dir)
                .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
            {
                continue;
            }

            // Copy the theme directory (package and metadata)
            for file_entry in [PACKAGE_ENTRY, META_ENTRY] {
                let src = device_theme_dir.join(file_entry);
                let dst = vault_theme_dir.join(file_entry);
                if self
                    .fs
                    .exists(&src)
                    .map_err(|e| ThemeStoreError::Fs(e.to_string()))?
                {
                    self.fs
                        .create_dir_all(&vault_theme_dir)
                        .map_err(|error| ThemeStoreError::Fs(error.to_string()))?;
                    let bytes = self
                        .fs
                        .read_if_exists(&src)
                        .map_err(|error| ThemeStoreError::Fs(error.to_string()))?
                        .ok_or_else(|| {
                            ThemeStoreError::Fs(format!("cannot read {}", src.display()))
                        })?;
                    atomic_write(self.fs.as_ref(), &dst, &bytes)
                        .map_err(|error| ThemeStoreError::Fs(error.to_string()))?;
                }
            }
            copied += 1;
        }
        Ok(copied)
    }
}

/// Filesystem-safe directory stem for a theme id: case-folded, restricted
/// to `[a-z0-9_-]` (dots fold away — no `..` can ever survive), bounded in
/// length, and suffixed with a digest of the full id so distinct
/// identities can never collide on one directory.
#[must_use]
pub fn sanitize_id(id: &str) -> String {
    let folded: String = id
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = folded.trim_matches('-');
    let stem: String = if trimmed.is_empty() {
        "theme".to_owned()
    } else {
        trimmed.chars().take(48).collect()
    };
    let digest: String = blake3::hash(id.as_bytes()).as_bytes()[..4]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{stem}-{digest}")
}

// ---------------------------------------------------------------------------
// Typed command payloads (§4.5)
// ---------------------------------------------------------------------------

/// Arguments of `theme.install`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallThemeArgs {
    /// Path of the standard `.xpi` package to convert and install.
    pub path: String,
}

/// Arguments of `theme.list` (there are none; the schema is an empty
/// object so `deny_unknown_fields` still rejects stray arguments).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListThemesArgs {}

/// Arguments of `theme.uninstall`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UninstallThemeArgs {
    /// The theme identity to remove.
    pub id: String,
}

/// Registry placeholder: theme commands are routed to
/// [`execute_theme_command`] on the facade turn (they need the
/// facade-owned store, which the `&Session` handler signature cannot
/// reach). The registry still owns their schemas and capabilities.
pub(crate) fn handle_theme_unroutable(
    _session: &flavite_vault::session::Session,
    _invocation: &crate::command::Invocation,
) -> Result<flavite_vault::session::SessionResult, String> {
    Err("theme commands are routed on the facade turn".into())
}

/// The facade-turn handler for `theme.*` commands. Runs after the
/// registry's argument validation and the capability check, before any
/// buffer precondition work (themes never touch buffers).
pub(crate) fn execute_theme_command(
    flavite: &crate::Flavite,
    invocation: &crate::command::Invocation,
) -> crate::result::ExecutionResult {
    use crate::result::{
        ExecutionResult, LocalError, LocalResult, LocalSuccess, RejectedResult, RejectionReason,
    };
    let Some(store) = flavite.themes.as_ref() else {
        return ExecutionResult::Rejected(
            RejectedResult::with_request(RejectionReason::Unavailable, invocation.request_id)
                .with_message("theme storage is unavailable"),
        );
    };
    let outcome = match invocation.command_id {
        "theme.install" => {
            if flavite.is_read_only() {
                return ExecutionResult::Local(LocalResult::Error(LocalError::with_request(
                    "theme_read_only",
                    "theme storage is read-only",
                    invocation.request_id,
                )));
            }
            let Ok(args) = serde_json::from_value::<InstallThemeArgs>(invocation.args.clone())
            else {
                return ExecutionResult::Rejected(
                    RejectedResult::with_request(
                        RejectionReason::InvalidArgs,
                        invocation.request_id,
                    )
                    .with_message("theme.install arguments are unreadable"),
                );
            };
            install_from_path(store, &args.path).map(|installed| {
                (
                    invocation.command_id,
                    serde_json::json!({
                        "theme": installed_to_value(&installed),
                    }),
                )
            })
        }
        "theme.list" => store.list().map(|themes| {
            (
                invocation.command_id,
                serde_json::json!({
                    "themes": themes
                        .iter()
                        .map(installed_to_value)
                        .collect::<Vec<_>>(),
                }),
            )
        }),
        "theme.uninstall" => {
            if flavite.is_read_only() {
                return ExecutionResult::Local(LocalResult::Error(LocalError::with_request(
                    "theme_read_only",
                    "theme storage is read-only",
                    invocation.request_id,
                )));
            }
            let Ok(args) = serde_json::from_value::<UninstallThemeArgs>(invocation.args.clone())
            else {
                return ExecutionResult::Rejected(
                    RejectedResult::with_request(
                        RejectionReason::InvalidArgs,
                        invocation.request_id,
                    )
                    .with_message("theme.uninstall arguments are unreadable"),
                );
            };
            store
                .uninstall(&args.id)
                .map(|_| (invocation.command_id, serde_json::json!({})))
        }
        other => {
            return ExecutionResult::Rejected(
                RejectedResult::with_request(RejectionReason::Unavailable, invocation.request_id)
                    .with_message(format!("command unavailable: {other}")),
            );
        }
    };
    match outcome {
        Ok((command_id, data)) => ExecutionResult::Local(LocalResult::Success(
            LocalSuccess::with_command(invocation.request_id, command_id, data),
        )),
        Err(error) => ExecutionResult::Local(LocalResult::Error(LocalError::with_request(
            theme_error_code(&error),
            error.to_string(),
            invocation.request_id,
        ))),
    }
}

/// Import a package from a host-authorized path. The source read is an
/// explicit user action (a picked or named file), so it uses `std::fs`
/// directly — the injected `Fs` remains the authority over the store's
/// own managed root. The size bound is checked from the metadata before
/// any bytes are read.
fn install_from_path(store: &ThemeStore, path: &str) -> Result<InstalledTheme, ThemeStoreError> {
    let path = std::path::Path::new(path);
    let metadata = std::fs::metadata(path).map_err(|error| {
        ThemeStoreError::SourceRead(format!("theme package stat failed: {error}"))
    })?;
    if metadata.len() > MAX_THEME_PACKAGE_BYTES {
        return Err(ThemeStoreError::TooLarge {
            size: metadata.len() as usize,
        });
    }
    let package = std::fs::read(path).map_err(|error| {
        ThemeStoreError::SourceRead(format!("theme package read failed: {error}"))
    })?;
    store.install(&package)
}

/// Serialize one [`InstalledTheme`] for command results.
#[must_use]
pub fn installed_to_value(installed: &InstalledTheme) -> Value {
    serde_json::to_value(installed).unwrap_or(Value::Null)
}

/// Encode a [`ThemeStoreError`] as a typed local error code + message.
pub(crate) fn theme_error_code(error: &ThemeStoreError) -> &'static str {
    match error {
        ThemeStoreError::Unavailable(_) | ThemeStoreError::Fs(_) => "theme_store_error",
        ThemeStoreError::SourceRead(_) => "theme_source_unreadable",
        ThemeStoreError::TooLarge { .. } => "theme_too_large",
        ThemeStoreError::Protected { .. } => "theme_protected",
        ThemeStoreError::InvalidPackage(_) => "theme_invalid_package",
        ThemeStoreError::NotFound { .. } => "theme_not_found",
        ThemeStoreError::Corrupt(_) => "theme_corrupt",
        ThemeStoreError::ReadOnly => "theme_read_only",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_fs::MemFs;
    use std::io::{Cursor, Write};

    const MANIFEST: &str = r##"{
        "manifest_version": 2,
        "name": "Store Fixture",
        "browser_specific_settings": { "gecko": { "id": "fixture@host" } },
        "theme": { "colors": { "frame": "#101010", "tab_background_text": "#eeeeee" } }
    }"##;

    fn fixture_package() -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut zip = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default();
            zip.start_file("manifest.json", options).expect("entry");
            zip.write_all(MANIFEST.as_bytes()).expect("bytes");
            zip.finish().expect("finish");
        }
        buffer.into_inner()
    }

    fn store() -> (ThemeStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("data dir");
        let fs: Box<dyn Fs + Send + Sync> = Box::new(MemFs::new());
        (ThemeStore::at_root(fs, dir.path().to_path_buf()), dir)
    }

    #[test]
    fn install_list_load_and_uninstall_round_trip() {
        let (store, _dir) = store();
        let installed = store.install(&fixture_package()).expect("install");
        assert_eq!(installed.id, "fixture@host");
        assert_eq!(installed.name, "Store Fixture");
        assert_eq!(store.list().expect("list").len(), 1);

        let loaded = store.load("fixture@host").expect("load");
        assert_eq!(loaded.installed.id, "fixture@host");
        assert_eq!(
            loaded.converted.light.semantic.top_bar_background,
            Some(flavite_theme::Rgba::rgb(0x10, 0x10, 0x10))
        );

        store.uninstall("fixture@host").expect("uninstall");
        assert!(store.list().expect("list").is_empty());
        assert!(matches!(
            store.uninstall("fixture@host"),
            Err(ThemeStoreError::NotFound { .. })
        ));
        assert!(matches!(
            store.load("fixture@host"),
            Err(ThemeStoreError::NotFound { .. })
        ));
    }

    #[test]
    fn reinstalling_the_same_identity_replaces_in_place() {
        let (store, _dir) = store();
        store.install(&fixture_package()).expect("first");
        let second = store.install(&fixture_package()).expect("second");
        assert_eq!(second.id, "fixture@host");
        assert_eq!(store.list().expect("list").len(), 1);
    }

    #[test]
    fn invalid_packages_are_typed_failures() {
        let (store, _dir) = store();
        assert!(matches!(
            store.install(b"not a zip"),
            Err(ThemeStoreError::InvalidPackage(ThemeError::NotZip))
        ));
    }

    #[test]
    fn ids_are_sanitized_into_safe_collision_free_directories() {
        assert_eq!(sanitize_id("../evil"), sanitize_id("../evil"));
        assert_ne!(sanitize_id("a@b"), sanitize_id("a-b"));
        for hostile in ["", "../../etc", "x/../../y", &"a".repeat(999)] {
            let stem = sanitize_id(hostile);
            assert!(!stem.contains('/'));
            assert!(!stem.contains(".."));
            assert!(stem.len() <= 64);
            assert!(!stem.is_empty());
        }
        // The digest suffix makes the stem stable and collision-free.
        let stem = sanitize_id("fixture@host");
        assert!(stem.starts_with("fixture-host-"));
        assert!(stem.ends_with(|c: char| c.is_ascii_hexdigit()));
    }
}
