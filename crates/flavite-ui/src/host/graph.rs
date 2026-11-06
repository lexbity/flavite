//! Graph view host state (RFC-0003 W07, §8.1, §9.3, §10.3).
//!
//! Owns everything per-graph-tab that is *not* authoritative workspace
//! state and *not* widget-tree state: camera mirror, selection, the click
//! arbiter, node-drag preview, geometry pairing cache, pin authority, and
//! the honest service/frame status the view renders. Tab identity
//! (`TabId` + view incarnation) is carried on every message and rechecked
//! by the caller, so a result or gesture for a moved/replaced view cannot
//! repaint a different tab (§8.1, FR-20).
//!
//! Pins are vault-user state shared across graph views (§10.3): this host
//! is the single in-process owner, persists through the API
//! (`Flavite::write_graph_pins`), and never touches the filesystem
//! directly (TD-11).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use flavite_api::{
    GraphServiceMetrics, GraphServiceStatus, QueryPayload, RequestToken, StoredUserState,
};
use flavite_graph::{
    GraphNodeRecord, GraphPublication, LayoutState, NodeId, NodeKind, PositionFrame, RenderKey,
};
use flavite_workspace::{GraphCamera, GraphView, LayoutAction, NoteLocator, TabId, ViewKind};
use iced::Rectangle;

use crate::app::{App, Message};
use crate::graph::events::{GestureArbiter, GestureOutcome};
use crate::graph::geometry::{GeometryOverrides, HitNode, RenderGeometry};
use crate::graph::instance::Camera;
use crate::graph::pins::{PinCommand, PinKey, PinStore, PinStoreError};
use crate::graph::renderer::{GraphMessage, ReleaseKind};

/// Explicit renderer backend for graph views (§12.2: a failed GPU
/// initialization must not disable the network or note editing).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GraphBackend {
    /// Instanced wgpu shader (production default).
    #[default]
    Gpu,
    /// Software canvas (fallback / `--backend canvas` evidence runs).
    Canvas,
}

impl GraphBackend {
    /// Resolve from the documented operational setting
    /// (`FLAVITE_GRAPH_BACKEND=gpu|canvas`). This is a render-backend
    /// choice like `WINIT_UNIX_BACKEND` — not a test-mode switch; the
    /// fixture harness cannot enable behavior the product lacks.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var("FLAVITE_GRAPH_BACKEND").ok().as_deref() {
            Some("canvas") => Self::Canvas,
            _ => Self::Gpu,
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Gpu => "gpu",
            Self::Canvas => "canvas",
        }
    }
}

/// Everything visible about one frame in a single comparable tuple. A
/// change in any component forces a geometry rebuild; equality means the
/// mounted geometry is already the honest current view (§10.3).
#[derive(Debug, Clone, PartialEq)]
pub struct FrameSignature {
    base: RenderKey,
    positions_generation: u64,
    selected: Option<NodeId>,
    selected_nodes: std::collections::BTreeSet<NodeId>,
    drag: Option<(NodeId, [f32; 2])>,
    writing_target: Option<NodeId>,
    pins_count: usize,
    node_count: usize,
    /// The active color scheme (W11): instance colors are semantic token
    /// data, so a light/dark switch rebuilds geometry from the *same*
    /// publication and positions frame — camera and layout never reset.
    scheme: crate::style::tokens::Scheme,
}

/// Per-graph-tab host state.
#[derive(Debug, Clone)]
pub struct GraphTabState {
    /// Durable camera mirrored from the authoritative view kind.
    pub camera: Camera,
    /// Pending camera commit instant (§7.4: gestures are ephemeral; release
    /// or idle commits one transaction).
    pub camera_pending: Option<Instant>,
    /// Current single selection (multi-selection arrives with W08 filters).
    pub selection: Option<HitNode>,
    pub selected_nodes: std::collections::BTreeSet<NodeId>,
    pub selection_summary: String,
    summary_key: Option<(RenderKey, std::collections::BTreeSet<NodeId>)>,
    pub box_preview: Option<([f32; 2], [f32; 2])>,
    pub list_offset: usize,
    pub occurrence_offset: usize,
    /// Click/double-click arbiter with injected tolerances.
    pub arbiter: GestureArbiter,
    /// In-flight node-drag preview (spatial only until release).
    pub drag_preview: Option<(HitNode, [f32; 2])>,
    /// Layout freeze toggle (§10.3).
    pub frozen: bool,
    pub frozen_nodes: BTreeMap<NodeId, [f32; 2]>,
    /// Accessible list mode (also the keyboard/AT graph equivalent).
    pub list_mode: bool,
    pub controls_open: bool,
    pub layout_controls_open: bool,
    pub display_controls_open: bool,
    /// Mounted geometry and the exact signature it was built from (§10.3).
    pub geometry: Option<Arc<RenderGeometry>>,
    pub signature: Option<FrameSignature>,
    /// Base publication key the mounted geometry was built for.
    pub geometry_key: Option<RenderKey>,
    /// Monotonic per-tab upload generation for mounted geometry.
    pub geometry_generation: u64,
    /// Frame staleness: set when a publication/positions pair refused to
    /// build; insertion is disabled while true (§10.3).
    pub stale: bool,
    /// Observed widget bounds (logical window coordinates) for
    /// screen-space harness reporting.
    pub bounds: Option<Rectangle>,
    /// Observed bounds of the activation-contract notice row, when the
    /// first-launch explanation is still owed (D-02).
    pub notice_bounds: Option<Rectangle>,
    /// Last seen base epoch (epoch resets clear transient entity state).
    pub last_epoch: u64,
    /// The user took the camera with a pan/zoom gesture; auto-fit and
    /// follow stop until the tab is recreated.
    pub user_camera_moved: bool,
    pub auto_fit_settled: bool,
    /// Layout activity seen at the previous pump (settle detection).
    last_layout_active: Option<bool>,
    pub scope: flavite_graph::GraphScope,
    pub scope_sequence: u64,
    pub scope_result: Option<Arc<flavite_graph::scope::ScopeResult>>,
    pub filter: crate::graph::filters::FilterDraft,
    pub filter_bounds: BTreeMap<crate::graph::filters::FilterField, Rectangle>,
    pub filter_error: Option<String>,
    pub previous_scope: Option<flavite_graph::GraphScope>,
    pub updating: bool,
    pub overlay_count: usize,
    pub failed_overlay_count: usize,
    pub occurrences: Arc<flavite_api::OccurrenceSet>,
}

impl GraphTabState {
    fn new(view: &GraphView) -> Self {
        let camera = view.camera();
        Self {
            camera: Camera {
                center: camera.center,
                zoom: camera.zoom,
            },
            camera_pending: None,
            selection: None,
            selected_nodes: std::collections::BTreeSet::new(),
            selection_summary: String::new(),
            summary_key: None,
            box_preview: None,
            list_offset: 0,
            occurrence_offset: 0,
            arbiter: GestureArbiter::new(),
            drag_preview: None,
            frozen: false,
            frozen_nodes: BTreeMap::new(),
            list_mode: false,
            controls_open: true,
            layout_controls_open: false,
            display_controls_open: false,
            geometry: None,
            signature: None,
            geometry_key: None,
            geometry_generation: 0,
            stale: false,
            bounds: None,
            notice_bounds: None,
            last_epoch: 0,
            user_camera_moved: false,
            auto_fit_settled: false,
            last_layout_active: None,
            scope: view.scope().clone(),
            scope_sequence: 1,
            scope_result: None,
            filter: crate::graph::filters::FilterDraft::from_scope(view.scope()),
            filter_bounds: BTreeMap::new(),
            filter_error: None,
            previous_scope: None,
            updating: true,
            overlay_count: 0,
            failed_overlay_count: 0,
            occurrences: Arc::new(flavite_api::OccurrenceSet::default()),
        }
    }

