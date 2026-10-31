//! Workspace restore planning (RFC-0003 §22, W06).
//!
//! A decoded [`PersistedWorkspace`] becomes a pure [`RestorePlan`]: durable
//! identities are rehydrated, unrepresentable or missing entries are
//! dropped with an honest report, and note tabs are opened through the
//! normal open commands afterward (never deserialized as live resources).

use crate::codec::{PersistedBinding, PersistedKind, PersistedNode, PersistedWorkspace};
use crate::context::ContextBinding;
use crate::ids::{GroupId, SplitId, TabId};
use crate::layout::{Arrangement, LayoutNode, MAX_RATIO_PPM, MIN_RATIO_PPM};
use crate::view::{NoteLocator, ViewKind};
use std::collections::BTreeSet;

/// Why a restore plan could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreError {
    /// The persisted layout tree exceeds the supported depth.
    TreeTooDeep,
}

/// One restored tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoredTab {
    /// Rehydrated identity.
    pub id: TabId,
    /// The durable kind.
    pub kind: ViewKind,
}

/// The pure restore plan the host applies through reducer actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    /// The rehydrated layout tree (existing durable identities).
    pub tree: LayoutNode,
    /// All restored tabs.
    pub tabs: Vec<RestoredTab>,
    /// Group identities of every surviving group.
    pub groups: Vec<GroupId>,
    /// The focused tab.
    pub focus: TabId,
    /// The maximized group, if any.
    pub maximized: Option<GroupId>,
    /// The reopen history, most recent last.
    pub closed_history: Vec<NoteLocator>,
    /// Notes no longer in the catalog; they open as recoverable
    /// missing-note tabs (FR-17).
    pub missing_notes: Vec<NoteLocator>,
    /// Human-readable report of dropped or adjusted entries.
    pub report: Vec<String>,
}

/// Build a restore plan from a decoded workspace and the current catalog
/// paths (vault-relative strings). Unknown kinds, invalid identities, and
/// unresolvable locators are dropped with report entries, never guessed.
///
/// # Errors
/// Returns [`RestoreError::TreeTooDeep`] when the tree cannot be rehydrated.
#[allow(clippy::too_many_lines)]
pub fn plan(
    workspace: &PersistedWorkspace,
    catalog_paths: &BTreeSet<String>,
) -> Result<RestorePlan, RestoreError> {
    let mut report: Vec<String> = Vec::new();

    // Tabs: rehydrate identities; drop unrepresentable kinds with a report.
    let mut tabs: Vec<RestoredTab> = Vec::new();
    let mut id_by_name: Vec<(String, TabId)> = Vec::new();
    for tab in &workspace.tabs {
        let Ok(raw) = tab.id.parse::<u128>() else {
            report.push(format!(
                "tab {} has an invalid identity and was dropped",
                tab.id
            ));
            continue;
        };
        let id = TabId::from_raw(raw);
        let kind = match &tab.kind {
            PersistedKind::Empty => Some(ViewKind::Empty),
            PersistedKind::Notes => Some(ViewKind::Notes),
            PersistedKind::Note { locator } => NoteLocator::from_components(locator.clone())
                .ok()
                .map(|resolved| {
                    ViewKind::Note(crate::view::NoteView::new(
                        resolved,
                        crate::view::NoteLoad::Reserved,
                    ))
                }),
            PersistedKind::Preview { binding } => {
                restore_binding(binding, &mut report).map(ViewKind::Preview)
            }
            PersistedKind::Backlinks { binding } => {
                restore_binding(binding, &mut report).map(ViewKind::Backlinks)
            }
            PersistedKind::Other { name, .. } => {
                report.push(format!(
                    "tab {} holds unsupported kind {name:?} and was dropped",
                    tab.id
                ));
                None
            }
        };
        if let Some(kind) = kind {
            id_by_name.push((tab.id.clone(), id));
            tabs.push(RestoredTab { id, kind });
        } else {
            report.push(format!("tab {} was dropped", tab.id));
        }
    }
    let tab_by_name = |name: &str| {
        id_by_name
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, id)| *id)
    };

    // Groups: keep only groups with surviving members.
    let mut groups: Vec<(GroupId, Vec<TabId>, TabId)> = Vec::new();
    for group in &workspace.groups {
        let Ok(group_raw) = group.id.parse::<u128>() else {
            report.push(format!(
                "group {} has an invalid identity and was dropped",
                group.id
            ));
            continue;
        };
        let mut members: Vec<TabId> = Vec::new();
        for member in &group.tabs {
            if let Some(id) = tab_by_name(member) {
                members.push(id);
            }
        }
        if members.is_empty() {
            report.push(format!("group {} lost every tab and was dropped", group.id));
            continue;
        }
        let active = tab_by_name(&group.active)
            .filter(|active| members.contains(active))
            .unwrap_or(members[0]);
        groups.push((GroupId::from_raw(group_raw), members, active));
    }
    if groups.is_empty() {
        return Ok(RestorePlan {
            tree: LayoutNode::Group(GroupId::from_raw(0)),
            tabs: Vec::new(),
            groups: Vec::new(),
            focus: TabId::from_raw(0),
            maximized: None,
            closed_history: Vec::new(),
            missing_notes: Vec::new(),
            report,
        });
    }
    let surviving: BTreeSet<u128> = groups.iter().map(|(id, _, _)| id.raw()).collect();

    // Tree rehydration; dead leaves become markers and orphans re-attach.
    let tree = restore_node(&workspace.tree, &surviving, &mut report)?;
    let tree = attach_orphans(tree, &groups, &mut report);

    let focus = tab_by_name(&workspace.focus)
        .filter(|focus| tabs.iter().any(|tab| tab.id == *focus))
        .unwrap_or(groups[0].2);
    let maximized = workspace
        .maximized
        .as_ref()
        .and_then(|id| id.parse::<u128>().ok())
        .and_then(|raw| groups.iter().find(|(group, _, _)| group.raw() == raw))
        .map(|(group, _, _)| *group);

    let mut missing_notes = Vec::new();
    for tab in &tabs {
        if let ViewKind::Note(note) = &tab.kind {
            let path = note.locator().components().join("/");
            if !catalog_paths.contains(&path) {
                missing_notes.push(note.locator().clone());
            }
        }
    }
    let mut closed_history = Vec::new();
    for locator in &workspace.recently_closed {
        if let Ok(locator) = NoteLocator::from_components(locator.clone()) {
            closed_history.push(locator);
        }
    }
    Ok(RestorePlan {
        tree,
        tabs,
        groups: groups.into_iter().map(|(id, _, _)| id).collect(),
        focus,
        maximized,
        closed_history,
        missing_notes,
        report,
    })
}

