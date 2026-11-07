//! W11 appearance preferences: theme/scale/font/line-width/reduced-motion
//! edits, device-local persistence semantics, invalid-value handling,
//! shortcut collision rules, and the guarantee that appearance changes
//! never touch document bytes (RFC-0003 §27).

use flavite_api::UserStateKind;
use flavite_fs::Fs;
use flavite_ui::app::{App, Message};
use flavite_ui::host::appearance::{AppearanceHost, PreferenceMessage};
use flavite_ui::preferences::{LineWidth, Preferences, ThemeChoice};
use flavite_ui::style::{Scheme, tokens};
mod support;

use iced::keyboard::{Key, Modifiers, key::Named};
use std::time::{Duration, Instant};
use support::{SeedNote, ui_fixture};

use flavite_api::command::modifiers as mods;

const NOTE: &str = "appearance.md";

fn seed() -> Vec<SeedNote> {
    vec![SeedNote {
        components: &[NOTE],
        body: "# Appearance\n\nUnchanged body.\n",
    }]
}

fn pump_all(app: &mut App) {
    for _ in 0..8 {
        let _ = app.update(Message::Pump);
    }
}

fn open_note(app: &mut App) {
    let group = app.focused_group().unwrap();
    let locator = flavite_workspace::NoteLocator::from_components([NOTE]).unwrap();
    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::OpenNote {
        expected_revision: revision,
        locator,
        group,
    });
    wait_ready(app);
}

fn wait_ready(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let _ = app.update(Message::Pump);
        if !app.notes.any_pending() || Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(!app.notes.any_pending(), "every open completed");
}

#[test]
fn theme_choices_switch_the_resolved_scheme() {
    let mut fixture = ui_fixture(1111, "/appearance-theme", &seed());
    let app = &mut fixture.app;
    assert_eq!(
        app.appearance.scheme(),
        Scheme::Dark,
        "System defaults to dark when no OS scheme is reported"
    );
    let _ = app.update_preferences(PreferenceMessage::SetTheme(ThemeChoice::Light));
    assert_eq!(app.appearance.scheme(), Scheme::Light);
    assert_eq!(
        app.appearance.theme().palette().background,
        tokens::light_palette().background
    );
    let _ = app.update_preferences(PreferenceMessage::SetTheme(ThemeChoice::Dark));
    assert_eq!(app.appearance.scheme(), Scheme::Dark);
    let _ = app.update_preferences(PreferenceMessage::SetTheme(ThemeChoice::System));
    assert_eq!(app.appearance.scheme(), Scheme::Dark);

    // The OS reporting a light scheme flips System, and only System.
    app.appearance.system_theme(iced::theme::Mode::Light);
    assert_eq!(app.appearance.scheme(), Scheme::Light);
    let _ = app.update_preferences(PreferenceMessage::SetTheme(ThemeChoice::Dark));
    assert_eq!(
        app.appearance.scheme(),
        Scheme::Dark,
        "explicit dark ignores the OS"
    );
}

#[test]
fn preference_edits_persist_and_reload_through_the_device_store() {
    let mut fixture = ui_fixture(1112, "/appearance-persist", &seed());
    let app = &mut fixture.app;
    let _ = app.update_preferences(PreferenceMessage::SetTheme(ThemeChoice::Light));
    let _ = app.update_preferences(PreferenceMessage::SetScale(150));
    let _ = app.update_preferences(PreferenceMessage::SetLineWidth(LineWidth::Columns(80)));
    let _ = app.update_preferences(PreferenceMessage::SetReducedMotion(true));
    pump_all(app);
    assert!(
        app.appearance.last_error.is_none(),
        "persistence errors surface: {:?}",
        app.appearance.last_error
    );

    // The stored document reloads with identical state (the device store
    // is keyed by vault UUID; cross-process durability of the store itself
    // is covered by flavite-api workspace_storage).
    let stored = fixture
        .app
        .flavite
        .as_ref()
        .unwrap()
        .read_user_state(UserStateKind::DevicePreferences)
        .expect("read stored preferences")
        .expect("preferences were written");
    let reloaded = Preferences::parse(Some(std::str::from_utf8(&stored.payload).unwrap()))
        .expect("stored document parses");
    assert_eq!(reloaded.preferences.theme, ThemeChoice::Light);
    assert_eq!(reloaded.preferences.interface_scale_pct, 150);
    assert_eq!(reloaded.preferences.line_width, LineWidth::Columns(80));
    assert!(reloaded.preferences.reduced_motion);
    assert!(
        reloaded.warnings.is_empty(),
        "a valid round trip has no warnings"
    );

    let mut fresh = AppearanceHost::boot();
    fresh.load(fixture.app.flavite.as_ref().unwrap());
    assert_eq!(fresh.scheme(), Scheme::Light);
    assert_eq!(fresh.metrics().scale, 1.5);
}