    /// Whether the mounted frame signature already carries at least one
    /// shared pin (the frozen-hold receipts check the catch-up applied the
    /// deferred entity frame).
    #[must_use]
    pub fn pins_in_signature(&self) -> bool {
        self.signature
            .as_ref()
            .is_some_and(|signature| signature.pins_count > 0)
    }
}

/// Pin authority: the single in-process owner of graph-pin state.
#[derive(Debug, Default)]
pub struct PinAuthority {
    store: PinStore,
    generation: u64,
    dirty: bool,
    /// Last typed failure (retry stays available; pins are never silently
    /// dropped).
    pub failure: Option<String>,
    next_retry: Option<Instant>,
    retry_index: usize,
    loaded: bool,
    load_failure: Option<String>,
}

/// Bounded retry backoff (§11.2: 1/2/4/8/16/30 s).
const PIN_RETRY_BACKOFF: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
    Duration::from_secs(30),
];

/// Camera idle window (§7.4 ephemeral preview + single commit).
const CAMERA_COMMIT_IDLE: Duration = Duration::from_millis(500);

impl PinAuthority {
    /// Seed from an API-read document. A decode refusal keeps the previous
    /// store and surfaces the typed error (never overwrite-by-default).
    pub fn observe_read(&mut self, stored: Option<StoredUserState>) -> Result<(), String> {
        match stored {
            None => {
                self.store = PinStore::default();
                self.generation = 0;
            }
            Some(stored) => match PinStore::decode(&stored.payload) {
                Ok(store) => {
                    self.store = store;
                    self.generation = stored.generation;
                }
                Err(error) => {
                    let message = error.to_string();
                    self.load_failure = Some(message.clone());
                    return Err(message);
                }
            },
        }
        self.loaded = true;
        self.load_failure = None;
        Ok(())
    }

    #[must_use]
    pub fn loaded(&self) -> bool {
        self.loaded
    }

    #[must_use]
    pub fn load_failure(&self) -> Option<&str> {
        self.load_failure.as_deref()
    }

    #[must_use]
    pub fn count(&self) -> usize {
        self.store.len()
    }

    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    #[must_use]
    pub fn store(&self) -> &PinStore {
        &self.store
    }

    /// Apply one pin command in memory; a validated change marks the
    /// document dirty for the next pump write (the UI emits a PinCommand
    /// and observes the result — no alternate writer exists).
    pub fn execute(&mut self, command: PinCommand) -> Result<bool, PinStoreError> {
        let changed = self.store.execute(command)?;
        if changed {
            self.dirty = true;
            self.failure = None;
            self.next_retry = None;
        }
        Ok(changed)
    }

    pub(super) fn record_failure(&mut self, text: String) {
        self.failure = Some(text);
        let delay = PIN_RETRY_BACKOFF[self.retry_index.min(PIN_RETRY_BACKOFF.len() - 1)];
        self.retry_index = self.retry_index.saturating_add(1);
        self.next_retry = Some(Instant::now() + delay);
    }

    /// Force the next backoff window to open now (Retry control).
    pub fn retry_now(&mut self) {
        self.next_retry = None;
        self.retry_index = 0;
    }

    /// Pinned world positions for the dense nodes of one publication
    /// (shared vault-user state: dragging a pinned node affects every
    /// view, with an explicit notice, §10.3).
    #[must_use]
    pub fn positions_for(&self, publication: &GraphPublication) -> BTreeMap<NodeId, [f32; 2]> {
        let mut overrides = BTreeMap::new();
        for record in publication
            .records
            .iter()
            .filter(|r| r.id.kind != NodeKind::Unresolved && r.id.id <= i64::MAX as u64)
        {
            let key = pin_key_for(record);
            if let Some(pinned) = self.store.get(&key) {
                overrides.insert(record.id, pinned.position);
            }
        }
        overrides
    }
}

/// Persist the dirty pin document when due. A free function over `App` so
/// the disjoint field borrows (`app.flavite` for the write,
/// `app.graph.pins` for the state) stay checkable; failures retain the
/// dirty flag and schedule bounded backoff (§11.2, TD-11).
pub fn flush_pins(app: &mut App, now: Instant) {
    if !app.graph.pins.dirty {
        return;
    }
    if app.graph.pins.next_retry.is_some_and(|due| now < due) {
        return;
    }
    if app.flavite.is_none() {
        return;
    }
    let expected = app.graph.pins.generation;
    let bytes = match app.graph.pins.store.encode() {
        Ok(bytes) => bytes,
        Err(error) => {
            app.graph
                .pins
                .record_failure(format!("graph pins could not be encoded: {error}"));
            return;
        }
    };
    let write = app
        .flavite
        .as_ref()
        .expect("checked")
        .write_graph_pins(expected, &bytes);
    match write {
        Ok(generation) => {
            let pins = &mut app.graph.pins;
            pins.generation = generation;
            pins.dirty = false;
            pins.failure = None;
            pins.next_retry = None;
            pins.retry_index = 0;
        }
        Err(error) => {
            let text = error.to_string();
            if text.contains("generation mismatch") {
                // Another writer advanced the file: re-read through the
                // API and keep our dirty intent on top of the fresh
                // document — never overwrite a newer file blind.
                let reloaded = app.flavite.as_ref().expect("checked").read_graph_pins();
                match reloaded {
                    Ok(stored) => {
                        let dirty = std::mem::replace(&mut app.graph.pins.dirty, false);
                        if let Err(read_error) = app.graph.pins.observe_read(stored) {
                            app.graph.pins.record_failure(read_error);
                        } else {
                            app.graph.pins.dirty = dirty;
                            app.graph.pins.record_failure(
                                "graph pins changed on disk; re-read — Retry persists your pins"
                                    .into(),
                            );
                        }
                    }
                    Err(read_error) => app.graph.pins.record_failure(read_error.to_string()),
                }
            } else {
                app.graph.pins.record_failure(text);
            }
        }
    }
}

/// The pin key for one node record: stable IDs only when unconflicted;
/// native path bytes otherwise. Identity semantics unchanged (T-rules are
/// enforced at carry time by the rename slice).
#[must_use]
pub fn pin_key_for(record: &GraphNodeRecord) -> PinKey {
    PinKey::for_note(
        record.stable_id.as_deref(),
        record.id_conflict,
        &record.path_native,
    )
}

/// Per-tab host for every mounted graph view.
#[derive(Debug)]
pub struct GraphHost {
    tabs: BTreeMap<TabId, GraphTabState>,
    pub pins: PinAuthority,
    pub backend: GraphBackend,
    pub status: Option<GraphServiceStatus>,
    pub metrics: Option<GraphServiceMetrics>,
    /// Frames discarded for version mismatch (diagnostics counter, §10.3).
    pub stale_discards_total: u64,
    context_version: Option<(RenderKey, bool)>,
    pub removal: Option<crate::host::occurrences::RemovalWorkflow>,
    pub removal_sequence: u64,
    pub removal_client: Option<u128>,
    /// D-02: graph-activated attachment inspector requests, keyed by the
    /// core-issued token. The reading outcome router consumes them; an
    /// unknown or replaced-tab completion drops without effects.
    pub attachment_opens: BTreeMap<RequestToken, TabId>,
}

#[allow(clippy::derivable_impls)]
impl Default for GraphHost {
    fn default() -> Self {
        Self {
            tabs: BTreeMap::new(),
            pins: PinAuthority::default(),
            backend: GraphBackend::default(),
            status: None,
            metrics: None,
            stale_discards_total: 0,
            context_version: None,
            removal: None,
            removal_sequence: 0,
            removal_client: None,
            attachment_opens: BTreeMap::new(),
        }
    }
}

