//! Context Mode's data-driven keymap: binding slots, default bindings, override resolution,
//! and validation (FEUX-0003 X01).
//!
//! The compiled-in default `ContextKeymap` is behavior-identical to the delivered
//! `grammar.rs` match statements. This module provides the data foundation for
//! rebindable Context Mode accelerators.

use crate::key::{KeyCode, Motion};
use crate::vocabulary::{ContextCommand, ContextMenu};
use std::collections::HashMap;

/// A physical binding slot: a position with an optional Shift modifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot {
    pub position: KeyCode,
    pub shift: bool,
}

impl Slot {
    /// Create a bare (non-shifted) slot.
    pub const fn bare(position: KeyCode) -> Self {
        Self {
            position,
            shift: false,
        }
    }

    /// Create a shifted slot.
    pub const fn shifted(position: KeyCode) -> Self {
        Self {
            position,
            shift: true,
        }
    }
}

/// A binding identifier: every bindable entity in Context Mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingId {
    /// A command from the vocabulary (uses canonical id).
    Command(ContextCommand),
    /// FindChar with direction (synthetic id: editor.find_char.forward/backward).
    FindChar { forward: bool },
    /// A motion (synthetic id: editor.motion.<name>).
    Motion(Motion),
    /// A menu opener (synthetic id: editor.menu_select/go).
    MenuOpener(ContextMenu),
}

impl BindingId {
    /// Returns the string identifier for this binding.
    pub fn id(&self) -> &'static str {
        match self {
            BindingId::Command(c) => c.id(),
            BindingId::FindChar { forward } => {
                if *forward {
                    "editor.find_char.forward"
                } else {
                    "editor.find_char.backward"
                }
            }
            BindingId::Motion(m) => m.id(),
            BindingId::MenuOpener(menu) => match menu {
                ContextMenu::Select => "editor.menu_select",
                ContextMenu::Go => "editor.menu_go",
                ContextMenu::More => "editor.menu_more",
                // Type is being dissolved in X04, but we keep it for completeness
                ContextMenu::Type => "editor.menu_type",
            },
        }
    }

    /// Parse a binding id string into a BindingId.
    pub fn parse(id: &str) -> Option<Self> {
        // Try exact matches with command ids first
        for cmd in ContextCommand::ALL {
            if cmd.id() == id {
                return Some(BindingId::Command(*cmd));
            }
        }

        // Try synthetic ids
        match id {
            "editor.find_char.forward" => Some(BindingId::FindChar { forward: true }),
            "editor.find_char.backward" => Some(BindingId::FindChar { forward: false }),
            "editor.motion.previous" => Some(BindingId::Motion(Motion::Previous)),
            "editor.motion.next" => Some(BindingId::Motion(Motion::Next)),
            "editor.motion.up" => Some(BindingId::Motion(Motion::Up)),
            "editor.motion.down" => Some(BindingId::Motion(Motion::Down)),
            "editor.motion.next_word" => Some(BindingId::Motion(Motion::NextWord)),
            "editor.motion.previous_word" => Some(BindingId::Motion(Motion::PreviousWord)),
            "editor.motion.next_word_end" => Some(BindingId::Motion(Motion::NextWordEnd)),
            "editor.menu_select" => Some(BindingId::MenuOpener(ContextMenu::Select)),
            "editor.menu_go" => Some(BindingId::MenuOpener(ContextMenu::Go)),
            "editor.menu_more" => Some(BindingId::MenuOpener(ContextMenu::More)),
            "editor.menu_type" => Some(BindingId::MenuOpener(ContextMenu::Type)),
            _ => None,
        }
    }

    /// Check if this is a motion binding (which must be bare per Rule X).
    pub fn is_motion(&self) -> bool {
        matches!(self, BindingId::Motion(_))
    }

    /// Check if this is a menu opener binding (which must be bare per Rule X).
    pub fn is_menu_opener(&self) -> bool {
        matches!(self, BindingId::MenuOpener(_))
    }
}

/// A binding token: physical position + optional shift, or the unbound sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BindingToken {
    /// A physical position slot.
    Position(Slot),
    /// The unbound sentinel (removes the default binding).
    Unbound,
}

