//! Typed open intents and the D-03 destination policy (RFC-0006 S04).
//!
//! Every navigation surface — graph activation, reading links, search rows,
//! backlinks, outline, pickers, palette, accessibility actions, and note
//! creation — routes through one [`OpenIntent`] handled by [`open`]. The
//! global first-map-match open is gone: the destination order below is the
//! only routing authority (D-03).
//!
//! Default [`OpenDestination::ReuseDocument`] order:
//!
//! 1. a *visible* matching note in the preferred document group (the last
//!    live group with acknowledged document focus — the workspace document
//!    anchor's group, independent of current graph focus),
//! 2. another visible matching note,
//! 3. a matching note in the most recently used live document group —
//!    never the source group when the navigation origin is a graph, so a
//!    hidden match cannot be activated *over* the source graph,
//! 4. a new tab in the most recently used live document group.
//!
//! When no live document group exists at all, the fallback chain applies:
//! under narrow adaptation the source group is the only fallback;
//! otherwise a neighboring document group is created by splitting the
//! source group when geometry permits; otherwise the note opens as a new
//! tab in the source group and the status line exposes the return path.
//! [`OpenDestination::NewTab`] bypasses view dedup (buffers still share)
//! and [`OpenDestination::ReplaceTab`] is the explicitly requested
//! same-tab quick switch, translated through the existing in-tab
//! navigation/close-safety contract.
//!
//! The destination memory ([`OpenMemory`]) is session-local device state:
//! it is never persisted, so a restored workspace recomputes it from
//! acknowledged document focus, and stale `GroupId`s cannot survive a
//! merge/close/restore or a vault switch (D-03).

use std::collections::BTreeMap;

use flavite_workspace::{GroupId, LayoutAction, NoteLocator, TabId, ViewKind};

use crate::app::App;

/// Where an open intent wants to land (D-03).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenDestination {
    /// The default policy: reuse an existing visible document view under
    /// the documented order, else a new tab in the fallback destination.
    ReuseDocument,
    /// Explicitly requested new tab. Bypasses view dedup but not buffer
    /// sharing; still uses the preferred/fallback destination chain.
    NewTab,
    /// Explicitly requested same-tab navigation (quick switch): the tab
    /// keeps its identity, position and mode; the replaced note enters the
    /// tab's Back history.
    ReplaceTab(TabId),
}

/// One typed navigation request (D-03: one intent controls all surfaces).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenIntent {
    /// The note to show.
    pub locator: NoteLocator,
    /// The requested destination policy.
    pub destination: OpenDestination,
    /// The navigation origin, when the request came from a view. Only used
    /// for the last-resort same-group fallback (and to keep a source graph
    /// visible); it never selects an accidental global destination.
    pub source: Option<TabId>,
}

impl OpenIntent {
    /// A default `ReuseDocument` intent from a navigation origin.
    #[must_use]
    pub fn reuse(locator: NoteLocator, source: Option<TabId>) -> Self {
        Self {
            locator,
            destination: OpenDestination::ReuseDocument,
            source,
        }
    }

    /// An explicit new-tab intent (bypasses view dedup).
    #[must_use]
    pub fn new_tab(locator: NoteLocator) -> Self {
        Self {
            locator,
            destination: OpenDestination::NewTab,
            source: None,
        }
    }
}

/// Session-local destination memory (D-03): focus recency for groups and
/// tabs, observed from committed layout transactions. Never persisted —
/// restore recomputes from acknowledged document focus, so no stale
/// `GroupId` survives a merge/close/restore or a vault switch.
#[derive(Debug, Default, Clone)]
pub struct OpenMemory {
    /// Monotonic observation counter.
    counter: u64,
    /// Last-focus epoch per live group (destination MRU order).
    group_epoch: BTreeMap<GroupId, u64>,
    /// Last-focus epoch per live tab (tie-break: highest last-focus
    /// sequence, then ascending `TabId`).
    tab_epoch: BTreeMap<TabId, u64>,
}

