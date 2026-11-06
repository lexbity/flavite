//! Interface appearance host (RFC-0003 W11, D-11, D-15): device-local
//! preferences, the resolved color scheme, scale-aware metrics, theme
//! packages, and preference persistence through the API user-state
//! service.

use crate::preferences::{self, LineWidth, Preferences, ThemeChoice};
use crate::style::{self, Metrics, Scheme};
use flavite_api::{Flavite, InstalledTheme, UserStateKind};
use flavite_theme::{SemanticTheme, ThemeImage};
use std::time::Instant;

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
        let prefs = Preferences::defaults();
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

    /// Load device preferences at vault attach. A read or parse failure is
    /// an honest report with defaults applied; the document is never
    /// overwritten by that fallback (D-12).
    pub fn load(&mut self, flavite: &Flavite) {
        match flavite.read_user_state(UserStateKind::DevicePreferences) {
            Ok(None) => {
                self.valid = true;
                self.generation = 0;
            }
            Ok(Some(stored)) => {
                self.generation = stored.generation;
                let text = std::str::from_utf8(&stored.payload).ok();
                match preferences::Preferences::parse(text) {
                    Ok(parsed) => {
                        self.prefs = parsed.preferences;
                        self.warnings = parsed.warnings;
                        self.valid = true;
                    }
                    Err(error) => {
                        self.valid = false;
                        self.last_error = Some(format!("Preferences could not be read: {error}"));
                    }
                }
            }
            Err(error) => {
                self.valid = false;
                self.last_error = Some(format!("Preferences could not be read: {error}"));
            }
        }
        self.metrics = Metrics::from_preferences(&self.prefs);
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
        if !(preferences::MIN_INTERFACE_SCALE_PCT..=preferences::MAX_INTERFACE_SCALE_PCT)
            .contains(&percent)
        {
            return Err(format!(
                "Interface scale must be {}–{}%",
                preferences::MIN_INTERFACE_SCALE_PCT,
                preferences::MAX_INTERFACE_SCALE_PCT
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
        if !(preferences::MIN_EDITOR_FONT_SIZE..=preferences::MAX_EDITOR_FONT_SIZE).contains(&size)
        {
            return Err(format!(
                "Editor font size must be {}–{}",
                preferences::MIN_EDITOR_FONT_SIZE,
                preferences::MAX_EDITOR_FONT_SIZE
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
        if let Err(error) = preferences::validate_shortcuts(&effective, &owned_defaults) {
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
                let chord =
                    preferences::parse_chord(text).or_else(|| preferences::parse_chord(default))?;
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

    /// Persist the current preferences. Failures are recorded and
    /// surfaced, never silent (FR-20).
    pub fn persist(&mut self, flavite: &mut Flavite) {
        if !self.valid {
            return;
        }
        let document = self.prefs.to_stored();
        let write = flavite.write_user_state(
            UserStateKind::DevicePreferences,
            self.generation,
            document.as_bytes(),
        );
        match write {
            Ok(generation) => {
                self.generation = generation;
                self.last_error = None;
            }
            Err(error) => {
                self.last_error = Some(format!("Preferences were not saved: {error}"));
            }
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
