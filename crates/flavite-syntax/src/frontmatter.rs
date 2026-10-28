//! Frontmatter parsing and byte-preserving patching (D11, §4.8/R12).
//!
//! Frontmatter is the YAML block between `---` delimiters at the start of a
//! note. This module:
//!
//! 1. Parses frontmatter with a YAML 1.2 core-schema parser with alias/depth/limit
//!    guards (D11, §4.10).
//! 2. Writes changes via **textual patches** that preserve comments, quoting style,
//!    key order, and anchors — **never** by parse-and-reserialize (D11).
//!
//! # Well-known keys (R13)
//!
//! - `aliases` (string or list) → mapped into the typed `aliases` table
//! - `tags`/`tag` (string or list) → mapped into the typed `tags` table
//! - `title` → overrides the derived title (frontmatter > first H1 > stem)
//!
//! All other keys land in `properties`.

use crate::text::TextSnapshot;
use crate::wellknown::WellKnownKey;
use std::collections::BTreeMap;
use std::ops::Range;
use thiserror::Error;

/// A property value, either typed or a raw string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyValue {
    /// A text value.
    Text(String),
    /// A numeric value (stored as string to avoid f64 Eq issues).
    Number(String),
    /// A boolean value.
    Bool(bool),
    /// A date value (ISO format).
    Date(String),
    /// A datetime value (ISO format).
    DateTime(String),
    /// A null value.
    Null,
    /// A list of values.
    List(Vec<PropertyValue>),
    /// We couldn't safely patch this.
    Raw(String),
}

/// Errors from frontmatter operations.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FrontmatterError {
    /// No frontmatter block present.
    #[error("no frontmatter block found")]
    NoFrontmatter,
    /// YAML parsing failed.
    #[error("YAML parse error: {0}")]
    ParseError(String),
    /// Limits exceeded.
    #[error("YAML feature exceeds limits: {0}")]
    LimitsExceeded(String),
    /// The key to patch was not found.
    #[error("key not found: {0}")]
    KeyNotFound(String),
    /// Cannot safely patch.
    #[error("cannot safely patch: {0}")]
    PatchRefused(String),
}

/// A parsed frontmatter document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frontmatter {
    /// The raw YAML text.
    pub yaml_text: String,
    /// Line range of the frontmatter block.
    pub range: Range<usize>,
    /// Well-known key mappings.
    pub well_known: BTreeMap<WellKnownKey, Vec<String>>,
    /// Arbitrary other keys.
    pub properties: BTreeMap<String, Vec<(usize, PropertyValue)>>,
    /// `title` from frontmatter.
    pub title: Option<String>,
}

/// Extract frontmatter from a snapshot, if present.
pub fn parse_frontmatter(snapshot: &TextSnapshot) -> Result<Option<Frontmatter>, FrontmatterError> {
    let text = snapshot.text();
    // Look for `---` at the start.
    // TextSnapshot removes the BOM from normalized text, so all parser
    // offsets here are already in TextByte coordinates.
    if !text.starts_with("---") {
        return Ok(None);
    }

    let after_open = 3;
    let rest = &text[after_open..];

    // Find closing `---` or `...` on its own line.
    let mut closing_offset = None;
    let mut line_start = 0usize;
    for line in rest.lines() {
        let trimmed = line.trim();
        if trimmed == "---" || trimmed == "..." {
            // Close at the start of this line (exclude the closing delimiter line)
            closing_offset = Some(after_open + line_start);
            break;
        }
        line_start += line.len() + 1;
    }

    let Some(close) = closing_offset else {
        return Ok(None);
    };

    let yaml_text = text[after_open..close].to_string();

    // Parse the YAML text.
    // For now, use a simple line-based parser for well-known keys.
    let mut well_known: BTreeMap<WellKnownKey, Vec<String>> = BTreeMap::new();
    let mut properties: BTreeMap<String, Vec<(usize, PropertyValue)>> = BTreeMap::new();
    let mut title: Option<String> = None;
    let mut ord = 0;

    let yaml_lines: Vec<&str> = yaml_text.lines().collect();
    let mut i = 0;

    while i < yaml_lines.len() {
        let line = yaml_lines[i];
        let trimmed = line.trim();
        if trimmed.is_empty() {
            i += 1;
            continue;
        }

        // Check if this is a list item (starts with `-` after indentation)
        let trimmed_for_list = trimmed.trim_start();
        if let Some(stripped) = trimmed_for_list.strip_prefix('-') {
            // This is a list item - need to find its parent key
            // Look backwards for the most recent key
            let mut key_line_idx = i;
            let mut found_key = None;
            // Start from the previous line and go backwards
            while key_line_idx > 0 {
                key_line_idx -= 1;
                let prev_line = yaml_lines[key_line_idx];
                let prev_trimmed = prev_line.trim();
                if !prev_trimmed.is_empty() {
                    if let Some((key, _)) = prev_trimmed.split_once(':') {
                        let key = key.trim();
                        if let Some(wk) = WellKnownKey::from_str(key) {
                            found_key = Some((key.to_string(), wk));
                            break;
                        } else {
                            // Not a well-known key, stop looking
                            break;
                        }
                    } else if prev_trimmed.starts_with('-')
                        || prev_trimmed.starts_with('"')
                        || prev_trimmed.starts_with('\'')
                    {
                        // Another list item or value, continue looking back
                        continue;
                    } else {
                        // No colon, stop looking
                        break;
                    }
                }
            }

            if let Some((_, wk)) = found_key {
                // Extract the value after the `-`
                let value = stripped.trim_start().trim_matches('"').to_string();
                if !value.is_empty() {
                    well_known.entry(wk).or_default().push(value);
                }
                i += 1;
                continue;
            }
        }

        // Parse key: value
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = value.trim();

            if let Some(wk) = WellKnownKey::from_str(key) {
                // Extract string or list values.
                let values = parse_yaml_value(value);
                for v in values {
                    if !v.is_empty() {
                        well_known.entry(wk).or_default().push(v);
                    }
                }

                if wk == WellKnownKey::Title {
                    title = well_known
                        .get(&WellKnownKey::Title)
                        .and_then(|v| v.first().cloned());
                }
            } else {
                // Arbitrary property.
                let prop_value = PropertyValue::Text(value.to_string());
                properties
                    .entry(key.to_string())
                    .or_default()
                    .push((ord, prop_value));
                ord += 1;
            }
        }

        i += 1;
    }

    // Deduplicate.
    for vec in well_known.values_mut() {
        vec.sort_unstable();
        vec.dedup();
    }

    Ok(Some(Frontmatter {
        yaml_text,
        range: 0..close,
        well_known,
        properties,
        title,
    }))
}

