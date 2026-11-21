//! Keyboard layout glyph tables for Context Mode key labels (FEUX-0003 X01/X05).
//!
//! This module provides the declared-layout-to-glyph mapping. The user declares
//! their layout via `appearance.keyboard_layout`; this module provides the
//! truthful glyph for each physical position under that layout.
//!
//! The contract: for the 35-position vocabulary (§5.1), every layout's table
//! MUST be total. Missing entries fall back to the position's token name.

use crate::key::KeyCode;

/// Declared keyboard layouts supported for Context Mode labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// US QWERTY layout.
    #[default]
    Qwerty,
    /// Dvorak layout.
    Dvorak,
    /// Colemak layout.
    Colemak,
    /// French AZERTY layout.
    Azerty,
    /// German QWERTZ layout.
    Qwertz,
}

impl Layout {
    /// Parse a layout from its identifier string.
    #[must_use]
    pub fn parse(ident: &str) -> Option<Self> {
        match ident {
            "qwerty" => Some(Self::Qwerty),
            "dvorak" => Some(Self::Dvorak),
            "colemak" => Some(Self::Colemak),
            "azerty" => Some(Self::Azerty),
            "qwertz" => Some(Self::Qwertz),
            _ => None,
        }
    }

    /// Return the identifier string for this layout.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Qwerty => "qwerty",
            Self::Dvorak => "dvorak",
            Self::Colemak => "colemak",
            Self::Azerty => "azerty",
            Self::Qwertz => "qwertz",
        }
    }

    /// Return the display title for this layout.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Qwerty => "QWERTY",
            Self::Dvorak => "Dvorak",
            Self::Colemak => "Colemak",
            Self::Azerty => "AZERTY",
            Self::Qwertz => "QWERTZ",
        }
    }
}

/// QWERTY layout glyph table (normative, complete for all 35 positions).
///
/// All positions map to their unshifted US QWERTY characters.
const QWERTY_GLYPHS: &[(KeyCode, &str)] = &[
    // Letters
    (KeyCode::KeyA, "a"),
    (KeyCode::KeyB, "b"),
    (KeyCode::KeyC, "c"),
    (KeyCode::KeyD, "d"),
    (KeyCode::KeyE, "e"),
    (KeyCode::KeyF, "f"),
    (KeyCode::KeyG, "g"),
    (KeyCode::KeyH, "h"),
    (KeyCode::KeyI, "i"),
    (KeyCode::KeyJ, "j"),
    (KeyCode::KeyK, "k"),
    (KeyCode::KeyL, "l"),
    (KeyCode::KeyM, "m"),
    (KeyCode::KeyN, "n"),
    (KeyCode::KeyO, "o"),
    (KeyCode::KeyP, "p"),
    (KeyCode::KeyQ, "q"),
    (KeyCode::KeyR, "r"),
    (KeyCode::KeyS, "s"),
    (KeyCode::KeyT, "t"),
    (KeyCode::KeyU, "u"),
    (KeyCode::KeyV, "v"),
    (KeyCode::KeyW, "w"),
    (KeyCode::KeyX, "x"),
    (KeyCode::KeyY, "y"),
    (KeyCode::KeyZ, "z"),
    // Punctuation
    (KeyCode::Semicolon, ";"),
    (KeyCode::Quote, "'"),
    (KeyCode::Comma, ","),
    (KeyCode::Period, "."),
    (KeyCode::Slash, "/"),
    (KeyCode::Backslash, "\\"),
    (KeyCode::BracketLeft, "["),
    (KeyCode::BracketRight, "]"),
    (KeyCode::Minus, "-"),
    (KeyCode::Equal, "="),
];

