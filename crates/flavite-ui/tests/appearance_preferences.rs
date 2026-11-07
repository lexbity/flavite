//! W11 appearance preferences (RFC-0007 S7/S8): theme/scale/font/line-
//! width/reduced-motion edits through the single settings funnel, vault-
//! document persistence semantics, invalid-value handling, shortcut
//! collision rules, and the guarantee that appearance changes never touch
//! document bytes (RFC-0003 §27).

use flavite_fs::Fs;
use flavite_settings::value::SettingValue;
use flavite_ui::app::{App, Message};
use flavite_ui::host::appearance::AppearanceHost;
use flavite_ui::host::settings::SettingsHost;
use flavite_ui::style::{Scheme, tokens};
mod support;
use support::{SeedNote, ui_fixture};

use std::time::{Duration, Instant};

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

fn set_setting(app: &mut App, id: &str, value: SettingValue) {
    let task = app.apply_setting(id, value);
    let _ = task;
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
    set_setting(app, "appearance.theme", SettingValue::Str("light".into()));
    assert_eq!(app.appearance.scheme(), Scheme::Light);
    assert_eq!(
        app.appearance.theme().palette().background,
        tokens::light_palette().background
    );
    set_setting(app, "appearance.theme", SettingValue::Str("dark".into()));
    assert_eq!(app.appearance.scheme(), Scheme::Dark);
    set_setting(app, "appearance.theme", SettingValue::Str("system".into()));
    assert_eq!(app.appearance.scheme(), Scheme::Dark);

    // The OS reporting a light scheme flips System, and only System.
    app.appearance.system_theme(iced::theme::Mode::Light);
    assert_eq!(app.appearance.scheme(), Scheme::Light);
    set_setting(app, "appearance.theme", SettingValue::Str("dark".into()));
    assert_eq!(
        app.appearance.scheme(),
        Scheme::Dark,
        "explicit dark ignores the OS"
    );
}

#[test]
fn preference_edits_persist_and_reload_through_the_vault_document() {
    let mut fixture = ui_fixture(1112, "/appearance-persist", &seed());
    let app = &mut fixture.app;
    set_setting(app, "appearance.theme", SettingValue::Str("light".into()));
    set_setting(app, "appearance.scale_pct", SettingValue::Int(150));
    set_setting(app, "editor.line_width", SettingValue::Int(80));
    set_setting(app, "appearance.reduced_motion", SettingValue::Bool(true));
    pump_all(app);
    assert!(
        app.appearance.last_error.is_none(),
        "persistence errors surface: {:?}",
        app.appearance.last_error
    );

    // The stored vault document holds the edited values (RFC-0007: the
    // vault is the single settings authority).
    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_settings_raw()
        .expect("read stored settings")
        .expect("settings were written");
    assert_eq!(
        stored
            .doc
            .get("appearance.theme")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some("light".to_owned())
    );
    assert_eq!(
        stored.doc.get("appearance.scale_pct").and_then(|v| v.as_int()),
        Some(150)
    );
    assert_eq!(stored.doc.get("editor.line_width").and_then(|v| v.as_int()), Some(80));
    assert_eq!(
        stored
            .doc
            .get("appearance.reduced_motion")
            .and_then(|v| v.as_bool()),
        Some(true)
    );
    assert!(
        stored.warnings.is_empty(),
        "a valid round trip has no warnings"
    );

    let mut fresh = AppearanceHost::boot();
    fresh.load(app.flavite.as_ref().unwrap());
    assert_eq!(fresh.scheme(), Scheme::Light);
    assert_eq!(fresh.metrics().scale, 1.5);
}

#[test]
fn unreadable_or_future_documents_disable_edits_instead_of_overwriting() {
    let mut fixture = ui_fixture(1113, "/appearance-corrupt", &seed());
    let app = &mut fixture.app;
    // Corrupt the vault settings document directly on the fixture fs, then
    // load it as attach would. The store refuses non-TOML payloads, so the
    // corrupt case is a torn document the codec cannot parse.
    let settings_path = std::path::Path::new("/appearance-corrupt/.flavite/settings.toml");
    fixture.fs.remove(settings_path).expect("seed removed");
    fixture
        .fs
        .write_new(settings_path, b"[1, 2]")
        .expect("corrupt seed written");
    let mut host = SettingsHost::default();
    host.load(app.flavite.as_ref().unwrap());
    assert!(host.read_only, "a corrupt document disables edits");
    assert!(host.load_error.is_some());
    assert_eq!(app.appearance.scheme(), Scheme::Dark, "defaults apply read-only");

    // A future schema is readable but never overwritten: writes are refused
    // so the newer build's data survives.
    fixture.fs.remove(settings_path).expect("seed removed");
    fixture
        .fs
        .write_new(settings_path, b"schema = 99\n")
        .expect("future schema written");
    let flavite = app.flavite.as_ref().unwrap();
    let stored = flavite
        .read_settings_raw()
        .expect("read stored settings")
        .expect("settings present");
    assert!(stored.is_read_only(), "a future schema is read-only");
    let doc = stored.doc.clone();
    assert!(
        flavite.settings_write(&doc, stored.generation).is_err(),
        "writes to a future-schema document are refused"
    );
}

