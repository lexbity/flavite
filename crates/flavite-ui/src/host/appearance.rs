//! Interface appearance host (RFC-0003 W11, D-11, D-15): unified settings
//! from the vault, the resolved color scheme, scale-aware metrics, theme
//! packages, and settings persistence through the SettingsStore.
//!
//! # Compatibility
//! During S3 migration, this module maintains compatibility types (Preferences,
//! ThemeChoice, LineWidth) that are used by the dialog. These will be removed
//! in S8 when the dialog is deleted and replaced by the Settings tab.

/// Minimum interface scale percentage.
pub const MIN_INTERFACE_SCALE_PCT: u16 = 75;
/// Maximum interface scale percentage.
pub const MAX_INTERFACE_SCALE_PCT: u16 = 200;
/// Minimum editor font size.
pub const MIN_EDITOR_FONT_SIZE: u16 = 12;
/// Maximum editor font size.
pub const MAX_EDITOR_FONT_SIZE: u16 = 28;

use crate::style::{self, Metrics, Scheme};
use flavite_api::{Flavite, InstalledTheme};
use flavite_settings::{
    resolve::Resolved,
    value::SettingValue,
};
use flavite_theme::{SemanticTheme, ThemeImage};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::time::Instant;

/// Which palette the shell uses (compatibility type for S3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    /// Follow the OS color scheme; dark when the OS reports nothing.
    #[default]
    System,
    /// Always the light palette.
    Light,
    /// Always the dark palette.
    Dark,
}

impl ThemeChoice {
    /// Convert from a string.
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "system" => Some(Self::System),
            "light" => Some(Self::Light),
            "dark" => Some(Self::Dark),
            _ => None,
        }
    }

    /// The stable preference name of this choice.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

/// The editor's readable line width: bounded columns, or the full pane (compatibility type for S3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LineWidth {
    /// Wrap text at this many columns.
    Columns(u16),
    /// Use the full editor pane width.
    #[default]
    Full,
}

impl LineWidth {
    /// The bounded column count, or `None` for the full pane width.
    #[must_use]
    pub const fn columns(self) -> Option<u16> {
        match self {
            Self::Columns(columns) => Some(columns),
            Self::Full => None,
        }
    }
}

/// Compatibility Preferences type for S3 migration.
/// This will be removed in S8 when the dialog is deleted.
#[derive(Debug, Clone, PartialEq)]
pub struct Preferences {
    /// Light/dark/System palette choice.
    pub theme: ThemeChoice,
    /// The installed theme package shaping the palette.
    pub theme_package: Option<String>,
    /// Interface scale in percent.
    pub interface_scale_pct: u16,
    /// Editor font size in logical pixels.
    pub editor_font_size: u16,
    /// Readable editor line width.
    pub line_width: LineWidth,
    /// Reduce motion.
    pub reduced_motion: bool,
    /// Prefer stable IDs when inserting links.
    pub prefer_stable_links: bool,
    /// Graph activation acknowledged.
    pub graph_activation_acknowledged: bool,
    /// Whether the collapsible vault navigator is open.
    pub sidebar_open: bool,
    /// Notes-sidebar width in logical pixels.
    pub sidebar_width: u16,
    /// Command-id -> chord-string overrides.
    pub shortcuts: BTreeMap<String, String>,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            theme_package: None,
            interface_scale_pct: 100,
            editor_font_size: 16,
            line_width: LineWidth::Full,
            reduced_motion: false,
            prefer_stable_links: true,
            graph_activation_acknowledged: false,
            sidebar_open: true,
            sidebar_width: 260,
            shortcuts: BTreeMap::new(),
        }
    }
}

impl Preferences {
    /// Compatibility: create default preferences (S3 migration).
    pub fn defaults() -> Self {
        Self::default()
    }

