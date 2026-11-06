//! Device-local interface preferences codec (RFC-0003 W11, D-11, D-15).
//!
//! The schema and validation live here — next to the user-state kinds and
//! the app-launch codec — so every host (GUI shell, CLI, examples) reads
//! and writes the same validated document through
//! [`crate::Flavite::read_user_state`] / [`write_user_state`]. The codec is
//! pure data: no toolkit, filesystem, or shell dependency.

use flavite_vault::sync::MutexExt;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

use crate::command::Chord;

/// The current preferences schema version.
pub const PREFERENCES_SCHEMA: u32 = 1;
/// Supported interface scale range (D-15): 75–200%.
pub const MIN_INTERFACE_SCALE_PCT: u16 = 75;
/// Supported interface scale range (D-15): 75–200%.
pub const MAX_INTERFACE_SCALE_PCT: u16 = 200;
/// Supported editor font sizes.
pub const MIN_EDITOR_FONT_SIZE: u16 = 12;
/// Supported editor font sizes.
pub const MAX_EDITOR_FONT_SIZE: u16 = 28;
/// Readable line width in columns (D-15): 40–160.
pub const MIN_LINE_WIDTH: u16 = 40;
/// Readable line width in columns (D-15): 40–160.
pub const MAX_LINE_WIDTH: u16 = 160;

/// Which palette the shell uses.
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

/// The editor's readable line width: bounded columns, or the full pane.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LineWidth {
    /// Wrap text at this many columns (40–160).
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

/// One validated device preference document.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Preferences {
    /// Light/dark/System palette choice.
    pub theme: ThemeChoice,
    /// Interface scale in percent (75–200).
    pub interface_scale_pct: u16,
    /// Editor font size in logical pixels.
    pub editor_font_size: u16,
    /// Readable editor line width.
    pub line_width: LineWidth,
    /// Reduce motion: gate animated transitions and camera easing.
    pub reduced_motion: bool,
    /// RFC-0006 S04 (D-02): the user acknowledged the changed graph
    /// gesture contract (double-click now activates; insertion is the
    /// explicit Insert command). Persisted once, dismissible thereafter.
    pub graph_activation_acknowledged: bool,
    /// Command-id → chord-string overrides for remappable commands.
    pub shortcuts: BTreeMap<String, String>,
    /// Unknown additive fields, preserved verbatim on save.
    unknown: BTreeMap<String, Value>,
}

/// A whole-document parse failure: the file is not a preferences JSON
/// object at all. Field-level invalid values are warnings with defaults.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreferencesError(pub String);

impl std::fmt::Display for PreferencesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PreferencesError {}

/// A parsed document plus the fields that were out of range or invalid
/// and therefore fell back to defaults. The shell surfaces these; the
/// document itself is never corrupted by them.
#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    /// The validated preferences.
    pub preferences: Preferences,
    /// Human-readable warnings for rejected fields.
    pub warnings: Vec<String>,
}

