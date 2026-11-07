//! TOML codec for settings documents (schema v2).
//!
//! This module implements the v2 namespaced TOML document codec with:
//! - Sections support
//! - Unknown field preservation at document and section level
//! - Schema gating
//! - Hard bounds (256 KiB maximum)

#![allow(clippy::missing_errors_doc)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_sign_loss)]

use std::collections::BTreeMap;

use crate::descriptor::SCHEMA_VERSION;
use crate::document::{Parsed, SettingsDoc};
use crate::error::SettingsCodecError;
use crate::registry::SettingRegistry;
use crate::value::SettingValue;

/// Maximum document size in bytes (256 KiB).
pub const MAX_DOCUMENT_SIZE: usize = 256 * 1024;

/// Parse a settings document from TOML text.
///
/// # Arguments
/// * `text` - Optional TOML text (None means empty document = defaults)
///
/// # Returns
/// A `Parsed` document with warnings, or an error if the document is fundamentally
/// unparseable or exceeds size limits.
pub fn parse(text: Option<&str>) -> Result<Parsed, SettingsCodecError> {
    let text = text.unwrap_or_default();

    // Check size limit
    if text.len() > MAX_DOCUMENT_SIZE {
        return Err(SettingsCodecError::DocumentTooLarge);
    }

    // Parse TOML
    let value: toml::Value = toml::from_str(text).map_err(|e| {
        SettingsCodecError::Unparseable(format!("TOML parse error: {e}"))
    })?;

    let table = value.as_table().ok_or_else(|| {
        SettingsCodecError::Unparseable("settings root must be a TOML table".to_owned())
    })?;

    // Check schema version
    let schema = table
        .get("schema")
        .and_then(toml::Value::as_integer)
        .map_or(SCHEMA_VERSION, |v| v as u32); // Default to current schema version

    let mut warnings = Vec::new();

    // Schema gate
    if schema > SCHEMA_VERSION {
        return Ok(Parsed::new(
            SettingsDoc::new(schema),
            vec![format!("schema version {schema} is newer than supported version {SCHEMA_VERSION}")],
        ));
    }

    let mut doc = SettingsDoc::new(schema);

    // Process known sections and unknown tables
    let mut known_sections = Vec::new();
    let mut found_unknown_tables = false;

    let registry = SettingRegistry::with_builtins();
    
    for (key, value) in table {
        if key == "schema" {
            continue;
        }

        // Check for reserved namespace - [plugins] and [plugins.x] tables
        if key == "plugins" {
            // Preserve the entire plugins section with all nested tables
            if let toml::Value::Table(plugin_table) = value {
                let mut map = BTreeMap::new();
                for (k, v) in plugin_table {
                    map.insert(k.clone(), v.clone());
                }
                doc.unknown_tables.insert(key.clone(), map);
                found_unknown_tables = true;
                continue;
            }
        }

        // Try to parse as a known section
        if let toml::Value::Table(section_table) = value {
            let categories = registry.by_category();
            // Check if the key (lowercase from TOML) matches a category (capitalized in registry)
            let is_known_category = categories.contains_key(key.as_str())
                || categories.keys().any(|c| c.to_lowercase() == key.to_lowercase());
            
            if is_known_category || is_valid_section_name(key) {
                let mut section_map = BTreeMap::new();
                let mut section_unknown = BTreeMap::new();

                for (k, v) in section_table {
                    // Try to look up the descriptor
                    let id = format!("{key}.{k}");
                    
                    if let Some(descriptor) = registry.lookup(&id) {
                        // Known setting - parse and validate
                        if let Ok(setting_value) = parse_value(v, &descriptor.kind) {
                            if descriptor.kind.validate(&setting_value).is_ok() {
                                section_map.insert(k.clone(), setting_value);
                            } else {
                                warnings.push(format!(
                                    "{id}: value out of bounds, using default"
                                ));
                                section_map.insert(k.clone(), descriptor.default.clone());
                            }
                        } else {
                            warnings.push(format!(
                                "{id}: invalid value type, using default"
                            ));
                            section_map.insert(k.clone(), descriptor.default.clone());
                        }
                    } else {
                        // Unknown key in known section - preserve
                        section_unknown.insert(k.clone(), v.clone());
                    }
                }

                if !section_map.is_empty() || is_known_category {
                    doc.sections.insert(key.clone(), section_map);
                }
                if !section_unknown.is_empty() {
                    doc.unknown_keys.insert(key.clone(), section_unknown);
                }
                known_sections.push(key.clone());
            } else {
                // Unknown top-level table - preserve
                let mut map = BTreeMap::new();
                if let toml::Value::Table(table) = value {
                    for (k, v) in table {
                        map.insert(k.clone(), v.clone());
                    }
                }
                doc.unknown_tables.insert(key.clone(), map);
                found_unknown_tables = true;
            }
        } else {
            // Non-table value at top level - this is unexpected but preserve it
            let mut map = BTreeMap::new();
            map.insert("value".to_owned(), value.clone());
            doc.unknown_tables.insert(key.clone(), map);
            found_unknown_tables = true;
        }
    }

    if found_unknown_tables {
        warnings.push("document contains unknown sections or tables".to_owned());
    }

    Ok(Parsed::new(doc, warnings))
}

