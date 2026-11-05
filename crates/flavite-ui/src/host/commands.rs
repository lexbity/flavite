//! The host command catalog: UI workspace commands and their bindings.
//!
//! Every shell entry point — palette, keyboard chord, button, native
//! accessibility action — resolves through [`ui_specs`] and [`dispatch`],
//! so availability and labels cannot drift between surfaces (D-16).

use crate::app::{App, Message};
use flavite_api::{ArgSchema, Chord, CommandSpec};
use flavite_workspace::{GroupId, LayoutAction, NoteLocator, TabId, ViewKind};
use iced::keyboard::{self, Modifiers};

/// Remappable shell commands and their default chords (W11 input catalog).
/// The chord strings parse through [`crate::preferences::parse_chord`];
/// `Mod` is the platform command modifier. A preference override replaces
/// the default for that command id only.
pub const DEFAULT_CHORDS: &[(&str, &str)] = &[
    ("workspace.find", "Mod+F"),
    ("workspace.replace", "Mod+H"),
    ("workspace.note_mode", "Mod+E"),
    ("workspace.quick_switch", "Mod+P"),
    ("workspace.command_palette", "Mod+Shift+P"),
    ("workspace.new_tab", "Mod+T"),
    ("workspace.close_tab", "Mod+W"),
    ("workspace.reopen_tab", "Mod+Shift+T"),
    ("workspace.next_tab", "Mod+Tab"),
    ("workspace.previous_tab", "Mod+Shift+Tab"),
    ("workspace.next_group", "F6"),
    ("workspace.previous_group", "Shift+F6"),
    ("workspace.split_right", "Mod+Alt+Right"),
    ("workspace.split_down", "Mod+Alt+Down"),
    ("workspace.move_tab_next_group", "Mod+Alt+Shift+Right"),
    ("workspace.move_tab_previous_group", "Mod+Alt+Shift+Left"),
    ("workspace.maximize_group", "Mod+Shift+M"),
    ("workspace.navigate_back", "Alt+Left"),
    ("workspace.navigate_forward", "Alt+Shift+Right"),
    ("workspace.new_note", "Mod+N"),
    ("workspace.open_note_dialog", "Mod+O"),
    ("workspace.group_actions", "Shift+F10"),
    ("workspace.preferences", "Mod+,"),
    ("note.save", "Mod+S"),
];

/// The pressed modifier state as the API's chord bit mask.
#[must_use]
pub fn modifier_bits(modifiers: &Modifiers) -> u8 {
    use flavite_api::command::modifiers as mods;
    let mut bits = mods::NONE;
    if modifiers.command() {
        bits |= mods::CTRL;
    }
    if modifiers.shift() {
        bits |= mods::SHIFT;
    }
    if modifiers.alt() {
        bits |= mods::ALT;
    }
    if modifiers.logo() {
        bits |= mods::SUPER;
    }
    bits
}

/// The canonical chord token for an iced key (`P`, `F6`, `ArrowLeft`, ...).
#[must_use]
pub fn key_token(key: &keyboard::Key) -> Option<String> {
    match key {
        keyboard::Key::Character(character) => {
            let mut characters = character.chars();
            match (characters.next(), characters.next()) {
                (Some(single), None) => Some(single.to_uppercase().next()?.to_string()),
                _ => None,
            }
        }
        keyboard::Key::Named(named) => named_token(*named).map(str::to_owned),
        keyboard::Key::Unidentified => None,
    }
}

/// The canonical chord token stored in a chord's key string.
#[must_use]
pub fn chord_key_token(key: &str) -> Option<String> {
    let token = key.trim();
    const NAMED: &[(&str, &str)] = &[
        ("Return", "Enter"),
        ("Esc", "Escape"),
        ("Del", "Delete"),
        ("Up", "ArrowUp"),
        ("Down", "ArrowDown"),
        ("Left", "ArrowLeft"),
        ("Right", "ArrowRight"),
    ];
    for (alias, canonical) in NAMED {
        if token.eq_ignore_ascii_case(alias) {
            return Some((*canonical).to_owned());
        }
    }
    Some(token.to_uppercase())
}

fn named_token(named: keyboard::key::Named) -> Option<&'static str> {
    use keyboard::key::Named;
    Some(match named {
        Named::Enter => "Enter",
        Named::Tab => "Tab",
        Named::Escape => "Escape",
        Named::Backspace => "Backspace",
        Named::Delete => "Delete",
        Named::Home => "Home",
        Named::End => "End",
        Named::PageUp => "PageUp",
        Named::PageDown => "PageDown",
        Named::ArrowUp => "ArrowUp",
        Named::ArrowDown => "ArrowDown",
        Named::ArrowLeft => "ArrowLeft",
        Named::ArrowRight => "ArrowRight",
        Named::F1 => "F1",
        Named::F2 => "F2",
        Named::F3 => "F3",
        Named::F4 => "F4",
        Named::F5 => "F5",
        Named::F6 => "F6",
        Named::F7 => "F7",
        Named::F8 => "F8",
        Named::F9 => "F9",
        Named::F10 => "F10",
        Named::F11 => "F11",
        Named::F12 => "F12",
        _ => return None,
    })
}

