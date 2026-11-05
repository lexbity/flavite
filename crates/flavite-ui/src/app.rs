//! The iced application host for the Flavite workspace (RFC-0003 W02).
//!
//! `App` owns the authoritative [`WorkspaceState`], the toolkit adapter,
//! and the host registries. Widget events become host messages; layout
//! truth changes only through the flavite-workspace reducer. Shell
//! rendering lives in [`crate::workspace_shell`].

use crate::host::commands as host_commands;
use crate::host::context::ContextRegistry;
use crate::host::documents::{AttachDecision, DocumentRegistry};
use crate::operation::OperationPump;
use crate::palette::matching_specs;
use crate::settings::Settings;
use crate::subscription;
use crate::workspace::{Adapter, StripMenu, gestures};
use flavite_api::queries::NoteCatalogEntry;
use flavite_api::{ExecutionResult, Flavite, Invocation, LocalResult, Origin, VaultConfig};
use flavite_editor::{EditorAction, EditorInteractionSnapshot, EditorView};
use flavite_workspace::{
    Effect, GroupId, Ids, LayoutAction, NoteLocator, Outcome, SplitId, TabId, WorkspaceState,
};
use iced::{Subscription, Task, keyboard};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

/// Outstanding notice credits per poll (W03: bounded credit-based polling)
/// — the same bound as the core ledger, imported rather than duplicated.
const NOTICE_CREDIT_CAPACITY: usize = flavite_api::MAX_NOTICE_ACKS;
/// Completions routed per pump tick; the rest wait in the backlog (NFR-08).
const MAX_COMPLETION_ROUTING_PER_PUMP: usize = 64;
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
    /// When set, the picker replaces this note tab in place (W10 open in
    /// this tab) instead of opening a new tab; the replaced note enters the
    /// tab's Back history.
    pub replace_tab: Option<TabId>,
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
    pub notes: DocumentRegistry,
    /// Context tool state: bindings, effective sources, and query states.
    pub contexts: ContextRegistry,
    pub reading: crate::host::reading::ReadingHost,
    /// RFC-0006 §6.10: per-tab pending encoding decisions and repairs.
    pub encoding: crate::host::encoding::EncodingHost,
    pub input_chains: BTreeMap<TabId, crate::host::input::InputChain>,
    /// Semantic link insertion gestures (W05).
    pub linking: crate::host::linking::LinkIntents,
    /// Graph view host: per-tab camera/selection/pins/geometry (W07).
    pub graph: crate::host::graph::GraphHost,
    /// Saved-content search states (W10).
    pub search: crate::views::search::SearchHost,
    /// Local-history listings (W10).
    pub history: crate::views::history::HistoryHost,
    /// Per-view back/forward navigation (W10).
    pub nav: crate::host::navigation::NavHost,
    /// One modal dialog, when open (W10).
    pub dialog: Option<crate::host::dialogs::Dialog>,
    /// No-vault start screen state (FR-22, W13-compatible).
    pub start: crate::host::dialogs::StartState,
    /// Notes view filter text per tab.
    notes_filters: BTreeMap<TabId, String>,
    /// Search tabs whose input focus was already requested once (W10): the
    /// pane mounts a frame after the open commit, so the pump re-issues the
    /// focus exactly once per tab.
    search_focus_tried: BTreeSet<TabId>,
    /// Unified candidate snapshot, rebuilt with the catalog (W10).
    note_candidates: crate::picker::NoteCandidates,
    /// Device-local workspace persistence (W06).
    pub persistence: crate::host::persistence::PersistenceHost,
    /// Interface appearance: theme, scale, editor type, reduced motion,
    /// shortcut overrides (W11).
    pub appearance: crate::host::appearance::AppearanceHost,
    /// While a Preferences shortcut editor row is capturing, the command
    /// id waiting for the next key press (W11). Escape cancels capture.
    pub shortcut_capture: Option<String>,
    /// The active close decision (W12), when a final dirty/conflicted view
    /// needs an explicit safety outcome before its tab may disappear.
    pub closing: Option<crate::host::closing::ClosePlan>,
    /// Tabs still awaiting decisions in a group/quit close sequence.
    pub close_queue: Vec<TabId>,
    /// The quit schedule (W12), when the window close request is being
    /// resolved through document decisions, operation flushes, and the
    /// layout flush.
    pub shutdown: Option<crate::host::shutdown::ShutdownPlan>,
    /// Named layouts carried for the next layout save (W06).
    pub named_layouts: Vec<flavite_workspace::NamedLayout>,
    pub picker: Picker,
    pub palette_open: bool,
    pub palette_query: String,
    pub palette_selected: usize,
    pub strip_menu: Option<(GroupId, StripMenu)>,
    pub gesture: gestures::TabGesture,
    pub resize_preview: BTreeMap<SplitId, u32>,
    pub resizing_split: Option<SplitId>,
    /// Actual mounted geometry, used by both paint projection and pointer hits.
    pub workspace_bounds: iced::Rectangle,
    pub group_bounds: BTreeMap<GroupId, iced::Rectangle>,
    pub editor_bounds: BTreeMap<TabId, iced::Rectangle>,
    pub menu_selected: usize,
    pub pointer: Option<iced::Point>,
    pub drag_revision: Option<u64>,
    /// Set by pointer handling when something observable changed (resize
    /// preview, drag start, status); gates the per-move semantics rebuild.
    pointer_feedback: bool,
    /// Set when the window/workspace bounds changed; gates the geometry
    /// re-solve that used to run on every message.
    pub(crate) window_geometry_dirty: bool,
    catalog: Vec<NoteCatalogEntry>,
    /// Ticketed acquisition state machine (RFC-0004 §6.4, D9).
    catalog_status: CatalogStatus,
    /// Saved-layout restore waits for the first terminal catalog outcome
    /// (W-D): `plan_restore` validates membership against the catalog,
    /// which is no longer synchronously available on the attach turn.
    restore_pending: bool,
    catalog_last_submit: Option<Instant>,
    catalog_retry_due: bool,
    settings: Settings,
    settings_valid: bool,
    pub operations: OperationPump,
    /// Completions routed past the per-pump turn budget; the next pump tick
    /// continues (NFR-08: UI event-loop work is budgeted, never lost).
    completion_backlog: VecDeque<flavite_api::OperationCompletion>,
    pub status: String,
    /// Visible lossless notices awaiting explicit presentation acknowledgement.
    pub notices: VecDeque<NoticePresentation>,
    reconcile_progress: Option<(u32, u32)>,
    window_size: iced::Size,
    /// Accessibility node routing map, rebuilt after every commit.
    pub a11y_nodes: BTreeMap<accesskit::NodeId, crate::accessibility::NodeTarget>,
    pub(crate) a11y_text_cache: std::cell::RefCell<crate::accessibility::text::EditorTextCache>,
}

/// Host messages. Widget events surface here and reduce through the
/// workspace model or the host catalog.
#[derive(Debug, Clone)]
pub enum Message {
    Keyboard(keyboard::Event),
    WorkspaceBounds(iced::Rectangle),
    GroupBounds(GroupId, iced::Rectangle),
    EditorBounds(TabId, iced::Rectangle),
    MenuClose,
    MenuNext(bool),
    MenuPick(usize),
    MenuFocus(usize),
    ActivateTab(TabId),
    CloseTab(TabId),
    CancelGesture,
    ResizeBegin(SplitId),
    /// Native editor actions carry the captured view and revision guard.
    AccessibleSelection {
        tab: TabId,
        incarnation: u64,
        revision: u64,
        anchor: usize,
        head: usize,
    },
    Mouse(iced::mouse::Event),
    Pump,
    WindowEvent(iced::window::Event),
    /// A typed layout transaction.
    Layout(flavite_workspace::LayoutAction),
    /// The OS color scheme changed (W11 System theme).
    SystemTheme(iced::theme::Mode),
    /// A device-preference edit (W11).
    Preferences(crate::host::appearance::PreferenceMessage),
    /// Begin a close for one tab (W12): decide or close.
    RequestCloseTab(TabId),
    /// Begin a close for a whole group (W12): plan per tab.
    RequestCloseGroup(GroupId),
    /// The user's decision for the active close plan (W12).
    CloseDecision(crate::host::closing::CloseDecision),
    /// Keep the document open; cancel the active close plan (W12).
    CloseKeepOpen,
    /// Arm or disarm the destructive discard confirmation (W12); arming
    /// captures the current revision the user actually sees.
    CloseDiscardArm(bool),
    /// Execute the armed revision-bound discard (W12).
    CloseDiscardConfirm,
    /// Resolve a conflicted buffer through the core policy (W12).
    ConflictResolve {
        tab: TabId,
        resolution: String,
    },
    /// Apply the confirmed merged text of a prepared merge (W12).
    MergeApply,
    /// Restore or discard a retained recovery copy (W12).
    RecoveryResolve {
        path: String,
        restore: bool,
    },
    /// Shutdown schedule steps and choices (W12).
    Shutdown(crate::host::shutdown::ShutdownMessage),
    /// Open a real view kind in the focused group.
    NewViewInFocused(flavite_workspace::ViewKind),
    EditorInput {
        tab: TabId,
        incarnation: u64,
        input: flavite_editor::input::CapturedInput,
    },
    FindOpen(TabId),
    /// RFC-0006 §6.10: typed encoding decisions routed to the encoding host.
    Encoding(crate::host::encoding::EncodingMessage),
    FindClose(TabId),
    FindQuery {
        tab: TabId,
        query: String,
    },
    FindNext {
        tab: TabId,
        forward: bool,
    },
    ToggleNoteMode(TabId),
    RetryReading(TabId),
    ToggleSourcePreview(TabId),
    AppMenu,
    ReadControlBounds {
        tab: TabId,
        document: std::sync::Arc<flavite_api::ReadSnapshot>,
        role: &'static str,
        start: usize,
        rect: iced::Rectangle,
    },
    ReadBlockBounds {
        tab: TabId,
        hash: [u8; 32],
        start: usize,
        rect: iced::Rectangle,
    },
    ReadScrolled {
        tab: TabId,
        offset: f32,
    },
    ReadActivate {
        tab: TabId,
        document: std::sync::Arc<flavite_api::ReadSnapshot>,
        start: usize,
    },
    OutlineNavigate {
        tab: TabId,
        document: std::sync::Arc<flavite_api::ReadSnapshot>,
        byte: usize,
    },
    StructureEdit {
        tab: TabId,
        document: std::sync::Arc<flavite_api::ReadSnapshot>,
        command: &'static str,
        args: serde_json::Value,
    },
    PropertyDraft {
        tab: TabId,
        document: std::sync::Arc<flavite_api::ReadSnapshot>,
        key: String,
        value: String,
    },
    PropertyApply {
        tab: TabId,
        document: std::sync::Arc<flavite_api::ReadSnapshot>,
        key: String,
    },
    ExternalOpened(Result<(), String>),
    /// Open a Preview/Backlinks tool with its D-04 default binding.
    OpenContextView {
        view: flavite_workspace::ContextViewKind,
        group: Option<GroupId>,
    },
    /// Rebind an existing context tool (follow document, pin source, ...).
    SetContextBinding {
        tab: TabId,
        binding: flavite_workspace::ContextBinding,
    },
    /// Force one tool to re-query its current source (retry).
    RequeryContext {
        tab: TabId,
    },
    /// Open a note chosen from a context tool row.
    OpenNoteFromContext {
        locator: NoteLocator,
    },
    EditSourceFromContext {
        locator: NoteLocator,
    },
    /// Insert a link to this note at the acknowledged writing target (W05).
    InsertLink {
        locator: NoteLocator,
    },
    /// Insert a link to the picked note instead of opening it (Ctrl+Enter).
    PickerInsert(usize),
    /// Commit the confirmed semantic link plan.
    ConfirmLinkInsert,
    /// Cancel the pending/confirmed semantic link gesture.
    CancelLinkInsert,
    /// Open (or focus) a note, optionally in a specific group.
    OpenNote {
        locator: NoteLocator,
        group: Option<GroupId>,
    },
    /// One typed graph widget message for an exact view incarnation (W07).
    GraphAction {
        tab: TabId,
        incarnation: u64,
        message: crate::graph::renderer::GraphMessage,
        key: flavite_graph::RenderKey,
    },
    /// One graph command from a button, keyboard, menu, or a11y row.
    GraphNodeCommand {
        tab: TabId,
        incarnation: u64,
        key: flavite_graph::RenderKey,
        command: crate::host::graph::GraphCommand,
    },
    GraphCommand {
        tab: TabId,
        command: crate::host::graph::GraphCommand,
    },
    /// Observed bounds of a mounted graph surface (hit + harness mapping).
    GraphBounds(TabId, iced::Rectangle),
    GraphFilterBounds {
        tab: TabId,
        field: crate::graph::filters::FilterField,
        bounds: iced::Rectangle,
    },
    InspectGraphOccurrence {
        tab: TabId,
        incarnation: u64,
        key: flavite_graph::RenderKey,
        index: usize,
    },
    GraphFilterInput {
        tab: TabId,
        incarnation: u64,
        field: crate::graph::filters::FilterField,
        value: String,
    },
    /// Toggle a group strip's inline menu row.
    GroupMenu(GroupId, StripMenu),
    /// Open the top menu bar entry (W10 standard menus).
    TopMenu(crate::workspace::StripMenu),
    // ---- W10: Notes view ----
    /// Edit the Notes view filter for one tab.
    NotesFilter {
        tab: TabId,
        value: String,
    },
    // ---- W10: Search view ----
    SearchQuery {
        tab: TabId,
        value: String,
    },
    SearchSubmitNow {
        tab: TabId,
    },
    SearchLoadMore {
        tab: TabId,
    },
    // ---- W10: History view ----
    HistoryRefresh {
        tab: TabId,
    },
    HistoryRestore {
        tab: TabId,
        hash: String,
    },
    // ---- W10: navigation ----
    NavigateBack,
    NavigateForward,
    // ---- W10: dialogs and menubar actions ----
    /// Open a note through the native file dialog.
    OpenNoteDialog,
    /// Open the new-note dialog.
    NewNoteDialog,
    /// Open the rename/move dialog for the focused note tab.
    RenameDialog,
    AboutDialog,
    /// Edit the active dialog's text field.
    DialogInput(String),
    /// Commit the active dialog.
    DialogSubmit,
    /// Dismiss the active dialog.
    DialogCancel,
    /// Result of the native open-note file dialog.
    NotePicked(Option<std::path::PathBuf>),
    // ---- W10: no-vault start screen (FR-22) ----
    /// Open the native folder dialog for vault selection.
    VaultDialog,
    /// Result of the native vault folder dialog.
    VaultPicked(Option<std::path::PathBuf>),
    /// Edit the typed vault path on the start screen.
    VaultPathInput(String),
    /// Open (or adopt) the vault at this root.
    StartOpenVault(std::path::PathBuf),
    /// Open the rename dialog from a detected-rename notice.
    ReviewRenameNotice(flavite_api::NoticeId),
    /// A tab was pressed (activation + drag candidate).
    TabPressed(TabId),
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
    /// A menu-initiated selection/caret command (W10 Selection menu).
    /// `usize::MAX` means "end of text"; the handler resolves against the
    /// live buffer and routes through the acknowledged interaction path.
    SelectionCommand {
        tab: TabId,
        anchor: usize,
        head: usize,
    },
}