    /// Compatibility: parse a JSON document into Preferences (S3 migration).
    /// This maintains the old device_preferences JSON format for tests.
    pub fn parse(document: Option<&str>) -> Result<ParseResult, String> {
        let document = document.unwrap_or("");
        let value: Value = serde_json::from_str(document)
            .map_err(|e| format!("failed to parse preferences JSON: {e}"))?;
        let empty_map = Map::new();
        let map = value.as_object().unwrap_or(&empty_map);
        let mut warnings = Vec::new();
        let mut prefs = Preferences::default();

        // Parse theme
        if let Some(Value::String(theme)) = map.get("theme") {
            prefs.theme = match theme.as_str() {
                "system" => ThemeChoice::System,
                "light" => ThemeChoice::Light,
                "dark" => ThemeChoice::Dark,
                _ => {
                    warnings.push(format!("unknown theme: {theme}"));
                    ThemeChoice::System
                }
            };
        }

        // Parse theme_package
        if let Some(Value::String(pkg)) = map.get("theme_package") {
            prefs.theme_package = if pkg.is_empty() { None } else { Some(pkg.clone()) };
        }

        // Parse interface_scale_pct
        prefs.interface_scale_pct = bounded_field(map, "interface_scale_pct", 75, 200, 100, &mut warnings);
        prefs.editor_font_size = bounded_field(map, "editor_font_size", 12, 28, 14, &mut warnings);

        // Parse line_width
        if let Some(value) = map.get("line_width") {
            match value {
                Value::Number(n) => {
                    let cols = n.as_f64().unwrap_or_default() as u16;
                    if (40..=160).contains(&cols) {
                        prefs.line_width = LineWidth::Columns(cols);
                    } else {
                        warnings.push("line_width must be 40..=160".to_string());
                    }
                }
                Value::String(s) => {
                    if s == "full" {
                        prefs.line_width = LineWidth::Full;
                    } else {
                        warnings.push("line_width must be an integer or \"full\"".to_string());
                    }
                }
                _ => warnings.push("line_width must be an integer or \"full\"".to_string()),
            }
        }

        // Parse reduced_motion
        if let Some(Value::Bool(b)) = map.get("reduced_motion") {
            prefs.reduced_motion = *b;
        }

        // Parse prefer_stable_links
        if let Some(Value::Bool(b)) = map.get("prefer_stable_links") {
            prefs.prefer_stable_links = *b;
        }

        // Parse graph_activation_acknowledged
        if let Some(Value::Bool(b)) = map.get("graph_activation_acknowledged") {
            prefs.graph_activation_acknowledged = *b;
        }

        // Parse sidebar_open
        if let Some(Value::Bool(b)) = map.get("sidebar_open") {
            prefs.sidebar_open = *b;
        }

        // Parse sidebar_width
        prefs.sidebar_width = bounded_field(map, "sidebar_width", 160, 480, 260, &mut warnings);

        // Parse shortcuts
        if let Some(Value::Object(shortcuts)) = map.get("shortcuts") {
            for (command, chord) in shortcuts {
                if let Value::String(chord_str) = chord {
                    // Validate chord
                    if flavite_settings::chord::parse_chord(chord_str).is_some() {
                        prefs.shortcuts.insert(command.clone(), chord_str.clone());
                    } else {
                        warnings.push(format!("invalid chord for {command}: {chord_str}"));
                    }
                }
            }
        }

        Ok(ParseResult { preferences: prefs, warnings })
    }

    /// Compatibility: convert Preferences to stored JSON string (S3 migration).
    pub fn to_stored(&self) -> String {
        self.to_json().to_string()
    }

    /// Compatibility: convert Preferences to JSON Value (S3 migration).
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("schema".to_string(), Value::Number(serde_json::Number::from(1)));
        map.insert("theme".to_string(), Value::String(self.theme.name().to_string()));
        map.insert(
            "theme_package".to_string(),
            Value::String(self.theme_package.clone().unwrap_or_default()),
        );
        map.insert("interface_scale_pct".to_string(), Value::Number(self.interface_scale_pct.into()));
        map.insert("editor_font_size".to_string(), Value::Number(self.editor_font_size.into()));
        match self.line_width {
            LineWidth::Columns(cols) => {
                map.insert("line_width".to_string(), Value::Number(cols.into()));
            }
            LineWidth::Full => {
                map.insert("line_width".to_string(), Value::String("full".to_string()));
            }
        }
        map.insert("reduced_motion".to_string(), Value::Bool(self.reduced_motion));
        map.insert("prefer_stable_links".to_string(), Value::Bool(self.prefer_stable_links));
        map.insert(
            "graph_activation_acknowledged".to_string(),
            Value::Bool(self.graph_activation_acknowledged),
        );
        map.insert("sidebar_open".to_string(), Value::Bool(self.sidebar_open));
        map.insert("sidebar_width".to_string(), Value::Number(self.sidebar_width.into()));
        
        let shortcuts: Map<String, Value> = self
            .shortcuts
            .iter()
            .map(|(cmd, chord)| (cmd.clone(), Value::String(chord.clone())))
            .collect();
        if !shortcuts.is_empty() {
            map.insert("shortcuts".to_string(), Value::Object(shortcuts));
        }
        
        Value::Object(map)
    }
}

/// Result of parsing preferences.
pub struct ParseResult {
    pub preferences: Preferences,
    pub warnings: Vec<String>,
}

/// Helper for bounded field parsing.
fn bounded_field(
    map: &Map<String, Value>,
    key: &str,
    min: u16,
    max: u16,
    default: u16,
    warnings: &mut Vec<String>,
) -> u16 {
    match map.get(key) {
        None => default,
        Some(Value::Number(number)) => {
            let raw = number.as_f64().unwrap_or_default();
            if raw.is_finite() && raw >= f64::from(min) && raw <= f64::from(max) {
                raw as u16
            } else {
                warnings.push(format!("{key} must be {min}..={max}"));
                default
            }
        }
        _ => {
            warnings.push(format!("{key} must be a number"));
            default
        }
    }
}