/// One UI-only command. Workspace commands are namespaced `workspace.*` so
/// a collision with core command ids is structurally impossible and still
/// checked at startup (D-16).
#[derive(Debug, Clone, Copy)]
pub struct UiCommand {
    /// Stable command id.
    pub id: &'static str,
    /// Palette title.
    pub title: &'static str,
}

/// All UI commands, in palette order. A capability absent from a stage
/// build is absent here (RFC §16.1): no placeholder rows.
pub const UI_COMMANDS: &[UiCommand] = &[
    UiCommand {
        id: "workspace.find",
        title: "Find in current note (includes unsaved text)",
    },
    UiCommand {
        id: "workspace.replace",
        title: "Replace in current note (literal)",
    },
    UiCommand {
        id: "context.source_preview",
        title: "Toggle source / rendered Preview",
    },
    UiCommand {
        id: "workspace.note_mode",
        title: "Toggle note Edit / Read",
    },
    UiCommand {
        id: "workspace.open_outline",
        title: "Open Outline",
    },
    UiCommand {
        id: "workspace.open_properties",
        title: "Open Properties",
    },
    UiCommand {
        id: "workspace.group_actions",
        title: "Open group actions menu",
    },
    UiCommand {
        id: "workspace.all_tabs",
        title: "Show all tabs in this group",
    },
    UiCommand {
        id: "workspace.new_view",
        title: "Open new view menu",
    },
    UiCommand {
        id: "workspace.quick_switch",
        title: "Quick switch: open a note",
    },
    UiCommand {
        id: "workspace.command_palette",
        title: "Command palette",
    },
    UiCommand {
        id: "workspace.preferences",
        title: "Open preferences (theme, scale, shortcuts)",
    },
    UiCommand {
        id: "workspace.toggle_theme",
        title: "Toggle light / dark theme",
    },
    UiCommand {
        id: "workspace.cycle_interface_scale",
        title: "Cycle interface scale (100 / 150 / 200 / 75 %)",
    },
    UiCommand {
        id: "workspace.new_tab",
        title: "New empty tab",
    },
    UiCommand {
        id: "workspace.close_tab",
        title: "Close tab",
    },
    UiCommand {
        id: "workspace.reopen_tab",
        title: "Reopen closed tab",
    },
    UiCommand {
        id: "workspace.duplicate_tab",
        title: "Duplicate tab",
    },
    UiCommand {
        id: "workspace.next_tab",
        title: "Next tab",
    },
    UiCommand {
        id: "workspace.previous_tab",
        title: "Previous tab",
    },
    UiCommand {
        id: "workspace.next_group",
        title: "Next group",
    },
    UiCommand {
        id: "workspace.previous_group",
        title: "Previous group",
    },
    UiCommand {
        id: "workspace.split_right",
        title: "Split group right",
    },
    UiCommand {
        id: "workspace.split_down",
        title: "Split group down",
    },
    UiCommand {
        id: "workspace.maximize_group",
        title: "Maximize or restore group",
    },
    UiCommand {
        id: "workspace.move_tab_next_group",
        title: "Move tab to next group",
    },
    UiCommand {
        id: "workspace.move_tab_previous_group",
        title: "Move tab to previous group",
    },
    UiCommand {
        id: "workspace.merge_next_group",
        title: "Merge next group into this one",
    },
    UiCommand {
        id: "workspace.open_notes_view",
        title: "Open notes list",
    },
    UiCommand {
        id: "workspace.open_search_view",
        title: "Open saved-content search",
    },
    UiCommand {
        id: "workspace.new_note",
        title: "New note…",
    },
    UiCommand {
        id: "workspace.open_note_dialog",
        title: "Open note…",
    },
    UiCommand {
        id: "workspace.open_in_tab",
        title: "Open note in this tab…",
    },
    UiCommand {
        id: "workspace.rename_note",
        title: "Rename or move note…",
    },
    UiCommand {
        id: "workspace.open_history",
        title: "Open note history",
    },
    UiCommand {
        id: "workspace.navigate_back",
        title: "Navigate back",
    },
    UiCommand {
        id: "workspace.navigate_forward",
        title: "Navigate forward",
    },
    UiCommand {
        id: "workspace.select_all",
        title: "Select all",
    },
    UiCommand {
        id: "workspace.caret_start",
        title: "Move caret to start",
    },
    UiCommand {
        id: "workspace.caret_end",
        title: "Move caret to end",
    },
    UiCommand {
        id: "workspace.refresh_catalog",
        title: "Refresh note catalog",
    },
    UiCommand {
        id: "help.about",
        title: "About Flavite",
    },
    UiCommand {
        id: "workspace.open_graph",
        title: "Open graph view",
    },
    UiCommand {
        id: "graph.freeze",
        title: "Freeze graph layout",
    },
    UiCommand {
        id: "graph.fit",
        title: "Fit graph to view",
    },
    UiCommand {
        id: "graph.list",
        title: "Toggle graph list",
    },
    UiCommand {
        id: "graph.filters",
        title: "Show graph filters",
    },
    UiCommand {
        id: "graph.apply_filters",
        title: "Apply graph filters",
    },
    UiCommand {
        id: "graph.clear_filters",
        title: "Clear graph filters",
    },
    UiCommand {
        id: "graph.undo_filters",
        title: "Undo graph filters",
    },
    UiCommand {
        id: "graph.explore",
        title: "Explore selected neighborhood",
    },
    UiCommand {
        id: "graph.clear_selection",
        title: "Clear graph selection",
    },
    UiCommand {
        id: "graph.open_selected",
        title: "Open the selected graph note",
    },
    UiCommand {
        id: "graph.insert_selected",
        title: "Insert a link to the selected graph note at the caret",
    },
    UiCommand {
        id: "workspace.open_preview",
        title: "Open preview of the followed note",
    },
    UiCommand {
        id: "workspace.open_backlinks",
        title: "Open backlinks of the followed note",
    },
    UiCommand {
        id: "context.pin",
        title: "Pin this context tool to its current source",
    },
    UiCommand {
        id: "context.follow_document",
        title: "Follow the document anchor again",
    },
    UiCommand {
        id: "link.insert_target",
        title: "Insert a link to the followed note at the caret",
    },
    UiCommand {
        id: "workspace.save_layout",
        title: "Save workspace layout now",
    },
    UiCommand {
        id: "workspace.reset_layout",
        title: "Reset workspace layout to the default",
    },
    UiCommand {
        id: "workspace.close_group",
        title: "Close group",
    },
];