#[test]
fn out_of_range_edit_inputs_are_refused_without_changing_state() {
    let mut fixture = ui_fixture(1114, "/appearance-bounds", &seed());
    let app = &mut fixture.app;
    set_setting(app, "appearance.scale_pct", SettingValue::Int(300));
    assert!(
        app.status.contains("Interface scale must be"),
        "status: {}",
        app.status
    );
    assert_eq!(app.appearance.scale_pct(), 100);
    set_setting(app, "editor.font_size", SettingValue::Int(5));
    assert!(
        app.status.contains("Editor font size must be"),
        "status: {}",
        app.status
    );
    assert_eq!(app.appearance.editor_font_size(), 14);
    // The valid range is accepted (D-15: 75–200%).
    set_setting(app, "appearance.scale_pct", SettingValue::Int(75));
    assert_eq!(app.appearance.metrics().scale, 0.75);
    set_setting(app, "appearance.scale_pct", SettingValue::Int(200));
    assert_eq!(app.appearance.metrics().scale, 2.0);
}

#[test]
fn shortcut_overrides_reject_collisions_and_os_reserved_chords() {
    let mut fixture = ui_fixture(1115, "/appearance-shortcuts", &seed());
    let app = &mut fixture.app;
    // A collision with another command is refused with a named conflict.
    app.set_shortcut_override("workspace.find", Some("Mod+W".into()));
    let error = app
        .appearance
        .shortcut_error
        .clone()
        .expect("collision refused");
    assert!(error.contains("already used by"), "error: {error}");
    assert!(!app.appearance.shortcut_overrides().contains_key("workspace.find"));

    // An OS-reserved chord is refused with its owner so the editor can
    // offer an alternate action.
    app.set_shortcut_override("workspace.find", Some("Alt+F4".into()));
    let error = app
        .appearance
        .shortcut_error
        .clone()
        .expect("OS-reserved refused");
    assert!(error.contains("reserved by the desktop"), "error: {error}");

    // An unreadable chord is refused.
    app.set_shortcut_override("workspace.find", Some("Ctrl+".into()));
    assert!(app.appearance.shortcut_error.is_some());

    // A valid, non-colliding override lands and takes effect.
    app.set_shortcut_override("workspace.find", Some("Mod+Shift+F".into()));
    assert!(app.appearance.shortcut_error.is_none());
    assert!(app.appearance.shortcut_overrides().contains_key("workspace.find"));
    assert!(app.appearance.shortcuts.iter().any(|(chord, id)| {
        *id == "workspace.find" && chord.key == "F" && chord.modifiers & mods::SHIFT != 0
    }));

    // Restoring defaults clears every override.
    app.set_shortcut_override("workspace.note_mode", Some("Mod+R".into()));
    set_setting(
        app,
        "shortcuts.overrides",
        SettingValue::ShortcutMap(std::collections::BTreeMap::new()),
    );
    assert!(app.appearance.shortcut_overrides().is_empty());
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
    app.set_shortcut_override("workspace.find", Some("Mod+Shift+F".into()));
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
    set_setting(app, "appearance.theme", SettingValue::Str("light".into()));
    set_setting(app, "appearance.scale_pct", SettingValue::Int(175));
    set_setting(app, "editor.font_size", SettingValue::Int(20));
    set_setting(app, "editor.line_width", SettingValue::Int(72));
    set_setting(app, "appearance.reduced_motion", SettingValue::Bool(true));
    app.set_shortcut_override("workspace.find", Some("Mod+Shift+F".into()));
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
    set_setting(app, "appearance.scale_pct", SettingValue::Int(200));
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
    assert!(app.sidebar_open(), "the registry default opens the sidebar");
    let _ = app.update(Message::ToggleSidebar);
    assert!(!app.sidebar_open());

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

    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_settings_raw()
        .expect("read stored settings")
        .expect("settings were written");
    assert_eq!(stored.doc.get("sidebar.open").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(stored.doc.get("sidebar.width").and_then(|v| v.as_int()), Some(300));

    let mut fresh = AppearanceHost::boot();
    fresh.load(app.flavite.as_ref().unwrap());
    assert!(!fresh.sidebar_open());
    assert_eq!(fresh.sidebar_width(), 300);
    assert_eq!(fresh.metrics().sidebar_width, 300.0);
}

use iced::keyboard::{Key, Modifiers, key::Named};
use flavite_api::command::modifiers as mods;