#[test]
fn invalid_stored_fields_fall_back_with_warnings_and_unknown_fields_survive() {
    let document = serde_json::json!({
        "schema": 1,
        "theme": "sepia",
        "interface_scale_pct": 300,
        "editor_font_size": 4,
        "line_width": "wide",
        "reduced_motion": "yes",
        "shortcuts": { "workspace.find": "Not A Chord" },
        "future_additive_field": { "nested": true }
    })
    .to_string();
    let parsed = Preferences::parse(Some(&document)).expect("field-level invalids are warnings");
    assert_eq!(parsed.preferences.theme, ThemeChoice::System);
    assert_eq!(parsed.preferences.interface_scale_pct, 100);
    assert_eq!(parsed.preferences.editor_font_size, 16);
    assert_eq!(parsed.preferences.line_width, LineWidth::Full);
    assert!(!parsed.preferences.reduced_motion);
    assert!(parsed.preferences.shortcuts.is_empty());
    assert_eq!(parsed.warnings.len(), 6, "every rejected field is reported");
    // Unknown additive fields are preserved for forward compatibility.
    let reencoded = parsed.preferences.to_json();
    assert_eq!(
        reencoded["future_additive_field"],
        serde_json::json!({ "nested": true })
    );
    // A valid document round-trips.
    let mut prefs = Preferences::defaults();
    prefs.theme = ThemeChoice::Light;
    let round = Preferences::parse(Some(&prefs.to_stored()))
        .unwrap()
        .preferences;
    assert_eq!(round, prefs);
}

#[test]
fn unreadable_or_future_documents_disable_saving_instead_of_overwriting() {
    let mut fixture = ui_fixture(1113, "/appearance-corrupt", &seed());
    let app = &mut fixture.app;
    // Write a corrupt document directly, then load it as attach would.
    let flavite = app.flavite.as_mut().unwrap();
    // The store refuses non-JSON payloads, so the corrupt case is a valid
    // JSON document with the wrong shape (not a preferences object).
    flavite
        .write_user_state(UserStateKind::DevicePreferences, 0, b"[1, 2]")
        .expect("corrupt seed written");
    let mut host = AppearanceHost::boot();
    host.load(app.flavite.as_ref().unwrap());
    assert!(!host.valid, "a corrupt document disables saving");
    assert!(host.last_error.is_some());
    assert_eq!(host.scheme(), Scheme::Dark, "defaults apply read-only");

    let future = serde_json::json!({ "schema": 99 }).to_string();
    assert!(
        Preferences::parse(Some(&future)).is_err(),
        "future schemas are refused, not guessed"
    );
}

#[test]
fn out_of_range_edit_inputs_are_refused_without_changing_state() {
    let mut fixture = ui_fixture(1114, "/appearance-bounds", &seed());
    let app = &mut fixture.app;
    let _ = app.update_preferences(PreferenceMessage::SetScale(300));
    assert!(
        app.status.contains("Interface scale must be"),
        "status: {}",
        app.status
    );
    assert_eq!(app.appearance.prefs.interface_scale_pct, 100);
    let _ = app.update_preferences(PreferenceMessage::SetEditorFontSize(5));
    assert!(
        app.status.contains("Editor font size must be"),
        "status: {}",
        app.status
    );
    assert_eq!(app.appearance.prefs.editor_font_size, 16);
    // The valid range is accepted (D-15: 75–200%).
    let _ = app.update_preferences(PreferenceMessage::SetScale(75));
    assert_eq!(app.appearance.metrics().scale, 0.75);
    let _ = app.update_preferences(PreferenceMessage::SetScale(200));
    assert_eq!(app.appearance.metrics().scale, 2.0);
}