impl GraphHost {
    /// Attach a newly opened graph view, seeded from its durable camera.
    pub fn attach(&mut self, tab: TabId, view: &GraphView) {
        self.tabs
            .entry(tab)
            .or_insert_with(|| GraphTabState::new(view));
    }

    /// Release one graph tab's transient state (shared pins survive).
    pub fn detach(&mut self, tab: TabId) {
        self.tabs.remove(&tab);
    }

    #[must_use]
    pub fn state(&self, tab: TabId) -> Option<&GraphTabState> {
        self.tabs.get(&tab)
    }

    #[must_use]
    pub fn state_mut(&mut self, tab: TabId) -> Option<&mut GraphTabState> {
        self.tabs.get_mut(&tab)
    }

    /// Consume one completed attachment-open token (called by the query
    /// outcome router). An unknown token is a no-op.
    pub fn take_attachment_open(&mut self, token: &RequestToken) -> Option<TabId> {
        self.attachment_opens.remove(token)
    }

    /// Whether this tab already has an outstanding inspector request.
    #[must_use]
    pub fn attachment_open_pending(&self, tab: TabId) -> bool {
        self.attachment_opens.values().any(|owner| *owner == tab)
    }

    #[must_use]
    pub fn tab_ids(&self) -> Vec<TabId> {
        self.tabs.keys().copied().collect()
    }

    /// The frame signature the mounted geometry would have to match.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn signature_for(
        &self,
        tab: TabId,
        base_key: &RenderKey,
        positions: &PositionFrame,
        pinned: &BTreeMap<NodeId, [f32; 2]>,
        publication: &GraphPublication,
        writing_target: Option<NodeId>,
        scheme: crate::style::tokens::Scheme,
    ) -> FrameSignature {
        let state = self.tabs.get(&tab);
        FrameSignature {
            base: *base_key,
            positions_generation: positions.generation(),
            selected: state.and_then(|state| state.selection.map(|hit| hit.id)),
            selected_nodes: state.map_or_else(std::collections::BTreeSet::new, |s| {
                s.selected_nodes.clone()
            }),
            drag: state.and_then(|state| {
                state
                    .drag_preview
                    .map(|(node, position)| (node.id, position))
            }),
            writing_target,
            pins_count: pinned.len(),
            node_count: publication.node_count(),
            scheme,
        }
    }
}

/// Commands the graph view exposes to menus, keyboard, palette, and the
/// accessibility list rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphCommand {
    ToggleControls,
    ToggleLayoutControls,
    ToggleDisplayControls,
    /// Toggle the force-layout freeze.
    ToggleFreeze,
    /// Toggle the accessible/keyboard list presentation.
    ToggleList,
    /// Fit the whole saved base into the viewport.
    FitView,
    /// Open the selected note (explicit navigation; never inserts).
    OpenSelected,
    /// Request one link insertion for the selected note.
    InsertSelected,
    /// Unpin the selected node (removes the shared pin).
    UnpinSelected,
    /// Retry the graph service after a failure.
    RetryService,
    /// Retry the pin persistence after a failure.
    RetryPins,
    /// Select one node by identity (list row activation).
    Select(NodeId),
    ToggleSelected(NodeId),
    NextListPage,
    PreviousListPage,
    NextOccurrences,
    PreviousOccurrences,
    ConfirmRemoval(u64),
    CancelRemoval,
    ToggleFilters,
    Depth(u8),
    Direction(flavite_workspace::Direction),
    Attachments(bool),
    Unresolved(bool),
    Embeds(bool),
    Any(bool),
    Negate(bool),
    ApplyFilters,
    ClearFilters,
    UndoFilters,
    ExploreSelected,
    RevealSelected,
    ClearSelection,

    /// Open one node by identity (list row action).
    Open(NodeId),
    /// Insert one node by identity (list row action).
    Insert(NodeId),
    /// Open every multi-selected note in its own tab (D-03: `Open selected
    /// in tabs`; native-key order, 20-view limit per invocation).
    OpenSelectedInTabs,
}

/// Resolve a node identity to its record within the current base
/// publication (dense-aligned lookup; never a guess).
#[must_use]
pub fn record_for(app: &App, id: NodeId) -> Option<GraphNodeRecord> {
    let frame = app.flavite.as_ref()?.graph_frame()?;
    let publication = frame.publication();
    let dense = publication.node_index(id)? as usize;
    publication.record(dense).cloned()
}

/// Vault-relative locator for a node record, `None` for attachments or
/// unrepresentable paths (the gesture then explains itself; it never
/// guesses or opens a note as a hidden fallback, D-05).
#[must_use]
pub fn locator_for(record: &GraphNodeRecord) -> Option<NoteLocator> {
    if record.id.kind != NodeKind::Note || record.binding_blocked {
        return None;
    }
    NoteLocator::from_relative(&record.native_path).ok()
}

/// D-02 freeze rule: with a click-pair hold pending, only a purely
/// emphasis change (selection, writing-target accent, palette) may rebuild
/// the mounted geometry. Anything that can move or add/remove entities —
/// base publication, position ticks, pins, node visibility, or a drag
/// preview — waits out the bounded hold, so the hit geometry the user is
/// clicking on stays exactly where it was drawn.
#[must_use]
pub fn emphasis_only_change(old: Option<&FrameSignature>, next: &FrameSignature) -> bool {
    old.is_some_and(|old| {
        old.base == next.base
            && old.positions_generation == next.positions_generation
            && old.pins_count == next.pins_count
            && old.node_count == next.node_count
            && old.drag == next.drag
    })
}

/// The selected record of one graph tab, resolved against the *current*
/// publication (never a remembered copy, D-02).
#[must_use]
pub fn selected_record(app: &App, tab: TabId) -> Option<GraphNodeRecord> {
    let hit = app.graph.state(tab).and_then(|state| state.selection)?;
    record_for(app, hit.id)
}

/// D-02/FR-15: the one predicate that decides whether explicit insertion
/// is available. Toolbar chip, palette, keyboard and the executing command
/// all agree: a resolvable note node, an acknowledged writing target, and
/// a current (not stale, not `Updating`) frame — insertion keeps the
/// current-paired-frame requirement even while navigation geometry is
/// frozen for the gesture hold.
#[must_use]
pub fn insert_available(app: &App, tab: TabId) -> bool {
    app.valid_writing_target().is_some()
        && selected_record(app, tab)
            .as_ref()
            .and_then(locator_for)
            .is_some()
        && app
            .graph
            .state(tab)
            .is_some_and(|state| !state.stale && !state.updating)
}

/// FR-15: the unresolved note node (if any) that the band and the palette
/// offer explicit Create/Locate for.
#[must_use]
pub fn selected_unresolved(app: &App, tab: TabId) -> Option<GraphNodeRecord> {
    selected_record(app, tab).filter(|record| record.id.kind == NodeKind::Unresolved)
}

/// The raw link target an unresolved record stands for. The mirror labels
/// unresolved nodes as `[[target]] (unresolved)` with an empty path; the
/// explicit offers operate on the target text, never the label.
#[must_use]
pub fn unresolved_target_raw(record: &GraphNodeRecord) -> Option<&str> {
    let label = record
        .title
        .strip_prefix("[[")?
        .strip_suffix("]] (unresolved)")?;
    (!label.is_empty()).then_some(label)
}

/// The canonical prefilled create path for an unresolved target (the same
/// helper the editor's unresolved-link activation uses), or `None` when it
/// cannot be represented as a note.
#[must_use]
pub fn unresolved_create_path(record: &GraphNodeRecord) -> Option<String> {
    let raw = unresolved_target_raw(record)?;
    let crate::host::dialogs::Dialog::NewNote { path } =
        crate::host::dialogs::offer_create_for_link(raw)
    else {
        return None;
    };
    NoteLocator::from_components(path.split('/'))
        .is_ok()
        .then_some(path)
}