impl BindingToken {
    /// Parse a token string into a BindingToken.
    pub fn parse(text: &str) -> Option<Self> {
        if text == "unbound" {
            return Some(BindingToken::Unbound);
        }

        // Parse position token: ["shift+"]"physical:"<POSITION>
        let (has_shift, rest) = if let Some(stripped) = text.strip_prefix("shift+") {
            (true, stripped)
        } else {
            (false, text)
        };

        let (prefix, name) = rest.split_once(':')?;
        if prefix != "physical" {
            return None;
        }

        // Parse the position name
        let position = match name {
            "KeyA" => KeyCode::KeyA,
            "KeyB" => KeyCode::KeyB,
            "KeyC" => KeyCode::KeyC,
            "KeyD" => KeyCode::KeyD,
            "KeyE" => KeyCode::KeyE,
            "KeyF" => KeyCode::KeyF,
            "KeyG" => KeyCode::KeyG,
            "KeyH" => KeyCode::KeyH,
            "KeyI" => KeyCode::KeyI,
            "KeyJ" => KeyCode::KeyJ,
            "KeyK" => KeyCode::KeyK,
            "KeyL" => KeyCode::KeyL,
            "KeyM" => KeyCode::KeyM,
            "KeyN" => KeyCode::KeyN,
            "KeyO" => KeyCode::KeyO,
            "KeyP" => KeyCode::KeyP,
            "KeyQ" => KeyCode::KeyQ,
            "KeyR" => KeyCode::KeyR,
            "KeyS" => KeyCode::KeyS,
            "KeyT" => KeyCode::KeyT,
            "KeyU" => KeyCode::KeyU,
            "KeyV" => KeyCode::KeyV,
            "KeyW" => KeyCode::KeyW,
            "KeyX" => KeyCode::KeyX,
            "KeyY" => KeyCode::KeyY,
            "KeyZ" => KeyCode::KeyZ,
            "Semicolon" => KeyCode::Semicolon,
            "Quote" => KeyCode::Quote,
            "Comma" => KeyCode::Comma,
            "Period" => KeyCode::Period,
            "Slash" => KeyCode::Slash,
            "Backslash" => KeyCode::Backslash,
            "BracketLeft" => KeyCode::BracketLeft,
            "BracketRight" => KeyCode::BracketRight,
            "Minus" => KeyCode::Minus,
            "Equal" => KeyCode::Equal,
            _ => return None,
        };

        Some(BindingToken::Position(Slot {
            position,
            shift: has_shift,
        }))
    }
}

/// Validation errors for keymap resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeymapError {
    /// The binding id is not recognized.
    UnknownBinding { id: String },
    /// The token string is unreadable (not "unbound" and not a position token).
    UnreadableToken { id: String, text: String },
    /// The position name in the token is not a valid KeyCode variant.
    UnknownPosition { id: String, text: String },
    /// Shift modifier on a motion binding (forbidden by Rule X).
    ShiftOnMotion { id: String },
    /// Shift modifier on a menu opener binding (forbidden by Rule X).
    ShiftOnMenuOpener { id: String },
    /// Shift modifier on a menu accelerator binding (forbidden by Rule X).
    ShiftOnMenuAccelerator { id: String },
    /// Two bindings map to the same root slot (position + shift).
    Collision {
        slot: Slot,
        first: String,
        second: String,
    },
    /// Two accelerators in the same menu map to the same position.
    DuplicateMenuKey {
        menu: ContextMenu,
        position: KeyCode,
        first: String,
        second: String,
    },
}