/// Preference edit messages, routed through the host catalog (D-16).
#[derive(Debug, Clone)]
pub enum PreferenceMessage {
    /// Choose the palette source.
    SetTheme(ThemeChoice),
    /// Set the interface scale in percent (refused out of range).
    SetScale(u16),
    /// Set the editor font size (refused out of range).
    SetEditorFontSize(u16),
    /// Set the readable editor line width.
    SetLineWidth(LineWidth),
    /// Enable or disable reduced motion.
    SetReducedMotion(bool),
    /// Prefer stable IDs when inserting links to notes that have one.
    SetStableLinks(bool),
    /// RFC-0006 S04 (D-02): acknowledge the changed graph gesture contract
    /// (double-click activates; Insert is explicit). Persisted once.
    AcknowledgeGraphActivation,
    /// Set or clear one command's chord override.
    SetShortcut {
        /// The command id.
        command: String,
        /// The new chord string, or `None` to restore the default.
        chord: Option<String>,
    },
    /// Begin capture: the next key press becomes that command's chord.
    BeginShortcutCapture(String),
    /// A captured key press (or Escape, `chord: None` to cancel).
    ShortcutCaptured {
        /// The command being captured.
        command: String,
        /// The captured chord string, or `None` when cancelled.
        chord: Option<String>,
    },
    /// Clear every chord override.
    RestoreShortcuts,
    /// Select an installed theme package (`None` restores the built-in
    /// palettes). The selection is a device preference like `theme`.
    SetThemePackage(Option<String>),
    /// Open the native file dialog to pick a standard `.xpi` package.
    ThemeInstallPick,
    /// Convert and install the package at this path (the dialog result or
    /// an explicitly typed path).
    ThemeInstallFromPath(std::path::PathBuf),
    /// Remove an installed theme package.
    ThemeUninstall(String),
}

/// The resolved appearance state for the shell.
#[derive(Debug, Default)]
pub struct AppearanceHost {
    /// The validated device preferences.
    pub prefs: Preferences,
    /// The OS color scheme, when the runtime reports one (`None` = no
    /// preference reported: System resolves to dark, the historical look).
    system_dark: Option<bool>,
    /// Whether the stored document parsed; false disables saving so a
    /// corrupt file can be diagnosed instead of overwritten (D-12).
    pub valid: bool,
    /// The stored generation for compare-and-set writes.
    generation: u64,
    /// Field-level fallback warnings from the last parse.
    pub warnings: Vec<String>,
    /// The last persistence failure, surfaced in the status line.
    pub last_error: Option<String>,
    /// The last shortcut-validation failure for the editor UI.
    pub shortcut_error: Option<String>,
    /// The resolved chord table: default chords with any validated
    /// overrides applied. Rebuilt on every preference refresh.
    pub shortcuts: Vec<(flavite_api::Chord, &'static str)>,
    /// The loaded theme package, when one is selected and loads.
    pub theme_package: Option<ThemePackage>,
    /// The installed theme packages, for the preferences surface.
    pub installed_themes: Vec<InstalledTheme>,
    /// The last theme load/install/uninstall failure, surfaced in the
    /// status line.
    pub theme_error: Option<String>,
    /// The animation frame index into the active theme frame sequence.
    frame_index: usize,
    /// When the current animation frame was first painted.
    frame_started: Option<Instant>,
    metrics: Metrics,
}

/// A loaded theme package: both scheme variants and the decoded header
/// images, with the image handles built once per load.
#[derive(Debug, Clone)]
pub struct ThemePackage {
    /// The theme identity.
    pub id: String,
    /// The theme display name.
    pub name: String,
    /// The base (light) mapping.
    pub light: SemanticTheme,
    /// The `dark_theme` mapping, when the package supplies one.
    pub dark: Option<SemanticTheme>,
    /// The header frames (base variant), with iced handles.
    pub frames: Option<ThemeFrames>,
    /// The header frames (dark variant), when the package supplies them.
    pub dark_frames: Option<ThemeFrames>,
    /// The conversion warnings of the loaded package (dropped keys and
    /// parse fallbacks), surfaced in the preferences dialog.
    pub warnings: Vec<String>,
}

/// Decoded header frames plus the iced image handles for them.
#[derive(Debug, Clone)]
pub struct ThemeFrames {
    /// Frame size in pixels (all frames share it).
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Per-frame delay in milliseconds.
    pub delays_ms: Vec<u32>,
    /// One handle per frame, in display order.
    handles: Vec<iced::widget::image::Handle>,
}

impl ThemeFrames {
    /// Build handles from a decoded theme image. The pixel buffers are
    /// owned by the handle (`Bytes::from_owner`), so the decoded image
    /// can be dropped.
    #[must_use]
    pub fn from_image(image: &ThemeImage) -> Self {
        let (width, height, delays_ms, buffers): (u32, u32, Vec<u32>, Vec<std::sync::Arc<[u8]>>) =
            match image {
                ThemeImage::Static {
                    width,
                    height,
                    rgba,
                } => (*width, *height, vec![0], vec![rgba.clone()]),
                ThemeImage::Animated {
                    width,
                    height,
                    frames,
                } => (
                    *width,
                    *height,
                    frames.iter().map(|frame| frame.delay_ms).collect(),
                    frames.iter().map(|frame| frame.rgba.clone()).collect(),
                ),
            };
        let handles = buffers
            .into_iter()
            .map(|pixels| {
                iced::widget::image::Handle::from_rgba(
                    width,
                    height,
                    iced::advanced::graphics::core::Bytes::from_owner(pixels),
                )
            })
            .collect();
        Self {
            width,
            height,
            delays_ms,
            handles,
        }
    }