impl Preferences {
    /// The default preferences (System theme, 100% scale).
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            theme: ThemeChoice::System,
            interface_scale_pct: 100,
            editor_font_size: 16,
            line_width: LineWidth::Full,
            reduced_motion: false,
            graph_activation_acknowledged: false,
            shortcuts: BTreeMap::new(),
            unknown: BTreeMap::new(),
        }
    }

    /// Parse a stored document. `None` (absent record) is the defaults.
    ///
    /// # Errors
    /// [`PreferencesError`] when the document is not a JSON object or the
    /// schema major is unknown; individual invalid fields become warnings
    /// with default fallbacks instead.
    pub fn parse(document: Option<&str>) -> Result<Parsed, PreferencesError> {
        let Some(text) = document else {
            return Ok(Parsed {
                preferences: Self::defaults(),
                warnings: Vec::new(),
            });
        };
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Parsed {
                preferences: Self::defaults(),
                warnings: Vec::new(),
            });
        }
        let value: Value = serde_json::from_str(trimmed).map_err(|error| {
            PreferencesError(format!("preferences are not valid JSON: {error}"))
        })?;
        let Value::Object(map) = value else {
            return Err(PreferencesError(
                "preferences document must be a JSON object".into(),
            ));
        };
        match map.get("schema") {
            Some(Value::Number(number))
                if number.as_u64() == Some(u64::from(PREFERENCES_SCHEMA)) => {}
            Some(Value::Number(number)) => {
                return Err(PreferencesError(format!(
                    "unsupported preferences schema {}",
                    number
                )));
            }
            _ => {
                return Err(PreferencesError(
                    "preferences document is missing its schema".into(),
                ));
            }
        }
        let mut warnings = Vec::new();
        let mut unknown = BTreeMap::new();
        for (key, value) in &map {
            if !matches!(
                key.as_str(),
                "schema"
                    | "theme"
                    | "interface_scale_pct"
                    | "editor_font_size"
                    | "line_width"
                    | "reduced_motion"
                    | "graph_activation_acknowledged"
                    | "shortcuts"
            ) {
                unknown.insert(key.clone(), value.clone());
            }
        }
        let theme = match map.get("theme") {
            None => ThemeChoice::default(),
            Some(Value::String(name)) => ThemeChoice::from_name(name).unwrap_or_else(|| {
                warnings.push("theme reset to system (invalid theme name)".into());
                ThemeChoice::default()
            }),
            Some(_) => {
                warnings.push("theme reset to system (invalid theme name)".into());
                ThemeChoice::default()
            }
        };
        let interface_scale_pct = bounded_field(
            &map,
            "interface_scale_pct",
            MIN_INTERFACE_SCALE_PCT,
            MAX_INTERFACE_SCALE_PCT,
            100,
            &mut warnings,
        );
        let editor_font_size = bounded_field(
            &map,
            "editor_font_size",
            MIN_EDITOR_FONT_SIZE,
            MAX_EDITOR_FONT_SIZE,
            16,
            &mut warnings,
        );
        let line_width = match map.get("line_width") {
            None => LineWidth::default(),
            Some(Value::String(text)) if text == "full" => LineWidth::Full,
            Some(Value::Number(number)) => {
                let raw = number.as_f64().unwrap_or_default();
                // JSON cannot carry NaN/Infinity, but a guard keeps the
                // bound explicit and rejects absurd magnitudes before the
                // u16 cast.
                if raw.is_finite()
                    && raw >= f64::from(MIN_LINE_WIDTH)
                    && raw <= f64::from(MAX_LINE_WIDTH)
                {
                    LineWidth::Columns(raw as u16)
                } else {
                    warnings.push(format!(
                        "line_width must be {MIN_LINE_WIDTH}–{MAX_LINE_WIDTH} or \"full\""
                    ));
                    LineWidth::default()
                }
            }
            Some(_) => {
                warnings.push("line_width must be 40–160 or \"full\"".into());
                LineWidth::default()
            }
        };
        let reduced_motion = match map.get("reduced_motion") {
            None => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => {
                warnings.push("reduced_motion must be true or false".into());
                false
            }
        };
        let graph_activation_acknowledged = match map.get("graph_activation_acknowledged") {
            None => false,
            Some(Value::Bool(flag)) => *flag,
            Some(_) => {
                warnings.push("graph_activation_acknowledged must be true or false".into());
                false
            }
        };
        let shortcuts = match map.get("shortcuts") {
            None => BTreeMap::new(),
            Some(Value::Object(entries)) => {
                let mut shortcuts = BTreeMap::new();
                for (id, chord) in entries {
                    match chord.as_str() {
                        Some(text) if parse_chord(text).is_some() => {
                            shortcuts.insert(id.clone(), text.to_owned());
                        }
                        _ => warnings
                            .push(format!("shortcut for {id} was ignored (unreadable chord)")),
                    }
                }
                shortcuts
            }
            Some(_) => {
                warnings.push("shortcuts must be an object of command ids to chords".into());
                BTreeMap::new()
            }
        };
        Ok(Parsed {
            preferences: Self {
                theme,
                interface_scale_pct,
                editor_font_size,
                line_width,
                reduced_motion,
                graph_activation_acknowledged,
                shortcuts,
                unknown,
            },
            warnings,
        })
    }

    /// Encode the document, preserving unknown additive fields.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("schema".into(), Value::from(PREFERENCES_SCHEMA));
        map.insert("theme".into(), Value::from(self.theme.name()));
        map.insert(
            "interface_scale_pct".into(),
            Value::from(self.interface_scale_pct),
        );
        map.insert(
            "editor_font_size".into(),
            Value::from(self.editor_font_size),
        );
        map.insert(
            "line_width".into(),
            match self.line_width {
                LineWidth::Columns(columns) => Value::from(columns),
                LineWidth::Full => Value::from("full"),
            },
        );
        map.insert("reduced_motion".into(), Value::from(self.reduced_motion));
        map.insert(
            "graph_activation_acknowledged".into(),
            Value::from(self.graph_activation_acknowledged),
        );
        let shortcuts = self
            .shortcuts
            .iter()
            .map(|(id, chord)| (id.clone(), Value::from(chord.as_str())))
            .collect::<Map<String, Value>>();
        map.insert("shortcuts".into(), Value::Object(shortcuts));
        for (key, value) in &self.unknown {
            map.insert(key.clone(), value.clone());
        }
        Value::Object(map)
    }

    /// Encode as the stored JSON text.
    #[must_use]
    pub fn to_stored(&self) -> String {
        self.to_json().to_string()
    }
}

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
                warnings.push(format!("{key} must be {min}–{max}"));
                default
            }
        }
        Some(_) => {
            warnings.push(format!("{key} must be a number"));
            default
        }
    }
}

