//! Settings document model with sections and unknown field preservation.

use std::collections::BTreeMap;

use crate::descriptor::SCHEMA_VERSION;
use crate::error::SettingsCodecError;
use crate::registry::SettingRegistry;
use crate::value::SettingValue;

/// A parsed settings document with schema version, sections, and preserved unknown fields.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsDoc {
    /// The schema version of this document.
    pub schema: u32,
    /// Known sections: category name -> (setting id -> value).
    pub sections: BTreeMap<String, BTreeMap<String, SettingValue>>,
    /// Unknown top-level tables (preserved verbatim).
    pub unknown_tables: BTreeMap<String, BTreeMap<String, toml::Value>>,
    /// Unknown keys within known sections (preserved verbatim).
    pub unknown_keys: BTreeMap<String, BTreeMap<String, toml::Value>>,
}

impl Default for SettingsDoc {
    fn default() -> Self {
        Self::new(SCHEMA_VERSION)
    }
}

impl SettingsDoc {
    /// Create a new document with the given schema version.
    #[must_use]
    pub fn new(schema: u32) -> Self {
        Self {
            schema,
            sections: BTreeMap::new(),
            unknown_tables: BTreeMap::new(),
            unknown_keys: BTreeMap::new(),
        }
    }

    /// Get a setting value by its dotted ID (e.g., "appearance.theme").
    #[must_use]
    pub fn get(&self, id: &str) -> Option<SettingValue> {
        let parts: Vec<&str> = id.split('.').collect();
        if parts.len() != 2 {
            return None;
        }
        let (section, key) = (parts[0], parts[1]);
        self.sections.get(section).and_then(|map| map.get(key).cloned())
    }

    /// Set a setting value by its dotted ID.
    /// Validates the value against the registry descriptor.
    ///
    /// # Errors
    /// Returns an error if the ID is invalid, reserved, or the value doesn't match the descriptor.
    pub fn set(
        &mut self,
        id: &str,
        value: SettingValue,
        registry: &SettingRegistry,
    ) -> Result<(), SettingsCodecError> {
        // Validate the ID
        if SettingRegistry::is_reserved_namespace(id) {
            return Err(SettingsCodecError::ReservedNamespace(id.to_owned()));
        }
        if !crate::descriptor::SettingDescriptor::is_valid_id(id) {
            return Err(SettingsCodecError::UnknownKey(id.to_owned()));
        }

        // Look up the descriptor to validate the value
        if let Some(descriptor) = registry.lookup(id) {
            descriptor.validate_value(&value)?;
        }

        // Set the value
        let parts: Vec<&str> = id.split('.').collect();
        if parts.len() != 2 {
            return Err(SettingsCodecError::UnknownKey(id.to_owned()));
        }
        let (section, key) = (parts[0], parts[1]);
        
        // Ensure the section exists and is mutable
        self.sections
            .entry(section.to_owned())
            .or_default()
            .insert(key.to_owned(), value);

        Ok(())
    }

    /// Check if a setting exists in this document.
    #[must_use]
    pub fn contains(&self, id: &str) -> bool {
        self.get(id).is_some()
    }

    /// Remove a setting from the document.
    pub fn remove(&mut self, id: &str) {
        let parts: Vec<&str> = id.split('.').collect();
        if parts.len() == 2 {
            let (section, key) = (parts[0], parts[1]);
            if let Some(map) = self.sections.get_mut(section) {
                map.remove(key);
            }
        }
    }

    /// Get all settings as a flat map of id -> value.
    #[must_use]
    pub fn all_settings(&self) -> BTreeMap<String, SettingValue> {
        let mut result = BTreeMap::new();
        for (section, map) in &self.sections {
            for (key, value) in map {
                result.insert(format!("{section}.{key}"), value.clone());
            }
        }
        result
    }

    /// Get a section by name, if it exists.
    #[must_use]
    pub fn section(&self, name: &str) -> Option<&BTreeMap<String, SettingValue>> {
        self.sections.get(name)
    }