    /// The handle for one frame index (wrapping).
    #[must_use]
    pub fn handle(&self, index: usize) -> &iced::widget::image::Handle {
        &self.handles[index % self.handles.len()]
    }

    /// Whether more than one frame exists.
    #[must_use]
    pub fn is_animated(&self) -> bool {
        self.handles.len() > 1
    }
}

impl AppearanceHost {
    /// Boot defaults before any vault is attached.
    #[must_use]
    pub fn boot() -> Self {
        let prefs = Preferences::default();
        let metrics = Metrics::from_preferences(&prefs);
        let mut host = Self {
            prefs,
            system_dark: None,
            valid: true,
            generation: 0,
            warnings: Vec::new(),
            last_error: None,
            shortcut_error: None,
            shortcuts: Vec::new(),
            theme_package: None,
            installed_themes: Vec::new(),
            theme_error: None,
            frame_index: 0,
            frame_started: None,
            metrics,
        };
        host.refresh();
        host
    }

    /// Load preferences from the unified settings store at vault attach.
    /// A read or parse failure is an honest report with defaults applied;
    /// the document is never overwritten by that fallback (D-12, RFC-0007 S3).
    pub fn load(&mut self, flavite: &Flavite) {
        // Read from the new unified settings store
        match flavite.settings_read() {
            Ok(stored) => {
                self.generation = stored.generation;
                self.valid = !stored.read_only;
                self.warnings = stored.warnings;
                
                // Convert Resolved settings to Preferences compatibility type
                self.prefs = self.resolved_to_preferences(&stored.resolved);
                self.metrics = Metrics::from_preferences(&self.prefs);
            }
            Err(error) => {
                self.valid = false;
                self.last_error = Some(format!("Settings could not be read: {error}"));
                // Use defaults on error
                self.prefs = Preferences::default();
                self.metrics = Metrics::from_preferences(&self.prefs);
            }
        }
    }

    /// Convert Resolved settings to the Preferences compatibility type.
    fn resolved_to_preferences(&self, resolved: &Resolved) -> Preferences {
        let get_str = |id: &str| -> String {
            resolved.get(id).and_then(|v| v.as_str()).map(|s| s.to_owned()).unwrap_or_default()
        };
        let get_bool = |id: &str| -> bool {
            resolved.get(id).and_then(|v| v.as_bool()).unwrap_or(false)
        };
        let get_int = |id: &str| -> u16 {
            resolved.get(id).and_then(|v| v.as_int()).map(|i| i as u16).unwrap_or_default()
        };

        // Map theme
        let theme_name = get_str("appearance.theme");
        let theme = ThemeChoice::from_name(&theme_name).unwrap_or(ThemeChoice::System);
        
        // Map line_width
        let line_width_value = resolved.get("editor.line_width");
        let line_width = match line_width_value {
            Some(SettingValue::Int(columns)) => LineWidth::Columns(*columns as u16),
            Some(SettingValue::Str(s)) if s == "full" => LineWidth::Full,
            _ => LineWidth::Columns(80),
        };

        // Map shortcuts
        let mut shortcuts = BTreeMap::new();
        if let Some(SettingValue::ShortcutMap(map)) = resolved.get("shortcuts.overrides") {
            for (command, chord) in map {
                shortcuts.insert(command.clone(), chord.clone());
            }
        }

        Preferences {
            theme,
            theme_package: if get_str("appearance.theme_package").is_empty() {
                None
            } else {
                Some(get_str("appearance.theme_package"))
            },
            interface_scale_pct: get_int("appearance.scale_pct"),
            editor_font_size: get_int("editor.font_size"),
            line_width,
            reduced_motion: get_bool("appearance.reduced_motion"),
            prefer_stable_links: get_bool("links.prefer_stable_links"),
            graph_activation_acknowledged: get_bool("state.graph_activation_acknowledged"),
            sidebar_open: get_bool("sidebar.open"),
            sidebar_width: get_int("sidebar.width"),
            shortcuts,
        }
    }

    /// The color scheme the shell currently paints with.
    #[must_use]
    pub fn scheme(&self) -> Scheme {
        match self.prefs.theme {
            ThemeChoice::Light => Scheme::Light,
            ThemeChoice::Dark => Scheme::Dark,
            ThemeChoice::System => {
                if self.system_dark == Some(false) {
                    Scheme::Light
                } else {
                    Scheme::Dark
                }
            }
        }
    }

    /// The resolved iced theme for the current scheme, with the selected
    /// theme package's slots overlaid when one is loaded.
    #[must_use]
    pub fn theme(&self) -> iced::Theme {
        style::theme_resolved(self.scheme(), self.theme_semantic())
    }

    /// The theme package's semantic slots for the current scheme: the
    /// dark variant when the package supplies one and the scheme is dark.
    #[must_use]
    pub fn theme_semantic(&self) -> Option<&SemanticTheme> {
        let package = self.theme_package.as_ref()?;
        match self.scheme() {
            Scheme::Dark => package.dark.as_ref().or(Some(&package.light)),
            Scheme::Light => Some(&package.light),
        }
    }