/// Whether an unresolved record's target can be represented as a note at
/// all (the Create offer's precondition; the dialog re-validates).
#[must_use]
pub fn unresolved_is_representable(record: &GraphNodeRecord) -> bool {
    unresolved_create_path(record).is_some()
}

/// D-02: an unresolved node offers explicit Create through the existing
/// new-note dialog (validated, user-confirmed). Navigation never creates
/// the note itself (FR-19).
pub fn create_from_selected(app: &mut App, tab: TabId) -> iced::Task<crate::app::Message> {
    let Some(record) = selected_unresolved(app, tab) else {
        app.status = "Select an unresolved note node first".into();
        return iced::Task::none();
    };
    let Some(path) = unresolved_create_path(&record) else {
        app.status = "This unresolved path cannot be represented as a note".into();
        return iced::Task::none();
    };
    if app.has_composition() {
        return iced::Task::none();
    }
    app.dialog = Some(crate::host::dialogs::Dialog::NewNote { path });
    iced::widget::operation::focus(crate::host::dialogs::NEW_NOTE_INPUT_ID)
}

/// D-02: an unresolved node offers Locate through the note picker,
/// prefilled with the link target so the user can find the intended note.
pub fn locate_from_selected(app: &mut App, tab: TabId) -> iced::Task<crate::app::Message> {
    let Some(record) = selected_unresolved(app, tab) else {
        app.status = "Select an unresolved note node first".into();
        return iced::Task::none();
    };
    let query = unresolved_target_raw(&record)
        .map(str::to_owned)
        .unwrap_or_else(|| record.title.clone());
    app.picker = crate::app::Picker {
        open: true,
        query,
        selected: 0,
        replace_tab: None,
    };
    if app.catalog().is_empty() {
        app.request_catalog_refresh();
    }
    iced::widget::operation::focus(crate::workspace_shell::PICKER_INPUT_ID)
}

/// The writing target's note node in the current publication, for the
/// violet accent (§13.1: the writing target is visible on the graph, not
/// only in color words).
#[must_use]
pub fn writing_target_node(app: &App) -> Option<NodeId> {
    let (tab, ..) = app.valid_writing_target()?;
    let locator = app.notes.locator_of(tab)?;
    let wanted = locator.components().join("/");
    let frame = app.flavite.as_ref()?.graph_frame()?;
    frame
        .publication()
        .records
        .iter()
        .find(|record| record.path == wanted && record.id.kind == NodeKind::Note)
        .map(|record| record.id)
}