#[test]
fn shortcut_overrides_reject_collisions_and_os_reserved_chords() {
    let mut fixture = ui_fixture(1115, "/appearance-shortcuts", &seed());
    let app = &mut fixture.app;
    // A collision with another command is refused with a named conflict.
    let _ = app.update_preferences(PreferenceMessage::SetShortcut {
        command: "workspace.find".into(),
        chord: Some("Mod+W".into()),
    });
    let error = app
        .appearance
        .shortcut_error
        .clone()
        .expect("collision refused");
    assert!(error.contains("already used by"), "error: {error}");
    assert!(
        !app.appearance
            .prefs
            .shortcuts
            .contains_key("workspace.find")
    );

    // An OS-reserved chord is refused with its owner so the editor can
    // offer an alternate action.
    let _ = app.update_preferences(PreferenceMessage::SetShortcut {
        command: "workspace.find".into(),
        chord: Some("Alt+F4".into()),
    });
    let error = app
        .appearance
        .shortcut_error
        .clone()
        .expect("OS-reserved refused");
    assert!(error.contains("reserved by the desktop"), "error: {error}");

    // An unreadable chord is refused.
    let _ = app.update_preferences(PreferenceMessage::SetShortcut {
        command: "workspace.find".into(),
        chord: Some("Ctrl+".into()),
    });
    assert!(app.appearance.shortcut_error.is_some());

    // A valid, non-colliding override lands and takes effect.
    let _ = app.update_preferences(PreferenceMessage::SetShortcut {
        command: "workspace.find".into(),
        chord: Some("Mod+Shift+F".into()),
    });
    assert!(app.appearance.shortcut_error.is_none());
    assert!(
        app.appearance
            .prefs
            .shortcuts
            .contains_key("workspace.find")
    );
    assert!(app.appearance.shortcuts.iter().any(|(chord, id)| {
        *id == "workspace.find" && chord.key == "F" && chord.modifiers & mods::SHIFT != 0
    }));

    // Restoring defaults clears every override.
    let _ = app.update_preferences(PreferenceMessage::SetShortcut {
        command: "workspace.note_mode".into(),
        chord: Some("Mod+R".into()),
    });
    let _ = app.update_preferences(PreferenceMessage::RestoreShortcuts);
    assert!(app.appearance.prefs.shortcuts.is_empty());
    // Defaults are still bound.
    assert!(
        app.appearance
            .shortcuts
            .iter()
            .any(|(chord, id)| *id == "workspace.find" && chord.modifiers & mods::SHIFT == 0)
    );
}

#[test]
fn keyboard_resolution_uses_the_effective_table_exactly() {
    let mut fixture = ui_fixture(1116, "/appearance-keys", &seed());
    let app = &mut fixture.app;
    // Default: Mod+F resolves find; Mod+Shift+F does not (exact match).
    let key_f = Key::Character("f".into());
    assert_eq!(
        app.appearance.chord_command(&key_f, &Modifiers::CTRL),
        Some("workspace.find")
    );
    assert_eq!(
        app.appearance
            .chord_command(&key_f, &(Modifiers::CTRL | Modifiers::SHIFT)),
        None
    );
    // F6 cycles groups without any modifier.
    assert_eq!(
        app.appearance
            .chord_command(&Key::Named(Named::F6), &Modifiers::empty()),
        Some("workspace.next_group")
    );
    // An override changes the effective binding immediately.
    let _ = app.update_preferences(PreferenceMessage::SetShortcut {
        command: "workspace.find".into(),
        chord: Some("Mod+Shift+F".into()),
    });
    assert_eq!(app.appearance.chord_command(&key_f, &Modifiers::CTRL), None);
    assert_eq!(
        app.appearance
            .chord_command(&key_f, &(Modifiers::CTRL | Modifiers::SHIFT)),
        Some("workspace.find")
    );
}