/// Parse a chord string like `Mod+P`, `Ctrl+Alt+Right`, or `F6` into the
/// canonical [`Chord`] used by the command catalog. `Mod` is
/// the platform command modifier (the API's CTRL bit).
#[must_use]
pub fn parse_chord(text: &str) -> Option<Chord> {
    use crate::command::modifiers as mods;
    let mut modifiers = mods::NONE;
    let mut key: Option<String> = None;
    for token in text.split('+') {
        match token.trim() {
            "Mod" if key.is_none() => modifiers |= mods::CTRL,
            "Shift" if key.is_none() => modifiers |= mods::SHIFT,
            "Alt" if key.is_none() => modifiers |= mods::ALT,
            "Ctrl" if key.is_none() => modifiers |= mods::CTRL,
            "Super" if key.is_none() => modifiers |= mods::SUPER,
            other => {
                if key.is_some() || other.is_empty() {
                    return None;
                }
                canonical_key(other)?;
                key = Some(other.trim().to_owned());
            }
        }
    }
    let key = key?;
    Some(Chord { modifiers, key })
}

fn canonical_key(token: &str) -> Option<()> {
    const NAMED: &[&str] = &[
        "Enter",
        "Return",
        "Tab",
        "Escape",
        "Esc",
        "Backspace",
        "Delete",
        "Del",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "ArrowUp",
        "Up",
        "ArrowDown",
        "Down",
        "ArrowLeft",
        "Left",
        "ArrowRight",
        "Right",
    ];
    if NAMED.contains(&token) {
        return Some(());
    }
    if let Some(rest) = token.strip_prefix(['F', 'f']) {
        // A function key is F + digits parsing into 1–24; an empty rest
        // (the bare letter "F") fails the parse and falls through to the
        // printable single-character rule below.
        let is_function_key = !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
            && rest
                .parse::<u8>()
                .is_ok_and(|number| (1..=24).contains(&number));
        if is_function_key {
            return Some(());
        }
        // Not a function key: fall through to the single-character rule
        // (the literal keys "F"/"f" are valid printable chords).
    }
    // Printable single characters (case-insensitive letters keep their
    // uppercase display form).
    let mut characters = token.chars();
    match (characters.next(), characters.next()) {
        (Some(_), None) => Some(()),
        _ => None,
    }
}

/// The canonical display/serialization form of a chord
/// (`Mod+Shift+P`, `F6`, `Ctrl+Alt+Right`).
#[must_use]
pub fn chord_string(chord: &Chord) -> String {
    use crate::command::modifiers as mods;
    let mut parts = Vec::new();
    if chord.modifiers & mods::CTRL != 0 {
        parts.push("Mod");
    }
    if chord.modifiers & mods::ALT != 0 {
        parts.push("Alt");
    }
    if chord.modifiers & mods::SHIFT != 0 {
        parts.push("Shift");
    }
    if chord.modifiers & mods::SUPER != 0 {
        parts.push("Super");
    }
    parts.push(&chord.key);
    parts.join("+")
}