impl OpenMemory {
    /// Observe one committed workspace state: drop dead references
    /// (merge/close/restore) and stamp the focused tab plus its group.
    /// Acknowledged document focus is the workspace document anchor (D-01);
    /// plain focus still recency-orders tabs/groups for the tie-break and
    /// the MRU fallbacks.
    pub fn observe(&mut self, workspace: &flavite_workspace::WorkspaceState) {
        self.counter = self.counter.saturating_add(1);
        let now = self.counter;
        let live_groups: Vec<GroupId> = workspace.group_order();
        self.group_epoch
            .retain(|group, _| live_groups.contains(group));
        self.tab_epoch.retain(|tab, _| workspace.tab(tab).is_some());
        let focused = workspace.focused_tab();
        self.tab_epoch.insert(focused, now);
        if let Some(group) = workspace.group_of(focused) {
            self.group_epoch.insert(group, now);
        }
    }

    /// Drop every reference (vault switch): no cross-vault identity may
    /// survive (D-03).
    pub fn reset(&mut self) {
        self.group_epoch.clear();
        self.tab_epoch.clear();
        self.counter = 0;
    }

    /// Focus epoch of a tab (0 = never observed focused).
    #[must_use]
    pub fn tab_epoch(&self, tab: TabId) -> u64 {
        self.tab_epoch.get(&tab).copied().unwrap_or(0)
    }

    /// Focus epoch of a group (0 = never observed focused).
    #[must_use]
    pub fn group_epoch(&self, group: GroupId) -> u64 {
        self.group_epoch.get(&group).copied().unwrap_or(0)
    }
}

/// The owned workspace facts one destination decision reads. Built from the
/// live `App` (or a test fixture) so the decision itself stays pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationScan {
    /// Note tabs ascending by `TabId`: `(tab, locator, group)`.
    pub note_tabs: Vec<(TabId, NoteLocator, GroupId)>,
    /// Live groups in stable traversal order.
    pub group_order: Vec<GroupId>,
    /// The preferred document group: the last live group with acknowledged
    /// document focus (the document anchor's group), when any.
    pub preferred: Option<GroupId>,
    /// The source group of the navigation origin, when any.
    pub source_group: Option<GroupId>,
    /// The navigation origin is a graph view: its group must not lose
    /// visibility to a hidden-note activation (D-03).
    pub source_is_graph: bool,
    /// A neighboring document group could be created (geometry admits a
    /// split beside the source group).
    pub split_permits: bool,
    /// Narrow adaptation: one group visible at a time (D-03's only narrow
    /// fallback is the source group).
    pub narrow: bool,
}

/// The outcome of the reuse decision over one [`DestinationScan`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReuseDecision {
    /// Activate this existing matching note tab.
    Activate(TabId),
    /// Open a new tab in this group.
    OpenIn(GroupId),
    /// Create a neighboring document group by splitting the source group,
    /// then open a new tab there (transaction-safe: two guarded actions).
    SplitSourceThenOpen,
    /// No destination exists; the intent is refused with this explanation.
    Refuse(&'static str),
}

/// Visibility filter injected by the caller (mounted bounds, maximized and
/// adaptive narrow state).
pub type Visible<'a> = dyn Fn(TabId) -> bool + 'a;

/// The D-03 `ReuseDocument` decision over one scan. Pure: the same scan,
/// memory, locator and visibility always decide the same way.
#[must_use]
pub fn choose_reuse(
    scan: &DestinationScan,
    memory: &OpenMemory,
    locator: &NoteLocator,
    visible: &Visible<'_>,
) -> ReuseDecision {
    // Matching note tabs, ascending TabId.
    let matches: Vec<(TabId, GroupId)> = scan
        .note_tabs
        .iter()
        .filter(|(_, candidate, _)| candidate == locator)
        .map(|(tab, _, group)| (*tab, *group))
        .collect();
    // MRU tie-break: highest last-focus epoch first, then ascending TabId.
    let mru_tabs = |mut tabs: Vec<TabId>| {
        tabs.sort_by_key(|tab| std::cmp::Reverse(memory.tab_epoch(*tab)));
        tabs
    };
    // 1. A visible matching note in the preferred document group.
    if let Some(preferred) = scan.preferred {
        let in_preferred: Vec<TabId> = matches
            .iter()
            .filter(|(tab, group)| *group == preferred && visible(*tab))
            .map(|(tab, _)| *tab)
            .collect();
        if let Some(tab) = mru_tabs(in_preferred).first() {
            return ReuseDecision::Activate(*tab);
        }
    }
    // 2. Another visible matching note anywhere.
    let visible_matches: Vec<TabId> = matches
        .iter()
        .filter(|(tab, _)| visible(*tab))
        .map(|(tab, _)| *tab)
        .collect();
    if let Some(tab) = mru_tabs(visible_matches).first() {
        return ReuseDecision::Activate(*tab);
    }
    // 3. A matching note in the most recently used live document group.
    for group in document_groups_mru(scan, memory) {
        let in_group: Vec<TabId> = matches
            .iter()
            .filter(|(_, candidate_group)| *candidate_group == group)
            .map(|(tab, _)| *tab)
            .collect();
        if let Some(tab) = mru_tabs(in_group).first() {
            return ReuseDecision::Activate(*tab);
        }
    }
    // 4. A new tab in the fallback destination group.
    match new_tab_destination(scan, &document_groups_mru(scan, memory)) {
        NewTabDestination::In(group) => ReuseDecision::OpenIn(group),
        NewTabDestination::SplitSource => ReuseDecision::SplitSourceThenOpen,
        NewTabDestination::Refuse(reason) => ReuseDecision::Refuse(reason),
    }
}

