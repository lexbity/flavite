//! Interface appearance host (RFC-0003 W11, RFC-0007 D-13): the resolved
//! vault settings, the color scheme, scale-aware metrics, theme packages,
//! and the effective chord table.
//!
//! The host holds no parallel preference state: every value reads from the
//! resolved settings document (RFC-0007 §4.4), adopted at attach, after
//! every funnel edit, and on focus-gained external adoption. Writes never
//! happen here — [`crate::host::settings`] owns the single edit pipeline.

use crate::style::{self, Metrics, Scheme};
use flavite_api::{Flavite, InstalledTheme};
use flavite_settings::resolve::Resolved;
use flavite_settings::value::SettingValue;
use flavite_theme::{SemanticTheme, ThemeImage};
use std::sync::LazyLock;
use std::time::Instant;

/// The built-in registry, shared by every accessor: the descriptor table
/// is the single authority for defaults (RFC-0007 D-04).
static REGISTRY: LazyLock<flavite_settings::registry::SettingRegistry> =
    LazyLock::new(flavite_settings::registry::SettingRegistry::with_builtins);

/// The registry default of one setting (the D-04 table).
fn default_value(id: &str) -> SettingValue {
    REGISTRY
        .lookup(id)
        .map(|descriptor| descriptor.default.clone())
        .unwrap_or_default()
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
    /// parse fallbacks), surfaced in the Settings tab.
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

/// The resolved appearance state for the shell.
#[derive(Debug, Default)]
pub struct AppearanceHost {
    /// The resolved effective values (document overlay on registry
    /// defaults). `None` before a vault attaches or when the document
    /// could not be read: accessors fall back to the registry defaults.
    resolved: Option<Resolved>,
    /// The OS color scheme, when the runtime reports one (`None` = no
    /// preference reported: System resolves to dark, the historical look).
    system_dark: Option<bool>,
    /// The last settings-load failure, surfaced in the status line.
    pub last_error: Option<String>,
    /// The last shortcut-validation failure for the editor UI.
    pub shortcut_error: Option<String>,
    /// The resolved chord table: default chords with any validated
    /// overrides applied. Rebuilt on every adoption.
    pub shortcuts: Vec<(flavite_api::Chord, &'static str)>,
    /// The loaded theme package, when one is selected and loads.
    pub theme_package: Option<ThemePackage>,
    /// The installed theme packages, for the Settings tab.
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

impl AppearanceHost {
    /// Boot defaults before any vault is attached.
    #[must_use]
    pub fn boot() -> Self {
        let mut host = Self {
            resolved: None,
            system_dark: None,
            last_error: None,
            shortcut_error: None,
            shortcuts: Vec::new(),
            theme_package: None,
            installed_themes: Vec::new(),
            theme_error: None,
            frame_index: 0,
            frame_started: None,
            metrics: Metrics::default(),
        };
        host.refresh();
        host
    }

    /// Adopt the resolved settings at vault attach. A read failure is an
    /// honest report with registry defaults rendered; the document is
    /// never overwritten by that fallback (D-12, RFC-0007).
    pub fn load(&mut self, flavite: &Flavite) {
        match flavite.settings_read() {
            Ok(stored) => {
                self.last_error = None;
                self.apply_resolved(stored.resolved);
            }
            Err(error) => {
                self.last_error = Some(format!("Settings could not be read: {error}"));
                self.resolved = None;
                self.refresh();
            }
        }
    }

    /// Adopt resolved settings into live state (RFC-0007 D-13: Live
    /// apply). Derived state (metrics, chord table) rebuilds; a changed
    /// theme package is reloaded by the caller after the write.
    pub fn apply_resolved(&mut self, resolved: Resolved) {
        self.resolved = Some(resolved);
        self.refresh();
    }

    /// The effective value of one setting (resolved overlay on the
    /// registry default).
    #[must_use]
    pub fn value(&self, id: &str) -> SettingValue {
        self.resolved
            .as_ref()
            .and_then(|resolved| resolved.get(id))
            .cloned()
            .unwrap_or_else(|| default_value(id))
    }

    /// The chosen palette source (`system`, `light`, or `dark`).
    #[must_use]
    pub fn theme_name(&self) -> String {
        self.value("appearance.theme").as_str().unwrap_or("system").to_owned()
    }

    /// The interface scale in percent.
    #[must_use]
    pub fn scale_pct(&self) -> u16 {
        u16::try_from(self.value("appearance.scale_pct").as_int().unwrap_or(100)).unwrap_or(100)
    }

    /// The editor font size in logical pixels.
    #[must_use]
    pub fn editor_font_size(&self) -> u16 {
        u16::try_from(self.value("editor.font_size").as_int().unwrap_or(14)).unwrap_or(14)
    }

    /// The readable editor line width in columns (`None` = full pane).
    #[must_use]
    pub fn line_width_columns(&self) -> Option<u16> {
        match self.value("editor.line_width") {
            SettingValue::Int(columns) => u16::try_from(columns).ok(),
            SettingValue::Str(text) if text == "full" => None,
            _ => Some(80),
        }
    }

    /// Whether the collapsible vault navigator is open.
    #[must_use]
    pub fn sidebar_open(&self) -> bool {
        self.value("sidebar.open").as_bool().unwrap_or(true)
    }

    /// The notes-sidebar width in logical pixels.
    #[must_use]
    pub fn sidebar_width(&self) -> u16 {
        u16::try_from(self.value("sidebar.width").as_int().unwrap_or(260)).unwrap_or(260)
    }

    /// Whether motion is reduced.
    #[must_use]
    pub fn reduced_motion(&self) -> bool {
        self.value("appearance.reduced_motion").as_bool().unwrap_or(false)
    }

    /// Whether stable IDs are preferred when inserting links.
    #[must_use]
    pub fn prefer_stable_links(&self) -> bool {
        self.value("links.prefer_stable_links").as_bool().unwrap_or(true)
    }

    /// Whether the graph gesture-contract notice was acknowledged.
    #[must_use]
    pub fn graph_activation_acknowledged(&self) -> bool {
        self.value("state.graph_activation_acknowledged").as_bool().unwrap_or(false)
    }

    /// The selected theme package id, when one is selected.
    #[must_use]
    pub fn theme_package_id(&self) -> Option<String> {
        match self.value("appearance.theme_package") {
            SettingValue::Str(id) if !id.is_empty() => Some(id),
            _ => None,
        }
    }

    /// The shortcut override map (command id → chord string).
    #[must_use]
    pub fn shortcut_overrides(&self) -> std::collections::BTreeMap<String, String> {
        match self.value("shortcuts.overrides") {
            SettingValue::ShortcutMap(map) => map,
            _ => std::collections::BTreeMap::new(),
        }
    }

    /// Whether the graph gesture-contract explanation still owes the user
    /// its first-launch dismissal (RFC-0006 D-02).
    #[must_use]
    pub fn graph_activation_notice_pending(&self) -> bool {
        !self.graph_activation_acknowledged()
    }

    /// The color scheme the shell currently paints with.
    #[must_use]
    pub fn scheme(&self) -> Scheme {
        match self.theme_name().as_str() {
            "light" => Scheme::Light,
            "dark" => Scheme::Dark,
            // System resolves to dark when the OS reports nothing.
            _ => {
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
        if !frames.is_animated() || self.reduced_motion() {
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

    /// Load the installed-theme list for the Settings tab.
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
        let Some(id) = self.theme_package_id() else {
            self.theme_package = None;
            return;
        };
        let Some(store) = flavite.themes() else {
            self.theme_error =
                Some("Theme packages are unavailable (no theme store attached)".into());
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

    /// The scale-aware metrics for the current settings.
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

    /// Refresh derived state after any adoption: the scale-aware metrics
    /// and the effective chord table.
    pub fn refresh(&mut self) {
        self.metrics = Metrics::new(
            self.scale_pct(),
            self.editor_font_size(),
            self.line_width_columns(),
            self.sidebar_width(),
        );
        // Overrides were validated at funnel time; an unparseable leftover
        // falls back to the default chord rather than dropping the
        // command's binding.
        let overrides = self.shortcut_overrides();
        self.shortcuts = crate::host::commands::DEFAULT_CHORDS
            .iter()
            .filter_map(|(id, default)| {
                let text = overrides
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

    /// A status-line suffix for load problems (empty when clean).
    #[must_use]
    pub fn status_note(&self) -> Option<String> {
        if let Some(error) = &self.last_error {
            return Some(error.clone());
        }
        None
    }
}

impl crate::app::App {
    /// Convert and install a standard theme package, then surface the
    /// conversion outcome. Runs through the `theme.install` command so
    /// the capability gate and typed results apply; the selection
    /// persists through the settings funnel.
    pub fn install_theme_package(
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
    pub fn uninstall_theme_package(&mut self, id: String) -> iced::Task<crate::app::Message> {
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
        if removed && self.appearance.theme_package_id().as_deref() == Some(id.as_str()) {
            let _ = self.apply_setting("appearance.theme_package", SettingValue::Str(String::new()));
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
                    // The selection persists through the funnel; the funnel
                    // applies the resolved values and reloads the package.
                    let _ = self.apply_setting(
                        "appearance.theme_package",
                        SettingValue::Str(installed.id.clone()),
                    );
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