/// Check if a section name is valid (lowercase alphanumeric with hyphens/underscores).
fn is_valid_section_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' || c == '.'
    })
}

/// Parse a `toml::Value` into a `SettingValue` based on the expected kind.
fn parse_value(value: &toml::Value, kind: &crate::descriptor::SettingKind) -> Result<SettingValue, String> {
    match kind {
        crate::descriptor::SettingKind::Bool => {
            value.as_bool().map(SettingValue::Bool).ok_or_else(|| "expected boolean".to_owned())
        }
        crate::descriptor::SettingKind::Int { .. } => {
            value.as_integer().map(SettingValue::Int).ok_or_else(|| "expected integer".to_owned())
        }
        crate::descriptor::SettingKind::Text => {
            value.as_str().map(|s| SettingValue::Str(s.to_owned())).ok_or_else(|| "expected string".to_owned())
        }
        crate::descriptor::SettingKind::Choice(_) => {
            value.as_str().map(|s| SettingValue::Str(s.to_owned())).ok_or_else(|| "expected string".to_owned())
        }
        crate::descriptor::SettingKind::LineWidth => {
            // Can be integer or string "full"
            if let Some(s) = value.as_str() {
                Ok(SettingValue::Str(s.to_owned()))
            } else if let Some(i) = value.as_integer() {
                Ok(SettingValue::Int(i))
            } else {
                Err("expected integer or string".to_owned())
            }
        }
        crate::descriptor::SettingKind::ThemePackage => {
            value.as_str().map(|s| SettingValue::Str(s.to_owned())).ok_or_else(|| "expected string".to_owned())
        }
        crate::descriptor::SettingKind::StringList => {
            let array = value.as_array().ok_or_else(|| "expected array".to_owned())?;
            let list: Result<Vec<String>, _> = array
                .iter()
                .map(|v| v.as_str().map(std::borrow::ToOwned::to_owned).ok_or_else(|| "expected string in list".to_owned()))
                .collect();
            list.map(SettingValue::List)
        }
        crate::descriptor::SettingKind::ShortcutMap => {
            let table = value.as_table().ok_or_else(|| "expected table".to_owned())?;
            let mut map = BTreeMap::new();
            for (k, v) in table {
                let chord = v.as_str().ok_or_else(|| format!("expected string chord for {k}"))?.to_owned();
                map.insert(k.clone(), chord);
            }
            Ok(SettingValue::ShortcutMap(map))
        }
    }
}

