//! The pure workspace reducer (RFC-0003 D-02, §7.4).
//!
//! Every change to [`WorkspaceState`] flows through [`reduce`]:
//!
//! 1. reject if the action's expected revision is stale,
//! 2. apply to a small copy (no I/O, no side effects),
//! 3. normalize empty groups,
//! 4. validate tree/tab/focus invariants and bounds,
//! 5. commit with a bumped revision and derived view-lifecycle effects.
//!
//! Rejected actions leave state byte-identical (AC-W01). The reducer never
//! allocates operating-system resources and never inspects the toolkit.

use crate::context::{ContextBinding, ContextViewKind, ExplorationSelection};
use crate::ids::{GroupId, Ids, SplitId, TabId, WorkspaceId};
use crate::layout::{
    Arrangement, DEFAULT_RATIO_PPM, Edge, LayoutNode, MAX_DEPTH, MAX_GROUPS, MAX_RATIO_PPM,
    MAX_REOPEN, MAX_TABS, MIN_RATIO_PPM, Side, TabDropTarget, TabGroup,
};
use crate::validate;
use crate::view::{NoteLoad, NoteLocator, NoteView, ViewKind, ViewSession};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// A typed, total failure of one layout transaction (§7.4). Every variant
/// is user-relevant: the host renders the reason verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceError {
    /// The transaction was built against an older workspace revision.
    StaleRevision {
        /// Revision the action expected.
        expected: u64,
        /// Current revision.
        actual: u64,
    },
    /// The named group does not exist.
    UnknownGroup(GroupId),
    /// The named tab does not exist.
    UnknownTab(TabId),
    /// The named split does not exist.
    UnknownSplit(SplitId),
    /// The workspace is at its supported group count.
    GroupLimitReached,
    /// The workspace is at its supported tab count.
    TabLimitReached,
    /// The layout would exceed its supported depth.
    DepthLimitReached,
    /// The desired ratio is outside the persisted range.
    InvalidRatio(u32),
    /// Reopen was requested with an empty closed-tab history.
    NothingToReopen,
    /// The view kind cannot be duplicated.
    NotDuplicateable(TabId),
    /// The operation would leave the state exactly unchanged.
    NoChange,
    /// A committed candidate failed post-transaction validation. This is a
    /// model bug, reported loudly instead of committed.
    InvariantViolation(String),
}

impl std::fmt::Display for WorkspaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleRevision { expected, actual } => {
                write!(
                    f,
                    "stale layout revision: expected {expected}, current {actual}"
                )
            }
            Self::UnknownGroup(group) => write!(f, "unknown group {group}"),
            Self::UnknownTab(tab) => write!(f, "unknown tab {tab}"),
            Self::UnknownSplit(split) => write!(f, "unknown split {split}"),
            Self::GroupLimitReached => write!(f, "workspace already has {MAX_GROUPS} groups"),
            Self::TabLimitReached => write!(f, "workspace already has {MAX_TABS} tabs"),
            Self::DepthLimitReached => write!(f, "layout would exceed depth {MAX_DEPTH}"),
            Self::InvalidRatio(ratio) => {
                write!(
                    f,
                    "split ratio {ratio} ppm is outside {MIN_RATIO_PPM}..={MAX_RATIO_PPM}"
                )
            }
            Self::NothingToReopen => write!(f, "no closed tab to reopen"),
            Self::NotDuplicateable(tab) => write!(f, "an empty tab cannot be duplicated ({tab})"),
            Self::NoChange => write!(f, "operation would not change the workspace"),
            Self::InvariantViolation(detail) => {
                write!(f, "workspace invariant violated: {detail}")
            }
        }
    }
}

impl std::error::Error for WorkspaceError {}

/// The authoritative workspace state (§7.2). Maps and the tree are private;
/// module constructors preserve the invariants documented in §7.3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    id: WorkspaceId,
    revision: u64,
    ids: Ids,
    tree: LayoutNode,
    groups: BTreeMap<GroupId, TabGroup>,
    tabs: BTreeMap<TabId, ViewSession>,
    focus: TabId,
    document_anchor: Option<TabId>,
    document_publication: u64,
    exploration: Option<ExplorationSelection>,
    exploration_sequence: u64,
    writing_target: Option<crate::input::WritingTarget>,
    maximized: Option<GroupId>,
    recently_closed: VecDeque<NoteLocator>,
}

/// One committed transaction handed back to the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The new state; revision is the old revision plus one.
    pub state: WorkspaceState,
    /// View lifecycle effects derived from the diff of both tab maps.
    pub effects: Vec<Effect>,
}

/// Host-observable consequences of a commit. The host owns resources;
/// the model only reports what appeared, disappeared, or changed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// A view appeared; the host should attach resources (open the note).
    ViewAttached {
        /// New tab identity.
        tab: TabId,
        /// Logical kind to attach.
        kind: ViewKind,
    },
    /// A view disappeared; the host releases resources without touching
    /// the shared buffer (D-10).
    ViewDetached {
        /// Removed tab identity.
        tab: TabId,
        /// Logical kind the host had attached.
        kind: ViewKind,
    },
    /// A note view's load state changed; the host may attach its buffer
    /// binding or surface the failure.
    NoteLoadChanged {
        /// The tab whose note load state changed.
        tab: TabId,
        /// The new load state.
        load: NoteLoad,
    },
    /// A context tool's effective source may have changed; the host must
    /// re-resolve its binding through [`crate::context::ContextRouter`].
    ContextRebound {
        /// The context tool tab to re-resolve.
        tab: TabId,
    },
}

/// The result of one [`reduce`] call.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The transaction committed.
    Committed(Commit),
    /// The transaction was rejected; the input state remains valid.
    Rejected(WorkspaceError),
}

/// A typed layout transaction (§7.4). Every action carries the revision it
/// was built against. Actions are constructed on the UI thread and consumed
/// immediately, so the size spread between variants is harmless.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutAction {
    /// Open a source-reading note tab at the end of `group`.
    OpenNote {
        expected_revision: u64,
        locator: NoteLocator,
        group: GroupId,
    },
    /// Open an empty tab in `group`.
    OpenEmpty {
        expected_revision: u64,
        group: GroupId,
    },
    /// Open the vault note list in `group`.
    OpenNotes {
        expected_revision: u64,
        group: GroupId,
    },
    /// Open a context tool (Preview/Backlinks) with an explicit binding.
    OpenContext {
        expected_revision: u64,
        view: ContextViewKind,
        binding: ContextBinding,
        group: GroupId,
    },
    /// Rebind an existing context tool; sinks cannot follow sinks, which
    /// [`ContextViewKind`] construction and this arm's validation enforce.
    SetContextBinding {
        expected_revision: u64,
        tab: TabId,
        binding: ContextBinding,
    },
    /// Activate and focus a tab.
    ActivateTab { expected_revision: u64, tab: TabId },
    /// Focus the active tab of `group` without changing it.
    FocusGroup {
        expected_revision: u64,
        group: GroupId,
    },
    /// Close one tab; the last workspace tab is replaced by a real Empty tab.
    CloseTab { expected_revision: u64, tab: TabId },
    /// Close every tab of `group`; the group collapses.
    CloseGroup {
        expected_revision: u64,
        group: GroupId,
    },
    /// Duplicate a note or list view with a new tab identity (FR-02).
    DuplicateTab { expected_revision: u64, tab: TabId },
    /// Reopen the most recently closed view in the focused group.
    ReopenClosed { expected_revision: u64 },
    /// Move one tab to a typed drop target (§7.4 gesture semantics).
    MoveTab {
        expected_revision: u64,
        tab: TabId,
        target: TabDropTarget,
    },
    /// Append every tab of `source` to `destination`, keeping the
    /// destination active tab, then remove the source group (§7.4).
    MergeGroups {
        expected_revision: u64,
        source: GroupId,
        destination: GroupId,
    },
    /// Move the whole `source` group into a new split beside `destination`.
    MoveGroupSplit {
        expected_revision: u64,
        source: GroupId,
        destination: GroupId,
        arrangement: Arrangement,
        side: Side,
    },
    /// Move one group to an outer edge of the whole tree.
    MoveGroupEdge {
        expected_revision: u64,
        group: GroupId,
        edge: Edge,
    },
    /// Duplicate the active view of `group` into a new split beside it
    /// (§7.4: splitting duplicates the view intentionally).
    SplitGroup {
        expected_revision: u64,
        group: GroupId,
        arrangement: Arrangement,
    },
    /// Persist a new desired ratio for one split (released drag/resize).
    ResizeSplit {
        expected_revision: u64,
        split: SplitId,
        desired_ratio_ppm: u32,
    },
    /// Manually maximize or restore `group` (D-13: manual and adaptive
    /// maximize are distinct; adaptive maximize never mutates the tree).
    ToggleMaximize {
        expected_revision: u64,
        group: GroupId,
    },
    /// A reserved note view finished opening successfully.
    NoteLoadReady { expected_revision: u64, tab: TabId },
    /// A reserved note view failed to open (missing note, denial, error).
    NoteLoadFailed { expected_revision: u64, tab: TabId },
    /// Advance focus to the next or previous tab of the focused group.
    CycleTab {
        expected_revision: u64,
        forward: bool,
    },
    /// Advance focus to the next or previous group in stable traversal
    /// order (F6/Shift+F6, §7.4).
    CycleGroup {
        expected_revision: u64,
        forward: bool,
    },
}