    /// The theme package header frames for the current scheme.
    #[must_use]
    pub fn theme_frames(&self) -> Option<&ThemeFrames> {
        let package = self.theme_package.as_ref()?;
        match self.scheme() {
            Scheme::Dark => package.dark_frames.as_ref().or(package.frames.as_ref()),
            Scheme::Light => package.frames.as_ref(),
        }
    }

    /// The header handle for the current animation frame, `None` when no
    /// package header is loaded for this scheme.
    #[must_use]
    pub fn theme_frame_handle(&self) -> Option<&iced::widget::image::Handle> {
        let frames = self.theme_frames()?;
        Some(frames.handle(self.frame_index))
    }

    /// Advance the header animation when its frame delay has elapsed.
    /// Honors `reduced_motion` by never advancing; returns true when the
    /// frame changed so the shell repaints.
    pub fn tick_theme_animation(&mut self) -> bool {
        let Some(frames) = self.theme_frames() else {
            return false;
        };
        if !frames.is_animated() || self.prefs.reduced_motion {
            return false;
        }
        let index = self.frame_index % frames.delays_ms.len();
        let delay = frames.delays_ms[index];
        let started = self.frame_started.unwrap_or_else(Instant::now);
        if started.elapsed() < std::time::Duration::from_millis(u64::from(delay)) {
            return false;
        }
        self.frame_index = (self.frame_index + 1) % frames.handles.len();
        self.frame_started = Some(Instant::now());
        true
    }

    /// Load the installed-theme list for the preferences surface.
    pub fn reload_installed_themes(&mut self, flavite: &Flavite) {
        self.installed_themes = flavite
            .themes()
            .map(|store| store.list().unwrap_or_default())
            .unwrap_or_default();
    }

    /// Load the selected theme package. A missing or broken package is an
    /// honest `theme_error` with the built-in palettes kept; the
    /// selection itself is never cleared by a failed load, so a transient
    /// store problem does not silently reset the preference (D-12 spirit).
    pub fn reload_theme_package(&mut self, flavite: &Flavite) {
        self.theme_error = None;
        self.frame_index = 0;
        self.frame_started = None;
        let Some(id) = self.prefs.theme_package.clone() else {
            self.theme_package = None;
            return;
        };
        let Some(store) = flavite.themes() else {
            self.theme_error =
                Some("Theme packages are unavailable (no device data directory)".into());
            return;
        };
        match store.load(&id) {
            Ok(loaded) => {
                let converted = &loaded.converted;
                let dark = converted.dark.as_ref().map(|dark| dark.semantic);
                self.theme_package = Some(ThemePackage {
                    id: converted.id.clone(),
                    name: converted.report.name.clone(),
                    light: converted.light.semantic,
                    dark,
                    frames: converted.theme_frame.as_ref().map(ThemeFrames::from_image),
                    dark_frames: converted
                        .dark_theme_frame
                        .as_ref()
                        .map(ThemeFrames::from_image),
                    warnings: converted.report.warnings.clone(),
                });
            }
            Err(error) => {
                self.theme_package = None;
                self.theme_error = Some(format!("Theme {id:?} could not be loaded: {error}"));
            }
        }
    }

    /// Apply a theme-package selection; returns true when it changed.
    pub fn set_theme_package(&mut self, id: Option<String>) -> bool {
        if self.prefs.theme_package == id {
            return false;
        }
        self.prefs.theme_package = id;
        true
    }

    /// The scale-aware metrics for the current preferences.
    #[must_use]
    pub const fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// The OS reported a color-scheme change (iced system subscription).
    pub fn system_theme(&mut self, mode: iced::theme::Mode) {
        self.system_dark = match mode {
            iced::theme::Mode::Dark => Some(true),
            iced::theme::Mode::Light => Some(false),
            iced::theme::Mode::None => None,
        };
    }

    /// Apply a theme choice; returns true when the document changed.
    pub fn set_theme(&mut self, theme: ThemeChoice) -> bool {
        if self.prefs.theme == theme {
            return false;
        }
        self.prefs.theme = theme;
        true
    }

    /// Apply an interface scale in percent; out-of-range values are
    /// refused, never clamped into a silently different preference.
    pub fn set_scale(&mut self, percent: u16) -> Result<bool, String> {
        if !(MIN_INTERFACE_SCALE_PCT..=MAX_INTERFACE_SCALE_PCT).contains(&percent) {
            return Err(format!(
                "Interface scale must be {MIN_INTERFACE_SCALE_PCT}–{MAX_INTERFACE_SCALE_PCT}%"
            ));
        }
        if self.prefs.interface_scale_pct == percent {
            return Ok(false);
        }
        self.prefs.interface_scale_pct = percent;
        Ok(true)
    }

    /// Apply an editor font size; out-of-range values are refused.
    pub fn set_editor_font_size(&mut self, size: u16) -> Result<bool, String> {
        if !(MIN_EDITOR_FONT_SIZE..=MAX_EDITOR_FONT_SIZE).contains(&size) {
            return Err(format!(
                "Editor font size must be {MIN_EDITOR_FONT_SIZE}–{MAX_EDITOR_FONT_SIZE}"
            ));
        }
        if self.prefs.editor_font_size == size {
            return Ok(false);
        }
        self.prefs.editor_font_size = size;
        Ok(true)
    }

