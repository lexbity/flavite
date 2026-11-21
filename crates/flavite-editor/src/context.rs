//! Native capture transport for Context Mode (FEUX-0002 X02): the widget's
//! event requests, plus the boundary conversions between iced's key identity
//! and the toolkit-free grammar that lives in `flavite-edit-context`. The
//! vocabulary, grammar, statechart and effect planning themselves are not
//! defined here anymore — this module only carries events and converts.

use crate::input::CapturedInput;
use crate::keymap::KeyAction;
use flavite_edit_context::{KeyCode, Modifiers, Motion};
use iced::keyboard::{Modifiers as NativeModifiers, key::Code};

pub mod layout;

/// Native editor context intents carry the exact source and the whole live
/// selection set (FEUX-0002 X04: `selection` is the primary pair kept for
/// captions; `selections` is the canonical set snapshot).
#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub buffer: u64,
    pub revision: u64,
    pub selection: (usize, usize),
    /// The whole live set as `(anchor, head)` pairs in canonical order.
    pub selections: Vec<(usize, usize)>,
    pub sequence: u64,
    pub event: ContextEvent,
}

#[derive(Debug, Clone)]
pub enum ContextEvent {
    Command(flavite_edit_context::ContextCommand),
    Menu(flavite_edit_context::ContextMenu),
    /// Right-click entry: the resolved selection set (kept when the press
    /// landed inside any member, replaced by word bounds otherwise, E-14).
    EnterAt(Vec<(usize, usize)>),
    Unbound,
    /// Interpret in acknowledged host mode, rather than the last drawn mode.
    Key(ContextKey),
    /// An IME commit ends composition and uses the same acknowledged edit path.
    Commit(crate::input::CapturedInput),
}

#[derive(Debug, Clone)]
pub struct ContextKey {
    pub code: Option<KeyCode>,
    pub modifiers: Modifiers,
    pub repeat: bool,
    pub input: CapturedInput,
}

/// Convert a native physical code to the grammar's key identity. Unlisted
/// positions convert to `None` and can never resolve to a command — the
/// grammar refuses what it does not name.
#[must_use]
pub fn native_code(code: Code) -> Option<KeyCode> {
    Some(match code {
        Code::KeyA => KeyCode::KeyA,
        Code::KeyB => KeyCode::KeyB,
        Code::KeyC => KeyCode::KeyC,
        Code::KeyD => KeyCode::KeyD,
        Code::KeyE => KeyCode::KeyE,
        Code::KeyF => KeyCode::KeyF,
        Code::KeyG => KeyCode::KeyG,
        Code::KeyH => KeyCode::KeyH,
        Code::KeyI => KeyCode::KeyI,
        Code::KeyJ => KeyCode::KeyJ,
        Code::KeyK => KeyCode::KeyK,
        Code::KeyL => KeyCode::KeyL,
        Code::KeyM => KeyCode::KeyM,
        Code::KeyN => KeyCode::KeyN,
        Code::KeyO => KeyCode::KeyO,
        Code::KeyP => KeyCode::KeyP,
        Code::KeyQ => KeyCode::KeyQ,
        Code::KeyR => KeyCode::KeyR,
        Code::KeyS => KeyCode::KeyS,
        Code::KeyT => KeyCode::KeyT,
        Code::KeyU => KeyCode::KeyU,
        Code::KeyV => KeyCode::KeyV,
        Code::KeyW => KeyCode::KeyW,
        Code::KeyX => KeyCode::KeyX,
        Code::KeyY => KeyCode::KeyY,
        Code::KeyZ => KeyCode::KeyZ,
        Code::Semicolon => KeyCode::Semicolon,
        Code::Quote => KeyCode::Quote,
        Code::Comma => KeyCode::Comma,
        Code::Period => KeyCode::Period,
        Code::Slash => KeyCode::Slash,
        Code::Backslash => KeyCode::Backslash,
        Code::BracketLeft => KeyCode::BracketLeft,
        Code::BracketRight => KeyCode::BracketRight,
        Code::Minus => KeyCode::Minus,
        Code::Equal => KeyCode::Equal,
        // FEUX-0004 X06: Digits (grammar-owned, E-4)
        // Note: Numpad digits are NOT converted (recorded in blast radius)
        Code::Digit0 => KeyCode::Digit0,
        Code::Digit1 => KeyCode::Digit1,
        Code::Digit2 => KeyCode::Digit2,
        Code::Digit3 => KeyCode::Digit3,
        Code::Digit4 => KeyCode::Digit4,
        Code::Digit5 => KeyCode::Digit5,
        Code::Digit6 => KeyCode::Digit6,
        Code::Digit7 => KeyCode::Digit7,
        Code::Digit8 => KeyCode::Digit8,
        Code::Digit9 => KeyCode::Digit9,
        _ => return None,
    })
}

/// Convert a native modifier set to the grammar's modifier identity.
#[must_use]
pub fn native_modifiers(modifiers: NativeModifiers) -> Modifiers {
    Modifiers {
        shift: modifiers.shift(),
        control: modifiers.control(),
        alt: modifiers.alt(),
        logo: modifiers.logo(),
    }
}