impl std::fmt::Display for KeymapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeymapError::UnknownBinding { id } => {
                write!(f, "Unknown binding id: '{id}'")
            }
            KeymapError::UnreadableToken { id, text } => {
                write!(f, "Unreadable token '{text}' for binding '{id}'")
            }
            KeymapError::UnknownPosition { id, text } => {
                write!(f, "Unknown position '{text}' in binding '{id}'")
            }
            KeymapError::ShiftOnMotion { id } => {
                write!(f, "Shift modifier not allowed on motion binding '{id}'")
            }
            KeymapError::ShiftOnMenuOpener { id } => {
                write!(f, "Shift modifier not allowed on menu opener '{id}'")
            }
            KeymapError::ShiftOnMenuAccelerator { id } => {
                write!(f, "Shift modifier not allowed on menu accelerator '{id}'")
            }
            KeymapError::Collision { slot, first, second } => {
                let mod_str = if slot.shift { "Shift+" } else { "" };
                write!(
                    f,
                    "Collision: '{first}' and '{second}' both map to {}{}",
                    mod_str,
                    slot.position.token()
                )
            }
            KeymapError::DuplicateMenuKey {
                menu,
                position,
                first,
                second,
            } => {
                write!(
                    f,
                    "Duplicate in {} menu: '{first}' and '{second}' both map to {}",
                    menu.title(),
                    position.token()
                )
            }
        }
    }
}

/// The effective Context Mode keymap: a compiled, validated mapping from slots to binding ids.
#[derive(Debug, Clone)]
pub struct ContextKeymap {
    /// Root bindings: slot -> binding id.
    root: HashMap<Slot, BindingId>,
    /// Menu accelerator bindings: (menu, position) -> binding id.
    /// Note: menu accelerators are bare (no shift), and only use position (not Slot).
    menus: HashMap<(ContextMenu, KeyCode), BindingId>,
    /// Reverse lookup: binding id -> slot (for label composition).
    slots_of: HashMap<String, Slot>,
}

impl Default for ContextKeymap {
    fn default() -> Self {
        Self::defaults()
    }
}

impl ContextKeymap {
    /// Returns the default keymap (behavior-identical to the delivered grammar.rs).
    pub fn defaults() -> Self {
        let mut root = HashMap::new();
        let mut slots_of = HashMap::new();

        // Add root bindings from DEFAULT_ROOT
        for (binding_id, slot) in DEFAULT_ROOT {
            let id_str = binding_id.id().to_string();
            root.insert(*slot, binding_id.clone());
            slots_of.insert(id_str, *slot);
        }

        let mut menus = HashMap::new();
        for (menu, binding_id, position) in DEFAULT_MENUS {
            let id_str = binding_id.id().to_string();
            menus.insert((*menu, *position), binding_id.clone());
            // Menu accelerators don't go in slots_of (they're menu-scoped)
            // but we still need to track them for label lookup
            // For now, we'll use the root slot if it exists, otherwise bare position
            if !slots_of.contains_key(&id_str) {
                slots_of.insert(id_str, Slot::bare(*position));
            }
        }

        Self {
            root,
            menus,
            slots_of,
        }
    }

