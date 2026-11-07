//! Settings commands (RFC-0007 §4.5).
//!
//! This module provides the command handlers for settings operations:
//! - `settings.list` - List all settings
//! - `settings.get` - Get a specific setting value
//! - `settings.set` - Set a specific setting value
//! - `settings.reset` - Reset a setting to its default
//!
//! All commands are gated by `Capability::Settings`.
//!
//! The commands are registered in the registry with stub handlers, then
//! routed to [`execute_settings_command`] on the facade turn (they need
//! the facade-owned settings store). The registry still owns their schemas
//! and capabilities.

use crate::command::Invocation;
use crate::flavite::Flavite;
use crate::result::ExecutionResult;
use flavite_settings::registry::SettingRegistry;
use flavite_settings::value::SettingValue;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Command argument types
// ---------------------------------------------------------------------------

/// Arguments for `settings.list`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SettingsListArgs {
    // Empty for now; can be extended with filter options
}

/// Arguments for `settings.get`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SettingsGetArgs {
    /// The setting ID to get.
    pub id: String,
}

/// Arguments for `settings.set`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SettingsSetArgs {
    /// The setting ID to set.
    pub id: String,
    /// The value to set (JSON-serializable).
    pub value: serde_json::Value,
}

/// Arguments for `settings.reset`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SettingsResetArgs {
    /// The setting ID to reset to default.
    pub id: String,
}

// ---------------------------------------------------------------------------
// Result types
// ---------------------------------------------------------------------------

/// Result of `settings.list`.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsListResult {
    /// All setting descriptors with their current values.
    pub settings: Vec<SettingEntry>,
    /// The document generation.
    pub generation: u64,
    /// Whether the document is read-only.
    pub read_only: bool,
    /// Any warnings from parsing.
    pub warnings: Vec<String>,
}

/// A single setting entry in the list result.
#[derive(Debug, Clone, Serialize)]
pub struct SettingEntry {
    /// The setting ID.
    pub id: String,
    /// The category.
    pub category: String,
    /// The title.
    pub title: String,
    /// The description.
    pub description: String,
    /// The current value (JSON-serializable).
    pub value: serde_json::Value,
    /// The default value (JSON-serializable).
    pub default: serde_json::Value,
    /// Whether the value is modified from default.
    pub modified: bool,
    /// When changes take effect.
    pub apply: String,
    /// Whether the setting requires write capability.
    pub requires_write: bool,
}

/// Result of `settings.get`.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsGetResult {
    /// The setting ID.
    pub id: String,
    /// The current value (JSON-serializable).
    pub value: serde_json::Value,
    /// The default value (JSON-serializable).
    pub default: serde_json::Value,
    /// The setting descriptor.
    pub descriptor: SettingDescriptorResult,
}

/// A simplified setting descriptor for results.
#[derive(Debug, Clone, Serialize)]
pub struct SettingDescriptorResult {
    pub category: String,
    pub title: String,
    pub description: String,
    pub kind: String,
    pub apply: String,
    pub visibility: String,
    pub requires_write: bool,
}

/// Result of `settings.set`.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsSetResult {
    /// The new generation.
    pub generation: u64,
}

/// Result of `settings.reset`.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsResetResult {
    /// The new generation.
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// Command handlers
// ---------------------------------------------------------------------------

/// Handle `settings.list` command.
pub fn handle_settings_list(
    flavite: &Flavite,
    _args: SettingsListArgs,
) -> Result<(String, serde_json::Value), String> {
    use flavite_settings::resolve::RegistryResolve;
    
    let registry = SettingRegistry::with_builtins();
    
    // Load or initialize settings
    let stored = flavite.settings_store.load_or_initialize(&registry)
        .map_err(|e| format!("failed to load settings: {}", e.wire_code()))?;
    
    let resolved = registry.resolve(&stored.doc);
    
    let settings: Vec<SettingEntry> = registry.all()
        .iter()
        .map(|descriptor| {
            let id = descriptor.id.to_owned();
            let value = resolved.get(&id).cloned().unwrap_or_else(|| descriptor.default.clone());
            let default = descriptor.default.clone();
            let modified = value != default;
            
            SettingEntry {
                id,
                category: descriptor.category.to_owned(),
                title: descriptor.title.to_owned(),
                description: descriptor.description.to_owned(),
                value: setting_value_to_json(&value),
                default: setting_value_to_json(&default),
                modified,
                apply: format!("{:?}", descriptor.apply),
                requires_write: descriptor.requires_write,
            }
        })
        .collect();
    
    let result = SettingsListResult {
        settings,
        generation: stored.generation,
        read_only: stored.is_read_only(),
        warnings: stored.warnings,
    };
    
    Ok(("settings.list".to_owned(), serde_json::to_value(result).unwrap()))
}

