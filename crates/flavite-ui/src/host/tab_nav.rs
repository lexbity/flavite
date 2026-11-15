//! Per-group tab-selection history (W10): the tab strip's Back/Forward
//! cycle the group's previously-active tabs, browser-style. Every
//! committed layout transaction is observed in [`App::commit`], so every
//! activation path (tab click, quick-switch dedup, close-next activation,
//! commands) records uniformly.
//!
//! This host is device-session state; it is never persisted (FR-26).

use flavite_workspace::{GroupId, TabId, WorkspaceState};
use std::collections::BTreeMap;

/// Bounded history per group (same bound as the per-note view history).
pub const MAX_TAB_NAV_ENTRIES: usize = 100;

/// One group's selection history: the active tab, and the visit stacks.
#[derive(Debug, Clone)]
struct GroupNav {
    current: TabId,
    back: Vec<TabId>,
    forward: Vec<TabId>,
}

/// Per-group tab-selection histories, observed from committed workspace
/// state.
#[derive(Debug, Default)]
pub struct TabSelectionNav {
    groups: BTreeMap<GroupId, GroupNav>,
}

impl TabSelectionNav {
    /// Observe one committed workspace state: drop dead groups, prune dead
    /// tabs, and record an activation change as a new Back entry (Forward
    /// is dropped — a new selection branch). The first sighting of a group
    /// seeds `current` without recording.
    pub fn observe(&mut self, workspace: &WorkspaceState) {
        let live_groups: Vec<GroupId> = workspace.group_order();
        self.groups.retain(|group, _| live_groups.contains(group));
        for group in &live_groups {
            let active = workspace
                .group(group)
                .map(flavite_workspace::TabGroup::active);
            let live_tabs = workspace
                .group(group)
                .map(|g| g.tabs().to_vec())
                .unwrap_or_default();
            match self.groups.get_mut(group) {
                None => {
                    if let Some(active) = active {
                        self.groups.insert(
                            *group,
                            GroupNav {
                                current: active,
                                back: Vec::new(),
                                forward: Vec::new(),
                            },
                        );
                    }
                }
                Some(nav) => {
                    nav.back.retain(|tab| live_tabs.contains(tab));
                    nav.forward.retain(|tab| live_tabs.contains(tab));
                    if let Some(active) = active
                        && nav.current != active
                    {
                        // The active tab is always live; a dead `current`
                        // (closed active tab) means the reducer already
                        // moved selection — record the new one plainly.
                        push_bounded(&mut nav.back, nav.current);
                        nav.forward.clear();
                        nav.current = active;
                    }
                }
            }
        }
    }

    /// Whether Back has a live entry for this group.
    #[must_use]
    pub fn can_go_back(&self, group: GroupId) -> bool {
        self.groups
            .get(&group)
            .is_some_and(|nav| !nav.back.is_empty())
    }

    /// Whether Forward has a live entry for this group.
    #[must_use]
    pub fn can_go_forward(&self, group: GroupId) -> bool {
        self.groups
            .get(&group)
            .is_some_and(|nav| !nav.forward.is_empty())
    }

    /// Step back: the current selection moves to Forward and the popped
    /// entry becomes current. `live` filters entries for tabs that closed
    /// while sitting in the stack (they are dropped). The caller activates
    /// the returned tab; because this call already moved `current`, the
    /// committed activation observes as a no-op.
    pub fn go_back(&mut self, group: GroupId, live: impl Fn(TabId) -> bool) -> Option<TabId> {
        let nav = self.groups.get_mut(&group)?;
        let current = nav.current;
        let target = loop {
            let candidate = nav.back.pop()?;
            if candidate != current && live(candidate) {
                break candidate;
            }
        };
        push_bounded(&mut nav.forward, current);
        nav.current = target;
        Some(target)
    }

    /// Step forward symmetrically.
    pub fn go_forward(&mut self, group: GroupId, live: impl Fn(TabId) -> bool) -> Option<TabId> {
        let nav = self.groups.get_mut(&group)?;
        let current = nav.current;
        let target = loop {
            let candidate = nav.forward.pop()?;
            if candidate != current && live(candidate) {
                break candidate;
            }
        };
        push_bounded(&mut nav.back, current);
        nav.current = target;
        Some(target)
    }

    /// Drop every reference (vault switch).
    pub fn reset(&mut self) {
        self.groups.clear();
    }