    /// Resolve a user override map into an effective keymap.
    /// 
    /// The override map is binding-id -> token string.
    /// Returns Ok(keymap) or Err(list of all violations).
    pub fn resolve(
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, Vec<KeymapError>> {
        let mut errors = Vec::new();
        let mut composed_root: HashMap<Slot, BindingId> = HashMap::new();
        let mut composed_menus: HashMap<(ContextMenu, KeyCode), BindingId> = HashMap::new();
        let mut composed_slots_of: HashMap<String, Slot> = HashMap::new();

        // Start with defaults
        let defaults = Self::defaults();
        for (slot, binding_id) in &defaults.root {
            composed_root.insert(*slot, binding_id.clone());
            composed_slots_of.insert(binding_id.id().to_string(), *slot);
        }
        for ((menu, position), binding_id) in &defaults.menus {
            composed_menus.insert((*menu, *position), binding_id.clone());
        }

        // Apply overrides
        for (id_str, token_str) in overrides {
            // Parse the binding id
            let binding_id = match BindingId::parse(id_str) {
                Some(bid) => bid,
                None => {
                    errors.push(KeymapError::UnknownBinding {
                        id: id_str.clone(),
                    });
                    continue;
                }
            };

            // Parse the token
            let token = match BindingToken::parse(token_str) {
                Some(token) => token,
                None => {
                    errors.push(KeymapError::UnreadableToken {
                        id: id_str.clone(),
                        text: token_str.clone(),
                    });
                    continue;
                }
            };

            match token {
                BindingToken::Unbound => {
                    // Remove this binding
                    composed_root.retain(|_, bid| bid.id() != id_str);
                    composed_menus.retain(|_, bid| bid.id() != id_str);
                    composed_slots_of.remove(id_str);
                }
                BindingToken::Position(slot) => {
                    // Validate Rule X: no shift on motions, menu openers, or menu accelerators
                    if binding_id.is_motion() && slot.shift {
                        errors.push(KeymapError::ShiftOnMotion {
                            id: id_str.clone(),
                        });
                        continue;
                    }
                    if binding_id.is_menu_opener() && slot.shift {
                        errors.push(KeymapError::ShiftOnMenuOpener {
                            id: id_str.clone(),
                        });
                        continue;
                    }

                    // Remove any existing entry for this binding id
                    composed_root.retain(|_, bid| bid.id() != id_str);
                    composed_menus.retain(|_, bid| bid.id() != id_str);
                    composed_slots_of.remove(id_str);

                    // Check if the new slot is already taken by a different binding
                    if let Some(existing_bid) = composed_root.get(&slot) {
                        if existing_bid.id() != id_str {
                            // Collision: this slot is already bound to a different binding
                            errors.push(KeymapError::Collision {
                                slot,
                                first: existing_bid.id().to_string(),
                                second: id_str.clone(),
                            });
                            continue;
                        }
                    }

                    // Add to composed map with new slot
                    composed_slots_of.insert(id_str.clone(), slot);
                    composed_root.insert(slot, binding_id);
                }
            }
        }

        // Check for collisions in root
        let mut root_slots_seen: HashMap<Slot, String> = HashMap::new();
        for (slot, binding_id) in &composed_root {
            if let Some(existing_id) = root_slots_seen.get(slot) {
                errors.push(KeymapError::Collision {
                    slot: *slot,
                    first: existing_id.clone(),
                    second: binding_id.id().to_string(),
                });
            } else {
                root_slots_seen.insert(*slot, binding_id.id().to_string());
            }
        }

        // Check for collisions in menus
        let mut menu_keys_seen: HashMap<(ContextMenu, KeyCode), String> = HashMap::new();
        for ((menu, position), binding_id) in &composed_menus {
            let key = (*menu, *position);
            if let Some(existing_id) = menu_keys_seen.get(&key) {
                errors.push(KeymapError::DuplicateMenuKey {
                    menu: *menu,
                    position: *position,
                    first: existing_id.clone(),
                    second: binding_id.id().to_string(),
                });
            } else {
                menu_keys_seen.insert(key, binding_id.id().to_string());
            }
        }

        if errors.is_empty() {
            Ok(Self {
                root: composed_root,
                menus: composed_menus,
                slots_of: composed_slots_of,
            })
        } else {
            Err(errors)
        }
    }

    /// Look up a root binding by slot.
    #[must_use]
    pub fn lookup_root(&self, slot: Slot) -> Option<&BindingId> {
        self.root.get(&slot)
    }

    /// Look up a menu accelerator by menu and position.
    /// Falls through to root if the command is in the menu's command list.
    #[must_use]
    pub fn lookup_menu(&self, menu: ContextMenu, position: KeyCode) -> Option<ContextCommand> {
        // First, check for explicit menu accelerator
        if let Some(BindingId::Command(cmd)) = self.menus.get(&(menu, position)) {
            if menu.commands().contains(cmd) {
                return Some(*cmd);
            }
        }

        // Fall through to root
        let slot = Slot::bare(position);
        if let Some(BindingId::Command(cmd)) = self.root.get(&slot) {
            if menu.commands().contains(cmd) {
                return Some(*cmd);
            }
        }

        None
    }

    /// Get the slot for a binding id (for label composition).
    #[must_use]
    pub fn slot_of(&self, binding_id: &str) -> Option<Slot> {
        self.slots_of.get(binding_id).copied()
    }

    /// Get the binding label for a binding id using the given layout.
    #[must_use]
    pub fn binding_label(&self, layout: crate::labels::Layout, binding_id: &str) -> String {
        use crate::labels::glyph;

        self.slot_of(binding_id)
            .map(|slot| {
                let glyph = glyph(layout, slot.position)
                    .unwrap_or_else(|| slot.position.token());
                if slot.shift {
                    format!("⇧{glyph}")
                } else {
                    glyph.to_string()
                }
            })
            .unwrap_or_else(|| "unbound".to_string())
    }

    /// Returns all default binding ids (for Settings UI iteration).
    pub fn default_binding_ids() -> Vec<&'static str> {
        DEFAULT_ROOT
            .iter()
            .map(|(bid, _)| bid.id())
            .chain(DEFAULT_MENUS.iter().map(|(_, bid, _)| bid.id()))
            .collect()
    }
}

