//! Setting descriptors that define the schema for all user-configurable settings.

use std::collections::BTreeMap;

use crate::error::SettingsCodecError;
use crate::value::SettingValue;

/// The schema version for the settings document.
pub const SCHEMA_VERSION: u32 = 2;

/// Maximum document size in bytes (256 KiB).
pub const MAX_DOCUMENT_SIZE: usize = 256 * 1024;

/// Visibility of a setting in the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// This setting is visible and editable in the UI.
    Visible,
    /// This setting is hidden from the UI (used for internal state).
    Hidden,
}

/// When a setting change takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Apply {
    /// Changes take effect immediately (live).
    Live,
    /// Changes require a restart to take effect.
    OnRestart,
}

/// The kind of a setting, determining its validation and UI representation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingKind {
    /// A boolean setting.
    Bool,
    /// An integer setting with bounds.
    Int { min: i64, max: i64 },
    /// A text string setting.
    Text,
    /// A choice from a fixed set of options.
    Choice(&'static [(&'static str, &'static str)]), // (value, label)
    /// Line width: bounded integer or "full".
    LineWidth,
    /// Theme package selection (dynamic options from ThemeStore).
    ThemePackage,
    /// A list of strings.
    StringList,
    /// Shortcut overrides: command-id -> chord-string map.
    ShortcutMap,
}

impl SettingKind {
    /// Validate a value against this kind.
    /// Returns Ok(()) if valid, or Err with a description of the validation failure.
    pub fn validate(&self, value: &SettingValue) -> Result<(), String> {
        match self {
            Self::Bool => {
                if value.is_bool() {
                    Ok(())
                } else {
                    Err("expected boolean".to_owned())
                }
            }
            Self::Int { min, max } => {
                if let Some(v) = value.as_int() {
                    if v >= *min && v <= *max {
                        Ok(())
                    } else {
                        Err(format!("value must be between {min} and {max}"))
                    }
                } else {
                    Err("expected integer".to_owned())
                }
            }
            Self::Text => {
                if value.is_str() {
                    Ok(())
                } else {
                    Err("expected string".to_owned())
                }
            }
            Self::Choice(options) => {
                if let Some(v) = value.as_str() {
                    if options.iter().any(|(val, _)| *val == v) {
                        Ok(())
                    } else {
                        let valid: Vec<_> = options.iter().map(|(val, _)| *val).collect();
                        Err(format!("value must be one of {:?}", valid))
                    }
                } else {
                    Err("expected string".to_owned())
                }
            }
            Self::LineWidth => {
                // Can be an integer in range or the string "full"
                if let Some(v) = value.as_int() {
                    if (40..=160).contains(&v) {
                        Ok(())
                    } else {
                        Err("value must be between 40 and 160 or 'full'".to_owned())
                    }
                } else if let Some(v) = value.as_str() {
                    if v == "full" {
                        Ok(())
                    } else {
                        Err("value must be between 40 and 160 or 'full'".to_owned())
                    }
                } else {
                    Err("expected integer or 'full'".to_owned())
                }
            }
            Self::ThemePackage => {
                // Any string is valid (will be validated against installed themes at runtime)
                if value.is_str() {
                    Ok(())
                } else {
                    Err("expected string".to_owned())
                }
            }
            Self::StringList => {
                if value.is_list() {
                    Ok(())
                } else {
                    Err("expected list of strings".to_owned())
                }
            }
            Self::ShortcutMap => {
                if value.is_shortcut_map() {
                    Ok(())
                } else {
                    Err("expected shortcut map".to_owned())
                }
            }
        }
    }

    /// Returns the default value for this kind.
    #[must_use]
    pub fn default_value(&self) -> SettingValue {
        match self {
            Self::Bool => SettingValue::Bool(false),
            Self::Int { min, .. } => SettingValue::Int(*min),
            Self::Text => SettingValue::Str(String::new()),
            Self::Choice(options) => {
                SettingValue::Str(options.first().map(|(v, _)| (*v).to_owned()).unwrap_or_default())
            }
            Self::LineWidth => SettingValue::Int(80),
            Self::ThemePackage => SettingValue::Str(String::new()),
            Self::StringList => SettingValue::List(Vec::new()),
            Self::ShortcutMap => SettingValue::ShortcutMap(BTreeMap::new()),
        }
    }
}

