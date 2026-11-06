//! Theme manifest parsing (`manifest.json` of a WebExtension theme).
//!
//! The parser is deliberately permissive at the field level and strict at
//! the document level: an invalid color or property becomes a warning plus
//! a dropped key (the conversion report records it), while a document that
//! is not a JSON object, or a `theme` key without any colors, is a hard
//! error — there is nothing to theme from. Unknown fields are preserved
//! verbatim so a `.flavite.xpi` can re-emit the manifest losslessly.

use crate::ThemeError;
use crate::colors::Rgba;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// The archive path every WebExtension manifest must live at.
pub const MANIFEST_NAME: &str = "manifest.json";

/// A parsed theme manifest.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeManifest {
    /// The theme's display name.
    pub name: String,
    /// The package version string, when present.
    pub version: Option<String>,
    /// The add-on identity (`browser_specific_settings.gecko.id` or the
    /// legacy `applications.gecko.id`), when present.
    pub id: Option<String>,
    /// The declared manifest version (2 or 3).
    pub manifest_version: Option<u64>,
    /// The base theme object (light scheme when a dark variant exists).
    pub theme: ThemeObject,
    /// The `dark_theme` variant, when present.
    pub dark_theme: Option<ThemeObject>,
    /// The original manifest object, preserved for lossless re-emission.
    pub raw: Map<String, Value>,
}

/// One theme object (`theme` or `dark_theme`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ThemeObject {
    /// Image references keyed by their standard names (`theme_frame`,
    /// `additional_backgrounds`). Gradients are kept as their CSS text so
    /// the report can drop them with a precise reason.
    pub images: BTreeMap<String, ImageRef>,
    /// Validated colors keyed by the standard browser names. Unparseable
    /// entries are dropped with a warning before this struct exists.
    pub colors: BTreeMap<String, Vec<Rgba>>,
    /// Property values keyed verbatim (alignment, tiling, size, schemes).
    pub properties: BTreeMap<String, Value>,
    /// Known structured keys the mapping cannot use, recorded with their
    /// reasons (e.g. Chrome `tints`).
    pub skipped: Vec<(String, String)>,
    /// Unknown keys inside this theme object, preserved verbatim.
    pub unknown: Map<String, Value>,
}

/// A reference to one theme image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageRef {
    /// A packaged image file path, relative to the manifest.
    Path(String),
    /// A CSS gradient object (`{"linear-gradient": "…"}`, Firefox 153+).
    Gradient(String),
    /// The Firefox `additional_backgrounds` form: an array of layered
    /// references. Recorded structurally so the report can drop it with
    /// its real reason instead of an "unreadable" warning.
    Multiple(Vec<ImageRef>),
}

impl ImageRef {
    /// The packaged file path, `None` for gradients and layered lists.
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        match self {
            Self::Path(path) => Some(path),
            Self::Gradient(_) | Self::Multiple(_) => None,
        }
    }

    /// The gradient CSS text, `None` for file paths.
    #[must_use]
    pub fn gradient(&self) -> Option<&str> {
        match self {
            Self::Path(_) | Self::Multiple(_) => None,
            Self::Gradient(text) => Some(text),
        }
    }
}

/// Parse a manifest document. Returns the manifest plus the field-level
/// warnings accumulated while parsing (invalid colors, unreadable
/// properties).
///
/// # Errors
/// [`ThemeError::InvalidManifest`] when the document is not a JSON object,
/// [`ThemeError::NotATheme`] when the `theme` key is missing or carries no
/// colors at all.
pub fn parse_manifest(text: &str) -> Result<(ThemeManifest, Vec<String>), ThemeError> {
    let value: Value = serde_json::from_str(text)
        .map_err(|error| ThemeError::InvalidManifest(error.to_string()))?;
    let Value::Object(raw) = value else {
        return Err(ThemeError::InvalidManifest(
            "manifest must be a JSON object".into(),
        ));
    };
    let theme_value = raw
        .get("theme")
        .ok_or_else(|| ThemeError::NotATheme("manifest has no theme key".into()))?;
    let mut warnings = Vec::new();
    let theme = parse_theme_object(theme_value, &mut warnings)
        .ok_or_else(|| ThemeError::NotATheme("the theme key carries no colors".into()))?;
    let dark_theme = match raw.get("dark_theme") {
        Some(value) => parse_theme_object(value, &mut warnings),
        None => None,
    };
    let name = raw
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("Unnamed theme")
        .to_owned();
    Ok((
        ThemeManifest {
            name,
            version: raw
                .get("version")
                .and_then(Value::as_str)
                .map(str::to_owned),
            id: gecko_id(&raw),
            manifest_version: raw.get("manifest_version").and_then(Value::as_u64),
            theme,
            dark_theme,
            raw,
        },
        warnings,
    ))
}

