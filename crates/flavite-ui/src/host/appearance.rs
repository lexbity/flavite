//! Interface appearance host (RFC-0003 W11, D-11, D-15): device-local
//! preferences, the resolved color scheme, scale-aware metrics, and
//! preference persistence through the API user-state service.

use crate::preferences::{self, LineWidth, Preferences, ThemeChoice};
use crate::style::{self, Metrics, Scheme};
use flavite_api::{Flavite, UserStateKind};

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
    metrics: Metrics,
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

    /// The resolved iced theme for the current scheme.
    #[must_use]
    pub fn theme(&self) -> iced::Theme {
        style::theme(self.scheme())
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
        for (id, text) in &next {
            effective.insert(id.clone(), text.clone());
        }
        if let Err(error) = preferences::validate_shortcuts(&effective) {
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
        };
        if !changed {
            return iced::Task::none();
        }
        self.appearance.refresh();
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
}