/// The UI portion of the host catalog, rendered as core `CommandSpec`s for
/// the shared palette matcher.
#[must_use]
pub fn ui_specs(overrides: &std::collections::BTreeMap<String, String>) -> Vec<CommandSpec> {
    UI_COMMANDS
        .iter()
        .map(|command| {
            let keys: Vec<Chord> = resolved_chord(command.id, overrides).into_iter().collect();
            CommandSpec::with_availability(
                command.id,
                command.title,
                ArgSchema::typed::<serde_json::Value>(),
                None,
                crate::preferences::intern_chords(&keys),
            )
        })
        .collect()
}

/// The effective chord for a command id: the validated preference
/// override when present, otherwise the catalog default.
#[must_use]
pub fn resolved_chord(
    id: &str,
    overrides: &std::collections::BTreeMap<String, String>,
) -> Option<Chord> {
    let default = DEFAULT_CHORDS
        .iter()
        .find(|(command, _)| *command == id)
        .map(|(_, chord)| *chord)?;
    let text = overrides.get(id).map(String::as_str).unwrap_or(default);
    crate::preferences::parse_chord(text).or_else(|| crate::preferences::parse_chord(default))
}

/// Verify the composed catalog has no collisions with the core registry.
/// Called at vault attach; a collision is a startup defect, surfaced in the
/// status line and refused.
#[must_use]
pub fn catalog_conflicts(core_ids: &[&str]) -> Vec<String> {
    let mut conflicts = Vec::new();
    for core in core_ids {
        if *core == "workspace.quick_switch" || core.starts_with("workspace.") {
            conflicts.push((*core).to_owned());
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    for command in UI_COMMANDS {
        if !seen.insert(command.id) {
            conflicts.push(command.id.to_owned());
        }
    }
    conflicts
}

/// Resolve a keyboard event to a host message. Overlay navigation takes
/// precedence; plain typing never triggers layout commands.
#[must_use]
pub fn keyboard_message(app: &App, event: keyboard::Event) -> Option<Message> {
    let keyboard::Event::KeyPressed {
        key,
        modifiers,
        text,
        ..
    } = event
    else {
        return None;
    };
    // A Preferences shortcut row is capturing: the next key press becomes
    // that command's chord; Escape cancels (W11 collision editor).
    if let Some(command) = app.shortcut_capture.clone() {
        if matches!(&key, keyboard::Key::Named(keyboard::key::Named::Escape)) {
            return Some(Message::Preferences(
                crate::host::appearance::PreferenceMessage::ShortcutCaptured {
                    command,
                    chord: None,
                },
            ));
        }
        let Some(token) = key_token(&key) else {
            // Bare modifier key presses carry no key token; wait for the
            // full combination.
            return None;
        };
        let mut parts = Vec::new();
        if modifiers.command() {
            parts.push("Mod");
        }
        if modifiers.alt() {
            parts.push("Alt");
        }
        if modifiers.shift() {
            parts.push("Shift");
        }
        parts.push(token.as_str());
        let chord = parts.join("+");
        return Some(Message::Preferences(
            crate::host::appearance::PreferenceMessage::ShortcutCaptured {
                command,
                chord: Some(chord),
            },
        ));
    }
    // Modal dialogs take Escape (cancel) and Enter (primary action) first;
    // the submit arm routes per dialog kind (W12).
    if app.dialog.is_some() {
        if matches!(&key, keyboard::Key::Named(keyboard::key::Named::Enter)) {
            return Some(Message::DialogSubmit);
        }
        if matches!(&key, keyboard::Key::Named(keyboard::key::Named::Escape)) {
            return Some(Message::DialogCancel);
        }
    }
    if matches!(&key, keyboard::Key::Named(keyboard::key::Named::Escape))
        && !app.palette_open
        && !app.picker.open
    {
        return Some(Message::CancelGesture);
    }
    if app.strip_menu.is_some() {
        return match key {
            keyboard::Key::Named(keyboard::key::Named::ArrowDown) => Some(Message::MenuNext(true)),
            keyboard::Key::Named(keyboard::key::Named::ArrowUp) => Some(Message::MenuNext(false)),
            keyboard::Key::Named(keyboard::key::Named::Enter) => {
                Some(Message::MenuPick(app.menu_selected))
            }
            _ => None,
        };
    }
    // Overlay-local navigation first (§13.1: IME/plain typing precedence).
    if app.palette_open || app.picker.open {
        if modifiers.is_empty()
            && let keyboard::Key::Named(named) = &key
        {
            return overlay_navigation(app, *named);
        }
        if app.picker.open
            && modifiers.command()
            && matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::Enter)
            )
        {
            return Some(Message::PickerInsert(app.picker.selected));
        }
        if modifiers.command()
            && let keyboard::Key::Character(character) = key.as_ref()
            && character.eq_ignore_ascii_case("p")
        {
            return Some(if app.palette_open {
                Message::PaletteClose
            } else {
                Message::PickerClose
            });
        }
        return None;
    }
    if modifiers.shift() && matches!(&key, keyboard::Key::Named(keyboard::key::Named::F10)) {
        return Some(Message::RunHostCommand("workspace.group_actions"));
    }
    // Graph tab operations (§9.3 keyboard): Enter opens the selected
    // note; command+Enter requests one link insertion at the retained
    // writing target. No selection or a non-graph tab never fires.
    if matches!(&key, keyboard::Key::Named(keyboard::key::Named::Enter))
        && let Some(tab) = graph_action_tab(app)
    {
        let command = if modifiers.is_empty() {
            Some(crate::host::graph::GraphCommand::OpenSelected)
        } else if modifiers.command() {
            Some(crate::host::graph::GraphCommand::InsertSelected)
        } else {
            None
        };
        if let Some(command) = command {
            return Some(Message::GraphCommand { tab, command });
        }
    }
    // Layout and shell chords resolve through the effective shortcut
    // table (W11): defaults with validated preference overrides, matched
    // exactly on modifiers and key. Availability is rechecked per command.
    if let Some(id) = app.appearance.chord_command(&key, &modifiers) {
        return keyboard_command_message(app, id);
    }
    if let Some(action) = splitter_increment(app, &key, &modifiers) {
        return Some(Message::Layout(action));
    }
    // An overlay hotkey that is also a printable character (P) must not
    // fire while the user types into a text field; overlays are the only
    // text inputs in W02, handled above.
    let _ = text;
    None
}