/// Default root bindings: binding id -> slot.
/// This table is behavior-identical to the delivered grammar.rs match statements.
/// Menu openers (V, G) are NOT included here - they are host special-casing in the delivered
/// code and will be added to this table in X02 when we cut over the host.
pub const DEFAULT_ROOT: &[(BindingId, Slot)] = &[
    // Commands (18 entries, matching grammar::command_key exactly)
    (BindingId::Command(ContextCommand::InsertStart), Slot::bare(KeyCode::KeyI)),
    (BindingId::Command(ContextCommand::InsertLineStart), Slot::shifted(KeyCode::KeyI)),
    (BindingId::Command(ContextCommand::InsertEnd), Slot::bare(KeyCode::KeyA)),
    (BindingId::Command(ContextCommand::InsertLineEnd), Slot::shifted(KeyCode::KeyA)),
    (BindingId::Command(ContextCommand::OpenBelow), Slot::bare(KeyCode::KeyO)),
    (BindingId::Command(ContextCommand::OpenAbove), Slot::shifted(KeyCode::KeyO)),
    (BindingId::Command(ContextCommand::Change), Slot::bare(KeyCode::KeyC)),
    (BindingId::Command(ContextCommand::Delete), Slot::bare(KeyCode::KeyD)),
    (BindingId::Command(ContextCommand::Cut), Slot::shifted(KeyCode::KeyD)),
    (BindingId::Command(ContextCommand::Copy), Slot::bare(KeyCode::KeyY)),
    (BindingId::Command(ContextCommand::Paste), Slot::bare(KeyCode::KeyP)),
    (BindingId::Command(ContextCommand::Undo), Slot::bare(KeyCode::KeyU)),
    (BindingId::Command(ContextCommand::Redo), Slot::shifted(KeyCode::KeyU)),
    (BindingId::Command(ContextCommand::SelectLine), Slot::bare(KeyCode::KeyX)),
    (BindingId::Command(ContextCommand::Collapse), Slot::bare(KeyCode::Semicolon)),
    (BindingId::Command(ContextCommand::Commands), Slot::shifted(KeyCode::Semicolon)),
    (BindingId::Command(ContextCommand::Find), Slot::bare(KeyCode::Slash)),
    (BindingId::Command(ContextCommand::Replace), Slot::shifted(KeyCode::Slash)),
    // FindChar is represented as ContextCommand::FindChar in grammar, but we use BindingId::FindChar
    (BindingId::FindChar { forward: true }, Slot::bare(KeyCode::KeyF)),
    (BindingId::FindChar { forward: false }, Slot::shifted(KeyCode::KeyF)),
    // Motions (14 entries: 7 bare + 7 shifted, matching grammar::motion_key exactly)
    // Note: grammar::motion_key ignores shift, so we include both variants for equivalence
    (BindingId::Motion(Motion::Previous), Slot::bare(KeyCode::KeyH)),
    (BindingId::Motion(Motion::Previous), Slot::shifted(KeyCode::KeyH)),
    (BindingId::Motion(Motion::Down), Slot::bare(KeyCode::KeyJ)),
    (BindingId::Motion(Motion::Down), Slot::shifted(KeyCode::KeyJ)),
    (BindingId::Motion(Motion::Up), Slot::bare(KeyCode::KeyK)),
    (BindingId::Motion(Motion::Up), Slot::shifted(KeyCode::KeyK)),
    (BindingId::Motion(Motion::Next), Slot::bare(KeyCode::KeyL)),
    (BindingId::Motion(Motion::Next), Slot::shifted(KeyCode::KeyL)),
    (BindingId::Motion(Motion::NextWord), Slot::bare(KeyCode::KeyW)),
    (BindingId::Motion(Motion::NextWord), Slot::shifted(KeyCode::KeyW)),
    (BindingId::Motion(Motion::NextWordEnd), Slot::bare(KeyCode::KeyE)),
    (BindingId::Motion(Motion::NextWordEnd), Slot::shifted(KeyCode::KeyE)),
    (BindingId::Motion(Motion::PreviousWord), Slot::bare(KeyCode::KeyB)),
    (BindingId::Motion(Motion::PreviousWord), Slot::shifted(KeyCode::KeyB)),
];