    /// Apply the readable line width.
    pub fn set_line_width(&mut self, width: LineWidth) -> bool {
        if self.prefs.line_width == width {
            return false;
        }
        self.prefs.line_width = width;
        true
    }

    /// Apply reduced motion.
    pub fn set_reduced_motion(&mut self, reduced: bool) -> bool {
        if self.prefs.reduced_motion == reduced {
            return false;
        }
        self.prefs.reduced_motion = reduced;
        true
    }

    /// Acknowledge the changed graph gesture contract (RFC-0006 S04/D-02).
    /// Persisted with the device preferences; the dismissible first-launch
    /// explanation never reappears after this.
    pub fn acknowledge_graph_activation(&mut self) -> bool {
        if self.prefs.graph_activation_acknowledged {
            return false;
        }
        self.prefs.graph_activation_acknowledged = true;
        true
    }

    /// Whether the graph gesture-contract explanation still owes the user
    /// its first-launch dismissal (D-02).
    #[must_use]
    pub const fn graph_activation_notice_pending(&self) -> bool {
        !self.prefs.graph_activation_acknowledged
    }

    /// Set or clear one command's chord override. Validation refuses
    /// unreadable chords, collisions with other commands, and
    /// OS-reserved chords with a named owner.
    pub fn set_shortcut(&mut self, command: &str, chord: Option<String>) {
        self.shortcut_error = None;
        let mut next = self.prefs.shortcuts.clone();
        match chord {
            Some(text) => {
                next.insert(command.to_owned(), text);
            }
            None => {
                next.remove(command);
            }
        }
        // Collisions are validated against the effective table: an
        // override must not shadow another command's default either.
        let mut effective: std::collections::BTreeMap<String, String> =
            crate::host::commands::DEFAULT_CHORDS
                .iter()
                .map(|(id, chord)| ((*id).to_owned(), (*chord).to_owned()))
                .collect();
        // Host-owned defaults are already declared bindings. OS-reserved
        // validation applies to overrides; collision checks still cover
        // the entire effective table using canonical key names.
        let owned_defaults = effective.clone();
        for (id, text) in &next {
            effective.insert(id.clone(), text.clone());
        }
        if let Err(error) = validate_shortcuts(&effective, &owned_defaults) {
            self.shortcut_error = Some(error.to_string());
            return;
        }
        self.prefs.shortcuts = next;
    }

    /// Clear every shortcut override, restoring the catalog defaults.
    pub fn restore_default_shortcuts(&mut self) {
        self.shortcut_error = None;
        self.prefs.shortcuts.clear();
    }

    /// Refresh derived state after any preference change.
    pub fn refresh(&mut self) {
        self.metrics = Metrics::from_preferences(&self.prefs);
        // Overrides were validated at parse/set time; an unparseable
        // leftover falls back to the default chord rather than dropping
        // the command's binding.
        self.shortcuts = crate::host::commands::DEFAULT_CHORDS
            .iter()
            .filter_map(|(id, default)| {
                let text = self
                    .prefs
                    .shortcuts
                    .get(*id)
                    .map(String::as_str)
                    .unwrap_or(default);
                let settings_chord = flavite_settings::chord::parse_chord(text)
                    .or_else(|| flavite_settings::chord::parse_chord(default))?;
                let chord = flavite_api::Chord {
                    modifiers: settings_chord.modifiers,
                    key: settings_chord.key,
                };
                Some((chord, *id))
            })
            .collect();
    }

    /// Resolve a keyboard event to its bound command id, if any chord
    /// matches exactly (modifiers and key).
    #[must_use]
    pub fn chord_command(
        &self,
        key: &iced::keyboard::Key,
        modifiers: &iced::keyboard::Modifiers,
    ) -> Option<&'static str> {
        let pressed = crate::host::commands::modifier_bits(modifiers);
        let token = crate::host::commands::key_token(key)?;
        self.shortcuts.iter().find_map(|(chord, id)| {
            (chord.modifiers == pressed
                && crate::host::commands::chord_key_token(&chord.key).as_deref()
                    == Some(token.as_str()))
            .then_some(*id)
        })
    }