/// Handle `settings.get` command.
pub fn handle_settings_get(
    flavite: &Flavite,
    args: SettingsGetArgs,
) -> Result<(String, serde_json::Value), String> {
    use flavite_settings::resolve::RegistryResolve;
    
    let registry = SettingRegistry::with_builtins();
    
    // Validate the ID
    if SettingRegistry::is_reserved_namespace(&args.id) {
        return Err(format!("reserved namespace: {}", args.id));
    }
    
    let descriptor = registry.lookup(&args.id)
        .ok_or_else(|| format!("unknown setting: {}", args.id))?;
    
    // Load or initialize settings
    let stored = flavite.settings_store.load_or_initialize(&registry)
        .map_err(|e| format!("failed to load settings: {}", e.wire_code()))?;
    
    let resolved = registry.resolve(&stored.doc);
    let value = resolved.get(&args.id).cloned().unwrap_or_else(|| descriptor.default.clone());
    
    let result = SettingsGetResult {
        id: args.id,
        value: setting_value_to_json(&value),
        default: setting_value_to_json(&descriptor.default),
        descriptor: SettingDescriptorResult {
            category: descriptor.category.to_owned(),
            title: descriptor.title.to_owned(),
            description: descriptor.description.to_owned(),
            kind: format!("{:?}", descriptor.kind),
            apply: format!("{:?}", descriptor.apply),
            visibility: format!("{:?}", descriptor.visibility),
            requires_write: descriptor.requires_write,
        },
    };
    
    Ok(("settings.get".to_owned(), serde_json::to_value(result).unwrap()))
}

/// Handle `settings.set` command.
pub fn handle_settings_set(
    flavite: &Flavite,
    args: SettingsSetArgs,
) -> Result<(String, serde_json::Value), String> {
    let registry = SettingRegistry::with_builtins();
    
    // Validate the ID
    if SettingRegistry::is_reserved_namespace(&args.id) {
        return Err(format!("reserved namespace: {}", args.id));
    }
    
    let descriptor = registry.lookup(&args.id)
        .ok_or_else(|| format!("unknown setting: {}", args.id))?;
    
    // Convert JSON value to SettingValue
    let value = json_to_setting_value(&args.value, &descriptor.kind)
        .map_err(|e| format!("invalid value: {}", e))?;
    
    // Validate the value
    descriptor.validate_value(&value)
        .map_err(|e| format!("validation failed: {}", e))?;
    
    // Write the value
    let generation = flavite.settings_store.set_value(&registry, &args.id, value)
        .map_err(|e| format!("failed to set setting: {}", e.wire_code()))?;
    
    let result = SettingsSetResult { generation };
    
    Ok(("settings.set".to_owned(), serde_json::to_value(result).unwrap()))
}

/// Handle `settings.reset` command.
pub fn handle_settings_reset(
    flavite: &Flavite,
    args: SettingsResetArgs,
) -> Result<(String, serde_json::Value), String> {
    let registry = SettingRegistry::with_builtins();
    
    // Validate the ID
    if SettingRegistry::is_reserved_namespace(&args.id) {
        return Err(format!("reserved namespace: {}", args.id));
    }
    
    let descriptor = registry.lookup(&args.id)
        .ok_or_else(|| format!("unknown setting: {}", args.id))?;
    
    // Write the default value
    let generation = flavite.settings_store.set_value(&registry, &args.id, descriptor.default.clone())
        .map_err(|e| format!("failed to reset setting: {}", e.wire_code()))?;
    
    let result = SettingsResetResult { generation };
    
    Ok(("settings.reset".to_owned(), serde_json::to_value(result).unwrap()))
}

/// Stub handler for settings commands (they're routed on the facade turn).
/// The registry still owns their schemas and capabilities.
pub(crate) fn handle_settings_unroutable(
    _session: &flavite_vault::session::Session,
    _invocation: &crate::command::Invocation,
) -> Result<flavite_vault::session::SessionResult, String> {
    Err("settings commands are routed on the facade turn".into())
}

// ---------------------------------------------------------------------------
// Command execution
// ---------------------------------------------------------------------------