/// Poll the service/pins/camera and rebuild mounted geometry. Called from
/// the host pump (budgeted; the frame load is an atomic pointer read —
/// the UI never waits on layout or feed workers, D15/§12.1).
pub fn pump(app: &mut App) {
    let now = Instant::now();
    let Some(flavite) = app.flavite.as_ref() else {
        return;
    };
    app.graph.status = Some(flavite.graph_status());
    app.graph.metrics = Some(flavite.graph_metrics());
    let frame = flavite.graph_frame();

    // Pin persistence (TD-11 authority with the W06 accounting pattern).
    flush_pins(app, now);

    let Some(frame) = frame else {
        return;
    };
    let publication = Arc::clone(frame.publication());
    let positions = frame.positions();
    let base_key = frame.render_key();
    let projection_current = app
        .flavite
        .as_ref()
        .is_some_and(|api| api.graph_is_current())
        && matches!(app.graph.status, Some(GraphServiceStatus::Live));
    let context_version = Some((base_key, projection_current));
    if app.graph.context_version != context_version {
        app.graph.context_version = context_version;
        app.contexts.invalidate_all();
    }
    let pinned = app.graph.pins.positions_for(&publication);
    let target_node = writing_target_node(app);
    let tabs = app.graph.tab_ids();

    for tab in tabs {
        let durable_scope = app.workspace.tab(&tab).and_then(|s| match s.kind() {
            ViewKind::Graph(v) => Some(v.scope().clone()),
            _ => None,
        });
        if let Some(scope) = durable_scope
            && let Some(state) = app.graph.state_mut(tab)
            && state.scope != scope
        {
            state.scope = scope;
            state.scope_sequence = state.scope_sequence.saturating_add(1);
            state.filter = crate::graph::filters::FilterDraft::from_scope(&state.scope);
            state.filter_error = None;
            state.updating = true;
            state.arbiter.cancel();
        }
        let incarnation = app
            .workspace
            .tab(&tab)
            .map_or(0, |s| s.interaction().incarnation);
        if let Some(state) = app.graph.state(tab)
            && let Some(api) = app.flavite.as_ref()
        {
            let _ = api.graph_request_scope(
                flavite_api::GraphViewToken {
                    tab: tab.raw(),
                    incarnation,
                    sequence: state.scope_sequence,
                },
                state.scope.clone(),
            );
        }
        let projection = frame.projection();
        let accepted_scope = app.graph.state(tab).and_then(|state| {
            projection.scopes.get(&tab.raw()).filter(|scoped| {
                scoped.token.incarnation == incarnation
                    && scoped.token.sequence == state.scope_sequence
            })
        });
        let mut key = base_key;
        if let Some(state) = app.graph.state_mut(tab) {
            key.scope_sequence = state.scope_sequence;
            state.updating = !projection_current || accepted_scope.is_none();
            state.overlay_count = projection.overlays.len();
            state.failed_overlay_count = projection
                .overlays
                .values()
                .filter(|s| matches!(s, flavite_api::OverlayState::Failed { .. }))
                .count();
            state.occurrences = Arc::new(projection.occurrences.clone());
            if let Some(scoped) = accepted_scope {
                match &scoped.result {
                    Ok(result) => state.scope_result = Some(result.clone()),
                    Err(error) => {
                        state.filter_error = Some(error.clone());
                        state.updating = true;
                    }
                }
            }
        }
        let epoch_reset = app
            .graph
            .state(tab)
            .is_some_and(|state| state.last_epoch != 0 && state.last_epoch != publication.epoch);
        if epoch_reset
            && app
                .workspace
                .exploration()
                .is_some_and(|s| s.source_tab == tab)
        {
            publish_empty(app, tab);
        }
        // Epoch resets clear transient entity state (selection/hit
        // candidates); durable locators re-resolve (§10.3).
        if let Some(state) = app.graph.state_mut(tab) {
            // D-02: the hit-geometry hold is bounded by the same interval
            // that completes the pair; an expired candidate releases the
            // frozen presentation on this pump.
            state.arbiter.expire(now);
            if publication.epoch != state.last_epoch && state.last_epoch != 0 {
                state.selection = None;
                state.selected_nodes.clear();
                state.box_preview = None;
                state.drag_preview = None;
                state.arbiter.cancel();
                state.frozen_nodes.clear();
            }
            state.last_epoch = publication.epoch;
            // D-02/S04: the navigation pair survives layout/overlay
            // generation movement inside its bounded hold; only a newer
            // identity epoch cancels it.
            state.arbiter.invalidate_epoch(publication.epoch);
        }

        let scheme = app.appearance.scheme();
        let signature = app.graph.signature_for(
            tab,
            &key,
            &positions,
            &pinned,
            &publication,
            target_node,
            scheme,
        );
        if let Some(state) = app.graph.state_mut(tab) {
            let summary_key = (base_key, state.selected_nodes.clone());
            if state.summary_key.as_ref() != Some(&summary_key) {
                let selected = &state.selected_nodes;
                let (mut internal, mut boundary, mut unsaved, mut saved) = (0, 0, 0, 0);
                if selected.len() > 1 {
                    for edge in &frame.projection().effective.edges {
                        let (from, to) =
                            (selected.contains(&edge.from), selected.contains(&edge.to));
                        internal += usize::from(from && to);
                        boundary += usize::from(from != to);
                    }
                    for occurrence in frame.projection().occurrences.iter().filter(|o| {
                        selected.contains(&o.edge.from) || selected.contains(&o.edge.to)
                    }) {
                        if matches!(
                            occurrence.source,
                            flavite_api::OccurrenceSource::Buffer { .. }
                        ) {
                            unsaved += 1;
                        } else {
                            saved += 1;
                        }
                    }
                    state.selection_summary = format!(
                        "{} selected · {internal} internal / {boundary} boundary directed connections · {unsaved} unsaved / {saved} saved occurrences",
                        selected.len()
                    );
                } else {
                    state.selection_summary.clear();
                }
                state.summary_key = Some(summary_key);
            }
        }
        let stale_now = app
            .graph
            .state(tab)
            .is_some_and(|state| state.stale || state.updating);
        let had_geometry = app
            .graph
            .state(tab)
            .is_some_and(|state| state.geometry.is_some());
        let rebuild = app
            .graph
            .state(tab)
            .is_some_and(|state| state.signature.as_ref() != Some(&signature) || stale_now);
        // D-02: while a click-pair hold is pending, the hit geometry is
        // frozen: entity-moving frames (base, positions, pins, node count,
        // drag) defer until the bounded hold ends; emphasis-only changes
        // still update. An accepted newer frame stays pending for
        // presentation (labelled "Updating") rather than discarding the
        // gesture.
        let hold_defers = app.graph.state(tab).is_some_and(|state| {
            state.arbiter.has_candidate()
                && !emphasis_only_change(state.signature.as_ref(), &signature)
        });
        let frozen_same = app.graph.state(tab).is_some_and(|state| {
            state.frozen
                && state.signature.as_ref().is_some_and(|old| {
                    old.base == signature.base
                        && old.selected == signature.selected
                        && old.selected_nodes == signature.selected_nodes
                        && old.drag == signature.drag
                        && old.writing_target == signature.writing_target
                        && old.pins_count == signature.pins_count
                        && old.scheme == signature.scheme
                })
        });
        if rebuild
            && !frozen_same
            && !hold_defers
            && app
                .graph
                .state(tab)
                .is_some_and(|s| !s.updating && s.scope_result.is_some())
        {
            let mut view_positions = app
                .graph
                .state(tab)
                .filter(|state| state.frozen)
                .map_or_else(BTreeMap::new, |state| state.frozen_nodes.clone());
            view_positions.extend(pinned.iter().map(|(id, position)| (*id, *position)));
            let overrides = GeometryOverrides {
                selected: signature.selected,
                selected_nodes: signature.selected_nodes.clone(),
                writing_target: signature.writing_target,
                pinned: view_positions,
                drag_preview: signature.drag,
                scope: app
                    .graph
                    .state(tab)
                    .and_then(|state| state.scope_result.clone()),
            };
            let generation = app
                .graph
                .state(tab)
                .map_or(1, |state| state.geometry_generation.saturating_add(1));
            let outcome =
                RenderGeometry::build(&publication, &positions, &overrides, generation, scheme);
            if let Some(state) = app.graph.state_mut(tab) {
                match outcome {
                    Ok(geometry) => {
                        state.geometry_generation = generation;
                        state.geometry = Some(Arc::new(geometry));
                        state.geometry_key = Some(key);
                        state.signature = Some(signature);
                        state.stale = false;
                    }
                    Err(_) => {
                        // Refused pairs keep the last valid frame visibly
                        // and disable insertion until the pair agrees
                        // (§10.3); the discard is counted, never silent.
                        if let Some(state) = app.graph.state_mut(tab) {
                            state.stale = true;
                        }
                        app.graph.stale_discards_total += 1;
                    }
                }
            }
        }

        // Camera auto-fit and follow (manual-UX: a fresh graph must settle
        // into view like a network diagram, not stream past a stationary
        // viewport). While the user has not taken the camera, fit on the
        // first mount, keep fitting while the layout is running, and fit
        // once more when it settles. A pan or zoom hands the camera to the
        // user permanently for this tab.
        let layout_active = matches!(positions.state(), LayoutState::Active);
        if let Some(state) = app.graph.state_mut(tab) {
            let was_active = state.last_layout_active;
            state.last_layout_active = Some(layout_active);
            // Before the first mounted-bounds message arrives, fit against
            // a nominal viewport: a settled map in view is the point; the
            // next real-bounds generation refines it.
            let bounds = state.bounds.unwrap_or(Rectangle {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            });
            if !state.user_camera_moved
                && !state.auto_fit_settled
                // D-02: while a pair hold is pending, the camera keeps the
                // frozen hit geometry stable; the fit catches up after the
                // bounded hold ends.
                && !state.arbiter.has_candidate()
                && let Some(geometry) = state.geometry.clone()
                && ((!had_geometry && state.geometry.is_some())
                    || layout_active
                    || (was_active == Some(true) && !layout_active))
                && let Some(camera) = fit_camera_to_geometry(&geometry, bounds)
            {
                state.camera = camera;
                state.camera_pending = None;
                if !layout_active {
                    state.auto_fit_settled = state.bounds.is_some();
                    // Settled: persist the fitted camera once. While the
                    // simulation runs, the camera stays ephemeral so
                    // following it does not spam layout transactions.
                    commit_camera_value(app, tab, camera);
                }
            }
        }

        // Camera debounce commit (wheel bursts): one transaction at idle.
        let due = app
            .graph
            .state(tab)
            .and_then(|state| state.camera_pending)
            .is_some_and(|since| now - since >= CAMERA_COMMIT_IDLE);
        if due {
            commit_camera(app, tab);
        }
    }
}

/// Commit one tab's ephemeral camera into the authoritative model.
/// Camera that fits every mounted node into `bounds` with margin, or `None`
/// for an empty/unfittable geometry. Shared by the Fit command and the
/// auto-fit/follow camera so a first render settles into view instead of
/// streaming past a stationary viewport.
fn fit_camera_to_geometry(geometry: &RenderGeometry, bounds: Rectangle) -> Option<Camera> {
    if geometry.node_count() == 0 {
        return None;
    }
    let mut min = [f32::MAX; 2];
    let mut max = [f32::MIN; 2];
    let mut radius = 1.0_f32;
    for node in geometry.nodes() {
        radius = radius.max(node.size);
        for axis in 0..2 {
            min[axis] = min[axis].min(node.center[axis] - node.size);
            max[axis] = max[axis].max(node.center[axis] + node.size);
        }
    }
    if !min[0].is_finite() {
        return None;
    }
    let extent = (max[0] - min[0]).max(max[1] - min[1]).max(1.0) * 1.25;
    let center = [(min[0] + max[0]) / 2.0, (min[1] + max[1]) / 2.0];
    let mut camera = Camera::centered();
    camera.fit_extent(extent, [0.0, 0.0], [bounds.width, bounds.height]);
    // Tiny/new layouts must not turn dots into viewport-sized discs.
    camera.zoom = camera.zoom.min(24.0 / radius).max(Camera::ZOOM_MIN);
    camera.center = center;
    Some(camera)
}

pub fn commit_camera(app: &mut App, tab: TabId) {
    let Some(camera) = app.graph.state(tab).map(|state| state.camera) else {
        return;
    };
    if let Some(state) = app.graph.state_mut(tab) {
        state.camera_pending = None;
    }
    commit_camera_value(app, tab, camera);
}

