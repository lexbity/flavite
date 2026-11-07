//! The flavite semantic surface and the browser-key mapping.
//!
//! A browser theme speaks ~42 surface-specific color keys; flavite speaks
//! a semantic palette (window, top bar, editor, menus, accent…). Mapping
//! is an explicit, documented table: every key is either mapped onto a
//! slot, or dropped with a stated reason. Slots the theme never supplies
//! stay `None` — the UI fills them from its own built-in palette and runs
//! every theme-supplied color through its contrast solver, so an arbitrary
//! package can never paint unreadable text.
//!
//! The mapping is the compatibility contract of this crate: a real Firefox
//! package installs with mapped colors plus a report the user can inspect.

use crate::colors::Rgba;
use crate::manifest::{ImageRef, ThemeObject};
use serde::{Deserialize, Serialize};

/// The top-level manifest key reserved for flavite's conversion metadata.
pub const FLAVITE_REPORT_KEY: &str = "flavite";

/// The semantic surface of flavite, as a theme package can shape it.
///
/// Every slot is optional: `None` means "the theme did not speak to this
/// surface" and the built-in palette keeps its seat. The UI resolves a
/// themed slot over its default, then repairs contrast.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SemanticTheme {
    /// Behind the top toolbar strip (`theme_frame` territory).
    pub top_bar_background: Option<Rgba>,
    /// Text over the top toolbar strip.
    pub top_bar_text: Option<Rgba>,
    /// The general window background.
    pub window_background: Option<Rgba>,
    /// The general window text.
    pub window_text: Option<Rgba>,
    /// The primary accent (buttons, selection highlights, links).
    pub accent: Option<Rgba>,
    /// Dropdown/menu surfaces.
    pub menu_background: Option<Rgba>,
    /// Dropdown/menu text.
    pub menu_text: Option<Rgba>,
    /// The editor pane background.
    pub editor_background: Option<Rgba>,
    /// The editor pane text.
    pub editor_text: Option<Rgba>,
    /// Text/selection highlight.
    pub selection: Option<Rgba>,
    /// Secondary pane background (sidebars, list rails).
    pub sidebar_background: Option<Rgba>,
    /// Secondary pane text.
    pub sidebar_text: Option<Rgba>,
    /// The frame tint for inactive windows.
    pub frame_inactive: Option<Rgba>,
}

impl SemanticTheme {
    /// Whether this variant supplies at least one slot.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One key that mapped onto a semantic slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappedKey {
    /// The source key in the browser manifest.
    pub key: String,
    /// The flavite semantic slot that absorbed it.
    pub slot: String,
}

/// One key the conversion refused, with the reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DroppedKey {
    /// The source key (or property group) that was dropped.
    pub key: String,
    /// Why: no flavite surface, chrome-only concept, or unsupported media.
    pub reason: String,
}

/// The conversion report stored inside a `.flavite.xpi` under the
/// reserved `flavite` manifest key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversionReport {
    /// The flavite theme-package format version (1).
    pub format_version: u32,
    /// The resolved theme identity (gecko id, else a content hash).
    pub theme_id: String,
    /// The theme display name.
    pub name: String,
    /// Keys mapped onto semantic slots.
    pub mapped: Vec<MappedKey>,
    /// Keys dropped, with reasons.
    pub dropped: Vec<DroppedKey>,
    /// Field-level warnings from parsing and mapping.
    pub warnings: Vec<String>,
    /// The `theme_frame` image is a multi-frame APNG.
    pub animated_frame: bool,
    /// A `dark_theme` variant was present and mapped.
    pub supports_dark_variant: bool,
}