    /// Get a mutable reference to a section by name.
    #[must_use]
    pub fn section_mut(&mut self, name: &str) -> Option<&mut BTreeMap<String, SettingValue>> {
        self.sections.get_mut(name)
    }

    /// Check if this document is read-only due to schema version.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.schema > SCHEMA_VERSION
    }

    /// Get the schema version.
    #[must_use]
    pub fn schema_version(&self) -> u32 {
        self.schema
    }
}

/// A parsed document with warnings from validation.
#[derive(Debug, Clone)]
pub struct Parsed {
    /// The parsed document.
    pub doc: SettingsDoc,
    /// Warnings accumulated during parsing.
    pub warnings: Vec<String>,
    /// Whether the document is read-only (schema > current).
    pub read_only: bool,
}

impl Parsed {
    /// Create a new parsed document.
    #[must_use]
    pub fn new(doc: SettingsDoc, warnings: Vec<String>) -> Self {
        Self {
            read_only: doc.is_read_only(),
            doc,
            warnings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_document() {
        let doc = SettingsDoc::new(2);
        assert_eq!(doc.schema, 2);
        assert!(doc.sections.is_empty());
        assert!(doc.unknown_tables.is_empty());
        assert!(doc.unknown_keys.is_empty());
    }

    #[test]
    fn test_get_set() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        // Set a value
        assert!(doc.set("appearance.theme", SettingValue::Str("dark".to_owned()), &registry).is_ok());
        
        // Get the value back
        let value = doc.get("appearance.theme");
        assert!(value.is_some());
        assert_eq!(value.unwrap().as_str(), Some("dark"));
    }

    #[test]
    fn test_invalid_id() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        // Invalid ID (no dot)
        assert!(doc.set("invalid", SettingValue::Bool(true), &registry).is_err());
        
        // Reserved namespace
        assert!(doc.set("plugins.foo", SettingValue::Bool(true), &registry).is_err());
    }

    #[test]
    fn test_invalid_value_type() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        // Try to set a string where an int is expected
        assert!(doc.set("editor.font_size", SettingValue::Str("not_a_number".to_owned()), &registry).is_err());
    }

    #[test]
    fn test_contains() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        assert!(!doc.contains("appearance.theme"));
        doc.set("appearance.theme", SettingValue::Str("light".to_owned()), &registry).unwrap();
        assert!(doc.contains("appearance.theme"));
    }

    #[test]
    fn test_remove() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        doc.set("appearance.theme", SettingValue::Str("dark".to_owned()), &registry).unwrap();
        assert!(doc.contains("appearance.theme"));
        
        doc.remove("appearance.theme");
        assert!(!doc.contains("appearance.theme"));
    }

    #[test]
    fn test_all_settings() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        doc.set("appearance.theme", SettingValue::Str("dark".to_owned()), &registry).unwrap();
        doc.set("editor.font_size", SettingValue::Int(16), &registry).unwrap();
        
        let all = doc.all_settings();
        assert_eq!(all.len(), 2);
        assert!(all.contains_key("appearance.theme"));
        assert!(all.contains_key("editor.font_size"));
    }

    #[test]
    fn test_section() {
        let registry = SettingRegistry::with_builtins();
        let mut doc = SettingsDoc::new(2);
        
        doc.set("appearance.theme", SettingValue::Str("dark".to_owned()), &registry).unwrap();
        doc.set("appearance.scale_pct", SettingValue::Int(125), &registry).unwrap();
        
        let section = doc.section("appearance");
        assert!(section.is_some());
        assert_eq!(section.unwrap().len(), 2);
    }

    #[test]
    fn test_is_read_only() {
        let doc = SettingsDoc::new(SCHEMA_VERSION);
        assert!(!doc.is_read_only());
        
        let doc = SettingsDoc::new(SCHEMA_VERSION + 1);
        assert!(doc.is_read_only());
    }
}
