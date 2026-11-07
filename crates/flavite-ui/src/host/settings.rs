//! Settings tab host (RFC-0007 §4.4): the state and the single edit
//! pipeline behind [`flavite_workspace::ViewKind::Settings`].
//!
//! The host owns the resolved view of the vault settings document, the raw
//! document with its generation receipt, and the per-tab navigation state
//! (search query, row/category selection). Every terminal edit — from the
//! tab and from the S3-era dialog — runs the same pipeline
//! ([`SettingsHost::apply`]): validate → `doc.set` → CAS write → live
//! apply; a CAS conflict reloads the document and reports, never silently
//! drops (D-11).
//!
//! The tab renders generically from [`SettingRegistry::builtins()`]; there
//! are no hand-coded per-setting widgets except the two kinds whose
//! options are dynamic (`ThemePackage`, `ShortcutMap`), which read the
//! theme store and the host chord catalog respectively.

use crate::app::{App, Message};
use flavite_api::{Flavite, FlaviteError};
use flavite_settings::document::SettingsDoc;
use flavite_settings::error::SettingsCodecError;
use flavite_settings::registry::SettingRegistry;
use flavite_settings::resolve::{RegistryResolve, Resolved};
use flavite_settings::value::SettingValue;
use flavite_workspace::TabId;
use std::collections::BTreeMap;

/// Settings tab messages (RFC-0007 §4.4). Read-path messages never
/// persist; every edit message lands in [`SettingsHost::apply`].
#[derive(Debug, Clone)]
pub enum SettingsMessage {
    /// The search query of one settings tab changed.
    Search {
        /// The tab whose search box was typed into.
        tab: TabId,
        /// The new query text.
        query: String,
    },
    /// A descriptor row was selected (keyboard navigation).
    Select {
        /// The tab the selection belongs to.
        tab: TabId,
        /// The descriptor id of the selected row.
        id: String,
    },
    /// A table-of-contents category was selected.
    CategorySelect {
        /// The tab the selection belongs to.
        tab: TabId,
        /// The category name, or an empty string for "All".
        category: String,
    },
    /// A `Bool` row toggled.
    Toggle {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
        /// The new value.
        value: bool,
    },
    /// An `Int` row committed a parsed value (bounds validated in the
    /// funnel; refusals surface as an inline row error and the status).
    SetInt {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
        /// The parsed value.
        value: i64,
    },
    /// A `Choice` segment pressed.
    Choose {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
        /// The chosen option value.
        choice: String,
    },
    /// A `LineWidth` segment pressed (`Some(columns)` or `None` = full).
    SetLineWidth {
        /// The tab.
        tab: TabId,
        /// The column count, or `None` for the full pane width.
        columns: Option<u16>,
    },
    /// A `StringList` row gained an entry.
    SetListAdd {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
        /// The entry to append.
        value: String,
    },
    /// A `StringList` row lost the entry at `index`.
    SetListRemove {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
        /// The index of the entry to remove.
        index: usize,
    },
    /// A theme package was selected (`None` = built-in palettes). The
    /// selection is a preference value; activation reloads the package
    /// after the write (no race — RFC-0007 D-08).
    ChooseThemePackage {
        /// The tab.
        tab: TabId,
        /// The package id, or `None` for the built-in palettes.
        package: Option<String>,
    },
    /// One row restored to its descriptor default.
    ResetOne {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
    },
    /// Copy the absolute `settings.toml` path to the clipboard (the ⧉
    /// affordance in the search bar).
    RevealPath,
    /// Focus the tab's search input (the `/` keyboard affordance).
    FocusSearch {
        /// The tab.
        tab: TabId,
    },
    /// The window gained focus: check the generation and adopt external
    /// edits (D-11 — focus is the sync point; the file watcher is not used).
    FocusGained,
    /// The document is unparseable and the user chose the last-good
    /// backup restore (FR-10).
    RestoreBackup,
    /// An `Int` row's draft text changed (applied on submit).
    IntDraft {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
        /// The draft text.
        text: String,
    },
    /// An `Int` row's draft was submitted (Enter): parsed and validated in
    /// the funnel; non-numeric or out-of-bounds drafts are refused with a
    /// row error, never partially applied.
    IntSubmit {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
    },
    /// A `StringList` row's draft text changed (applied on submit).
    ListDraft {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
        /// The draft text.
        text: String,
    },
    /// A `StringList` row's draft was submitted (Enter): trimmed, deduped,
    /// and appended through the funnel.
    ListSubmit {
        /// The tab.
        tab: TabId,
        /// The descriptor id.
        id: String,
    },
    /// Set or clear one command's chord override (the per-row Reset uses
    /// `chord: None`); validated in the funnel.
    SetShortcut {
        /// The command id.
        command: String,
        /// The new chord string, or `None` to restore the default.
        chord: Option<String>,
    },
    /// Clear every chord override, restoring the catalog defaults.
    RestoreShortcuts,
    /// Begin shortcut capture: the next key press becomes the command's
    /// chord (the tab's capture row; Escape cancels).
    BeginShortcutCapture {
        /// The command id being captured.
        command: String,
    },
    /// A captured key press, or Escape (`chord: None` cancels).
    ShortcutCaptured {
        /// The command being captured.
        command: String,
        /// The captured chord string, or `None` when cancelled.
        chord: Option<String>,
    },
    /// Open the native file dialog to pick a standard `.xpi` package.
    ThemeInstallPick,
    /// Convert and install the package at this path (the dialog result or
    /// an explicitly typed path).
    ThemeInstallFromPath(std::path::PathBuf),
    /// Ask before removing an installed theme package (the row shows the
    /// confirm/cancel pair until resolved).
    ThemeUninstallAsk {
        /// The tab.
        tab: TabId,
        /// The package id.
        id: String,
    },
    /// Remove the package after the explicit confirmation.
    ThemeUninstallConfirm {
        /// The tab.
        tab: TabId,
    },
    /// Cancel a pending package removal.
    ThemeUninstallCancel {
        /// The tab.
        tab: TabId,
    },
    /// RFC-0006 S04 (D-02): acknowledge the changed graph gesture contract.
    /// Persisted through the funnel (the hidden `state.*` descriptor).
    AcknowledgeGraphActivation,
}

