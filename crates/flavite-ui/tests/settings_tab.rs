//! The Settings tab (RFC-0007 S6): `workspace.preferences` opens the
//! durable tab, every visible descriptor renders with its correct
//! effective value, the hidden descriptor never renders, search narrows
//! the rows, modified markers track edits, and the tab persists as a
//! workspace tab. Read path only — the edit pipeline is S7.

use flavite_fs::Fs;
use flavite_settings::registry::SettingRegistry;
use flavite_ui::app::{App, Message};
use flavite_ui::host::commands;
use flavite_ui::host::settings::SettingsMessage;
use flavite_ui::views::settings;
use flavite_workspace::{TabId, ViewKind};
mod support;
use support::{SeedNote, ui_fixture};

use std::time::Instant;

fn seed() -> Vec<SeedNote<'static>> {
    vec![SeedNote {
        components: &["settings.md"],
        body: "# Settings\n\nUnchanged body.\n",
    }]
}

fn pump_all(app: &mut App) {
    for _ in 0..8 {
        let _ = app.update(Message::Pump);
    }
}

fn open_settings(app: &mut App) -> TabId {
    let _ = app.update(Message::RunHostCommand("workspace.preferences"));
    pump_all(app);
    let tab = app.workspace.focused_tab();
    assert!(
        matches!(
            app.workspace.tab(&tab).map(|session| session.kind()),
            Some(ViewKind::Settings)
        ),
        "the focused tab is the Settings tab"
    );
    tab
}

#[test]
fn workspace_preferences_opens_the_settings_tab() {
    let mut fixture = ui_fixture(2111, "/settings-open", &seed());
    let app = &mut fixture.app;
    open_settings(app);
    // Mod+, is bound to the same command; the chord table entry exists.
    assert!(
        app.appearance
            .shortcuts
            .iter()
            .any(|(_, id)| *id == "workspace.preferences"),
        "workspace.preferences stays bound (Mod+,)"
    );
}

#[test]
fn every_visible_descriptor_renders_and_hidden_ones_do_not() {
    let mut fixture = ui_fixture(2112, "/settings-sweep", &seed());
    let app = &mut fixture.app;
    let tab = open_settings(app);

    let registry = SettingRegistry::with_builtins();
    let expected: Vec<&'static str> = registry
        .visible()
        .iter()
        .map(|descriptor| descriptor.id)
        .collect();
    let rows = settings::rows(app, tab);
    let rendered: Vec<&'static str> = rows.iter().map(|row| row.id).collect();
    assert_eq!(rendered, expected, "registry order is the render order");

    // The hidden descriptor (D-04: `state.*`) never renders as a row.
    assert!(!rendered.contains(&"state.graph_activation_acknowledged"));

    // Values are honest read-only truth: defaults resolve through the doc.
    let font = rows
        .iter()
        .find(|row| row.id == "editor.font_size")
        .expect("font size row");
    assert_eq!(font.value, "14");
    let theme = rows
        .iter()
        .find(|row| row.id == "appearance.theme")
        .expect("theme row");
    assert_eq!(theme.value, "System");
    let shortcuts = rows
        .iter()
        .find(|row| row.id == "shortcuts.overrides")
        .expect("shortcuts row");
    assert_eq!(shortcuts.value, "(no overrides)");
}

#[test]
fn search_narrows_the_rows_to_matches() {
    let mut fixture = ui_fixture(2113, "/settings-search", &seed());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let registry = SettingRegistry::with_builtins();

    let _ = app.update(Message::Settings(SettingsMessage::Search {
        tab,
        query: "font size in pixels".into(),
    }));
    let rows = settings::rows(app, tab);
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec!["editor.font_size"],
        "the distinctive query matches exactly the font-size row"
    );

    let _ = app.update(Message::Settings(SettingsMessage::Search {
        tab,
        query: "font".into(),
    }));
    let fuzzy = settings::rows(app, tab);
    assert!(
        fuzzy.iter().any(|row| row.id == "editor.font_size")
            && fuzzy.len() < registry.visible().len(),
        "the fuzzy query narrows but still finds the font-size row"
    );

    let _ = app.update(Message::Settings(SettingsMessage::Search {
        tab,
        query: "zzz-no-match".into(),
    }));
    assert!(settings::rows(app, tab).is_empty());

    // Clearing the query restores the full list.
    let _ = app.update(Message::Settings(SettingsMessage::Search {
        tab,
        query: String::new(),
    }));
    let registry = SettingRegistry::with_builtins();
    assert_eq!(settings::rows(app, tab).len(), registry.visible().len());
}

