//! The host command catalog: UI workspace commands and their bindings.
//!
//! Every shell entry point — palette, keyboard chord, button, native
//! accessibility action — resolves through [`ui_specs`] and [`dispatch`],
//! so availability and labels cannot drift between surfaces (D-16).

use crate::app::{App, Message};
use flavite_api::{ArgSchema, Chord, CommandSpec};
use flavite_workspace::{GroupId, LayoutAction, NoteLocator, TabId, ViewKind};
use iced::keyboard::{self, Modifiers};
use std::sync::LazyLock;

macro_rules! chords {
    ($($modifiers:expr),+; $key:literal $(,)?) => {
        LazyLock::new(|| [Chord::new($($modifiers)|+, $key)])
    };
}

use flavite_api::command::modifiers::{ALT, CTRL, SHIFT};

static KEYS_QUICK_SWITCH: LazyLock<[Chord; 1]> = chords!(CTRL; "P");
static KEYS_COMMAND_PALETTE: LazyLock<[Chord; 1]> = chords!(CTRL, SHIFT; "P");
static KEYS_NEW_TAB: LazyLock<[Chord; 1]> = chords!(CTRL; "T");
static KEYS_CLOSE_TAB: LazyLock<[Chord; 1]> = chords!(CTRL; "W");
static KEYS_REOPEN_TAB: LazyLock<[Chord; 1]> = chords!(CTRL, SHIFT; "T");
static KEYS_NEXT_TAB: LazyLock<[Chord; 1]> = chords!(CTRL; "Tab");
static KEYS_PREVIOUS_TAB: LazyLock<[Chord; 1]> = chords!(CTRL, SHIFT; "Tab");
static KEYS_NEXT_GROUP: LazyLock<[Chord; 1]> = LazyLock::new(|| [Chord::new(0, "F6")]);
static KEYS_PREVIOUS_GROUP: LazyLock<[Chord; 1]> = chords!(SHIFT; "F6");
static KEYS_SPLIT_RIGHT: LazyLock<[Chord; 1]> = chords!(CTRL, ALT; "Right");
static KEYS_SPLIT_DOWN: LazyLock<[Chord; 1]> = chords!(CTRL, ALT; "Down");
static KEYS_MOVE_TAB_NEXT_GROUP: LazyLock<[Chord; 1]> = chords!(CTRL, ALT, SHIFT; "Right");
static KEYS_MOVE_TAB_PREVIOUS_GROUP: LazyLock<[Chord; 1]> = chords!(CTRL, ALT, SHIFT; "Left");
static KEYS_MAXIMIZE_GROUP: LazyLock<[Chord; 1]> = chords!(CTRL, SHIFT; "M");

/// The `'static` default chord slice for a command id, if any.
fn keys_for(id: &str) -> Option<&'static [Chord]> {
    use flavite_api::command::modifiers as mods;
    Some(match id {
        "workspace.quick_switch" => &LazyLock::force(&KEYS_QUICK_SWITCH)[..],
        "workspace.command_palette" => &LazyLock::force(&KEYS_COMMAND_PALETTE)[..],
        "workspace.new_tab" => &LazyLock::force(&KEYS_NEW_TAB)[..],
        "workspace.close_tab" => &LazyLock::force(&KEYS_CLOSE_TAB)[..],
        "workspace.reopen_tab" => &LazyLock::force(&KEYS_REOPEN_TAB)[..],
        "workspace.next_tab" => &LazyLock::force(&KEYS_NEXT_TAB)[..],
        "workspace.previous_tab" => &LazyLock::force(&KEYS_PREVIOUS_TAB)[..],
        "workspace.next_group" => &LazyLock::force(&KEYS_NEXT_GROUP)[..],
        "workspace.previous_group" => &LazyLock::force(&KEYS_PREVIOUS_GROUP)[..],
        "workspace.split_right" => &LazyLock::force(&KEYS_SPLIT_RIGHT)[..],
        "workspace.split_down" => &LazyLock::force(&KEYS_SPLIT_DOWN)[..],
        "workspace.move_tab_next_group" => &LazyLock::force(&KEYS_MOVE_TAB_NEXT_GROUP)[..],
        "workspace.move_tab_previous_group" => &LazyLock::force(&KEYS_MOVE_TAB_PREVIOUS_GROUP)[..],
        "workspace.maximize_group" => &LazyLock::force(&KEYS_MAXIMIZE_GROUP)[..],
        _ => {
            let _ = mods::NONE;
            return None;
        }
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
        id: "workspace.quick_switch",
        title: "Quick switch: open a note",
    },
    UiCommand {
        id: "workspace.command_palette",
        title: "Command palette",
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
        id: "workspace.close_group",
        title: "Close group",
    },
];

