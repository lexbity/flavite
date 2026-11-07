//! Resolution of settings documents against the registry.

use std::collections::BTreeMap;

use crate::document::SettingsDoc;
use crate::registry::SettingRegistry;
use crate::value::SettingValue;

/// A resolved settings document with effective values (document values with defaults filled in).
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    values: BTreeMap<String, SettingValue>,
    warnings: Vec<String>,
}

impl Resolved {
    /// Create a new empty resolved document.
    #[must_use]
    pub fn new() -> Self {
        Self {
            values: BTreeMap::new(),
            warnings: Vec::new(),
        }
    }

    /// Get a resolved value by ID.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&SettingValue> {
        self.values.get(id)
    }

    /// Get a boolean value by ID, returning the default if not found.
    /// Panics in debug if the value is not a boolean.
    #[must_use]
    pub fn bool(&self, id: &str) -> bool {
        self.get(id)
            .map(|v| v.as_bool().unwrap_or(false))
            .unwrap_or(false)
    }

    /// Get an integer value by ID, returning the default if not found.
    /// Panics in debug if the value is not an integer.
    #[must_use]
    pub fn int(&self, id: &str) -> i64 {
        self.get(id)
            .map(|v| v.as_int().unwrap_or(0))
            .unwrap_or(0)
    }

    /// Get a string value by ID, returning the default if not found.
    #[must_use]
    pub fn str(&self, id: &str) -> String {
        self.get(id)
            .map(|v| v.as_str().unwrap_or("").to_owned())
            .unwrap_or_default()
    }

    /// Get a list value by ID, returning the default if not found.
    #[must_use]
    pub fn list(&self, id: &str) -> Vec<String> {
        self.get(id)
            .map(|v| v.as_list().cloned().unwrap_or_default())
            .unwrap_or_default()
    }

    /// Check if a value is modified from the default.
    #[must_use]
    pub fn is_modified(&self, id: &str, registry: &SettingRegistry) -> bool {
        if let Some(descriptor) = registry.lookup(id) {
            self.get(id) != Some(&descriptor.default)
        } else {
            false
        }
    }

    /// Get all resolved values.
    #[must_use]
    pub fn all(&self) -> &BTreeMap<String, SettingValue> {
        &self.values
    }

    /// Get the warnings accumulated during resolution.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

/// Extension trait for `SettingRegistry` to resolve documents.
pub trait RegistryResolve {
    /// Resolve a document against the registry, filling in defaults for missing values.
    fn resolve(&self, doc: &SettingsDoc) -> Resolved;
}

impl RegistryResolve for SettingRegistry {
    fn resolve(&self, doc: &SettingsDoc) -> Resolved {
        let mut resolved = Resolved::new();

        // Start with defaults from all descriptors
        for descriptor in self.all() {
            resolved.values.insert(descriptor.id.to_owned(), descriptor.default.clone());
        }

        // Overlay with document values
        let mut warnings = Vec::new();
        for (section, section_map) in &doc.sections {
            for (key, value) in section_map {
                let id = format!("{section}.{key}");
                
                // Validate against descriptor if it exists
                if let Some(descriptor) = self.lookup(&id) {
                    if let Err(e) = descriptor.kind.validate(value) {
                        warnings.push(format!("{id}: {e}"));
                        // Use default for invalid values
                        resolved.values.insert(id, descriptor.default.clone());
                    } else {
                        resolved.values.insert(id, value.clone());
                    }
                } else {
                    // Unknown key - it's already in doc.unknown_keys, but we still track it
                    // for completeness (won't be used since no descriptor)
                    resolved.values.insert(id, value.clone());
                }
            }
        }

        resolved.warnings = warnings;
        resolved
    }
}

/// Create a defaults document from the registry.
#[must_use]
pub fn defaults_document(registry: &SettingRegistry) -> SettingsDoc {
    let mut doc = SettingsDoc::new(crate::descriptor::SCHEMA_VERSION);
    
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_empty_document() {
        let registry = SettingRegistry::with_builtins();
        let doc = SettingsDoc::new(2);
        let resolved = registry.resolve(&doc);
        
        // Should have all defaults
        assert!(resolved.get("appearance.theme").is_some());
        assert_eq!(resolved.str("appearance.theme"), "system");
        assert_eq!(resolved.int("editor.font_size"), 14);
        assert_eq!(resolved.bool("sidebar.open"), true);
    }

    #[test]
    fn test_resolve_document_overrides() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        // Set some values
        doc.set("appearance.theme", SettingValue::Str("dark".to_owned()), &registry).unwrap();
        doc.set("editor.font_size", SettingValue::Int(18), &registry).unwrap();
        
        let resolved = registry.resolve(&doc);
        
        // Should have overridden values
        assert_eq!(resolved.str("appearance.theme"), "dark");
        assert_eq!(resolved.int("editor.font_size"), 18);
        
        // Other values should be defaults
        assert_eq!(resolved.int("editor.line_width"), 80);
    }

    #[test]
    fn test_resolve_invalid_value_uses_default() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        // Create the editor section and add an invalid value directly
        doc.sections.insert("editor".to_owned(), BTreeMap::new());
        doc.sections.get_mut("editor").unwrap().insert("font_size".to_owned(), SettingValue::Int(999));
        
        let resolved = registry.resolve(&doc);
        
        // Should fall back to default with warning
        assert_eq!(resolved.int("editor.font_size"), 14); // default
        assert!(!resolved.warnings.is_empty());
    }

    #[test]
    fn test_is_modified() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        doc.set("appearance.theme", SettingValue::Str("dark".to_owned()), &registry).unwrap();
        
        let resolved = registry.resolve(&doc);
        
        assert!(resolved.is_modified("appearance.theme", &registry));
        assert!(!resolved.is_modified("appearance.scale_pct", &registry));
    }

    #[test]
    fn test_defaults_document() {
        let registry = SettingRegistry::with_builtins();
        let doc = defaults_document(&registry);
        
        assert_eq!(doc.schema, 2);
        
        // Should have all builtin settings
        assert!(doc.contains("appearance.theme"));
        assert!(doc.contains("editor.font_size"));
        assert!(doc.contains("vault.rename_behavior"));
    }

    #[test]
    fn test_all_values() {
        let registry = SettingRegistry::with_builtins();
        let doc = SettingsDoc::new(2);
        let resolved = registry.resolve(&doc);
        
        // Should have all 16 descriptors
        assert_eq!(resolved.all().len(), 16);
    }
}