fn commit_camera_value(app: &mut App, tab: TabId, camera: Camera) {
    // A composing view blocks layout transactions (§7.4/D-03): defer to
    // the next pump rather than force the change.
    if app.has_composition() {
        if let Some(state) = app.graph.state_mut(tab) {
            state.camera_pending = Some(Instant::now());
        }
        return;
    }
    let validated = GraphCamera::new(
        [
            camera.center[0].clamp(-GraphCamera::MAX_COORDINATE, GraphCamera::MAX_COORDINATE),
            camera.center[1].clamp(-GraphCamera::MAX_COORDINATE, GraphCamera::MAX_COORDINATE),
        ],
        camera
            .zoom
            .clamp(GraphCamera::MIN_ZOOM, GraphCamera::MAX_ZOOM),
    );
    let Ok(camera) = validated else {
        return;
    };
    // Only commit when the authoritative camera actually differs; the
    // reducer's NoChange guard is not a UX channel.
    let differs = app
        .workspace
        .tab(&tab)
        .is_some_and(|session| match session.kind() {
            ViewKind::Graph(view) => view.camera() != camera,
            _ => false,
        });
    if !differs {
        return;
    }
    app.dispatch_layout(|revision| LayoutAction::SetGraphCamera {
        expected_revision: revision,
        tab,
        camera,
    });
}

/// Handle one typed widget message for a graph tab. The caller has already
/// validated the tab exists, is a visible graph view, and the captured
/// incarnation matches the current one (stale-widget guard, §8.2).
pub fn handle_message(app: &mut App, tab: TabId, message: GraphMessage) {
    let now = Instant::now();
    match message {
        GraphMessage::BoxPreview { first, second } => {
            if first.into_iter().chain(second).all(f32::is_finite)
                && let Some(state) = app.graph.state_mut(tab)
            {
                state.box_preview = Some((first, second));
                state.arbiter.cancel();
            }
        }
        GraphMessage::BoxSelected { first, second } => {
            let Some(state) = app.graph.state(tab) else {
                return;
            };
            let Some(geometry) = &state.geometry else {
                return;
            };
            if !first.into_iter().chain(second).all(f32::is_finite) {
                return;
            }
            let selected: std::collections::BTreeSet<_> = geometry
                .nodes()
                .iter()
                .enumerate()
                .filter(|(_, node)| {
                    (0..2).all(|axis| {
                        node.center[axis] >= first[axis].min(second[axis])
                            && node.center[axis] <= first[axis].max(second[axis])
                    })
                })
                .filter_map(|(index, _)| geometry.node_id(index))
                .collect();
            if selected.len() > 1000 {
                app.status = "Selection exceeds 1000 entities; choose a smaller region".into();
                if let Some(state) = app.graph.state_mut(tab) {
                    state.box_preview = None;
                }
                return;
            }
            let primary = selected.first().copied();
            if let Some(state) = app.graph.state_mut(tab) {
                state.selected_nodes = selected;
                state.selection = primary.map(|id| HitNode::new(id, id.kind));
                if state.selection.is_some() {
                    state.controls_open = false;
                }
                state.box_preview = None;
                state.arbiter.cancel();
            }
            if app
                .graph
                .state(tab)
                .is_some_and(|s| s.selected_nodes.len() == 1)
            {
                publish_selection(
                    app,
                    tab,
                    HitNode::new(
                        primary.expect("one selection"),
                        primary.expect("one selection").kind,
                    ),
                );
            } else {
                publish_empty(app, tab);
            }
        }
        GraphMessage::Viewport { .. } | GraphMessage::ScaleFactor { .. } => {}
        GraphMessage::PanByPixels { dx, dy } => {
            let Some(state) = app.graph.state_mut(tab) else {
                return;
            };
            let Some(world) = GraphMessage::PanByPixels { dx, dy }.pan_world_at(state.camera.zoom)
            else {
                return;
            };
            state.camera.pan_by(world[0], world[1]);
            state.camera_pending = Some(now);
            state.user_camera_moved = true;
        }
        GraphMessage::ZoomAt { anchor, factor } => {
            let Some(state) = app.graph.state_mut(tab) else {
                return;
            };
            let bounds = state.bounds.unwrap_or(Rectangle {
                x: 0.0,
                y: 0.0,
                width: 800.0,
                height: 600.0,
            });
            state
                .camera
                .zoom_at(anchor, [0.0, 0.0], [bounds.width, bounds.height], factor);
            state.camera_pending = Some(now);
            state.user_camera_moved = true;
        }
        GraphMessage::NodeDragged { node, world } => {
            let Some(state) = app.graph.state_mut(tab) else {
                return;
            };
            state.drag_preview = Some((node, world));
        }
        GraphMessage::Released { node, world, kind } => {
            let selected_before = app.graph.state(tab).and_then(|state| state.selection);
            let _ = selected_before;
            match kind {
                ReleaseKind::PanCompleted => {
                    if let Some(state) = app.graph.state_mut(tab) {
                        state.arbiter.cancel();
                    }
                    // One commit transaction for the completed pan.
                    if let Some(camera) = app.graph.state(tab).map(|state| state.camera) {
                        if let Some(state) = app.graph.state_mut(tab) {
                            state.camera_pending = None;
                        }
                        commit_camera_value(app, tab, camera);
                    }
                }
                ReleaseKind::NodeDragCompleted { node } => {
                    let preview = app
                        .graph
                        .state_mut(tab)
                        .and_then(|state| state.drag_preview.take());
                    if let Some(state) = app.graph.state_mut(tab) {
                        state.arbiter.cancel();
                    }
                    let Some((_, position)) = preview.filter(|(dragged, _)| *dragged == node)
                    else {
                        return;
                    };
                    let Some(record) = record_for(app, node.id) else {
                        app.status = "Dragged node is no longer in the current frame".into();
                        return;
                    };
                    if record.id.kind == NodeKind::Unresolved || record.id.id > i64::MAX as u64 {
                        app.status =
                            "Only saved note and attachment positions can be retained".into();
                        return;
                    }
                    let key = pin_key_for(&record);
                    match app.graph.pins.execute(PinCommand::Pin { key, position }) {
                        Ok(changed) => {
                            flush_pins(app, now);
                            if changed {
                                app.status = format!(
                                    "Pinned {} at ({:.0}, {:.0}) — shared across graph views",
                                    record.title, position[0], position[1]
                                );
                            }
                        }
                        Err(error) => {
                            app.status = format!("Pin rejected: {error}");
                        }
                    }
                }
                ReleaseKind::Click => {
                    // Pairing key: identity epoch (D-02), not the render
                    // generation; the router already validated the frame.
                    let Some(epoch) = app
                        .graph
                        .state(tab)
                        .and_then(|state| state.geometry_key)
                        .map(|key| key.epoch)
                    else {
                        return;
                    };
                    let zoom = app.graph.state(tab).map_or(1.0, |state| state.camera.zoom);
                    let Some(state) = app.graph.state_mut(tab) else {
                        return;
                    };
                    let outcome = state.arbiter.click(now, node, world, epoch, zoom);
                    match outcome {
                        GestureOutcome::Select(hit) => {
                            state.selection = Some(hit);
                            state.controls_open = false;
                            publish_selection(app, tab, hit);
                        }
                        GestureOutcome::PublishEmptySelection => {
                            state.selection = None;
                            state.selected_nodes.clear();
                            publish_empty(app, tab);
                        }
                        GestureOutcome::ActivateNode(hit) => {
                            // D-02: a completed pair activates (opens) the
                            // node — it never mutates text. Insertion stays
                            // with the explicit Insert commands.
                            state.selection = Some(hit);
                            state.controls_open = false;
                            publish_selection(app, tab, hit);
                            open_node(app, Some(tab), hit.id);
                        }
                    }
                }
            }
        }
    }
    app.refresh_semantics();
}