/// Why a shortcut map was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShortcutError {
    /// A chord string did not parse.
    UnreadableChord { command: String, chord: String },
    /// Two commands resolve to the same chord.
    Collision {
        chord: String,
        first: String,
        second: String,
    },
    /// The chord is owned by the operating system / desktop environment.
    OsReserved { chord: String, owner: &'static str },
}

impl std::fmt::Display for ShortcutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnreadableChord { command, chord } => {
                write!(f, "{command}: {chord:?} is not a readable chord")
            }
            Self::Collision {
                chord,
                first,
                second,
            } => write!(
                f,
                "{chord} is already used by {first}; choose another for {second}"
            ),
            Self::OsReserved { chord, owner } => {
                write!(
                    f,
                    "{chord} is reserved by {owner}; choose another chord for it"
                )
            }
        }
    }
}

impl std::error::Error for ShortcutError {}

/// Intern a chord slice for the catalog's `&'static [Chord]` key lists.
/// The cache is keyed by the serialized chords, so it is bounded by the
/// number of distinct chords ever resolved, not by calls.
#[must_use]
pub fn intern_chords(chords: &[Chord]) -> &'static [Chord] {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static INTERNED: OnceLock<Mutex<HashMap<String, &'static [Chord]>>> = OnceLock::new();
    let key = chords
        .iter()
        .map(chord_string)
        .collect::<Vec<_>>()
        .join(";");
    let cache = INTERNED.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = cache.lock_recovering();
    if let Some(slice) = guard.get(&key) {
        return slice;
    }
    let leaked: &'static [Chord] = Box::leak(chords.to_vec().into_boxed_slice());
    guard.insert(key, leaked);
    leaked
}

/// Validate a command-id → chord-string override map: every chord must
/// parse, two commands must not share a chord, and OS-reserved chords are
/// refused with their owner so the editor can offer a clear alternate
/// action (W11 input catalog).
///
/// # Errors
/// [`ShortcutError`] naming the offending command and chord.
pub fn validate_shortcuts(shortcuts: &BTreeMap<String, String>) -> Result<(), ShortcutError> {
    let mut resolved: std::collections::HashMap<Chord, String> = std::collections::HashMap::new();
    for (command, text) in shortcuts {
        let chord = parse_chord(text).ok_or_else(|| ShortcutError::UnreadableChord {
            command: command.clone(),
            chord: text.clone(),
        })?;
        if let Some(owner) = os_reserved(&chord) {
            return Err(ShortcutError::OsReserved {
                chord: chord_string(&chord),
                owner,
            });
        }
        let serialized = chord_string(&chord);
        if let Some(previous) = resolved.insert(chord, command.clone()) {
            // Rebinding a command to its own current chord is a no-op, not
            // a collision.
            if previous != *command {
                return Err(ShortcutError::Collision {
                    chord: serialized,
                    first: previous,
                    second: command.clone(),
                });
            }
        }
    }
    Ok(())
}

/// The operating-system-reserved chords the shell must never claim, with
/// the owner named for the user-facing explanation. The editor offers a
/// clear alternate action for these instead of accepting a conflict the
/// desktop will swallow (W11 input catalog).
#[must_use]
pub fn os_reserved(chord: &Chord) -> Option<&'static str> {
    use crate::command::modifiers as mods;
    let command = chord.modifiers & mods::CTRL != 0;
    let alt = chord.modifiers & mods::ALT != 0;
    let shift = chord.modifiers & mods::SHIFT != 0;
    let super_key = chord.modifiers & mods::SUPER != 0;
    match chord.key.as_str() {
        // Close the focused window on every supported desktop.
        "F4" if alt && !command && !shift && !super_key => Some("the desktop (close window)"),
        // X11 zap binding where enabled; the desktop exits the session.
        "Backspace" if command && alt => Some("the desktop (session kill switch)"),
        // Workspace navigation switches on GNOME and KDE.
        "ArrowUp" | "ArrowDown" | "ArrowLeft" | "ArrowRight" if command && alt => {
            Some("the desktop (workspace switching)")
        }
        // The compositor's own application/overview switchers.
        "Tab" if alt => Some("the desktop (application switching)"),
        _ => None,
    }
}
