//! Versioned vault settings with forward-compatible unknown-field preservation.

use std::collections::BTreeMap;

const SETTINGS_VERSION: i64 = 1;

/// Link behavior for in-app note renames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RenameBehavior {
    /// Rewrite inbound links (the shipped default).
    #[default]
    Rewrite,
    /// Add the old name as an alias.
    Alias,
    /// Leave inbound links unchanged.
    None,
}

/// User-overridable settings stored in `.flavite/settings.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// User state schema version.
    pub version: i64,
    /// Rename policy (default: rewrite).
    pub rename_behavior: RenameBehavior,
    /// Dirty-buffer recovery interval (default: 2 seconds).
    pub recovery_interval_secs: u64,
    /// User ignore globs.
    pub ignore_globs: Vec<String>,
    /// Content verification interval (default: 10 minutes).
    pub verification_interval_mins: u64,
    /// Change-log budget (default: 64 MiB).
    pub change_log_budget_mib: u64,
    /// Unknown fields retained when loading and saving newer settings files.
    unknown: BTreeMap<String, toml::Value>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: SETTINGS_VERSION,
            rename_behavior: RenameBehavior::Rewrite,
            recovery_interval_secs: 2,
            ignore_globs: Vec::new(),
            verification_interval_mins: 10,
            change_log_budget_mib: 64,
            unknown: BTreeMap::new(),
        }
    }
}

impl Settings {
    /// Parse a settings document, using defaults for missing or invalid fields.
    pub fn parse(contents: Option<&str>) -> Result<Self, String> {
        let Some(contents) = contents else {
            return Ok(Self::default());
        };
        let value: toml::Value = toml::from_str(contents).map_err(|error| error.to_string())?;
        let table = value
            .as_table()
            .ok_or_else(|| "settings root must be a TOML table".to_string())?;
        let version = table
            .get("version")
            .and_then(toml::Value::as_integer)
            .unwrap_or(SETTINGS_VERSION);
        let rename_behavior = match table
            .get("rename_behavior")
            .and_then(toml::Value::as_str)
            .unwrap_or("rewrite")
        {
            "alias" => RenameBehavior::Alias,
            "none" => RenameBehavior::None,
            _ => RenameBehavior::Rewrite,
        };
        let recovery_interval_secs = positive_integer(table, "recovery_interval_secs", 2);
        let verification_interval_mins = positive_integer(table, "verification_interval_mins", 10);
        let change_log_budget_mib = positive_integer(table, "change_log_budget_mib", 64);
        let ignore_globs = table
            .get("ignore_globs")
            .and_then(toml::Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let unknown = table
            .iter()
            .filter(|(key, _)| {
                !matches!(
                    key.as_str(),
                    "version"
                        | "rename_behavior"
                        | "recovery_interval_secs"
                        | "verification_interval_mins"
                        | "change_log_budget_mib"
                        | "ignore_globs"
                )
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        Ok(Self {
            version,
            rename_behavior,
            recovery_interval_secs,
            ignore_globs,
            verification_interval_mins,
            change_log_budget_mib,
            unknown,
        })
    }

    /// Serialize known settings together with every unrecognized top-level field.
    pub fn to_toml(&self) -> Result<String, String> {
        let mut table = toml::map::Map::new();
        table.extend(self.unknown.clone());
        table.insert("version".into(), toml::Value::Integer(self.version));
        table.insert(
            "rename_behavior".into(),
            toml::Value::String(
                match self.rename_behavior {
                    RenameBehavior::Rewrite => "rewrite",
                    RenameBehavior::Alias => "alias",
                    RenameBehavior::None => "none",
                }
                .into(),
            ),
        );
        table.insert(
            "recovery_interval_secs".into(),
            toml::Value::Integer(self.recovery_interval_secs as i64),
        );
        table.insert(
            "verification_interval_mins".into(),
            toml::Value::Integer(self.verification_interval_mins as i64),
        );
        table.insert(
            "change_log_budget_mib".into(),
            toml::Value::Integer(self.change_log_budget_mib as i64),
        );
        table.insert(
            "ignore_globs".into(),
            toml::Value::Array(
                self.ignore_globs
                    .iter()
                    .cloned()
                    .map(toml::Value::String)
                    .collect(),
            ),
        );
        toml::to_string(&toml::Value::Table(table)).map_err(|error| error.to_string())
    }
}

fn positive_integer(table: &toml::Table, key: &str, default: u64) -> u64 {
    table
        .get(key)
        .and_then(toml::Value::as_integer)
        .and_then(|value| u64::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_product_contract() {
        let settings = Settings::default();
        assert_eq!(settings.rename_behavior, RenameBehavior::Rewrite);
        assert_eq!(settings.recovery_interval_secs, 2);
        assert_eq!(settings.verification_interval_mins, 10);
        assert_eq!(settings.change_log_budget_mib, 64);
    }

    #[test]
    fn round_trip_preserves_unknown_fields() {
        let mut settings = Settings::parse(Some(
            "version = 4\nrename_behavior = 'alias'\nfuture_option = { enabled = true }\n",
        ))
        .unwrap();
        settings.recovery_interval_secs = 5;
        let saved = settings.to_toml().unwrap();
        let parsed = Settings::parse(Some(&saved)).unwrap();
        assert_eq!(parsed.rename_behavior, RenameBehavior::Alias);
        assert_eq!(parsed.recovery_interval_secs, 5);
        assert!(saved.contains("future_option"));
        assert!(saved.contains("enabled = true"));
    }
}