/// Publish this graph tab's exploration selection (or its deliberate
/// empty) through the workspace source rules (§8.1, D-04).
fn publish_selection(app: &mut App, tab: TabId, hit: HitNode) {
    if let Some(state) = app.graph.state_mut(tab) {
        state.selected_nodes = std::collections::BTreeSet::from([hit.id]);
        state.occurrence_offset = 0;
    }
    let incarnation = app
        .workspace
        .tab(&tab)
        .map(|session| session.interaction().incarnation)
        .unwrap_or(1);
    match record_for(app, hit.id).as_ref().and_then(locator_for) {
        Some(locator) => {
            if app
                .workspace
                .publish_exploration(tab, incarnation, Some(locator))
                .is_ok()
            {
                app.reconcile_contexts();
            }
        }
        None => {
            // Attachments and unrepresentable paths publish the deliberate
            // empty selection: sinks show "No selection" instead of
            // silently keeping a stale source (D-04). The reserved band
            // carries the node's own explicit actions (open attachment,
            // create/locate for an unresolved target).
            publish_empty(app, tab);
        }
    }
}

pub(crate) fn publish_empty(app: &mut App, tab: TabId) {
    let incarnation = app
        .workspace
        .tab(&tab)
        .map(|session| session.interaction().incarnation)
        .unwrap_or(1);
    let _ = app.workspace.publish_exploration(tab, incarnation, None);
    app.reconcile_contexts();
}

/// The *explicit* insert path (`Insert` command, Ctrl/Cmd+Enter): refusal
/// with repair guidance when there is no valid writing target — never
/// navigation, never a manufactured caret, never a fallback editor (D-02:
/// activation and insertion are distinct intents).
fn request_node_link(app: &mut App, tab: TabId, hit: HitNode) {
    let Some(record) = record_for(app, hit.id) else {
        app.status = "Selected node left the current frame; choose it again".into();
        return;
    };
    let Some(locator) = locator_for(&record) else {
        app.status = if record.id.kind == NodeKind::Attachment {
            "Attachments open in the inspector; they are not link-insertion targets".into()
        } else if record.id.kind == NodeKind::Unresolved {
            "Unresolved targets offer Create or Locate, not insertion".into()
        } else {
            "This note's path cannot be serialized as a wikilink target".into()
        };
        return;
    };
    let stale = app
        .graph
        .state(tab)
        .is_some_and(|state| state.stale || state.updating);
    if stale {
        app.status =
            "Graph frame is stale; insertion is disabled until target validation is current".into();
        return;
    }
    if app.valid_writing_target().is_none() {
        app.status =
            "Choose an editor and place the cursor — then use Insert or press Ctrl+Enter".into();
        return;
    }
    let expected = app
        .graph
        .state(tab)
        .and_then(|state| state.geometry_key)
        .map(|key| flavite_api::LinkTargetIdentity {
            epoch: key.epoch,
            entity_id: record.id.id,
            native_path: record.path_native.clone(),
        });
    app.request_link_insert_expected(locator, expected, None);
}

/// Handle one graph command (keyboard/palette/menu/a11y row).
pub fn command(app: &mut App, tab: TabId, command: GraphCommand) {
    if !app.is_tab_visible(tab)
        || app
            .workspace
            .tab(&tab)
            .is_none_or(|s| !matches!(s.kind(), ViewKind::Graph(_)))
    {
        return;
    }
    match command {
        GraphCommand::ToggleControls
        | GraphCommand::ToggleLayoutControls
        | GraphCommand::ToggleDisplayControls => {
            if let Some(state) = app.graph.state_mut(tab) {
                match command {
                    GraphCommand::ToggleControls => state.controls_open = !state.controls_open,
                    GraphCommand::ToggleLayoutControls => {
                        state.layout_controls_open = !state.layout_controls_open
                    }
                    _ => state.display_controls_open = !state.display_controls_open,
                }
            }
        }
        GraphCommand::NextListPage
        | GraphCommand::PreviousListPage
        | GraphCommand::NextOccurrences
        | GraphCommand::PreviousOccurrences => {
            if let Some(state) = app.graph.state_mut(tab) {
                match command {
                    GraphCommand::NextListPage => {
                        if state
                            .scope_result
                            .as_ref()
                            .is_some_and(|s| state.list_offset + 50 < s.nodes.len())
                        {
                            state.list_offset += 50;
                        }
                    }
                    GraphCommand::PreviousListPage => {
                        state.list_offset = state.list_offset.saturating_sub(50)
                    }
                    GraphCommand::NextOccurrences => {
                        state.occurrence_offset = state.occurrence_offset.saturating_add(12)
                    }
                    _ => state.occurrence_offset = state.occurrence_offset.saturating_sub(12),
                }
            }
        }
        GraphCommand::ToggleSelected(node) => {
            let Some(state) = app.graph.state_mut(tab) else {
                return;
            };
            if !state.selected_nodes.remove(&node) {
                if state.selected_nodes.len() >= 1000 {
                    app.status = "Selection limit is 1000 entities".into();
                    return;
                }
                state.selected_nodes.insert(node);
            }
            state.selection = state
                .selected_nodes
                .first()
                .copied()
                .map(|id| HitNode::new(id, id.kind));
            if state.selected_nodes.len() == 1 {
                let hit = state.selection.expect("one selection");
                publish_selection(app, tab, hit);
            } else {
                publish_empty(app, tab);
            }
        }
        GraphCommand::ConfirmRemoval(sequence) => {
            crate::host::occurrences::confirm(app, tab, sequence)
        }
        GraphCommand::CancelRemoval => {
            crate::host::occurrences::cancel(app);
        }
        GraphCommand::ToggleFilters
        | GraphCommand::Depth(_)
        | GraphCommand::Direction(_)
        | GraphCommand::Unresolved(_)
        | GraphCommand::Attachments(_)
        | GraphCommand::Embeds(_)
        | GraphCommand::Any(_)
        | GraphCommand::Negate(_)
        | GraphCommand::ApplyFilters
        | GraphCommand::ClearFilters
        | GraphCommand::UndoFilters
        | GraphCommand::ExploreSelected
        | GraphCommand::RevealSelected
        | GraphCommand::ClearSelection => {
            crate::graph::filters::command(app, tab, command);
        }
        GraphCommand::ToggleFreeze => {
            if let Some(state) = app.graph.state_mut(tab) {
                state.frozen = !state.frozen;
                state.frozen_nodes.clear();
                if state.frozen
                    && let Some(geometry) = &state.geometry
                {
                    for (index, node) in geometry.nodes().iter().enumerate() {
                        if let Some(id) = geometry.node_id(index) {
                            state.frozen_nodes.insert(id, node.center);
                        }
                    }
                }
                let action = if state.frozen { "frozen" } else { "resumed" };
                app.status = format!("Layout {action} for this graph view");
            }
        }
        GraphCommand::ToggleList => {
            if let Some(state) = app.graph.state_mut(tab) {
                state.list_mode = !state.list_mode;
            }
        }
        GraphCommand::FitView => {
            let Some((geometry, bounds)) = app
                .graph
                .state(tab)
                .and_then(|state| Some((state.geometry.clone()?, state.bounds?)))
            else {
                return;
            };
            if let Some(camera) = fit_camera_to_geometry(&geometry, bounds) {
                if let Some(state) = app.graph.state_mut(tab) {
                    state.camera = camera;
                    state.camera_pending = None;
                }
                commit_camera_value(app, tab, camera);
            }
        }
        GraphCommand::OpenSelected | GraphCommand::InsertSelected => {
            let selected = app.graph.state(tab).and_then(|state| state.selection);
            let Some(hit) = selected else {
                app.status = "Select a node first (click a dot or a list row)".into();
                return;
            };
            if command == GraphCommand::OpenSelected {
                open_node(app, Some(tab), hit.id);
            } else {
                request_node_link(app, tab, hit);
            }
        }
        GraphCommand::UnpinSelected => {
            let selected = app.graph.state(tab).and_then(|state| state.selection);
            let Some(hit) = selected else {
                app.status = "Select a node first".into();
                return;
            };
            let Some(record) = record_for(app, hit.id) else {
                return;
            };
            let key = pin_key_for(&record);
            let removed = app
                .graph
                .pins
                .execute(PinCommand::Unpin { key })
                .unwrap_or(false);
            if removed {
                flush_pins(app, Instant::now());
                app.status = format!("Unpinned {}", record.title);
            } else {
                app.status = "That node has no pin to remove".into();
            }
        }
        GraphCommand::RetryService => {
            if let Some(flavite) = app.flavite.as_ref() {
                // The relayout directive both wakes the service and
                // reinstalls the layout worker; the retired §12.1 pulse
                // relay is not needed (RFC-0005 S5).
                flavite.graph_relayout();
            }
            app.status = "Graph service retried".into();
        }
        GraphCommand::RetryPins => {
            app.graph.pins.retry_now();
            flush_pins(app, Instant::now());
        }
        GraphCommand::Select(node) => {
            let Some(record) = record_for(app, node) else {
                app.status = "That node is no longer in the current frame".into();
                return;
            };
            let hit = HitNode::new(node, record.id.kind);
            if let Some(state) = app.graph.state_mut(tab) {
                state.selection = Some(hit);
                state.controls_open = false;
            }
            publish_selection(app, tab, hit);
        }
        GraphCommand::Open(node) => open_node(app, Some(tab), node),
        GraphCommand::Insert(node) => {
            let Some(record) = record_for(app, node) else {
                return;
            };
            let Some(locator) = locator_for(&record) else {
                app.status = "Attachments and unrepresentable paths cannot be inserted".into();
                return;
            };
            if app.valid_writing_target().is_none() {
                app.status = "Choose an editor and place the cursor before inserting".into();
                return;
            }
            app.request_link_insert(locator);
        }
        GraphCommand::OpenSelectedInTabs => {
            let selected = app
                .graph
                .state(tab)
                .map(|state| state.selected_nodes.clone())
                .unwrap_or_default();
            if selected.is_empty() {
                app.status = "Select nodes first (click a dot, a list row, or drag a box)".into();
                return;
            }
            // Deterministic native-key order (D-03): byte order of the
            // native path identity, then entity id for identical paths.
            let mut records: Vec<GraphNodeRecord> = selected
                .iter()
                .filter_map(|id| record_for(app, *id))
                .filter(|record| locator_for(record).is_some())
                .collect();
            records.sort_by(|a, b| {
                a.path_native
                    .cmp(&b.path_native)
                    .then_with(|| a.id.id.cmp(&b.id.id))
            });
            if records.len() > crate::host::open_intents::MAX_MULTI_OPEN {
                app.status = format!(
                    "{} notes selected — open at most {} per invocation; reduce the selection first",
                    records.len(),
                    crate::host::open_intents::MAX_MULTI_OPEN
                );
                return;
            }
            let count = records.len();
            let tabs_before = app.workspace.tabs().count();
            for record in records {
                // checked above
                let locator = locator_for(&record).expect("filtered above");
                app.open_intent(crate::host::open_intents::OpenIntent::new_tab(locator));
            }
            // Honest accounting: a refused destination (no live group)
            // never inflates the opened count.
            let opened = app.workspace.tabs().count().saturating_sub(tabs_before);
            app.status = if opened == count {
                format!("Opened {count} tabs (NewTab bypasses view dedup; buffers shared)")
            } else {
                format!(
                    "Opened {opened} of {count} selected notes; {}/{count} destinations refused (no room for a document group)",
                    count - opened
                )
            };
        }
    }
    app.refresh_semantics();
}