    /// Persist the current preferences to the unified settings store.
    /// Failures are recorded and surfaced, never silent (FR-20, RFC-0007 S3).
    pub fn persist(&mut self, flavite: &mut Flavite) {
        if !self.valid {
            return;
        }
        
        // For S3, we need to use the public API. However, we need to set multiple
        // values atomically. For now, we'll use settings_set for each value.
        // This is not atomic but will be fixed when the Settings tab is implemented.
        
        // Set each preference via settings_set
        let mut any_error = None;
        
        // appearance.theme
        let theme_str = self.prefs.theme.name();
        if let Err(e) = flavite.settings_set("appearance.theme", SettingValue::Str(theme_str.to_owned())) {
            any_error = Some(e.to_string());
        }
        
        // appearance.theme_package
        if let Some(ref package) = self.prefs.theme_package {
            if let Err(e) = flavite.settings_set("appearance.theme_package", SettingValue::Str(package.clone())) {
                any_error = Some(e.to_string());
            }
        } else {
            if let Err(e) = flavite.settings_set("appearance.theme_package", SettingValue::Str("".to_owned())) {
                any_error = Some(e.to_string());
            }
        }
        
        // appearance.scale_pct
        if let Err(e) = flavite.settings_set("appearance.scale_pct", SettingValue::Int(self.prefs.interface_scale_pct as i64)) {
            any_error = Some(e.to_string());
        }
        
        // editor.font_size
        if let Err(e) = flavite.settings_set("editor.font_size", SettingValue::Int(self.prefs.editor_font_size as i64)) {
            any_error = Some(e.to_string());
        }
        
        // editor.line_width
        let line_width_value = match self.prefs.line_width {
            LineWidth::Columns(cols) => SettingValue::Int(cols as i64),
            LineWidth::Full => SettingValue::Str("full".to_owned()),
        };
        if let Err(e) = flavite.settings_set("editor.line_width", line_width_value) {
            any_error = Some(e.to_string());
        }
        
        // Other boolean settings
        if let Err(e) = flavite.settings_set("appearance.reduced_motion", SettingValue::Bool(self.prefs.reduced_motion)) {
            any_error = Some(e.to_string());
        }
        if let Err(e) = flavite.settings_set("links.prefer_stable_links", SettingValue::Bool(self.prefs.prefer_stable_links)) {
            any_error = Some(e.to_string());
        }
        if let Err(e) = flavite.settings_set("sidebar.open", SettingValue::Bool(self.prefs.sidebar_open)) {
            any_error = Some(e.to_string());
        }
        if let Err(e) = flavite.settings_set("sidebar.width", SettingValue::Int(self.prefs.sidebar_width as i64)) {
            any_error = Some(e.to_string());
        }
        if let Err(e) = flavite.settings_set("state.graph_activation_acknowledged", SettingValue::Bool(self.prefs.graph_activation_acknowledged)) {
            any_error = Some(e.to_string());
        }
        
        // Shortcuts - this one is more complex as it's a map
        let shortcuts_map: std::collections::BTreeMap<String, String> = self.prefs.shortcuts.clone();
        if let Err(e) = flavite.settings_set("shortcuts.overrides", SettingValue::ShortcutMap(shortcuts_map)) {
            any_error = Some(e.to_string());
        }
        
        // Update generation (read it back)
        if let Some(error) = any_error {
            self.last_error = Some(format!("Settings were not saved: {}", error));
        } else if let Ok(stored) = flavite.settings_read() {
            self.generation = stored.generation;
            self.last_error = None;
        }
    }

    /// A status-line suffix for load/persist problems (empty when clean).
    #[must_use]
    pub fn status_note(&self) -> Option<String> {
        if let Some(error) = &self.last_error {
            return Some(error.clone());
        }
        if self.warnings.is_empty() {
            None
        } else {
            Some(self.warnings.join("; "))
        }
    }
}

impl crate::app::App {
    /// Reduce a preference edit: validate, apply, refresh derived state,
    /// persist through the device-local user state, and report honestly.
    pub fn update_preferences(
        &mut self,
        update: PreferenceMessage,
    ) -> iced::Task<crate::app::Message> {
        let selecting_package = matches!(update, PreferenceMessage::SetThemePackage(_));
        let changed = match update {
            PreferenceMessage::SetTheme(theme) => self.appearance.set_theme(theme),
            PreferenceMessage::SetScale(percent) => match self.appearance.set_scale(percent) {
                Ok(changed) => changed,
                Err(message) => {
                    self.status = message;
                    return iced::Task::none();
                }
            },
            PreferenceMessage::SetEditorFontSize(size) => {
                match self.appearance.set_editor_font_size(size) {
                    Ok(changed) => changed,
                    Err(message) => {
                        self.status = message;
                        return iced::Task::none();
                    }
                }
            }
            PreferenceMessage::SetLineWidth(width) => self.appearance.set_line_width(width),
            PreferenceMessage::SetReducedMotion(reduced) => {
                self.appearance.set_reduced_motion(reduced)
            }
            PreferenceMessage::SetStableLinks(enabled) => {
                if self.appearance.prefs.prefer_stable_links == enabled {
                    false
                } else {
                    self.appearance.prefs.prefer_stable_links = enabled;
                    true
                }
            }
            PreferenceMessage::SetShortcut { command, chord } => {
                self.appearance.set_shortcut(&command, chord);
                if let Some(error) = &self.appearance.shortcut_error {
                    self.status = error.clone();
                    return iced::Task::none();
                }
                true
            }
            PreferenceMessage::BeginShortcutCapture(command) => {
                self.shortcut_capture = Some(command.clone());
                self.status =
                    format!("Press the new key combination for {command} (Escape cancels)");
                return iced::Task::none();
            }
            PreferenceMessage::ShortcutCaptured { command, chord } => {
                self.shortcut_capture = None;
                match chord {
                    Some(chord) => {
                        self.appearance.set_shortcut(&command, Some(chord));
                        if let Some(error) = &self.appearance.shortcut_error {
                            self.status = error.clone();
                            return iced::Task::none();
                        }
                    }
                    None => self.appearance.shortcut_error = None,
                }
                true
            }
            PreferenceMessage::RestoreShortcuts => {
                self.appearance.restore_default_shortcuts();
                true
            }
            PreferenceMessage::AcknowledgeGraphActivation => {
                self.appearance.acknowledge_graph_activation()
            }
            PreferenceMessage::SetThemePackage(id) => self.appearance.set_theme_package(id),
            PreferenceMessage::ThemeInstallPick => {
                return crate::host::dialogs::pick_theme_file();
            }
            PreferenceMessage::ThemeInstallFromPath(path) => {
                return self.install_theme_package(path);
            }
            PreferenceMessage::ThemeUninstall(id) => {
                return self.uninstall_theme_package(id);
            }
        };
        if !changed {
            return iced::Task::none();
        }
        self.appearance.refresh();
        // A theme-package selection reloads the decoded package before
        // persisting, so the very next frame paints with it.
        if selecting_package && let Some(flavite) = self.flavite.as_ref() {
            self.appearance.reload_theme_package(flavite);
        }
        self.window_geometry_dirty = true;
        if let Some(flavite) = self.flavite.as_mut() {
            self.appearance.persist(flavite);
        }
        if let Some(error) = &self.appearance.last_error {
            self.status = error.clone();
        } else {
            self.status = "Preferences saved".into();
        }
        iced::Task::none()
    }