/// Serialize a settings document to TOML text.
///
/// Known sections are written first in fixed order, followed by preserved
/// unknown tables and keys to ensure byte-stable output.
pub fn serialize(doc: &SettingsDoc) -> Result<String, SettingsCodecError> {
    let mut table = toml::value::Table::new();

    // Add schema
    table.insert("schema".to_owned(), toml::Value::Integer(i64::from(doc.schema)));

    // Add known sections in fixed order
    let category_order = [
        "appearance", "editor", "links", "sidebar", "vault", "shortcuts", "state",
    ];

    for category in &category_order {
        if let Some(section) = doc.sections.get(*category) {
            let mut section_table = toml::value::Table::new();
            
            for (key, value) in section {
                let toml_value = to_toml_value(value);
                section_table.insert(key.clone(), toml_value);
            }

            // Add unknown keys for this section
            if let Some(unknown) = doc.unknown_keys.get(*category) {
                for (key, value) in unknown {
                    section_table.insert(key.clone(), value.clone());
                }
            }

            if !section_table.is_empty() {
                table.insert(category.to_string(), toml::Value::Table(section_table));
            }
        }
    }

    // Add unknown sections in sorted order
    for (name, keys) in &doc.unknown_keys {
        if !doc.sections.contains_key(name.as_str()) {
            let mut section_table = toml::value::Table::new();
            for (key, value) in keys {
                section_table.insert(key.clone(), value.clone());
            }
            table.insert(name.clone(), toml::Value::Table(section_table));
        }
    }

    // Add unknown tables in sorted order
    for (name, keys) in &doc.unknown_tables {
        if !doc.sections.contains_key(name.as_str()) && !doc.unknown_keys.contains_key(name.as_str()) {
            let mut section_table = toml::value::Table::new();
            for (key, value) in keys {
                section_table.insert(key.clone(), value.clone());
            }
            table.insert(name.clone(), toml::Value::Table(section_table));
        }
    }

    toml::to_string(&toml::Value::Table(table)).map_err(|e| {
        SettingsCodecError::Unparseable(format!("TOML serialization error: {e}"))
    })
}