/// A descriptor for a single setting.
#[derive(Debug, Clone)]
pub struct SettingDescriptor {
    /// The unique identifier for this setting (dotted, lowercase, <=128 bytes).
    pub id: &'static str,
    /// The category this setting belongs to.
    pub category: &'static str,
    /// Human-readable title.
    pub title: &'static str,
    /// Description of what this setting does.
    pub description: &'static str,
    /// The kind of this setting (determines validation and UI).
    pub kind: SettingKind,
    /// The default value for this setting.
    pub default: SettingValue,
    /// Whether this setting is visible in the UI.
    pub visibility: Visibility,
    /// When changes to this setting take effect.
    pub apply: Apply,
    /// Whether this setting requires a writable vault.
    pub requires_write: bool,
}

impl SettingDescriptor {
    /// Create a new setting descriptor.
    #[must_use]
    pub const fn new(
        id: &'static str,
        category: &'static str,
        title: &'static str,
        description: &'static str,
        kind: SettingKind,
        default: SettingValue,
        visibility: Visibility,
        apply: Apply,
        requires_write: bool,
    ) -> Self {
        Self {
            id,
            category,
            title,
            description,
            kind,
            default,
            visibility,
            apply,
            requires_write,
        }
    }

    /// Validate that the ID is well-formed.
    /// IDs must be dotted (at least two components), lowercase alphanumeric with hyphens,
    /// and <= 128 bytes. They must not start with reserved prefixes.
    #[must_use]
    pub fn is_valid_id(id: &str) -> bool {
        if id.len() > 128 {
            return false;
        }
        if !id.contains('.') {
            return false;
        }
        if id.starts_with("plugins.") {
            return false;
        }
        id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-' || c == '_')
    }

    /// Validate a value against this descriptor's kind.
    pub fn validate_value(&self, value: &SettingValue) -> Result<(), SettingsCodecError> {
        self.kind.validate(value).map_err(|e| SettingsCodecError::InvalidValue(self.id.to_owned(), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_valid_ids() {
        assert!(SettingDescriptor::is_valid_id("appearance.theme"));
        assert!(SettingDescriptor::is_valid_id("editor.font_size"));
        assert!(SettingDescriptor::is_valid_id("vault.rename_behavior"));
        assert!(SettingDescriptor::is_valid_id("a.b"));
        assert!(SettingDescriptor::is_valid_id("editor.line-width"));
    }

    #[test]
    fn test_invalid_ids() {
        // Too long
        let long_id = "a".repeat(129);
        assert!(!SettingDescriptor::is_valid_id(&long_id));
        
        // No dot
        assert!(!SettingDescriptor::is_valid_id("appearance"));
        
        // Starts with plugins.
        assert!(!SettingDescriptor::is_valid_id("plugins.foo.bar"));
        
        // Uppercase
        assert!(!SettingDescriptor::is_valid_id("Appearance.Theme"));
        
        // Invalid characters (underscores are actually allowed per RFC)
        assert!(!SettingDescriptor::is_valid_id("appearance.theme value"));
    }

    #[test]
    fn test_int_kind_validation() {
        let kind = SettingKind::Int { min: 10, max: 100 };
        
        assert!(kind.validate(&SettingValue::Int(50)).is_ok());
        assert!(kind.validate(&SettingValue::Int(10)).is_ok());
        assert!(kind.validate(&SettingValue::Int(100)).is_ok());
        assert!(kind.validate(&SettingValue::Int(9)).is_err());
        assert!(kind.validate(&SettingValue::Int(101)).is_err());
        assert!(kind.validate(&SettingValue::Bool(true)).is_err());
    }

    #[test]
    fn test_choice_kind_validation() {
        let kind = SettingKind::Choice(&[("light", "Light"), ("dark", "Dark")]);
        
        assert!(kind.validate(&SettingValue::Str("light".to_owned())).is_ok());
        assert!(kind.validate(&SettingValue::Str("dark".to_owned())).is_ok());
        assert!(kind.validate(&SettingValue::Str("system".to_owned())).is_err());
    }

    #[test]
    fn test_line_width_validation() {
        let kind = SettingKind::LineWidth;
        
        assert!(kind.validate(&SettingValue::Int(80)).is_ok());
        assert!(kind.validate(&SettingValue::Int(40)).is_ok());
        assert!(kind.validate(&SettingValue::Int(160)).is_ok());
        assert!(kind.validate(&SettingValue::Str("full".to_owned())).is_ok());
        assert!(kind.validate(&SettingValue::Int(39)).is_err());
        assert!(kind.validate(&SettingValue::Int(161)).is_err());
    }
}
