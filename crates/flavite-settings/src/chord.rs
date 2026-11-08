//! Keyboard chord parsing and validation.
//!
//! This module provides the authoritative implementation of the chord grammar
//! used throughout flavite. It is the single source of truth for:
//! - Parsing chord strings (e.g., "Mod+S", "Ctrl+Alt+ArrowRight")
//! - Formatting chords for display
//! - Validating shortcuts (no collisions, no OS-reserved chords)
//!
//! A chord consists of zero or more modifier keys (Mod, Ctrl, Shift, Alt, Super)
//! followed by exactly one key (printable character, function key, or named key).

use std::collections::BTreeMap;

/// Modifier bit flags.
pub mod modifiers {
    /// No modifier.
    pub const NONE: u8 = 0b0000;
    /// Shift key.
    pub const SHIFT: u8 = 0b0001;
    /// Literal Control key on every platform.
    pub const CTRL: u8 = 0b0010;
    /// Alt key (Option on macOS).
    pub const ALT: u8 = 0b0100;
    /// Super key (Windows key on Windows, Command on macOS).
    pub const SUPER: u8 = 0b1000;
    /// Platform action modifier: Command on macOS, Control elsewhere.
    pub const MOD: u8 = 0b1_0000;
}

/// Resolve semantic platform modifiers to literal native modifier bits.
#[must_use]
pub const fn physical_modifiers(bits: u8, macos: bool) -> u8 {
    let literal = bits & !modifiers::MOD;
    if bits & modifiers::MOD != 0 {
        literal
            | if macos {
                modifiers::SUPER
            } else {
                modifiers::CTRL
            }
    } else {
        literal
    }
}

/// A keyboard chord (combination of modifier and key).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Chord {
    /// Modifier keys bitmask.
    pub modifiers: u8,
    /// The key.
    pub key: String,
}

impl Chord {
    /// Create a new chord with the given modifiers and key.
    #[must_use]
    pub fn new(modifiers: u8, key: &str) -> Self {
        Self {
            modifiers,
            key: key.to_string(),
        }
    }
}

/// Parse a chord string like `Mod+P`, `Ctrl+Alt+ArrowRight`, or `F6` into a `Chord`.
///
/// The grammar:
/// - Modifiers: Mod, Ctrl, Shift, Alt, Super (case-insensitive but canonical form is as named)
/// - Key: printable single character, function key (F1-F24), or named key
/// - Separator: `+` (with optional surrounding whitespace)
///
/// Returns `None` if the string does not parse.
#[must_use]
pub fn parse_chord(text: &str) -> Option<Chord> {
    let mut modifiers = modifiers::NONE;
    let mut key: Option<String> = None;

    for token in text.split('+') {
        let token = token.trim();
        match token {
            "Mod" | "Ctrl" | "Shift" | "Alt" | "Super" if key.is_none() => {
                let flag = match token {
                    "Mod" => modifiers::MOD,
                    "Ctrl" => modifiers::CTRL,
                    "Shift" => modifiers::SHIFT,
                    "Alt" => modifiers::ALT,
                    _ => modifiers::SUPER,
                };
                if physical_modifiers(modifiers, cfg!(target_os = "macos"))
                    & physical_modifiers(flag, cfg!(target_os = "macos"))
                    != 0
                {
                    return None;
                }
                modifiers |= flag;
            }
            other => {
                if key.is_some() || other.is_empty() {
                    return None;
                }
                // canonical_key returns Option<()>, and we want to return None if it's None
                #[allow(clippy::question_mark)]
                if canonical_key(other).is_none() {
                    return None;
                }
                key = Some(other.trim().to_owned());
            }
        }
    }

    let key = key?;
    Some(Chord { modifiers, key })
}