/// Explicit navigation for one node (D-02/D-03: Open is its own action and
/// never mutates documents; activation routes the typed open intent through
/// the shared destination policy, never an accidental global destination).
/// Activation resolves the *current* record: a removed node fails here even
/// if its old dot is still painted in the frozen hold geometry.
pub(crate) fn open_node(app: &mut App, source: Option<TabId>, node: NodeId) {
    let Some(record) = record_for(app, node) else {
        app.status = "That node is no longer in the current frame".into();
        return;
    };
    if let Some(locator) = locator_for(&record) {
        app.open_intent(crate::host::open_intents::OpenIntent::reuse(
            locator, source,
        ));
        return;
    }
    // D-02: each non-note target has its own explicit intent — an
    // attachment opens the existing local inspector, an unresolved note
    // offers Create-or-Locate, and nothing is ever created or opened by
    // guessing (FR-19).
    match record.id.kind {
        NodeKind::Attachment => {
            if let Some(tab) = source {
                open_attachment(app, tab, &record);
            } else {
                app.status = "Select the attachment in a graph view first".into();
            }
        }
        NodeKind::Unresolved => {
            app.status = if unresolved_is_representable(&record) {
                "Unresolved link target — use Create note or Locate in the selection band; navigation never creates notes".into()
            } else {
                "Unresolved link target — its path cannot be represented as a note".into()
            };
        }
        NodeKind::Note => {
            app.status = if record.binding_blocked {
                "Another file now owns this path (rename conflict) — resolve it before opening"
                    .into()
            } else {
                "This path cannot be opened as a note".into()
            };
        }
    }
}

/// D-02: activating an attachment node opens the existing local attachment
/// inspector — the same core-validated open primitive the reading surface
/// uses. Never a note open, never a guessed target; the OS launch only
/// happens after the core validation returns a contained path.
fn open_attachment(app: &mut App, tab: TabId, record: &GraphNodeRecord) {
    if app.graph.attachment_open_pending(tab) {
        app.status = "An attachment is already opening".into();
        return;
    }
    let Some(api) = app.flavite.as_ref() else {
        app.status = "No vault is open".into();
        return;
    };
    let incarnation = app
        .workspace
        .tab(&tab)
        .map_or(0, |session| session.interaction().incarnation);
    match api.query_service().submit(
        tab.raw(),
        incarnation,
        0,
        QueryPayload::AttachmentOpen {
            path: record.native_path.clone(),
        },
    ) {
        Ok(token) => {
            app.graph.attachment_opens.insert(token, tab);
            app.status = "Opening local attachment…".into();
        }
        Err(_) => app.status = "Attachment service busy; try again".into(),
    }
}

/// Ensure every mounted graph tab is attached (covers restore/layout
/// switches where `Effect::ViewAttached` ordering is coarse) and refresh
/// ephemeral cameras from the authoritative view kind when the user has no
/// pending gesture (e.g. after a tab move, the camera identity survives;
/// after a restore, it comes from disk).
pub fn sync_hosts(app: &mut App) {
    let mut graph_tabs = Vec::new();
    for (tab, session) in app.workspace.tabs() {
        if let ViewKind::Graph(view) = session.kind() {
            graph_tabs.push((*tab, view.camera()));
        }
    }
    for (tab, camera) in graph_tabs {
        if app.graph.state(tab).is_none() {
            app.graph
                .attach(tab, &flavite_workspace::GraphView::new(camera));
        } else if let Some(state) = app.graph.state_mut(tab)
            && state.camera_pending.is_none()
            && state.drag_preview.is_none()
        {
            state.camera = Camera {
                center: camera.center,
                zoom: camera.zoom,
            };
        }
    }
}

/// Cancel in-flight graph gesture state for every tab (Escape, window
/// blur): node-drag preview restored without a pin, click candidates
/// dropped, camera commit kept (pan/zoom are non-destructive).
pub fn cancel_gestures(app: &mut App) {
    for tab in app.graph.tab_ids() {
        if let Some(state) = app.graph.state_mut(tab) {
            if state.drag_preview.take().is_some() {
                app.status = "Node drag cancelled; the prior position is restored".into();
            }
            state.box_preview = None;
            state.arbiter.cancel();
        }
    }
}

/// The message wrapper the graph widgets publish through `App`.
#[must_use]
pub fn graph_message(
    tab: TabId,
    incarnation: u64,
    key: RenderKey,
    message: GraphMessage,
) -> Message {
    Message::GraphAction {
        tab,
        incarnation,
        key,
        message,
    }
}