#[test]
fn category_selection_narrows_and_all_restores() {
    let mut fixture = ui_fixture(2114, "/settings-toc", &seed());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let _ = app.update(Message::Settings(SettingsMessage::CategorySelect {
        tab,
        category: "Editor".into(),
    }));
    let rows = settings::rows(app, tab);
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec![
            "editor.font_size",
            "editor.line_width",
            "editor.burst_hint_enabled",
        ]
    );
    let _ = app.update(Message::Settings(SettingsMessage::CategorySelect {
        tab,
        category: String::new(),
    }));
    let registry = SettingRegistry::with_builtins();
    assert_eq!(settings::rows(app, tab).len(), registry.visible().len());
}

#[test]
fn modified_markers_track_edits() {
    let mut fixture = ui_fixture(2115, "/settings-modified", &seed());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let before = settings::rows(app, tab);
    assert!(
        before.iter().all(|row| !row.modified),
        "defaults carry no modified markers"
    );

    let _ = app.apply_setting(
        "appearance.scale_pct",
        flavite_settings::value::SettingValue::Int(150),
    );
    let after = settings::rows(app, tab);
    let scale = after
        .iter()
        .find(|row| row.id == "appearance.scale_pct")
        .expect("scale row");
    assert!(scale.modified, "the edited row is marked");
    assert_eq!(scale.value, "150");
    assert!(
        after
            .iter()
            .filter(|row| row.id != "appearance.scale_pct")
            .all(|row| !row.modified),
        "only the edited row is marked"
    );
}

#[test]
fn settings_tab_persists_in_the_layout_file() {
    let mut fixture = ui_fixture(2116, "/settings-layout", &seed());
    let app = &mut fixture.app;
    open_settings(app);
    // The autosave is debounced; the explicit flush is the honest path.
    app.save_layout_now();
    let bytes = fixture
        .fs
        .read_if_exists(std::path::Path::new(
            "/settings-layout/.flavite/workspace-layouts.json",
        ))
        .expect("layout readable")
        .expect("layout persisted");
    let layout: serde_json::Value = serde_json::from_slice(&bytes).expect("layout parses");
    let kinds: Vec<&str> = layout["tabs"]
        .as_array()
        .expect("tabs")
        .iter()
        .filter_map(|tab| tab["kind"]["kind"].as_str())
        .collect();
    assert!(
        kinds.contains(&"settings"),
        "the Settings tab persists: {kinds:?}"
    );
}

/// NFR-4 (RFC-0007): the full-registry settings render fits the iced turn
/// budget (16 ms) in a debug build.
#[test]
fn settings_render_meets_the_turn_budget() {
    let mut fixture = ui_fixture(2117, "/settings-budget", &seed());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let started = Instant::now();
    let _element = settings::view(app, tab);
    let elapsed = started.elapsed();
    assert!(
        elapsed.as_millis() <= 16,
        "settings render took {elapsed:?}, over the 16 ms turn budget"
    );
}

// ---- S7: the edit path (CAS persistence + live apply) ----

use flavite_ui::host::appearance::AppearanceHost;
use flavite_ui::style::Scheme;
use support::reopen_facade;

fn seed_for_reopen() -> Vec<SeedNote<'static>> {
    vec![SeedNote {
        components: &["reopen.md"],
        body: "# Reopen\n\nbody\n",
    }]
}