/// Where a *new* note tab should be created when no existing view is
/// reused (`ReuseDocument` step 4 and every `NewTab` intent).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewTabDestination {
    /// Open a new tab in this group.
    In(GroupId),
    /// Create a neighboring document group from the source group first.
    SplitSource,
    /// No destination; refuse with this explanation.
    Refuse(&'static str),
}

/// The fallback chain for creating a destination group (D-03): the
/// preferred group, then the most recently used document group, then —
/// only under narrow adaptation — the source group; then a neighboring
/// document group when geometry permits; then the source group itself
/// (the caller exposes the return path); then any live group.
#[must_use]
pub fn new_tab_destination(
    scan: &DestinationScan,
    document_groups_mru: &[GroupId],
) -> NewTabDestination {
    if let Some(preferred) = scan.preferred {
        return NewTabDestination::In(preferred);
    }
    if let Some(group) = document_groups_mru.first() {
        return NewTabDestination::In(*group);
    }
    if scan.narrow
        && let Some(source) = scan.source_group
    {
        // Adaptive narrow mode shows one group at a time; a fresh split
        // would hide the source anyway. This is the only narrow fallback
        // (D-13).
        return NewTabDestination::In(source);
    }
    if scan.split_permits && scan.source_group.is_some() {
        return NewTabDestination::SplitSource;
    }
    if let Some(source) = scan.source_group {
        return NewTabDestination::In(source);
    }
    if let Some(group) = scan.group_order.first() {
        return NewTabDestination::In(*group);
    }
    NewTabDestination::Refuse("Open a group before opening notes")
}

/// Document groups most-recently-used first, excluding the source group
/// when the navigation origin is a graph (D-03: a hidden match must not be
/// activated over the source graph).
#[must_use]
pub fn document_groups_mru(scan: &DestinationScan, memory: &OpenMemory) -> Vec<GroupId> {
    let mut groups: Vec<GroupId> = scan
        .group_order
        .iter()
        .filter(|group| {
            !(scan.source_is_graph && scan.source_group == Some(**group))
                && scan
                    .note_tabs
                    .iter()
                    .any(|(_, _, group_of_note)| group_of_note == *group)
        })
        .copied()
        .collect();
    groups.sort_by_key(|group| std::cmp::Reverse(memory.group_epoch(*group)));
    groups
}

/// Route one typed open intent (D-03). The single entry point every
/// navigation surface uses; per-note deduplication, destination ordering,
/// and same-tab safety all live here.
pub fn open(app: &mut App, intent: OpenIntent) {
    match intent.destination {
        OpenDestination::ReplaceTab(tab) => replace_tab(app, tab, intent.locator),
        OpenDestination::ReuseDocument => {
            let scan = scan_workspace(app, intent.source);
            let decision = choose_reuse(&scan, &app.open_memory, &intent.locator, &|tab| {
                app.is_tab_visible(tab)
            });
            match decision {
                ReuseDecision::Activate(tab) => {
                    app.dispatch_layout(|revision| LayoutAction::ActivateTab {
                        expected_revision: revision,
                        tab,
                    });
                }
                ReuseDecision::OpenIn(group) => {
                    open_in_group(app, intent.locator, group, false);
                }
                ReuseDecision::SplitSourceThenOpen => {
                    let OpenIntent {
                        locator, source, ..
                    } = intent;
                    if !split_source_then_open(app, locator.clone(), source) {
                        // The split was refused (geometry): the source group
                        // is the honest last resort; expose the return path.
                        open_in_source_group(app, locator, source);
                    }
                }
                ReuseDecision::Refuse(reason) => app.status = reason.to_owned(),
            }
        }
        OpenDestination::NewTab => {
            // View dedup is bypassed; the fallback chain still picks the
            // destination (D-03).
            let scan = scan_workspace(app, intent.source);
            match new_tab_destination(&scan, &document_groups_mru(&scan, &app.open_memory)) {
                NewTabDestination::In(group) => {
                    open_in_group(app, intent.locator, group, false);
                }
                NewTabDestination::SplitSource => {
                    let OpenIntent {
                        locator, source, ..
                    } = intent;
                    if !split_source_then_open(app, locator.clone(), source) {
                        open_in_source_group(app, locator, source);
                    }
                }
                NewTabDestination::Refuse(reason) => app.status = reason.to_owned(),
            }
        }
    }
}