/// The facade-turn handler for `settings.*` commands.
///
/// This is called from Flavite::execute_turn after the registry's argument
/// validation and the capability check, before any buffer precondition work
/// (settings never touch buffers).
pub(crate) fn execute_settings_command(
    flavite: &Flavite,
    invocation: &Invocation,
) -> ExecutionResult {
    use crate::result::{LocalResult, LocalSuccess};
    
    let outcome = match invocation.command_id {
        "settings.list" => {
            let Ok(args) = serde_json::from_value::<SettingsListArgs>(invocation.args.clone())
            else {
                return rejected(invocation, "invalid arguments");
            };
            handle_settings_list(flavite, args)
        }
        "settings.get" => {
            let Ok(args) = serde_json::from_value::<SettingsGetArgs>(invocation.args.clone())
            else {
                return rejected(invocation, "invalid arguments");
            };
            handle_settings_get(flavite, args)
        }
        "settings.set" => {
            let Ok(args) = serde_json::from_value::<SettingsSetArgs>(invocation.args.clone())
            else {
                return rejected(invocation, "invalid arguments");
            };
            handle_settings_set(flavite, args)
        }
        "settings.reset" => {
            let Ok(args) = serde_json::from_value::<SettingsResetArgs>(invocation.args.clone())
            else {
                return rejected(invocation, "invalid arguments");
            };
            handle_settings_reset(flavite, args)
        }
        _ => return rejected(invocation, "unknown settings command"),
    };
    
    match outcome {
        Ok((command_id, data)) => ExecutionResult::Local(LocalResult::Success(
            LocalSuccess::with_command(invocation.request_id, command_id, data),
        )),
        Err(error) => rejected(invocation, &error),
    }
}

fn rejected(invocation: &Invocation, message: &str) -> ExecutionResult {
    use crate::result::{LocalError, LocalResult};
    ExecutionResult::Local(LocalResult::Error(LocalError::with_request(
        "settings_invalid_args",
        message.to_owned(),
        invocation.request_id,
    )))
}

// ---------------------------------------------------------------------------
// Value conversion utilities
// ---------------------------------------------------------------------------

/// Convert a SettingValue to a JSON value.
fn setting_value_to_json(value: &SettingValue) -> serde_json::Value {
    match value {
        SettingValue::Bool(v) => serde_json::Value::Bool(*v),
        SettingValue::Int(v) => serde_json::Value::from(*v),
        SettingValue::Str(v) => serde_json::Value::String(v.clone()),
        SettingValue::List(v) => serde_json::Value::Array(v.iter().map(|s| serde_json::Value::String(s.clone())).collect()),
        SettingValue::ShortcutMap(map) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in map {
                obj.insert(k.clone(), serde_json::Value::String(v.clone()));
            }
            serde_json::Value::Object(obj)
        }
    }
}

/// Convert a JSON value to a SettingValue based on the expected kind.
fn json_to_setting_value(
    value: &serde_json::Value,
    kind: &flavite_settings::descriptor::SettingKind,
) -> Result<SettingValue, String> {
    use flavite_settings::descriptor::SettingKind;
    
    match kind {
        SettingKind::Bool => {
            value.as_bool().map(SettingValue::Bool).ok_or_else(|| "expected boolean".to_owned())
        }
        SettingKind::Int { min, max } => {
            value.as_i64().ok_or_else(|| "expected integer".to_owned())
                .and_then(|v| {
                    if v >= *min && v <= *max {
                        Ok(SettingValue::Int(v))
                    } else {
                        Err(format!("value must be between {min} and {max}"))
                    }
                })
        }
        SettingKind::Text => {
            value.as_str().map(|s| SettingValue::Str(s.to_owned())).ok_or_else(|| "expected string".to_owned())
        }
        SettingKind::Choice(options) => {
            value.as_str().ok_or_else(|| "expected string".to_owned())
                .and_then(|s| {
                    if options.iter().any(|(val, _)| *val == s) {
                        Ok(SettingValue::Str(s.to_owned()))
                    } else {
                        let valid: Vec<_> = options.iter().map(|(val, _)| *val).collect();
                        Err(format!("value must be one of {:?}", valid))
                    }
                })
        }
        SettingKind::LineWidth => {
            if let Some(s) = value.as_str()
                && s == "full" {
                return Ok(SettingValue::Str("full".to_owned()));
            }
            value.as_i64().ok_or_else(|| "expected integer or 'full'".to_owned())
                .and_then(|v| {
                    if (40..=160).contains(&v) {
                        Ok(SettingValue::Int(v))
                    } else {
                        Err("value must be between 40 and 160 or 'full'".to_owned())
                    }
                })
        }
        SettingKind::ThemePackage => {
            value.as_str().map(|s| SettingValue::Str(s.to_owned())).ok_or_else(|| "expected string".to_owned())
        }
        SettingKind::StringList => {
            value.as_array().ok_or_else(|| "expected array".to_owned())
                .and_then(|arr| {
                    arr.iter().map(|v| v.as_str().map(|s| s.to_owned()).ok_or_else(|| "expected string in list".to_owned()))
                        .collect::<Result<Vec<String>, _>>()
                })
                .map(SettingValue::List)
        }
        SettingKind::ShortcutMap => {
            value.as_object().ok_or_else(|| "expected object".to_owned())
                .and_then(|obj| {
                    let mut map = BTreeMap::new();
                    for (k, v) in obj {
                        let chord = v.as_str().ok_or_else(|| format!("expected string chord for {k}"))?.to_owned();
                        map.insert(k.clone(), chord);
                    }
                    Ok(SettingValue::ShortcutMap(map))
                })
        }
    }
}
