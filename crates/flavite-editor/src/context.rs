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
        Code::KeyG => KeyCode::KeyG,
        Code::KeyH => KeyCode::KeyH,
        Code::KeyI => KeyCode::KeyI,
        Code::KeyJ => KeyCode::KeyJ,
        Code::KeyK => KeyCode::KeyK,
        Code::KeyL => KeyCode::KeyL,
        Code::KeyO => KeyCode::KeyO,
        Code::KeyP => KeyCode::KeyP,
        Code::KeyT => KeyCode::KeyT,
        Code::KeyU => KeyCode::KeyU,
        Code::KeyV => KeyCode::KeyV,
        Code::KeyW => KeyCode::KeyW,
        Code::KeyX => KeyCode::KeyX,
        Code::KeyY => KeyCode::KeyY,
        Code::Semicolon => KeyCode::Semicolon,
        Code::Slash => KeyCode::Slash,
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
        Motion::NextWordEnd => KeyAction::NextWordEnd,
        Motion::PreviousWord => KeyAction::PreviousWord,
    }
}