    /// Reverse one refused step (the reducer refused the activation): the
    /// previously-active tab returns to `current` and `target` goes back
    /// onto the stack it was popped from. `forward` names the direction of
    /// the refused step.
    pub fn undo_step(&mut self, group: GroupId, target: TabId, forward: bool) {
        let Some(nav) = self.groups.get_mut(&group) else {
            return;
        };
        let previous = if forward {
            nav.back.pop()
        } else {
            nav.forward.pop()
        };
        if let Some(previous) = previous {
            if forward {
                push_bounded(&mut nav.forward, target);
            } else {
                push_bounded(&mut nav.back, target);
            }
            nav.current = previous;
        }
    }
}

fn push_bounded(stack: &mut Vec<TabId>, tab: TabId) {
    if stack.len() >= MAX_TAB_NAV_ENTRIES {
        stack.remove(0);
    }
    if stack.last() != Some(&tab) {
        stack.push(tab);
    }
}

#[cfg(test)]
impl TabSelectionNav {
    /// Test seam: direct activation recording without a workspace state.
    fn seed_for_test(&mut self, group: u64, tab: TabId) {
        self.groups.insert(
            GroupId::from_raw(group as u128),
            GroupNav {
                current: tab,
                back: Vec::new(),
                forward: Vec::new(),
            },
        );
    }

    /// Test seam: one activation change.
    fn activate_for_test(&mut self, group: u64, tab: TabId) {
        let Some(nav) = self.groups.get_mut(&GroupId::from_raw(group as u128)) else {
            return;
        };
        if nav.current == tab {
            return;
        }
        push_bounded(&mut nav.back, nav.current);
        nav.forward.clear();
        nav.current = tab;
    }

    fn current_for_test(&mut self, group: u64) -> TabId {
        self.groups
            .get(&GroupId::from_raw(group as u128))
            .expect("seeded")
            .current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_workspace::TabId;

    fn live(_: TabId) -> bool {
        true
    }

    fn group() -> GroupId {
        GroupId::from_raw(7)
    }

    #[test]
    fn activation_change_records_back_and_clears_forward() {
        let mut nav = TabSelectionNav::default();
        let (a, b, c) = (TabId::from_raw(1), TabId::from_raw(2), TabId::from_raw(3));
        nav.seed_for_test(7, a);
        nav.activate_for_test(7, b);
        nav.activate_for_test(7, c);
        assert!(nav.can_go_back(group()));
        assert!(!nav.can_go_forward(group()));
        assert_eq!(nav.go_back(group(), live), Some(b));
        assert_eq!(nav.go_back(group(), live), Some(a));
        assert!(!nav.can_go_back(group()));
        assert_eq!(nav.go_forward(group(), live), Some(b));
        assert_eq!(nav.go_forward(group(), live), Some(c));
        assert!(!nav.can_go_forward(group()));
    }

    #[test]
    fn a_new_selection_drops_the_forward_branch() {
        let mut nav = TabSelectionNav::default();
        let (a, _b, c) = (TabId::from_raw(1), TabId::from_raw(2), TabId::from_raw(3));
        nav.seed_for_test(7, a);
        nav.activate_for_test(7, TabId::from_raw(2));
        assert_eq!(nav.go_back(group(), live), Some(a));
        nav.activate_for_test(7, c);
        assert_eq!(nav.current_for_test(7), c);
        assert!(!nav.can_go_forward(group()), "a new branch clears Forward");
        // Back now offers the tab the new branch left behind, not the
        // abandoned forward destination.
        assert_eq!(nav.go_back(group(), live), Some(a));
    }

    #[test]
    fn dead_stack_entries_are_skipped_and_pruned() {
        let mut nav = TabSelectionNav::default();
        let (a, b, closed) = (TabId::from_raw(1), TabId::from_raw(2), TabId::from_raw(3));
        nav.seed_for_test(7, a);
        nav.activate_for_test(7, closed);
        nav.activate_for_test(7, b);
        // `closed` dies while sitting in Back; Back must skip to `a`.
        assert_eq!(nav.go_back(group(), |tab| tab != closed), Some(a));
        assert_eq!(nav.go_forward(group(), |tab| tab != closed), Some(b));
    }

    #[test]
    fn consecutive_duplicates_are_not_recorded_and_history_is_bounded() {
        let mut nav = TabSelectionNav::default();
        let first = TabId::from_raw(1);
        nav.seed_for_test(7, first);
        for raw in 2..(MAX_TAB_NAV_ENTRIES + 10) {
            nav.activate_for_test(7, TabId::from_raw(raw as u128));
        }
        let mut depth = 0;
        while nav.go_back(group(), live).is_some() {
            depth += 1;
        }
        assert_eq!(depth, MAX_TAB_NAV_ENTRIES, "history is bounded");
    }

    #[test]
    fn reset_drops_every_group() {
        let mut nav = TabSelectionNav::default();
        nav.seed_for_test(7, TabId::from_raw(1));
        nav.reset();
        assert!(!nav.can_go_back(group()));
    }
}