fn gecko_id(raw: &Map<String, Value>) -> Option<String> {
    for container in ["browser_specific_settings", "applications"] {
        if let Some(id) = raw
            .get(container)
            .and_then(|settings| settings.get("gecko"))
            .and_then(|gecko| gecko.get("id"))
            .and_then(Value::as_str)
        {
            return Some(id.to_owned());
        }
    }
    None
}

fn parse_theme_object(value: &Value, warnings: &mut Vec<String>) -> Option<ThemeObject> {
    let Value::Object(map) = value else {
        warnings.push("theme is not a JSON object; it was ignored".into());
        return None;
    };
    let colors_value = map.get("colors")?;
    let Value::Object(colors_map) = colors_value else {
        warnings.push("theme.colors is not a JSON object; it was ignored".into());
        return None;
    };
    let mut colors = BTreeMap::new();
    let mut skipped = Vec::new();
    for (key, entry) in colors_map {
        // `tints` is Chrome's structured HSL concept, not a color: record
        // it with its real reason instead of a misleading color warning.
        if key == "tints" && entry.is_object() {
            skipped.push((
                "tints".to_owned(),
                "Chrome-only HSL tints have no flavite surface".to_owned(),
            ));
            continue;
        }
        match crate::colors::parse_color_value(entry) {
            Some(parsed) => {
                colors.insert(key.clone(), parsed);
            }
            None => warnings.push(format!(
                "theme color {key:?} is not a readable CSS color or RGB array; it was dropped"
            )),
        }
    }
    if colors.is_empty() {
        return None;
    }
    let mut images = BTreeMap::new();
    if let Some(Value::Object(images_map)) = map.get("images") {
        for (key, entry) in images_map {
            match entry {
                Value::String(path) => {
                    images.insert(key.clone(), ImageRef::Path(path.clone()));
                }
                Value::Object(object) => {
                    // Firefox 153+: `{"linear-gradient": "…"}` objects.
                    if let Some((kind, text)) = object.iter().next() {
                        let text = text.as_str().unwrap_or_default();
                        images.insert(key.clone(), ImageRef::Gradient(format!("{kind}: {text}")));
                    }
                }
                Value::Array(layers) => {
                    // The Firefox `additional_backgrounds` form: layered
                    // references, parsed structurally for the report.
                    let parsed = layers
                        .iter()
                        .filter_map(|layer| match layer {
                            Value::String(path) => Some(ImageRef::Path(path.clone())),
                            Value::Object(object) => object.iter().next().map(|(kind, text)| {
                                ImageRef::Gradient(format!(
                                    "{kind}: {}",
                                    text.as_str().unwrap_or_default()
                                ))
                            }),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    images.insert(key.clone(), ImageRef::Multiple(parsed));
                }
                _ => warnings.push(format!("theme image {key:?} was ignored (unreadable)")),
            }
        }
    }
    let mut properties = BTreeMap::new();
    if let Some(Value::Object(properties_map)) = map.get("properties") {
        for (key, entry) in properties_map {
            properties.insert(key.clone(), entry.clone());
        }
    }
    let known = ["images", "colors", "properties"];
    let unknown: Map<String, Value> = map
        .iter()
        .filter(|(key, _)| !known.contains(&key.as_str()))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    Some(ThemeObject {
        images,
        colors,
        properties,
        skipped,
        unknown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r##"{
        "manifest_version": 2,
        "name": "Purple Starfield",
        "version": "1.0",
        "theme": {
            "images": { "theme_frame": "header.png" },
            "colors": { "frame": "#1a0f38", "tab_background_text": "#e8e2ff" }
        }
    }"##;

    #[test]
    fn minimal_manifest_parses() {
        let (manifest, warnings) = parse_manifest(MINIMAL).unwrap();
        assert_eq!(manifest.name, "Purple Starfield");
        assert_eq!(manifest.version.as_deref(), Some("1.0"));
        assert!(manifest.dark_theme.is_none());
        assert!(warnings.is_empty());
        assert_eq!(
            manifest.theme.images.get("theme_frame"),
            Some(&ImageRef::Path("header.png".into()))
        );
        assert_eq!(
            manifest.theme.colors.get("frame").map(Vec::as_slice),
            Some(&[Rgba::rgb(0x1a, 0x0f, 0x38)][..])
        );
        // Raw manifest is preserved for lossless re-emission.
        assert_eq!(
            manifest.raw.get("name").and_then(Value::as_str),
            Some("Purple Starfield")
        );
    }

    #[test]
    fn gecko_identity_is_found_in_both_containers() {
        let modern = r##"{"name":"t","theme":{"colors":{"frame":"#000"}},
            "browser_specific_settings":{"gecko":{"id":"theme@host"}}}"##;
        let (manifest, _) = parse_manifest(modern).unwrap();
        assert_eq!(manifest.id.as_deref(), Some("theme@host"));
        let legacy = r##"{"name":"t","theme":{"colors":{"frame":"#000"}},
            "applications":{"gecko":{"id":"legacy@host"}}}"##;
        let (manifest, _) = parse_manifest(legacy).unwrap();
        assert_eq!(manifest.id.as_deref(), Some("legacy@host"));
    }

    #[test]
    fn dark_theme_variant_parses() {
        let text = r##"{"name":"t","theme":{"colors":{"frame":"#fff"}},
            "dark_theme":{"colors":{"frame":"#000"}}}"##;
        let (manifest, _) = parse_manifest(text).unwrap();
        let dark = manifest.dark_theme.expect("dark variant");
        assert_eq!(
            dark.colors.get("frame").map(Vec::as_slice),
            Some(&[Rgba::rgb(0, 0, 0)][..])
        );
    }

    #[test]
    fn unreadable_colors_become_warnings_not_errors() {
        let text = r##"{"name":"t","theme":{"colors":{"frame":"#000",
            "toolbar":"not-a-color","popup":42}}}"##;
        let (manifest, warnings) = parse_manifest(text).unwrap();
        assert!(manifest.theme.colors.contains_key("frame"));
        assert!(!manifest.theme.colors.contains_key("toolbar"));
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn gradient_image_objects_are_captured() {
        let text = r##"{"name":"t","theme":{"images":{"theme_frame":
            {"linear-gradient":"to bottom, #a, #b"}},
            "colors":{"frame":"#000"}}}"##;
        let (manifest, _) = parse_manifest(text).unwrap();
        assert!(matches!(
            manifest.theme.images.get("theme_frame"),
            Some(ImageRef::Gradient(_))
        ));
    }

    #[test]
    fn non_theme_documents_are_typed_errors() {
        assert_eq!(
            parse_manifest("{}"),
            Err(ThemeError::NotATheme("manifest has no theme key".into()))
        );
        assert!(matches!(
            parse_manifest(r##"{"name":"t","theme":{"images":{}}}"##),
            Err(ThemeError::NotATheme(_))
        ));
        assert!(matches!(
            parse_manifest("not json"),
            Err(ThemeError::InvalidManifest(_))
        ));
        assert!(matches!(
            parse_manifest("[1,2]"),
            Err(ThemeError::InvalidManifest(_))
        ));
    }

    #[test]
    fn layered_image_arrays_parse_structurally() {
        let text = r##"{"name":"t","theme":{"images":{"additional_backgrounds":
            ["images/0.png", "images/1.png", {"linear-gradient": "to top, #a, #b"}]},
            "colors":{"frame":"#000"}}}"##;
        let (manifest, warnings) = parse_manifest(text).unwrap();
        assert!(warnings.is_empty());
        match manifest.theme.images.get("additional_backgrounds") {
            Some(ImageRef::Multiple(layers)) => assert_eq!(layers.len(), 3),
            other => panic!("expected a layered reference, got {other:?}"),
        }
    }

    #[test]
    fn tints_are_recorded_with_their_reason_not_a_color_warning() {
        let text = r##"{"name":"t","theme":{"colors":{"frame":"#000",
            "tints":{"buttons":[0.33, 0.5, 0.47]}}}}"##;
        let (manifest, warnings) = parse_manifest(text).unwrap();
        assert!(!manifest.theme.colors.contains_key("tints"));
        assert!(warnings.is_empty(), "tints must not look like a bad color");
        assert_eq!(
            manifest.theme.skipped,
            vec![(
                "tints".to_owned(),
                "Chrome-only HSL tints have no flavite surface".to_owned()
            )]
        );
    }

    #[test]
    fn unknown_theme_keys_are_preserved() {
        let text = r##"{"name":"t","theme":{"colors":{"frame":"#000"},
            "theme_experiment":{"colors":{"--x":"frame"}}}}"##;
        let (manifest, _) = parse_manifest(text).unwrap();
        assert!(manifest.theme.unknown.contains_key("theme_experiment"));
    }
}
