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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyCode {
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
    KeyO,
    KeyP,
    KeyT,
    KeyU,
    KeyV,
    KeyW,
    KeyX,
    KeyY,
    Semicolon,
    Slash,
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