#[test]
fn scalar_edits_persist_apply_and_survive_reopen() {
    let mut fixture = ui_fixture(2211, "/settings-persist", &seed_for_reopen());
    let app = &mut fixture.app;
    let tab = open_settings(app);

    // An Int edit through the draft pipeline applies live and persists.
    let _ = app.update(Message::Settings(SettingsMessage::IntDraft {
        tab,
        id: "editor.font_size".into(),
        text: "16".into(),
    }));
    let _ = app.update(Message::Settings(SettingsMessage::IntSubmit {
        tab,
        id: "editor.font_size".into(),
    }));
    assert!(
        app.status.starts_with("Settings saved"),
        "status: {}",
        app.status
    );
    assert_eq!(app.appearance.editor_font_size(), 16, "live apply");
    assert_eq!(app.appearance.metrics().editor_font_size, 16.0);

    // Choice, toggle, line-width, and reduced-motion edits.
    let _ = app.update(Message::Settings(SettingsMessage::Choose {
        tab,
        id: "appearance.theme".into(),
        choice: "light".into(),
    }));
    assert_eq!(app.appearance.scheme(), Scheme::Light);
    let _ = app.update(Message::Settings(SettingsMessage::Toggle {
        tab,
        id: "appearance.reduced_motion".into(),
        value: true,
    }));
    assert!(app.appearance.reduced_motion());
    let _ = app.update(Message::Settings(SettingsMessage::SetLineWidth {
        tab,
        columns: None,
    }));
    assert_eq!(app.appearance.line_width_columns(), None);
    let _ = app.update(Message::Settings(SettingsMessage::Toggle {
        tab,
        id: "sidebar.open".into(),
        value: false,
    }));
    assert!(!app.appearance.sidebar_open());

    // The vault document holds every edit.
    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_settings_raw()
        .expect("read")
        .expect("written");
    assert_eq!(
        stored.doc.get("editor.font_size").and_then(|v| v.as_int()),
        Some(16)
    );
    assert_eq!(
        stored
            .doc
            .get("editor.line_width")
            .map(|v| v.as_str().map(str::to_owned)),
        Some(Some("full".to_owned()))
    );

    // A fresh facade over the same vault (restart) restores everything.
    let (reopened, _cache) = reopen_facade(&fixture.fs, "/settings-persist");
    let mut fresh = AppearanceHost::boot();
    fresh.load(&reopened);
    assert_eq!(fresh.editor_font_size(), 16);
    assert_eq!(fresh.scheme(), Scheme::Light);
    assert!(fresh.reduced_motion());
    assert_eq!(fresh.line_width_columns(), None);
    assert!(!fresh.sidebar_open());
}

#[test]
fn cas_conflict_reloads_and_reports_without_silent_drop() {
    let mut fixture = ui_fixture(2212, "/settings-conflict", &seed_for_reopen());
    let app = &mut fixture.app;
    let tab = open_settings(app);

    // An external writer advances the document between the UI's read and
    // the UI's write (the in-process analogue of a second device).
    let _ = app.flavite.as_ref().unwrap().settings_set(
        "editor.font_size",
        flavite_settings::value::SettingValue::Int(18),
    );
    let stale_generation = app.settings.generation;
    assert_ne!(stale_generation, 0);

    let _ = app.update(Message::Settings(SettingsMessage::Toggle {
        tab,
        id: "appearance.reduced_motion".into(),
        value: true,
    }));
    assert!(
        app.status.contains("changed elsewhere"),
        "the conflict is reported: {}",
        app.status
    );
    assert!(
        !app.appearance.reduced_motion(),
        "the failed edit is not silently dropped into state"
    );
    // The reloaded document adopts the external truth.
    assert_eq!(app.appearance.editor_font_size(), 18);
    let _ = app.update(Message::Settings(SettingsMessage::Toggle {
        tab,
        id: "appearance.reduced_motion".into(),
        value: true,
    }));
    assert!(app.appearance.reduced_motion(), "the retried edit lands");
}

#[test]
fn external_edits_adopt_on_focus_gained() {
    let mut fixture = ui_fixture(2213, "/settings-focus", &seed_for_reopen());
    let app = &mut fixture.app;
    open_settings(app);
    let _ = app.flavite.as_ref().unwrap().settings_set(
        "appearance.scale_pct",
        flavite_settings::value::SettingValue::Int(150),
    );
    let _ = app.update(Message::Settings(SettingsMessage::FocusGained));
    assert!(
        app.status.contains("changed elsewhere"),
        "status: {}",
        app.status
    );
    assert_eq!(app.appearance.scale_pct(), 150);
    assert_eq!(app.appearance.metrics().scale, 1.5);
}

#[test]
fn unparseable_document_shows_restore_affordance_and_round_trips() {
    let mut fixture = ui_fixture(2214, "/settings-corrupt", &seed_for_reopen());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    // Two edits: the second write leaves a last-good `.bak`.
    let _ = app.update(Message::Settings(SettingsMessage::Toggle {
        tab,
        id: "appearance.reduced_motion".into(),
        value: true,
    }));
    let _ = app.update(Message::Settings(SettingsMessage::Choose {
        tab,
        id: "appearance.theme".into(),
        choice: "light".into(),
    }));

    let settings_path = std::path::Path::new("/settings-corrupt/.flavite/settings.toml");
    fixture.fs.remove(settings_path).expect("remove");
    fixture
        .fs
        .write_new(settings_path, b"totally [ not toml")
        .expect("torn document");

    app.settings.load(app.flavite.as_ref().unwrap());
    assert!(app.settings.load_error.is_some(), "the failure is named");
    assert!(
        app.settings.backup_available,
        "the last-good backup is offered"
    );
    let _ = app.update(Message::Settings(SettingsMessage::RestoreBackup));
    assert!(app.status.contains("restored"), "status: {}", app.status);
    // The restored document parses and holds the last-good backup state.
    // The store's `.bak` trails by one write (pre-write backup, the same
    // rule as the device store): the restore recovers every write except
    // the very last one before the corruption.
    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_settings_raw()
        .expect("read")
        .expect("restored");
    assert_eq!(
        stored
            .doc
            .get("appearance.reduced_motion")
            .and_then(|v| v.as_bool()),
        Some(true),
        "the backup carried the last good document"
    );
}