/// Catalog acquisition state (RFC-0004 §6.4, D9): the catalog is a
/// ticketed QueryService read, so the host turn never blocks on it.
/// `Loading` never clears an existing ready snapshot (no picker flicker);
/// `Failed` is visible retryable text while the snapshot stays served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogStatus {
    /// No vault attached yet (or nothing ever requested).
    Empty,
    /// A ticket is admitted; the previous snapshot (if any) keeps serving.
    Loading,
    /// Snapshot served from index `epoch`/`generation`; `truncated` means
    /// the vault holds more notes than the 50k catalog bound (R9).
    Ready {
        epoch: u64,
        generation: u64,
        truncated: bool,
    },
    /// The last acquisition failed with a retryable message.
    Failed { message: String },
}

/// `VaultChanged` catalog pulses coalesce to at most one ticket per window.
pub const CATALOG_REFRESH_THROTTLE: Duration = Duration::from_secs(1);

impl App {
    /// Open the configured vault and construct the shell.
    #[must_use]
    pub fn boot() -> Self {
        let root = crate::host::env::vault_root_from_env()
            .unwrap_or_else(crate::host::env::default_vault_root);
        Self::boot_from_root(root)
    }

    /// Construct the shell against an explicit vault root.
    ///
    /// When no vault can be opened, the shell starts vaultless on the start
    /// screen (FR-22): real open/create actions, never demo content.
    #[must_use]
    pub fn boot_from_root(root: PathBuf) -> Self {
        let mut app = Self::empty_at(root.clone());
        // Fixture-only knob: the visual harness stretches the autosave idle
        // window so a crash lands between the recovery checkpoint and the
        // autosave. Production execution never reads this variable.
        #[cfg(feature = "ui-test-support")]
        let config = {
            let mut config = VaultConfig::default();
            if let Ok(secs) = std::env::var("FLAVITE_HARNESS_AUTOSAVE_IDLE_SECS")
                && let Ok(idle) = secs.parse::<u64>()
            {
                config.autosave_idle_interval = std::time::Duration::from_secs(idle);
            }
            config
        };
        #[cfg(not(feature = "ui-test-support"))]
        let config = VaultConfig::default();
        match Flavite::open_root(root, config) {
            Ok(flavite) => app.attach_vault(flavite),
            Err(error) => {
                app.start.last_vault = flavite_api::read_last_vault_device().ok().flatten();
                app.start.error = Some(format!("Could not open that vault: {error}"));
                app.status = "Choose a vault folder to continue".into();
            }
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
            notes: DocumentRegistry::default(),
            contexts: ContextRegistry::default(),
            reading: crate::host::reading::ReadingHost::default(),
            encoding: crate::host::encoding::EncodingHost::default(),
            input_chains: BTreeMap::new(),
            linking: crate::host::linking::LinkIntents::default(),
            graph: {
                let mut host = crate::host::graph::GraphHost::default();
                host.backend = crate::host::graph::GraphBackend::from_env();
                host
            },
            search: crate::views::search::SearchHost::default(),
            history: crate::views::history::HistoryHost::default(),
            nav: crate::host::navigation::NavHost::default(),
            dialog: None,
            start: crate::host::dialogs::StartState::default(),
            notes_filters: BTreeMap::new(),
            search_focus_tried: BTreeSet::new(),
            note_candidates: crate::picker::NoteCandidates::default(),
            persistence: crate::host::persistence::PersistenceHost::default(),
            appearance: crate::host::appearance::AppearanceHost::boot(),
            shortcut_capture: None,
            closing: None,
            close_queue: Vec::new(),
            shutdown: None,
            named_layouts: Vec::new(),
            picker: Picker::default(),
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            strip_menu: None,
            gesture: gestures::TabGesture::default(),
            resize_preview: BTreeMap::new(),
            resizing_split: None,
            workspace_bounds: iced::Rectangle::new(
                iced::Point::ORIGIN,
                iced::Size::new(1180.0, 650.0),
            ),
            group_bounds: BTreeMap::new(),
            editor_bounds: BTreeMap::new(),
            menu_selected: 0,
            pointer: None,
            drag_revision: None,
            pointer_feedback: false,
            window_geometry_dirty: true,
            catalog: Vec::new(),
            catalog_status: CatalogStatus::Empty,
            catalog_last_submit: None,
            catalog_retry_due: false,
            restore_pending: false,
            settings: Settings::default(),
            settings_valid: true,
            operations: OperationPump::default(),
            completion_backlog: VecDeque::new(),
            status,
            notices: VecDeque::new(),
            reconcile_progress: None,
            window_size: iced::Size::new(1180.0, 760.0),
            a11y_nodes: BTreeMap::new(),
            a11y_text_cache: std::cell::RefCell::default(),
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
        // Device preferences load at attach (W11): parse failures keep
        // defaults with saving disabled; the document is never overwritten.
        self.appearance.load(&flavite);
        if let Some(note) = self.appearance.status_note() {
            self.status = format!("{} — {}", self.status, note);
        }
        self.flavite = Some(flavite);
        if self.settings_valid && conflicts.is_empty() && self.appearance.last_error.is_none() {
            self.status = "Vault ready".into();
        }
        // Graph pins load once through the API at attach (TD-11): a decode
        // or read failure is an honest report, never an overwrite.
        if let Some(flavite) = self.flavite.as_ref() {
            match flavite.read_graph_pins() {
                Ok(stored) => {
                    let observed = self.graph.pins.observe_read(stored);
                    if let Err(error) = observed {
                        self.status =
                            format!("{} — graph pins could not be read: {error}", self.status);
                    }
                }
                Err(error) => {
                    self.status = format!("Vault ready (graph pins unavailable: {error})");
                }
            }
        }
        // RFC-0004 W-D: the catalog arrives as a ticketed page, so the
        // saved-layout restore (which validates membership against the
        // catalog) defers to the first terminal acquisition outcome.
        self.restore_pending = true;
        self.request_catalog_refresh();
        crate::host::graph::sync_hosts(self);
        self.refresh_semantics();
        // Device-local launch memory (W10): best effort, never a gate for
        // vault usability.
        // Best effort: a missing data directory never blocks the vault.
        let _ = flavite_api::write_last_vault_device(&self.root);
    }

    /// Restore the device-local workspace layout (W06): decode, plan, and
    /// apply through one atomic reducer transaction; note tabs reopen
    /// through their normal open flights. Every failure is an honest
    /// report — never a default overwrite.
    fn restore_workspace_layout(&mut self) {
        use flavite_api::UserStateKind;
        let Some(flavite) = &self.flavite else {
            return;
        };
        let uuid = flavite.vault_ref().uuid.to_string();
        let stored = match flavite.read_user_state(UserStateKind::WorkspaceLayouts) {
            Ok(Some(stored)) => stored,
            Ok(None) => {
                self.status = "Vault ready (no saved workspace layout)".into();
                return;
            }
            Err(error) => {
                self.status = format!("Workspace layout not restored: {error}");
                return;
            }
        };
        self.persistence.observe_generation(stored.generation);
        let decoded = match flavite_workspace::decode_workspace(&stored.payload, &uuid) {
            Ok(decoded) => decoded,
            Err(error) => {
                self.status = format!("Workspace layout not restored: {error}");
                return;
            }
        };
        let catalog: std::collections::BTreeSet<String> = self
            .catalog
            .iter()
            .filter_map(|entry| entry.path.to_str().map(str::to_owned))
            .collect();
        let plan = match flavite_workspace::plan_restore(&decoded, &catalog) {
            Ok(plan) => plan,
            Err(error) => {
                self.status = format!("Workspace layout not restored: {error:?}");
                return;
            }
        };
        let revision = self.workspace.revision();
        let action = LayoutAction::RestoreWorkspace {
            expected_revision: revision,
            tree: plan.tree.clone(),
            tabs: plan
                .tabs
                .iter()
                .map(|tab| (tab.id, tab.kind.clone()))
                .collect(),
            groups: decoded
                .groups
                .iter()
                .filter_map(|group| {
                    let id = group.id.parse::<u128>().ok()?;
                    let members: Vec<TabId> = group
                        .tabs
                        .iter()
                        .filter_map(|tab| tab.parse::<u128>().ok().map(TabId::from_raw))
                        .collect();
                    let active = group.active.parse::<u128>().ok().map(TabId::from_raw)?;
                    Some((flavite_workspace::GroupId::from_raw(id), members, active))
                })
                .collect(),
            focus: plan.focus,
            maximized: plan.maximized,
            closed_history: plan.closed_history.clone(),
        };
        // The restore is the persisted truth, not a user-initiated split:
        // it bypasses the interactive fits gate and goes through the same
        // atomic reduce/validate/commit path.
        let outcome = flavite_workspace::reduce(&self.workspace, action);
        if let flavite_workspace::Outcome::Rejected(error) = &outcome {
            self.status = format!("Workspace layout not restored: {error}");
            return;
        }
        self.commit(outcome);
        let missing = plan
            .missing_notes
            .iter()
            .map(|locator| locator.components().join("/"))
            .collect::<Vec<_>>()
            .join(", ");
        let dropped = plan.report.len();
        let mut report = String::from("Workspace layout restored");
        if !missing.is_empty() {
            report.push_str(&format!("; missing notes: {missing}"));
        }
        if dropped > 0 {
            report.push_str(&format!("; {dropped} entries dropped"));
        }
        self.status = report;
    }

    /// Save the workspace layout immediately (W06: explicit flush with a
    /// durability acknowledgement in the status line). The flush always
    /// attempts the write: the debounce governs the autosave, never the
    /// acknowledgement, so "saved" is only reported after a durable receipt.
    pub fn save_layout_now(&mut self) {
        let request = self.persistence.flush_request();
        self.write_layout(request);
        match self.persistence.last_error() {
            None => {
                self.status = format!(
                    "Workspace layout saved (generation {})",
                    self.persistence.generation()
                )
            }
            Some(failure) => {
                self.status = format!("Workspace layout not saved: {failure:?}");
            }
        }
    }

    /// Reset the device-local layout: the next save stores the current
    /// default shell; notes are untouched (FR-17).
    pub fn reset_layout(&mut self) {
        self.named_layouts.clear();
        self.persistence
            .mark_dirty(Instant::now() - crate::host::persistence::LAYOUT_SAVE_DEBOUNCE);
        self.persist_workspace_layout();
        self.status = "Workspace layout reset; notes are untouched".into();
    }

    /// Persist the device-local workspace layout when the autosave is due
    /// (W06: debounce, receipts, retry; independent of note saves).
    fn persist_workspace_layout(&mut self) {
        self.write_layout(self.persistence.due(Instant::now()));
    }

    /// Write the device-local layout under this save request (W06).
    /// Failures are recorded on the persistence host, never silent.
    fn write_layout(&mut self, request: crate::host::persistence::SaveRequest) {
        use flavite_api::UserStateKind;
        let crate::host::persistence::SaveRequest::Write {
            expected_generation,
        } = request
        else {
            return;
        };
        let Some(flavite) = &self.flavite else {
            return;
        };
        let uuid = flavite.vault_ref().uuid.to_string();
        let persisted = flavite_workspace::persist_workspace(&self.workspace, &uuid);
        let named = self.named_layouts.clone();
        let persisted = flavite_workspace::PersistedWorkspace {
            named_layouts: named,
            ..persisted
        };
        let bytes = match flavite_workspace::encode_workspace(&persisted) {
            Ok(bytes) => bytes,
            Err(error) => {
                self.persistence.record_failure(
                    crate::host::persistence::PersistenceFailure::Failed {
                        message: error.to_string(),
                    },
                );
                return;
            }
        };
        // The store is behind &mut; go through the facade with a
        // fresh mutable borrow.
        let write = self
            .flavite
            .as_mut()
            .expect("checked above")
            .write_user_state(UserStateKind::WorkspaceLayouts, expected_generation, &bytes);
        match write {
            Ok(generation) => self.persistence.record_success(generation),
            Err(flavite_api::FlaviteError::UserState(
                flavite_api::user_state::UserStateError::GenerationMismatch { expected, actual },
            )) => {
                self.persistence.record_failure(
                    crate::host::persistence::PersistenceFailure::GenerationMismatch {
                        expected,
                        actual,
                    },
                );
            }
            Err(flavite_api::FlaviteError::UserState(error)) => {
                self.persistence.record_failure(
                    crate::host::persistence::PersistenceFailure::Failed {
                        message: error.to_string(),
                    },
                );
            }
            Err(error) => {
                self.persistence.record_failure(
                    crate::host::persistence::PersistenceFailure::Failed {
                        message: error.to_string(),
                    },
                );
            }
        }
    }

    /// Request a catalog refresh (replaces the pre-S5 blocking load): the
    /// whole read goes out as a QueryService ticket, so this turn pays only
    /// an admission (NFR-4). Results apply in the pump drain path.
    /// Explicit triggers (startup, vault-open completion, create/rename
    /// completions, retry) submit even while a ticket is in flight — the
    /// newest ticket supersedes, so responding to an event can never be
    /// swallowed by an older read that started before the event landed.
    pub(crate) fn request_catalog_refresh(&mut self) {
        self.try_submit_catalog_ticket();
    }

    /// A `VaultChanged` pulse refreshes at most once per throttle window
    /// (§6.4: ≥ 1 s); later pulses coalesce into one retry.
    pub(crate) fn note_vault_changed_pulse(&mut self) {
        if self
            .catalog_last_submit
            .is_none_or(|when| when.elapsed() >= CATALOG_REFRESH_THROTTLE)
        {
            self.try_submit_catalog_ticket();
        } else {
            self.catalog_retry_due = true;
        }
    }

    fn try_submit_catalog_ticket(&mut self) {
        let Some(flavite) = self.flavite.as_ref() else {
            return;
        };
        match flavite.submit_catalog() {
            Ok(_token) => {
                self.catalog_status = CatalogStatus::Loading;
                self.catalog_last_submit = Some(Instant::now());
                self.catalog_retry_due = false;
            }
            Err(flavite_api::QuerySubmitError::Busy) => {
                // No silent loss: keep the last status and snapshot, retry
                // from the pump once the throttle window has passed.
                self.catalog_retry_due = true;
            }
        }
    }

    /// Apply one drained catalog outcome. `Loading` and `Failed` never
    /// clear the existing snapshot (picker keeps serving candidates, no
    /// flicker); `Failed` surfaces visible retryable text.
    pub(crate) fn apply_catalog_outcome(&mut self, result: &flavite_api::ContextQueryResult) {
        match result {
            flavite_api::ContextQueryResult::Catalog {
                entries,
                provenance,
                truncated,
                ..
            } => {
                self.catalog = entries.to_vec();
                // One candidate snapshot feeds the picker, Notes view,
                // dialogs, and open-in-tab flows (W10 de-duplication).
                self.note_candidates = crate::picker::NoteCandidates::from_catalog(&self.catalog);
                self.catalog_status = CatalogStatus::Ready {
                    epoch: provenance.epoch,
                    generation: provenance.generation,
                    truncated: *truncated,
                };
            }
            flavite_api::ContextQueryResult::Failed { message } => {
                self.catalog_status = CatalogStatus::Failed {
                    message: message.clone(),
                };
            }
            other => {
                self.catalog_status = CatalogStatus::Failed {
                    message: format!("unexpected catalog outcome: {other:?}"),
                };
            }
        }
        // The first terminal outcome (page served or honest failure)
        // unblocks the deferred workspace restore.
        self.run_pending_restore();
    }

    fn run_pending_restore(&mut self) {
        if !self.restore_pending {
            return;
        }
        self.restore_pending = false;
        self.restore_workspace_layout();
    }

    /// Consume a coalesced retry once the previous ticket has settled and
    /// the throttle window has passed.
    pub(crate) fn catalog_retry_tick(&mut self) {
        if !self.catalog_retry_due
            || matches!(self.catalog_status, CatalogStatus::Loading)
            || !self
                .catalog_last_submit
                .is_some_and(|when| when.elapsed() >= CATALOG_REFRESH_THROTTLE)
        {
            return;
        }
        self.catalog_retry_due = false;
        self.try_submit_catalog_ticket();
    }

    /// The unified note-candidate snapshot (W10 matcher service).
    #[must_use]
    pub fn note_candidates(&self) -> &crate::picker::NoteCandidates {
        &self.note_candidates
    }

    /// The Notes view filter text for one tab.
    #[must_use]
    pub fn notes_filter(&self, tab: TabId) -> &str {
        self.notes_filters
            .get(&tab)
            .map(String::as_str)
            .unwrap_or("")
    }

    /// The focused note tab, when the focused view is a note.
    #[must_use]
    pub fn focused_note_tab(&self) -> Option<TabId> {
        let tab = self.workspace.focused_tab();
        matches!(
            self.workspace
                .tab(&tab)
                .map(flavite_workspace::ViewSession::kind),
            Some(flavite_workspace::ViewKind::Note(_))
        )
        .then_some(tab)
    }

    /// Request focus for a just-opened Search view's input, once per tab.
    /// The pane mounts one frame after the open commit, so the focus issued
    /// at dispatch time lands before the widget exists; the pump retries.
    fn focus_fresh_search_input(&mut self) -> Option<iced::Task<Message>> {
        let tab = self.focused_group().and_then(|group| {
            self.workspace
                .group(&group)
                .map(|g| g.active())
                .filter(|active| {
                    matches!(
                        self.workspace
                            .tab(active)
                            .map(flavite_workspace::ViewSession::kind),
                        Some(flavite_workspace::ViewKind::Search)
                    )
                })
        })?;
        if self.search_focus_tried.contains(&tab) || self.search.tabs.contains_key(&tab) {
            return None;
        }
        self.search_focus_tried.insert(tab);
        let id = iced::widget::Id::from(format!(
            "{}{}",
            crate::views::search::SEARCH_INPUT_ID_PREFIX,
            tab.raw()
        ));
        Some(iced::widget::operation::focus(id))
    }

    /// The locator of one note tab.
    #[must_use]
    pub fn note_locator(&self, tab: TabId) -> Option<NoteLocator> {
        match self
            .workspace
            .tab(&tab)
            .map(flavite_workspace::ViewSession::kind)
        {
            Some(flavite_workspace::ViewKind::Note(note)) => Some(note.locator().clone()),
            _ => None,
        }
    }

    /// Whether a history restore is currently possible for this note: the
    /// note must be open with a live buffer (restore never opens notes).
    #[must_use]
    pub fn can_restore_history(&self, locator: &NoteLocator) -> bool {
        let Some(flavite) = &self.flavite else {
            return false;
        };
        flavite.is_note_open(&locator.to_path(&self.root))
    }

    /// Menu-initiated selection/caret command (W10 Selection menu): resolve
    /// `usize::MAX` against the live buffer and route through the same
    /// acknowledged interaction path as native selection actions.
    fn selection_command(&mut self, tab: TabId, anchor: usize, head: usize) {
        let Some(view) = self.note_editor_view(tab) else {
            return;
        };
        let incarnation = self.notes.incarnation(tab).unwrap_or(0);
        let resolve = |at: usize| {
            if at == usize::MAX {
                view.text.len()
            } else {
                at
            }
        };
        self.accessible_selection(
            tab,
            incarnation,
            view.revision,
            resolve(anchor),
            resolve(head),
        );
    }

    /// One step of per-view back/forward navigation (W10). The replacement
    /// is a typed reducer transaction; the nav host records the move only
    /// when the transaction committed.
    fn navigate_history(&mut self, forward: bool) {
        let Some(tab) = self.focused_note_tab() else {
            self.status = "Back/forward applies to a note tab".into();
            return;
        };
        let anchor = self
            .workspace
            .tab(&tab)
            .map(|session| session.interaction().viewport_anchor);
        let Some(entry) = (if forward {
            self.nav.go_forward(tab, anchor)
        } else {
            self.nav.go_back(tab, anchor)
        }) else {
            return;
        };
        let target = entry.locator.clone();
        let committed = self.dispatch_layout_result(|revision| LayoutAction::NavigateTab {
            expected_revision: revision,
            tab,
            locator: target,
        });
        // go_back/go_forward already maintain the stacks; only a fresh
        // in-tab replacement (open-in-tab) records a new history branch.
        if committed {
            self.status = if forward {
                "Forward".into()
            } else {
                "Back".into()
            };
        }
    }

    /// Commit the active dialog (W10). Creation and rename run through the
    /// core commands; nothing here touches the filesystem directly.
    fn submit_dialog(&mut self) {
        let Some(dialog) = self.dialog.clone() else {
            return;
        };
        match dialog {
            crate::host::dialogs::Dialog::NewNote { path } => {
                let trimmed = path.trim().to_owned();
                if NoteLocator::from_components(trimmed.split('/')).is_err() {
                    return;
                }
                if self
                    .note_candidates()
                    .all()
                    .iter()
                    .any(|candidate| candidate.path().to_string() == trimmed)
                {
                    self.status = "Create refused: a note already exists at this path".into();
                    return;
                }
                let Some(flavite) = &self.flavite else {
                    return;
                };
                let full = self.root.join(&trimmed);
                let request_id = flavite.command_registry().next_request_id();
                let invocation = Invocation::new(
                    request_id,
                    "note.create",
                    serde_json::json!(full),
                    Origin::Gui,
                );
                match flavite.execute(invocation) {
                    ExecutionResult::Pending { request_id } => {
                        self.operations.accept(request_id, "note.create");
                        self.status = format!("Creating {}…", trimmed);
                        self.dialog = None;
                    }
                    ExecutionResult::Local(LocalResult::Error(error)) => {
                        self.status = format!("Could not create the note: {}", error.message);
                    }
                    ExecutionResult::Rejected(rejected) => {
                        self.status = format!("Create rejected: {:?}", rejected.reason);
                    }
                    _ => {
                        self.status = "Create is unavailable right now".into();
                    }
                }
            }
            crate::host::dialogs::Dialog::Rename { from, to, tier } => {
                let trimmed = to.trim().to_owned();
                let Ok(target) = NoteLocator::from_components(trimmed.split('/')) else {
                    return;
                };
                // Keyboard submit bypasses the dialog button guard; the same
                // validation runs here so a collision never dispatches.
                let collides = target == from
                    || self
                        .note_candidates()
                        .all()
                        .iter()
                        .any(|candidate| candidate.path().to_string() == trimmed);
                if collides {
                    self.status = "Rename refused: the destination already exists".into();
                    return;
                }
                let Some(flavite) = &self.flavite else {
                    return;
                };
                let strategy = match self.settings.rename_behavior {
                    crate::settings::RenameBehavior::Rewrite => "rewrite",
                    crate::settings::RenameBehavior::Alias => "alias",
                    crate::settings::RenameBehavior::None => "none",
                };
                let request_id = flavite.command_registry().next_request_id();
                let invocation = Invocation::new(
                    request_id,
                    "note.rename",
                    serde_json::json!({
                        "from": self.root.join(from.components().join("/")),
                        "to": self.root.join(target.components().join("/")),
                        "strategy": strategy,
                    }),
                    Origin::Gui,
                );
                let _ = tier;
                match flavite.execute(invocation) {
                    ExecutionResult::Pending { request_id } => {
                        self.operations.accept(request_id, "note.rename");
                        self.status = "Renaming with inbound-link rewrite…".into();
                        self.dialog = None;
                    }
                    ExecutionResult::Local(LocalResult::Error(error)) => {
                        self.status = format!("Rename failed: {}", error.message);
                    }
                    _ => {
                        self.status = "Rename is unavailable right now".into();
                    }
                }
            }
            crate::host::dialogs::Dialog::About => {
                self.dialog = None;
            }
            // Preferences edits commit through Message::Preferences as
            // they are made; submit only closes the dialog.
            crate::host::dialogs::Dialog::Preferences => {
                self.dialog = None;
                self.shortcut_capture = None;
            }
            // W12 dialogs resolve through their own messages; Enter runs
            // the primary action where it is safe (never a destructive one).
            crate::host::dialogs::Dialog::RecoveryOffer { path } => {
                self.resolve_recovery(&path, true);
            }
            crate::host::dialogs::Dialog::Merge { .. } => self.apply_prepared_merge(),
            crate::host::dialogs::Dialog::ShutdownTimeout { .. } => {
                let _ = self.update_shutdown(crate::host::shutdown::ShutdownMessage::KeepWaiting);
            }
            crate::host::dialogs::Dialog::Close { .. }
            | crate::host::dialogs::Dialog::Conflict { .. } => {
                self.dialog = None;
                self.shortcut_capture = None;
            }
        }
    }

    /// Open a note picked through the native file dialog, when it lives in
    /// this vault. Foreign paths are refused honestly (never guessed).
    /// Open a note picked through the native file dialog, when it lives in
    /// this vault. Foreign paths are refused honestly (never guessed).
    pub fn open_picked_note(&mut self, picked: Option<std::path::PathBuf>) {
        let Some(path) = picked else {
            return;
        };
        let Ok(relative) = path.strip_prefix(&self.root) else {
            self.status = "That file is outside this vault".into();
            return;
        };
        match NoteLocator::from_relative(relative) {
            Ok(locator) => self.request_open(locator, None),
            Err(error) => self.status = format!("Cannot open that file: {error}"),
        }
    }

    /// Restore one local-history snapshot into the open note buffer (W10):
    /// one guarded, undoable Session edit; disk is written only on save.
    fn restore_history_snapshot(&mut self, tab: TabId, hash: &str) {
        let Some(locator) = self
            .workspace
            .tab(&tab)
            .and_then(|session| match session.kind() {
                flavite_workspace::ViewKind::History(locator) => Some(locator.clone()),
                _ => None,
            })
        else {
            return;
        };
        let Some(flavite) = &self.flavite else {
            return;
        };
        // The API owns open-buffer lookup (open_source_view), keyed by the
        // vault-relative path: the restore targets the exact live buffer and
        // revision it reports.
        let relative = locator.to_path(std::path::Path::new(""));
        let Some(view) = flavite.open_source_view(&relative) else {
            self.status = "Open the note before restoring a snapshot".into();
            return;
        };
        let request_id = flavite.command_registry().next_request_id();
        let invocation = Invocation::new(
            request_id,
            "note.restore_history",
            serde_json::json!({
                "path": self.root.join(&relative),
                "buffer_id": view.buffer_id,
                "revision": view.revision,
                "hash": hash,
            }),
            Origin::Gui,
        );
        match flavite.execute(invocation) {
            ExecutionResult::Local(LocalResult::Success(success)) => {
                let revision = success
                    .data
                    .get("revision")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default();
                self.status = format!("Snapshot restored at revision {revision} (undoable)");
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                self.status = format!("Restore failed: {}", error.message);
            }
            ExecutionResult::Rejected(rejected) => {
                self.status = format!("Restore rejected: {:?}", rejected.reason);
            }
            ExecutionResult::ActionRequired { code, .. } => {
                self.status = format!("Restore needs attention: {code}");
            }
            _ => {
                self.status = "Restore is unavailable right now".into();
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
    /// The live source-format descriptor for a tab's buffer (§6.10).
    pub fn note_source_format(&self, tab: TabId) -> Option<flavite_api::SourceFormat> {
        let buffer = self.notes.state(&tab)?.buffer?;
        self.flavite
            .as_ref()?
            .get_shared_buffer_view(flavite_buffer::BufferId::from(buffer), None)
            .ok()
            .map(|view| view.source_format)
    }

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
    pub fn note_buffer_state(&self, tab: TabId) -> Option<flavite_api::BufferStatus> {
        self.note_editor_view(tab).map(|view| view.state)
    }

    /// Visible, identity-safe description of the insertion authority.
    #[must_use]
    pub fn writing_target_label(&self) -> String {
        if self.workspace.writing_target().is_none() {
            return "Writing target: none — focus an editor caret".into();
        }
        // One validation authority: the same predicate the pump revalidates
        // and the insertion gesture consumes (no divergent validity views).
        let Some((tab, ..)) = self.valid_writing_target() else {
            return "Writing target: invalid — place the caret again".into();
        };
        let title = self
            .notes
            .locator_of(tab)
            .map(|locator| locator.title().to_owned())
            .unwrap_or_else(|| "note".into());
        format!("Writing target: {title}")
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

    /// The catalog's last acquisition error, if any (visible text pattern
    /// backed by the ticketed status machine).
    #[must_use]
    pub fn catalog_error(&self) -> Option<&str> {
        match &self.catalog_status {
            CatalogStatus::Failed { message } => Some(message),
            _ => None,
        }
    }

    /// The catalog acquisition state (ticketed; RFC-0004 §6.4).
    #[must_use]
    pub const fn catalog_status(&self) -> &CatalogStatus {
        &self.catalog_status
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
                self.project_geometry();
                let effects = commit.effects;
                self.apply_effects(&effects);
                crate::host::graph::sync_hosts(self);
                self.refresh_semantics();
                // Every committed layout change marks the device-local
                // layout dirty (W06 autosave).
                self.persistence.mark_dirty(Instant::now());
            }
            Outcome::Rejected(error) => {
                self.status = format!("Workspace: {error}");
            }
        }
    }

    /// Build and reduce one layout transaction at the current revision.
    pub fn dispatch_layout(&mut self, build: impl FnOnce(u64) -> LayoutAction) {
        self.dispatch_layout_result(build);
    }

    /// [`Self::dispatch_layout`] reporting whether the transaction committed.
    pub fn dispatch_layout_result(&mut self, build: impl FnOnce(u64) -> LayoutAction) -> bool {
        if self.has_composing_view() {
            self.status = "Finish or cancel text composition before changing the layout".into();
            return false;
        }
        let revision = self.workspace.revision();
        let action = build(revision);
        if !self.layout_fits(&action) {
            self.status = "There is not enough space for that split".into();
            return false;
        }
        let outcome = flavite_workspace::reduce(&self.workspace, action);
        let committed = matches!(outcome, Outcome::Committed(_));
        self.commit(outcome);
        committed
    }

    fn has_composing_view(&self) -> bool {
        self.workspace.tabs().any(|(_, view)| {
            view.interaction().composition == flavite_workspace::CompositionState::Active
        })
    }

    /// Route one graph widget message through the host after stale-guard
    /// validation (replaced views and hidden tabs never reach the model).
    fn handle_graph_action(
        &mut self,
        tab: TabId,
        incarnation: u64,
        message: crate::graph::renderer::GraphMessage,
        key: flavite_graph::RenderKey,
    ) {
        // Overlays suppress graph pointer input. (The edit/read gate is for
        // note tabs: a Graph view is never a Note, so `is_edit` would drop
        // every action — pan, zoom, selection, insertion — unconditionally.)
        if self.picker.open || self.palette_open || self.strip_menu.is_some() {
            return;
        }
        if !self.is_tab_visible(tab) {
            return;
        }
        let Some(session) = self.workspace.tab(&tab) else {
            return;
        };
        if !matches!(session.kind(), flavite_workspace::ViewKind::Graph(_)) {
            return;
        }
        if session.interaction().incarnation != incarnation {
            self.status = "Ignored input from a replaced graph view".into();
            return;
        }
        if self.graph.state(tab).is_none_or(|s| {
            s.filter.expanded || s.updating || s.stale || s.geometry_key != Some(key)
        }) || self
            .flavite
            .as_ref()
            .is_none_or(|api| !api.graph_is_current())
        {
            self.graph.stale_discards_total += 1;
            return;
        }
        crate::host::graph::handle_message(self, tab, message);
    }

    fn apply_effects(&mut self, effects: &[Effect]) {
        for effect in effects {
            match effect {
                Effect::ViewAttached { tab, kind } => {
                    if let flavite_workspace::ViewKind::Graph(view) = kind {
                        self.graph.attach(*tab, view);
                    }
                    if let flavite_workspace::ViewKind::History(locator) = kind {
                        crate::views::history::load(self, *tab, locator);
                    }
                    if let flavite_workspace::ViewKind::Note(note) = kind {
                        let locator = note.locator().clone();
                        self.nav.seed(*tab, locator.clone());
                        // In-place navigation (W10 NavigateTab) re-attaches a
                        // tab that already holds a lease for the replaced
                        // note; release the old lease before reserving.
                        if self.notes.locator_of(*tab).is_some() {
                            self.input_chains.remove(tab);
                            self.notes.detach(*tab);
                        }
                        let (tab, incarnation, decision) = self.notes.attach(*tab, locator.clone());
                        if let Err(error) =
                            self.workspace.install_view_incarnation(tab, incarnation)
                        {
                            self.status = format!("Could not install editor incarnation: {error}");
                            self.notes.detach(tab);
                            continue;
                        }
                        match decision {
                            AttachDecision::IssueOpen => self.open_note_tab(tab, &locator),
                            // The document's buffer is already installed: the
                            // reopened lease resolves immediately.
                            AttachDecision::Ready => {
                                self.dispatch_layout(|revision| LayoutAction::NoteLoadReady {
                                    expected_revision: revision,
                                    tab,
                                });
                            }
                            // A shared flight completion installs the buffer
                            // for every waiting lease at once.
                            AttachDecision::FlightPending => {}
                        }
                    }
                }
                Effect::ViewDetached { tab, .. } => {
                    self.input_chains.remove(tab);
                    self.notes.detach(*tab);
                    self.contexts.detach(*tab);
                    self.graph.detach(*tab);
                    self.search.detach(*tab);
                    self.search_focus_tried.remove(tab);
                    self.history.detach(*tab);
                    self.nav.detach(*tab);
                    self.notes_filters.remove(tab);
                    if let Some(api) = self.flavite.as_ref() {
                        api.graph_release_scope(tab.raw());
                    }
                }
                Effect::NoteLoadChanged { .. } => {}
                Effect::ContextRebound { .. } => {}
            }
        }
        self.reconcile_contexts();
    }

    /// Re-resolve every context tool against the router and submit queries
    /// for changed sources. Busy admission is recorded, never lost.
    pub(crate) fn reconcile_contexts(&mut self) {
        let stale: Vec<_> = self
            .workspace
            .tabs()
            .filter_map(|(tab, _)| match self.contexts.state(tab) {
                Some(crate::host::context::ToolState::Ready(
                    crate::host::context::ToolContent::Document(doc),
                )) if !crate::host::reading::current(self, doc) => Some(*tab),
                _ => None,
            })
            .collect();
        for tab in stale {
            self.contexts.invalidate(tab);
        }
        let Some(flavite) = &self.flavite else {
            return;
        };
        for (tab, session) in self.workspace.tabs() {
            if !self.is_tab_visible(*tab)
                && !matches!(session.kind(), flavite_workspace::ViewKind::Backlinks(_))
            {
                self.contexts.detach(*tab);
                continue;
            }
            let backlinks = match session.kind() {
                flavite_workspace::ViewKind::Preview(_)
                | flavite_workspace::ViewKind::Outline(_)
                | flavite_workspace::ViewKind::Properties(_) => false,
                flavite_workspace::ViewKind::Backlinks(_) => true,
                _ => continue,
            };
            if self.contexts.incarnation_of(tab).is_none() {
                self.contexts
                    .attach(*tab, session.interaction().incarnation, backlinks);
            }
            let Some(binding) = self.workspace.binding_of(tab) else {
                continue;
            };
            let source = flavite_workspace::ContextRouter::resolve(&self.workspace, binding);
            if let Some(job) = self.contexts.rebind(*tab, &source) {
                let payload = if job.backlinks {
                    flavite_api::QueryPayload::Backlinks {
                        path: job.path.clone(),
                    }
                } else {
                    flavite_api::QueryPayload::Read {
                        path: job.path.clone(),
                        expected_buffer: None,
                    }
                };
                match flavite.query_service().submit(
                    job.tab.raw(),
                    job.incarnation,
                    job.binding_sequence,
                    payload,
                ) {
                    Ok(token) => {
                        self.contexts.submitted(
                            job.tab,
                            job.binding_sequence,
                            token.request_sequence,
                        );
                    }
                    Err(_) => self.contexts.busy(job.tab, job.binding_sequence),
                }
            }
        }
    }

    fn handle_editor_action(&mut self, tab: TabId, incarnation: u64, action: EditorAction) {
        if self.picker.open
            || self.palette_open
            || self.strip_menu.is_some()
            || !crate::host::reading::is_edit(self, tab)
        {
            return;
        }
        if !self.is_tab_visible(tab) {
            return;
        }
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
                    self.input_chains.remove(&tab);
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
                let previous_buffer = interaction.buffer_id;
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
                if snapshot.focused {
                    if self.workspace.focused_tab() != tab {
                        self.dispatch_layout(|revision| LayoutAction::ActivateTab {
                            expected_revision: revision,
                            tab,
                        });
                    }
                    self.workspace.release_editor_focus(Some(tab));
                }
                if let Err(error) = self
                    .workspace
                    .acknowledge_interaction(interaction, &current.text)
                {
                    self.status = format!("Editor state was not accepted: {error}");
                } else {
                    if snapshot.focused
                        && let Some(find) = self.reading.find.get_mut(&tab)
                    {
                        find.focused = false;
                    }
                    // Context tools resolve by anchor identity: a caret or
                    // selection move within the same buffer does not change
                    // what they display. Re-resolve only when the view took
                    // a different buffer (first acknowledgement, retarget,
                    // or a replaced view).
                    if previous_buffer != Some(snapshot.buffer_id) {
                        self.reconcile_contexts();
                    }
                }
            }
            EditorAction::Navigate {
                buffer_id,
                expected_revision,
                navigation,
            } => {
                let Some(source) = self.note_editor_view(tab) else {
                    self.status = "Edit link: the source note is no longer open".into();
                    return;
                };
                if source.buffer_id != buffer_id || source.revision != expected_revision {
                    self.status = "Edit link: the note changed; click the link again".into();
                    return;
                }
                let resolution = self.flavite.as_ref().and_then(|flavite| {
                    flavite
                        .resolve_note_target(navigation.target.clone(), None)
                        .ok()
                });
                let Some(resolution) = resolution else {
                    self.status =
                        "Edit link: the index is not ready yet; try again after indexing".into();
                    return;
                };
                if resolution.is_ambiguous {
                    self.status =
                        "Edit link: several notes match; pick one in the quick switcher".into();
                    return;
                }
                let Some(target) = resolution.note else {
                    if resolution.is_unresolved && navigation.create_if_unresolved {
                        // Explicit creation on unresolved activation (W10):
                        // the dialog pre-fills the canonical path and the
                        // user confirms; nothing is created implicitly.
                        self.dialog = Some(crate::host::dialogs::offer_create_for_link(
                            &navigation.target,
                        ));
                    } else {
                        self.status = "Edit link: the target is not an indexed note".into();
                    }
                    return;
                };
                // The resolver already proved the target entity; decode its
                // native path identity directly instead of consulting the
                // bounded display catalog, which may not contain the note.
                let opened = self.flavite.as_ref().map(|flavite| {
                    flavite
                        .decode_note_path(target.path_native)
                        .map_err(|error| error.to_string())
                        .and_then(|path| NoteLocator::from_relative(&path))
                });
                match opened {
                    Some(Ok(locator)) => self.request_open(locator, self.group_of(tab)),
                    Some(Err(error)) => {
                        self.status =
                            format!("Edit link: the resolved path cannot be opened here: {error}");
                    }
                    None => {
                        self.status =
                            "Edit link: the index is not ready yet; try again after indexing"
                                .into();
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
                // Any different-origin mutation cancels an outstanding
                // insertion gesture: no auto commit after cancellation.
                self.linking.invalidate_if_source_changed(None);
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
                    ExecutionResult::ActionRequired { code, data, .. } => {
                        self.status = format!("Edit needs attention: {code}");
                        if code == "encoding-unencodable" {
                            // §6.10: an unencodable draft stays intact; the
                            // repair (explicit convert) is offered in view.
                            let tab = self.workspace.focused_tab();
                            let path = self
                                .note_editor_view(tab)
                                .map(|view| view.path.clone())
                                .unwrap_or_default();
                            let payload =
                                data.as_ref().and_then(serde_json::Value::as_str).map(str::to_owned);
                            self.encoding.record(tab, code, payload, path);
                        }
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
            ExecutionResult::ActionRequired { code, data, .. } => {
                // §6.10: an undecodable/ambiguous source is a typed encoding
                // decision, surfaced per tab instead of a blank editor.
                if code == "encoding-choice-required" {
                    let payload = data.as_ref().and_then(serde_json::Value::as_str).map(str::to_owned);
                    self.encoding.record(tab, code.to_owned(), payload, path);
                    self.status = "Encoding choice required: pick an explicit format".into();
                } else {
                    self.status = format!("Open needs attention: {code}");
                }
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
    pub(crate) fn request_open(&mut self, locator: NoteLocator, group: Option<GroupId>) {
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
        // A raw pointer message that changed nothing observable (the
        // overwhelmingly common case while hovering or drag-selecting)
        // must not rebuild the accessibility map; the editor widget
        // redraws its own selection.
        let pointer_only = matches!(message, Message::Mouse(_));
        self.pointer_feedback = false;
        let task = self.update_inner(message);
        if !pointer_only || self.pointer_feedback {
            self.refresh_semantics();
        }
        #[cfg(feature = "ui-test-support")]
        crate::harness::observe(self);
        task
    }

    fn update_inner(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EditorInput {
                tab,
                incarnation,
                input,
            } => return crate::host::input::handle(self, tab, incarnation, input),
            Message::Encoding(message) => {
                let task = crate::host::encoding::handle(self, message);
                self.refresh_semantics();
                return task;
            }
            message @ (Message::FindOpen(_)
            | Message::FindClose(_)
            | Message::FindQuery { .. }
            | Message::FindNext { .. }
            | Message::ToggleNoteMode(_)
            | Message::RetryReading(_)
            | Message::ToggleSourcePreview(_)
            | Message::ReadControlBounds { .. }
            | Message::ReadBlockBounds { .. }
            | Message::ReadScrolled { .. }
            | Message::ReadActivate { .. }
            | Message::OutlineNavigate { .. }
            | Message::StructureEdit { .. }
            | Message::PropertyDraft { .. }
            | Message::PropertyApply { .. }
            | Message::ExternalOpened(_)) => return crate::host::reading::handle(self, message),
            Message::AppMenu => {
                // Opening a menu disables editor input; never interrupt an
                // active composition (D-03). Entry availability is rechecked
                // by the shared command resolver (D-16).
                if !self.has_composition()
                    && let Some(group) = self
                        .focused_group()
                        .or_else(|| self.workspace.group_order().first().copied())
                {
                    self.strip_menu = Some((group, crate::workspace::StripMenu::App));
                    self.menu_selected = 0;
                }
            }
            Message::ResizeBegin(split) => {
                if !self.has_composing_view() {
                    self.resizing_split = Some(split);
                }
            }
            Message::WorkspaceBounds(bounds) => {
                if self.workspace_bounds != bounds {
                    self.workspace_bounds = bounds;
                    self.window_geometry_dirty = true;
                }
            }
            Message::GroupBounds(group, bounds) => {
                if self.workspace.group(&group).is_some() {
                    self.group_bounds.insert(group, bounds);
                }
            }
            Message::EditorBounds(tab, bounds) => {
                if self.workspace.tab(&tab).is_some() {
                    self.editor_bounds.insert(tab, bounds);
                }
            }
            Message::MenuClose => self.strip_menu = None,
            Message::CancelGesture => {
                self.gesture.cancel();
                self.drag_revision = None;
                self.resize_preview.clear();
                self.resizing_split = None;
                self.strip_menu = None;
                // Escape also restores an in-flight node drag to its prior
                // position and drops click candidates (never a pin write).
                crate::host::graph::cancel_gestures(self);
            }
            Message::MenuNext(forward) => {
                if let Some((group, menu)) = self.strip_menu {
                    let entries = crate::workspace::tab_strip::menu_entries(self, group, menu);
                    let count = entries.len();
                    for _ in 0..count {
                        self.menu_selected = if forward {
                            (self.menu_selected + 1) % count
                        } else {
                            (self.menu_selected + count - 1) % count
                        };
                        if entries[self.menu_selected].action.is_some() {
                            break;
                        }
                    }
                }
            }
            Message::MenuFocus(index) => {
                self.menu_selected = index;
            }
            Message::MenuPick(index) => {
                if let Some((group, menu)) = self.strip_menu {
                    let entry = crate::workspace::tab_strip::menu_entries(self, group, menu)
                        .get(index)
                        .cloned();
                    if let Some(action) = entry.and_then(|e| e.action) {
                        self.strip_menu = None;
                        return self.update(action);
                    }
                }
            }
            Message::ActivateTab(tab) => {
                self.strip_menu = None;
                self.dispatch_layout(|revision| LayoutAction::ActivateTab {
                    expected_revision: revision,
                    tab,
                });
            }
            // W12: the tab-strip close button goes through the close plan
            // (final-view lease accounting, decisions, core policy).
            Message::CloseTab(tab) => {
                let _ = self.request_close_tab(tab);
            }
            Message::AccessibleSelection {
                tab,
                incarnation,
                revision,
                anchor,
                head,
            } => self.accessible_selection(tab, incarnation, revision, anchor, head),
            Message::SelectionCommand { tab, anchor, head } => {
                self.selection_command(tab, anchor, head);
            }
            Message::Keyboard(event) => {
                if let Some(message) = host_commands::keyboard_message(self, event) {
                    return self.update(message);
                }
            }
            Message::Mouse(event) => self.handle_mouse(event),
            Message::RequestCloseTab(tab) => {
                let _ = self.request_close_tab(tab);
            }
            Message::RequestCloseGroup(group) => {
                let _ = self.request_close_group(group);
            }
            Message::CloseDecision(decision) => {
                let _ = self.apply_close_decision(decision);
            }
            Message::CloseKeepOpen => {
                // W12: keeping the note open during a quit cancels the whole
                // quit schedule, not just the close plan.
                if self.shutdown.is_some() {
                    self.cancel_shutdown();
                } else {
                    self.cancel_close();
                }
            }
            Message::CloseDiscardArm(arm) => {
                if arm {
                    let revision = self
                        .closing
                        .as_ref()
                        .and_then(|plan| self.note_editor_view(plan.tab))
                        .map(|view| view.revision);
                    if let (Some(plan), Some(revision)) = (self.closing.as_mut(), revision) {
                        plan.discard_armed = Some(revision);
                        // Arming re-presents the document to the user (D-07):
                        // the plan is valid for the revision they now see,
                        // otherwise a re-armed discard could never proceed.
                        plan.captured_revision = revision;
                    }
                } else if let Some(plan) = self.closing.as_mut() {
                    plan.discard_armed = None;
                }
            }
            Message::ConflictResolve { tab, resolution } => {
                self.resolve_conflict(tab, &resolution);
            }
            Message::MergeApply => self.apply_prepared_merge(),
            Message::RecoveryResolve { path, restore } => {
                self.resolve_recovery(&path, restore);
            }
            Message::CloseDiscardConfirm => {
                let revision = self
                    .closing
                    .as_ref()
                    .and_then(|plan| plan.discard_armed)
                    .unwrap_or_default();
                let _ = self.apply_close_decision(crate::host::closing::CloseDecision::Discard {
                    expected_revision: revision,
                });
            }
            Message::Shutdown(message) => {
                let _ = self.update_shutdown(message);
            }
            Message::SystemTheme(mode) => self.appearance.system_theme(mode),
            Message::Preferences(update) => {
                // Preference edits persist inline; the task is always none.
                let _ = self.update_preferences(update);
            }
            Message::WindowEvent(iced::window::Event::CloseRequested) => {
                self.begin_shutdown();
            }
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
                self.drag_revision = None;
                self.resize_preview.clear();
                self.resizing_split = None;
            }
            Message::WindowEvent(_) => {}
            Message::Pump => {
                self.pump();
                self.catalog_retry_tick();
                self.refresh_semantics();
                // W12: the quit schedule advances with the pump; the exit
                // task closes the window only from ReadyToExit.
                if let Some(task) = self.shutdown_step() {
                    return task;
                }
                if let Some(task) = self.focus_fresh_search_input() {
                    return task;
                }
                return crate::host::reading::scroll_pending(self);
            }
            Message::Layout(action) => {
                // W12: while the quit schedule is being resolved the layout
                // is frozen; the close machinery mutates it exclusively.
                if self.shutdown.is_some() {
                    self.status =
                        "Quitting: resolve the quit (decide or cancel) before changing the layout"
                            .into();
                } else if !self.has_composing_view() && self.layout_fits(&action) {
                    let outcome = flavite_workspace::reduce(&self.workspace, action);
                    self.commit(outcome);
                } else {
                    self.status =
                        "This layout change needs more space or an unfinished composition to end"
                            .into();
                }
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
                        flavite_workspace::ViewKind::Graph(_) => LayoutAction::OpenGraph {
                            expected_revision: revision,
                            group,
                        },
                        flavite_workspace::ViewKind::Preview(_) => LayoutAction::OpenContext {
                            expected_revision: revision,
                            view: flavite_workspace::ContextViewKind::Preview,
                            binding: flavite_workspace::ContextBinding::FollowExplorationOrDocument,
                            group,
                        },
                        flavite_workspace::ViewKind::Outline(_) => LayoutAction::OpenContext {
                            expected_revision: revision,
                            view: flavite_workspace::ContextViewKind::Outline,
                            binding: flavite_workspace::ContextBinding::FollowDocument,
                            group,
                        },
                        flavite_workspace::ViewKind::Properties(_) => LayoutAction::OpenContext {
                            expected_revision: revision,
                            view: flavite_workspace::ContextViewKind::Properties,
                            binding: flavite_workspace::ContextBinding::FollowDocument,
                            group,
                        },
                        flavite_workspace::ViewKind::Backlinks(_) => LayoutAction::OpenContext {
                            expected_revision: revision,
                            view: flavite_workspace::ContextViewKind::Backlinks,
                            binding: flavite_workspace::ContextBinding::FollowDocument,
                            group,
                        },
                        flavite_workspace::ViewKind::Search => LayoutAction::OpenSearch {
                            expected_revision: revision,
                            group,
                        },
                        // History opens for a specific note through the
                        // note tab's group actions, never generically.
                        flavite_workspace::ViewKind::History(_) => LayoutAction::OpenEmpty {
                            expected_revision: revision,
                            group,
                        },
                    });
                }
            }
            Message::OpenNote { locator, group } => self.request_open(locator, group),
            Message::GraphAction {
                tab,
                incarnation,
                message,
                key,
            } => self.handle_graph_action(tab, incarnation, message, key),
            Message::GraphNodeCommand {
                tab,
                incarnation,
                key,
                command,
            } => {
                if self
                    .workspace
                    .tab(&tab)
                    .is_some_and(|s| s.interaction().incarnation == incarnation)
                    && self
                        .graph
                        .state(tab)
                        .is_some_and(|s| !s.updating && !s.stale && s.geometry_key == Some(key))
                    && self
                        .flavite
                        .as_ref()
                        .is_some_and(flavite_api::Flavite::graph_is_current)
                {
                    crate::host::graph::command(self, tab, command);
                } else {
                    self.graph.stale_discards_total += 1;
                }
            }
            Message::GraphCommand { tab, command } => {
                if self.picker.open || self.palette_open || self.strip_menu.is_some() {
                    return Task::none();
                }
                crate::host::graph::command(self, tab, command);
            }
            Message::InspectGraphOccurrence {
                tab,
                incarnation,
                key,
                index,
            } => crate::host::occurrences::inspect(self, tab, incarnation, key, index),
            Message::GraphFilterInput {
                tab,
                incarnation,
                field,
                value,
            } => {
                crate::graph::filters::input(self, tab, incarnation, field, value);
            }
            Message::GraphFilterBounds { tab, field, bounds } => {
                if let Some(state) = self.graph.state_mut(tab) {
                    state.filter_bounds.insert(field, bounds);
                }
            }
            Message::GraphBounds(tab, rect) => {
                if let Some(state) = self.graph.state_mut(tab) {
                    state.bounds = Some(rect);
                }
            }
            Message::GroupMenu(group, menu) => {
                if self.has_composing_view() {
                    return Task::none();
                }
                self.dispatch_layout(|revision| LayoutAction::FocusGroup {
                    expected_revision: revision,
                    group,
                });
                self.menu_selected = crate::workspace::tab_strip::menu_entries(self, group, menu)
                    .iter()
                    .position(|entry| entry.action.is_some())
                    .unwrap_or(0);
                self.strip_menu = match self.strip_menu {
                    Some((open_group, open_menu)) if open_group == group && open_menu == menu => {
                        None
                    }
                    _ => Some((group, menu)),
                };
            }
            Message::TabPressed(tab) => {
                if self.has_composing_view() || self.picker.open || self.palette_open {
                    return Task::none();
                }
                self.strip_menu = None;
                self.gesture.press(tab);
                if let Some(pointer) = self.pointer {
                    self.gesture.moved(pointer);
                }
                let revision = self.workspace.revision();
                let outcome = flavite_workspace::reduce(
                    &self.workspace,
                    LayoutAction::ActivateTab {
                        expected_revision: revision,
                        tab,
                    },
                );
                self.commit(outcome);
                self.drag_revision = Some(self.workspace.revision());
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
                    replace_tab: None,
                };
                if self.catalog.is_empty() {
                    self.request_catalog_refresh();
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
            Message::RefreshCatalog => self.request_catalog_refresh(),
            Message::TopMenu(menu) => {
                // The standard menu bar never interrupts composition (D-03).
                if !self.has_composition() {
                    let group = self
                        .focused_group()
                        .or_else(|| self.workspace.group_order().first().copied());
                    if let Some(group) = group {
                        self.strip_menu = Some((group, menu));
                        self.menu_selected = 0;
                    }
                }
            }
            // ---- W10: Notes view ----
            Message::NotesFilter { tab, value } => {
                if self.workspace.tab(&tab).is_some() {
                    self.notes_filters.insert(tab, value);
                } else {
                    self.notes_filters.remove(&tab);
                }
            }
            // ---- W10: Search view ----
            Message::SearchQuery { tab, value } => self.search.edited(tab, value),
            Message::SearchSubmitNow { tab } => {
                self.search.submit_now(tab);
            }
            Message::SearchLoadMore { tab } => self.search.load_more(tab),
            // ---- W10: History view ----
            Message::HistoryRefresh { tab } => {
                if let Some(locator) =
                    self.workspace
                        .tab(&tab)
                        .and_then(|session| match session.kind() {
                            flavite_workspace::ViewKind::History(locator) => Some(locator.clone()),
                            _ => None,
                        })
                {
                    crate::views::history::load(self, tab, &locator);
                }
            }
            Message::HistoryRestore { tab, hash } => {
                self.restore_history_snapshot(tab, &hash);
            }
            // ---- W10: navigation ----
            Message::NavigateBack => self.navigate_history(false),
            Message::NavigateForward => self.navigate_history(true),
            // ---- W10: dialogs ----
            Message::NewNoteDialog => {
                if !self.has_composition() {
                    self.dialog = Some(crate::host::dialogs::Dialog::NewNote {
                        path: String::new(),
                    });
                    return iced::widget::operation::focus(crate::host::dialogs::NEW_NOTE_INPUT_ID);
                }
            }
            Message::RenameDialog => {
                if !self.has_composition()
                    && let Some(tab) = self.focused_note_tab()
                    && let Some(from) = self.note_locator(tab)
                {
                    self.dialog = Some(crate::host::dialogs::Dialog::Rename {
                        from,
                        to: String::new(),
                        tier: None,
                    });
                    return iced::widget::operation::focus(crate::host::dialogs::RENAME_INPUT_ID);
                }
            }
            Message::AboutDialog => {
                self.dialog = Some(crate::host::dialogs::Dialog::About);
            }
            Message::OpenNoteDialog => {
                if self.has_composition() {
                    return iced::Task::none();
                }
                return crate::host::dialogs::pick_note_file();
            }
            Message::DialogInput(value) => match &mut self.dialog {
                Some(crate::host::dialogs::Dialog::NewNote { path }) => {
                    *path = value;
                }
                Some(crate::host::dialogs::Dialog::Rename { to, .. }) => {
                    *to = value;
                }
                _ => {}
            },
            Message::DialogSubmit => {
                self.submit_dialog();
            }
            Message::DialogCancel => {
                self.shortcut_capture = None;
                if matches!(
                    self.dialog,
                    Some(crate::host::dialogs::Dialog::Close { .. })
                ) {
                    // W12: cancelling the decision dialog during a quit
                    // cancels the whole quit schedule.
                    if self.shutdown.is_some() {
                        self.cancel_shutdown();
                    } else {
                        self.cancel_close();
                    }
                } else if matches!(
                    self.dialog,
                    Some(crate::host::dialogs::Dialog::ShutdownTimeout { .. })
                ) {
                    self.cancel_shutdown();
                } else {
                    self.dialog = None;
                }
            }
            Message::NotePicked(picked) => {
                self.open_picked_note(picked);
            }
            // ---- W10: start screen ----
            Message::VaultDialog => {
                return crate::host::dialogs::pick_vault_folder();
            }
            Message::VaultPicked(picked) => {
                if let Some(root) = picked {
                    return self.update(Message::StartOpenVault(root));
                }
            }
            Message::VaultPathInput(value) => {
                self.start.path = value;
                self.start.error = None;
            }
            Message::StartOpenVault(root) => {
                if self.flavite.is_some() {
                    return iced::Task::none();
                }
                if !root.is_dir() {
                    self.start.error =
                        Some("That folder does not exist. Create it first, then open it.".into());
                    return iced::Task::none();
                }
                match Flavite::open_root(root, VaultConfig::default()) {
                    Ok(flavite) => {
                        self.start = crate::host::dialogs::StartState::default();
                        self.attach_vault(flavite);
                    }
                    Err(error) => {
                        self.start.error = Some(format!("Could not open that vault: {error}"));
                    }
                }
            }
            Message::ReviewRenameNotice(id) => {
                // Find the notice's from/to pair and open the rename dialog.
                let suggestion = self.notices.iter().find_map(|notice| {
                    if notice.id == id
                        && let flavite_api::Event::RenameSuggested { from, to, tier, .. } =
                            &notice.event
                    {
                        Some((from.clone(), to.clone(), tier.clone()))
                    } else {
                        None
                    }
                });
                if let Some((from, to, tier)) = suggestion
                    && let Ok(from_locator) = NoteLocator::from_relative(&from)
                {
                    self.dialog = Some(crate::host::dialogs::Dialog::Rename {
                        from: from_locator,
                        to: to.display().to_string(),
                        tier: Some(tier.as_str().to_owned()),
                    });
                    if let Some(index) = self.notices.iter().position(|notice| notice.id == id) {
                        self.notices.remove(index);
                        if let Some(flavite) = &self.flavite {
                            flavite.acknowledge_notice(id);
                        }
                    }
                }
            }
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
            Message::OpenContextView { view, group } => {
                let group = group
                    .or_else(|| self.focused_group())
                    .or_else(|| self.workspace.first_group());
                if let Some(group) = group {
                    let binding = match view {
                        flavite_workspace::ContextViewKind::Preview => {
                            flavite_workspace::ContextBinding::FollowExplorationOrDocument
                        }
                        flavite_workspace::ContextViewKind::Backlinks
                        | flavite_workspace::ContextViewKind::Outline
                        | flavite_workspace::ContextViewKind::Properties => {
                            flavite_workspace::ContextBinding::FollowDocument
                        }
                    };
                    self.dispatch_layout(|revision| LayoutAction::OpenContext {
                        expected_revision: revision,
                        view,
                        binding,
                        group,
                    });
                } else {
                    self.status = "Open a group before opening context tools".into();
                }
            }
            Message::SetContextBinding { tab, binding } => {
                self.dispatch_layout(|revision| LayoutAction::SetContextBinding {
                    expected_revision: revision,
                    tab,
                    binding,
                });
            }
            Message::RequeryContext { tab } => {
                self.contexts.detach(tab);
                self.reconcile_contexts();
            }
            Message::OpenNoteFromContext { locator } => self.request_open(locator, None),
            Message::EditSourceFromContext { locator } => {
                self.request_open(locator, None);
                let tab = self.workspace.focused_tab();
                if matches!(self.workspace.tab(&tab).map(|s| s.kind()), Some(flavite_workspace::ViewKind::Note(note)) if note.mode() == flavite_workspace::NoteMode::Read)
                {
                    return self.update(Message::ToggleNoteMode(tab));
                }
            }
            Message::InsertLink { locator } => self.request_link_insert(locator),
            Message::PickerInsert(index) => {
                let matches = self.picker_matches();
                self.picker = Picker::default();
                let Some(entry) = matches.get(index) else {
                    self.status = "The picked note is no longer available".into();
                    return Task::none();
                };
                match flavite_workspace::NoteLocator::from_relative(&entry.path) {
                    Ok(locator) => self.request_link_insert(locator),
                    Err(error) => {
                        self.status = format!("The picked note path cannot be linked: {error}")
                    }
                }
            }
            Message::ConfirmLinkInsert => match self.linking.confirmed() {
                Some((sequence, plan_id)) => {
                    let (serialized, display) = match self.linking.state() {
                        crate::host::linking::GestureState::Ready {
                            serialized_link,
                            target,
                            ..
                        } => (serialized_link.clone(), target.clone()),
                        _ => (String::new(), String::new()),
                    };
                    self.commit_link_insert(sequence, plan_id, serialized, display);
                }
                None => self.status = "No confirmed link plan is outstanding".into(),
            },
            Message::CancelLinkInsert => {
                self.linking.cancel();
                self.status = "Link insertion cancelled; the note is unchanged".into();
            }
        }
        // Geometry is re-solved on layout commits (see `commit`), window
        // size changes, and active resize previews — not on every message.
        // Semantics are refreshed once by `update` on every path.
        if self.window_geometry_dirty || !self.resize_preview.is_empty() {
            self.project_geometry();
            self.window_geometry_dirty = false;
        }
        if let Some(sequence) = self.linking.take_cancelled()
            && let Some(client) = self.linking.client
            && let Some(api) = &self.flavite
        {
            api.cancel_link_intent(client, sequence);
        }

        Task::none()
    }

    fn handle_mouse(&mut self, event: iced::mouse::Event) {
        match event {
            iced::mouse::Event::CursorMoved { position } => {
                self.pointer = Some(position);
                if let Some(split) = self.resizing_split
                    && let Some(ratio) = self.pointer_resize_ratio(split, position)
                    && self.resize_preview.get(&split) != Some(&ratio_to_ppm(ratio))
                {
                    self.resize_preview.insert(split, ratio_to_ppm(ratio));
                    self.pointer_feedback = true;
                }
                let was_dragging = self.gesture.dragging();
                self.gesture.moved(position);
                if self.gesture.dragging() {
                    if !was_dragging {
                        // The drag began: drop-zone overlays appear.
                        self.pointer_feedback = true;
                    }
                    let target = self.dock_target(position);
                    if let Some(target) = target
                        && self
                            .gesture
                            .dragged_tab()
                            .is_some_and(|tab| !self.drop_allowed(tab, target))
                    {
                        self.status = "This split needs more space or another tab".into();
                        self.pointer_feedback = true;
                    }
                    self.gesture.set_target(target);
                }
            }
            iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left) => {
                if let Some((tab, target)) = self.gesture.release() {
                    if self.drag_revision == Some(self.workspace.revision())
                        && self.drop_allowed(tab, target)
                    {
                        self.dispatch_layout(|revision| LayoutAction::MoveTab {
                            expected_revision: revision,
                            tab,
                            target: target.layout_target(),
                        });
                    }
                    self.drag_revision = None;
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

    /// Process vault events and completions on the host pump. Presentation
    /// never gates core progress (FR-28).
    fn pump(&mut self) {
        let drained = {
            let Some(flavite) = &self.flavite else {
                return;
            };
            flavite.pump();
            let mut refresh_catalog = false;
            let credits = NOTICE_CREDIT_CAPACITY.saturating_sub(self.notices.len());
            let mut events = Vec::new();
            for delivery in flavite.poll_events(credits) {
                let event = delivery.event;
                if let flavite_api::Event::ReconcileProgress { done, total } = &event {
                    self.reconcile_progress = (*done < *total).then_some((*done, *total));
                }
                // W12: a recovery offer is presentation, never mutation —
                // the dialog offers restore/discard explicitly (D-12).
                if let flavite_api::Event::RecoveryOffer { note_path, .. } = &event
                    && self.dialog.is_none()
                {
                    self.dialog = Some(crate::host::dialogs::Dialog::RecoveryOffer {
                        path: note_path.clone(),
                    });
                }
                // A conflicted buffer surfaces its resolution dialog once;
                // resolution stays explicit (no notice-driven mutation).
                if let flavite_api::Event::Conflict { note_path, .. } = &event
                    && self.dialog.is_none()
                    && let Some(tab) = self.tab_for_note_path(note_path)
                {
                    self.dialog = Some(crate::host::dialogs::Dialog::Conflict { tab });
                }
                if matches!(&event, flavite_api::Event::VaultChanged { .. }) {
                    refresh_catalog = true;
                    // Saved content changed: previews and backlinks re-query.
                    self.contexts.invalidate_all();
                    // No graph pulse: the writer-side generation broadcast
                    // (RFC-0005 S1/S5) wakes the feed service directly —
                    // the UI relay retired with the poll cadence.
                }
                // S4: a contained vault-session panic is an operator-grade
                // signal — warn on stderr (the crate's host-side channel,
                // cf. host/env.rs) and surface on the status line. The
                // session stays operable; the host decides whether to
                // continue or restart.
                if let flavite_api::Event::SessionPanicked {
                    message,
                    panics_total,
                    ..
                } = &event
                {
                    eprintln!(
                        "warning: vault session contained an internal panic (#{panics_total}): {message}"
                    );
                    self.status = format!("Recovered from an internal error: {message}");
                }
                events.push(format!("{}: {event:?}", event.event_type()));
                if let Some(id) = delivery.notice_id {
                    self.notices.push_back(NoticePresentation { id, event });
                }
            }
            let completions = flavite.drain_operation_completions();
            if refresh_catalog {
                // Scoped borrow ends before mutation below.
                self.note_vault_changed_pulse();
            }
            (events, completions)
        };
        let (_events, completions) = drained;
        // W10: admit debounced search submissions whose window elapsed.
        let search_incarnations: BTreeMap<TabId, u64> = self
            .search
            .tabs
            .keys()
            .copied()
            .filter_map(|tab| {
                let incarnation = self
                    .workspace
                    .tab(&tab)
                    .map(|session| session.interaction().incarnation)?;
                Some((tab, incarnation))
            })
            .collect();
        let search_api = self.flavite.as_ref();
        self.search.admit_due(search_api, &search_incarnations);
        let query_outcomes = self
            .flavite
            .as_ref()
            .map_or_else(Vec::new, |api| api.query_service().drain());
        {
            // Context query results: late or superseded tokens are dropped
            // by the registry, so a rebind or replaced view is never
            // repainted by an older query.
            for outcome in query_outcomes {
                if outcome.token.purpose == flavite_api::QueryPurpose::Catalog
                    && outcome.token.context_tab == flavite_api::CATALOG_CONTEXT_TAB
                {
                    self.apply_catalog_outcome(&outcome.result);
                    continue;
                }
                if crate::host::reading::observe(self, &outcome)
                    || crate::host::occurrences::observe(self, &outcome)
                    || self.search.observe(&outcome)
                {
                    continue;
                }
                if let flavite_api::ContextQueryResult::Document(doc) = &outcome.result {
                    crate::host::reading::cache_images(self, doc);
                }
                let _ = self.contexts.observe(
                    TabId::from_raw(outcome.token.context_tab),
                    outcome.token.incarnation,
                    outcome.token.binding_sequence,
                    outcome.token.request_sequence,
                    &outcome.result,
                );
            }
        }
        self.operations.ingest(&completions);
        // Turn budget (NFR-08): route a bounded number of completions per
        // pump; the remainder waits in the backlog for the next tick, typed
        // and lossless in the ledger.
        let mut queue = std::mem::take(&mut self.completion_backlog);
        queue.extend(completions);
        for _ in 0..MAX_COMPLETION_ROUTING_PER_PUMP {
            let Some(completion) = queue.pop_front() else {
                break;
            };
            self.route_completion(completion);
        }
        self.completion_backlog = queue;
        self.reconcile_contexts();
        crate::host::reading::sync(self);
        self.persist_workspace_layout();
        if let Some(target) = self.workspace.writing_target() {
            let remains_current = self.valid_writing_target().is_some();
            if !remains_current {
                self.workspace.invalidate_writing_target(target.buffer_id);
            }
        }
        // A prepared gesture whose source caret/revision moved is cancelled,
        // never committed (§9: no auto commit after cancellation).
        self.linking
            .invalidate_if_source_changed(self.valid_writing_target());
        // Graph frames, pins, geometry pairing, and camera commits (§W07).
        crate::host::graph::pump(self);
        crate::host::occurrences::pump(self);
    }

    /// Route one completion to its document flight, independent of which
    /// tab still exists (TD-09). Outcomes without a live flight stay typed
    /// in the ledger and are presented with their real result.
    /// Route one terminal completion to its owning surface. Public so the
    /// visual/integration harnesses can drive correlated completions
    /// directly; production callers go through the pump.
    pub fn route_completion(&mut self, completion: flavite_api::OperationCompletion) {
        // W12: a close plan's own save completion drives the close; other
        // routing must not consume it.
        if self.route_close_completion(completion.request_id, &completion.result) {
            return;
        }
        if crate::host::occurrences::completion(self, &completion) {
            return;
        }
        if self.linking.handles(completion.request_id) {
            self.route_link_completion(completion);
            return;
        }
        let success_data = match &completion.result {
            ExecutionResult::Local(LocalResult::Success(success)) => Some(&success.data),
            _ => None,
        };
        let is_create = self.operations.label_of(completion.request_id) == Some("note.create")
            || success_data.is_some_and(|data| {
                data.get("path").is_some()
                    && data.get("buffer_id").is_some()
                    && data.get("hash").is_none()
            });
        let is_rename = self.operations.label_of(completion.request_id) == Some("note.rename")
            || success_data.is_some_and(|data| {
                data.get("txn_id").is_some()
                    && data.get("from").is_some()
                    && data.get("to").is_some()
            });
        if is_create
            && let ExecutionResult::Local(LocalResult::Success(success)) = &completion.result
            && let Some(path) = success.data.get("path").and_then(serde_json::Value::as_str)
            && let Ok(relative) = std::path::Path::new(path).strip_prefix(&self.root)
            && let Ok(locator) = NoteLocator::from_relative(relative)
        {
            self.status = format!("Created {}", locator.components().join("/"));
            self.request_catalog_refresh();
            self.request_open(locator, None);
            return;
        }
        if is_rename
            && let ExecutionResult::Local(LocalResult::Success(success)) = &completion.result
            && let (Some(from), Some(to)) = (
                success.data.get("from").and_then(serde_json::Value::as_str),
                success.data.get("to").and_then(serde_json::Value::as_str),
            )
            && let (Ok(from_path), Ok(to_path)) = (
                std::path::Path::new(from).strip_prefix(&self.root),
                std::path::Path::new(to).strip_prefix(&self.root),
            )
            && let (Ok(from_locator), Ok(to_locator)) = (
                NoteLocator::from_relative(from_path),
                NoteLocator::from_relative(to_path),
            )
        {
            self.status = format!("Renamed to {}", to_locator.components().join("/"));
            // Every note view, history view, pinned binding, and nav entry
            // follows the renamed identity (T0–T2 authoritative update).
            self.dispatch_layout(|revision| LayoutAction::RelocateNote {
                expected_revision: revision,
                from: from_locator.clone(),
                to: to_locator.clone(),
            });
            self.nav.relocate(&from_locator, &to_locator);
            self.request_catalog_refresh();
            self.refresh_semantics();
            return;
        }
        let flight = self.notes.finish_request(completion.request_id);
        let locator = flight.as_ref().map(|flight| flight.locator.clone());
        let tabs = flight.map(|flight| flight.tabs).unwrap_or_default();
        match &completion.result {
            ExecutionResult::Local(LocalResult::Success(success)) => {
                let buffer = success
                    .data
                    .get("buffer_id")
                    .and_then(serde_json::Value::as_u64);
                if let Some(locator) = &locator {
                    self.notes.install_buffer(locator, buffer);
                }
                // Every lease on the document shares the flight outcome.
                for tab in tabs {
                    self.dispatch_layout(|revision| LayoutAction::NoteLoadReady {
                        expected_revision: revision,
                        tab,
                    });
                }
            }
            _ => {
                for tab in tabs {
                    self.dispatch_layout(|revision| LayoutAction::NoteLoadFailed {
                        expected_revision: revision,
                        tab,
                    });
                }
                // A flight with a locator would otherwise swallow the reason;
                // the failed note view says "unavailable" but not why.
                self.status = crate::operation::describe_completion(&completion);
            }
        }
        if locator.is_none() {
            self.status = crate::operation::describe_completion(&completion);
        }
    }

    /// Route a served `link.prepare` completion: commit immediately by
    /// default, or hold the plan for explicit confirmation (D-05).
    fn route_link_completion(&mut self, completion: flavite_api::OperationCompletion) {
        match &completion.result {
            ExecutionResult::Local(LocalResult::Success(success)) => {
                let Some(prepared) = crate::host::linking::prepared_from_data(&success.data) else {
                    self.linking
                        .finish(crate::host::linking::GestureState::Failed {
                            code: "link.prepare_malformed".into(),
                            repair: "prepare the link again".into(),
                        });
                    return;
                };
                let serialized = prepared.serialized_link.clone();
                let display = prepared.target_display.clone();
                match self.linking.observe_prepared(
                    completion.request_id,
                    prepared.plan_id,
                    prepared.serialized_link,
                    prepared.target_display,
                    prepared.requires_confirmation,
                ) {
                    Some((sequence, plan_id)) => {
                        self.commit_link_insert(sequence, plan_id, serialized, display)
                    }
                    None => {
                        self.status =
                            format!("Link plan to {display} is ready — confirm to insert it");
                    }
                }
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                self.linking
                    .finish(crate::host::linking::GestureState::Failed {
                        code: error.code.clone(),
                        repair: error.message.clone(),
                    });
                self.status = crate::operation::describe_local_error(error);
            }
            other => {
                self.linking
                    .finish(crate::host::linking::GestureState::Failed {
                        code: "link.prepare_failed".into(),
                        repair: "prepare the link again".into(),
                    });
                self.status = format!("Link not inserted: {other:?}");
            }
        }
    }

    /// Commit one prepared plan (synchronous guarded edit at the core).
    fn commit_link_insert(
        &mut self,
        sequence: u64,
        plan_id: String,
        serialized: String,
        display: String,
    ) {
        let expected = self.workspace.writing_target();
        let same_source = self.linking.pending_source().is_some_and(|source| {
            self.valid_writing_target()
                == Some((
                    source.tab,
                    source.incarnation,
                    source.buffer_id,
                    source.revision,
                    source.caret,
                ))
        });
        if !same_source || self.has_composition() {
            self.linking.cancel();
            self.status = "The insertion point changed; place the cursor and try again".into();
            return;
        }
        let Some(flavite) = &self.flavite else {
            self.linking.cancel();
            return;
        };
        let Some(client) = self.linking.client else {
            self.linking.cancel();
            return;
        };
        let Ok(plan_id_value) = plan_id.parse::<u128>() else {
            self.linking
                .finish(crate::host::linking::GestureState::Failed {
                    code: "link.plan_id".into(),
                    repair: "prepare the link again".into(),
                });
            return;
        };
        let request_id = flavite.command_registry().next_request_id();
        let invocation = Invocation::with_preconditions(
            request_id,
            "link.insert",
            serde_json::json!({
                "intent": { "client": client, "sequence": sequence },
                "plan_id": plan_id_value,
            }),
            Origin::Gui,
            flavite_api::Preconditions::new().requires_write(),
        );
        match flavite.execute(invocation) {
            ExecutionResult::Local(LocalResult::Success(success)) => {
                if let Some(target) = expected
                    && let Some(revision) = success.data["revision"].as_u64()
                    && let Some(caret) = target.caret_text_byte.checked_add(serialized.len())
                    && let Some(view) = self.note_editor_view(target.tab)
                    && view.revision == revision
                    && self
                        .workspace
                        .acknowledge_target_edit(target, revision, caret, &view.text)
                        .is_err()
                {
                    self.workspace.invalidate_writing_target(target.buffer_id);
                }
                self.linking
                    .finish(crate::host::linking::GestureState::Inserted {
                        serialized_link: serialized.clone(),
                        target: display,
                    });
                self.status = format!("Inserted {serialized}");
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                self.linking
                    .finish(crate::host::linking::GestureState::Failed {
                        code: error.code.clone(),
                        repair: error.message.clone(),
                    });
                self.status = crate::operation::describe_local_error(error);
            }
            ExecutionResult::Rejected(rejected) => {
                self.linking
                    .finish(crate::host::linking::GestureState::Failed {
                        code: format!("{:?}", rejected.reason),
                        repair: "prepare the link again".into(),
                    });
                self.status = format!(
                    "Link not inserted: {}",
                    crate::operation::describe_rejection(&rejected.reason)
                );
            }
            other => {
                self.linking
                    .finish(crate::host::linking::GestureState::Failed {
                        code: "link.insert_unexpected".into(),
                        repair: "prepare the link again".into(),
                    });
                self.status = format!("Link not inserted: {other:?}");
            }
        }
    }

    /// The acknowledged writing target identity, fully validated against the
    /// live editor view (the same predicate the pump revalidates).
    #[must_use]
    pub fn valid_writing_target(&self) -> Option<(TabId, u64, u64, u64, usize)> {
        let target = self.workspace.writing_target()?;
        if !crate::host::reading::is_edit(self, target.tab) {
            return None;
        }
        let remains = self.notes.incarnation(target.tab) == Some(target.incarnation)
            && self.note_editor_view(target.tab).is_some_and(|view| {
                view.buffer_id == target.buffer_id && view.revision == target.revision
            })
            && self
                .workspace
                .tab(&target.tab)
                .is_some_and(|view| view.interaction().sequence == target.interaction_sequence);
        remains.then_some((
            target.tab,
            target.incarnation,
            target.buffer_id,
            target.revision,
            target.caret_text_byte,
        ))
    }

    /// Begin one semantic insertion gesture from a picker or context action
    /// (W05). The caret is never manufactured: an unmet precondition is an
    /// honest refusal.
    pub fn request_link_insert(&mut self, locator: NoteLocator) {
        let expected = self.flavite.as_ref().and_then(|api| {
            api.link_target_identity(&locator.components().join("/"))
                .ok()
        });
        self.request_link_insert_expected(locator, expected);
    }

    pub(crate) fn request_link_insert_expected(
        &mut self,
        locator: NoteLocator,
        expected: Option<flavite_api::LinkTargetIdentity>,
    ) {
        let Some(expected) = expected else {
            self.status = "Target identities are updating or missing; choose the note again".into();
            return;
        };
        let Some(flavite) = &self.flavite else {
            self.status = "Open a vault before inserting links".into();
            return;
        };
        let Some((tab, incarnation, buffer_id, revision, caret)) = self.valid_writing_target()
        else {
            self.status = "Writing target: none — focus an editor caret to insert a link".into();
            return;
        };
        if self
            .notes
            .locator_of(tab)
            .is_some_and(|source| source == locator)
        {
            self.status =
                "Links to the note you are editing are refused; choose another note".into();
            return;
        }
        let target_path = locator.components().join("/");
        let target_display = locator.title().to_owned();
        let source = crate::host::linking::GestureSource {
            tab,
            incarnation,
            buffer_id,
            revision,
            caret,
        };
        if let Err(reason) = self
            .linking
            .begin(source, target_path.clone(), target_display.clone())
        {
            self.status = reason;
            return;
        }
        let client = match self.linking.client {
            Some(client) => client,
            None => {
                let client = flavite.new_link_client();
                self.linking.client = Some(client);
                client
            }
        };
        let request_id = flavite.command_registry().next_request_id();
        self.linking.stamp_request(request_id);
        let Some(sequence) = self.linking.pending_sequence() else {
            return;
        };
        let invocation = Invocation::with_preconditions(
            request_id,
            "link.prepare",
            serde_json::json!({
                "intent": { "client": client, "sequence": sequence },
                "source": { "buffer_id": buffer_id, "revision": revision, "caret": caret },
                "target_path": target_path.clone(),
                "expected_target": expected,
                "stable_link_policy": "disabled",
            }),
            Origin::Gui,
            flavite_api::Preconditions::new()
                .with_buffer_id(buffer_id)
                .with_revision(revision)
                .requires_write(),
        );
        match flavite.execute(invocation) {
            ExecutionResult::Pending { request_id } => {
                self.operations.accept(request_id, "link.prepare");
                self.status = format!("Preparing link to {target_display}…");
            }
            ExecutionResult::Local(LocalResult::Error(error)) => {
                self.linking
                    .finish(crate::host::linking::GestureState::Failed {
                        code: error.code.clone(),
                        repair: error.message.clone(),
                    });
                self.status = crate::operation::describe_local_error(error);
            }
            ExecutionResult::Rejected(rejected) => {
                self.linking
                    .finish(crate::host::linking::GestureState::Failed {
                        code: format!("{:?}", rejected.reason),
                        repair: "prepare the link again".into(),
                    });
                self.status = format!(
                    "Link not inserted: {}",
                    crate::operation::describe_rejection(&rejected.reason)
                );
            }
            other => {
                self.linking
                    .finish(crate::host::linking::GestureState::Failed {
                        code: "link.prepare_unexpected".into(),
                        repair: "prepare the link again".into(),
                    });
                self.status = format!("Link not inserted: {other:?}");
            }
        }
    }

    fn run_core_command(&mut self, command_id: &'static str) {
        let Some(flavite) = &self.flavite else {
            self.status = "Open a vault before running commands".into();
            return;
        };
        let args = match command_id {
            "note.save" => {
                let focused = self.workspace.focused_tab();
                let Some(state) = self.notes.state(&focused) else {
                    self.status = format!("{command_id} requires a note tab");
                    return;
                };
                serde_json::json!(state.locator.to_path(&self.root))
            }
            "note.close" => {
                // Palette close goes through the full W12 decision path.
                let focused = self.workspace.focused_tab();
                let _ = self.request_close_tab(focused);
                return;
            }
            "vault.reconcile" => serde_json::json!({ "scope": "full", "mode": "fast" }),
            "buffer.undo" | "buffer.redo" => {
                let focused = self.workspace.focused_tab();
                let Some(view) = self.note_editor_view(focused) else {
                    self.status = format!("{command_id} requires an open note");
                    return;
                };
                serde_json::json!({
                    "buffer_id": view.buffer_id,
                    "revision": view.revision,
                })
            }
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
                    "buffer.undo" => "Undo".into(),
                    "buffer.redo" => "Redo".into(),
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
        let mut specs = host_commands::ui_specs(&self.appearance.prefs.shortcuts);
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
        self.note_candidates
            .matches(&self.picker.query)
            .into_iter()
            .map(|candidate| candidate.entry)
            .collect()
    }

    fn run_picker_selection(&mut self, index: usize) -> Task<Message> {
        let matches = self.picker_matches();
        let replace = self.picker.replace_tab.take();
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
        if let Some(tab) = replace {
            // Open in this tab (W10): same TabId/mode, old note to Back.
            if let Some(previous) = self.note_locator(tab) {
                if previous == locator {
                    self.status = "This tab already shows that note".into();
                    return Task::none();
                }
                let anchor = self
                    .workspace
                    .tab(&tab)
                    .map(|session| session.interaction().viewport_anchor);
                let previous_clone = previous.clone();
                let committed = self.dispatch_layout_result(|revision| LayoutAction::NavigateTab {
                    expected_revision: revision,
                    tab,
                    locator: locator.clone(),
                });
                if committed {
                    self.nav
                        .record_replacement(tab, previous_clone, anchor, locator);
                }
                return Task::none();
            }
        }
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
            iced::system::theme_changes().map(Message::SystemTheme),
        ])
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

impl App {
    /// The tab whose document lives at `note_path` (vault-relative or
    /// native display form), if any.
    #[must_use]
    pub fn tab_for_note_path(&self, note_path: &str) -> Option<TabId> {
        let native = std::path::PathBuf::from(note_path);
        self.notes.tabs().map(|(tab, _)| tab).find(|tab| {
            self.notes.state(tab).is_some_and(|state| {
                state.locator.to_path(self.root()) == native
                    || state.locator.components().join("/") == note_path.replace('\\', "/")
            })
        })
    }

    /// Resolve a conflicted buffer through the core policy (W12). A merge
    /// preparation opens the merge-confirmation dialog with the typed
    /// receipt; nothing is applied without explicit confirmation.
    pub fn resolve_conflict(&mut self, tab: TabId, resolution: &str) {
        let Some(flavite) = self.flavite.as_mut() else {
            return;
        };
        let Some(state) = self.notes.state(&tab) else {
            return;
        };
        let path = state.locator.to_path(&self.root);
        let request_id = flavite.command_registry().next_request_id();
        let invocation = flavite_api::Invocation::new(
            request_id,
            "resolve_conflict",
            serde_json::json!({ "path": path, "resolution": resolution }),
            flavite_api::Origin::Gui,
        );
        match flavite.execute(invocation) {
            flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(success)) => {
                if resolution == "merge"
                    && let Some(prepared) = prepared_merge_from(&success.data, &self.root)
                {
                    self.dialog = Some(crate::host::dialogs::Dialog::Merge { tab, prepared });
                    self.status = "Merge prepared; confirm to apply".into();
                    return;
                }
                self.dialog = None;
                self.status = match resolution {
                    "reload" => "Reloaded from disk; my edits were kept in local history".into(),
                    "keep_mine" => "My edits kept; save to persist them".into(),
                    _ => "Conflict resolution completed".into(),
                };
            }
            flavite_api::ExecutionResult::ActionRequired { code, .. } => {
                self.status = format!("Conflict resolution needs attention: {code}");
            }
            other => {
                self.status = format!("Conflict resolution did not complete: {other:?}");
            }
        }
    }

    /// Apply the confirmed merged text against the prepared receipt.
    pub fn apply_prepared_merge(&mut self) {
        let Some(crate::host::dialogs::Dialog::Merge { prepared, .. }) = self.dialog.clone() else {
            return;
        };
        let Some(flavite) = self.flavite.as_mut() else {
            return;
        };
        let request_id = flavite.command_registry().next_request_id();
        let invocation = flavite_api::Invocation::new(
            request_id,
            "conflict.apply_merge",
            serde_json::json!({
                "path": prepared.path,
                "buffer_id": prepared.buffer_id,
                "revision": prepared.revision,
                "disk_hash": prepared.disk_hash,
                "merged": prepared.merged,
            }),
            flavite_api::Origin::Gui,
        );
        match flavite.execute(invocation) {
            flavite_api::ExecutionResult::Local(local) => {
                self.dialog = None;
                if local.is_success() {
                    self.status = "Merged text applied as one guarded edit".into();
                } else {
                    self.status = format!(
                        "Merge not applied: {}",
                        crate::operation::describe_result(&local)
                    );
                }
            }
            other => {
                self.status = format!("Merge not applied: {other:?}");
            }
        }
    }

    /// Restore or discard a retained recovery copy through the API (W12).
    pub fn resolve_recovery(&mut self, path: &str, restore: bool) {
        let Some(flavite) = self.flavite.as_mut() else {
            return;
        };
        let command = if restore {
            "recovery.restore"
        } else {
            "recovery.discard"
        };
        let request_id = flavite.command_registry().next_request_id();
        let invocation = flavite_api::Invocation::new(
            request_id,
            command,
            serde_json::json!({ "path": path }),
            flavite_api::Origin::Gui,
        );
        match flavite.execute(invocation) {
            flavite_api::ExecutionResult::Pending { request_id } => {
                self.operations.accept(
                    request_id,
                    if restore {
                        "recovery restore"
                    } else {
                        "recovery discard"
                    },
                );
                self.dialog = None;
                self.status = "Recovery command accepted".into();
            }
            flavite_api::ExecutionResult::Local(local) => {
                self.dialog = None;
                self.status = if local.is_success() {
                    if restore {
                        "Recovered changes restored into the note".into()
                    } else {
                        "Recovered copy discarded".into()
                    }
                } else {
                    format!(
                        "Recovery command failed: {}",
                        crate::operation::describe_result(&local)
                    )
                };
            }
            other => {
                self.status = format!("Recovery command did not complete: {other:?}");
            }
        }
    }
}

/// Build a [`PreparedMerge`] from a `MergeReady` success payload.
fn prepared_merge_from(
    data: &serde_json::Value,
    root: &std::path::Path,
) -> Option<crate::host::dialogs::PreparedMerge> {
    let path = data.get("path")?.as_str()?;
    let buffer_id = data.get("buffer_id")?.as_u64()?;
    let revision = data.get("revision")?.as_u64()?;
    let disk_hash = data.get("disk_hash")?.as_str()?.to_owned();
    let merged = data.get("merged")?.as_str()?.to_owned();
    let conflicts = data
        .get("conflicts")
        .and_then(serde_json::Value::as_array)
        .map(|hunks| {
            hunks
                .iter()
                .map(serde_json::Value::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // Display paths stay vault-relative; identity uses the native path.
    let display = std::path::PathBuf::from(path)
        .strip_prefix(root)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.to_owned());
    Some(crate::host::dialogs::PreparedMerge {
        path: display,
        buffer_id,
        revision,
        disk_hash,
        merged,
        conflicts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog_page(
        entries: &[(&str, &str)],
        epoch: u64,
        generation: u64,
        truncated: bool,
        total: u64,
    ) -> flavite_api::ContextQueryResult {
        let entries: std::sync::Arc<[NoteCatalogEntry]> = std::sync::Arc::from(
            entries
                .iter()
                .map(|(path, title)| NoteCatalogEntry {
                    path: PathBuf::from(path),
                    native_path: path.as_bytes().to_vec(),
                    title: (*title).to_owned(),
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        flavite_api::ContextQueryResult::Catalog {
            entries,
            provenance: flavite_api::Provenance { epoch, generation },
            total,
            truncated,
        }
    }

    /// S5 (D9, R9): the ticketed state machine serves the last good
    /// snapshot through `Loading` and `Failed` (no flicker), surfaces
    /// retryable text on failure, and propagates the 50k truncation flag.
    #[test]
    fn catalog_status_machine_preserves_snapshots_and_flags() {
        let mut app = empty_app();
        assert_eq!(*app.catalog_status(), CatalogStatus::Empty);
        assert_eq!(app.catalog_error(), None);

        app.apply_catalog_outcome(&catalog_page(
            &[("alpha.md", "Alpha"), ("beta.md", "Beta")],
            1,
            7,
            true,
            50_001,
        ));
        assert_eq!(
            *app.catalog_status(),
            CatalogStatus::Ready {
                epoch: 1,
                generation: 7,
                truncated: true
            }
        );
        assert_eq!(app.catalog().len(), 2);
        assert_eq!(app.note_candidates().all().len(), 2);
        assert_eq!(app.catalog_error(), None, "Ready clears the failure text");

        // Failed: visible retryable text, snapshot and candidates retained.
        app.apply_catalog_outcome(&flavite_api::ContextQueryResult::Failed {
            message: "catalog ticket failed".into(),
        });
        assert!(matches!(
            app.catalog_status(),
            CatalogStatus::Failed { message } if message == "catalog ticket failed"
        ));
        assert_eq!(app.catalog_error(), Some("catalog ticket failed"));
        assert_eq!(app.catalog().len(), 2, "failures never clear the snapshot");
        assert_eq!(app.note_candidates().all().len(), 2);

        // An out-of-contract outcome is honest failure text, not a panic.
        app.apply_catalog_outcome(&flavite_api::ContextQueryResult::Missing);
        assert!(
            app.catalog_error()
                .is_some_and(|error| error.contains("unexpected"))
        );
        assert_eq!(app.catalog().len(), 2);

        // A fresh Ready replaces both the text and the snapshot wholesale.
        app.apply_catalog_outcome(&catalog_page(&[("gamma.md", "Gamma")], 2, 9, false, 1));
        assert_eq!(
            *app.catalog_status(),
            CatalogStatus::Ready {
                epoch: 2,
                generation: 9,
                truncated: false
            }
        );
        assert_eq!(app.catalog().len(), 1);
        assert_eq!(app.catalog_error(), None);
    }

    /// Without an attached vault the trigger is inert; with one, every
    /// refresh is an admission (never a blocking read — the load path that
    /// used to run `note_catalog` on this turn is gone).
    #[test]
    fn catalog_triggers_never_block_without_an_attachment() {
        let mut app = empty_app();
        app.request_catalog_refresh();
        app.note_vault_changed_pulse();
        app.catalog_retry_tick();
        assert_eq!(*app.catalog_status(), CatalogStatus::Empty);
    }

    /// §6.4 throttle: pulses inside the window coalesce into exactly one
    /// pending retry; the retry fires only after the window elapsed and is
    /// consumed once (never a queue of redundant host-turn work).
    #[test]
    fn vault_pulse_throttle_coalesces_into_one_retry() {
        let mut app = empty_app();
        app.catalog_status = CatalogStatus::Ready {
            epoch: 1,
            generation: 9,
            truncated: false,
        };
        app.catalog_last_submit = Some(Instant::now());

        app.note_vault_changed_pulse();
        assert!(app.catalog_retry_due, "in-window pulse must coalesce");
        assert!(matches!(
            app.catalog_status(),
            CatalogStatus::Ready { generation: 9, .. }
        ));

        // Retry tick respects the window while it is open.
        app.catalog_retry_tick();
        assert!(
            app.catalog_retry_due,
            "tick before the window keeps the retry"
        );

        let long_ago = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("test clock started more than two seconds ago");
        app.catalog_last_submit = Some(long_ago);
        app.catalog_retry_tick();
        assert!(
            !app.catalog_retry_due,
            "expired window consumes the single coalesced retry"
        );
    }

    fn empty_app() -> App {
        let mut ids = Ids::new();
        let workspace = WorkspaceState::with_ids(&mut ids);
        App {
            flavite: None,
            root: PathBuf::from("fixture-vault"),
            adapter: Adapter::build(&workspace),
            notes: DocumentRegistry::default(),
            contexts: ContextRegistry::default(),
            reading: crate::host::reading::ReadingHost::default(),
            encoding: crate::host::encoding::EncodingHost::default(),
            input_chains: BTreeMap::new(),
            linking: crate::host::linking::LinkIntents::default(),
            graph: {
                let mut host = crate::host::graph::GraphHost::default();
                host.backend = crate::host::graph::GraphBackend::from_env();
                host
            },
            search: crate::views::search::SearchHost::default(),
            history: crate::views::history::HistoryHost::default(),
            nav: crate::host::navigation::NavHost::default(),
            dialog: None,
            start: crate::host::dialogs::StartState::default(),
            notes_filters: BTreeMap::new(),
            search_focus_tried: BTreeSet::new(),
            note_candidates: crate::picker::NoteCandidates::default(),
            persistence: crate::host::persistence::PersistenceHost::default(),
            appearance: crate::host::appearance::AppearanceHost::boot(),
            shortcut_capture: None,
            closing: None,
            close_queue: Vec::new(),
            shutdown: None,
            named_layouts: Vec::new(),
            picker: Picker::default(),
            palette_open: false,
            palette_query: String::new(),
            palette_selected: 0,
            strip_menu: None,
            gesture: gestures::TabGesture::default(),
            resize_preview: BTreeMap::new(),
            resizing_split: None,
            workspace_bounds: iced::Rectangle::new(
                iced::Point::ORIGIN,
                iced::Size::new(1180.0, 650.0),
            ),
            group_bounds: BTreeMap::new(),
            editor_bounds: BTreeMap::new(),
            menu_selected: 0,
            pointer: None,
            drag_revision: None,
            pointer_feedback: false,
            window_geometry_dirty: true,
            catalog: Vec::new(),
            catalog_status: CatalogStatus::Empty,
            catalog_last_submit: None,
            catalog_retry_due: false,
            restore_pending: false,
            settings: Settings::default(),
            settings_valid: true,
            operations: OperationPump::default(),
            completion_backlog: VecDeque::new(),
            status: "Ready".into(),
            notices: VecDeque::new(),
            reconcile_progress: None,
            window_size: iced::Size::new(1180.0, 760.0),
            a11y_nodes: BTreeMap::new(),
            a11y_text_cache: std::cell::RefCell::default(),
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
    fn stale_layout_messages_leave_authoritative_state_unchanged() {
        let mut app = empty_app();
        let group = app.workspace.first_group().expect("group");
        let stale = LayoutAction::OpenEmpty {
            expected_revision: app.workspace.revision() + 50,
            group,
        };
        let before = app.workspace.clone();
        let _ = app.update(Message::Layout(stale));
        assert_eq!(
            app.workspace, before,
            "stale widget actions must not be silently reauthorized"
        );
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
        let matched = crate::picker::NoteCandidates::from_catalog(&entries).matches("feedback");
        assert_eq!(matched.len(), 1);
        assert_eq!(matched[0].title(), "Feedback loops");
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
        let _ = app.update(Message::ResizeBegin(split));
        let _ = app.update(Message::Mouse(iced::mouse::Event::CursorMoved {
            position: iced::Point::new(
                app.workspace_bounds.x + 2.0 + 0.7 * (app.workspace_bounds.width - 4.0),
                app.workspace_bounds.y + 200.0,
            ),
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