/// The message for a chord-resolved command id. This preserves the exact
/// keyboard semantics of the pre-W11 chords (layout actions carry the
/// current revision guard; availability is presentation advice).
fn keyboard_command_message(app: &App, id: &'static str) -> Option<Message> {
    let layout = |action: LayoutAction| Some(Message::Layout(action));
    let revision = app.workspace.revision();
    match id {
        "workspace.find"
        | "workspace.replace"
        | "workspace.note_mode"
        | "workspace.new_tab"
        | "workspace.close_tab"
        | "workspace.maximize_group"
        | "workspace.new_note"
        | "workspace.open_note_dialog"
        | "workspace.group_actions"
        | "workspace.move_tab_next_group"
        | "workspace.move_tab_previous_group"
        | "workspace.navigate_back"
        | "workspace.navigate_forward" => Some(Message::RunHostCommand(id)),
        "workspace.quick_switch" => Some(Message::PickerOpen),
        "workspace.command_palette" => Some(Message::PaletteOpen),
        "workspace.reopen_tab" => layout(LayoutAction::ReopenClosed {
            expected_revision: revision,
        }),
        "workspace.next_tab" => layout(LayoutAction::CycleTab {
            expected_revision: revision,
            forward: true,
        }),
        "workspace.previous_tab" => layout(LayoutAction::CycleTab {
            expected_revision: revision,
            forward: false,
        }),
        "workspace.next_group" => layout(LayoutAction::CycleGroup {
            expected_revision: revision,
            forward: true,
        }),
        "workspace.previous_group" => layout(LayoutAction::CycleGroup {
            expected_revision: revision,
            forward: false,
        }),
        "workspace.split_right" => layout(LayoutAction::SplitGroup {
            expected_revision: revision,
            group: app.focused_group()?,
            arrangement: flavite_workspace::Arrangement::SideBySide,
        }),
        "workspace.split_down" => layout(LayoutAction::SplitGroup {
            expected_revision: revision,
            group: app.focused_group()?,
            arrangement: flavite_workspace::Arrangement::Stacked,
        }),
        "note.save" => Some(Message::RunCoreCommand("note.save")),
        _ => None,
    }
}