/// The preference chains from browser keys to semantic slots. The first
/// key a theme supplies within a chain wins; every other present color key
/// is dropped with a precise reason. Field names are the `SemanticTheme`
/// slots.
const CHAINS: &[(&str, &[&str])] = &[
    ("top_bar_background", &["frame", "toolbar"]),
    ("top_bar_text", &["tab_background_text", "toolbar_text"]),
    ("window_background", &["toolbar", "frame"]),
    ("window_text", &["toolbar_text", "tab_background_text"]),
    (
        "accent",
        &["icons", "tab_selected", "toolbar_field_highlight"],
    ),
    ("menu_background", &["popup"]),
    ("menu_text", &["popup_text"]),
    ("editor_background", &["toolbar_field", "popup"]),
    ("editor_text", &["toolbar_field_text", "popup_text"]),
    ("selection", &["toolbar_field_highlight", "tab_selected"]),
    ("sidebar_background", &["sidebar"]),
    ("sidebar_text", &["sidebar_text"]),
    ("frame_inactive", &["frame_inactive"]),
];

/// The outcome of mapping one theme object onto the semantic surface.
#[derive(Debug, Clone, PartialEq)]
pub struct MappedTheme {
    /// The filled slots (`None` where the theme stayed silent).
    pub semantic: SemanticTheme,
    /// Keys that landed on slots.
    pub mapped: Vec<MappedKey>,
    /// Keys dropped, with reasons.
    pub dropped: Vec<DroppedKey>,
    /// Warnings about translucent or degenerate input.
    pub warnings: Vec<String>,
}

/// Map one theme object. `scheme` names the variant in warnings.
#[must_use]
pub fn map_theme(theme: &ThemeObject, scheme: &'static str) -> MappedTheme {
    let mut semantic = SemanticTheme::default();
    let mut mapped = Vec::new();
    let mut warnings = Vec::new();
    let mut supplied_slot: Vec<&str> = Vec::new();

    for (slot, keys) in CHAINS {
        for key in *keys {
            let Some(entries) = theme.colors.get(*key) else {
                continue;
            };
            let Some(&color) = entries.first() else {
                continue;
            };
            set_slot(&mut semantic, slot, color);
            mapped.push(MappedKey {
                key: (*key).to_owned(),
                slot: (*slot).to_owned(),
            });
            supplied_slot.push(key);
            // Firefox `frame` pairs carry the inactive tint as the second
            // entry; honor it on the inactive slot unless a key did.
            if *key == "frame"
                && let Some(inactive) = entries.get(1)
            {
                semantic.frame_inactive = Some(*inactive);
            }
            break;
        }
    }

    // Every present color key that did not win a chain slot is dropped
    // with its reason, never silently ignored.
    let mut dropped: Vec<DroppedKey> = theme
        .colors
        .keys()
        .filter(|key| !supplied_slot.contains(&key.as_str()))
        .map(|key| DroppedKey {
            key: key.clone(),
            reason: drop_reason(key),
        })
        .collect();

    for (key, reason) in &theme.skipped {
        dropped.push(DroppedKey {
            key: key.clone(),
            reason: reason.clone(),
        });
    }
    if theme
        .images
        .values()
        .any(|image| matches!(image, ImageRef::Gradient(_)))
    {
        dropped.push(DroppedKey {
            key: "images (gradient)".into(),
            reason: "CSS gradient images are not supported yet".into(),
        });
    }
    for key in theme.images.keys() {
        if key != "theme_frame" {
            dropped.push(DroppedKey {
                key: format!("images.{key}"),
                reason: "layered header backgrounds have no flavite surface".into(),
            });
        }
    }
    for key in theme.properties.keys() {
        dropped.push(DroppedKey {
            key: format!("properties.{key}"),
            reason: "background layout properties have no flavite surface".into(),
        });
    }
    for key in theme.unknown.keys() {
        dropped.push(DroppedKey {
            key: key.clone(),
            reason: "unknown theme key".into(),
        });
    }

    if semantic.is_empty() {
        warnings.push(format!(
            "the {scheme} theme object mapped to no flavite surface"
        ));
    }
    // Translucent theme colors are recorded with their alpha intact: the
    // UI composites them over its own default, preserving the author's
    // intent instead of guessing a backdrop here.
    MappedTheme {
        semantic,
        mapped,
        dropped,
        warnings,
    }
}