impl LayoutAction {
    /// The revision this action was built against.
    #[must_use]
    pub const fn expected_revision(&self) -> u64 {
        match self {
            Self::OpenNote {
                expected_revision, ..
            }
            | Self::OpenEmpty {
                expected_revision, ..
            }
            | Self::OpenNotes {
                expected_revision, ..
            }
            | Self::OpenContext {
                expected_revision, ..
            }
            | Self::SetContextBinding {
                expected_revision, ..
            }
            | Self::ActivateTab {
                expected_revision, ..
            }
            | Self::FocusGroup {
                expected_revision, ..
            }
            | Self::CloseTab {
                expected_revision, ..
            }
            | Self::CloseGroup {
                expected_revision, ..
            }
            | Self::DuplicateTab {
                expected_revision, ..
            }
            | Self::ReopenClosed {
                expected_revision, ..
            }
            | Self::MoveTab {
                expected_revision, ..
            }
            | Self::MergeGroups {
                expected_revision, ..
            }
            | Self::MoveGroupSplit {
                expected_revision, ..
            }
            | Self::MoveGroupEdge {
                expected_revision, ..
            }
            | Self::SplitGroup {
                expected_revision, ..
            }
            | Self::ResizeSplit {
                expected_revision, ..
            }
            | Self::ToggleMaximize {
                expected_revision, ..
            }
            | Self::NoteLoadReady {
                expected_revision, ..
            }
            | Self::NoteLoadFailed {
                expected_revision, ..
            }
            | Self::CycleTab {
                expected_revision, ..
            }
            | Self::CycleGroup {
                expected_revision, ..
            } => *expected_revision,
        }
    }
}

impl WorkspaceState {
    /// A fresh single-group workspace containing one real Empty tab.
    #[must_use]
    pub fn new() -> Self {
        let mut ids = Ids::new();
        Self::with_ids(&mut ids)
    }

    /// A fresh workspace driven by a caller-owned generator (tests seed it).
    #[must_use]
    pub fn with_ids(ids: &mut Ids) -> Self {
        let id = ids.workspace_id();
        let group = ids.group_id();
        let tab = ids.tab_id();
        let mut groups = BTreeMap::new();
        groups.insert(group, TabGroup::new(tab));
        let mut tabs = BTreeMap::new();
        tabs.insert(tab, ViewSession::new(tab, ViewKind::Empty));
        Self {
            id,
            revision: 0,
            ids: ids.clone(),
            tree: LayoutNode::Group(group),
            groups,
            tabs,
            focus: tab,
            document_anchor: None,
            document_publication: 0,
            exploration: None,
            exploration_sequence: 0,
            writing_target: None,
            maximized: None,
            recently_closed: VecDeque::new(),
        }
    }

    /// Workspace identity.
    #[must_use]
    pub const fn id(&self) -> WorkspaceId {
        self.id
    }

    /// Monotonic transaction revision.
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// The layout tree.
    #[must_use]
    pub const fn tree(&self) -> &LayoutNode {
        &self.tree
    }

    /// All groups in map order.
    pub fn groups(&self) -> impl Iterator<Item = (&GroupId, &TabGroup)> {
        self.groups.iter()
    }

    /// One group, if present.
    #[must_use]
    pub fn group(&self, id: &GroupId) -> Option<&TabGroup> {
        self.groups.get(id)
    }

    /// All tabs in map order.
    pub fn tabs(&self) -> impl Iterator<Item = (&TabId, &ViewSession)> {
        self.tabs.iter()
    }

    /// One tab, if present.
    #[must_use]
    pub fn tab(&self, id: &TabId) -> Option<&ViewSession> {
        self.tabs.get(id)
    }