fn restore_binding(binding: &PersistedBinding, report: &mut Vec<String>) -> Option<ContextBinding> {
    match binding {
        PersistedBinding::FollowDocument => Some(ContextBinding::FollowDocument),
        PersistedBinding::FollowExplorationOrDocument => {
            Some(ContextBinding::FollowExplorationOrDocument)
        }
        PersistedBinding::FollowSource { tab } => {
            // A closed source renders typed Unbound at runtime (D-04); the
            // identity is kept so an explicit rebind stays inspectable.
            tab.parse::<u128>()
                .ok()
                .map(|raw| ContextBinding::FollowSource {
                    tab: TabId::from_raw(raw),
                })
        }
        PersistedBinding::Pinned { path } => {
            if let Ok(locator) = NoteLocator::from_components(path.clone()) {
                Some(ContextBinding::Pinned(crate::context::EntityLocator::new(
                    locator,
                )))
            } else {
                report.push(
                    "a pinned locator was invalid; the tool was restored following the document"
                        .into(),
                );
                Some(ContextBinding::FollowDocument)
            }
        }
    }
}

fn restore_node(
    node: &PersistedNode,
    surviving: &BTreeSet<u128>,
    report: &mut Vec<String>,
) -> Result<LayoutNode, RestoreError> {
    match node {
        PersistedNode::Group { id } => {
            let raw = id.parse::<u128>().unwrap_or(0);
            if surviving.contains(&raw) {
                Ok(LayoutNode::Group(GroupId::from_raw(raw)))
            } else {
                report.push(format!("tree leaf group {id} was dropped with its group"));
                Ok(dead_leaf())
            }
        }
        PersistedNode::Split {
            id,
            arrangement,
            ratio_ppm,
            first,
            second,
        } => {
            if depth_of(node) > 16 {
                return Err(RestoreError::TreeTooDeep);
            }
            let first = restore_node(first, surviving, report)?;
            let second = restore_node(second, surviving, report)?;
            let first_dead = is_dead(&first);
            let second_dead = is_dead(&second);
            match (first_dead, second_dead) {
                // Both sides died with their groups; the split retires.
                (true, true) => Ok(dead_leaf()),
                // One side died: the surviving subtree takes its place and
                // the split identity retires with the dead side.
                (true, false) => Ok(second),
                (false, true) => Ok(first),
                (false, false) => {
                    let arrangement = match arrangement.as_str() {
                        "side_by_side" => Arrangement::SideBySide,
                        _ => Arrangement::Stacked,
                    };
                    Ok(LayoutNode::Split {
                        id: SplitId::from_raw(id.parse::<u128>().unwrap_or(0)),
                        arrangement,
                        desired_ratio_ppm: (*ratio_ppm).clamp(MIN_RATIO_PPM, MAX_RATIO_PPM),
                        first: Box::new(first),
                        second: Box::new(second),
                    })
                }
            }
        }
    }
}

/// Depth of a persisted node (a lone group has depth 1).
fn depth_of(node: &PersistedNode) -> usize {
    match node {
        PersistedNode::Group { .. } => 1,
        PersistedNode::Split { first, second, .. } => 1 + depth_of(first).max(depth_of(second)),
    }
}

/// Marker for a subtree whose groups all died.
fn dead_leaf() -> LayoutNode {
    LayoutNode::Group(GroupId::from_raw(0))
}

fn is_dead(node: &LayoutNode) -> bool {
    matches!(node, LayoutNode::Group(id) if id.raw() == 0)
}

fn attach_orphans(
    tree: LayoutNode,
    groups: &[(GroupId, Vec<TabId>, TabId)],
    report: &mut Vec<String>,
) -> LayoutNode {
    let mut tree = tree;
    for (id, _, _) in groups {
        if !tree_mentions(&tree, *id) {
            report.push(format!(
                "group {} was not present in the persisted tree and was re-attached beside it",
                id.raw()
            ));
            tree = LayoutNode::Split {
                id: SplitId::from_raw(synthetic_split(id)),
                arrangement: Arrangement::SideBySide,
                desired_ratio_ppm: 50_000,
                first: Box::new(tree),
                second: Box::new(LayoutNode::Group(*id)),
            };
        }
    }
    tree
}

fn synthetic_split(group: &GroupId) -> u128 {
    // Deterministic synthetic split ids live in a reserved high range so
    // they can never collide with generator-allocated split identities.
    0xD0C0_0000_0000_0000_0000_0000_0000_0000_u128 | group.raw()
}

fn tree_mentions(tree: &LayoutNode, group: GroupId) -> bool {
    match tree {
        LayoutNode::Group(id) => *id == group,
        LayoutNode::Split { first, second, .. } => {
            tree_mentions(first, group) || tree_mentions(second, group)
        }
    }
}