/// The settings host: one per app, loaded at vault attach and refreshed on
/// every edit, preference persist, and focus-gained generation check.
#[derive(Debug, Default)]
pub struct SettingsHost {
    /// The resolved effective values (document overlay on registry
    /// defaults), when the document loaded.
    pub resolved: Option<Resolved>,
    /// The raw document, the base of the CAS edit pipeline.
    pub doc: Option<SettingsDoc>,
    /// The document generation receipt for compare-and-set writes.
    pub generation: u64,
    /// Field-level warnings from the last parse.
    pub warnings: Vec<String>,
    /// Whether edits are refused (schema newer, load failure, or missing
    /// document): the tab renders truth but offers no write path.
    pub read_only: bool,
    /// The load failure, when the document could not be read at all
    /// (FR-10: surfaced as an error card, never silently reset).
    pub load_error: Option<String>,
    /// Whether a last-good backup exists for the FR-10 restore affordance.
    pub backup_available: bool,
    /// Whether the last write conflicted and the document was reloaded
    /// (D-11): the funnel adopts the reloaded truth into live state before
    /// reporting the refused edit.
    pub reloaded: bool,
    /// The inline error of the last refused edit, keyed by descriptor id
    /// (rendered under the row).
    pub row_error: Option<(String, String)>,
    /// While a shortcut row is capturing, the command id waiting for the
    /// next key press. Escape cancels (the keyboard router owns the
    /// interception, the state lives here).
    pub capture: Option<String>,
    /// The theme package awaiting removal confirmation.
    pub pending_uninstall: Option<String>,
    /// Per-tab `Int` row draft texts, keyed by descriptor id.
    int_drafts: BTreeMap<(TabId, String), String>,
    /// Per-tab `StringList` row draft texts, keyed by descriptor id.
    list_drafts: BTreeMap<(TabId, String), String>,
    /// Per-tab search queries.
    queries: BTreeMap<TabId, String>,
    /// Per-tab selected descriptor row.
    selected: BTreeMap<TabId, String>,
    /// Per-tab selected TOC category (`None` = All).
    categories: BTreeMap<TabId, Option<String>>,
}