    /// Acknowledge host-visible editor input for a note view. This updates
    /// logical input state without changing the structural layout revision.
    /// The widget event must match the current tab incarnation and advance
    /// its interaction sequence.
    ///
    /// # Errors
    /// Returns an error when the tab is missing, the interaction is stale or
    /// invalid for the current text, or a focused caret has no buffer identity.
    pub fn acknowledge_interaction(
        &mut self,
        interaction: crate::input::EditorInteraction,
        text: &str,
    ) -> Result<(), &'static str> {
        let tab = interaction.tab;
        let current = self.tabs.get(&tab).ok_or("interaction tab is closed")?;
        interaction.validate(tab, current.interaction().incarnation, text.len())?;
        if interaction.sequence <= current.interaction().sequence {
            return Err("interaction sequence is stale");
        }
        if !text.is_char_boundary(interaction.viewport_anchor)
            || interaction.selections.iter().any(|selection| {
                !text.is_char_boundary(selection.anchor) || !text.is_char_boundary(selection.head)
            })
        {
            return Err("interaction position is not a UTF-8 boundary");
        }
        if interaction.revision < current.interaction().revision {
            return Err("interaction revision is stale");
        }
        let focused = interaction.focused;
        // Blur/scroll acknowledgements may advance the interaction sequence
        // without changing the remembered caret. Preserve existing authority
        // only when every text/view guard still matches; never create it here.
        let retained_target = self
            .writing_target
            .filter(|target| {
                target.tab == tab
                    && target.incarnation == interaction.incarnation
                    && Some(target.buffer_id) == interaction.buffer_id
                    && target.revision == interaction.revision
                    && target.interaction_sequence == current.interaction().sequence
                    && interaction.composition == crate::input::CompositionState::Idle
                    && interaction.selections
                        == [crate::input::Selection::caret(target.caret_text_byte)]
            })
            .map(|mut target| {
                target.interaction_sequence = interaction.sequence;
                target
            });
        let target = if focused
            && interaction.composition == crate::input::CompositionState::Idle
            && interaction.selections.len() == 1
            && interaction.selections[0].is_caret()
        {
            Some(crate::input::WritingTarget {
                tab,
                incarnation: interaction.incarnation,
                buffer_id: interaction
                    .buffer_id
                    .ok_or("writing target has no buffer")?,
                revision: interaction.revision,
                interaction_sequence: interaction.sequence,
                caret_text_byte: interaction.selections[0].head,
            })
        } else {
            None
        };
        self.tabs
            .get_mut(&tab)
            .ok_or("interaction tab closed during acknowledgement")?
            .set_interaction(interaction);
        if focused {
            if self.document_anchor != Some(tab) {
                // A new document anchor is a new publication; context tools
                // re-resolve through the router and re-query by publication.
                self.document_publication = self.document_publication.saturating_add(1);
            }
            self.document_anchor = Some(tab);
            self.writing_target = target;
        } else if self.writing_target.is_some_and(|target| target.tab == tab) {
            self.writing_target = retained_target;
        }
        Ok(())
    }

    /// Release keyboard ownership without changing document or writing context.
    /// Advancing the sequence makes controlled runtimes observe the blur.
    pub fn release_editor_focus(&mut self, except: Option<TabId>) {
        for (tab, view) in &mut self.tabs {
            if Some(*tab) != except && view.interaction().focused {
                let mut interaction = view.interaction().clone();
                interaction.focused = false;
                if let Some(target) = self.writing_target.as_mut()
                    && target.tab == *tab
                    && target.interaction_sequence == interaction.sequence
                {
                    target.interaction_sequence = interaction.sequence.saturating_add(1);
                }
                interaction.sequence = interaction.sequence.saturating_add(1);
                view.set_interaction(interaction);
            }
        }
    }

    /// Note source most recently focused in an editable or reading view.
    #[must_use]
    pub const fn document_anchor(&self) -> Option<TabId> {
        self.document_anchor
    }

    /// Monotonic publication counter for the document anchor. Every anchor
    /// change or retirement increments it; context tools re-resolve when
    /// their observed publication differs.
    #[must_use]
    pub const fn document_publication(&self) -> u64 {
        self.document_publication
    }

    /// The publication sequence at which `tab` became the document anchor;
    /// zero when the tab is not the current anchor.
    #[must_use]
    pub fn document_publication_of(&self, tab: TabId) -> u64 {
        if self.document_anchor == Some(tab) {
            self.document_publication
        } else {
            0
        }
    }

    /// The current exploration selection, published by a source view.
    #[must_use]
    pub const fn exploration(&self) -> Option<&ExplorationSelection> {
        self.exploration.as_ref()
    }

    /// Publish an exploration selection from a source view (graph W07,
    /// search W10). Sinks cannot publish; the source tab must exist and be
    /// a source kind.
    ///
    /// # Errors
    /// Returns an error when the publishing tab is unknown or is a sink.
    pub fn publish_exploration(
        &mut self,
        source_tab: TabId,
        incarnation: u64,
        target: NoteLocator,
    ) -> Result<(), &'static str> {
        let session = self
            .tabs
            .get(&source_tab)
            .ok_or("exploration source tab is closed")?;
        if !crate::context::is_source_kind(session.kind()) {
            return Err("sinks cannot publish exploration selections");
        }
        self.exploration_sequence = self.exploration_sequence.saturating_add(1);
        self.exploration = Some(ExplorationSelection {
            source_tab,
            incarnation,
            sequence: self.exploration_sequence,
            target,
        });
        self.document_publication = self.document_publication.saturating_add(1);
        Ok(())
    }

    /// Publish an explicitly empty exploration selection for one source.
    /// Only the publishing source can clear its own selection.
    pub fn clear_exploration(&mut self, source_tab: TabId) {
        if self
            .exploration
            .as_ref()
            .is_some_and(|selection| selection.source_tab == source_tab)
        {
            self.exploration = None;
            self.document_publication = self.document_publication.saturating_add(1);
        }
    }

    /// The explicit binding of a context tool tab.
    #[must_use]
    pub fn binding_of(&self, tab: &TabId) -> Option<&ContextBinding> {
        match self.tab(tab)?.kind() {
            ViewKind::Preview(binding) | ViewKind::Backlinks(binding) => Some(binding),
            _ => None,
        }
    }

    /// One rebind signal per surviving context tool, after any change that
    /// can alter their effective sources.
    fn context_rebound_effects(&self) -> Vec<Effect> {
        self.tabs
            .iter()
            .filter(|(_, session)| {
                matches!(
                    session.kind(),
                    ViewKind::Preview(_) | ViewKind::Backlinks(_)
                )
            })
            .map(|(tab, _)| Effect::ContextRebound { tab: *tab })
            .collect()
    }

    /// Current explicitly acknowledged semantic insertion target.
    #[must_use]
    pub const fn writing_target(&self) -> Option<crate::input::WritingTarget> {
        self.writing_target
    }

    /// Invalidate insertion authorization after any edit to the bound buffer.
    pub fn invalidate_writing_target(&mut self, buffer_id: u64) {
        if self
            .writing_target
            .is_some_and(|target| target.buffer_id == buffer_id)
        {
            self.writing_target = None;
        }
    }

    /// Replace a view incarnation before its keyed runtime is released.
    /// Existing interaction coordinates are intentionally invalidated.
    ///
    /// # Errors
    /// Returns an error when the tab is missing or the resulting incarnation
    /// cannot be installed.
    pub fn replace_view_incarnation(&mut self, tab: TabId) -> Result<u64, &'static str> {
        let incarnation = self
            .tabs
            .get(&tab)
            .ok_or("view tab is closed")?
            .interaction()
            .incarnation
            .saturating_add(1);
        self.install_view_incarnation(tab, incarnation)?;
        Ok(incarnation)
    }

    /// Install the host-assigned resource generation for a newly attached
    /// view. Logical coordinates are reset because they belong to the prior
    /// runtime incarnation.
    ///
    /// # Errors
    /// Returns an error when the incarnation is zero or the tab is missing.
    pub fn install_view_incarnation(
        &mut self,
        tab: TabId,
        incarnation: u64,
    ) -> Result<(), &'static str> {
        if incarnation == 0 {
            return Err("view incarnation must be nonzero");
        }
        let view = self.tabs.get_mut(&tab).ok_or("view tab is closed")?;
        view.set_interaction(crate::input::EditorInteraction::new(tab, incarnation));
        if self.writing_target.is_some_and(|target| target.tab == tab) {
            self.writing_target = None;
        }
        Ok(())
    }

    /// The focused tab (invariant: an active tab of some group).
    #[must_use]
    pub const fn focused_tab(&self) -> TabId {
        self.focus
    }

    /// The manually maximized group, if any (D-13).
    #[must_use]
    pub const fn maximized_group(&self) -> Option<GroupId> {
        self.maximized
    }

    /// The group that owns the focused tab.
    #[must_use]
    pub fn focused_group(&self) -> Option<GroupId> {
        self.groups
            .iter()
            .find(|(_, group)| group.active() == self.focus)
            .map(|(id, _)| *id)
    }

    /// Number of closed views retained for reopen.
    #[must_use]
    pub fn recently_closed_len(&self) -> usize {
        self.recently_closed.len()
    }

    /// First group in deterministic top-left traversal order.
    #[must_use]
    pub fn first_group(&self) -> Option<GroupId> {
        self.tree.groups().into_iter().next()
    }

    /// Groups in stable traversal order (F6 ordering, §7.4).
    #[must_use]
    pub fn group_order(&self) -> Vec<GroupId> {
        self.tree.groups()
    }

    /// The group that displays `tab`, if any.
    #[must_use]
    pub fn group_of(&self, tab: TabId) -> Option<GroupId> {
        self.groups
            .iter()
            .find(|(_, group)| group.tabs().contains(&tab))
            .map(|(id, _)| *id)
    }

    fn group_mut(&mut self, id: &GroupId) -> Result<&mut TabGroup, WorkspaceError> {
        self.groups
            .get_mut(id)
            .ok_or(WorkspaceError::UnknownGroup(*id))
    }

    fn tab_kind(&self, tab: &TabId) -> Result<&ViewKind, WorkspaceError> {
        self.tabs
            .get(tab)
            .map(ViewSession::kind)
            .ok_or(WorkspaceError::UnknownTab(*tab))
    }

    fn ensure_tab_capacity(&self) -> Result<(), WorkspaceError> {
        if self.tabs.len() >= MAX_TABS {
            Err(WorkspaceError::TabLimitReached)
        } else {
            Ok(())
        }
    }

    fn ensure_group_capacity(&self) -> Result<(), WorkspaceError> {
        if self.groups.len() >= MAX_GROUPS {
            Err(WorkspaceError::GroupLimitReached)
        } else {
            Ok(())
        }
    }

    fn ensure_split_depth(&self) -> Result<(), WorkspaceError> {
        if self.tree.depth() + 1 > MAX_DEPTH {
            Err(WorkspaceError::DepthLimitReached)
        } else {
            Ok(())
        }
    }

    fn owner_of(&self, tab: TabId) -> Result<GroupId, WorkspaceError> {
        self.group_of(tab).ok_or(WorkspaceError::UnknownTab(tab))
    }

    /// Insert a fresh tab at the end of `group`, activating and focusing it.
    fn append_tab(
        &mut self,
        group: GroupId,
        session: ViewSession,
    ) -> Result<TabId, WorkspaceError> {
        self.ensure_tab_capacity()?;
        if !self.groups.contains_key(&group) {
            return Err(WorkspaceError::UnknownGroup(group));
        }
        let tab = session.tab();
        self.tabs.insert(tab, session);
        self.group_mut(&group)?.insert(usize::MAX, tab);
        self.focus = tab;
        self.maximized = None;
        Ok(tab)
    }

    /// Remove a tab from its group and collapse any structure that became
    /// empty. Returns the group it lived in.
    fn detach_tab(&mut self, tab: TabId) -> Result<GroupId, WorkspaceError> {
        let group = self.owner_of(tab)?;
        let became_empty = {
            let group_state = self.group_mut(&group)?;
            group_state.remove(tab).is_none()
        };
        self.tabs.remove(&tab);
        if became_empty {
            self.remove_group_collapse(group)?;
        }
        Ok(group)
    }

    /// Closing destroys a source identity; moving only detaches membership.
    fn forget_source(&mut self, tab: TabId) {
        if self.document_anchor == Some(tab) {
            self.document_anchor = None;
            // Retirement of the anchor is observable: sinks re-resolve to a
            // typed unbound state instead of silently keeping stale content.
            self.document_publication = self.document_publication.saturating_add(1);
        }
        if self
            .exploration
            .as_ref()
            .is_some_and(|selection| selection.source_tab == tab)
        {
            self.exploration = None;
            self.document_publication = self.document_publication.saturating_add(1);
        }
        if self.writing_target.is_some_and(|target| target.tab == tab) {
            self.writing_target = None;
        }
    }

    /// Remove a group, collapsing its parent split. Returns false when no
    /// groups remain (the caller normalizes a fresh workspace).
    fn remove_group_collapse(&mut self, group: GroupId) -> Result<bool, WorkspaceError> {
        if self.groups.remove(&group).is_none() {
            return Err(WorkspaceError::UnknownGroup(group));
        }
        if self.groups.is_empty() {
            return Ok(false);
        }
        match self.tree.detach_group(group) {
            Some(()) => Ok(true),
            None => Ok(false),
        }
    }

    fn remember_closed(&mut self, locator: NoteLocator) {
        if self.recently_closed.len() >= MAX_REOPEN {
            self.recently_closed.pop_front();
        }
        self.recently_closed.push_back(locator);
    }

    fn set_note_load(&mut self, tab: TabId, load: NoteLoad) -> Result<Vec<Effect>, WorkspaceError> {
        let Some(session) = self.tabs.get_mut(&tab) else {
            return Err(WorkspaceError::UnknownTab(tab));
        };
        let ViewKind::Note(note) = session.kind_mut() else {
            return Err(WorkspaceError::UnknownTab(tab));
        };
        if note.load() == load {
            return Err(WorkspaceError::NoChange);
        }
        note.set_load(load);
        Ok(vec![Effect::NoteLoadChanged { tab, load }])
    }

    fn move_tab_index(
        &mut self,
        tab: TabId,
        group: GroupId,
        index: usize,
    ) -> Result<(), WorkspaceError> {
        if !self.groups.contains_key(&group) {
            return Err(WorkspaceError::UnknownGroup(group));
        }
        let source = self.owner_of(tab)?;
        if source == group {
            let position = self
                .group(&group)
                .and_then(|group| group.index_of(tab))
                .unwrap_or(usize::MAX);
            let effective = index.min(
                self.group(&group)
                    .map_or(0, |group| group.tabs().len())
                    .saturating_sub(1),
            );
            if position == effective {
                return Err(WorkspaceError::NoChange);
            }
        }
        let session = self
            .tabs
            .get(&tab)
            .cloned()
            .ok_or(WorkspaceError::UnknownTab(tab))?;
        self.detach_tab(tab)?;
        if !self.groups.contains_key(&group) {
            return Err(WorkspaceError::UnknownGroup(group));
        }
        self.tabs.insert(tab, session);
        self.group_mut(&group)?.insert(index, tab);
        self.focus = tab;
        self.maximized = None;
        Ok(())
    }

    fn move_tab_split(
        &mut self,
        tab: TabId,
        anchor: GroupId,
        arrangement: Arrangement,
        side: Side,
    ) -> Result<(), WorkspaceError> {
        if !self.groups.contains_key(&anchor) {
            return Err(WorkspaceError::UnknownGroup(anchor));
        }
        self.ensure_split_depth()?;
        self.ensure_group_capacity()?;
        let session = self
            .tabs
            .get(&tab)
            .cloned()
            .ok_or(WorkspaceError::UnknownTab(tab))?;
        self.detach_tab(tab)?;
        // The anchor may have collapsed if it only held this tab; the new
        // group then forms beside whatever remains at the tree root.
        let split_id = self.ids.split_id();
        let new_group = self.ids.group_id();
        self.groups.insert(new_group, TabGroup::new(tab));
        self.tabs.insert(tab, session);
        let anchor_node = if self.groups.contains_key(&anchor) {
            anchor
        } else {
            self.first_group().ok_or(WorkspaceError::NoChange)?
        };
        if !self
            .tree
            .insert_group_leaf(anchor_node, split_id, arrangement, side, new_group)
        {
            return Err(WorkspaceError::UnknownGroup(anchor_node));
        }
        self.focus = tab;
        self.maximized = None;
        Ok(())
    }

    /// After any structural change: drop group entries that vanished from
    /// the tree, guarantee at least one real group with one tab, keep the
    /// tab map exactly the union of group members, and repair focus and
    /// maximize references (§7.3 invariants 2/3/4).
    fn normalize(&mut self) {
        let present: BTreeSet<GroupId> = self.tree.groups().into_iter().collect();
        let stale: Vec<GroupId> = self
            .groups
            .keys()
            .copied()
            .filter(|id| !present.contains(id))
            .collect();
        for id in &stale {
            self.groups.remove(id);
        }
        if self
            .maximized
            .is_some_and(|group| !self.groups.contains_key(&group))
        {
            self.maximized = None;
        }
        if self.groups.is_empty() {
            let group = self.ids.group_id();
            let tab = self.ids.tab_id();
            self.tree = LayoutNode::Group(group);
            self.groups.insert(group, TabGroup::new(tab));
            self.tabs
                .insert(tab, ViewSession::new(tab, ViewKind::Empty));
            self.focus = tab;
            self.maximized = None;
            return;
        }
        // Drop tabs that no group displays, then prune group entries that
        // lost their tab.
        let displayed: BTreeSet<TabId> = self
            .groups
            .values()
            .flat_map(|group| group.tabs().iter().copied())
            .collect();
        self.tabs.retain(|tab, _| displayed.contains(tab));
        for id in &present {
            if self
                .groups
                .get(id)
                .is_some_and(|group| group.tabs().is_empty())
            {
                self.groups.remove(id);
            }
        }
        // A group pruned above may have been a tree leaf; collapse those.
        loop {
            let tree_groups: BTreeSet<GroupId> = self.tree.groups().into_iter().collect();
            let orphan = tree_groups
                .iter()
                .copied()
                .find(|id| !self.groups.contains_key(id));
            match orphan {
                Some(orphan) => {
                    if self.tree.detach_group(orphan).is_none() {
                        break; // root itself; handled by the empty check below
                    }
                }
                None => break,
            }
            if self.groups.is_empty() {
                break;
            }
        }
        if self.groups.is_empty() {
            let group = self.ids.group_id();
            let tab = self.ids.tab_id();
            self.tree = LayoutNode::Group(group);
            self.groups.insert(group, TabGroup::new(tab));
            self.tabs
                .insert(tab, ViewSession::new(tab, ViewKind::Empty));
            self.focus = tab;
            self.maximized = None;
            return;
        }
        let live: BTreeSet<TabId> = self.tabs.keys().copied().collect();
        for group in self.groups.values_mut() {
            group.retain(|tab| live.contains(tab));
            group.ensure_active();
        }
        if !self
            .groups
            .values()
            .any(|group| group.active() == self.focus)
            && let Some(first) = self.first_group()
            && let Some(group) = self.groups.get(&first)
        {
            self.focus = group.active();
        }
    }
}