/// Open a new note tab in `group`, guarding the transaction. When
/// `expose_return` is set the source graph lost visibility, so the status
/// line names the way back (D-03 "Return to graph").
fn open_in_group(app: &mut App, locator: NoteLocator, group: GroupId, expose_return: bool) {
    if !app.workspace.groups().any(|(id, _)| *id == group) {
        app.status = "Open a group before opening notes".into();
        return;
    }
    let committed = app.dispatch_layout_result(|revision| LayoutAction::OpenNote {
        expected_revision: revision,
        locator: locator.clone(),
        group,
    });
    if committed && expose_return {
        app.status =
            "Opened in the graph's group — no space for a document group; return via the graph tab"
                .into();
    }
}

/// Explicit same-tab navigation (quick switch): the tab keeps its identity
/// and mode, the replaced note enters Back. Same contract as the reducer's
/// in-tab navigation: the replaced view's caret authority is dropped, and
/// the shared buffer/entry stays with the document registry until the last
/// lease closes (navigation participates in the last-view recovery
/// contract, D-06).
pub fn replace_tab(app: &mut App, tab: TabId, locator: NoteLocator) {
    if let Some(previous) = app.note_locator(tab) {
        if previous == locator {
            app.status = "This tab already shows that note".into();
            return;
        }
        let anchor = app
            .workspace
            .tab(&tab)
            .map(|session| session.interaction().viewport_anchor);
        let previous_clone = previous.clone();
        let committed = app.dispatch_layout_result(|revision| LayoutAction::NavigateTab {
            expected_revision: revision,
            tab,
            locator: locator.clone(),
        });
        if committed {
            app.nav
                .record_replacement(tab, previous_clone, anchor, locator);
        }
        return;
    }
    // Not an attached note tab: the explicit same-tab request cannot cross
    // view kinds — fall back to the default policy.
    open(app, OpenIntent::reuse(locator, Some(tab)));
}

/// Build the destination scan from the live workspace. `preferred` is the
/// last live group with acknowledged document focus (D-01: the document
/// anchor), independent of current graph focus.
#[must_use]
pub fn scan_workspace(app: &App, source: Option<TabId>) -> DestinationScan {
    let note_tabs = app
        .workspace
        .tabs()
        .filter_map(|(tab, session)| match session.kind() {
            ViewKind::Note(note) => {
                let group = app.workspace.group_of(*tab)?;
                Some((*tab, note.locator().clone(), group))
            }
            _ => None,
        })
        .collect();
    let source_is_graph = source.is_some_and(|tab| {
        matches!(
            app.workspace.tab(&tab).map(|session| session.kind()),
            Some(ViewKind::Graph(_))
        )
    });
    let source_group = source.and_then(|tab| app.workspace.group_of(tab));
    let split_permits = source.zip(source_group).is_some_and(|(tab, group)| {
        app.split_fits(tab, group, flavite_workspace::Arrangement::SideBySide)
    });
    DestinationScan {
        note_tabs,
        group_order: app.workspace.group_order(),
        preferred: app
            .workspace
            .document_anchor()
            .and_then(|anchor| app.workspace.group_of(anchor)),
        source_group,
        source_is_graph,
        split_permits,
        narrow: !flavite_workspace::fits(
            &app.workspace,
            flavite_workspace::Size {
                width: app.workspace_bounds.width,
                height: app.workspace_bounds.height,
            },
            app.appearance.metrics().scale,
        ),
    }
}