/// Map a grammar motion onto the editor's acknowledged motion vocabulary.
/// The grammar resolves *which* motion a physical position means; this maps
/// it onto the editor's existing motion vocabulary without re-deciding.
#[must_use]
pub const fn motion_action(motion: Motion) -> KeyAction {
    match motion {
        Motion::Previous => KeyAction::Previous,
        Motion::Next => KeyAction::Next,
        Motion::Up => KeyAction::Up,
        Motion::Down => KeyAction::Down,
        Motion::NextWord => KeyAction::NextWord,
        Motion::PreviousWord => KeyAction::PreviousWord,
        Motion::NextWordEnd => KeyAction::NextWordEnd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every position in the vocabulary converts from a native physical code
    /// (FEUX-0002 E-8/R9, FEUX-0003 E-2, FEUX-0004 X06): the conversion boundary
    /// covers the whole advertised physical vocabulary, so `f`/`Shift+F` and every
    /// other binding reach the keymap on real hardware.
    /// FEUX-0004: digits are grammar-owned and must convert; Numpad digits are NOT.
    #[test]
    fn native_converter_covers_the_whole_vocabulary() {
        // Full vocabulary: 26 letters + 9 punctuation + 10 digits = 45 positions
        // (FEUX-0003 E-2: 35 positions, FEUX-0004 X06: +10 digits)
        let all = [
            // Letters A-Z
            (Code::KeyA, KeyCode::KeyA),
            (Code::KeyB, KeyCode::KeyB),
            (Code::KeyC, KeyCode::KeyC),
            (Code::KeyD, KeyCode::KeyD),
            (Code::KeyE, KeyCode::KeyE),
            (Code::KeyF, KeyCode::KeyF),
            (Code::KeyG, KeyCode::KeyG),
            (Code::KeyH, KeyCode::KeyH),
            (Code::KeyI, KeyCode::KeyI),
            (Code::KeyJ, KeyCode::KeyJ),
            (Code::KeyK, KeyCode::KeyK),
            (Code::KeyL, KeyCode::KeyL),
            (Code::KeyM, KeyCode::KeyM),
            (Code::KeyN, KeyCode::KeyN),
            (Code::KeyO, KeyCode::KeyO),
            (Code::KeyP, KeyCode::KeyP),
            (Code::KeyQ, KeyCode::KeyQ),
            (Code::KeyR, KeyCode::KeyR),
            (Code::KeyS, KeyCode::KeyS),
            (Code::KeyT, KeyCode::KeyT),
            (Code::KeyU, KeyCode::KeyU),
            (Code::KeyV, KeyCode::KeyV),
            (Code::KeyW, KeyCode::KeyW),
            (Code::KeyX, KeyCode::KeyX),
            (Code::KeyY, KeyCode::KeyY),
            (Code::KeyZ, KeyCode::KeyZ),
            // Punctuation
            (Code::Semicolon, KeyCode::Semicolon),
            (Code::Quote, KeyCode::Quote),
            (Code::Comma, KeyCode::Comma),
            (Code::Period, KeyCode::Period),
            (Code::Slash, KeyCode::Slash),
            (Code::Backslash, KeyCode::Backslash),
            (Code::BracketLeft, KeyCode::BracketLeft),
            (Code::BracketRight, KeyCode::BracketRight),
            (Code::Minus, KeyCode::Minus),
            (Code::Equal, KeyCode::Equal),
            // FEUX-0004 X06: Digits
            (Code::Digit0, KeyCode::Digit0),
            (Code::Digit1, KeyCode::Digit1),
            (Code::Digit2, KeyCode::Digit2),
            (Code::Digit3, KeyCode::Digit3),
            (Code::Digit4, KeyCode::Digit4),
            (Code::Digit5, KeyCode::Digit5),
            (Code::Digit6, KeyCode::Digit6),
            (Code::Digit7, KeyCode::Digit7),
            (Code::Digit8, KeyCode::Digit8),
            (Code::Digit9, KeyCode::Digit9),
        ];
        for (native, expected) in all {
            assert_eq!(
                native_code(native),
                Some(expected),
                "{native:?} must convert to {expected:?}"
            );
        }
        // Numpad digits are NOT converted (recorded in blast radius)
        assert_eq!(native_code(Code::Numpad0), None);
        assert_eq!(native_code(Code::Numpad1), None);
    }

    /// The default keymap is the sole source of truth for Context Mode bindings (FEUX-0003 X02).
    /// This test verifies that all expected default bindings are present in the keymap.
    #[test]
    fn default_keymap_has_expected_bindings() {
        use flavite_edit_context::key::KeyCode;
        use flavite_edit_context::keymap::{BindingId, ContextKeymap, Slot};

        let keymap = ContextKeymap::defaults();

        // Verify that F (bare and shifted) maps to FindChar
        let f_slot = Slot {
            position: KeyCode::KeyF,
            shift: false,
        };
        let sf_slot = Slot {
            position: KeyCode::KeyF,
            shift: true,
        };
        assert!(matches!(
            keymap.lookup_root(f_slot),
            Some(BindingId::FindChar { forward: true })
        ));
        assert!(matches!(
            keymap.lookup_root(sf_slot),
            Some(BindingId::FindChar { forward: false })
        ));

        // Verify that D maps to Delete
        let d_slot = Slot {
            position: KeyCode::KeyD,
            shift: false,
        };
        assert_eq!(
            keymap.lookup_root(d_slot).map(|b| b.id()),
            Some("editor.delete")
        );

        // Verify that Shift+D maps to Cut
        let sd_slot = Slot {
            position: KeyCode::KeyD,
            shift: true,
        };
        assert_eq!(
            keymap.lookup_root(sd_slot).map(|b| b.id()),
            Some("editor.cut")
        );
    }
}
