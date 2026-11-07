//! Error types for the settings codec and registry.

use thiserror::Error;

/// Wire-compatible error codes for settings operations.
pub const SETTINGS_UNKNOWN_KEY: &str = "settings_unknown_key";
pub const SETTINGS_READ_ONLY: &str = "settings_read_only";
pub const SETTINGS_CONFLICT: &str = "settings_conflict";
pub const SETTINGS_TOO_LARGE: &str = "settings_too_large";
pub const SETTINGS_UNPARSEABLE: &str = "settings_unparseable";
pub const SETTINGS_SCHEMA_NEWER: &str = "settings_schema_newer";
pub const SETTINGS_INVALID_VALUE: &str = "settings_invalid_value";
pub const SETTINGS_CAPABILITY_DENIED: &str = "settings_capability_denied";
pub const SETTINGS_IO: &str = "settings_io";

/// Errors that can occur during settings parsing, validation, or serialization.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SettingsCodecError {
    /// The document contains an unknown setting key that cannot be resolved.
    #[error("unknown setting key: {0}")]
    UnknownKey(String),

    /// The setting value is invalid (out of bounds, wrong type, etc.).
    #[error("invalid value for {0}: {1}")]
    InvalidValue(String, String),

    /// The document is too large (> 256 KiB).
    #[error("settings document exceeds maximum size")]
    DocumentTooLarge,

    /// The document is unparseable (invalid TOML syntax).
    #[error("settings document is unparseable: {0}")]
    Unparseable(String),

    /// The schema version is newer than what this code can write.
    #[error("settings schema version is newer")]
    SchemaNewer,

    /// Attempt to write to a reserved namespace (e.g., plugins.*)
    #[error("reserved namespace: {0}")]
    ReservedNamespace(String),

    /// The chord string is invalid.
    #[error("invalid chord string: {0}")]
    InvalidChord(String),

    /// The shortcut map contains a collision.
    #[error("shortcut collision: {0} is used by both {1} and {2}")]
    ShortcutCollision(String, String, String),

    /// The shortcut is reserved by the OS.
    #[error("shortcut {0} is reserved by {1}")]
    OsReservedChord(String, &'static str),
}

impl SettingsCodecError {
    /// Returns the wire-compatible error code for this error.
    #[must_use]
    pub fn wire_code(&self) -> &'static str {
        match self {
            Self::UnknownKey(_) | Self::ReservedNamespace(_) => SETTINGS_UNKNOWN_KEY,
            Self::InvalidValue(_, _)
            | Self::InvalidChord(_)
            | Self::ShortcutCollision(_, _, _)
            | Self::OsReservedChord(_, _) => SETTINGS_INVALID_VALUE,
            Self::DocumentTooLarge => SETTINGS_TOO_LARGE,
            Self::Unparseable(_) => SETTINGS_UNPARSEABLE,
            Self::SchemaNewer => SETTINGS_SCHEMA_NEWER,
        }
    }
}