#[test]
fn read_only_facade_refuses_edits_and_keeps_the_document() {
    let mut fixture = ui_fixture(2215, "/settings-readonly", &seed_for_reopen());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let before = app
        .flavite
        .as_ref()
        .unwrap()
        .read_settings_raw()
        .expect("read")
        .expect("doc");
    app.flavite.as_mut().unwrap().set_read_only(true);
    app.settings.load(app.flavite.as_ref().unwrap());

    let _ = app.update(Message::Settings(SettingsMessage::Toggle {
        tab,
        id: "appearance.reduced_motion".into(),
        value: true,
    }));
    assert!(
        app.status.starts_with("settings_read_only"),
        "status: {}",
        app.status
    );
    let after = app
        .flavite
        .as_ref()
        .unwrap()
        .read_settings_raw()
        .expect("read")
        .expect("doc");
    assert_eq!(before.generation, after.generation, "document unchanged");
    assert!(!app.appearance.reduced_motion());
}

#[test]
fn bounds_refused_without_changing_state() {
    let mut fixture = ui_fixture(2216, "/settings-bounds", &seed_for_reopen());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let _ = app.update(Message::Settings(SettingsMessage::IntDraft {
        tab,
        id: "editor.font_size".into(),
        text: "9999".into(),
    }));
    let _ = app.update(Message::Settings(SettingsMessage::IntSubmit {
        tab,
        id: "editor.font_size".into(),
    }));
    assert!(
        app.status.contains("Editor font size must be"),
        "status: {}",
        app.status
    );
    assert_eq!(app.appearance.editor_font_size(), 14, "state unchanged");
    let (_, message) = app
        .settings
        .row_error
        .clone()
        .expect("the row shows the inline error");
    assert!(message.contains("must be"), "error: {message}");
    // A non-numeric draft is refused the same way.
    let _ = app.update(Message::Settings(SettingsMessage::IntDraft {
        tab,
        id: "editor.font_size".into(),
        text: "abc".into(),
    }));
    let _ = app.update(Message::Settings(SettingsMessage::IntSubmit {
        tab,
        id: "editor.font_size".into(),
    }));
    assert!(app.settings.row_error.is_some());
    assert_eq!(app.appearance.editor_font_size(), 14);
}

#[test]
fn string_list_edits_round_trip_through_the_funnel() {
    let mut fixture = ui_fixture(2217, "/settings-list", &seed_for_reopen());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let _ = app.update(Message::Settings(SettingsMessage::ListDraft {
        tab,
        id: "vault.ignore_globs".into(),
        text: "*.tmp".into(),
    }));
    let _ = app.update(Message::Settings(SettingsMessage::ListSubmit {
        tab,
        id: "vault.ignore_globs".into(),
    }));
    assert_eq!(
        app.settings
            .resolved
            .as_ref()
            .unwrap()
            .get("vault.ignore_globs")
            .and_then(|v| v.as_list().cloned()),
        Some(vec!["*.tmp".to_owned()])
    );
    // Duplicates are refused with a row error.
    let _ = app.update(Message::Settings(SettingsMessage::ListDraft {
        tab,
        id: "vault.ignore_globs".into(),
        text: "*.tmp".into(),
    }));
    let _ = app.update(Message::Settings(SettingsMessage::ListSubmit {
        tab,
        id: "vault.ignore_globs".into(),
    }));
    assert!(app.settings.row_error.is_some(), "duplicate refused");
    // Removal lands.
    let _ = app.update(Message::Settings(SettingsMessage::SetListRemove {
        tab,
        id: "vault.ignore_globs".into(),
        index: 0,
    }));
    assert_eq!(
        app.settings
            .resolved
            .as_ref()
            .unwrap()
            .get("vault.ignore_globs")
            .and_then(|v| v.as_list().cloned()),
        Some(Vec::new())
    );
}