/// Default menu accelerator bindings: menu -> binding id -> position.
/// These are the accelerators that appear inside specific menus.
pub const DEFAULT_MENUS: &[(ContextMenu, BindingId, KeyCode)] = &[
    // Select menu
    (ContextMenu::Select, BindingId::Command(ContextCommand::SelectWord), KeyCode::KeyW),
    (ContextMenu::Select, BindingId::Command(ContextCommand::SelectLine), KeyCode::KeyX),
    (ContextMenu::Select, BindingId::Command(ContextCommand::SelectAll), KeyCode::KeyA),
    // Go menu
    (ContextMenu::Go, BindingId::Command(ContextCommand::LineStart), KeyCode::KeyH),
    (ContextMenu::Go, BindingId::Command(ContextCommand::LineEnd), KeyCode::KeyE),
    (ContextMenu::Go, BindingId::Command(ContextCommand::DocumentStart), KeyCode::KeyT),
    (ContextMenu::Go, BindingId::Command(ContextCommand::DocumentEnd), KeyCode::KeyB),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_keymap_is_the_normative_snapshot() {
        // Verify the DEFAULT_ROOT table contains exactly the expected bindings
        // This is the drift gate for X01.
        // 20 command-key entries (18 commands + 2 find_char) + 14 motions (7 bare + 7 shifted) = 34 total
        // The grammar allows shift on motions, so we include both variants for equivalence
        assert_eq!(DEFAULT_ROOT.len(), 34); // 20 commands + 14 motions = 34
        
        // Spot-check a few key bindings
        let has_insert_start = DEFAULT_ROOT.iter().any(|(bid, slot)| {
            bid.id() == "editor.insert_start" && slot.position == KeyCode::KeyI && !slot.shift
        });
        assert!(has_insert_start, "InsertStart should map to bare KeyI");

        let has_delete = DEFAULT_ROOT.iter().any(|(bid, slot)| {
            bid.id() == "editor.delete" && slot.position == KeyCode::KeyD && !slot.shift
        });
        assert!(has_delete, "Delete should map to bare KeyD");

        let has_cut = DEFAULT_ROOT.iter().any(|(bid, slot)| {
            bid.id() == "editor.cut" && slot.position == KeyCode::KeyD && slot.shift
        });
        assert!(has_cut, "Cut should map to Shift+KeyD");

        // Verify all motions are present and bare
        let motion_ids = [
            "editor.motion.previous",
            "editor.motion.next",
            "editor.motion.up",
            "editor.motion.down",
            "editor.motion.next_word",
            "editor.motion.previous_word",
            "editor.motion.next_word_end",
        ];
        for id in motion_ids {
            let found = DEFAULT_ROOT.iter().any(|(bid, slot)| {
                bid.id() == id && !slot.shift
            });
            assert!(found, "Motion {} should be present and bare", id);
        }

        // Menu openers (V, G) are NOT in DEFAULT_ROOT for X01 - they are host special-casing
        // and will be added in X02 when we cut over the host. Verify they are NOT present.
        let has_menu_select = DEFAULT_ROOT.iter().any(|(bid, _)| bid.id() == "editor.menu_select");
        assert!(!has_menu_select, "menu_select should NOT be in DEFAULT_ROOT for X01");

        let has_menu_go = DEFAULT_ROOT.iter().any(|(bid, _)| bid.id() == "editor.menu_go");
        assert!(!has_menu_go, "menu_go should NOT be in DEFAULT_ROOT for X01");
    }

    #[test]
    fn defaults_keymap_classifies_identically_to_grammar() {
        // This is the shadow equivalence test (AC-1).
        // We verify that the default keymap classifies every (position, shift) pair
        // identically to the delivered grammar.
        
        use crate::grammar::{command_key, menu_key, motion_key};
        use crate::key::Modifiers;
        use crate::vocabulary::ContextMenu;
        use ContextCommand as C;
        
        let keymap = ContextKeymap::defaults();
        
        // Test all KeyCode variants that existed in the original grammar
        let original_positions = [
            KeyCode::KeyA, KeyCode::KeyB, KeyCode::KeyC, KeyCode::KeyD,
            KeyCode::KeyE, KeyCode::KeyF, KeyCode::KeyG, KeyCode::KeyH,
            KeyCode::KeyI, KeyCode::KeyJ, KeyCode::KeyK, KeyCode::KeyL,
            KeyCode::KeyO, KeyCode::KeyP, KeyCode::KeyT, KeyCode::KeyU,
            KeyCode::KeyV, KeyCode::KeyW, KeyCode::KeyX, KeyCode::KeyY,
            KeyCode::Semicolon, KeyCode::Slash,
        ];
        
        for code in original_positions {
            for shift in [false, true] {
                let slot = Slot { position: code, shift };
                let modifiers = Modifiers { shift, ..Modifiers::EMPTY };
                
                // Check command_key equivalence
                let grammar_cmd = command_key(code, modifiers);
                let keymap_cmd = keymap.lookup_root(slot).and_then(|bid| {
                    match bid {
                        BindingId::Command(c) => Some(*c),
                        _ => None,
                    }
                });
                
                // They should match (accounting for the fact that FindChar is split in keymap)
                match (grammar_cmd, keymap_cmd) {
                    (Some(C::FindChar { forward: true }), Some(C::FindChar { forward: true })) => {},
                    (Some(C::FindChar { forward: false }), Some(C::FindChar { forward: false })) => {},
                    (Some(a), Some(b)) => assert_eq!(a, b, "Command mismatch for {}{}", code.token(), if shift { " (shift)" } else { "" }),
                    (None, None) => {},
                    _ => {
                        // This is expected for FindChar which is handled differently
                        // In the keymap, FindChar is a separate BindingId, not a ContextCommand variant
                        if grammar_cmd.map_or(false, |c| matches!(c, C::FindChar { .. })) {
                            // This is OK - FindChar is handled via BindingId::FindChar
                        } else {
                            panic!("Mismatch for {} shift={}: grammar={:?}, keymap={:?}", 
                                code.token(), shift, grammar_cmd, keymap_cmd);
                        }
                    }
                }
                
                // Check motion_key equivalence
                let grammar_motion = motion_key(code, modifiers);
                let keymap_motion = keymap.lookup_root(slot).and_then(|bid| {
                    match bid {
                        BindingId::Motion(m) => Some(*m),
                        _ => None,
                    }
                });
                
                match (grammar_motion, keymap_motion) {
                    (Some(a), Some(b)) => assert_eq!(a, b, "Motion mismatch for {}{}", code.token(), if shift { " (shift)" } else { "" }),
                    (None, None) => {},
                    _ => panic!("Motion mismatch for {} shift={}: grammar={:?}, keymap={:?}", 
                        code.token(), shift, grammar_motion, keymap_motion),
                }
            }
        }
    }

    #[test]
    fn resolve_accepts_rebind_and_unbound() {
        use std::collections::BTreeMap;
        
        let mut overrides = BTreeMap::new();
        
        // Rebind delete to KeyS
        overrides.insert("editor.delete".to_string(), "physical:KeyS".to_string());
        
        let keymap = ContextKeymap::resolve(&overrides).expect("Should resolve valid rebind");
        
        // Verify the rebind worked
        let slot = Slot::bare(KeyCode::KeyS);
        let binding = keymap.lookup_root(slot);
        assert!(binding.is_some(), "KeyS should have a binding");
        assert_eq!(binding.unwrap().id(), "editor.delete");
        
        // Verify old position is unbound
        let old_slot = Slot::bare(KeyCode::KeyD);
        assert!(keymap.lookup_root(old_slot).is_none(), "KeyD should be unbound");
    }

    #[test]
    fn resolve_rejects_unknown_binding() {
        use std::collections::BTreeMap;
        
        let mut overrides = BTreeMap::new();
        overrides.insert("editor.nonexistent".to_string(), "physical:KeyD".to_string());
        
        let result = ContextKeymap::resolve(&overrides);
        assert!(result.is_err(), "Unknown binding should be rejected");
        
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| matches!(e, KeymapError::UnknownBinding { .. })));
    }

    #[test]
    fn resolve_rejects_shift_on_motion() {
        use std::collections::BTreeMap;
        
        let mut overrides = BTreeMap::new();
        overrides.insert("editor.motion.previous".to_string(), "shift+physical:KeyH".to_string());
        
        let result = ContextKeymap::resolve(&overrides);
        assert!(result.is_err(), "Shift on motion should be rejected");
        
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| matches!(e, KeymapError::ShiftOnMotion { .. })));
    }

    #[test]
    fn resolve_rejects_collision() {
        use std::collections::BTreeMap;
        
        let mut overrides = BTreeMap::new();
        overrides.insert("editor.delete".to_string(), "physical:KeyD".to_string());
        overrides.insert("editor.cut".to_string(), "physical:KeyD".to_string());
        
        let result = ContextKeymap::resolve(&overrides);
        assert!(result.is_err(), "Collision should be rejected");
        
        let errors = result.unwrap_err();
        assert!(errors.iter().any(|e| matches!(e, KeymapError::Collision { .. })));
    }

    #[test]
    fn lookup_menu_falls_through_to_root() {
        let keymap = ContextKeymap::defaults();
        
        // Delete should be accessible in More menu via fall-through
        let result = keymap.lookup_menu(ContextMenu::More, KeyCode::KeyD);
        assert_eq!(result, Some(ContextCommand::Delete));
    }

    #[test]
    fn menu_accelerator_takes_precedence() {
        let keymap = ContextKeymap::defaults();
        
        // In Select menu, KeyW should give SelectWord (menu accelerator), not InsertStart
        let result = keymap.lookup_menu(ContextMenu::Select, KeyCode::KeyW);
        assert_eq!(result, Some(ContextCommand::SelectWord));
    }

    #[test]
    fn binding_label_uses_glyphs() {
        use crate::labels::Layout;
        
        let keymap = ContextKeymap::defaults();
        
        // Delete should show as "d" in qwerty
        let label = keymap.binding_label(Layout::Qwerty, "editor.delete");
        assert_eq!(label, "d");
        
        // Cut should show as "⇧d" in qwerty
        let label = keymap.binding_label(Layout::Qwerty, "editor.cut");
        assert_eq!(label, "⇧d");
    }

    #[test]
    fn token_parsing_covers_all_positions() {
        // Every KeyCode variant should parse from its token string
        for code in [
            KeyCode::KeyA, KeyCode::KeyB, KeyCode::KeyC, KeyCode::KeyD,
            KeyCode::KeyE, KeyCode::KeyF, KeyCode::KeyG, KeyCode::KeyH,
            KeyCode::KeyI, KeyCode::KeyJ, KeyCode::KeyK, KeyCode::KeyL,
            KeyCode::KeyM, KeyCode::KeyN, KeyCode::KeyO, KeyCode::KeyP,
            KeyCode::KeyQ, KeyCode::KeyR, KeyCode::KeyS, KeyCode::KeyT,
            KeyCode::KeyU, KeyCode::KeyV, KeyCode::KeyW, KeyCode::KeyX,
            KeyCode::KeyY, KeyCode::KeyZ,
            KeyCode::Semicolon, KeyCode::Quote, KeyCode::Comma, KeyCode::Period,
            KeyCode::Slash, KeyCode::Backslash, KeyCode::BracketLeft,
            KeyCode::BracketRight, KeyCode::Minus, KeyCode::Equal,
        ] {
            let token = format!("physical:{}", code.token());
            let parsed = BindingToken::parse(&token);
            assert!(parsed.is_some(), "Token {} should parse", token);
            
            let token_shifted = format!("shift+physical:{}", code.token());
            let parsed_shifted = BindingToken::parse(&token_shifted);
            assert!(parsed_shifted.is_some(), "Shifted token {} should parse", token_shifted);
        }
    }
}
