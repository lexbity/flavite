//! Setting registry that owns all built-in setting descriptors.

use std::collections::BTreeMap;

use crate::descriptor::{Apply, SettingDescriptor, SettingKind, Visibility};
use crate::value::SettingValue;

/// The setting registry containing all built-in descriptors.
#[derive(Debug, Clone, Default)]
pub struct SettingRegistry {
    descriptors: Vec<SettingDescriptor>,
}

impl SettingRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            descriptors: Vec::new(),
        }
    }

    /// Get the built-in descriptors (the complete v1 set from RFC-0007 D-04).
    /// This is the authoritative list of all settings the backend expresses.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn builtins() -> Vec<SettingDescriptor> {
        vec![
            // Appearance settings
            SettingDescriptor::new(
                "appearance.theme",
                "Appearance",
                "Theme",
                "Light/dark/System color scheme",
                SettingKind::Choice(&[("system", "System"), ("light", "Light"), ("dark", "Dark")]),
                SettingValue::Str("system".to_owned()),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            SettingDescriptor::new(
                "appearance.theme_package",
                "Appearance",
                "Theme Package",
                "Installed theme package shaping the palette",
                SettingKind::ThemePackage,
                SettingValue::Str(String::new()),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            SettingDescriptor::new(
                "appearance.scale_pct",
                "Appearance",
                "Scale",
                "Interface scale percentage",
                SettingKind::Int { min: 75, max: 200 },
                SettingValue::Int(100),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            SettingDescriptor::new(
                "appearance.reduced_motion",
                "Appearance",
                "Reduced Motion",
                "Reduce motion in animations and transitions",
                SettingKind::Bool,
                SettingValue::Bool(false),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            // Editor settings
            SettingDescriptor::new(
                "editor.font_size",
                "Editor",
                "Font Size",
                "Controls the font size in pixels",
                SettingKind::Int { min: 12, max: 28 },
                SettingValue::Int(14),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            SettingDescriptor::new(
                "editor.line_width",
                "Editor",
                "Line Width",
                "Readable editor line width in columns, or 'full' for pane width",
                SettingKind::LineWidth,
                SettingValue::Int(80),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            // Links settings
            SettingDescriptor::new(
                "links.prefer_stable_links",
                "Links",
                "Prefer Stable Links",
                "Prefer unconflicted stable-ID syntax when inserting links",
                SettingKind::Bool,
                SettingValue::Bool(true),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            // Sidebar settings
            SettingDescriptor::new(
                "sidebar.open",
                "Sidebar",
                "Open",
                "Whether the collapsible vault navigator is open",
                SettingKind::Bool,
                SettingValue::Bool(true),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            SettingDescriptor::new(
                "sidebar.width",
                "Sidebar",
                "Width",
                "Notes sidebar width in logical pixels",
                SettingKind::Int { min: 160, max: 480 },
                SettingValue::Int(260),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            // Vault settings
            SettingDescriptor::new(
                "vault.rename_behavior",
                "Vault",
                "Rename Behavior",
                "Link behavior for in-app note renames",
                SettingKind::Choice(&[("rewrite", "Rewrite"), ("alias", "Alias"), ("none", "None")]),
                SettingValue::Str("rewrite".to_owned()),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            SettingDescriptor::new(
                "vault.recovery_interval_secs",
                "Vault",
                "Recovery Interval",
                "Dirty-buffer recovery interval in seconds",
                SettingKind::Int { min: 1, max: 3600 },
                SettingValue::Int(2),
                Visibility::Visible,
                Apply::OnRestart,
                true,
            ),
            SettingDescriptor::new(
                "vault.verification_interval_mins",
                "Vault",
                "Verification Interval",
                "Content verification interval in minutes",
                SettingKind::Int { min: 1, max: 10080 },
                SettingValue::Int(10),
                Visibility::Visible,
                Apply::OnRestart,
                true,
            ),
            SettingDescriptor::new(
                "vault.change_log_budget_mib",
                "Vault",
                "Change Log Budget",
                "Change-log budget in MiB",
                SettingKind::Int { min: 1, max: 1024 },
                SettingValue::Int(64),
                Visibility::Visible,
                Apply::OnRestart,
                true,
            ),
            SettingDescriptor::new(
                "vault.ignore_globs",
                "Vault",
                "Ignore Globs",
                "Glob patterns to exclude from vault scans",
                SettingKind::StringList,
                SettingValue::List(Vec::new()),
                Visibility::Visible,
                Apply::OnRestart,
                true,
            ),
            // Shortcuts settings
            SettingDescriptor::new(
                "shortcuts.overrides",
                "Shortcuts",
                "Shortcut Overrides",
                "Command-id to chord-string overrides",
                SettingKind::ShortcutMap,
                SettingValue::ShortcutMap(BTreeMap::new()),
                Visibility::Visible,
                Apply::Live,
                false,
            ),
            // State settings (hidden)
            SettingDescriptor::new(
                "state.graph_activation_acknowledged",
                "State",
                "Graph Activation Acknowledged",
                "User acknowledged the changed graph gesture contract",
                SettingKind::Bool,
                SettingValue::Bool(false),
                Visibility::Hidden,
                Apply::Live,
                false,
            ),
        ]
    }

    /// Create a registry with the built-in descriptors.
    #[must_use]
    pub fn with_builtins() -> Self {
        Self {
            descriptors: Self::builtins(),
        }
    }

    /// Get a descriptor by ID.
    #[must_use]
    pub fn lookup(&self, id: &str) -> Option<&SettingDescriptor> {
        self.descriptors.iter().find(|d| d.id == id)
    }

    /// Get all descriptors.
    #[must_use]
    pub fn all(&self) -> &[SettingDescriptor] {
        &self.descriptors
    }

    /// Get all visible descriptors.
    #[must_use]
    pub fn visible(&self) -> Vec<&SettingDescriptor> {
        self.descriptors.iter().filter(|d| d.visibility == Visibility::Visible).collect()
    }

    /// Get descriptors by category.
    #[must_use]
    pub fn by_category(&self) -> BTreeMap<String, Vec<&SettingDescriptor>> {
        let mut map: BTreeMap<String, Vec<&SettingDescriptor>> = BTreeMap::new();
        for descriptor in &self.descriptors {
            map.entry(descriptor.category.to_owned()).or_default().push(descriptor);
        }
        map
    }

    /// Validate that an ID is well-formed and not reserved.
    ///
    /// # Errors
    /// Returns `Err(String)` if the ID is not valid or is in a reserved namespace.
    pub fn validate_id(&self, id: &str) -> Result<(), String> {
        if !SettingDescriptor::is_valid_id(id) {
            return Err(format!("{id} is not a valid setting ID"));
        }
        if id.starts_with("plugins.") {
            return Err(format!("{id} is in a reserved namespace"));
        }
        Ok(())
    }

    /// Check if an ID is in a reserved namespace.
    #[must_use]
    pub fn is_reserved_namespace(id: &str) -> bool {
        id.starts_with("plugins.")
    }
}

/// Convenience function to get the built-in registry.
#[must_use]
pub fn builtins() -> Vec<SettingDescriptor> {
    SettingRegistry::builtins()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtins_count() {
        // RFC-0007 D-04 specifies 16 descriptors (15 visible + 1 hidden)
        let descriptors = SettingRegistry::builtins();
        assert_eq!(descriptors.len(), 16);
    }

    #[test]
    fn test_builtins_all_valid_ids() {
        let descriptors = SettingRegistry::builtins();
        for d in &descriptors {
            assert!(SettingDescriptor::is_valid_id(d.id), "invalid id: {}", d.id);
        }
    }

    #[test]
    fn test_builtins_categories() {
        let registry = SettingRegistry::with_builtins();
        let categories = registry.by_category();
        
        // Should have: Appearance, Editor, Links, Sidebar, Vault, Shortcuts, State
        assert!(categories.contains_key("Appearance"));
        assert!(categories.contains_key("Editor"));
        assert!(categories.contains_key("Links"));
        assert!(categories.contains_key("Sidebar"));
        assert!(categories.contains_key("Vault"));
        assert!(categories.contains_key("Shortcuts"));
        assert!(categories.contains_key("State"));
    }

    #[test]
    fn test_builtins_visible_count() {
        let registry = SettingRegistry::with_builtins();
        let visible = registry.visible();
        // 15 visible + 1 hidden = 16 total
        assert_eq!(visible.len(), 15);
    }

    #[test]
    fn test_lookup() {
        let registry = SettingRegistry::with_builtins();
        
        assert!(registry.lookup("appearance.theme").is_some());
        assert!(registry.lookup("editor.font_size").is_some());
        assert!(registry.lookup("nonexistent").is_none());
    }

    #[test]
    fn test_reserved_namespace() {
        assert!(SettingRegistry::is_reserved_namespace("plugins.foo.bar"));
        assert!(!SettingRegistry::is_reserved_namespace("appearance.theme"));
    }

    #[test]
    fn test_validate_id() {
        let registry = SettingRegistry::with_builtins();
        
        assert!(registry.validate_id("appearance.theme").is_ok());
        assert!(registry.validate_id("plugins.foo").is_err());
        assert!(registry.validate_id("invalid").is_err());
    }

    #[test]
    fn test_snapshot_builtins_table() {
        // Normative test: the D-04 table must be exactly represented
        let descriptors = SettingRegistry::builtins();
        
        // Check each descriptor matches the specification
        let mut found_ids = Vec::new();
        for d in &descriptors {
            found_ids.push(d.id);
        }
        
        // All 16 IDs from D-04
        let expected_ids = vec![
            "appearance.theme",
            "appearance.theme_package",
            "appearance.scale_pct",
            "appearance.reduced_motion",
            "editor.font_size",
            "editor.line_width",
            "links.prefer_stable_links",
            "sidebar.open",
            "sidebar.width",
            "vault.rename_behavior",
            "vault.recovery_interval_secs",
            "vault.verification_interval_mins",
            "vault.change_log_budget_mib",
            "vault.ignore_globs",
            "shortcuts.overrides",
            "state.graph_activation_acknowledged",
        ];
        
        assert_eq!(found_ids, expected_ids);
    }
}