/// Create a neighboring document group by splitting the source group, then
/// open the note there. Two revision-guarded transactions; the duplicated
/// source view stays as a real tab in the new group (a split creates a new
/// view by definition, D-03). Returns whether both transactions committed.
fn split_source_then_open(app: &mut App, locator: NoteLocator, source: Option<TabId>) -> bool {
    let Some(source_group) = source.and_then(|tab| app.workspace.group_of(tab)) else {
        return false;
    };
    let before: std::collections::BTreeSet<GroupId> =
        app.workspace.group_order().into_iter().collect();
    let committed = app.dispatch_layout_result(|revision| LayoutAction::SplitGroup {
        expected_revision: revision,
        group: source_group,
        arrangement: flavite_workspace::Arrangement::SideBySide,
    });
    if !committed {
        return false;
    }
    let Some(created) = app
        .workspace
        .group_order()
        .into_iter()
        .find(|group| !before.contains(group))
    else {
        return false;
    };
    open_in_group(app, locator, created, false);
    true
}

/// The last-resort fallback: a new document tab in the source group with
/// the return path exposed (D-03). Without any source group, the first
/// live group receives the tab.
fn open_in_source_group(app: &mut App, locator: NoteLocator, source: Option<TabId>) {
    let group = source
        .and_then(|tab| app.workspace.group_of(tab))
        .or_else(|| app.workspace.first_group());
    match group {
        Some(group) => open_in_group(app, locator, group, true),
        None => app.status = "Open a group before opening notes".into(),
    }
}