/// Convert a `SettingValue` to a `toml::Value`.
fn to_toml_value(value: &SettingValue) -> toml::Value {
    match value {
        SettingValue::Bool(v) => toml::Value::Boolean(*v),
        SettingValue::Int(v) => toml::Value::Integer(*v),
        SettingValue::Str(v) => toml::Value::String(v.clone()),
        SettingValue::List(v) => {
            toml::Value::Array(v.iter().map(|s| toml::Value::String(s.clone())).collect())
        }
        SettingValue::ShortcutMap(map) => {
            let mut table = toml::value::Table::new();
            for (k, v) in map {
                table.insert(k.clone(), toml::Value::String(v.clone()));
            }
            toml::Value::Table(table)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_empty() {
        let result = parse(None);
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.doc.schema, 2); // Defaults to v2
    }

    #[test]
    fn test_parse_schema_2() {
        let toml = r#"
schema = 2

[appearance]
theme = "dark"
scale_pct = 125
"#;
        let result = parse(Some(toml));
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.doc.schema, 2);
        assert!(parsed.doc.contains("appearance.theme"));
        assert_eq!(parsed.doc.get("appearance.theme").unwrap().as_str(), Some("dark"));
    }

    #[test]
    fn test_parse_unknown_section_keys_preserved() {
        let toml = r#"
schema = 2

[appearance]
theme = "dark"
future_option = "value"
"#;
        let result = parse(Some(toml));
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert!(parsed.doc.unknown_keys.get("appearance").is_some());
        assert!(parsed.doc.unknown_keys["appearance"].contains_key("future_option"));
    }

    #[test]
    fn test_parse_unknown_top_level_table_preserved() {
        let toml = r#"
schema = 2

[appearance]
theme = "dark"

[plugins.foo]
option = "value"
"#;
        let result = parse(Some(toml));
        assert!(result.is_ok());
        let parsed = result.unwrap();
        // [plugins.foo] is stored as {"plugins": {"foo": {"option": "value"}}}
        // So unknown_tables contains "plugins" as the key
        assert!(parsed.doc.unknown_tables.contains_key("plugins"));
        assert!(parsed.doc.unknown_tables["plugins"].contains_key("foo"));
    }

    #[test]
    fn test_parse_schema_3_read_only() {
        let toml = r#"
schema = 3

[appearance]
theme = "dark"
"#;
        let result = parse(Some(toml));
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert!(parsed.read_only);
        assert_eq!(parsed.doc.schema, 3);
    }

    #[test]
    fn test_parse_invalid_toml() {
        let toml = r#"
schema = 2
[appearance
"#;
        let result = parse(Some(toml));
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_oversized_document() {
        let toml = "x".repeat(MAX_DOCUMENT_SIZE + 1);
        let result = parse(Some(&toml));
        assert!(matches!(result.unwrap_err(), SettingsCodecError::DocumentTooLarge));
    }

    #[test]
    fn test_round_trip_known_sections() {
        let toml = r#"
schema = 2

[appearance]
theme = "dark"
scale_pct = 125
reduced_motion = true

[editor]
font_size = 16
line_width = 80
"#;
        let parsed = parse(Some(toml)).unwrap();
        let serialized = serialize(&parsed.doc).unwrap();
        let reparsed = parse(Some(&serialized)).unwrap();
        
        assert_eq!(parsed.doc.schema, reparsed.doc.schema);
        assert_eq!(parsed.doc.get("appearance.theme"), reparsed.doc.get("appearance.theme"));
        assert_eq!(parsed.doc.get("appearance.scale_pct"), reparsed.doc.get("appearance.scale_pct"));
        assert_eq!(parsed.doc.get("editor.font_size"), reparsed.doc.get("editor.font_size"));
    }

    #[test]
    fn test_round_trip_preserves_unknown() {
        let toml = r#"
schema = 2

[appearance]
theme = "dark"
future_option = "value"

[plugins.foo]
option = "bar"
"#;
        let parsed = parse(Some(toml)).unwrap();
        let serialized = serialize(&parsed.doc).unwrap();
        let reparsed = parse(Some(&serialized)).unwrap();
        
        assert!(reparsed.doc.unknown_keys.get("appearance").is_some());
        // plugins.foo is stored under "plugins" key
        assert!(reparsed.doc.unknown_tables.contains_key("plugins"));
        assert!(reparsed.doc.unknown_tables["plugins"].contains_key("foo"));
    }

    #[test]
    fn test_byte_stable_serialization() {
        let toml = r#"
schema = 2

[appearance]
theme = "dark"

[editor]
font_size = 16
"#;
        let parsed1 = parse(Some(toml)).unwrap();
        let serialized1 = serialize(&parsed1.doc).unwrap();
        
        let parsed2 = parse(Some(&serialized1)).unwrap();
        let serialized2 = serialize(&parsed2.doc).unwrap();
        
        // Should be byte-identical
        assert_eq!(serialized1, serialized2);
    }

    #[test]
    fn test_parse_line_width_full() {
        let toml = r#"
schema = 2

[editor]
line_width = "full"
"#;
        let result = parse(Some(toml));
        assert!(result.is_ok());
        let parsed = result.unwrap();
        assert_eq!(parsed.doc.get("editor.line_width").unwrap().as_str(), Some("full"));
    }

    #[test]
    fn test_parse_shortcuts_section() {
        // Shortcuts section with direct command-id -> chord-string entries
        // These are unknown keys in the shortcuts section (the descriptor is shortcuts.overrides)
        let toml = r#"
schema = 2

[shortcuts]
"note.save" = "Mod+S"
"note.new" = "Mod+N"
"#;
        let result = parse(Some(toml));
        assert!(result.is_ok());
        let parsed = result.unwrap();
        // Debug output
        // println!("sections: {:?}", parsed.doc.sections.keys().collect::<Vec<_>>());
        // println!("unknown_keys: {:?}", parsed.doc.unknown_keys.keys().collect::<Vec<_>>());
        
        // The shortcuts section should be created, and the unknown keys preserved
        assert!(parsed.doc.sections.contains_key("shortcuts"), "sections should contain shortcuts");
        // The actual shortcut entries are unknown keys in the shortcuts section
        assert!(parsed.doc.unknown_keys.contains_key("shortcuts"));
        assert_eq!(parsed.doc.unknown_keys["shortcuts"].len(), 2);
    }

    #[test]
    fn test_parse_string_list() {
        let toml = r#"
schema = 2

[vault]
ignore_globs = ["*.tmp", ".DS_Store"]
"#;
        let result = parse(Some(toml));
        assert!(result.is_ok());
        let parsed = result.unwrap();
        let value = parsed.doc.get("vault.ignore_globs");
        assert!(value.is_some());
        let binding = value.unwrap();
        let list_ref = binding.as_list().unwrap();
        let expected = vec!["*.tmp".to_owned(), ".DS_Store".to_owned()];
        assert_eq!(list_ref, &expected);
    }
}
