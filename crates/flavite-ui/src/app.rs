//! The iced application host for the Flavite workspace (RFC-0003 W02).
//!
//! `App` owns the authoritative [`WorkspaceState`], the toolkit adapter,
//! and the host registries. Widget events become host messages; layout
//! truth changes only through the flavite-workspace reducer. Shell
//! rendering lives in [`crate::workspace_shell`].

use crate::host::commands as host_commands;
use crate::host::effects::NoteRegistry;
use crate::operation::OperationPump;
use crate::palette::matching_specs;
use crate::settings::Settings;
use crate::subscription;
use crate::workspace::{Adapter, StripMenu, gestures};
use flavite_api::queries::{MAX_NOTE_CATALOG_LIMIT, NoteCatalogEntry};
use flavite_api::{ExecutionResult, Flavite, Invocation, LocalResult, Origin, VaultConfig};
use flavite_editor::{EditorAction, EditorInteractionSnapshot, EditorView};
use flavite_workspace::{
    Effect, GroupId, Ids, LayoutAction, NoteLocator, Outcome, SplitId, TabId, WorkspaceState,
};
use iced::{Subscription, Task, keyboard};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

/// The note picker overlay state.
#[derive(Debug, Clone, Default)]
pub struct Picker {
    /// Whether the overlay is open.
    pub open: bool,
    /// Query text.
    pub query: String,
    /// Selected row.
    pub selected: usize,
}

/// Lossless notice retained until the user dismisses its visible entry.
#[derive(Debug, Clone)]
pub struct NoticePresentation {
    /// Stable core acknowledgement token.
    pub id: flavite_api::NoticeId,
    /// Typed notice event.
    pub event: flavite_api::Event,
}

/// Run-ready application state. `Flavite` owns the only Session and buffer
/// authority; the workspace model owns layout truth.
pub struct App {
    pub flavite: Option<Flavite>,
    root: PathBuf,
    pub workspace: WorkspaceState,
    pub adapter: Adapter,
    pub notes: NoteRegistry,
    pub picker: Picker,
    pub palette_open: bool,
    pub palette_query: String,
    pub palette_selected: usize,
    pub strip_menu: Option<(GroupId, StripMenu)>,
    pub gesture: gestures::TabGesture,
    pub resize_preview: BTreeMap<SplitId, u32>,
    pub resizing_split: Option<SplitId>,
    catalog: Vec<NoteCatalogEntry>,
    catalog_error: Option<String>,
    settings: Settings,
    settings_valid: bool,
    operations: OperationPump,
    pub status: String,
    pub recent_events: Vec<String>,
    /// Visible lossless notices awaiting explicit presentation acknowledgement.
    pub notices: VecDeque<NoticePresentation>,
    reconcile_progress: Option<(u32, u32)>,
    window_size: iced::Size,
    /// Accessibility node routing map, rebuilt after every commit.
    pub a11y_nodes: BTreeMap<accesskit::NodeId, crate::accessibility::NodeTarget>,
}

