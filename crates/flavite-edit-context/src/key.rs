//! Toolkit-free input identity for the Context Mode grammar.
//!
//! `flavite-editor` converts the native toolkit's physical codes and
//! modifier sets at its capture boundary; nothing in this crate depends on
//! a UI toolkit.

/// The modifier bits the grammar distinguishes. Global action modifiers
/// (Ctrl/Alt/Logo) never enter the Context grammar; they only mark a key as
/// out of scope.
#[allow(clippy::struct_excessive_bools)] // a modifier set is a bit list by nature
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub logo: bool,
}

impl Modifiers {
    pub const EMPTY: Self = Self {
        shift: false,
        control: false,
        alt: false,
        logo: false,
    };
    pub const SHIFT: Self = Self {
        shift: true,
        ..Self::EMPTY
    };
    pub const CTRL: Self = Self {
        control: true,
        ..Self::EMPTY
    };
    pub const ALT: Self = Self {
        alt: true,
        ..Self::EMPTY
    };
    pub const LOGO: Self = Self {
        logo: true,
        ..Self::EMPTY
    };
}

/// The physical key positions the Context grammar names. Positions, not
/// produced glyphs: an unlisted code can never resolve to a command.
///
/// The full grammar-addressable set: 26 letters + 9 punctuation positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum KeyCode {
    // Letters A-Z (all 26 positions)
    KeyA,
    KeyB,
    KeyC,
    KeyD,
    KeyE,
    KeyF,
    KeyG,
    KeyH,
    KeyI,
    KeyJ,
    KeyK,
    KeyL,
    KeyM,
    KeyN,
    KeyO,
    KeyP,
    KeyQ,
    KeyR,
    KeyS,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    KeyZ,
    // Punctuation positions
    Semicolon,
    Quote,
    Comma,
    Period,
    Slash,
    Backslash,
    BracketLeft,
    BracketRight,
    Minus,
    Equal,
}

impl KeyCode {
    /// Returns the canonical token name for this position (variant name).
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            KeyCode::KeyA => "KeyA",
            KeyCode::KeyB => "KeyB",
            KeyCode::KeyC => "KeyC",
            KeyCode::KeyD => "KeyD",
            KeyCode::KeyE => "KeyE",
            KeyCode::KeyF => "KeyF",
            KeyCode::KeyG => "KeyG",
            KeyCode::KeyH => "KeyH",
            KeyCode::KeyI => "KeyI",
            KeyCode::KeyJ => "KeyJ",
            KeyCode::KeyK => "KeyK",
            KeyCode::KeyL => "KeyL",
            KeyCode::KeyM => "KeyM",
            KeyCode::KeyN => "KeyN",
            KeyCode::KeyO => "KeyO",
            KeyCode::KeyP => "KeyP",
            KeyCode::KeyQ => "KeyQ",
            KeyCode::KeyR => "KeyR",
            KeyCode::KeyS => "KeyS",
            KeyCode::KeyT => "KeyT",
            KeyCode::KeyU => "KeyU",
            KeyCode::KeyV => "KeyV",
            KeyCode::KeyW => "KeyW",
            KeyCode::KeyX => "KeyX",
            KeyCode::KeyY => "KeyY",
            KeyCode::KeyZ => "KeyZ",
            KeyCode::Semicolon => "Semicolon",
            KeyCode::Quote => "Quote",
            KeyCode::Comma => "Comma",
            KeyCode::Period => "Period",
            KeyCode::Slash => "Slash",
            KeyCode::Backslash => "Backslash",
            KeyCode::BracketLeft => "BracketLeft",
            KeyCode::BracketRight => "BracketRight",
            KeyCode::Minus => "Minus",
            KeyCode::Equal => "Equal",
        }
    }
}

/// Selection motions a physical position resolves to. The editor maps these
/// onto its acknowledged motion vocabulary; the grammar never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Previous,
    Next,
    Up,
    Down,
    NextWord,
    PreviousWord,
    NextWordEnd,
}

impl Motion {
    /// Returns the synthetic binding id for this motion.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Motion::Previous => "editor.motion.previous",
            Motion::Next => "editor.motion.next",
            Motion::Up => "editor.motion.up",
            Motion::Down => "editor.motion.down",
            Motion::NextWord => "editor.motion.next_word",
            Motion::PreviousWord => "editor.motion.previous_word",
            Motion::NextWordEnd => "editor.motion.next_word_end",
        }
    }

    /// Returns a stable token name for this motion variant.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Motion::Previous => "previous",
            Motion::Next => "next",
            Motion::Up => "up",
            Motion::Down => "down",
            Motion::NextWord => "next_word",
            Motion::PreviousWord => "previous_word",
            Motion::NextWordEnd => "next_word_end",
        }
    }
}