impl SettingsHost {
    /// Load the settings document from the facade at vault attach. A read
    /// or parse failure is an honest report with defaults rendered; the
    /// document is never overwritten by that fallback (D-11).
    pub fn load(&mut self, flavite: &Flavite) {
        match flavite.read_settings_raw() {
            Ok(Some(stored)) => {
                let registry = SettingRegistry::with_builtins();
                let read_only = stored.is_read_only() || flavite.is_read_only();
                let resolved = registry.resolve(&stored.doc);
                self.resolved = Some(resolved);
                self.doc = Some(stored.doc);
                self.generation = stored.generation;
                self.warnings = stored.warnings;
                self.read_only = read_only;
                self.load_error = None;
                self.row_error = None;
                self.backup_available = false;
                self.reloaded = false;
            }
            // The facade initializes the document at open; a missing one
            // means that initialization failed. Registry defaults render
            // truthfully; writes stay refused until the document exists.
            Ok(None) => {
                let registry = SettingRegistry::with_builtins();
                let doc = flavite_settings::resolve::defaults_document(&registry);
                self.resolved = Some(registry.resolve(&doc));
                self.doc = Some(doc);
                self.generation = 0;
                self.warnings = Vec::new();
                self.read_only = true;
                self.load_error =
                    Some("Settings document missing: initialization did not complete".into());
                self.row_error = None;
                self.backup_available = false;
            }
            Err(error) => {
                let registry = SettingRegistry::with_builtins();
                let doc = flavite_settings::resolve::defaults_document(&registry);
                self.resolved = Some(registry.resolve(&doc));
                self.doc = None;
                self.generation = 0;
                self.warnings = Vec::new();
                self.read_only = true;
                self.load_error = Some(error.to_string());
                self.row_error = None;
                self.backup_available = flavite.settings_backup().is_ok_and(|b| b.is_some());
            }
        }
    }

    /// The tab's search query (empty before the first edit).
    #[must_use]
    pub fn query(&self, tab: TabId) -> &str {
        self.queries.get(&tab).map(String::as_str).unwrap_or("")
    }

    /// Record the tab's search query.
    pub fn set_query(&mut self, tab: TabId, query: String) {
        self.queries.insert(tab, query);
    }

    /// The tab's selected descriptor id, if any.
    #[must_use]
    pub fn selection(&self, tab: TabId) -> Option<&str> {
        self.selected.get(&tab).map(String::as_str)
    }

    /// Select one descriptor row.
    pub fn select(&mut self, tab: TabId, id: String) {
        self.selected.insert(tab, id);
    }

    /// The tab's selected TOC category (`None` = All).
    #[must_use]
    pub fn category(&self, tab: TabId) -> Option<&str> {
        self.categories.get(&tab).and_then(|c| c.as_deref())
    }

    /// Select a TOC category (`None` = All).
    pub fn select_category(&mut self, tab: TabId, category: Option<String>) {
        self.categories.insert(tab, category);
    }

    /// The `Int` row's draft text (the current value before the first edit).
    #[must_use]
    pub fn int_draft(&self, tab: TabId, id: &str) -> String {
        self.int_drafts
            .get(&(tab, id.to_owned()))
            .cloned()
            .unwrap_or_else(|| {
                self.resolved
                    .as_ref()
                    .and_then(|resolved| resolved.get(id))
                    .and_then(|value| value.as_int())
                    .map_or_else(String::new, |int| int.to_string())
            })
    }