impl Default for WorkspaceState {
    fn default() -> Self {
        Self::new()
    }
}

/// Apply one transaction to a copy of `state`, or fail without changing it.
/// One arm per action keeps each rule readable in isolation.
#[allow(clippy::too_many_lines)]
fn apply(state: &mut WorkspaceState, action: &LayoutAction) -> Result<Vec<Effect>, WorkspaceError> {
    match action {
        LayoutAction::OpenNote { locator, group, .. } => {
            let tab = state.ids.tab_id();
            let session = ViewSession::new(
                tab,
                ViewKind::Note(NoteView::new(locator.clone(), NoteLoad::Reserved)),
            );
            let tab = state.append_tab(*group, session)?;
            Ok(vec![Effect::ViewAttached {
                tab,
                kind: state.tabs[&tab].kind().clone(),
            }])
        }
        LayoutAction::OpenEmpty { group, .. } => {
            let tab = state.ids.tab_id();
            let tab = state.append_tab(*group, ViewSession::new(tab, ViewKind::Empty))?;
            Ok(vec![Effect::ViewAttached {
                tab,
                kind: ViewKind::Empty,
            }])
        }
        LayoutAction::OpenNotes { group, .. } => {
            let tab = state.ids.tab_id();
            let tab = state.append_tab(*group, ViewSession::new(tab, ViewKind::Notes))?;
            Ok(vec![Effect::ViewAttached {
                tab,
                kind: ViewKind::Notes,
            }])
        }
        LayoutAction::OpenContext {
            view,
            binding,
            group,
            ..
        } => {
            let tab = state.ids.tab_id();
            let kind = view.view_kind(binding.clone());
            let tab = state.append_tab(*group, ViewSession::new(tab, kind.clone()))?;
            Ok(vec![
                Effect::ViewAttached {
                    tab,
                    kind: state.tabs[&tab].kind().clone(),
                },
                Effect::ContextRebound { tab },
            ])
        }
        LayoutAction::SetContextBinding { tab, binding, .. } => {
            let session = state
                .tabs
                .get_mut(tab)
                .ok_or(WorkspaceError::UnknownTab(*tab))?;
            match session.kind_mut() {
                ViewKind::Preview(binding_slot) | ViewKind::Backlinks(binding_slot) => {
                    *binding_slot = binding.clone();
                }
                _ => return Err(WorkspaceError::UnknownTab(*tab)),
            }
            Ok(vec![Effect::ContextRebound { tab: *tab }])
        }
        LayoutAction::ActivateTab { tab, .. } => {
            let owner = state.owner_of(*tab)?;
            state.group_mut(&owner)?.set_active(*tab);
            state.focus = *tab;
            state.maximized = None;
            Ok(Vec::new())
        }
        LayoutAction::FocusGroup { group, .. } => {
            let active = state.group_mut(group)?.active();
            state.focus = active;
            state.maximized = None;
            Ok(Vec::new())
        }
        LayoutAction::CloseTab { tab, .. } => {
            let kind = state.tab_kind(tab)?.clone();
            if let ViewKind::Note(note) = &kind {
                state.remember_closed(note.locator().clone());
            }
            state.detach_tab(*tab)?;
            state.forget_source(*tab);
            let mut effects = vec![Effect::ViewDetached { tab: *tab, kind }];
            effects.extend(state.context_rebound_effects());
            Ok(effects)
        }
        LayoutAction::CloseGroup { group, .. } => {
            let members = state
                .group(group)
                .ok_or(WorkspaceError::UnknownGroup(*group))?
                .tabs()
                .to_vec();
            let mut effects = Vec::new();
            for tab in members {
                if let Some(session) = state.tabs.get(&tab).cloned() {
                    if let ViewKind::Note(note) = session.kind() {
                        state.remember_closed(note.locator().clone());
                    }
                    effects.push(Effect::ViewDetached {
                        tab,
                        kind: session.kind().clone(),
                    });
                }
                state.detach_tab(tab)?;
                state.forget_source(tab);
            }
            effects.extend(state.context_rebound_effects());
            Ok(effects)
        }
        LayoutAction::DuplicateTab { tab, .. } => {
            let owner = state.owner_of(*tab)?;
            let kind = state
                .tab_kind(tab)?
                .duplicate()
                .ok_or(WorkspaceError::NotDuplicateable(*tab))?;
            let position = state
                .group(&owner)
                .and_then(|group| group.index_of(*tab))
                .unwrap_or(0);
            let new_tab = state.ids.tab_id();
            let session = ViewSession::new(new_tab, kind.clone());
            state.ensure_tab_capacity()?;
            state.tabs.insert(new_tab, session);
            state.group_mut(&owner)?.insert(position + 1, new_tab);
            state.focus = new_tab;
            state.maximized = None;
            Ok(vec![Effect::ViewAttached { tab: new_tab, kind }])
        }
        LayoutAction::ReopenClosed { .. } => {
            let locator = state
                .recently_closed
                .pop_back()
                .ok_or(WorkspaceError::NothingToReopen)?;
            let group = state.focused_group().ok_or(WorkspaceError::NoChange)?;
            let tab = state.ids.tab_id();
            let session = ViewSession::new(
                tab,
                ViewKind::Note(NoteView::new(locator, NoteLoad::Reserved)),
            );
            let tab = state.append_tab(group, session)?;
            Ok(vec![Effect::ViewAttached {
                tab,
                kind: state.tabs[&tab].kind().clone(),
            }])
        }
        LayoutAction::MoveTab { tab, target, .. } => {
            if !state.tabs.contains_key(tab) {
                return Err(WorkspaceError::UnknownTab(*tab));
            }
            match *target {
                TabDropTarget::Index { group, index } => {
                    state.move_tab_index(*tab, group, index)?;
                }
                TabDropTarget::SplitFrom {
                    group,
                    arrangement,
                    side,
                } => state.move_tab_split(*tab, group, arrangement, side)?,
            }
            Ok(Vec::new())
        }
        LayoutAction::MergeGroups {
            source,
            destination,
            ..
        } => {
            if source == destination {
                return Err(WorkspaceError::NoChange);
            }
            if !state.groups.contains_key(source) {
                return Err(WorkspaceError::UnknownGroup(*source));
            }
            if !state.groups.contains_key(destination) {
                return Err(WorkspaceError::UnknownGroup(*destination));
            }
            let keep_active = state
                .group(destination)
                .ok_or(WorkspaceError::UnknownGroup(*destination))?
                .active();
            let moving = {
                let source_group = state
                    .group_mut(source)
                    .map_err(|_| WorkspaceError::UnknownGroup(*source))?;
                source_group.take_all()
            };
            state
                .group_mut(destination)?
                .push_all(moving.iter().copied());
            state.group_mut(destination)?.set_active(keep_active);
            state.remove_group_collapse(*source)?;
            Ok(Vec::new())
        }
        LayoutAction::MoveGroupSplit {
            source,
            destination,
            arrangement,
            side,
            ..
        } => {
            if source == destination {
                return Err(WorkspaceError::NoChange);
            }
            if !state.groups.contains_key(source) {
                return Err(WorkspaceError::UnknownGroup(*source));
            }
            if !state.groups.contains_key(destination) {
                return Err(WorkspaceError::UnknownGroup(*destination));
            }
            state.ensure_split_depth()?;
            state.ensure_group_capacity()?;
            let mut tree = state.tree.clone();
            if tree.detach_group(*source).is_none() {
                return Err(WorkspaceError::NoChange);
            }
            if !tree.contains_group(*destination) {
                return Err(WorkspaceError::UnknownGroup(*destination));
            }
            let split_id = state.ids.split_id();
            if !tree.insert_group_leaf(*destination, split_id, *arrangement, *side, *source) {
                return Err(WorkspaceError::UnknownGroup(*destination));
            }
            state.tree = tree;
            Ok(Vec::new())
        }
        LayoutAction::MoveGroupEdge { group, edge, .. } => {
            if !state.groups.contains_key(group) {
                return Err(WorkspaceError::UnknownGroup(*group));
            }
            if state.group_order().len() < 2 {
                // Moving the only group to an edge cannot change the tree.
                return Err(WorkspaceError::NoChange);
            }
            state.ensure_split_depth()?;
            let mut tree = state.tree.clone();
            if tree.detach_group(*group).is_none() {
                // The group is the whole root; with two or more groups this
                // cannot happen, so treat it as a no-op request.
                return Err(WorkspaceError::NoChange);
            }
            let split_id = state.ids.split_id();
            let group_leaf = LayoutNode::Group(*group);
            let (first, second) = match edge.side() {
                Side::First => (group_leaf, tree),
                Side::Second => (tree, group_leaf),
            };
            state.tree = LayoutNode::Split {
                id: split_id,
                arrangement: edge.arrangement(),
                desired_ratio_ppm: DEFAULT_RATIO_PPM,
                first: Box::new(first),
                second: Box::new(second),
            };
            Ok(Vec::new())
        }
        LayoutAction::SplitGroup {
            group, arrangement, ..
        } => {
            let active = state
                .group(group)
                .ok_or(WorkspaceError::UnknownGroup(*group))?
                .active();
            let kind = state
                .tab_kind(&active)
                .map_err(|_| WorkspaceError::UnknownTab(active))?
                .duplicate()
                .unwrap_or(ViewKind::Empty);
            state.ensure_split_depth()?;
            state.ensure_group_capacity()?;
            state.ensure_tab_capacity()?;
            let new_group = state.ids.group_id();
            let split_id = state.ids.split_id();
            let tab = state.ids.tab_id();
            let session = ViewSession::new(tab, kind.clone());
            state.tabs.insert(tab, session);
            state.groups.insert(new_group, TabGroup::new(tab));
            if !state.tree.insert_group_leaf(
                *group,
                split_id,
                *arrangement,
                Side::Second,
                new_group,
            ) {
                return Err(WorkspaceError::UnknownGroup(*group));
            }
            state.focus = tab;
            state.maximized = None;
            Ok(vec![Effect::ViewAttached { tab, kind }])
        }
        LayoutAction::ResizeSplit {
            split,
            desired_ratio_ppm,
            ..
        } => {
            if !(MIN_RATIO_PPM..=MAX_RATIO_PPM).contains(desired_ratio_ppm) {
                return Err(WorkspaceError::InvalidRatio(*desired_ratio_ppm));
            }
            if !state.tree.splits().contains(split) {
                return Err(WorkspaceError::UnknownSplit(*split));
            }
            state.tree = set_split_ratio(&state.tree, *split, *desired_ratio_ppm);
            Ok(Vec::new())
        }
        LayoutAction::ToggleMaximize { group, .. } => {
            if !state.groups.contains_key(group) {
                return Err(WorkspaceError::UnknownGroup(*group));
            }
            state.maximized = if state.maximized == Some(*group) {
                None
            } else {
                Some(*group)
            };
            Ok(Vec::new())
        }
        LayoutAction::NoteLoadReady { tab, .. } => state.set_note_load(*tab, NoteLoad::Ready),
        LayoutAction::NoteLoadFailed { tab, .. } => state.set_note_load(*tab, NoteLoad::Failed),
        LayoutAction::CycleTab { forward, .. } => {
            let group_id = state.focused_group().ok_or(WorkspaceError::NoChange)?;
            let next = {
                let group = state
                    .group(&group_id)
                    .ok_or(WorkspaceError::UnknownGroup(group_id))?;
                if group.tabs().len() < 2 {
                    return Err(WorkspaceError::NoChange);
                }
                group.next_active(group.active(), *forward)
            };
            let Some(next) = next else {
                return Err(WorkspaceError::NoChange);
            };
            state.group_mut(&group_id)?.set_active(next);
            state.focus = next;
            state.maximized = None;
            Ok(Vec::new())
        }
        LayoutAction::CycleGroup { forward, .. } => {
            let order = state.group_order();
            if order.len() < 2 {
                return Err(WorkspaceError::NoChange);
            }
            let current = state.focused_group().ok_or(WorkspaceError::NoChange)?;
            let position = order
                .iter()
                .position(|candidate| *candidate == current)
                .ok_or(WorkspaceError::NoChange)?;
            let next = if *forward {
                (position + 1) % order.len()
            } else {
                (position + order.len() - 1) % order.len()
            };
            let active = state
                .group(&order[next])
                .ok_or(WorkspaceError::UnknownGroup(order[next]))?
                .active();
            state.focus = active;
            state.maximized = None;
            Ok(Vec::new())
        }
    }
}