    /// Convert and install a standard theme package, then surface the
    /// conversion outcome. Runs through the `theme.install` command so
    /// the capability gate and typed results apply.
    fn install_theme_package(
        &mut self,
        path: std::path::PathBuf,
    ) -> iced::Task<crate::app::Message> {
        let Some(flavite) = self.flavite.as_ref() else {
            self.status = "Attach a vault before installing themes".into();
            return iced::Task::none();
        };
        let result = flavite.execute(flavite_api::Invocation::new(
            flavite.command_registry().next_request_id(),
            "theme.install",
            serde_json::json!({ "path": path.display().to_string() }),
            flavite_api::Origin::Gui,
        ));
        self.report_theme_result(result, "Theme installed");
        iced::Task::none()
    }

    /// Remove an installed theme package through the command bus, then
    /// clear the selection when the removed theme was active.
    fn uninstall_theme_package(&mut self, id: String) -> iced::Task<crate::app::Message> {
        let Some(flavite) = self.flavite.as_ref() else {
            self.status = "Attach a vault before managing themes".into();
            return iced::Task::none();
        };
        let result = flavite.execute(flavite_api::Invocation::new(
            flavite.command_registry().next_request_id(),
            "theme.uninstall",
            serde_json::json!({ "id": id }),
            flavite_api::Origin::Gui,
        ));
        let removed = result.is_success();
        self.report_theme_result(result, "Theme removed");
        if removed && self.appearance.prefs.theme_package.as_deref() == Some(id.as_str()) {
            self.appearance.set_theme_package(None);
            if let Some(flavite) = self.flavite.as_ref() {
                self.appearance.reload_theme_package(flavite);
            }
            self.appearance.refresh();
            if let Some(flavite) = self.flavite.as_mut() {
                self.appearance.persist(flavite);
            }
        }
        self.appearance
            .reload_installed_themes(self.flavite.as_ref().expect("vault present above"));
        iced::Task::none()
    }

    /// Translate a theme command result into a status line and refresh
    /// the installed-theme list.
    fn report_theme_result(&mut self, result: flavite_api::ExecutionResult, ok_status: &str) {
        match result {
            flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) => {
                self.status = ok_status.to_owned();
                if success.command_id == "theme.install"
                    && let Some(theme) = success.data.get("theme")
                    && let Ok(installed) =
                        serde_json::from_value::<flavite_api::InstalledTheme>(theme.clone())
                {
                    self.appearance
                        .set_theme_package(Some(installed.id.clone()));
                    if let Some(flavite) = self.flavite.as_ref() {
                        self.appearance.reload_theme_package(flavite);
                    }
                    self.appearance.refresh();
                    if let Some(flavite) = self.flavite.as_mut() {
                        self.appearance.persist(flavite);
                    }
                }
            }
            flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Error(error)) => {
                self.status = format!("Theme operation failed: {}", error.message);
            }
            other => {
                self.status = format!("Theme operation did not complete: {other:?}");
            }
        }
        if let Some(flavite) = self.flavite.as_ref() {
            self.appearance.reload_installed_themes(flavite);
        }
    }
}

/// Validate a shortcut map (compatibility wrapper for flavite_settings::chord::validate_shortcuts).
/// This will be removed in S8 when the dialog is deleted.
pub fn validate_shortcuts(
    shortcuts: &BTreeMap<String, String>,
    owned_defaults: &BTreeMap<String, String>,
) -> Result<(), String> {
    // The input maps are already in the right format for flavite_settings
    flavite_settings::chord::validate_shortcuts(shortcuts, owned_defaults)
        .map_err(|e| e.to_string())
}