    /// Record an `Int` row's draft text.
    pub fn set_int_draft(&mut self, tab: TabId, id: &str, text: String) {
        self.int_drafts.insert((tab, id.to_owned()), text);
    }

    /// The `StringList` row's draft text (empty before the first edit).
    #[must_use]
    pub fn list_draft(&self, tab: TabId, id: &str) -> String {
        self.list_drafts
            .get(&(tab, id.to_owned()))
            .cloned()
            .unwrap_or_default()
    }

    /// Record a `StringList` row's draft text.
    pub fn set_list_draft(&mut self, tab: TabId, id: &str, text: String) {
        self.list_drafts.insert((tab, id.to_owned()), text);
    }

    /// Clear a `StringList` row's draft after a committed entry.
    fn clear_list_draft(&mut self, tab: TabId, id: &str) {
        self.list_drafts.remove(&(tab, id.to_owned()));
    }

    /// The single settings edit pipeline (RFC-0007 §4.4): validate →
    /// `doc.set` → `settings_write(CAS)` → refresh the resolved view.
    ///
    /// Returns whether the `appearance.theme_package` value changed (the
    /// caller reloads the decoded package). Refusals carry a human message
    /// for the status line and the row error slot; a CAS conflict reloads
    /// the document first so the next edit builds on adopted truth (D-11).
    ///
    /// # Errors
    /// Read-only mode, unparseable/missing document, value validation,
    /// CAS conflict, or store failure — each as a status message.
    pub fn apply(
        &mut self,
        flavite: &Flavite,
        id: &str,
        value: SettingValue,
    ) -> Result<bool, String> {
        self.row_error = None;
        if self.read_only {
            return Err("settings_read_only: this vault's settings cannot be changed here".into());
        }
        let Some(doc) = self.doc.clone() else {
            return Err(
                "Settings document unavailable: resolve the load error before editing".into(),
            );
        };
        let registry = SettingRegistry::with_builtins();
        // An unchanged value is a no-op: no CAS churn, no rewritten file.
        if self.resolved.as_ref().and_then(|r| r.get(id)) == Some(&value) {
            return Ok(false);
        }
        let mut doc = doc;
        if let Err(error) = doc.set(id, value, &registry) {
            let message = edit_error(id, &error, &registry);
            self.row_error = Some((id.to_owned(), message.clone()));
            return Err(message);
        }
        let previous_package = self
            .resolved
            .as_ref()
            .and_then(|resolved| resolved.get("appearance.theme_package"))
            .cloned();
        match flavite.settings_write(&doc, self.generation) {
            Ok(generation) => {
                let resolved = registry.resolve(&doc);
                let package_changed =
                    previous_package != resolved.get("appearance.theme_package").cloned();
                self.resolved = Some(resolved);
                self.doc = Some(doc);
                self.generation = generation;
                self.load_error = None;
                Ok(package_changed)
            }
            Err(FlaviteError::Settings(flavite_api::SettingsStoreError::Conflict { .. })) => {
                // Another writer advanced the document between our read
                // and our write: reload and report — the failed edit is
                // named, never silently dropped (D-11, R1).
                self.load(flavite);
                self.reloaded = true;
                Err("Settings changed elsewhere — reloaded; apply your edit again".into())
            }
            Err(error) => Err(error.to_string()),
        }
    }
}

/// The human message for a refused value, from the descriptor shape.
fn edit_error(id: &str, error: &SettingsCodecError, registry: &SettingRegistry) -> String {
    let Some(descriptor) = registry.lookup(id) else {
        return error.to_string();
    };
    match error {
        SettingsCodecError::InvalidValue(_, reason) => {
            // The two bounded editor inputs keep their long-standing copy.
            match (&descriptor.kind, id) {
                (flavite_settings::descriptor::SettingKind::Int { min, max }, "appearance.scale_pct") => {
                    format!("Interface scale must be {min}–{max}%")
                }
                (flavite_settings::descriptor::SettingKind::Int { min, max }, "editor.font_size") => {
                    format!("Editor font size must be {min}–{max}")
                }
                _ => format!("{}: {reason}", descriptor.title),
            }
        }
        _ => error.to_string(),
    }
}