/// Check if a key token is a valid, canonical key name.
/// Valid keys are: named keys, function keys (F1-F24), or single printable characters.
fn canonical_key(token: &str) -> Option<()> {
    const NAMED: &[&str] = &[
        "Enter",
        "Space",
        "Tab",
        "Escape",
        "Backspace",
        "Delete",
        "Home",
        "End",
        "PageUp",
        "PageDown",
        "ArrowUp",
        "ArrowDown",
        "ArrowLeft",
        "ArrowRight",
    ];

    if NAMED.contains(&token) {
        return Some(());
    }

    if let Some(rest) = token.strip_prefix(['F', 'f']) {
        // A function key is F + digits parsing into 1–24
        let is_function_key = !rest.is_empty()
            && rest.chars().all(|c| c.is_ascii_digit())
            && rest
                .parse::<u8>()
                .is_ok_and(|number| (1..=24).contains(&number));
        if is_function_key {
            return Some(());
        }
        // Not a function key: fall through to the single-character rule
    }

    // Printable single characters
    let mut characters = token.chars();
    match (characters.next(), characters.next()) {
        (Some(_), None) => Some(()),
        _ => None,
    }
}

/// Format a chord for display or serialization.
///
/// The canonical form orders modifiers as: Mod, Alt, Shift, Super, then the key.
/// For example: "Mod+Alt+Shift+P", "F6", "Ctrl+Alt+Right".
#[must_use]
pub fn chord_string(chord: &Chord) -> String {
    let mut parts = Vec::new();

    // Order: Ctrl/Mod, Alt, Shift, Super
    if chord.modifiers & modifiers::MOD != 0 {
        parts.push("Mod");
    }
    if chord.modifiers & modifiers::CTRL != 0 {
        parts.push("Ctrl");
    }
    if chord.modifiers & modifiers::ALT != 0 {
        parts.push("Alt");
    }
    if chord.modifiers & modifiers::SHIFT != 0 {
        parts.push("Shift");
    }
    if chord.modifiers & modifiers::SUPER != 0 {
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
                write!(f, "{chord} is reserved by {owner}; choose another chord")
            }
        }
    }
}

impl std::error::Error for ShortcutError {}

