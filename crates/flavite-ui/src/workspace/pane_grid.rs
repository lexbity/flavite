//! `pane_grid` projection of the workspace layout tree (RFC-0003 D-02).
//!
//! Iced pane and split ids are disposable adapter keys (§7.1). The adapter
//! is rebuilt deterministically from the authoritative tree after every
//! commit; a bidirectional group↔pane map keeps event routing stable.

use flavite_workspace::{Arrangement, Edge, GroupId, LayoutNode, Side, SplitId, WorkspaceState};
use iced::widget::pane_grid::{self, Axis, Configuration};
use std::collections::BTreeMap;

/// The per-pane content of the toolkit projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupPane {
    /// The workspace group this pane displays.
    pub group: GroupId,
}

/// The disposable projection. Rebuilt by [`Adapter::reconcile`].
#[derive(Debug, Clone)]
pub struct Adapter {
    /// The toolkit pane state; each pane holds its group identity.
    pub panes: pane_grid::State<GroupPane>,
    /// group → toolkit pane.
    pub group_to_pane: BTreeMap<GroupId, pane_grid::Pane>,
    /// toolkit pane → group.
    pub pane_to_group: BTreeMap<pane_grid::Pane, GroupId>,
    /// workspace split → toolkit split (same pre-order position).
    pub split_map: BTreeMap<SplitId, pane_grid::Split>,
    /// toolkit split → workspace split.
    pub split_reverse: BTreeMap<pane_grid::Split, SplitId>,
    /// Workspace split ids in pre-order, captured at build time so the
    /// positional zip with `Node::splits` pairs them deterministically.
    ordered_splits: Vec<SplitId>,
}

impl Adapter {
    /// Build the projection for `state`.
    #[must_use]
    pub fn build(state: &WorkspaceState) -> Self {
        let ordered_splits = collect_splits(state.tree());
        let fallback_group = state.first_group().unwrap_or_else(|| GroupId::from_raw(0));
        let mut adapter = Self {
            panes: pane_grid::State::with_configuration(Configuration::Pane(GroupPane {
                group: fallback_group,
            })),
            group_to_pane: BTreeMap::new(),
            pane_to_group: BTreeMap::new(),
            split_map: BTreeMap::new(),
            split_reverse: BTreeMap::new(),
            ordered_splits,
        };
        if state.first_group().is_some() {
            adapter.panes = pane_grid::State::with_configuration(build_configuration(state.tree()));
        }
        adapter.reindex();
        adapter
    }

    /// Rebuild the pane projection from the authoritative tree.
    pub fn reconcile(&mut self, state: &WorkspaceState) {
        self.ordered_splits = collect_splits(state.tree());
        self.panes = if state.first_group().is_some() {
            pane_grid::State::with_configuration(build_configuration(state.tree()))
        } else {
            pane_grid::State::with_configuration(Configuration::Pane(GroupPane {
                group: GroupId::from_raw(0),
            }))
        };
        self.reindex();
    }

    /// Rebuild the id maps after any projection rebuild. Both trees are
    /// walked in the same pre-order, so the split mapping is positional.
    fn reindex(&mut self) {
        self.group_to_pane.clear();
        self.pane_to_group.clear();
        self.split_map.clear();
        self.split_reverse.clear();
        for (pane, content) in self.panes.iter() {
            self.group_to_pane.insert(content.group, *pane);
            self.pane_to_group.insert(*pane, content.group);
        }
        let iced_splits: Vec<pane_grid::Split> = self.panes.layout().splits().copied().collect();
        for (workspace_split, iced_split) in self.ordered_splits.iter().zip(iced_splits) {
            self.split_map.insert(*workspace_split, iced_split);
            self.split_reverse.insert(iced_split, *workspace_split);
        }
    }

    /// The group displayed by a toolkit pane.
    #[must_use]
    pub fn group_of_pane(&self, pane: pane_grid::Pane) -> Option<GroupId> {
        self.pane_to_group.get(&pane).copied()
    }

    /// The toolkit pane of a group.
    #[must_use]
    pub fn pane_of_group(&self, group: &GroupId) -> Option<pane_grid::Pane> {
        self.group_to_pane.get(group).copied()
    }

    /// The workspace split behind a toolkit split resize event.
    #[must_use]
    pub fn workspace_split(&self, split: pane_grid::Split) -> Option<SplitId> {
        self.split_reverse.get(&split).copied()
    }

    /// Rectangles for every group inside `bounds` (adapter coordinates).
    /// These are the toolkit's own computed regions, used by tests to prove
    /// the projection matches the authoritative solver.
    #[must_use]
    pub fn pane_rectangles(
        &self,
        spacing: f32,
        bounds: iced::Size,
    ) -> BTreeMap<GroupId, iced::Rectangle> {
        self.panes
            .layout()
            .pane_regions(spacing, 0.0, bounds)
            .into_iter()
            .filter_map(|(pane, rect)| self.pane_to_group.get(&pane).map(|group| (*group, rect)))
            .collect()
    }
}

fn collect_splits(node: &LayoutNode) -> Vec<SplitId> {
    let mut out = Vec::new();
    collect_splits_into(node, &mut out);
    out
}

fn collect_splits_into(node: &LayoutNode, out: &mut Vec<SplitId>) {
    if let LayoutNode::Split {
        id, first, second, ..
    } = node
    {
        out.push(*id);
        collect_splits_into(first, out);
        collect_splits_into(second, out);
    }
}

fn build_configuration(node: &LayoutNode) -> Configuration<GroupPane> {
    match node {
        LayoutNode::Group(group) => Configuration::Pane(GroupPane { group: *group }),
        LayoutNode::Split {
            arrangement,
            desired_ratio_ppm,
            first,
            second,
            ..
        } => Configuration::Split {
            axis: match arrangement {
                // §7.5: SideBySide is iced's vertical dividing line.
                Arrangement::SideBySide => Axis::Vertical,
                Arrangement::Stacked => Axis::Horizontal,
            },
            ratio: f32_ratio(*desired_ratio_ppm),
            a: Box::new(build_configuration(first)),
            b: Box::new(build_configuration(second)),
        },
    }
}

/// Ratio ppm to f32. Splitting by 10_000 first keeps the value inside the
/// u16 range, which converts to f32 exactly.
fn f32_ratio(ratio_ppm: u32) -> f32 {
    #[allow(clippy::cast_precision_loss)]
    {
        f32::from((ratio_ppm / 100) as u16) / 10_000.0
    }
}

/// Map a pane-grid drop edge to the workspace model.
#[must_use]
pub const fn workspace_edge(edge: pane_grid::Edge) -> Edge {
    match edge {
        pane_grid::Edge::Top => Edge::Top,
        pane_grid::Edge::Left => Edge::Left,
        pane_grid::Edge::Right => Edge::Right,
        pane_grid::Edge::Bottom => Edge::Bottom,
    }
}

/// Map a pane-grid drop region edge to arrangement and side.
#[must_use]
pub const fn workspace_split_target(edge: pane_grid::Edge) -> (Arrangement, Side) {
    let mapped = workspace_edge(edge);
    (mapped.arrangement(), mapped.side())
}