/// Reduce a settings-tab message. Edit messages funnel through
/// [`App::apply_setting`]; read-path messages never persist.
#[allow(clippy::too_many_lines)]
pub fn handle(app: &mut App, message: SettingsMessage) -> iced::Task<Message> {
    match message {
        SettingsMessage::Search { tab, query } => {
            app.settings.set_query(tab, query);
            iced::Task::none()
        }
        SettingsMessage::Select { tab, id } => {
            app.settings.select(tab, id);
            iced::Task::none()
        }
        SettingsMessage::CategorySelect { tab, category } => {
            let category = if category.is_empty() { None } else { Some(category) };
            app.settings.select_category(tab, category);
            iced::Task::none()
        }
        SettingsMessage::Toggle { value, id, .. } => {
            app.apply_setting(&id, SettingValue::Bool(value))
        }
        SettingsMessage::SetInt { value, id, .. } => {
            app.apply_setting(&id, SettingValue::Int(value))
        }
        SettingsMessage::Choose { choice, id, .. } => {
            app.apply_setting(&id, SettingValue::Str(choice))
        }
        SettingsMessage::SetLineWidth { columns, .. } => {
            let value = match columns {
                Some(cols) => SettingValue::Int(i64::from(cols)),
                None => SettingValue::Str("full".into()),
            };
            app.apply_setting("editor.line_width", value)
        }
        SettingsMessage::SetListAdd { value, id, .. } => {
            let mut list = current_list(app, &id);
            if !value.trim().is_empty() && !list.contains(&value) {
                list.push(value);
            }
            app.apply_setting(&id, SettingValue::List(list))
        }
        SettingsMessage::SetListRemove { index, id, .. } => {
            let mut list = current_list(app, &id);
            if index < list.len() {
                list.remove(index);
            }
            app.apply_setting(&id, SettingValue::List(list))
        }
        SettingsMessage::ChooseThemePackage { package, .. } => {
            app.apply_setting(
                "appearance.theme_package",
                SettingValue::Str(package.unwrap_or_default()),
            )
        }
        SettingsMessage::IntDraft { tab, id, text } => {
            app.settings.set_int_draft(tab, &id, text);
            iced::Task::none()
        }
        SettingsMessage::IntSubmit { tab, id } => {
            let draft = app.settings.int_draft(tab, &id);
            match draft.trim().parse::<i64>() {
                Ok(value) => {
                    let task = app.apply_setting(&id, SettingValue::Int(value));
                    if app.settings.row_error.is_none() {
                        app.settings.set_int_draft(tab, &id, value.to_string());
                    }
                    task
                }
                Err(_) => {
                    app.settings.row_error =
                        Some((id.clone(), format!("{draft:?} is not a whole number")));
                    app.status = format!("{id}: {draft:?} is not a whole number");
                    iced::Task::none()
                }
            }
        }
        SettingsMessage::ListDraft { tab, id, text } => {
            app.settings.set_list_draft(tab, &id, text);
            iced::Task::none()
        }
        SettingsMessage::ListSubmit { tab, id } => {
            let draft = app.settings.list_draft(tab, &id).trim().to_owned();
            if draft.is_empty() {
                return iced::Task::none();
            }
            let mut list = current_list(app, &id);
            if list.contains(&draft) {
                app.settings.row_error =
                    Some((id.clone(), format!("{draft:?} is already in the list")));
                return iced::Task::none();
            }
            list.push(draft);
            let task = app.apply_setting(&id, SettingValue::List(list));
            if app.settings.row_error.is_none() {
                app.settings.clear_list_draft(tab, &id);
            }
            task
        }
        SettingsMessage::ResetOne { id, .. } => {
            let registry = SettingRegistry::with_builtins();
            match registry.lookup(&id).map(|descriptor| descriptor.default.clone()) {
                Some(value) => app.apply_setting(&id, value),
                None => iced::Task::none(),
            }
        }
        SettingsMessage::SetShortcut { command, chord } => app.set_shortcut_override(&command, chord),
        SettingsMessage::RestoreShortcuts => app.apply_setting(
            "shortcuts.overrides",
            SettingValue::ShortcutMap(BTreeMap::new()),
        ),
        SettingsMessage::BeginShortcutCapture { command } => {
            app.settings.capture = Some(command.clone());
            app.status = format!("Press the new key combination for {command} (Escape cancels)");
            iced::Task::none()
        }
        SettingsMessage::ShortcutCaptured { command, chord } => {
            app.settings.capture = None;
            match chord {
                Some(chord) => app.set_shortcut_override(&command, Some(chord)),
                None => {
                    app.appearance.shortcut_error = None;
                    iced::Task::none()
                }
            }
        }
        SettingsMessage::ThemeInstallPick => crate::host::dialogs::pick_theme_file(),
        SettingsMessage::ThemeInstallFromPath(path) => app.install_theme_package(path),
        SettingsMessage::ThemeUninstallAsk { id, .. } => {
            app.settings.pending_uninstall = Some(id);
            app.status = "Remove this theme package? Confirm on its row.".into();
            iced::Task::none()
        }
        SettingsMessage::ThemeUninstallConfirm { .. } => {
            let id = app.settings.pending_uninstall.take();
            match id {
                Some(id) => app.uninstall_theme_package(id),
                None => iced::Task::none(),
            }
        }
        SettingsMessage::ThemeUninstallCancel { .. } => {
            app.settings.pending_uninstall = None;
            iced::Task::none()
        }
        SettingsMessage::AcknowledgeGraphActivation => {
            app.apply_setting("state.graph_activation_acknowledged", SettingValue::Bool(true))
        }
        SettingsMessage::FocusSearch { tab } => {
            iced::widget::operation::focus(crate::views::settings::search_input_id(tab))
        }
        SettingsMessage::RevealPath => {
            let path = app.root().join(".flavite").join("settings.toml");
            app.status = format!("Settings file path copied: {}", path.display());
            iced::clipboard::write(path.display().to_string())
        }
        SettingsMessage::FocusGained => adopt_on_focus(app),
        SettingsMessage::RestoreBackup => match app.flavite.as_ref() {
            Some(flavite) => match flavite.settings_restore_backup() {
                Ok(_) => {
                    app.settings.load(flavite);
                    app.appearance.load(flavite);
                    app.appearance.reload_theme_package(flavite);
                    app.status = "Settings restored from the last-good backup".into();
                    iced::Task::none()
                }
                Err(error) => {
                    app.status = format!("Backup restore failed: {error}");
                    iced::Task::none()
                }
            },
            None => {
                app.status = "Attach a vault before restoring settings".into();
                iced::Task::none()
            }
        },
    }
}