#[test]
fn reset_one_restores_the_descriptor_default() {
    let mut fixture = ui_fixture(2218, "/settings-reset", &seed_for_reopen());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let _ = app.update(Message::Settings(SettingsMessage::IntDraft {
        tab,
        id: "appearance.scale_pct".into(),
        text: "150".into(),
    }));
    let _ = app.update(Message::Settings(SettingsMessage::IntSubmit {
        tab,
        id: "appearance.scale_pct".into(),
    }));
    let rows = settings::rows(app, tab);
    assert!(
        rows.iter()
            .find(|row| row.id == "appearance.scale_pct")
            .unwrap()
            .modified,
        "edited row is marked"
    );
    let _ = app.update(Message::Settings(SettingsMessage::ResetOne {
        tab,
        id: "appearance.scale_pct".into(),
    }));
    let rows = settings::rows(app, tab);
    let scale = rows
        .iter()
        .find(|row| row.id == "appearance.scale_pct")
        .unwrap();
    assert!(!scale.modified, "the default clears the marker");
    assert_eq!(scale.value, "100");
    assert_eq!(app.appearance.scale_pct(), 100);
}

// ---- S8: shortcut capture + theme management in the tab ----

use iced::keyboard::{Key, Modifiers, key::Named};

/// S8: the capture state machine works from the tab — Begin puts the host
/// in capture, the next key press becomes the chord, Escape cancels, and
/// collisions/OS-reserved chords are refused with the named owner.
#[test]
fn shortcut_capture_flows_through_the_tab() {
    let mut fixture = ui_fixture(2311, "/settings-capture", &seed());
    let app = &mut fixture.app;
    let tab = open_settings(app);

    // Begin capture on a row.
    let _ = app.update(Message::Settings(SettingsMessage::BeginShortcutCapture {
        command: "workspace.find".into(),
    }));
    assert_eq!(app.settings.capture.as_deref(), Some("workspace.find"));
    assert!(
        app.status.contains("Press the new key combination"),
        "status: {}",
        app.status
    );

    // Ctrl+Shift+Q is free. Mod+Shift+F belongs to vault Replace; using
    // that default here would test collision refusal instead of capture.
    let captured = commands::keyboard_message(
        app,
        iced::keyboard::Event::KeyPressed {
            key: Key::Character("q".into()),
            modified_key: Key::Character("q".into()),
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            text: None,
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            repeat: false,
        },
    );
    let message = captured.expect("capture intercepts the key press");
    let _ = app.update(message);
    assert!(app.settings.capture.is_none(), "capture ends");
    assert!(
        app.appearance
            .shortcut_overrides()
            .contains_key("workspace.find"),
        "the captured override lands"
    );
    assert_eq!(
        app.appearance.chord_command(
            &Key::Character("q".into()),
            &(Modifiers::CTRL | Modifiers::SHIFT)
        ),
        Some("workspace.find")
    );

    // Escape cancels a new capture without recording anything.
    let _ = app.update(Message::Settings(SettingsMessage::BeginShortcutCapture {
        command: "workspace.next_group".into(),
    }));
    let cancelled = commands::keyboard_message(
        app,
        iced::keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            modified_key: Key::Named(Named::Escape),
            modifiers: Modifiers::empty(),
            text: None,
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            repeat: false,
        },
    );
    let _ = app.update(cancelled.expect("escape is intercepted during capture"));
    assert!(app.settings.capture.is_none());
    assert!(
        !app.appearance
            .shortcut_overrides()
            .contains_key("workspace.next_group"),
        "a cancelled capture records nothing"
    );

    // A colliding capture is refused with the named conflict.
    let _ = app.update(Message::Settings(SettingsMessage::BeginShortcutCapture {
        command: "workspace.replace".into(),
    }));
    let colliding = commands::keyboard_message(
        app,
        iced::keyboard::Event::KeyPressed {
            key: Key::Character("q".into()),
            modified_key: Key::Character("q".into()),
            modifiers: Modifiers::CTRL | Modifiers::SHIFT,
            text: None,
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            repeat: false,
        },
    );
    let _ = app.update(colliding.expect("capture intercepts"));
    let error = app
        .appearance
        .shortcut_error
        .clone()
        .expect("collision refused");
    assert!(error.contains("already used by"), "error: {error}");
    assert!(
        !app.appearance
            .shortcut_overrides()
            .contains_key("workspace.replace")
    );

    // The per-row Reset clears one override through the funnel.
    let _ = app.update(app.reset_shortcut_message("workspace.find".into()));
    assert!(
        !app.appearance
            .shortcut_overrides()
            .contains_key("workspace.find"),
        "the override is cleared"
    );
    // The tab still renders after the whole flow.
    let _ = settings::view(app, tab);
}