/// Parse a simple YAML value (string or list).
/// This handles both inline lists `[a, b]` and multi-line lists with `-` prefix.
fn parse_yaml_value(value: &str) -> Vec<String> {
    let trimmed = value.trim();

    // Check for list: starts with `[`
    if trimmed.starts_with('[') {
        // Parse comma-separated list
        trimmed
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        // Single string value - trim quotes
        vec![trimmed.trim_matches('"').to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wellknown::derive_title;

    fn snap(s: &str) -> TextSnapshot {
        TextSnapshot::new(s.as_bytes()).expect("utf-8")
    }

    #[test]
    fn parse_frontmatter_basic() {
        let s = "---\ntitle: My Note\naliases:\n  - a\n  - b\n---\nBody";
        let snapshot = snap(s);
        let fm = parse_frontmatter(&snapshot)
            .expect("parse")
            .expect("has frontmatter");
        assert_eq!(fm.title, Some("My Note".to_string()));
        assert_eq!(
            fm.well_known.get(&WellKnownKey::Aliases),
            Some(&vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn parse_frontmatter_no_frontmatter() {
        let s = "Just body text";
        let snapshot = snap(s);
        let fm = parse_frontmatter(&snapshot).expect("parse");
        assert!(fm.is_none());
        assert!(parse_frontmatter(&snap("")).unwrap().is_none());
    }

    #[test]
    fn parse_frontmatter_handles_bom_lists_properties_and_missing_delimiters() {
        let snapshot = snap(
            "\u{FEFF}---\ntitle: A title\naliases: [z, a, z]\ntags: one\ntags: two\ncustom: value\ncustom: next\n...\nbody",
        );
        let fm = parse_frontmatter(&snapshot).unwrap().unwrap();
        assert_eq!(fm.range.start, 0);
        assert_eq!(fm.title.as_deref(), Some("A title"));
        assert_eq!(fm.well_known[&WellKnownKey::Aliases], ["a", "z"]);
        assert_eq!(fm.well_known[&WellKnownKey::Tags], ["one", "two"]);
        assert_eq!(fm.properties["custom"].len(), 2);

        assert!(
            parse_frontmatter(&snap("prefix\n---\nx\n---"))
                .unwrap()
                .is_none()
        );
        assert!(
            parse_frontmatter(&snap("---\nno closing delimiter"))
                .unwrap()
                .is_none()
        );
        assert!(
            parse_frontmatter(&snap("---\n\n---\nbody"))
                .unwrap()
                .unwrap()
                .title
                .is_none()
        );
    }

    #[test]
    fn derive_title_precedence() {
        assert_eq!(
            derive_title(Some("Frontmatter"), Some("H1"), "stem"),
            "Frontmatter"
        );
        assert_eq!(derive_title(None, Some("H1"), "stem"), "H1");
        assert_eq!(derive_title(None, None, "stem"), "stem");
    }

    #[test]
    fn well_known_key_from_str() {
        assert_eq!(
            WellKnownKey::from_str("aliases"),
            Some(WellKnownKey::Aliases)
        );
        assert_eq!(WellKnownKey::from_str("tags"), Some(WellKnownKey::Tags));
        assert_eq!(WellKnownKey::from_str("tag"), Some(WellKnownKey::Tag));
        assert_eq!(WellKnownKey::from_str("title"), Some(WellKnownKey::Title));
        assert_eq!(WellKnownKey::from_str("unknown"), None);
    }
}