/// Host messages. Widget events surface here and reduce through the
/// workspace model or the host catalog.
#[derive(Debug, Clone)]
pub enum Message {
    Keyboard(keyboard::Event),
    Mouse(iced::mouse::Event),
    Pump,
    WindowEvent(iced::window::Event),
    /// A typed layout transaction.
    Layout(flavite_workspace::LayoutAction),
    /// Open a real view kind in the focused group.
    NewViewInFocused(flavite_workspace::ViewKind),
    /// Open (or focus) a note, optionally in a specific group.
    OpenNote {
        locator: NoteLocator,
        group: Option<GroupId>,
    },
    /// Toggle a group strip's inline menu row.
    GroupMenu(GroupId, StripMenu),
    /// A tab was pressed (activation + drag candidate).
    TabPressed(TabId),
    /// A tab strip row is hovered during a drag.
    TabHover {
        group: GroupId,
        index: usize,
    },
    /// A group body is hovered during a drag (append target).
    BodyHover(GroupId),
    /// pane_grid drag events (whole-group moves and merges).
    PaneDrag(iced::widget::pane_grid::DragEvent),
    /// pane_grid resize previews (committed on release).
    PaneResize(iced::widget::pane_grid::ResizeEvent),
    /// A pane was clicked (focus that group).
    PaneClicked(iced::widget::pane_grid::Pane),
    PaletteOpen,
    PaletteClose,
    PaletteQuery(String),
    PaletteNext,
    PalettePrevious,
    PaletteSubmit,
    /// A palette result row was clicked.
    PalettePick(usize),
    PickerOpen,
    PickerClose,
    PickerQuery(String),
    PickerNext,
    PickerPrevious,
    PickerSubmit,
    /// A picker result row was clicked.
    PickerPick(usize),
    /// Dispatch a host (UI) command by catalog id.
    RunHostCommand(&'static str),
    /// Dispatch a core command through the API for the focused note tab.
    RunCoreCommand(&'static str),
    /// Reload the note catalog snapshot.
    RefreshCatalog,
    /// Dismiss a notice after its entry has been presented in the shell.
    AcknowledgeNotice(flavite_api::NoticeId),
    /// Apply a typed editor action from one exact view incarnation.
    EditorAction {
        tab: TabId,
        incarnation: u64,
        action: EditorAction,
    },
}

impl App {
    /// Open the configured vault and construct the shell.
    #[must_use]
    pub fn boot() -> Self {
        let root = crate::host::env::vault_root_from_env()
            .unwrap_or_else(crate::host::env::default_vault_root);
        Self::boot_from_root(root)
    }

    /// Construct the shell against an explicit vault root.
    #[must_use]
    pub fn boot_from_root(root: PathBuf) -> Self {
        let mut app = Self::empty_at(root.clone());
        match Flavite::open_root(root, VaultConfig::default()) {
            Ok(flavite) => app.attach_vault(flavite),
            Err(error) => app.status = format!("Vault unavailable: {error}"),
        }
        app
    }

    /// Build the shell around an already-authorized facade. Fixture seam
    /// for integration tests.
    #[must_use]
    pub fn from_flavite(flavite: Flavite) -> Self {
        let root = flavite.vault_ref().root.clone();
        let mut app = Self::empty_at(root);
        app.attach_vault(flavite);
        app
    }

    fn empty_at(root: PathBuf) -> Self {
        let mut ids = Ids::new();
        let workspace = WorkspaceState::with_ids(&mut ids);
        let adapter = Adapter::build(&workspace);
        let status = format!("Opening {}…", root.display());
        Self {
            flavite: None,
            root,
            adapter,
            notes: NoteRegistry::default(),
            picker: Picker::default(),
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            strip_menu: None,
            gesture: gestures::TabGesture::default(),
            resize_preview: BTreeMap::new(),
            resizing_split: None,
            catalog: Vec::new(),
            catalog_error: None,
            settings: Settings::default(),
            settings_valid: true,
            operations: OperationPump::default(),
            status,
            recent_events: Vec::new(),
            notices: VecDeque::new(),
            reconcile_progress: None,
            window_size: iced::Size::new(1180.0, 760.0),
            a11y_nodes: BTreeMap::new(),
            workspace,
        }
    }

    fn attach_vault(&mut self, flavite: Flavite) {
        self.root = flavite.vault_ref().root.clone();
        self.settings = match flavite.read_settings() {
            Ok(contents) => Settings::parse(contents.as_deref()).unwrap_or_else(|error| {
                self.settings_valid = false;
                self.status = format!("Settings could not be parsed; saving is disabled: {error}");
                Settings::default()
            }),
            Err(error) => {
                self.settings_valid = false;
                self.status = format!("Settings could not be read: {error}");
                Settings::default()
            }
        };
        // D-16: the composed catalog is checked once at attach; UI ids are
        // namespaced, so any overlap is a build defect worth surfacing.
        let core_ids: Vec<&str> = flavite.command_registry().command_ids().to_vec();
        let conflicts = host_commands::catalog_conflicts(&core_ids);
        if !conflicts.is_empty() {
            self.status = format!("Command catalog conflict: {}", conflicts.join(", "));
        }
        self.flavite = Some(flavite);
        if self.settings_valid && conflicts.is_empty() {
            self.status = "Vault ready".into();
        }
        self.load_catalog();
        self.refresh_semantics();
    }

    /// Load a bounded note catalog snapshot; failures stay visible.
    fn load_catalog(&mut self) {
        let Some(flavite) = &self.flavite else {
            return;
        };
        match flavite.note_catalog(MAX_NOTE_CATALOG_LIMIT) {
            Ok(catalog) => {
                self.catalog = catalog.entries;
                self.catalog_error = None;
            }
            Err(error) => {
                self.catalog_error = Some(error.to_string());
            }
        }
    }

    /// The note catalog snapshot for views and the picker.
    #[must_use]
    pub fn catalog(&self) -> &[NoteCatalogEntry] {
        &self.catalog
    }

    /// The group displaying a tab, if any.
    #[must_use]
    pub fn group_of(&self, tab: TabId) -> Option<GroupId> {
        self.workspace.group_of(tab)
    }

    /// The palette candidate snapshot (host + core commands).
    #[must_use]
    pub fn palette_snapshot(&self) -> Vec<flavite_api::CommandSpec> {
        self.palette_matches()
    }

    /// The selected palette row clamped to the candidate list.
    #[must_use]
    pub fn palette_selection(&self, candidate_count: usize) -> usize {
        self.palette_selected.min(candidate_count.saturating_sub(1))
    }

    /// The current palette query text.
    #[must_use]
    pub fn palette_query_text(&self) -> &str {
        &self.palette_query
    }

    /// Current source text of a note tab, if its buffer is installed.
    #[must_use]
    pub fn note_state_text(&self, tab: TabId) -> Option<String> {
        let buffer = self.notes.state(&tab)?.buffer?;
        self.note_text(buffer)
    }

    /// Shared immutable editor snapshot for a note tab.
    #[must_use]
    pub fn note_editor_view(&self, tab: TabId) -> Option<EditorView> {
        let buffer = self.notes.state(&tab)?.buffer?;
        let view = self
            .flavite
            .as_ref()?
            .get_shared_buffer_view(flavite_buffer::BufferId::from(buffer), None)
            .ok()?;
        Some(EditorView::from(&view))
    }

    /// Host-acknowledged input to restore into a new widget runtime.
    #[must_use]
    pub fn editor_interaction_snapshot(&self, tab: TabId) -> Option<EditorInteractionSnapshot> {
        let view = self.note_editor_view(tab)?;
        let logical = self.workspace.tab(&tab)?.interaction();
        let selection = logical.selections.first().copied().unwrap_or_default();
        Some(EditorInteractionSnapshot {
            sequence: logical.sequence,
            buffer_id: view.buffer_id,
            revision: view.revision,
            caret: selection.head,
            selection: (selection.anchor, selection.head),
            viewport_anchor: logical.viewport_anchor,
            focused: logical.focused,
            composing: logical.composition == flavite_workspace::CompositionState::Active,
        })
    }

    /// The buffer state chip label for a note tab ("Clean"/"Dirty"/…).
    #[must_use]
    pub fn note_buffer_state(&self, tab: TabId) -> Option<String> {
        let buffer = self.notes.state(&tab)?.buffer?;
        let flavite = self.flavite.as_ref()?;
        Some(
            flavite
                .get_buffer_view(flavite_buffer::BufferId::from(buffer), None)
                .ok()?
                .state
                .to_string(),
        )
    }

    /// Visible, identity-safe description of the insertion authority.
    #[must_use]
    pub fn writing_target_label(&self) -> String {
        let Some(target) = self.workspace.writing_target() else {
            return "Writing target: none — focus an editor caret".into();
        };
        let Some(view) = self.workspace.tab(&target.tab) else {
            return "Writing target: invalid — editor closed".into();
        };
        let current = self.note_editor_view(target.tab);
        if view.interaction().incarnation != target.incarnation
            || current.is_none_or(|snapshot| {
                snapshot.buffer_id != target.buffer_id || snapshot.revision != target.revision
            })
        {
            return "Writing target: invalid — place the caret again".into();
        }
        let title = self
            .notes
            .state(&target.tab)
            .map(|state| state.locator.title())
            .unwrap_or("note");
        format!("Writing target: {title} at byte {}", target.caret_text_byte)
    }

    /// Pending operation count.
    #[must_use]
    pub fn pending_operations(&self) -> usize {
        self.operations.pending().count()
    }

    /// Host-side count of terminal outcomes displaced by capacity pressure.
    #[must_use]
    pub fn operation_history_overflowed(&self) -> u64 {
        self.operations.terminal_overflowed()
    }

    /// Current reconcile progress.
    #[must_use]
    pub fn reconcile_progress(&self) -> Option<(u32, u32)> {
        self.reconcile_progress
    }

    /// The status line text.
    #[must_use]
    pub fn status_text(&self) -> &str {
        &self.status
    }

    /// The catalog's last load error, if any.
    #[must_use]
    pub fn catalog_error(&self) -> Option<&str> {
        self.catalog_error.as_deref()
    }

    /// The vault root (locators join against it).
    #[must_use]
    pub fn root(&self) -> &PathBuf {
        &self.root
    }

    /// Current logical window size.
    #[must_use]
    pub const fn window_size(&self) -> iced::Size {
        self.window_size
    }

    /// The group owning the focused tab.
    #[must_use]
    pub fn focused_group(&self) -> Option<GroupId> {
        self.workspace.focused_group()
    }

    /// The manually maximized group.
    #[must_use]
    pub fn maximized_group(&self) -> Option<GroupId> {
        self.workspace.maximized_group()
    }

    /// The next or previous group in stable traversal order.
    #[must_use]
    pub fn next_group_of(&self, from: GroupId, forward: bool) -> Option<GroupId> {
        let order = self.workspace.group_order();
        let position = order.iter().position(|group| *group == from)?;
        let next = if forward {
            (position + 1) % order.len()
        } else {
            (position + order.len() - 1) % order.len()
        };
        order.get(next).copied()
    }

    /// The rendered text of a ready note tab, for the semantic tree.
    #[must_use]
    pub fn note_text(&self, buffer: u64) -> Option<String> {
        let flavite = self.flavite.as_ref()?;
        Some(
            flavite
                .get_shared_buffer_view(flavite_buffer::BufferId::from(buffer), None)
                .ok()?
                .text
                .to_string(),
        )
    }

    /// Commit one reducer outcome, reconciling the adapter and effects.
    fn commit(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Committed(commit) => {
                self.workspace = commit.state;
                self.adapter.reconcile(&self.workspace);
                let effects = commit.effects;
                self.apply_effects(&effects);
                self.refresh_semantics();
            }
            Outcome::Rejected(error) => {
                self.status = format!("Workspace: {error}");
            }
        }
    }

    /// Build and reduce one layout transaction at the current revision.
    pub fn dispatch_layout(&mut self, build: impl FnOnce(u64) -> LayoutAction) {
        if self.has_composing_view() {
            self.status = "Finish or cancel text composition before changing the layout".into();
            return;
        }
        let revision = self.workspace.revision();
        let outcome = flavite_workspace::reduce(&self.workspace, build(revision));
        self.commit(outcome);
    }

    fn has_composing_view(&self) -> bool {
        self.workspace.tabs().any(|(_, view)| {
            view.interaction().composition == flavite_workspace::CompositionState::Active
        })
    }

    fn apply_effects(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::ViewAttached { tab, kind } => {
                    if let flavite_workspace::ViewKind::Note(note) = kind {
                        let (tab, locator) = match self.notes.attach(*tab, note.locator().clone()) {
                            Some(attached) => attached,
                            None => continue,
                        };
                        let incarnation = self.notes.incarnation(tab).expect("attached note tab");
                        if let Err(error) =
                            self.workspace.install_view_incarnation(tab, incarnation)
                        {
                            self.status = format!("Could not install editor incarnation: {error}");
                            self.notes.detach(tab);
                            continue;
                        }
                        self.open_note_tab(tab, &locator);
                    }
                }
                Effect::ViewDetached { tab, .. } => {
                    self.notes.detach(*tab);
                }
                Effect::NoteLoadChanged { .. } => {}
            }
        }
    }

    fn handle_editor_action(&mut self, tab: TabId, incarnation: u64, action: EditorAction) {
        if self.notes.incarnation(tab) != Some(incarnation)
            || self
                .workspace
                .tab(&tab)
                .is_none_or(|view| view.interaction().incarnation != incarnation)
        {
            self.status = "Ignored input from a replaced editor view".into();
            return;
        }
        match action {
            EditorAction::Interaction(snapshot) => {
                let Some(current) = self.note_editor_view(tab) else {
                    return;
                };
                if snapshot.buffer_id != current.buffer_id || snapshot.revision != current.revision
                {
                    self.status =
                        "Editor input was stale; the current caret state was restored".into();
                    return;
                }
                let selection =
                    flavite_workspace::Selection::new(snapshot.selection.0, snapshot.selection.1);
                let mut interaction = self
                    .workspace
                    .tab(&tab)
                    .expect("tab verified above")
                    .interaction()
                    .clone();
                interaction.buffer_id = Some(snapshot.buffer_id);
                interaction.revision = snapshot.revision;
                interaction.sequence = snapshot.sequence;
                interaction.selections = vec![selection];
                interaction.viewport_anchor = snapshot.viewport_anchor;
                interaction.composition = if snapshot.composing {
                    flavite_workspace::CompositionState::Active
                } else {
                    flavite_workspace::CompositionState::Idle
                };
                interaction.focused = snapshot.focused;
                if let Err(error) = self
                    .workspace
                    .acknowledge_interaction(interaction, &current.text)
                {
                    self.status = format!("Editor state was not accepted: {error}");
                }
            }
            EditorAction::Navigate {
                buffer_id,
                expected_revision,
                navigation,
            } => {
                let Some(source) = self.note_editor_view(tab) else {
                    self.status = "Link source is no longer open".into();
                    return;
                };
                if source.buffer_id != buffer_id || source.revision != expected_revision {
                    self.status = "Link source changed; activate the link again".into();
                    return;
                }
                let resolution = self.flavite.as_ref().and_then(|flavite| {
                    flavite
                        .resolve_note_target(navigation.target.clone(), None)
                        .ok()
                });
                let Some(resolution) = resolution else {
                    self.status =
                        "Link target resolution is unavailable; retry after indexing".into();
                    return;
                };
                if resolution.is_ambiguous {
                    self.status = "Link target is ambiguous; choose a note from the picker".into();
                    return;
                }
                let Some(target) = resolution.note else {
                    self.status = if resolution.is_unresolved && navigation.create_if_unresolved {
                        "Link target does not exist; create it explicitly from the Notes view"
                            .into()
                    } else {
                        "Link target is not an indexed note".into()
                    };
                    return;
                };
                let Some(entry) = self
                    .catalog
                    .iter()
                    .find(|entry| entry.native_path == target.path_native)
                else {
                    self.status =
                        "Resolved note is outside the current catalog page; refresh the catalog"
                            .into();
                    return;
                };
                match NoteLocator::from_relative(&entry.path) {
                    Ok(locator) => self.request_open(locator, self.group_of(tab)),
                    Err(error) => {
                        self.status = format!("Resolved note path cannot be opened here: {error}");
                    }
                }
            }
            edit_or_history => {
                let expected_buffer = self.notes.state(&tab).and_then(|state| state.buffer);
                let Some(expected_buffer) = expected_buffer else {
                    self.status = "Editor buffer is not ready".into();
                    return;
                };
                let buffer_matches = match &edit_or_history {
                    EditorAction::Edit { buffer_id, .. }
                    | EditorAction::History { buffer_id, .. } => *buffer_id == expected_buffer,
                    EditorAction::Navigate { .. } | EditorAction::Interaction(_) => false,
                };
                if !buffer_matches {
                    self.status = "Editor action targeted a different buffer".into();
                    return;
                }
                // Any mutation changes the authorization revision. A later
                // acknowledged caret event may explicitly establish a new
                // target; the old byte position is never silently rebased.
                self.workspace.invalidate_writing_target(expected_buffer);
                let Some(flavite) = &self.flavite else {
                    self.status = "Open a vault before editing".into();
                    return;
                };
                let request_id = flavite.command_registry().next_request_id();
                let Some(invocation) = edit_or_history.into_invocation(request_id) else {
                    self.status = "Editor action could not be routed".into();
                    return;
                };
                match flavite.execute(invocation) {
                    ExecutionResult::Local(LocalResult::Success(success)) => {
                        let revision = success
                            .data
                            .get("revision")
                            .and_then(serde_json::Value::as_u64);
                        self.status = revision.map_or_else(
                            || "Edit applied".into(),
                            |revision| format!("Edited revision {revision}"),
                        );
                    }
                    ExecutionResult::Local(LocalResult::Error(error)) => {
                        self.status = format!("Edit failed: {}", error.message);
                    }
                    ExecutionResult::Rejected(rejected) => {
                        self.status = format!("Edit rejected: {:?}", rejected.reason);
                    }
                    ExecutionResult::Pending { request_id } => {
                        self.operations.accept(request_id, "editor mutation");
                        self.status = "Editor mutation pending".into();
                    }
                    ExecutionResult::ActionRequired { code, .. } => {
                        self.status = format!("Edit needs attention: {code}");
                    }
                    ExecutionResult::Superseded { by_request, .. } => {
                        self.status = format!("Edit superseded by request {by_request}");
                    }
                    ExecutionResult::Local(LocalResult::Void) => {
                        self.status = "Editor command completed".into();
                    }
                }
            }
        }
    }

    /// Issue the async open for one note tab (RFC-0003 §8.3 cold open).
    fn open_note_tab(&mut self, tab: TabId, locator: &NoteLocator) {
        let Some(flavite) = &self.flavite else {
            return;
        };
        let path = locator.to_path(&self.root);
        let request_id = flavite.command_registry().next_request_id();
        let invocation = Invocation::new(
            request_id,
            "note.open",
            serde_json::json!({ "path": path, "policy": "existing_only" }),
            Origin::Gui,
        );
        match flavite.execute(invocation) {
            ExecutionResult::Pending { request_id } => {
                self.operations.accept(request_id, "note.open");
                self.notes.track_request(tab, request_id);
            }
            ExecutionResult::Local(LocalResult::Success(success)) => {
                let buffer = success
                    .data
                    .get("buffer_id")
                    .and_then(serde_json::Value::as_u64);
                self.notes.ready(tab, buffer);
                self.dispatch_layout(|revision| LayoutAction::NoteLoadReady {
                    expected_revision: revision,
                    tab,
                });
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                self.status = format!(
                    "Could not open {}: {}",
                    locator.components().join("/"),
                    error.message
                );
                self.dispatch_layout(|revision| LayoutAction::NoteLoadFailed {
                    expected_revision: revision,
                    tab,
                });
            }
            ExecutionResult::Rejected(rejected) => {
                self.status = format!(
                    "Could not open {}: {:?}",
                    locator.components().join("/"),
                    rejected.reason
                );
                self.dispatch_layout(|revision| LayoutAction::NoteLoadFailed {
                    expected_revision: revision,
                    tab,
                });
            }
            ExecutionResult::ActionRequired { code, .. } => {
                self.status = format!("Open needs attention: {code}");
                self.dispatch_layout(|revision| LayoutAction::NoteLoadFailed {
                    expected_revision: revision,
                    tab,
                });
            }
            ExecutionResult::Superseded { by_request, .. } => {
                self.status = format!("Open superseded by request {by_request}");
                self.dispatch_layout(|revision| LayoutAction::NoteLoadFailed {
                    expected_revision: revision,
                    tab,
                });
            }
            ExecutionResult::Local(LocalResult::Void) => {
                self.dispatch_layout(|revision| LayoutAction::NoteLoadFailed {
                    expected_revision: revision,
                    tab,
                });
            }
        }
    }

    /// Route an open request canonically: focus an existing tab of the same
    /// note, otherwise open into `group` (default: the focused group).
    fn request_open(&mut self, locator: NoteLocator, group: Option<GroupId>) {
        let existing = self
            .workspace
            .tabs()
            .find_map(|(tab, session)| match session.kind() {
                flavite_workspace::ViewKind::Note(note) if note.locator() == &locator => Some(*tab),
                _ => None,
            });
        if let Some(tab) = existing {
            self.dispatch_layout(|revision| LayoutAction::ActivateTab {
                expected_revision: revision,
                tab,
            });
            return;
        }
        let Some(group) = group
            .or_else(|| self.focused_group())
            .or_else(|| self.workspace.first_group())
        else {
            self.status = "Open a group before opening notes".into();
            return;
        };
        self.dispatch_layout(|revision| LayoutAction::OpenNote {
            expected_revision: revision,
            locator,
            group,
        });
    }

    /// Apply one host event.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Keyboard(event) => {
                if let Some(message) = host_commands::keyboard_message(self, event) {
                    return self.update(message);
                }
            }
            Message::Mouse(event) => self.handle_mouse(event),
            Message::WindowEvent(iced::window::Event::Focused) => {
                if let Some(flavite) = &self.flavite
                    && let Err(error) = flavite.focus()
                {
                    self.status = format!("Vault focus reconcile failed: {error}");
                }
            }
            Message::WindowEvent(iced::window::Event::Resized(size)) => {
                self.window_size = size;
            }
            Message::WindowEvent(iced::window::Event::Unfocused) => {
                self.gesture.cancel();
            }
            Message::WindowEvent(_) => {}
            Message::Pump => self.pump(),
            Message::Layout(action) => {
                if self.has_composing_view() {
                    self.status =
                        "Finish or cancel text composition before changing the layout".into();
                    return Task::none();
                }
                let revision = self.workspace.revision();
                let mut action = action;
                if action.expected_revision() != revision {
                    // Re-stamp actions that arrived through stateless
                    // widgets so a stale click cannot masquerade as valid.
                    action = restamp(action, revision);
                }
                let outcome = flavite_workspace::reduce(&self.workspace, action);
                self.commit(outcome);
            }
            Message::NewViewInFocused(kind) => {
                if let Some(group) = self
                    .focused_group()
                    .or_else(|| self.workspace.first_group())
                {
                    self.dispatch_layout(|revision| match kind {
                        flavite_workspace::ViewKind::Empty => LayoutAction::OpenEmpty {
                            expected_revision: revision,
                            group,
                        },
                        flavite_workspace::ViewKind::Notes => LayoutAction::OpenNotes {
                            expected_revision: revision,
                            group,
                        },
                        flavite_workspace::ViewKind::Note(_) => LayoutAction::OpenEmpty {
                            expected_revision: revision,
                            group,
                        },
                    });
                }
            }
            Message::OpenNote { locator, group } => self.request_open(locator, group),
            Message::GroupMenu(group, menu) => {
                self.strip_menu = match self.strip_menu {
                    Some((open_group, open_menu)) if open_group == group && open_menu == menu => {
                        None
                    }
                    _ => Some((group, menu)),
                };
            }
            Message::TabPressed(tab) => {
                self.gesture.press(tab);
                let revision = self.workspace.revision();
                let outcome = flavite_workspace::reduce(
                    &self.workspace,
                    LayoutAction::ActivateTab {
                        expected_revision: revision,
                        tab,
                    },
                );
                self.commit(outcome);
            }
            Message::TabHover { group, index } => self.gesture.hover(group, index),
            Message::BodyHover(group) => {
                let count = self
                    .workspace
                    .group(&group)
                    .map_or(0, |group| group.tabs().len());
                self.gesture.hover_group(group, count);
            }
            Message::PaneDrag(event) => self.handle_pane_drag(event),
            Message::PaneResize(event) => {
                if let Some(split) = self.adapter.workspace_split(event.split) {
                    let ppm = ratio_to_ppm(event.ratio);
                    self.resizing_split = Some(split);
                    self.resize_preview.insert(split, ppm);
                }
            }
            Message::PaneClicked(pane) => {
                if let Some(group) = self.adapter.group_of_pane(pane) {
                    let revision = self.workspace.revision();
                    let outcome = flavite_workspace::reduce(
                        &self.workspace,
                        LayoutAction::FocusGroup {
                            expected_revision: revision,
                            group,
                        },
                    );
                    self.commit(outcome);
                }
            }
            Message::PaletteOpen => {
                self.palette_open = true;
                self.palette_selected = 0;
                return iced::widget::operation::focus(crate::workspace_shell::PALETTE_INPUT_ID);
            }
            Message::PaletteClose => self.palette_open = false,
            Message::PaletteQuery(query) => {
                self.palette_query = query;
                self.palette_selected = 0;
            }
            Message::PaletteNext => {
                let last = self.palette_matches().len().saturating_sub(1);
                self.palette_selected = (self.palette_selected + 1).min(last);
            }
            Message::PalettePrevious => {
                self.palette_selected = self.palette_selected.saturating_sub(1);
            }
            Message::PaletteSubmit => {
                let selected = self.palette_selected;
                return self.run_palette_selection(selected);
            }
            Message::PalettePick(index) => return self.run_palette_selection(index),
            Message::PickerOpen => {
                self.picker = Picker {
                    open: true,
                    query: String::new(),
                    selected: 0,
                };
                if self.catalog.is_empty() {
                    self.load_catalog();
                }
                return iced::widget::operation::focus(crate::workspace_shell::PICKER_INPUT_ID);
            }
            Message::PickerClose => self.picker = Picker::default(),
            Message::PickerQuery(query) => {
                self.picker.query = query;
                self.picker.selected = 0;
            }
            Message::PickerNext => {
                let last = self.picker_matches().len().saturating_sub(1);
                self.picker.selected = (self.picker.selected + 1).min(last);
            }
            Message::PickerPrevious => {
                self.picker.selected = self.picker.selected.saturating_sub(1);
            }
            Message::PickerSubmit => {
                let selected = self.picker.selected;
                return self.run_picker_selection(selected);
            }
            Message::PickerPick(index) => return self.run_picker_selection(index),
            Message::RunHostCommand(id) => return host_commands::dispatch(self, id),
            Message::RunCoreCommand(id) => self.run_core_command(id),
            Message::RefreshCatalog => self.load_catalog(),
            Message::AcknowledgeNotice(id) => {
                if let Some(index) = self.notices.iter().position(|notice| notice.id == id) {
                    self.notices.remove(index);
                    if let Some(flavite) = &self.flavite {
                        flavite.acknowledge_notice(id);
                    }
                }
            }
            Message::EditorAction {
                tab,
                incarnation,
                action,
            } => self.handle_editor_action(tab, incarnation, action),
        }
        #[cfg(feature = "ui-test-support")]
        crate::harness::observe(self);
        Task::none()
    }

    fn handle_mouse(&mut self, event: iced::mouse::Event) {
        match event {
            iced::mouse::Event::CursorMoved { position } => {
                self.gesture.moved(position);
            }
            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left) => {
                if let Some((tab, target)) = self.gesture.release() {
                    self.dispatch_layout(|revision| LayoutAction::MoveTab {
                        expected_revision: revision,
                        tab,
                        target: flavite_workspace::TabDropTarget::Index {
                            group: target.group,
                            index: target.index,
                        },
                    });
                }
                if let Some(split) = self.resizing_split.take()
                    && let Some(ppm) = self.resize_preview.remove(&split)
                {
                    self.dispatch_layout(|revision| LayoutAction::ResizeSplit {
                        expected_revision: revision,
                        split,
                        desired_ratio_ppm: ppm,
                    });
                }
            }
            _ => {}
        }
    }

    fn handle_pane_drag(&mut self, event: iced::widget::pane_grid::DragEvent) {
        use iced::widget::pane_grid::{DragEvent, Target};
        match event {
            DragEvent::Dropped { pane, target } => {
                let Some(source) = self.adapter.group_of_pane(pane) else {
                    return;
                };
                let action = match target {
                    Target::Pane(other, region) => {
                        let Some(destination) = self.adapter.group_of_pane(other) else {
                            return;
                        };
                        match region {
                            iced::widget::pane_grid::Region::Center => LayoutAction::MergeGroups {
                                expected_revision: self.workspace.revision(),
                                source,
                                destination,
                            },
                            iced::widget::pane_grid::Region::Edge(edge) => {
                                let (arrangement, side) =
                                    crate::workspace::pane_grid::workspace_split_target(edge);
                                LayoutAction::MoveGroupSplit {
                                    expected_revision: self.workspace.revision(),
                                    source,
                                    destination,
                                    arrangement,
                                    side,
                                }
                            }
                        }
                    }
                    Target::Edge(edge) => {
                        let edge = crate::workspace::pane_grid::workspace_edge(edge);
                        LayoutAction::MoveGroupEdge {
                            expected_revision: self.workspace.revision(),
                            group: source,
                            edge,
                        }
                    }
                };
                let outcome = flavite_workspace::reduce(&self.workspace, action);
                self.commit(outcome);
            }
            DragEvent::Picked { .. } | DragEvent::Canceled { .. } => {}
        }
    }

    /// Process vault events and completions on the host pump. Presentation
    /// never gates core progress (FR-28).
    fn pump(&mut self) {
        let drained = {
            let Some(flavite) = &self.flavite else {
                return;
            };
            flavite.pump();
            let mut refresh_catalog = false;
            let credits = 256usize.saturating_sub(self.notices.len());
            let mut events = Vec::new();
            for delivery in flavite.poll_events(credits) {
                let event = delivery.event;
                if let flavite_api::Event::ReconcileProgress { done, total } = &event {
                    self.reconcile_progress = (*done < *total).then_some((*done, *total));
                }
                if matches!(&event, flavite_api::Event::VaultChanged { .. }) {
                    refresh_catalog = true;
                }
                events.push(format!("{}: {event:?}", event.event_type()));
                if let Some(id) = delivery.notice_id {
                    self.notices.push_back(NoticePresentation { id, event });
                }
            }
            let completions = flavite.drain_operation_completions();
            if refresh_catalog {
                // Scoped borrow ends before mutation below.
                self.load_catalog();
            }
            (events, completions)
        };
        let (events, completions) = drained;
        for event in events {
            self.recent_events.push(event);
        }
        self.recent_events.truncate(100);
        self.operations.ingest(&completions);
        for completion in completions {
            if let Some(tab) = self.notes.finish_request(completion.request_id) {
                match &completion.result {
                    ExecutionResult::Local(LocalResult::Success(success)) => {
                        let buffer = success
                            .data
                            .get("buffer_id")
                            .and_then(serde_json::Value::as_u64);
                        self.notes.ready(tab, buffer);
                        self.dispatch_layout(|revision| LayoutAction::NoteLoadReady {
                            expected_revision: revision,
                            tab,
                        });
                    }
                    _ => {
                        self.dispatch_layout(|revision| LayoutAction::NoteLoadFailed {
                            expected_revision: revision,
                            tab,
                        });
                    }
                }
            } else {
                self.status = format!("Request {} completed", completion.request_id);
            }
        }
        if let Some(target) = self.workspace.writing_target() {
            let remains_current = self.notes.incarnation(target.tab) == Some(target.incarnation)
                && self.note_editor_view(target.tab).is_some_and(|view| {
                    view.buffer_id == target.buffer_id && view.revision == target.revision
                })
                && self
                    .workspace
                    .tab(&target.tab)
                    .is_some_and(|view| view.interaction().sequence == target.interaction_sequence);
            if !remains_current {
                self.workspace.invalidate_writing_target(target.buffer_id);
            }
        }
    }

    fn run_core_command(&mut self, command_id: &'static str) {
        let Some(flavite) = &self.flavite else {
            self.status = "Open a vault before running commands".into();
            return;
        };
        let args = match command_id {
            "note.save" | "note.close" => {
                let focused = self.workspace.focused_tab();
                let Some(state) = self.notes.state(&focused) else {
                    self.status = format!("{command_id} requires a note tab");
                    return;
                };
                serde_json::json!(state.locator.to_path(&self.root))
            }
            "vault.reconcile" => serde_json::json!({ "scope": "full", "mode": "fast" }),
            _ => serde_json::Value::Null,
        };
        let request_id = flavite.command_registry().next_request_id();
        let invocation = Invocation::new(request_id, command_id, args, Origin::Gui);
        match flavite.execute(invocation) {
            ExecutionResult::Local(LocalResult::Success(_)) => {
                self.status = match command_id {
                    "note.save" => "Note saved".into(),
                    "note.close" => "Note closed".into(),
                    "vault.reconcile" => "Vault refreshed".into(),
                    _ => format!("{command_id} completed"),
                };
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                self.status = format!("{command_id} failed: {}", error.message);
            }
            ExecutionResult::Local(LocalResult::Void) => {
                self.status = format!("{command_id} completed");
            }
            ExecutionResult::Pending { request_id } => {
                self.operations.accept(request_id, command_id);
                self.status = format!("{command_id} pending");
            }
            ExecutionResult::ActionRequired { code, .. } => {
                self.status = format!("{command_id} needs a decision: {code}");
            }
            ExecutionResult::Superseded { by_request, .. } => {
                self.status = format!("{command_id} superseded by request {by_request}");
            }
            ExecutionResult::Rejected(rejected) => {
                self.status = format!("{command_id} rejected: {:?}", rejected.reason);
            }
        }
    }

    /// The combined catalog snapshot for the palette: core specs plus the
    /// host commands, in stable order.
    fn palette_matches(&self) -> Vec<flavite_api::CommandSpec> {
        let mut specs = host_commands::ui_specs();
        if let Some(flavite) = &self.flavite {
            specs.extend(flavite.command_registry().specs());
        }
        matching_specs(specs, &self.palette_query)
    }

    fn run_palette_selection(&mut self, index: usize) -> Task<Message> {
        let matches = self.palette_matches();
        let Some(spec) = matches.get(index.min(matches.len().saturating_sub(1))) else {
            self.palette_open = false;
            return Task::none();
        };
        let id = spec.id;
        self.palette_open = false;
        self.palette_query.clear();
        self.palette_selected = 0;
        if let Some(command) = host_commands::UI_COMMANDS.iter().find(|c| c.id == id) {
            return host_commands::dispatch(self, command.id);
        }
        self.run_core_command(id);
        Task::none()
    }

    /// Fuzzy-matched catalog entries for the picker.
    #[must_use]
    pub fn picker_matches(&self) -> Vec<NoteCatalogEntry> {
        picker_matches(self.catalog.clone(), &self.picker.query)
    }

    fn run_picker_selection(&mut self, index: usize) -> Task<Message> {
        let matches = self.picker_matches();
        self.picker = Picker::default();
        let Some(entry) = matches.get(index.min(matches.len().saturating_sub(1))) else {
            return Task::none();
        };
        let Ok(locator) = NoteLocator::from_relative(&entry.path) else {
            self.status = format!(
                "The picker entry is not a valid note path: {}",
                entry.path.display()
            );
            return Task::none();
        };
        self.request_open(locator, None);
        Task::none()
    }

    /// Rebuild the accessibility routing map from current state.
    pub fn refresh_semantics(&mut self) {
        self.a11y_nodes = crate::accessibility::tree::build_node_map(self);
    }

    /// Bridge vault channels and operation replies.
    pub fn subscription(_state: &Self) -> Subscription<Message> {
        Subscription::batch([
            subscription::pump().map(|_| Message::Pump),
            keyboard::listen().map(Message::Keyboard),
            iced::event::listen_with(|event, _status, _id| match event {
                iced::Event::Mouse(mouse_event) => Some(Message::Mouse(mouse_event)),
                _ => None,
            }),
            iced::window::events().map(|(_, event)| Message::WindowEvent(event)),
        ])
    }
}

