//! The binary layout tree and tab groups (RFC-0003 §7.2/§7.3).
//!
//! The tree is authoritative: validated application-owned structure with
//! opaque persistent ids. Ratios are stored as integer parts per million so
//! persistence never depends on float serialization.

use crate::ids::{GroupId, SplitId, TabId};

/// Supported workspace scale (NFR-01).
pub const MAX_GROUPS: usize = 32;
/// Supported workspace scale (NFR-01).
pub const MAX_TABS: usize = 128;
/// Supported layout depth (NFR-01).
pub const MAX_DEPTH: usize = 16;
/// Reopen-closed-tab history length (NFR-01).
pub const MAX_REOPEN: usize = 20;
/// Minimum desired first-child ratio, in parts per million (§7.2).
pub const MIN_RATIO_PPM: u32 = 50_000;
/// Maximum desired first-child ratio, in parts per million (§7.2).
pub const MAX_RATIO_PPM: u32 = 950_000;
/// Ratio assigned to newly created splits (§7.4).
pub const DEFAULT_RATIO_PPM: u32 = 500_000;

/// Split arrangement. Axis names are explicit; nothing ambiguous is
/// persisted (§7.2). `SideBySide` divides along the horizontal axis (a
/// vertical divider), `Stacked` along the vertical axis (a horizontal
/// divider).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Arrangement {
    /// Children left/right of a vertical divider.
    SideBySide,
    /// Children above/below a horizontal divider.
    Stacked,
}

/// An outer edge of the whole workspace, used by pane-edge drops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edge {
    /// Top edge of the workspace.
    Top,
    /// Left edge of the workspace.
    Left,
    /// Right edge of the workspace.
    Right,
    /// Bottom edge of the workspace.
    Bottom,
}

impl Edge {
    /// The split arrangement that an edge drop produces.
    #[must_use]
    pub const fn arrangement(self) -> Arrangement {
        match self {
            Self::Top | Self::Bottom => Arrangement::Stacked,
            Self::Left | Self::Right => Arrangement::SideBySide,
        }
    }

    /// Whether the dropped group becomes the first child.
    #[must_use]
    pub const fn side(self) -> Side {
        match self {
            Self::Top | Self::Left => Side::First,
            Self::Bottom | Self::Right => Side::Second,
        }
    }
}

/// First or second child of a new split.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// First child (left for side-by-side, top for stacked).
    First,
    /// Second child (right for side-by-side, bottom for stacked).
    Second,
}

/// The authoritative binary layout tree: groups as leaves, splits as
/// interior nodes. Every group appears at most once; every split id is
/// unique (§7.3 invariant 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutNode {
    /// A leaf holding one tab group.
    Group(GroupId),
    /// An interior split with a desired first-child ratio in ppm.
    Split {
        /// Unique split identity.
        id: SplitId,
        /// How the two children divide the region.
        arrangement: Arrangement,
        /// Desired first-child fraction, `MIN_RATIO_PPM..=MAX_RATIO_PPM`.
        desired_ratio_ppm: u32,
        /// First child.
        first: Box<LayoutNode>,
        /// Second child.
        second: Box<LayoutNode>,
    },
}

impl LayoutNode {
    /// Length of the longest root-to-leaf chain (a lone group has depth 1).
    #[must_use]
    pub fn depth(&self) -> usize {
        match self {
            Self::Group(_) => 1,
            Self::Split { first, second, .. } => 1 + first.depth().max(second.depth()),
        }
    }

    /// Groups in deterministic pre-order (top-left first).
    #[must_use]
    pub fn groups(&self) -> Vec<GroupId> {
        let mut out = Vec::new();
        self.collect_groups(&mut out);
        out
    }

    fn collect_groups(&self, out: &mut Vec<GroupId>) {
        match self {
            Self::Group(id) => out.push(*id),
            Self::Split { first, second, .. } => {
                first.collect_groups(out);
                second.collect_groups(out);
            }
        }
    }

    /// Split ids in deterministic pre-order.
    #[must_use]
    pub fn splits(&self) -> Vec<SplitId> {
        let mut out = Vec::new();
        self.collect_splits(&mut out);
        out
    }

    fn collect_splits(&self, out: &mut Vec<SplitId>) {
        if let Self::Split {
            id, first, second, ..
        } = self
        {
            out.push(*id);
            first.collect_splits(out);
            second.collect_splits(out);
        }
    }

    /// Whether the tree contains the group.
    #[must_use]
    pub fn contains_group(&self, id: GroupId) -> bool {
        match self {
            Self::Group(group) => *group == id,
            Self::Split { first, second, .. } => {
                first.contains_group(id) || second.contains_group(id)
            }
        }
    }

    /// Wrap the leaf `target` with a new split introducing `leaf` as the
    /// given side. Returns false when `target` is not a leaf of this tree.
    pub(crate) fn insert_group_leaf(
        &mut self,
        target: GroupId,
        split_id: SplitId,
        arrangement: Arrangement,
        side: Side,
        leaf: GroupId,
    ) -> bool {
        match self {
            Self::Group(group) if *group == target => {
                let (first, second) = match side {
                    Side::First => (LayoutNode::Group(leaf), LayoutNode::Group(target)),
                    Side::Second => (LayoutNode::Group(target), LayoutNode::Group(leaf)),
                };
                *self = Self::Split {
                    id: split_id,
                    arrangement,
                    desired_ratio_ppm: DEFAULT_RATIO_PPM,
                    first: Box::new(first),
                    second: Box::new(second),
                };
                true
            }
            Self::Group(_) => false,
            Self::Split { first, second, .. } => {
                first.insert_group_leaf(target, split_id, arrangement, side, leaf)
                    || second.insert_group_leaf(target, split_id, arrangement, side, leaf)
            }
        }
    }