/// The multi-open ceiling for one graph invocation (D-03: `Open selected in
/// tabs` is limited to 20 notes per invocation; more requires an explicit
/// selection reduction).
pub const MAX_MULTI_OPEN: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;

    fn locator(name: &str) -> NoteLocator {
        NoteLocator::from_components([name]).expect("locator")
    }

    fn group(id: u64) -> GroupId {
        GroupId::from_raw(u128::from(id))
    }

    fn tab(id: u64) -> TabId {
        TabId::from_raw(u128::from(id))
    }

    /// Three movable groups: A holds a visible duplicate of the target,
    /// B (source graph group) holds a hidden one, C is a document group
    /// without the note.
    fn scan() -> DestinationScan {
        DestinationScan {
            note_tabs: vec![
                (tab(11), locator("dup.md"), group(1)),
                (tab(12), locator("other.md"), group(2)),
                (tab(13), locator("dup.md"), group(2)),
            ],
            group_order: vec![group(1), group(2), group(3)],
            preferred: Some(group(3)),
            source_group: Some(group(2)),
            source_is_graph: true,
            split_permits: true,
            narrow: false,
        }
    }

    fn everything_visible(_: TabId) -> bool {
        true
    }

    fn nothing_visible(_: TabId) -> bool {
        false
    }

    #[test]
    fn preferred_visible_match_wins() {
        let memory = OpenMemory::default();
        let mut scan = scan();
        scan.preferred = Some(group(1));
        assert_eq!(
            choose_reuse(&scan, &memory, &locator("dup.md"), &everything_visible),
            ReuseDecision::Activate(tab(11))
        );
    }

    #[test]
    fn visible_match_outside_preferred_group_is_reused() {
        let mut memory = OpenMemory::default();
        memory.seed_tab(tab(13), 4);
        let scan = scan();
        assert_eq!(
            choose_reuse(&scan, &memory, &locator("dup.md"), &everything_visible),
            ReuseDecision::Activate(tab(13)),
            "the more recently focused visible match wins the tie-break"
        );
    }

    /// D-03: a hidden matching note must not be activated over the source
    /// graph merely because it is the first match in a map. With the only
    /// match hidden in the source graph group, the decision falls through
    /// to a new tab in the preferred group.
    #[test]
    fn hidden_match_in_source_graph_group_is_never_activated() {
        let memory = OpenMemory::default();
        let mut scan = scan();
        scan.note_tabs = vec![
            (tab(12), locator("other.md"), group(2)),
            (tab(13), locator("dup.md"), group(2)),
        ];
        assert_eq!(
            choose_reuse(&scan, &memory, &locator("dup.md"), &nothing_visible),
            ReuseDecision::OpenIn(group(3)),
            "never the hidden source-group tab"
        );
    }

    #[test]
    fn mru_document_group_order_decides_step_three() {
        let mut memory = OpenMemory::default();
        let mut scan = scan();
        scan.preferred = None;
        // Group 2 focused more recently than group 1 — but group 2 is the
        // source graph group, so it is not a document destination. Group 1
        // is the MRU document group and its hidden match (a non-source
        // group, so the graph keeps its visibility) activates.
        memory.seed_group(group(1), 1);
        memory.seed_group(group(2), 5);
        assert_eq!(
            choose_reuse(&scan, &memory, &locator("dup.md"), &nothing_visible),
            ReuseDecision::Activate(tab(11))
        );
        // Without the graph origin, the more recently focused group 2 wins
        // and its highest-focus match activates.
        let mut not_graph = scan.clone();
        not_graph.source_is_graph = false;
        assert_eq!(
            choose_reuse(&not_graph, &memory, &locator("dup.md"), &nothing_visible),
            ReuseDecision::Activate(tab(13))
        );
    }

    #[test]
    fn narrow_mode_falls_back_to_the_source_group() {
        let memory = OpenMemory::default();
        let mut scan = scan();
        scan.preferred = None;
        scan.note_tabs.clear();
        scan.narrow = true;
        scan.split_permits = true;
        assert_eq!(
            choose_reuse(&scan, &memory, &locator("new.md"), &nothing_visible),
            ReuseDecision::OpenIn(group(2)),
            "the source group is the only narrow fallback"
        );
    }

    #[test]
    fn wide_layout_creates_a_neighboring_document_group() {
        let memory = OpenMemory::default();
        let mut scan = scan();
        scan.preferred = None;
        scan.note_tabs.clear();
        scan.narrow = false;
        scan.split_permits = true;
        assert_eq!(
            choose_reuse(&scan, &memory, &locator("new.md"), &nothing_visible),
            ReuseDecision::SplitSourceThenOpen
        );
    }

    #[test]
    fn no_geometry_opens_in_the_source_group() {
        let memory = OpenMemory::default();
        let mut scan = scan();
        scan.preferred = None;
        scan.note_tabs.clear();
        scan.narrow = false;
        scan.split_permits = false;
        assert_eq!(
            choose_reuse(&scan, &memory, &locator("new.md"), &nothing_visible),
            ReuseDecision::OpenIn(group(2))
        );
    }

    #[test]
    fn no_group_at_all_refuses_with_a_repair_message() {
        let memory = OpenMemory::default();
        let scan = DestinationScan {
            note_tabs: vec![],
            group_order: vec![],
            preferred: None,
            source_group: None,
            source_is_graph: false,
            split_permits: false,
            narrow: false,
        };
        assert_eq!(
            choose_reuse(&scan, &memory, &locator("new.md"), &nothing_visible),
            ReuseDecision::Refuse("Open a group before opening notes")
        );
    }

    #[test]
    fn new_tab_destination_prefers_preferred_group() {
        let scan = scan();
        let groups = vec![group(1)];
        assert_eq!(
            new_tab_destination(&scan, &groups),
            NewTabDestination::In(group(3)),
            "NewTab bypasses dedup but still uses the preferred group"
        );
    }

    #[test]
    fn memory_observation_drops_dead_references_and_stamps_focus() {
        let mut workspace = flavite_workspace::WorkspaceState::new();
        let mut memory = OpenMemory::default();
        let first_group = workspace.first_group().expect("one group");
        let focused = workspace.focused_tab();
        memory.observe(&workspace);
        assert_eq!(memory.group_epoch(first_group), 1);
        assert_eq!(memory.tab_epoch(focused), 1);
        // A committed transaction that the memory re-observes stamps a new
        // epoch for the focused group and never resurrects closed ones.
        let outcome = flavite_workspace::reduce(
            &workspace,
            flavite_workspace::LayoutAction::OpenGraph {
                expected_revision: workspace.revision(),
                group: first_group,
            },
        );
        let flavite_workspace::Outcome::Committed(commit) = outcome else {
            panic!("the fixture transaction must commit");
        };
        workspace = commit.state;
        memory.observe(&workspace);
        assert!(memory.group_epoch(first_group) >= 2);
    }

    /// Test seeding of the private focus epochs (same-crate visibility).
    impl OpenMemory {
        fn seed_group(&mut self, group: GroupId, epoch: u64) {
            self.counter = self.counter.max(epoch);
            self.group_epoch.insert(group, epoch);
        }

        fn seed_tab(&mut self, tab: TabId, epoch: u64) {
            self.counter = self.counter.max(epoch);
            self.tab_epoch.insert(tab, epoch);
        }
    }
}