/// Dvorak layout glyph table (normative, complete for all 35 positions).
///
/// Maps physical positions to their unshifted Dvorak characters.
const DVORAK_GLYPHS: &[(KeyCode, &str)] = &[
    // Letters
    (KeyCode::KeyA, "a"),
    (KeyCode::KeyB, "x"),
    (KeyCode::KeyC, "j"),
    (KeyCode::KeyD, "e"),
    (KeyCode::KeyE, "."),
    (KeyCode::KeyF, "y"),
    (KeyCode::KeyG, "i"),
    (KeyCode::KeyH, "d"),
    (KeyCode::KeyI, "c"),
    (KeyCode::KeyJ, "h"),
    (KeyCode::KeyK, "t"),
    (KeyCode::KeyL, "n"),
    (KeyCode::KeyM, "m"),
    (KeyCode::KeyN, "b"),
    (KeyCode::KeyO, "r"),
    (KeyCode::KeyP, "l"),
    (KeyCode::KeyQ, "'"),
    (KeyCode::KeyR, "o"),
    (KeyCode::KeyS, "o"),
    (KeyCode::KeyT, "y"),
    (KeyCode::KeyU, "g"),
    (KeyCode::KeyV, "k"),
    (KeyCode::KeyW, ","),
    (KeyCode::KeyX, "q"),
    (KeyCode::KeyY, "f"),
    (KeyCode::KeyZ, ";"),
    // Punctuation
    (KeyCode::Semicolon, "s"),
    (KeyCode::Quote, "-"),
    (KeyCode::Comma, "w"),
    (KeyCode::Period, "v"),
    (KeyCode::Slash, "z"),
    (KeyCode::Backslash, "\\"),
    (KeyCode::BracketLeft, "/"),
    (KeyCode::BracketRight, "="),
    (KeyCode::Minus, "["),
    (KeyCode::Equal, "]"),
];

/// Colemak layout glyph table (normative, complete for all 35 positions).
///
/// Maps physical positions to their unshifted Colemak characters.
const COLEMAK_GLYPHS: &[(KeyCode, &str)] = &[
    // Letters
    (KeyCode::KeyA, "a"),
    (KeyCode::KeyB, "b"),
    (KeyCode::KeyC, "c"),
    (KeyCode::KeyD, "s"),
    (KeyCode::KeyE, "f"),
    (KeyCode::KeyF, "t"),
    (KeyCode::KeyG, "d"),
    (KeyCode::KeyH, "h"),
    (KeyCode::KeyI, "u"),
    (KeyCode::KeyJ, "n"),
    (KeyCode::KeyK, "e"),
    (KeyCode::KeyL, "i"),
    (KeyCode::KeyM, "m"),
    (KeyCode::KeyN, "k"),
    (KeyCode::KeyO, "y"),
    (KeyCode::KeyP, "p"),
    (KeyCode::KeyQ, "q"),
    (KeyCode::KeyR, "p"),
    (KeyCode::KeyS, "r"),
    (KeyCode::KeyT, "g"),
    (KeyCode::KeyU, "l"),
    (KeyCode::KeyV, "v"),
    (KeyCode::KeyW, "w"),
    (KeyCode::KeyX, "x"),
    (KeyCode::KeyY, "j"),
    (KeyCode::KeyZ, "z"),
    // Punctuation
    (KeyCode::Semicolon, "o"),
    (KeyCode::Quote, "'"),
    (KeyCode::Comma, ","),
    (KeyCode::Period, "."),
    (KeyCode::Slash, "/"),
    (KeyCode::Backslash, "\\"),
    (KeyCode::BracketLeft, "["),
    (KeyCode::BracketRight, "]"),
    (KeyCode::Minus, "-"),
    (KeyCode::Equal, "="),
];