/// Validate a command-id → chord-string override map.
///
/// Every chord must parse, no two commands may share a chord, and OS-reserved
/// chords are refused.
///
/// # Arguments
/// * `shortcuts` - The override map to validate
/// * `owned_defaults` - The default shortcuts (chord overrides that match defaults are allowed)
///
/// # Errors
/// Returns `ShortcutError` naming the offending command and chord.
pub fn validate_shortcuts(
    shortcuts: &BTreeMap<String, String>,
    owned_defaults: &BTreeMap<String, String>,
) -> Result<(), ShortcutError> {
    let mut resolved: std::collections::HashMap<Chord, String> = std::collections::HashMap::new();

    for (command, text) in shortcuts {
        let chord = parse_chord(text).ok_or_else(|| ShortcutError::UnreadableChord {
            command: command.clone(),
            chord: text.clone(),
        })?;

        // Check if OS-reserved (only if it's not overriding the default)
        if let Some(owner) = os_reserved(&chord)
            && owned_defaults.get(command) != Some(text)
        {
            return Err(ShortcutError::OsReserved {
                chord: chord_string(&chord),
                owner,
            });
        }

        let serialized = chord_string(&chord);
        let physical = Chord {
            modifiers: physical_modifiers(chord.modifiers, cfg!(target_os = "macos")),
            key: chord.key.to_uppercase(),
        };
        if let Some(previous) = resolved.insert(physical, command.clone()) {
            // Rebinding a command to its own current chord is a no-op, not a collision
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
/// the owner named for the user-facing explanation.
#[must_use]
pub fn os_reserved(chord: &Chord) -> Option<&'static str> {
    let command =
        physical_modifiers(chord.modifiers, cfg!(target_os = "macos")) & modifiers::CTRL != 0;
    let alt = chord.modifiers & modifiers::ALT != 0;
    let shift = chord.modifiers & modifiers::SHIFT != 0;
    let super_key = chord.modifiers & modifiers::SUPER != 0;

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

/// Parse a chord string and get its canonical display form.
/// Returns None if the chord doesn't parse.
#[must_use]
pub fn canonical_chord(text: &str) -> Option<String> {
    parse_chord(text).map(|c| chord_string(&c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_chord() {
        assert_eq!(
            parse_chord("Mod+S"),
            Some(Chord {
                modifiers: modifiers::MOD,
                key: "S".to_owned()
            })
        );
        assert_eq!(
            parse_chord("Ctrl+S"),
            Some(Chord {
                modifiers: modifiers::CTRL,
                key: "S".to_owned()
            })
        );
    }

    #[test]
    fn test_parse_multi_modifier() {
        assert_eq!(
            parse_chord("Mod+Alt+Shift+P"),
            Some(Chord {
                modifiers: modifiers::MOD | modifiers::ALT | modifiers::SHIFT,
                key: "P".to_owned()
            })
        );
    }

    #[test]
    fn test_parse_function_key() {
        assert_eq!(
            parse_chord("F6"),
            Some(Chord {
                modifiers: modifiers::NONE,
                key: "F6".to_owned()
            })
        );
        assert_eq!(
            parse_chord("Mod+F12"),
            Some(Chord {
                modifiers: modifiers::MOD,
                key: "F12".to_owned()
            })
        );
    }

    #[test]
    fn test_parse_named_key() {
        assert_eq!(
            parse_chord("Enter"),
            Some(Chord {
                modifiers: modifiers::NONE,
                key: "Enter".to_owned()
            })
        );
        assert_eq!(
            parse_chord("Escape"),
            Some(Chord {
                modifiers: modifiers::NONE,
                key: "Escape".to_owned()
            })
        );
    }

    #[test]
    fn test_parse_whitespace_handling() {
        assert_eq!(
            parse_chord("Mod + S"),
            Some(Chord {
                modifiers: modifiers::MOD,
                key: "S".to_owned()
            })
        );
        assert_eq!(
            parse_chord("  Mod  +  S  "),
            Some(Chord {
                modifiers: modifiers::MOD,
                key: "S".to_owned()
            })
        );
    }

    #[test]
    fn test_parse_invalid() {
        assert_eq!(parse_chord(""), None);
        assert_eq!(parse_chord("Mod"), None); // No key - modifiers without key
        assert_eq!(parse_chord("Mod+"), None); // No key
        assert_eq!(parse_chord("Mod+Shift"), None); // No key - only modifiers
        assert_eq!(parse_chord("Mod+Mod+S"), None);
        assert_eq!(parse_chord("S+Mod"), None); // Modifier after key
    }

    #[test]
    fn test_chord_string() {
        let chord = Chord {
            modifiers: modifiers::MOD | modifiers::SHIFT,
            key: "S".to_owned(),
        };
        assert_eq!(chord_string(&chord), "Mod+Shift+S");

        let chord = Chord {
            modifiers: modifiers::ALT | modifiers::SUPER,
            key: "F4".to_owned(),
        };
        assert_eq!(chord_string(&chord), "Alt+Super+F4");
    }

    #[test]
    fn test_round_trip() {
        let original = "Mod+Alt+Shift+P";
        let parsed = parse_chord(original).unwrap();
        let formatted = chord_string(&parsed);

        // Note: order may differ (Mod vs Ctrl), but the chord is equivalent
        let reparsed = parse_chord(&formatted).unwrap();
        assert_eq!(parsed.modifiers, reparsed.modifiers);
        assert_eq!(parsed.key, reparsed.key);
    }

    #[test]
    fn test_os_reserved() {
        // Alt+F4 is reserved
        let chord = parse_chord("Alt+F4").unwrap();
        assert_eq!(os_reserved(&chord), Some("the desktop (close window)"));

        // Ctrl+Alt+Backspace is reserved
        let chord = parse_chord("Ctrl+Alt+Backspace").unwrap();
        assert_eq!(
            os_reserved(&chord),
            Some("the desktop (session kill switch)")
        );

        // Ctrl+Alt+Arrow keys are reserved
        let chord = parse_chord("Ctrl+Alt+ArrowLeft").unwrap();
        assert_eq!(
            os_reserved(&chord),
            Some("the desktop (workspace switching)")
        );

        // Alt+Tab is reserved
        let chord = parse_chord("Alt+Tab").unwrap();
        assert_eq!(
            os_reserved(&chord),
            Some("the desktop (application switching)")
        );

        // Regular chords are not reserved
        let chord = parse_chord("Mod+S").unwrap();
        assert_eq!(os_reserved(&chord), None);
    }

    #[test]
    fn test_validate_shortcuts() {
        let mut shortcuts = BTreeMap::new();
        shortcuts.insert("note.save".to_owned(), "Mod+S".to_owned());
        shortcuts.insert("note.new".to_owned(), "Mod+N".to_owned());

        let defaults = BTreeMap::new();
        assert!(validate_shortcuts(&shortcuts, &defaults).is_ok());
    }

    #[test]
    fn test_validate_shortcuts_collision() {
        let mut shortcuts = BTreeMap::new();
        shortcuts.insert("note.save".to_owned(), "Mod+S".to_owned());
        shortcuts.insert("note.new".to_owned(), "Mod+S".to_owned());

        let defaults = BTreeMap::new();
        let result = validate_shortcuts(&shortcuts, &defaults);
        assert!(matches!(result, Err(ShortcutError::Collision { .. })));
    }

    #[test]
    fn test_validate_shortcuts_os_reserved() {
        let mut shortcuts = BTreeMap::new();
        shortcuts.insert("note.close".to_owned(), "Alt+F4".to_owned());

        let defaults = BTreeMap::new();
        let result = validate_shortcuts(&shortcuts, &defaults);
        assert!(matches!(result, Err(ShortcutError::OsReserved { .. })));
    }

    #[test]
    fn test_validate_shortcuts_unreadable() {
        let mut shortcuts = BTreeMap::new();
        shortcuts.insert("note.save".to_owned(), "Invalid+Chord".to_owned());

        let defaults = BTreeMap::new();
        let result = validate_shortcuts(&shortcuts, &defaults);
        assert!(matches!(result, Err(ShortcutError::UnreadableChord { .. })));
    }

    #[test]
    fn test_validate_shortcuts_os_reserved_override_default_allowed() {
        // If a command's default is an OS-reserved chord, overriding to the same chord is allowed
        let mut shortcuts = BTreeMap::new();
        shortcuts.insert("note.close".to_owned(), "Alt+F4".to_owned());

        let mut defaults = BTreeMap::new();
        defaults.insert("note.close".to_owned(), "Alt+F4".to_owned());

        let result = validate_shortcuts(&shortcuts, &defaults);
        assert!(result.is_ok());
    }

    #[test]
    fn test_canonical_chord() {
        assert_eq!(canonical_chord("Mod+S"), Some("Mod+S".to_owned()));
        assert_eq!(canonical_chord("Ctrl+S"), Some("Ctrl+S".to_owned()));
        assert_eq!(canonical_chord("invalid"), None);
    }

    #[test]
    fn control_and_platform_modifier_are_distinct_and_collisions_use_native_keys() {
        assert_eq!(physical_modifiers(modifiers::CTRL, true), modifiers::CTRL);
        assert_eq!(physical_modifiers(modifiers::MOD, true), modifiers::SUPER);
        assert_eq!(physical_modifiers(modifiers::MOD, false), modifiers::CTRL);
        let literal = if cfg!(target_os = "macos") {
            "Super+S"
        } else {
            "Ctrl+S"
        };
        for alternate in [literal, "Mod+s"] {
            let bindings = BTreeMap::from([
                ("first".to_string(), "Mod+S".to_string()),
                ("second".to_string(), alternate.to_string()),
            ]);
            assert!(matches!(
                validate_shortcuts(&bindings, &BTreeMap::new()),
                Err(ShortcutError::Collision { .. })
            ));
        }
        assert_eq!(parse_chord("Ctrl+Ctrl+Space"), None);
        assert_eq!(parse_chord("Control+Space"), None);
        assert_eq!(parse_chord("Ctrl+Right"), None);
    }
}