/// The current effective list of a `StringList` setting.
fn current_list(app: &App, id: &str) -> Vec<String> {
    app.settings
        .resolved
        .as_ref()
        .and_then(|resolved| resolved.get(id))
        .and_then(|value| value.as_list().cloned())
        .unwrap_or_default()
}

/// The focus-gained sync point (D-11): adopt external edits when the
/// document generation moved. The appearance host re-adopts the resolved
/// values so an externally changed theme or scale repaints.
fn adopt_on_focus(app: &mut App) -> iced::Task<Message> {
    let changed = app.flavite.as_ref().is_some_and(|flavite| {
        match flavite.read_settings_raw() {
            Ok(Some(stored)) => stored.generation != app.settings.generation,
            Ok(None) => false,
            Err(_) => app.settings.load_error.is_none(),
        }
    });
    if changed {
        let flavite = app.flavite.as_ref().expect("checked above");
        app.settings.load(flavite);
        app.appearance.load(flavite);
        app.appearance.reload_theme_package(flavite);
        app.status = "Settings changed elsewhere — reloaded".into();
    }
    iced::Task::none()
}

impl App {
    /// The single settings edit funnel (RFC-0007 S7): CAS-write the vault
    /// document through [`SettingsHost::apply`], then apply the new
    /// resolved values live (D-13) — metrics, chord table, and the theme
    /// package when it changed. Both the Settings tab and the S3-era
    /// dialog land here, so there is exactly one edit pipeline.
    pub fn apply_setting(&mut self, id: &str, value: SettingValue) -> iced::Task<Message> {
        let Some(flavite) = self.flavite.as_ref() else {
            self.status = "Attach a vault before changing settings".into();
            return iced::Task::none();
        };
        match self.settings.apply(flavite, id, value) {
            Ok(package_changed) => {
                if let Some(resolved) = self.settings.resolved.clone() {
                    self.appearance.apply_resolved(resolved);
                }
                if package_changed && let Some(flavite) = self.flavite.as_ref() {
                    self.appearance.reload_theme_package(flavite);
                }
                self.window_geometry_dirty = true;
                self.status = "Settings saved".into();
                iced::Task::none()
            }
            Err(message) => {
                // A conflict reload adopted external truth; the live state
                // follows so the next frame paints the reloaded document.
                if self.settings.reloaded {
                    self.settings.reloaded = false;
                    if let Some(resolved) = self.settings.resolved.clone() {
                        self.appearance.apply_resolved(resolved);
                    }
                }
                self.status = message;
                iced::Task::none()
            }
        }
    }