/// Keyboard splitter increments (D-13, W11): with a focused group,
/// `Mod+Alt+Shift+Arrow` grows or shrinks the split that separates it from
/// its sibling. The arrow points in the growth direction of the focused
/// group; only the arrows along the split's axis fire. The step is 2.5%
/// of usable space, committed as one normal `ResizeSplit` transaction.
fn splitter_increment(
    app: &App,
    key: &keyboard::Key,
    modifiers: &Modifiers,
) -> Option<LayoutAction> {
    use flavite_workspace::{Side, layout::Arrangement as Arr};
    use keyboard::key::Named;
    if modifier_bits(modifiers)
        != flavite_api::command::modifiers::CTRL
            | flavite_api::command::modifiers::ALT
            | flavite_api::command::modifiers::SHIFT
    {
        return None;
    }
    let keyboard::Key::Named(named) = key else {
        return None;
    };
    if !matches!(
        named,
        Named::ArrowUp | Named::ArrowDown | Named::ArrowLeft | Named::ArrowRight
    ) {
        return None;
    }
    let focused = app.focused_group()?;
    let (split, arrangement, side) = adjacent_split(app.workspace.tree(), &focused)?;
    let grow_toward_sibling = match (named, arrangement) {
        (Named::ArrowRight, Arr::SideBySide) | (Named::ArrowUp, Arr::Stacked) => false,
        (Named::ArrowLeft, Arr::SideBySide) | (Named::ArrowDown, Arr::Stacked) => true,
        _ => return None,
    };
    // Growing a first child raises the ratio; growing a second child
    // lowers it. Shrink arrows mirror this.
    let direction: i64 = match (side, grow_toward_sibling) {
        (Side::First, false) | (Side::Second, true) => 1,
        (Side::First, true) | (Side::Second, false) => -1,
    };
    let current = current_ratio(app.workspace.tree(), split)?;
    let step = 25_000_i64;
    let desired = (i64::from(current) + direction * step).clamp(
        i64::from(flavite_workspace::MIN_RATIO_PPM),
        i64::from(flavite_workspace::MAX_RATIO_PPM),
    );
    Some(LayoutAction::ResizeSplit {
        expected_revision: app.workspace.revision(),
        split,
        desired_ratio_ppm: desired as u32,
    })
}

/// The split adjacent to `group` (its own parent split), with the side
/// the group sits on. Returns the innermost enclosing split first.
fn adjacent_split(
    node: &flavite_workspace::LayoutNode,
    group: &GroupId,
) -> Option<(
    flavite_workspace::SplitId,
    flavite_workspace::Arrangement,
    flavite_workspace::Side,
)> {
    match node {
        flavite_workspace::LayoutNode::Group(_) => None,
        flavite_workspace::LayoutNode::Split {
            id,
            arrangement,
            first,
            second,
            ..
        } => {
            if first.contains_group(*group) {
                Some((*id, *arrangement, flavite_workspace::Side::First))
            } else if second.contains_group(*group) {
                Some((*id, *arrangement, flavite_workspace::Side::Second))
            } else {
                adjacent_split(first, group).or_else(|| adjacent_split(second, group))
            }
        }
    }
}

/// The current desired ratio of a split.
fn current_ratio(
    node: &flavite_workspace::LayoutNode,
    split: flavite_workspace::SplitId,
) -> Option<u32> {
    match node {
        flavite_workspace::LayoutNode::Group(_) => None,
        flavite_workspace::LayoutNode::Split {
            id,
            desired_ratio_ppm,
            first,
            second,
            ..
        } => {
            if *id == split {
                Some(*desired_ratio_ppm)
            } else {
                current_ratio(first, split).or_else(|| current_ratio(second, split))
            }
        }
    }
}