/// Re-stamp an action built against an older revision. Widget-held actions
/// are rebuilt at the current revision so stale clicks still express intent
/// without silently passing the guard.
fn restamp(action: LayoutAction, revision: u64) -> LayoutAction {
    use flavite_workspace::LayoutAction as A;
    match action {
        A::OpenNote { locator, group, .. } => A::OpenNote {
            expected_revision: revision,
            locator,
            group,
        },
        A::OpenEmpty { group, .. } => A::OpenEmpty {
            expected_revision: revision,
            group,
        },
        A::OpenNotes { group, .. } => A::OpenNotes {
            expected_revision: revision,
            group,
        },
        A::ActivateTab { tab, .. } => A::ActivateTab {
            expected_revision: revision,
            tab,
        },
        A::FocusGroup { group, .. } => A::FocusGroup {
            expected_revision: revision,
            group,
        },
        A::CloseTab { tab, .. } => A::CloseTab {
            expected_revision: revision,
            tab,
        },
        A::CloseGroup { group, .. } => A::CloseGroup {
            expected_revision: revision,
            group,
        },
        A::DuplicateTab { tab, .. } => A::DuplicateTab {
            expected_revision: revision,
            tab,
        },
        A::ReopenClosed { .. } => A::ReopenClosed {
            expected_revision: revision,
        },
        A::MoveTab { tab, target, .. } => A::MoveTab {
            expected_revision: revision,
            tab,
            target,
        },
        A::MergeGroups {
            source,
            destination,
            ..
        } => A::MergeGroups {
            expected_revision: revision,
            source,
            destination,
        },
        A::MoveGroupSplit {
            source,
            destination,
            arrangement,
            side,
            ..
        } => A::MoveGroupSplit {
            expected_revision: revision,
            source,
            destination,
            arrangement,
            side,
        },
        A::MoveGroupEdge { group, edge, .. } => A::MoveGroupEdge {
            expected_revision: revision,
            group,
            edge,
        },
        A::SplitGroup {
            group, arrangement, ..
        } => A::SplitGroup {
            expected_revision: revision,
            group,
            arrangement,
        },
        A::ResizeSplit {
            split,
            desired_ratio_ppm,
            ..
        } => A::ResizeSplit {
            expected_revision: revision,
            split,
            desired_ratio_ppm,
        },
        A::ToggleMaximize { group, .. } => A::ToggleMaximize {
            expected_revision: revision,
            group,
        },
        A::NoteLoadReady { tab, .. } => A::NoteLoadReady {
            expected_revision: revision,
            tab,
        },
        A::NoteLoadFailed { tab, .. } => A::NoteLoadFailed {
            expected_revision: revision,
            tab,
        },
        A::CycleTab { forward, .. } => A::CycleTab {
            expected_revision: revision,
            forward,
        },
        A::CycleGroup { forward, .. } => A::CycleGroup {
            expected_revision: revision,
            forward,
        },
    }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn ratio_to_ppm(ratio: f32) -> u32 {
    let ppm = (ratio * 1_000_000.0).round() as i64;
    ppm.clamp(
        i64::from(flavite_workspace::MIN_RATIO_PPM),
        i64::from(flavite_workspace::MAX_RATIO_PPM),
    ) as u32
}

/// Fuzzy-match catalog entries against title and path (§4.7 in-memory).
#[must_use]
pub fn picker_matches(entries: Vec<NoteCatalogEntry>, query: &str) -> Vec<NoteCatalogEntry> {
    let needle = query.trim();
    if needle.is_empty() {
        return entries;
    }
    use nucleo::pattern::{CaseMatching, Normalization, Pattern};
    use nucleo::{Config, Matcher};
    static MATCHER: std::sync::LazyLock<std::sync::Mutex<Matcher>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(Matcher::new(Config::DEFAULT)));
    let texts: Vec<String> = entries
        .iter()
        .map(|entry| format!("{} {}", entry.title, entry.path.display()))
        .collect();
    let pattern = Pattern::parse(needle, CaseMatching::Smart, Normalization::Smart);
    let mut matcher = MATCHER
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    pattern
        .match_list(texts.iter().map(String::as_str), &mut matcher)
        .into_iter()
        .filter_map(|(matched, _)| {
            texts
                .iter()
                .position(|candidate| *candidate == matched)
                .map(|index| entries[index].clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_app() -> App {
        let mut ids = Ids::new();
        let workspace = WorkspaceState::with_ids(&mut ids);
        App {
            flavite: None,
            root: PathBuf::from("fixture-vault"),
            adapter: Adapter::build(&workspace),
            notes: NoteRegistry::default(),
            picker: Picker::default(),
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            strip_menu: None,
            gesture: gestures::TabGesture::default(),
            resize_preview: BTreeMap::new(),
            resizing_split: None,
            catalog: Vec::new(),
            catalog_error: None,
            settings: Settings::default(),
            settings_valid: true,
            operations: OperationPump::default(),
            status: "Ready".into(),
            recent_events: Vec::new(),
            notices: VecDeque::new(),
            reconcile_progress: None,
            window_size: iced::Size::new(1180.0, 760.0),
            a11y_nodes: BTreeMap::new(),
            workspace,
        }
    }

    #[test]
    fn palette_overlay_keeps_its_query_until_a_command_runs() {
        let mut app = empty_app();
        let _ = app.update(Message::PaletteOpen);
        assert!(app.palette_open);
        let _ = app.update(Message::PaletteQuery("save".into()));
        assert_eq!(app.palette_query, "save");
        let _ = app.update(Message::PaletteClose);
        assert!(!app.palette_open);
        assert_eq!(app.palette_query, "save", "Esc preserves the query");
        let _ = app.update(Message::RunHostCommand("workspace.new_tab"));
        assert!(app.palette_query.is_empty() || !app.palette_open);
    }

    #[test]
    fn layout_messages_re_stamp_stale_revisions_and_commit() {
        let mut app = empty_app();
        let group = app.workspace.first_group().expect("group");
        let stale = LayoutAction::OpenEmpty {
            expected_revision: app.workspace.revision() + 50,
            group,
        };
        let _ = app.update(Message::Layout(stale));
        assert_eq!(app.workspace.revision(), 1, "restamped action commits");
        assert_eq!(app.workspace.tabs().count(), 2);
    }

    #[test]
    fn new_view_actions_land_in_the_focused_group() {
        let mut app = empty_app();
        let group = app.workspace.first_group().expect("group");
        let _ = app.update(Message::Layout(LayoutAction::FocusGroup {
            expected_revision: app.workspace.revision(),
            group,
        }));
        let _ = app.update(Message::NewViewInFocused(
            flavite_workspace::ViewKind::Notes,
        ));
        assert_eq!(app.workspace.tabs().count(), 2);
        let focused = app.workspace.focused_tab();
        assert_eq!(
            app.workspace.tab(&focused).map(|session| session.title()),
            Some("Notes".into())
        );
    }

    #[test]
    fn picker_matching_ranks_titles_and_paths() {
        let entries = vec![
            NoteCatalogEntry {
                path: PathBuf::from("Systems/Feedback loops.md"),
                native_path: b"Systems/Feedback loops.md".to_vec(),
                title: "Feedback loops".into(),
            },
            NoteCatalogEntry {
                path: PathBuf::from("Sources/archive.md"),
                native_path: b"Sources/archive.md".to_vec(),
                title: "Archive".into(),
            },
        ];
        let matched = picker_matches(entries, "feedback");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].title, "Feedback loops");
    }

    #[test]
    fn closing_the_focused_tab_returns_focus_to_a_neighbor() {
        let mut app = empty_app();
        let group = app.workspace.first_group().expect("group");
        let locator = NoteLocator::from_components(["a.md"]).expect("locator");
        let _ = app.update(Message::OpenNote {
            locator,
            group: Some(group),
        });
        assert_eq!(app.workspace.tabs().count(), 2);
        let focused = app.workspace.focused_tab();
        let _ = app.update(Message::Layout(LayoutAction::CloseTab {
            expected_revision: app.workspace.revision(),
            tab: focused,
        }));
        assert_eq!(app.workspace.tabs().count(), 1);
        let remaining = app.workspace.focused_tab();
        assert_ne!(remaining, focused);
    }

    #[test]
    fn gestures_resolve_to_one_move_on_release() {
        let mut app = empty_app();
        let group = app.workspace.first_group().expect("group");
        let locator = NoteLocator::from_components(["a.md"]).expect("locator");
        let _ = app.update(Message::OpenNote {
            locator,
            group: Some(group),
        });
        let tab = app.workspace.focused_tab();
        let _ = app.update(Message::TabPressed(tab));
        let _ = app.update(Message::Mouse(iced::mouse::Event::CursorMoved {
            position: iced::Point::new(10.0, 10.0),
        }));
        let _ = app.update(Message::Mouse(iced::mouse::Event::CursorMoved {
            position: iced::Point::new(40.0, 10.0),
        }));
        assert!(app.gesture.dragging());
        let _ = app.update(Message::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left,
        )));
        assert!(!app.gesture.dragging());
        // The tab stays where it was: no hover target was set, so nothing
        // moved and the reducer reported the no-op.
        assert!(app.workspace.tab(&tab).is_some());
    }

    #[test]
    fn keyboard_chords_route_to_host_commands() {
        use flavite_workspace::LayoutAction;
        let app = empty_app();
        let press =
            |key: keyboard::Key, modifiers: keyboard::Modifiers| keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key,
                physical_key: keyboard::key::Physical::Unidentified(
                    keyboard::key::NativeCode::Unidentified,
                ),
                location: keyboard::Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            };
        let message = host_commands::keyboard_message(
            &app,
            press(
                keyboard::Key::Character("t".into()),
                keyboard::Modifiers::COMMAND,
            ),
        )
        .expect("ctrl+t resolves");
        assert!(matches!(
            message,
            Message::RunHostCommand("workspace.new_tab")
        ));
        let message = host_commands::keyboard_message(
            &app,
            press(
                keyboard::Key::Named(keyboard::key::Named::ArrowRight),
                keyboard::Modifiers::COMMAND | keyboard::Modifiers::ALT,
            ),
        );
        assert!(
            matches!(
                message,
                Some(Message::Layout(LayoutAction::SplitGroup {
                    arrangement: flavite_workspace::Arrangement::SideBySide,
                    ..
                }))
            ),
            "ctrl+alt+right splits side by side"
        );
        let message = host_commands::keyboard_message(
            &app,
            press(
                keyboard::Key::Named(keyboard::key::Named::ArrowLeft),
                keyboard::Modifiers::COMMAND
                    | keyboard::Modifiers::ALT
                    | keyboard::Modifiers::SHIFT,
            ),
        );
        assert!(
            matches!(
                message,
                Some(Message::RunHostCommand("workspace.move_tab_previous_group"))
            ),
            "ctrl+alt+shift+left moves the tab to the previous group"
        );
        // Plain typing never triggers commands.
        assert!(
            host_commands::keyboard_message(
                &app,
                press(
                    keyboard::Key::Character("x".into()),
                    keyboard::Modifiers::empty()
                ),
            )
            .is_none()
        );
    }

    #[test]
    fn resize_previews_commit_one_transaction_on_release() {
        let mut app = empty_app();
        let group = app.workspace.first_group().expect("group");
        let _ = app.update(Message::Layout(LayoutAction::SplitGroup {
            expected_revision: app.workspace.revision(),
            group,
            arrangement: flavite_workspace::Arrangement::SideBySide,
        }));
        let split = app.workspace.tree().splits()[0];
        let pane_split = app.adapter.split_map[&split];
        let _ = app.update(Message::PaneResize(iced::widget::pane_grid::ResizeEvent {
            split: pane_split,
            ratio: 0.7,
        }));
        assert_eq!(app.resize_preview.get(&split), Some(&700_000));
        let revision_before = app.workspace.revision();
        let _ = app.update(Message::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left,
        )));
        assert_eq!(app.workspace.revision(), revision_before + 1);
        assert!(app.resize_preview.is_empty());
        match app.workspace.tree() {
            flavite_workspace::LayoutNode::Split {
                desired_ratio_ppm, ..
            } => assert_eq!(*desired_ratio_ppm, 700_000),
            flavite_workspace::LayoutNode::Group(_) => panic!("expected a split"),
        }
    }

    fn button_press(key: keyboard::Key, modifiers: keyboard::Modifiers) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }
    }

    #[test]
    fn command_palette_binding_moves_to_ctrl_shift_p() {
        let app = empty_app();
        assert!(
            host_commands::keyboard_message(
                &app,
                button_press(
                    keyboard::Key::Character("p".into()),
                    keyboard::Modifiers::COMMAND
                ),
            )
            .is_some_and(|message| matches!(message, Message::PickerOpen)),
            "ctrl+p is the note picker"
        );
        assert!(
            host_commands::keyboard_message(
                &app,
                button_press(
                    keyboard::Key::Character("p".into()),
                    keyboard::Modifiers::COMMAND | keyboard::Modifiers::SHIFT,
                ),
            )
            .is_some_and(|message| matches!(message, Message::PaletteOpen)),
            "ctrl+shift+p is the command palette"
        );
    }

    #[test]
    fn quick_switch_and_palette_specs_do_not_collide_with_core_ids() {
        let conflicts = host_commands::catalog_conflicts(&[
            "note.open",
            "note.save",
            "vault.reconcile",
            "buffer.undo",
        ]);
        assert!(conflicts.is_empty());
        let seeded = host_commands::catalog_conflicts(&["workspace.quick_switch"]);
        assert_eq!(seeded.len(), 1, "a colliding core id is reported");
    }
}