/// AZERTY layout glyph table (normative, complete for all 35 positions).
///
/// Maps physical positions to their unshifted French AZERTY characters.
const AZERTY_GLYPHS: &[(KeyCode, &str)] = &[
    // Letters
    (KeyCode::KeyA, "q"),
    (KeyCode::KeyB, "b"),
    (KeyCode::KeyC, "c"),
    (KeyCode::KeyD, "d"),
    (KeyCode::KeyE, "e"),
    (KeyCode::KeyF, "f"),
    (KeyCode::KeyG, "g"),
    (KeyCode::KeyH, "h"),
    (KeyCode::KeyI, "i"),
    (KeyCode::KeyJ, "j"),
    (KeyCode::KeyK, "k"),
    (KeyCode::KeyL, "l"),
    (KeyCode::KeyM, ","),
    (KeyCode::KeyN, "n"),
    (KeyCode::KeyO, "o"),
    (KeyCode::KeyP, "p"),
    (KeyCode::KeyQ, "a"),
    (KeyCode::KeyR, "r"),
    (KeyCode::KeyS, "s"),
    (KeyCode::KeyT, "t"),
    (KeyCode::KeyU, "u"),
    (KeyCode::KeyV, "v"),
    (KeyCode::KeyW, "z"),
    (KeyCode::KeyX, "x"),
    (KeyCode::KeyY, "y"),
    (KeyCode::KeyZ, "w"),
    // Punctuation
    (KeyCode::Semicolon, "m"),
    (KeyCode::Quote, "\u{00F9}"), // ù
    (KeyCode::Comma, ";"),
    (KeyCode::Period, ":"),
    (KeyCode::Slash, "!"),
    (KeyCode::Backslash, "*"),
    (KeyCode::BracketLeft, "^"),
    (KeyCode::BracketRight, "$"),
    (KeyCode::Minus, ")"),
    (KeyCode::Equal, "="),
];

/// QWERTZ layout glyph table (normative, complete for all 35 positions).
///
/// Maps physical positions to their unshifted German QWERTZ characters.
const QWERTZ_GLYPHS: &[(KeyCode, &str)] = &[
    // Letters
    (KeyCode::KeyA, "a"),
    (KeyCode::KeyB, "b"),
    (KeyCode::KeyC, "c"),
    (KeyCode::KeyD, "d"),
    (KeyCode::KeyE, "e"),
    (KeyCode::KeyF, "f"),
    (KeyCode::KeyG, "g"),
    (KeyCode::KeyH, "h"),
    (KeyCode::KeyI, "i"),
    (KeyCode::KeyJ, "j"),
    (KeyCode::KeyK, "k"),
    (KeyCode::KeyL, "l"),
    (KeyCode::KeyM, "m"),
    (KeyCode::KeyN, "n"),
    (KeyCode::KeyO, "o"),
    (KeyCode::KeyP, "p"),
    (KeyCode::KeyQ, "q"),
    (KeyCode::KeyR, "r"),
    (KeyCode::KeyS, "s"),
    (KeyCode::KeyT, "t"),
    (KeyCode::KeyU, "u"),
    (KeyCode::KeyV, "v"),
    (KeyCode::KeyW, "w"),
    (KeyCode::KeyX, "x"),
    (KeyCode::KeyY, "z"),
    (KeyCode::KeyZ, "y"),
    // Punctuation
    (KeyCode::Semicolon, "\u{00F6}"), // ö
    (KeyCode::Quote, "\u{00E4}"),     // ä
    (KeyCode::Comma, ","),
    (KeyCode::Period, "."),
    (KeyCode::Slash, "-"),
    (KeyCode::Backslash, "#"),
    (KeyCode::BracketLeft, "\u{00FC}"), // ü
    (KeyCode::BracketRight, "+"),
    (KeyCode::Minus, "\u{00DF}"), // ß
    (KeyCode::Equal, "="),
];