    /// Remove the subtree `target`, collapsing the smallest split that
    /// contained it. Returns `None` when the target was the root itself.
    pub(crate) fn detach_group(&mut self, target: GroupId) -> Option<()> {
        let (replacement, found) =
            Self::detach_recursive(std::mem::replace(self, Self::Group(target)), target);
        match (replacement, found) {
            (Some(replacement), true) => {
                *self = replacement;
                Some(())
            }
            // The root itself was the target leaf: the tree is empty and the
            // caller decides what replaces it.
            (None, true) => None,
            (Some(replacement), false) => {
                *self = replacement;
                None
            }
            (None, false) => unreachable!("a non-target group reconstructs itself"),
        }
    }

    /// Returns `(replacement, found)`. The replacement is `None` exactly
    /// when the node itself was the target leaf; the enclosing split then
    /// collapses to the sibling.
    fn detach_recursive(node: Self, target: GroupId) -> (Option<Self>, bool) {
        match node {
            Self::Group(group) if group == target => (None, true),
            Self::Group(group) => (Some(Self::Group(group)), false),
            Self::Split {
                id,
                arrangement,
                desired_ratio_ppm,
                first,
                second,
            } => {
                let (first, found) = Self::detach_recursive(*first, target);
                if found {
                    return match first {
                        // First was exactly the target leaf: collapse to
                        // the sibling subtree.
                        None => (Some(*second), true),
                        // The target was deeper inside first: keep the
                        // split with its repaired first child.
                        Some(first) => (
                            Some(Self::Split {
                                id,
                                arrangement,
                                desired_ratio_ppm,
                                first: Box::new(first),
                                second,
                            }),
                            true,
                        ),
                    };
                }
                let (second, found) = Self::detach_recursive(*second, target);
                if found {
                    return match second {
                        None => (first, true),
                        Some(second) => (
                            Some(Self::Split {
                                id,
                                arrangement,
                                desired_ratio_ppm,
                                first: Box::new(
                                    first.expect("first survives when the target is in second"),
                                ),
                                second: Box::new(second),
                            }),
                            true,
                        ),
                    };
                }
                (
                    Some(Self::Split {
                        id,
                        arrangement,
                        desired_ratio_ppm,
                        first: Box::new(first.expect("first survives when the target is absent")),
                        second: Box::new(
                            second.expect("second survives when the target is absent"),
                        ),
                    }),
                    false,
                )
            }
        }
    }
}

/// One movable tab group: an ordered tab list with exactly one active tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TabGroup {
    tabs: Vec<TabId>,
    active: TabId,
}

impl TabGroup {
    /// Create a group with the single tab `active`.
    #[must_use]
    pub fn new(active: TabId) -> Self {
        Self {
            tabs: vec![active],
            active,
        }
    }

    /// Tabs in display order.
    #[must_use]
    pub fn tabs(&self) -> &[TabId] {
        &self.tabs
    }

    /// The active tab.
    #[must_use]
    pub const fn active(&self) -> TabId {
        self.active
    }

    pub(crate) fn set_active(&mut self, tab: TabId) {
        self.active = tab;
    }

    pub(crate) fn insert(&mut self, index: usize, tab: TabId) {
        let index = index.min(self.tabs.len());
        self.tabs.insert(index, tab);
        self.active = tab;
    }

    pub(crate) fn remove(&mut self, tab: TabId) -> Option<TabId> {
        let position = self.tabs.iter().position(|candidate| *candidate == tab)?;
        self.tabs.remove(position);
        if self.active == tab {
            // RFC §7.4: focus chooses the tab immediately to the left, else
            // the first remaining tab.
            let next = position
                .checked_sub(1)
                .or(Some(0))
                .and_then(|index| self.tabs.get(index).copied());
            self.active = next?;
        } else if self.tabs.is_empty() {
            return None;
        }
        Some(self.active)
    }

    pub(crate) fn push_all(&mut self, tabs: impl IntoIterator<Item = TabId>) {
        self.tabs.extend(tabs);
    }

    /// Remove and return every tab, leaving the group empty (merge handoff).
    pub(crate) fn take_all(&mut self) -> Vec<TabId> {
        self.active = TabId::from_raw(0);
        std::mem::take(&mut self.tabs)
    }

    pub(crate) fn next_active(&self, from: TabId, forward: bool) -> Option<TabId> {
        let position = self.tabs.iter().position(|tab| *tab == from)?;
        let next = if forward {
            (position + 1) % self.tabs.len()
        } else {
            (position + self.tabs.len() - 1) % self.tabs.len()
        };
        self.tabs.get(next).copied()
    }

    /// The group of the smallest enclosing tree, used by geometry. Present
    /// so the solver never needs the raw maps.
    pub(crate) fn index_of(&self, tab: TabId) -> Option<usize> {
        self.tabs.iter().position(|candidate| *candidate == tab)
    }

    pub(crate) fn retain(&mut self, keep: impl Fn(&TabId) -> bool) {
        self.tabs.retain(keep);
    }

    pub(crate) fn ensure_active(&mut self) {
        if !self.tabs.contains(&self.active)
            && let Some(first) = self.tabs.first().copied()
        {
            self.active = first;
        }
    }
}

/// Drop target for a tab move (§7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TabDropTarget {
    /// Insert at an index inside an existing group (center drop, palette
    /// move, keyboard reorder).
    Index {
        /// Destination group.
        group: GroupId,
        /// Destination index, clamped on apply.
        index: usize,
    },
    /// Move the tab into a new group split beside `group`.
    SplitFrom {
        /// The anchor group the new split forms beside.
        group: GroupId,
        /// Split arrangement.
        arrangement: Arrangement,
        /// Which side the moved tab's new group takes.
        side: Side,
    },
}
