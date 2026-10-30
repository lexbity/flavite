//! Post-transaction validation of every workspace invariant (§7.3).

use crate::layout::{LayoutNode, MAX_DEPTH, MAX_GROUPS, MAX_RATIO_PPM, MAX_TABS, MIN_RATIO_PPM};
use crate::reducer::WorkspaceState;
use std::collections::BTreeSet;

/// Validate the full invariant set. Returns a human-readable description of
/// the first violation; `Ok(())` means the state is committable.
///
/// # Errors
/// Returns the first violated invariant as a message.
///
/// Invariants (RFC-0003 §7.3):
/// 1. exactly one root tree; every group appears once; split ids unique;
/// 2. every tab belongs to exactly one group; the tab map is exactly the
///    union of group members;
/// 3. every group has at least one tab and a valid active tab;
/// 4. focus and maximize reference existing entities; focus is an active
///    tab;
/// 7. bounds from NFR-01 hold.
pub fn validate(state: &WorkspaceState) -> Result<(), String> {
    let tree = state.tree();
    // 1. Tree shape.
    let groups = tree.groups();
    if groups.is_empty() {
        return Err("layout tree has no groups".into());
    }
    let mut seen = BTreeSet::new();
    for group in &groups {
        if !seen.insert(*group) {
            return Err(format!("group {group} appears twice in the tree"));
        }
    }
    let splits = tree.splits();
    let mut seen_splits = BTreeSet::new();
    for split in &splits {
        if !seen_splits.insert(*split) {
            return Err(format!("split {split} appears twice in the tree"));
        }
    }
    if tree.depth() > MAX_DEPTH {
        return Err(format!("layout depth {} exceeds {MAX_DEPTH}", tree.depth()));
    }
    if groups.len() > MAX_GROUPS {
        return Err(format!("{} groups exceed {MAX_GROUPS}", groups.len()));
    }
    // Ratios stay in the persisted range.
    check_ratios(tree)?;
    // The group map is exactly the tree's group set.
    for (id, _) in state.groups() {
        if !seen.contains(id) {
            return Err(format!("group {id} is mapped but not in the tree"));
        }
    }
    // 2/3. Tab membership and active tabs.
    let mut member_of: BTreeSet<_> = BTreeSet::new();
    for (id, group) in state.groups() {
        if group.tabs().is_empty() {
            return Err(format!("group {id} has no tabs"));
        }
        if !group.tabs().contains(&group.active()) {
            return Err(format!("group {id} has a stale active tab"));
        }
        for tab in group.tabs() {
            if !member_of.insert(*tab) {
                return Err(format!("tab {tab} belongs to more than one group"));
            }
            let Some(session) = state.tab(tab) else {
                return Err(format!("tab {tab} is grouped but unmapped"));
            };
            if session.tab() != *tab {
                return Err(format!("tab {tab} maps a session for {}", session.tab()));
            }
        }
    }
    for (tab, _) in state.tabs() {
        if !member_of.contains(tab) {
            return Err(format!("tab {tab} is mapped but not in any group"));
        }
    }
    if state.tabs().count() > MAX_TABS {
        return Err("too many tabs".into());
    }
    // 4. Focus and maximize reference real, displayed tabs.
    let focus = state.focused_tab();
    let focus_displayed = state.groups().any(|(_, group)| group.active() == focus);
    if !focus_displayed {
        return Err(format!("focus {focus} is not an active tab of any group"));
    }
    if let Some(maximized) = state.maximized_group()
        && !seen.contains(&maximized)
    {
        return Err(format!("maximized group {maximized} is not in the tree"));
    }
    Ok(())
}

fn check_ratios(node: &LayoutNode) -> Result<(), String> {
    match node {
        LayoutNode::Group(_) => Ok(()),
        LayoutNode::Split {
            desired_ratio_ppm,
            first,
            second,
            ..
        } => {
            if !(MIN_RATIO_PPM..=MAX_RATIO_PPM).contains(desired_ratio_ppm) {
                return Err(format!("split ratio {desired_ratio_ppm} ppm out of range"));
            }
            check_ratios(first)?;
            check_ratios(second)
        }
    }
}