fn overlay_navigation(app: &App, named: keyboard::key::Named) -> Option<Message> {
    match named {
        keyboard::key::Named::Escape => {
            if app.palette_open {
                Some(Message::PaletteClose)
            } else {
                Some(Message::PickerClose)
            }
        }
        keyboard::key::Named::ArrowDown => {
            if app.palette_open {
                Some(Message::PaletteNext)
            } else {
                Some(Message::PickerNext)
            }
        }
        keyboard::key::Named::ArrowUp => {
            if app.palette_open {
                Some(Message::PalettePrevious)
            } else {
                Some(Message::PickerPrevious)
            }
        }
        keyboard::key::Named::Enter => {
            if app.palette_open {
                Some(Message::PaletteSubmit)
            } else {
                Some(Message::PickerSubmit)
            }
        }
        _ => None,
    }
}
/// Dispatch a host command id against current shell state. Availability
/// failures surface as a status message, never as a silent no-op.
pub fn dispatch(app: &mut App, id: &'static str) -> iced::Task<Message> {
    // W12: while the quit schedule is being resolved the interface is
    // paused; every command waits for the user's decision or cancellation.
    if app.shutdown.is_some() {
        app.status = "Quitting: resolve the quit (decide or cancel) first".into();
        return iced::Task::none();
    }
    if !available(app, id) {
        app.status = "This command is unavailable in the current view".into();
        return iced::Task::none();
    }
    let focused_group = app.focused_group();
    match id {
        "workspace.group_actions" | "workspace.all_tabs" | "workspace.new_view" => {
            if let Some(group) = focused_group {
                let menu = match id {
                    "workspace.all_tabs" => crate::workspace::StripMenu::Tabs,
                    "workspace.new_view" => crate::workspace::StripMenu::NewView,
                    _ => crate::workspace::StripMenu::Actions,
                };
                return app.update(Message::GroupMenu(group, menu));
            }
        }
        "workspace.quick_switch" => return app.update(Message::PickerOpen),
        "workspace.command_palette" => return app.update(Message::PaletteOpen),
        "workspace.new_tab" => {
            if let Some(group) = focused_group {
                app.dispatch_layout(|revision| LayoutAction::OpenEmpty {
                    expected_revision: revision,
                    group,
                });
            }
        }
        "workspace.close_tab" => {
            // W12: through the close plan (final-view lease accounting).
            let _ = app.request_close_tab(app.workspace.focused_tab());
        }
        "workspace.reopen_tab" => app.dispatch_layout(|revision| LayoutAction::ReopenClosed {
            expected_revision: revision,
        }),
        "workspace.duplicate_tab" => {
            let tab = app.workspace.focused_tab();
            app.dispatch_layout(|revision| LayoutAction::DuplicateTab {
                expected_revision: revision,
                tab,
            });
        }
        "workspace.next_tab" => app.dispatch_layout(|revision| LayoutAction::CycleTab {
            expected_revision: revision,
            forward: true,
        }),
        "workspace.previous_tab" => app.dispatch_layout(|revision| LayoutAction::CycleTab {
            expected_revision: revision,
            forward: false,
        }),
        "workspace.next_group" => app.dispatch_layout(|revision| LayoutAction::CycleGroup {
            expected_revision: revision,
            forward: true,
        }),
        "workspace.previous_group" => app.dispatch_layout(|revision| LayoutAction::CycleGroup {
            expected_revision: revision,
            forward: false,
        }),
        "workspace.split_right" => {
            if let Some(group) = focused_group {
                app.dispatch_layout(|revision| LayoutAction::SplitGroup {
                    expected_revision: revision,
                    group,
                    arrangement: flavite_workspace::Arrangement::SideBySide,
                });
            }
        }
        "workspace.split_down" => {
            if let Some(group) = focused_group {
                app.dispatch_layout(|revision| LayoutAction::SplitGroup {
                    expected_revision: revision,
                    group,
                    arrangement: flavite_workspace::Arrangement::Stacked,
                });
            }
        }
        "workspace.maximize_group" => {
            if let Some(group) = app.maximized_group().or(focused_group) {
                app.dispatch_layout(|revision| LayoutAction::ToggleMaximize {
                    expected_revision: revision,
                    group,
                });
            }
        }
        "workspace.close_group" => {
            // W12: group close plans per tab; undecided tabs are retained.
            if let Some(group) = focused_group {
                let _ = app.request_close_group(group);
            }
        }
        "workspace.move_tab_next_group" => {
            move_tab_to_neighbor(app, true);
        }
        "workspace.move_tab_previous_group" => {
            move_tab_to_neighbor(app, false);
        }
        "workspace.merge_next_group" => {
            if let Some(focused) = focused_group
                && let Some(source) = app.next_group_of(focused, true)
            {
                app.dispatch_layout(|revision| LayoutAction::MergeGroups {
                    expected_revision: revision,
                    source,
                    destination: focused,
                });
            }
        }
        "workspace.open_notes_view" => {
            if let Some(group) = focused_group {
                app.dispatch_layout(|revision| LayoutAction::OpenNotes {
                    expected_revision: revision,
                    group,
                });
            }
        }
        // ---- W10 ----
        "workspace.open_search_view" => {
            if let Some(group) = focused_group {
                app.dispatch_layout(|revision| LayoutAction::OpenSearch {
                    expected_revision: revision,
                    group,
                });
                // The search input is the view's primary control; focus it so
                // typed queries land immediately.
                if let Some(active) = app.workspace.group(&group).map(|g| g.active()) {
                    let id = iced::widget::Id::from(format!(
                        "{}{}",
                        crate::views::search::SEARCH_INPUT_ID_PREFIX,
                        active.raw()
                    ));
                    return iced::widget::operation::focus(id);
                }
            }
        }
        "workspace.new_note" => {
            app.dialog = Some(crate::host::dialogs::Dialog::NewNote {
                path: String::new(),
            });
            return iced::widget::operation::focus(crate::host::dialogs::NEW_NOTE_INPUT_ID);
        }
        "workspace.open_note_dialog" => {
            return crate::host::dialogs::pick_note_file();
        }
        "workspace.open_in_tab" => {
            // Replace the focused note tab through the shared picker; the
            // replaced note lands in the tab's Back history.
            app.picker.open = true;
            app.picker.query.clear();
            app.picker.selected = 0;
            app.picker.replace_tab = app.focused_note_tab();
        }
        "workspace.rename_note" => {
            if let Some(tab) = app.focused_note_tab()
                && let Some(from) = app.note_locator(tab)
            {
                app.dialog = Some(crate::host::dialogs::Dialog::Rename {
                    from,
                    to: String::new(),
                    tier: None,
                });
                return iced::widget::operation::focus(crate::host::dialogs::RENAME_INPUT_ID);
            }
        }
        "workspace.open_history" => {
            if let Some(tab) = app.focused_note_tab()
                && let Some(locator) = app.note_locator(tab)
                && let Some(group) = focused_group
            {
                app.dispatch_layout(|revision| LayoutAction::OpenHistory {
                    expected_revision: revision,
                    locator,
                    group,
                });
            }
        }
        "workspace.navigate_back" => {
            return app.update(Message::NavigateBack);
        }
        "workspace.navigate_forward" => {
            return app.update(Message::NavigateForward);
        }
        "workspace.select_all" | "workspace.caret_start" | "workspace.caret_end" => {
            let tab = app.workspace.focused_tab();
            let (anchor, head) = match id {
                "workspace.select_all" => (0usize, usize::MAX),
                "workspace.caret_start" => (0usize, 0usize),
                _ => (usize::MAX, usize::MAX),
            };
            return app.update(Message::SelectionCommand { tab, anchor, head });
        }
        "workspace.refresh_catalog" => app.request_catalog_refresh(),
        "help.about" => {
            app.dialog = Some(crate::host::dialogs::Dialog::About);
        }
        "workspace.preferences" => {
            app.shortcut_capture = None;
            app.dialog = Some(crate::host::dialogs::Dialog::Preferences);
        }
        "workspace.toggle_theme" => {
            // Toggle resolves the active scheme to an explicit choice, so
            // the flip is one undoable, inspectable preference edit.
            let next = match app.appearance.scheme() {
                crate::style::Scheme::Dark => crate::preferences::ThemeChoice::Light,
                crate::style::Scheme::Light => crate::preferences::ThemeChoice::Dark,
            };
            return app.update(Message::Preferences(
                crate::host::appearance::PreferenceMessage::SetTheme(next),
            ));
        }
        "workspace.cycle_interface_scale" => {
            const LADDER: [u16; 4] = [100, 150, 200, 75];
            let current = app.appearance.prefs.interface_scale_pct;
            let position = LADDER.iter().position(|step| *step == current);
            let next = match position {
                Some(index) => LADDER[(index + 1) % LADDER.len()],
                None => 100,
            };
            return app.update(Message::Preferences(
                crate::host::appearance::PreferenceMessage::SetScale(next),
            ));
        }
        "workspace.open_graph" => {
            if let Some(group) = focused_group {
                app.dispatch_layout(|revision| LayoutAction::OpenGraph {
                    expected_revision: revision,
                    group,
                });
            }
        }
        "graph.freeze" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::ToggleFreeze,
            });
        }
        "graph.fit" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::FitView,
            });
        }
        "graph.list" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::ToggleList,
            });
        }
        "graph.filters" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::ToggleFilters,
            });
        }
        "graph.apply_filters" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::ApplyFilters,
            });
        }
        "graph.clear_filters" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::ClearFilters,
            });
        }
        "graph.undo_filters" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::UndoFilters,
            });
        }
        "graph.explore" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::ExploreSelected,
            });
        }
        "graph.clear_selection" => {
            return app.update(Message::GraphCommand {
                tab: app.workspace.focused_tab(),
                command: crate::host::graph::GraphCommand::ClearSelection,
            });
        }
        "graph.open_selected" | "graph.insert_selected" => {
            let tab = app.workspace.focused_tab();
            let command = if id == "graph.open_selected" {
                crate::host::graph::GraphCommand::OpenSelected
            } else {
                crate::host::graph::GraphCommand::InsertSelected
            };
            return app.update(Message::GraphCommand { tab, command });
        }
        "workspace.find" => {
            return app.update(Message::FindOpen(app.workspace.focused_tab()));
        }
        "workspace.replace" => {
            return app.update(Message::NoteSearch(
                crate::host::note_search::NoteSearchMessage::ReplaceOpen(
                    app.workspace.focused_tab(),
                ),
            ));
        }
        "context.source_preview" => {
            return app.update(Message::ToggleSourcePreview(app.workspace.focused_tab()));
        }
        "workspace.note_mode" => {
            return app.update(Message::ToggleNoteMode(app.workspace.focused_tab()));
        }
        "workspace.open_outline" => {
            return app.update(Message::OpenContextView {
                view: flavite_workspace::ContextViewKind::Outline,
                group: app.focused_group(),
            });
        }
        "workspace.open_properties" => {
            return app.update(Message::OpenContextView {
                view: flavite_workspace::ContextViewKind::Properties,
                group: app.focused_group(),
            });
        }
        "workspace.open_preview" => {
            return app.update(Message::OpenContextView {
                view: flavite_workspace::ContextViewKind::Preview,
                group: focused_group,
            });
        }
        "workspace.open_backlinks" => {
            return app.update(Message::OpenContextView {
                view: flavite_workspace::ContextViewKind::Backlinks,
                group: focused_group,
            });
        }
        "context.pin" => {
            let tab = app.workspace.focused_tab();
            if let Some(source) = app.contexts.observed(&tab) {
                return app.update(Message::SetContextBinding {
                    tab,
                    binding: flavite_workspace::ContextBinding::Pinned(
                        flavite_workspace::EntityLocator::new(source.locator.clone()),
                    ),
                });
            }
            app.status = "This context tool has no source to pin yet".into();
        }
        "context.follow_document" => {
            let tab = app.workspace.focused_tab();
            return app.update(Message::SetContextBinding {
                tab,
                binding: flavite_workspace::ContextBinding::FollowDocument,
            });
        }
        "link.insert_target" => {
            let tab = app.workspace.focused_tab();
            if let Some(source) = app.contexts.observed(&tab) {
                return app.update(Message::InsertLink {
                    locator: source.locator.clone(),
                });
            }
            app.status = "This context tool has no source to insert yet".into();
        }
        "workspace.save_layout" => app.save_layout_now(),
        "workspace.reset_layout" => app.reset_layout(),
        other => app.status = format!("{other} is not a host command"),
    }
    iced::Task::none()
}