/// S8: theme management in the tab — install from a path persists the
/// selection through the funnel with the package loading; removal asks
/// first; cancelling keeps everything.
#[test]
fn theme_package_management_flows_through_the_tab() {
    let mut fixture = ui_fixture(2312, "/settings-themes", &seed());
    let app = &mut fixture.app;
    let tab = open_settings(app);

    // Install a valid package from an explicit path (the rfd flow's typed
    // result): the install runs through the theme.install command, the
    // selection persists through the funnel, and the package loads.
    let package = support::fixture_theme_package(fixture.cache.path());
    let _ = app.update(Message::Settings(SettingsMessage::ThemeInstallFromPath(
        package,
    )));
    assert!(
        app.appearance.theme_package.is_some(),
        "the installed package loads: {:?}",
        app.appearance.theme_error
    );
    assert!(
        !app.appearance.installed_themes.is_empty(),
        "the installed list refreshes"
    );
    let stored = app
        .flavite
        .as_ref()
        .unwrap()
        .read_settings_raw()
        .expect("read")
        .expect("doc");
    assert_ne!(
        stored
            .doc
            .get("appearance.theme_package")
            .and_then(|v| v.as_str().map(str::to_owned)),
        Some(String::new()),
        "the selection persists in the vault document"
    );

    // Selecting built-in palettes clears the selection through the funnel.
    let _ = app.update(Message::Settings(SettingsMessage::ChooseThemePackage {
        tab,
        package: None,
    }));
    assert!(app.appearance.theme_package.is_none());

    // Removing a package asks first; cancelling keeps it.
    let theme_id = app.appearance.installed_themes[0].id.clone();
    let _ = app.update(Message::Settings(SettingsMessage::ThemeUninstallAsk {
        tab,
        id: theme_id.clone(),
    }));
    assert_eq!(
        app.settings.pending_uninstall.as_deref(),
        Some(theme_id.as_str())
    );
    let _ = app.update(Message::Settings(SettingsMessage::ThemeUninstallCancel {
        tab,
    }));
    assert!(app.settings.pending_uninstall.is_none());
    assert!(
        !app.appearance.installed_themes.is_empty(),
        "cancel keeps the package"
    );

    // Confirming removes the package through the theme.uninstall command.
    let _ = app.update(Message::Settings(SettingsMessage::ThemeUninstallAsk {
        tab,
        id: theme_id.clone(),
    }));
    let _ = app.update(Message::Settings(SettingsMessage::ThemeUninstallConfirm {
        tab,
    }));
    assert!(
        app.appearance.installed_themes.is_empty(),
        "the package is removed: {:?}",
        app.appearance.installed_themes
    );
    let _ = settings::view(app, tab);
}

/// FR-6 (keyboard-only): `/` focuses the tab's search input, and the
/// query typed there narrows the rows without the mouse.
#[test]
fn slash_focuses_the_search_input_for_keyboard_operation() {
    let mut fixture = ui_fixture(2313, "/settings-slash", &seed());
    let app = &mut fixture.app;
    let tab = open_settings(app);
    let message = commands::keyboard_message(
        app,
        iced::keyboard::Event::KeyPressed {
            key: Key::Character("/".into()),
            modified_key: Key::Character("/".into()),
            physical_key: iced::keyboard::key::Physical::Unidentified(
                iced::keyboard::key::NativeCode::Unidentified,
            ),
            location: iced::keyboard::Location::Standard,
            modifiers: Modifiers::empty(),
            text: None,
            repeat: false,
        },
    )
    .expect("`/` is intercepted on the settings view");
    // The message routes (the focus task resolves in a real window; in the
    // headless program the task is inert). Typing the query still narrows.
    let _ = app.update(message);
    let _ = app.update(Message::Settings(SettingsMessage::Search {
        tab,
        query: "font size in pixels".into(),
    }));
    let rows = settings::rows(app, tab);
    assert_eq!(
        rows.iter().map(|row| row.id).collect::<Vec<_>>(),
        vec!["editor.font_size"]
    );
}