/// The UI portion of the host catalog, rendered as core `CommandSpec`s for
/// the shared palette matcher.
#[must_use]
pub fn ui_specs() -> Vec<CommandSpec> {
    UI_COMMANDS
        .iter()
        .map(|command| {
            CommandSpec::with_availability(
                command.id,
                command.title,
                ArgSchema::typed::<serde_json::Value>(),
                None,
                keys_for(command.id).unwrap_or(&[]),
            )
        })
        .collect()
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
    // Overlay-local navigation first (§13.1: IME/plain typing precedence).
    if app.palette_open || app.picker.open {
        if modifiers.is_empty()
            && let keyboard::Key::Named(named) = &key
        {
            return overlay_navigation(app, *named);
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
    // Layout and shell chords.
    if modifiers.command() {
        return command_chord(app, &key, modifiers);
    }
    if matches!(&key, keyboard::Key::Named(keyboard::key::Named::F6)) {
        return Some(Message::Layout(LayoutAction::CycleGroup {
            expected_revision: app.workspace.revision(),
            forward: !modifiers.shift(),
        }));
    }
    // An overlay hotkey that is also a printable character (P) must not
    // fire while the user types into a text field; overlays are the only
    // text inputs in W02, handled above.
    let _ = text;
    None
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

fn command_chord(app: &App, key: &keyboard::Key, modifiers: Modifiers) -> Option<Message> {
    let layout = |action: LayoutAction| Some(Message::Layout(action));
    let revision = app.workspace.revision();
    match key.as_ref() {
        keyboard::Key::Character(character) => {
            if character.eq_ignore_ascii_case("p") {
                return Some(if modifiers.shift() {
                    Message::PaletteOpen
                } else {
                    Message::PickerOpen
                });
            }
            if character.eq_ignore_ascii_case("t") && modifiers.shift() {
                return layout(LayoutAction::ReopenClosed {
                    expected_revision: revision,
                });
            }
            if character.eq_ignore_ascii_case("t") {
                return Some(Message::RunHostCommand("workspace.new_tab"));
            }
            if character.eq_ignore_ascii_case("w") {
                return Some(Message::RunHostCommand("workspace.close_tab"));
            }
            if character.eq_ignore_ascii_case("m") && modifiers.shift() {
                return Some(Message::RunHostCommand("workspace.maximize_group"));
            }
            if character.eq_ignore_ascii_case("s") {
                return Some(Message::RunCoreCommand("note.save"));
            }
            None
        }
        keyboard::Key::Named(named) => match named {
            keyboard::key::Named::Tab => layout(LayoutAction::CycleTab {
                expected_revision: revision,
                forward: !modifiers.shift(),
            }),
            keyboard::key::Named::ArrowLeft if modifiers.alt() && modifiers.shift() => {
                Some(Message::RunHostCommand("workspace.move_tab_previous_group"))
            }
            keyboard::key::Named::ArrowRight if modifiers.alt() && modifiers.shift() => {
                Some(Message::RunHostCommand("workspace.move_tab_next_group"))
            }
            keyboard::key::Named::ArrowRight if modifiers.alt() => {
                layout(LayoutAction::SplitGroup {
                    expected_revision: revision,
                    group: app.focused_group()?,
                    arrangement: flavite_workspace::Arrangement::SideBySide,
                })
            }
            keyboard::key::Named::ArrowDown if modifiers.alt() => {
                layout(LayoutAction::SplitGroup {
                    expected_revision: revision,
                    group: app.focused_group()?,
                    arrangement: flavite_workspace::Arrangement::Stacked,
                })
            }
            _ => None,
        },
        _ => None,
    }
}

/// Dispatch a host command id against current shell state. Availability
/// failures surface as a status message, never as a silent no-op.
pub fn dispatch(app: &mut App, id: &'static str) -> iced::Task<Message> {
    let focused_group = app.focused_group();
    match id {
        "workspace.quick_switch" => return iced::Task::done(Message::PickerOpen),
        "workspace.command_palette" => return iced::Task::done(Message::PaletteOpen),
        "workspace.new_tab" => {
            if let Some(group) = focused_group {
                app.dispatch_layout(|revision| LayoutAction::OpenEmpty {
                    expected_revision: revision,
                    group,
                });
            }
        }
        "workspace.close_tab" => {
            let tab = app.workspace.focused_tab();
            app.dispatch_layout(|revision| LayoutAction::CloseTab {
                expected_revision: revision,
                tab,
            });
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
            if let Some(group) = focused_group {
                app.dispatch_layout(|revision| LayoutAction::CloseGroup {
                    expected_revision: revision,
                    group,
                });
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
        other => app.status = format!("{other} is not a host command"),
    }
    iced::Task::none()
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

/// The focused group the shell routes commands to.
#[must_use]
pub fn focused_group_or_first(app: &App) -> Option<GroupId> {
    app.focused_group().or_else(|| app.workspace.first_group())
}