/// Shared availability for shell menus and host dispatch; reducer/core guards
/// still recheck every accepted action.
#[must_use]
pub fn available(app: &App, id: &str) -> bool {
    if app.has_composition() {
        return false;
    }
    let Some(group) = app.focused_group() else {
        return false;
    };
    match id {
        "workspace.find" | "workspace.replace" => {
            app.note_editor_view(app.workspace.focused_tab()).is_some()
        }
        "context.source_preview" => matches!(
            app.workspace
                .tab(&app.workspace.focused_tab())
                .map(|s| s.kind()),
            Some(ViewKind::Preview(_))
        ),
        "workspace.note_mode" => {
            matches!(app.workspace.tab(&app.workspace.focused_tab()).map(|s| s.kind()), Some(ViewKind::Note(n)) if n.load() == flavite_workspace::NoteLoad::Ready)
        }
        "context.pin" => {
            app.contexts
                .observed(&app.workspace.focused_tab())
                .is_some()
                && crate::views::context_header::is_context_tab(app, app.workspace.focused_tab())
        }
        "workspace.open_graph" => app.focused_group().is_some(),
        "graph.freeze" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.fit" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.list" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.filters" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.apply_filters" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.clear_filters" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.undo_filters" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.explore" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.clear_selection" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|view| matches!(view.kind(), ViewKind::Graph(_))),
        "graph.open_selected" => graph_action_tab(app).is_some(),
        "graph.insert_selected" => {
            graph_action_tab(app).is_some() && app.valid_writing_target().is_some()
        }
        "context.follow_document" => {
            crate::views::context_header::is_context_tab(app, app.workspace.focused_tab())
        }
        "link.insert_target" => {
            let tab = app.workspace.focused_tab();
            crate::views::context_header::is_context_tab(app, tab)
                && app.contexts.observed(&tab).is_some()
                && app.valid_writing_target().is_some()
        }
        "workspace.duplicate_tab" => app
            .workspace
            .tab(&app.workspace.focused_tab())
            .is_some_and(|v| v.duplicate().is_some()),
        "workspace.split_right" | "workspace.split_down" => {
            app.layout_fits(&LayoutAction::SplitGroup {
                expected_revision: app.workspace.revision(),
                group,
                arrangement: if id.ends_with("right") {
                    flavite_workspace::Arrangement::SideBySide
                } else {
                    flavite_workspace::Arrangement::Stacked
                },
            })
        }
        "workspace.move_tab_next_group"
        | "workspace.move_tab_previous_group"
        | "workspace.merge_next_group" => app.workspace.groups().count() > 1,
        // ---- W10 ----
        "workspace.navigate_back" => app
            .focused_note_tab()
            .is_some_and(|tab| app.nav.can_go_back(tab)),
        "workspace.navigate_forward" => app
            .focused_note_tab()
            .is_some_and(|tab| app.nav.can_go_forward(tab)),
        "workspace.open_history" | "workspace.rename_note" => app.focused_note_tab().is_some(),
        "workspace.open_in_tab" => app.focused_note_tab().is_some(),
        "workspace.select_all" | "workspace.caret_start" | "workspace.caret_end" => {
            app.note_editor_view(app.workspace.focused_tab()).is_some()
        }
        "workspace.new_note"
        | "workspace.open_note_dialog"
        | "workspace.open_search_view"
        | "workspace.refresh_catalog" => app.flavite.is_some(),
        "help.about" => true,
        _ => true,
    }
}