/// Returns the unshifted glyph for a position under the given layout.
///
/// Returns `None` if the position is not in the layout's table.
/// The caller should fall back to the position's token name in this case.
///
/// # Contract
/// For all 35 positions in the vocabulary, QWERTY MUST be total.
/// Other layouts should be total but may return None temporarily during
/// phased implementation (X01 ships QWERTY only; X05 adds the rest).
#[must_use]
pub fn glyph(layout: Layout, position: KeyCode) -> Option<&'static str> {
    let table = match layout {
        Layout::Qwerty => QWERTY_GLYPHS,
        Layout::Dvorak => DVORAK_GLYPHS,
        Layout::Colemak => COLEMAK_GLYPHS,
        Layout::Azerty => AZERTY_GLYPHS,
        Layout::Qwertz => QWERTZ_GLYPHS,
    };
    table
        .iter()
        .find(|(code, _)| *code == position)
        .map(|(_, g)| *g)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qwerty_table_is_total() {
        // Verify QWERTY has all 35 positions (FR-12 / AC-6 requirement).
        let all_positions = [
            KeyCode::KeyA,
            KeyCode::KeyB,
            KeyCode::KeyC,
            KeyCode::KeyD,
            KeyCode::KeyE,
            KeyCode::KeyF,
            KeyCode::KeyG,
            KeyCode::KeyH,
            KeyCode::KeyI,
            KeyCode::KeyJ,
            KeyCode::KeyK,
            KeyCode::KeyL,
            KeyCode::KeyM,
            KeyCode::KeyN,
            KeyCode::KeyO,
            KeyCode::KeyP,
            KeyCode::KeyQ,
            KeyCode::KeyR,
            KeyCode::KeyS,
            KeyCode::KeyT,
            KeyCode::KeyU,
            KeyCode::KeyV,
            KeyCode::KeyW,
            KeyCode::KeyX,
            KeyCode::KeyY,
            KeyCode::KeyZ,
            KeyCode::Semicolon,
            KeyCode::Quote,
            KeyCode::Comma,
            KeyCode::Period,
            KeyCode::Slash,
            KeyCode::Backslash,
            KeyCode::BracketLeft,
            KeyCode::BracketRight,
            KeyCode::Minus,
            KeyCode::Equal,
        ];

        let mut missing = Vec::new();
        for position in all_positions {
            if glyph(Layout::Qwerty, position).is_none() {
                missing.push(position.token());
            }
        }

        assert!(
            missing.is_empty(),
            "QWERTY table missing positions: {missing:?}"
        );
    }

    #[test]
    fn qwerty_glyphs_are_correct() {
        // Spot-check key glyphs match expectations
        assert_eq!(glyph(Layout::Qwerty, KeyCode::KeyD), Some("d"));
        assert_eq!(glyph(Layout::Qwerty, KeyCode::KeyH), Some("h"));
        assert_eq!(glyph(Layout::Qwerty, KeyCode::KeyJ), Some("j"));
        assert_eq!(glyph(Layout::Qwerty, KeyCode::KeyK), Some("k"));
        assert_eq!(glyph(Layout::Qwerty, KeyCode::Semicolon), Some(";"));
        assert_eq!(glyph(Layout::Qwerty, KeyCode::Slash), Some("/"));
        assert_eq!(glyph(Layout::Qwerty, KeyCode::Quote), Some("'"));
        assert_eq!(glyph(Layout::Qwerty, KeyCode::Backslash), Some("\\"));
    }

    #[test]
    fn layout_parse_and_id_roundtrip() {
        for layout in [
            Layout::Qwerty,
            Layout::Dvorak,
            Layout::Colemak,
            Layout::Azerty,
            Layout::Qwertz,
        ] {
            let parsed = Layout::parse(layout.id());
            assert_eq!(
                parsed,
                Some(layout),
                "{} should parse and roundtrip",
                layout.id()
            );
        }
    }

    #[test]
    fn all_layouts_are_total() {
        // After X05, all five layouts must be total over the 35-position vocabulary
        let all_positions = [
            KeyCode::KeyA,
            KeyCode::KeyB,
            KeyCode::KeyC,
            KeyCode::KeyD,
            KeyCode::KeyE,
            KeyCode::KeyF,
            KeyCode::KeyG,
            KeyCode::KeyH,
            KeyCode::KeyI,
            KeyCode::KeyJ,
            KeyCode::KeyK,
            KeyCode::KeyL,
            KeyCode::KeyM,
            KeyCode::KeyN,
            KeyCode::KeyO,
            KeyCode::KeyP,
            KeyCode::KeyQ,
            KeyCode::KeyR,
            KeyCode::KeyS,
            KeyCode::KeyT,
            KeyCode::KeyU,
            KeyCode::KeyV,
            KeyCode::KeyW,
            KeyCode::KeyX,
            KeyCode::KeyY,
            KeyCode::KeyZ,
            KeyCode::Semicolon,
            KeyCode::Quote,
            KeyCode::Comma,
            KeyCode::Period,
            KeyCode::Slash,
            KeyCode::Backslash,
            KeyCode::BracketLeft,
            KeyCode::BracketRight,
            KeyCode::Minus,
            KeyCode::Equal,
        ];

        for layout in [
            Layout::Qwerty,
            Layout::Dvorak,
            Layout::Colemak,
            Layout::Azerty,
            Layout::Qwertz,
        ] {
            let mut missing = Vec::new();
            for position in all_positions {
                if glyph(layout, position).is_none() {
                    missing.push(position.token());
                }
            }
            assert!(
                missing.is_empty(),
                "{} layout missing positions: {missing:?}",
                layout.id()
            );
        }
    }

    #[test]
    fn dvorak_glyphs_are_correct() {
        // Spot-check distinctive Dvorak mappings
        assert_eq!(glyph(Layout::Dvorak, KeyCode::KeyD), Some("e"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::KeyH), Some("d"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::KeyJ), Some("h"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::KeyK), Some("t"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::Semicolon), Some("s"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::Quote), Some("-"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::Comma), Some("w"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::Period), Some("v"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::Slash), Some("z"));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::Minus), Some("["));
        assert_eq!(glyph(Layout::Dvorak, KeyCode::Equal), Some("]"));
    }

    #[test]
    fn colemak_glyphs_are_correct() {
        // Spot-check distinctive Colemak mappings
        assert_eq!(glyph(Layout::Colemak, KeyCode::KeyD), Some("s"));
        assert_eq!(glyph(Layout::Colemak, KeyCode::KeyE), Some("f"));
        assert_eq!(glyph(Layout::Colemak, KeyCode::KeyF), Some("t"));
        assert_eq!(glyph(Layout::Colemak, KeyCode::KeyG), Some("d"));
        assert_eq!(glyph(Layout::Colemak, KeyCode::KeyK), Some("e"));
        assert_eq!(glyph(Layout::Colemak, KeyCode::Semicolon), Some("o"));
        assert_eq!(glyph(Layout::Colemak, KeyCode::Quote), Some("'"));
        assert_eq!(glyph(Layout::Colemak, KeyCode::Backslash), Some("\\"));
    }

    #[test]
    fn azerty_glyphs_are_correct() {
        // Spot-check distinctive AZERTY mappings
        assert_eq!(glyph(Layout::Azerty, KeyCode::KeyA), Some("q"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::KeyQ), Some("a"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::KeyW), Some("z"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::KeyM), Some(","));
        assert_eq!(glyph(Layout::Azerty, KeyCode::Semicolon), Some("m"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::Quote), Some("\u{00F9}")); // ù
        assert_eq!(glyph(Layout::Azerty, KeyCode::Comma), Some(";"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::Period), Some(":"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::Slash), Some("!"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::Backslash), Some("*"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::BracketLeft), Some("^"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::BracketRight), Some("$"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::Minus), Some(")"));
        assert_eq!(glyph(Layout::Azerty, KeyCode::Equal), Some("="));
    }

    #[test]
    fn qwertz_glyphs_are_correct() {
        // Spot-check distinctive QWERTZ mappings
        assert_eq!(glyph(Layout::Qwertz, KeyCode::KeyY), Some("z"));
        assert_eq!(glyph(Layout::Qwertz, KeyCode::KeyZ), Some("y"));
        assert_eq!(glyph(Layout::Qwertz, KeyCode::Semicolon), Some("\u{00F6}")); // ö
        assert_eq!(glyph(Layout::Qwertz, KeyCode::Quote), Some("\u{00E4}")); // ä
        assert_eq!(glyph(Layout::Qwertz, KeyCode::Slash), Some("-"));
        assert_eq!(glyph(Layout::Qwertz, KeyCode::Backslash), Some("#"));
        assert_eq!(
            glyph(Layout::Qwertz, KeyCode::BracketLeft),
            Some("\u{00FC}")
        ); // ü
        assert_eq!(glyph(Layout::Qwertz, KeyCode::BracketRight), Some("+"));
        assert_eq!(glyph(Layout::Qwertz, KeyCode::Minus), Some("\u{00DF}")); // ß
        assert_eq!(glyph(Layout::Qwertz, KeyCode::Equal), Some("="));
    }
}