#[test]
fn appearance_edits_never_change_document_bytes() {
    let mut fixture = ui_fixture(1117, "/appearance-bytes", &seed());
    let app = &mut fixture.app;
    open_note(app);
    let _ = app.update_preferences(PreferenceMessage::SetTheme(ThemeChoice::Light));
    let _ = app.update_preferences(PreferenceMessage::SetScale(175));
    let _ = app.update_preferences(PreferenceMessage::SetEditorFontSize(20));
    let _ = app.update_preferences(PreferenceMessage::SetLineWidth(LineWidth::Columns(72)));
    let _ = app.update_preferences(PreferenceMessage::SetReducedMotion(true));
    let _ = app.update_preferences(PreferenceMessage::SetShortcut {
        command: "workspace.find".into(),
        chord: Some("Mod+Shift+F".into()),
    });
    pump_all(app);
    let bytes = fixture
        .fs
        .read_if_exists(std::path::Path::new("/appearance-bytes/appearance.md"))
        .expect("note file exists")
        .expect("note file readable");
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        "# Appearance\n\nUnchanged body.\n"
    );
}

#[test]
fn scale_changes_reproject_geometry_without_rewriting_ratios() {
    let mut fixture = ui_fixture(1118, "/appearance-geometry", &seed());
    let app = &mut fixture.app;
    open_note(app);
    // Split for a two-group tree through the host catalog.
    let _ = app.update(Message::RunHostCommand("workspace.split_right"));
    pump_all(app);
    let before = format!("{:?}", app.workspace.tree());
    let _ = app.update_preferences(PreferenceMessage::SetScale(200));
    pump_all(app);
    assert_eq!(app.appearance.metrics().scale, 2.0);
    assert_eq!(
        format!("{:?}", app.workspace.tree()),
        before,
        "a scale change never rewrites layout"
    );
    // A later pump reprojects at the new scale without changing state.
    pump_all(app);
    assert_eq!(format!("{:?}", app.workspace.tree()), before);
}

#[test]
fn sidebar_presentation_persists_and_survives_a_fresh_host() {
    let mut fixture = ui_fixture(1119, "/appearance-sidebar", &seed());
    let app = &mut fixture.app;
    assert!(!app.sidebar_open());
    let _ = app.update(Message::ToggleSidebar);
    assert!(app.sidebar_open());

    // The divider drag previews live and commits the width on release.
    let _ = app.update(Message::SidebarResizeBegin);
    let _ = app.update(Message::Mouse(iced::mouse::Event::CursorMoved {
        position: iced::Point::new(300.0, 40.0),
    }));
    assert_eq!(
        app.sidebar_width(),
        300.0,
        "the drag preview follows the pointer"
    );
    let _ = app.update(Message::Mouse(iced::mouse::Event::ButtonReleased(
        iced::mouse::Button::Left,
    )));
    assert_eq!(app.sidebar_width(), 300.0);
    assert!(!app.resizing_sidebar);
    pump_all(app);
    assert!(
        app.appearance.last_error.is_none(),
        "persistence errors surface: {:?}",
        app.appearance.last_error
    );

    let stored = fixture
        .app
        .flavite
        .as_ref()
        .unwrap()
        .read_user_state(UserStateKind::DevicePreferences)
        .expect("read stored preferences")
        .expect("preferences were written");
    let reloaded = Preferences::parse(Some(std::str::from_utf8(&stored.payload).unwrap()))
        .expect("stored document parses");
    assert!(reloaded.preferences.sidebar_open);
    assert_eq!(reloaded.preferences.sidebar_width, 300);

    let mut fresh = AppearanceHost::boot();
    fresh.load(fixture.app.flavite.as_ref().unwrap());
    assert!(fresh.prefs.sidebar_open);
    assert_eq!(fresh.prefs.sidebar_width, 300);
    assert_eq!(fresh.metrics().sidebar_width, 300.0);
}