/// Replace the stored desired ratio of `split`, rebuilding the spine.
#[allow(clippy::needless_pass_by_value)]
fn set_split_ratio(node: &LayoutNode, split: SplitId, ratio: u32) -> LayoutNode {
    match node {
        LayoutNode::Group(_) => node.clone(),
        LayoutNode::Split {
            id,
            arrangement,
            desired_ratio_ppm,
            first,
            second,
        } => LayoutNode::Split {
            id: *id,
            arrangement: *arrangement,
            desired_ratio_ppm: if *id == split {
                ratio
            } else {
                *desired_ratio_ppm
            },
            first: Box::new(set_split_ratio(first, split, ratio)),
            second: Box::new(set_split_ratio(second, split, ratio)),
        },
    }
}

/// Reduce one layout transaction against `state`.
///
/// Returns a fresh committed state or a typed rejection; the input is never
/// mutated. Effects are derived by diffing view sets so the host can attach
/// and release resources without inspecting internals.
///
/// # Panics
/// Only if the u64 revision counter overflows, which cannot happen in a
/// process lifetime.
#[must_use]
#[allow(clippy::needless_pass_by_value)]
pub fn reduce(state: &WorkspaceState, action: LayoutAction) -> Outcome {
    // 1. Stale revisions are rejected before any work.
    if action.expected_revision() != state.revision {
        return Outcome::Rejected(WorkspaceError::StaleRevision {
            expected: action.expected_revision(),
            actual: state.revision,
        });
    }
    // 2-4. Apply to a copy, normalize, validate.
    let mut candidate = state.clone();
    match apply(&mut candidate, &action) {
        Ok(mut effects) => {
            candidate.normalize();
            candidate.revision = state.revision.checked_add(1).expect("revision overflow");
            if let Err(error) = validate::validate(&candidate) {
                return Outcome::Rejected(WorkspaceError::InvariantViolation(error));
            }
            // 5. Commit with derived lifecycle effects, de-duplicated
            // against the ones the apply step already emitted.
            for (tab, session) in &candidate.tabs {
                if !state.tabs.contains_key(tab) {
                    effects.push(Effect::ViewAttached {
                        tab: *tab,
                        kind: session.kind().clone(),
                    });
                }
            }
            for (tab, session) in &state.tabs {
                if !candidate.tabs.contains_key(tab) {
                    effects.push(Effect::ViewDetached {
                        tab: *tab,
                        kind: session.kind().clone(),
                    });
                }
            }
            let mut seen = BTreeSet::new();
            effects.retain(|effect| match effect {
                Effect::ViewAttached { tab, .. } | Effect::ViewDetached { tab, .. } => {
                    seen.insert(*tab)
                }
                // Load changes are unique per tab and state in one commit;
                // the apply step rejects no-op transitions. Rebind signals
                // are unique per context tab by construction.
                Effect::NoteLoadChanged { .. } => true,
                Effect::ContextRebound { tab } => seen.insert(*tab),
            });
            Outcome::Committed(Commit {
                state: candidate,
                effects,
            })
        }
        Err(error) => Outcome::Rejected(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::TabDropTarget;

    fn seeded() -> WorkspaceState {
        let mut ids = Ids::with_seed(42);
        WorkspaceState::with_ids(&mut ids)
    }

    fn commit(state: &WorkspaceState, action: LayoutAction) -> (WorkspaceState, Vec<Effect>) {
        match reduce(state, action) {
            Outcome::Committed(commit) => (commit.state, commit.effects),
            Outcome::Rejected(error) => panic!("expected commit, got {error}"),
        }
    }

    fn open_note(state: &WorkspaceState, path: &str) -> (WorkspaceState, TabId) {
        let group = state.first_group().expect("group");
        let locator = NoteLocator::from_components([path]).expect("locator");
        let (next, effects) = commit(
            state,
            LayoutAction::OpenNote {
                expected_revision: state.revision(),
                locator,
                group,
            },
        );
        let Some(Effect::ViewAttached { tab, .. }) = effects.first().cloned() else {
            panic!("open emits ViewAttached");
        };
        (next, tab)
    }

    #[test]
    fn stale_revisions_are_byte_identical_noops() {
        let state = seeded();
        let group = state.first_group().unwrap();
        assert_eq!(
            reduce(
                &state,
                LayoutAction::OpenEmpty {
                    expected_revision: state.revision() + 7,
                    group,
                }
            ),
            Outcome::Rejected(WorkspaceError::StaleRevision {
                expected: state.revision() + 7,
                actual: state.revision(),
            })
        );
        let (next, _) = commit(
            &state,
            LayoutAction::OpenEmpty {
                expected_revision: state.revision(),
                group,
            },
        );
        assert_eq!(next.revision(), state.revision() + 1);
        assert_eq!(next.tabs().count(), state.tabs().count() + 1);
    }

    #[test]
    fn unknown_targets_are_rejected_without_change() {
        let state = seeded();
        let ghost_group = GroupId::from_raw(u128::MAX);
        let ghost_tab = TabId::from_raw(u128::MAX);
        let snapshot = state.clone();
        let rejections = [
            reduce(
                &state,
                LayoutAction::OpenEmpty {
                    expected_revision: state.revision(),
                    group: ghost_group,
                },
            ),
            reduce(
                &state,
                LayoutAction::ActivateTab {
                    expected_revision: state.revision(),
                    tab: ghost_tab,
                },
            ),
            reduce(
                &state,
                LayoutAction::CloseTab {
                    expected_revision: state.revision(),
                    tab: ghost_tab,
                },
            ),
            reduce(
                &state,
                LayoutAction::MergeGroups {
                    expected_revision: state.revision(),
                    source: state.first_group().unwrap(),
                    destination: ghost_group,
                },
            ),
        ];
        for outcome in rejections {
            assert!(matches!(outcome, Outcome::Rejected(_)), "{outcome:?}");
        }
        assert_eq!(state, snapshot, "rejected transactions never touch state");
    }

    #[test]
    fn self_moves_and_noop_operations_are_rejected() {
        let initial = seeded();
        let (state, tab) = open_note(&initial, "a.md");
        let group = state.first_group().unwrap();
        let position = state
            .group(&group)
            .and_then(|group| group.index_of(tab))
            .unwrap();
        assert_eq!(
            reduce(
                &state,
                LayoutAction::MoveTab {
                    expected_revision: state.revision(),
                    tab,
                    target: TabDropTarget::Index {
                        group,
                        index: position,
                    },
                },
            ),
            Outcome::Rejected(WorkspaceError::NoChange)
        );
        assert_eq!(
            reduce(
                &state,
                LayoutAction::MergeGroups {
                    expected_revision: state.revision(),
                    source: group,
                    destination: group,
                },
            ),
            Outcome::Rejected(WorkspaceError::NoChange)
        );
    }

    #[test]
    fn closing_the_last_tab_creates_a_real_empty_tab() {
        let state = seeded();
        let group = state.first_group().unwrap();
        let only = state.group(&group).unwrap().active();
        let (next, effects) = commit(
            &state,
            LayoutAction::CloseTab {
                expected_revision: state.revision(),
                tab: only,
            },
        );
        assert_eq!(next.groups().count(), 1);
        let new_group = next.first_group().unwrap();
        assert_ne!(new_group, group, "a fresh group replaces the emptied one");
        assert_eq!(next.tabs().count(), 1);
        let session = next.group(&new_group).unwrap().active();
        assert_eq!(next.tab(&session).unwrap().kind(), &ViewKind::Empty);
        assert!(effects.contains(&Effect::ViewDetached {
            tab: only,
            kind: ViewKind::Empty,
        }));
    }

    #[test]
    fn moving_a_tab_between_nested_groups_preserves_identity() {
        let initial = seeded();
        let (s1, a) = open_note(&initial, "a.md");
        let (s2, b) = open_note(&s1, "b.md");
        let group = s2.first_group().unwrap();
        let (s3, effects) = commit(
            &s2,
            LayoutAction::SplitGroup {
                expected_revision: s2.revision(),
                group,
                arrangement: Arrangement::SideBySide,
            },
        );
        let Some(Effect::ViewAttached { tab: dup, .. }) = effects.first().cloned() else {
            panic!("split attaches a duplicate");
        };
        assert_eq!(s3.groups().count(), 2);
        assert_eq!(s3.tree().depth(), 2);
        let new_group = s3
            .group_order()
            .into_iter()
            .find(|id| {
                s3.group(id)
                    .is_some_and(|group| group.tabs().contains(&dup))
            })
            .unwrap();
        let (s4, _) = commit(
            &s3,
            LayoutAction::MoveTab {
                expected_revision: s3.revision(),
                tab: a,
                target: TabDropTarget::Index {
                    group: new_group,
                    index: 0,
                },
            },
        );
        assert_eq!(s4.group_of(a), Some(new_group));
        assert_eq!(s4.tab(&a).unwrap().tab(), a, "identity survives the move");
        assert_eq!(s4.tabs().count(), s3.tabs().count());
        assert_eq!(
            s4.tab(&b).map(ViewSession::kind).cloned(),
            s3.tab(&b).map(ViewSession::kind).cloned()
        );
        assert_eq!(s4.focused_tab(), a, "a moved tab is activated");
    }

    #[test]
    fn merge_keeps_destination_active_tab_and_collapses_splits() {
        let initial = seeded();
        let (s1, _) = open_note(&initial, "a.md");
        let group = s1.first_group().unwrap();
        let (s2, _) = commit(
            &s1,
            LayoutAction::SplitGroup {
                expected_revision: s1.revision(),
                group,
                arrangement: Arrangement::Stacked,
            },
        );
        assert_eq!(s2.tree().depth(), 2);
        let order = s2.group_order();
        let (destination, source) = (order[0], order[1]);
        let destination_active = s2.group(&destination).unwrap().active();
        let (s3, _) = commit(
            &s2,
            LayoutAction::MergeGroups {
                expected_revision: s2.revision(),
                source,
                destination,
            },
        );
        assert_eq!(s3.groups().count(), 1);
        assert_eq!(s3.tree().depth(), 1);
        assert_eq!(s3.group(&destination).unwrap().active(), destination_active);
        assert_eq!(
            s3.group(&destination).unwrap().tabs().len(),
            s2.tabs().count()
        );
    }

    #[test]
    fn reopen_refuses_when_empty_and_restores_locators_in_order() {
        let initial = seeded();
        assert_eq!(
            reduce(
                &initial,
                LayoutAction::ReopenClosed {
                    expected_revision: initial.revision(),
                },
            ),
            Outcome::Rejected(WorkspaceError::NothingToReopen)
        );
        let (s1, a) = open_note(&initial, "a.md");
        let (s2, b) = open_note(&s1, "b.md");
        let (s3, _) = commit(
            &s2,
            LayoutAction::CloseTab {
                expected_revision: s2.revision(),
                tab: b,
            },
        );
        let (s4, _) = commit(
            &s3,
            LayoutAction::CloseTab {
                expected_revision: s3.revision(),
                tab: a,
            },
        );
        assert_eq!(s4.recently_closed_len(), 2);
        let (s5, effects) = commit(
            &s4,
            LayoutAction::ReopenClosed {
                expected_revision: s4.revision(),
            },
        );
        let Some(Effect::ViewAttached { tab, kind }) = effects.first().cloned() else {
            panic!("reopen attaches");
        };
        assert_eq!(
            kind,
            ViewKind::Note(NoteView::new(
                NoteLocator::from_components(["a.md"]).unwrap(),
                NoteLoad::Reserved,
            )),
            "most recently closed reopens first"
        );
        assert_eq!(s5.tab(&tab).unwrap().title(), "a");
    }

    #[test]
    fn duplicate_views_are_independent_with_fresh_identity() {
        let initial = seeded();
        let (s1, a) = open_note(&initial, "a.md");
        let (s2, effects) = commit(
            &s1,
            LayoutAction::DuplicateTab {
                expected_revision: s1.revision(),
                tab: a,
            },
        );
        let Some(Effect::ViewAttached { tab: dup, kind }) = effects.first().cloned() else {
            panic!("duplicate attaches");
        };
        assert_ne!(dup, a);
        assert!(matches!(kind, ViewKind::Note(_)));
        let session = s2.tab(&dup).unwrap();
        assert!(
            matches!(session.kind(), ViewKind::Note(note) if note.load() == NoteLoad::Reserved),
            "duplicates reopen independently from Reserved"
        );
        assert_eq!(s2.group_of(dup), s2.group_of(a));
        assert_eq!(s2.tabs().count(), s1.tabs().count() + 1);
        let empty_tab = s1
            .group(&s1.first_group().unwrap())
            .unwrap()
            .tabs()
            .iter()
            .copied()
            .find(|tab| {
                s1.tab(tab)
                    .is_some_and(|session| session.kind() == &ViewKind::Empty)
            })
            .expect("the initial Empty tab");
        assert_eq!(
            reduce(
                &s1,
                LayoutAction::DuplicateTab {
                    expected_revision: s1.revision(),
                    tab: empty_tab,
                },
            ),
            Outcome::Rejected(WorkspaceError::NotDuplicateable(empty_tab)),
            "Empty tabs cannot be duplicated"
        );
    }

    #[test]
    fn note_load_transitions_track_reserved_to_ready_and_failed() {
        let initial = seeded();
        let (s1, a) = open_note(&initial, "a.md");
        let (s2, effects) = commit(
            &s1,
            LayoutAction::NoteLoadReady {
                expected_revision: s1.revision(),
                tab: a,
            },
        );
        assert_eq!(
            effects,
            vec![Effect::NoteLoadChanged {
                tab: a,
                load: NoteLoad::Ready,
            }]
        );
        assert!(matches!(
            s2.tab(&a).unwrap().kind(),
            ViewKind::Note(note) if note.load() == NoteLoad::Ready
        ));
        assert_eq!(
            reduce(
                &s2,
                LayoutAction::NoteLoadReady {
                    expected_revision: s2.revision(),
                    tab: a,
                },
            ),
            Outcome::Rejected(WorkspaceError::NoChange),
        );
        let (s3, _) = commit(
            &s2,
            LayoutAction::NoteLoadFailed {
                expected_revision: s2.revision(),
                tab: a,
            },
        );
        assert!(matches!(
            s3.tab(&a).unwrap().kind(),
            ViewKind::Note(note) if note.load() == NoteLoad::Failed
        ));
    }

    #[test]
    fn resize_updates_only_the_named_split_and_validates_bounds() {
        let initial = seeded();
        let (s1, _) = open_note(&initial, "a.md");
        let group = s1.first_group().unwrap();
        let (s2, _) = commit(
            &s1,
            LayoutAction::SplitGroup {
                expected_revision: s1.revision(),
                group,
                arrangement: Arrangement::SideBySide,
            },
        );
        let split = s2.tree().splits()[0];
        let (s3, _) = commit(
            &s2,
            LayoutAction::ResizeSplit {
                expected_revision: s2.revision(),
                split,
                desired_ratio_ppm: 700_000,
            },
        );
        match s3.tree() {
            LayoutNode::Split {
                desired_ratio_ppm, ..
            } => assert_eq!(*desired_ratio_ppm, 700_000),
            LayoutNode::Group(_) => panic!("expected split, got a lone group"),
        }
        assert_eq!(
            reduce(
                &s3,
                LayoutAction::ResizeSplit {
                    expected_revision: s3.revision(),
                    split,
                    desired_ratio_ppm: 1_000,
                },
            ),
            Outcome::Rejected(WorkspaceError::InvalidRatio(1_000))
        );
        assert_eq!(
            reduce(
                &s3,
                LayoutAction::ResizeSplit {
                    expected_revision: s3.revision(),
                    split: crate::ids::SplitId::from_raw(u128::MAX),
                    desired_ratio_ppm: 700_000,
                },
            ),
            Outcome::Rejected(WorkspaceError::UnknownSplit(crate::ids::SplitId::from_raw(
                u128::MAX
            ))),
        );
    }

    #[test]
    fn manual_maximize_and_group_cycle_follow_traversal_order() {
        let initial = seeded();
        let (s1, _) = open_note(&initial, "a.md");
        let group = s1.first_group().unwrap();
        let (s2, _) = commit(
            &s1,
            LayoutAction::SplitGroup {
                expected_revision: s1.revision(),
                group,
                arrangement: Arrangement::SideBySide,
            },
        );
        assert_eq!(s2.maximized_group(), None);
        let order = s2.group_order();
        let (s3, _) = commit(
            &s2,
            LayoutAction::ToggleMaximize {
                expected_revision: s2.revision(),
                group: order[1],
            },
        );
        assert_eq!(s3.maximized_group(), Some(order[1]));
        let (s4, _) = commit(
            &s3,
            LayoutAction::ToggleMaximize {
                expected_revision: s3.revision(),
                group: order[1],
            },
        );
        assert_eq!(s4.maximized_group(), None);
        let (s5, _) = commit(
            &s4,
            LayoutAction::CycleGroup {
                expected_revision: s4.revision(),
                forward: false,
            },
        );
        assert_eq!(s5.focused_group(), Some(order[0]));
        let (s6, _) = commit(
            &s5,
            LayoutAction::CycleGroup {
                expected_revision: s5.revision(),
                forward: true,
            },
        );
        assert_eq!(s6.focused_group(), Some(order[1]));
    }

    #[test]
    fn group_and_tab_limits_reject_without_change() {
        let mut state = seeded();
        let group = state.first_group().unwrap();
        for index in 0..MAX_TABS - 1 {
            let locator = NoteLocator::from_components([format!("n{index}.md")]).unwrap();
            match reduce(
                &state,
                LayoutAction::OpenNote {
                    expected_revision: state.revision(),
                    locator,
                    group,
                },
            ) {
                Outcome::Committed(commit) => state = commit.state,
                Outcome::Rejected(error) => panic!("open {index} rejected: {error}"),
            }
        }
        assert_eq!(state.tabs().count(), MAX_TABS);
        let snapshot = state.clone();
        assert_eq!(
            reduce(
                &state,
                LayoutAction::OpenEmpty {
                    expected_revision: state.revision(),
                    group,
                },
            ),
            Outcome::Rejected(WorkspaceError::TabLimitReached)
        );
        assert_eq!(state, snapshot);
    }
}