fn move_tab_to_neighbor(app: &mut App, forward: bool) {
    let Some(focused) = app.focused_group() else {
        return;
    };
    let Some(neighbor) = app.next_group_of(focused, forward) else {
        app.status = "There is no other group to move this tab into".into();
        return;
    };
    let tab = app.workspace.focused_tab();
    let position = app
        .workspace
        .group(&neighbor)
        .map_or(0, |group| group.tabs().len());
    app.dispatch_layout(|revision| LayoutAction::MoveTab {
        expected_revision: revision,
        tab,
        target: flavite_workspace::TabDropTarget::Index {
            group: neighbor,
            index: position,
        },
    });
}

/// Build the note locator for a tab title, used by dialogs in later slices.
#[must_use]
pub fn locator_for(app: &App, tab: TabId) -> Option<NoteLocator> {
    match app.workspace.tab(&tab).map(|session| session.kind()) {
        Some(ViewKind::Note(note)) => Some(note.locator().clone()),
        _ => None,
    }
}

/// The focused tab when it is a graph view holding a selection (the
/// keyboard Enter/Cmd+Enter chord target).
#[must_use]
fn graph_action_tab(app: &App) -> Option<TabId> {
    let tab = app.workspace.focused_tab();
    let session = app.workspace.tab(&tab)?;
    if !matches!(session.kind(), ViewKind::Graph(_)) {
        return None;
    }
    app.graph
        .state(tab)
        .and_then(|state| state.selection)
        .map(|_| tab)
}

/// The focused group the shell routes commands to.
#[must_use]
pub fn focused_group_or_first(app: &App) -> Option<GroupId> {
    app.focused_group().or_else(|| app.workspace.first_group())
}