    /// Set or clear one command's chord override through the funnel,
    /// refusing unreadable chords, collisions with other commands, and
    /// OS-reserved chords with a named owner.
    pub fn set_shortcut_override(
        &mut self,
        command: &str,
        chord: Option<String>,
    ) -> iced::Task<Message> {
        let mut next = current_shortcuts(self);
        match chord {
            Some(text) => {
                next.insert(command.to_owned(), text);
            }
            None => {
                next.remove(command);
            }
        }
        // Collisions are validated against the effective table: an
        // override must not shadow another command's default either.
        let mut effective: BTreeMap<String, String> = crate::host::commands::DEFAULT_CHORDS
            .iter()
            .map(|(id, chord)| ((*id).to_owned(), (*chord).to_owned()))
            .collect();
        let owned_defaults = effective.clone();
        for (id, text) in &next {
            effective.insert(id.clone(), text.clone());
        }
        if let Err(error) =
            flavite_settings::chord::validate_shortcuts(&effective, &owned_defaults)
        {
            let message = error.to_string();
            self.appearance.shortcut_error = Some(message.clone());
            self.status = message;
            return iced::Task::none();
        }
        self.appearance.shortcut_error = None;
        self.apply_setting("shortcuts.overrides", SettingValue::ShortcutMap(next))
    }
}

impl App {
    /// The message that clears one command's chord override (the tab's
    /// per-row Reset button).
    #[must_use]
    pub fn reset_shortcut_message(&self, command: String) -> Message {
        Message::Settings(SettingsMessage::SetShortcut {
            command,
            chord: None,
        })
    }

    /// The message that clears every chord override (the tab's
    /// restore-defaults action).
    #[must_use]
    pub fn restore_shortcuts_message(&self) -> Message {
        Message::Settings(SettingsMessage::RestoreShortcuts)
    }
}

/// The current effective shortcut override map.
fn current_shortcuts(app: &App) -> BTreeMap<String, String> {
    app.settings
        .resolved
        .as_ref()
        .and_then(|resolved| resolved.get("shortcuts.overrides"))
        .and_then(|value| value.as_shortcut_map().cloned())
        .unwrap_or_default()
}