fn set_slot(semantic: &mut SemanticTheme, slot: &str, color: Rgba) {
    match slot {
        "top_bar_background" => semantic.top_bar_background = Some(color),
        "top_bar_text" => semantic.top_bar_text = Some(color),
        "window_background" => semantic.window_background = Some(color),
        "window_text" => semantic.window_text = Some(color),
        "accent" => semantic.accent = Some(color),
        "menu_background" => semantic.menu_background = Some(color),
        "menu_text" => semantic.menu_text = Some(color),
        "editor_background" => semantic.editor_background = Some(color),
        "editor_text" => semantic.editor_text = Some(color),
        "selection" => semantic.selection = Some(color),
        "sidebar_background" => semantic.sidebar_background = Some(color),
        "sidebar_text" => semantic.sidebar_text = Some(color),
        "frame_inactive" => semantic.frame_inactive = Some(color),
        _ => {}
    }
}

fn drop_reason(key: &str) -> String {
    match key {
        "tab_loading" | "tab_background_separator" | "toolbar_field_separator" => {
            "removed or inert in current Firefox; no flavite surface".into()
        }
        "ntp_background" | "ntp_text" | "ntp_card_background" => {
            "new-tab page has no flavite surface".into()
        }
        "toolbar_top_separator" | "toolbar_bottom_separator" | "toolbar_vertical_separator" => {
            "separator lines are derived from the palette".into()
        }
        "toolbar_field_border"
        | "toolbar_field_focus"
        | "toolbar_field_text_focus"
        | "toolbar_field_border_focus" => "focus rings are derived from the accent".into(),
        "icons_attention" | "button_background_hover" | "button_background_active" => {
            "button states are derived from the accent".into()
        }
        "popup_border" | "popup_highlight" | "popup_highlight_text" => {
            "menu highlights are derived from the accent".into()
        }
        "sidebar_border" | "sidebar_highlight" | "sidebar_highlight_text" => {
            "sidebar accents are derived from the palette".into()
        }
        "tab_line" | "tab_selected" | "tab_text" => {
            "tab-strip colors are folded into the accent and window slots".into()
        }
        "bookmark_text" => "alias of toolbar_text".into(),
        "tints" => "Chrome-only HSL tints have no flavite surface".into(),
        _ => "a higher-preference key supplied the same slot".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn theme(colors: serde_json::Value) -> ThemeObject {
        // Parse through the manifest codec so the mapping tests exercise
        // the real entry path.
        let text = format!(
            r#"{{"name":"t","theme":{{"colors":{}}}}}"#,
            serde_json::to_string(&colors).unwrap()
        );
        let (manifest, _) = crate::manifest::parse_manifest(&text).unwrap();
        manifest.theme
    }

    #[test]
    fn a_typical_dark_theme_maps_app_wide() {
        let object = theme(json!({
            "frame": "#1a0f38",
            "tab_background_text": "#e8e2ff",
            "toolbar": "#111729",
            "toolbar_text": "#c9d1e8",
            "toolbar_field": "#0b0f1a",
            "toolbar_field_text": "#e6e0ff",
            "icons": "#b489f0",
            "popup": "#141b2e",
            "popup_text": "#dfe4f2",
            "sidebar": "#0d1220",
            "sidebar_text": "#b9c2d8",
            "ntp_background": "#000000"
        }));
        let result = map_theme(&object, "light");
        assert!(result.warnings.is_empty());
        assert_eq!(
            result.semantic.top_bar_background,
            Some(Rgba::rgb(0x1a, 0x0f, 0x38))
        );
        assert_eq!(
            result.semantic.top_bar_text,
            Some(Rgba::rgb(0xe8, 0xe2, 0xff))
        );
        assert_eq!(
            result.semantic.window_background,
            Some(Rgba::rgb(0x11, 0x17, 0x29))
        );
        assert_eq!(
            result.semantic.editor_background,
            Some(Rgba::rgb(0x0b, 0x0f, 0x1a))
        );
        assert_eq!(
            result.semantic.editor_text,
            Some(Rgba::rgb(0xe6, 0xe0, 0xff))
        );
        assert_eq!(result.semantic.accent, Some(Rgba::rgb(0xb4, 0x89, 0xf0)));
        assert_eq!(
            result.semantic.menu_background,
            Some(Rgba::rgb(0x14, 0x1b, 0x2e))
        );
        assert_eq!(
            result.semantic.sidebar_background,
            Some(Rgba::rgb(0x0d, 0x12, 0x20))
        );
        assert_eq!(result.mapped.len(), 11);
        assert!(
            result
                .mapped
                .iter()
                .any(|m| m.key == "frame" && m.slot == "top_bar_background")
        );
        // ntp_* is dropped with its reason, never silently.
        assert!(
            result
                .dropped
                .iter()
                .any(|d| d.key == "ntp_background" && d.reason.contains("new-tab page"))
        );
    }

    #[test]
    fn sparse_theme_leaves_unset_slots_to_the_ui() {
        let object = theme(json!({ "frame": "#101010", "tab_background_text": "#eeeeee" }));
        let result = map_theme(&object, "light");
        assert!(result.warnings.is_empty());
        assert!(result.semantic.top_bar_background.is_some());
        assert!(result.semantic.editor_background.is_none());
        assert!(result.semantic.accent.is_none());
        // `frame` fills both the top-bar and window slots (the window
        // chain's `toolbar` is absent), and `tab_background_text` fills
        // top-bar and window text: a key may speak to several surfaces.
        assert_eq!(result.mapped.len(), 4);
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn frame_pairs_fill_the_inactive_slot() {
        let object = theme(json!({ "frame": ["#101010", "#202020"] }));
        let result = map_theme(&object, "light");
        assert_eq!(
            result.semantic.top_bar_background,
            Some(Rgba::rgb(0x10, 0x10, 0x10))
        );
        assert_eq!(
            result.semantic.frame_inactive,
            Some(Rgba::rgb(0x20, 0x20, 0x20))
        );
    }

    #[test]
    fn chain_preference_first_key_wins() {
        // Both `toolbar` and `frame` present: `frame` takes the top bar,
        // `toolbar` takes the window, and neither is dropped.
        let object = theme(json!({ "frame": "#101010", "toolbar": "#202020" }));
        let result = map_theme(&object, "light");
        assert_eq!(
            result.semantic.top_bar_background,
            Some(Rgba::rgb(0x10, 0x10, 0x10))
        );
        assert_eq!(
            result.semantic.window_background,
            Some(Rgba::rgb(0x20, 0x20, 0x20))
        );
    }

    #[test]
    fn translucent_colors_are_recorded_not_flattened() {
        let object = theme(json!({ "toolbar": "rgba(10, 20, 30, 0.5)" }));
        let result = map_theme(&object, "light");
        assert_eq!(
            result.semantic.window_background,
            Some(Rgba::with_alpha(10, 20, 30, 128))
        );
    }

    #[test]
    fn fully_unmappable_theme_warns() {
        let object = theme(json!({ "ntp_background": "#000", "tab_line": "#111" }));
        let result = map_theme(&object, "light");
        assert!(result.semantic.is_empty());
        assert!(!result.warnings.is_empty());
        assert_eq!(result.dropped.len(), 2);
    }

    #[test]
    fn skipped_structured_keys_surface_in_the_report() {
        let object = theme(json!({ "frame": "#101010" }));
        let mut with_tints = object.clone();
        with_tints.skipped.push((
            "tints".to_owned(),
            "Chrome-only HSL tints have no flavite surface".to_owned(),
        ));
        let result = map_theme(&with_tints, "light");
        assert!(
            result
                .dropped
                .iter()
                .any(|d| d.key == "tints" && d.reason.contains("Chrome-only"))
        );
    }

    #[test]
    fn unknown_and_property_keys_are_dropped_with_reasons() {
        let object = theme(json!({ "frame": "#101010" }));
        let mut with_extras = object.clone();
        with_extras
            .properties
            .insert("ntp_background_alignment".into(), json!("bottom"));
        with_extras
            .unknown
            .insert("theme_experiment".into(), json!({}));
        let result = map_theme(&with_extras, "light");
        assert!(
            result
                .dropped
                .iter()
                .any(|d| d.key == "properties.ntp_background_alignment")
        );
        assert!(
            result
                .dropped
                .iter()
                .any(|d| d.key == "theme_experiment" && d.reason.contains("unknown"))
        );
    }
}
