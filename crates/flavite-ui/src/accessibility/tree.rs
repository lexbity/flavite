//! Semantic projection for the running Flavite workspace (RFC-0003 §13.3).
//!
//! The tree exposes tablists and tabs per group, splitters with value and
//! increment/decrement actions, and the active note's source text. Accessible
//! ids derive from workspace identities (TabId/GroupId/SplitId), never from
//! widget positions, so they stay stable through docking.

use crate::app::App;
use accesskit::{Action, Node, NodeId, Rect, Role, TreeId, TreeInfo, TreeUpdate};
use std::collections::BTreeMap;

/// What a native action on a semantic node routes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeTarget {
    /// Activate (and focus) a tab.
    ActivateTab(flavite_workspace::TabId),
    /// Close a tab.
    CloseTab(flavite_workspace::TabId),
    /// Increment or decrement a splitter by one step.
    ResizeSplit {
        /// The workspace split.
        split: flavite_workspace::SplitId,
        /// +1 for increment, -1 for decrement.
        direction: i8,
    },
    /// Open a note from the catalog list.
    OpenNote(std::path::PathBuf),
    /// Open the note picker.
    OpenPicker,
    /// Open the command palette.
    OpenPalette,
}

pub(crate) const RESIZE_STEP_PPM: u32 = 50_000;

/// Hash workspace identities into stable semantic node ids. `DefaultHasher`
/// starts from fixed keys, so ids are deterministic across runs.
fn node_id(kind: &str, raw: u128) -> NodeId {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    kind.hash(&mut hasher);
    raw.hash(&mut hasher);
    NodeId(hasher.finish())
}

fn tab_node_id(tab: flavite_workspace::TabId) -> NodeId {
    node_id("tab", tab.raw())
}

/// The semantic node id for a tab, exposed for integration tests.
#[must_use]
pub fn node_for_tab(tab: flavite_workspace::TabId) -> NodeId {
    tab_node_id(tab)
}

fn close_node_id(tab: flavite_workspace::TabId) -> NodeId {
    node_id("tab-close", tab.raw())
}

fn group_node_id(group: flavite_workspace::GroupId) -> NodeId {
    node_id("group", group.raw())
}

fn split_node_id(split: flavite_workspace::SplitId) -> NodeId {
    node_id("split", split.raw())
}

fn note_node_id(path: &std::path::Path) -> NodeId {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    NodeId(hasher.finish())
}

/// Build the routing map consumed by [`crate::accessibility::route_action`].
#[must_use]
pub fn build_node_map(app: &App) -> BTreeMap<NodeId, NodeTarget> {
    let mut map = BTreeMap::new();
    for group in app.workspace.group_order() {
        map.insert(group_node_id(group), NodeTarget::OpenPicker);
        if let Some(group_state) = app.workspace.group(&group) {
            for tab in group_state.tabs() {
                map.insert(tab_node_id(*tab), NodeTarget::ActivateTab(*tab));
                map.insert(close_node_id(*tab), NodeTarget::CloseTab(*tab));
            }
        }
    }
    for split in app.workspace.tree().splits() {
        map.insert(
            split_node_id(split),
            NodeTarget::ResizeSplit {
                split,
                direction: 0,
            },
        );
    }
    for entry in app.catalog() {
        map.insert(
            note_node_id(&entry.path),
            NodeTarget::OpenNote(entry.path.clone()),
        );
    }
    map
}

/// Build the complete semantic tree for the current workspace state.
#[must_use]
pub fn shell_tree(app: &App) -> TreeUpdate {
    let window_size = app.window_size();
    let mut nodes: Vec<(NodeId, Node)> = Vec::new();
    let mut window_children: Vec<NodeId> = Vec::new();

    let mut root = Node::new(Role::Window);
    root.set_label("Flavite workspace");
    root.set_bounds(Rect {
        x0: 0.0,
        y0: 0.0,
        x1: f64::from(window_size.width),
        y1: f64::from(window_size.height),
    });

    let focused_tab = app.workspace.focused_tab();
    let groups = app.workspace.group_order();
    for group in &groups {
        let Some(group_state) = app.workspace.group(group) else {
            continue;
        };
        let group_id = group_node_id(*group);
        let mut tablist = Node::new(Role::TabList);
        tablist.set_label(format!("Group {}", group_state.tabs().len()));
        let mut tab_children = Vec::new();
        for tab in group_state.tabs() {
            let Some(session) = app.workspace.tab(tab) else {
                continue;
            };
            let is_active = *tab == group_state.active();
            let tab_id = tab_node_id(*tab);
            let mut tab_node = Node::new(Role::Tab);
            tab_node.set_label(session.title());
            tab_node.add_action(Action::Click);
            if is_active {
                tab_node.set_selected(true);
            }
            nodes.push((tab_id, tab_node));
            tab_children.push(tab_id);
            let close_id = close_node_id(*tab);
            let mut close_node = Node::new(Role::Button);
            close_node.set_label(format!("Close {}", session.title()));
            close_node.add_action(Action::Click);
            nodes.push((close_id, close_node));
            tab_children.push(close_id);
        }
        tablist.set_children(tab_children);
        nodes.push((group_id, tablist));
        window_children.push(group_id);
    }

    for split in app.workspace.tree().splits() {
        let id = split_node_id(split);
        let mut splitter = Node::new(Role::Splitter);
        splitter.set_label("Split divider");
        splitter.add_action(Action::Increment);
        splitter.add_action(Action::Decrement);
        let ratio = f64::from(split_ratio(app, split)) / 1_000_000.0;
        splitter.set_value(format!("{}%", (ratio * 100.0).round() as u64));
        nodes.push((id, splitter));
        window_children.push(id);
    }

    // The focused note tab's source text is exposed for reading.
    let mut focus = NodeId(0);
    let mut text_node = Node::new(Role::Document);
    if let Some(session) = app.workspace.tab(&focused_tab)
        && let flavite_workspace::ViewKind::Note(note) = session.kind()
        && note.load() == flavite_workspace::NoteLoad::Ready
    {
        let path = note.locator().components().join("/");
        text_node.set_label(path.clone());
        if let Some(contents) = app.note_state_text(focused_tab) {
            text_node.set_value(contents);
        } else {
            text_node.set_value(String::new());
        }
        nodes.push((note_text_node_id(), text_node));
        window_children.push(note_text_node_id());
        focus = note_text_node_id();
    }

    // Catalog entries are exposed as list items with open actions.
    let mut list_children = Vec::new();
    let catalog_list_id = node_id("catalog", 1);
    for entry in app.catalog().iter().take(500) {
        let id = note_node_id(&entry.path);
        let mut item = Node::new(Role::ListItem);
        item.set_label(format!("{} ({})", entry.title, entry.path.display()));
        item.add_action(Action::Click);
        nodes.push((id, item));
        list_children.push(id);
    }
    if !list_children.is_empty() {
        let mut list = Node::new(Role::List);
        list.set_label("Vault notes");
        list.set_children(list_children);
        nodes.push((catalog_list_id, list));
        window_children.push(catalog_list_id);
    }

    root.set_children(window_children);
    nodes.insert(0, (ROOT, root));

    TreeUpdate {
        nodes,
        tree: Some(TreeInfo::new(ROOT)),
        tree_id: TreeId::ROOT,
        focus: if focus == NodeId(0) { ROOT } else { focus },
    }
}

const ROOT: NodeId = NodeId(0);

const fn note_text_node_id() -> NodeId {
    NodeId(1)
}

fn split_ratio(app: &App, split: flavite_workspace::SplitId) -> u32 {
    find_split_ratio(app.workspace.tree(), split).unwrap_or(500_000)
}

fn find_split_ratio(
    node: &flavite_workspace::LayoutNode,
    split: flavite_workspace::SplitId,
) -> Option<u32> {
    match node {
        flavite_workspace::LayoutNode::Group(_) => None,
        flavite_workspace::LayoutNode::Split {
            id,
            desired_ratio_ppm,
            first,
            second,
            ..
        } => {
            if *id == split {
                Some(*desired_ratio_ppm)
            } else {
                find_split_ratio(first, split).or_else(|| find_split_ratio(second, split))
            }
        }
    }
}

/// The semantic node id of the exposed note text (used by tests).
#[must_use]
pub const fn exposed_text_node() -> NodeId {
    note_text_node_id()
}

/// Convert an already validated UTF-8 byte boundary to AccessKit's
/// character index for a text run.
#[must_use]
pub fn byte_to_char_index(text: &str, byte: usize) -> usize {
    text.get(..byte).map_or(0, |prefix| prefix.chars().count())
}

/// Convert an AccessKit text-run character index to a UTF-8 byte boundary.
#[must_use]
pub fn char_index_to_byte(text: &str, character_index: usize) -> Option<usize> {
    if character_index == text.chars().count() {
        return Some(text.len());
    }
    text.char_indices()
        .nth(character_index)
        .map(|(byte, _)| byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app_with_note() -> App {
        use flavite_api::{Flavite, VaultConfig};
        use flavite_buffer::manager::BufferManager;
        use flavite_fs::{Fs, MemFs};
        use flavite_index::IndexActor;
        use flavite_vault::actor::VaultActorBuilder;
        use flavite_vault::channels::ChannelSet;
        use flavite_vault::session::Session;
        use std::path::PathBuf;
        let fs = MemFs::new();
        let root = PathBuf::from("/test/a11y-tree-vault");
        fs.create_dir_all(&root).unwrap();
        fs.write_new(&root.join("a.md"), b"# A\n").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("tree-a11y.sqlite3");
        drop(flavite_index::open_or_init(&db).unwrap());
        let identity =
            flavite_api::VaultIdentity::new(uuid::Uuid::now_v7(), root.clone(), vec![4, 2, 2]);
        let actor = VaultActorBuilder::new()
            .with_fs(Box::new(fs) as Box<dyn Fs + Send + Sync>)
            .with_identity(identity.clone())
            .with_index(IndexActor::new(&db, 1).unwrap())
            .build()
            .unwrap()
            .start()
            .unwrap();
        let channels = ChannelSet::new();
        let session = Session::new(
            identity.clone(),
            actor,
            BufferManager::new(),
            channels.pulse,
            channels.notice,
            VaultConfig::default(),
        );
        let flavite = Flavite::open(identity, session, VaultConfig::default()).unwrap();
        let mut app = App::from_flavite(flavite);
        let group = app.workspace.first_group().unwrap();
        let locator = flavite_workspace::NoteLocator::from_components(["a.md"]).unwrap();
        match flavite_workspace::reduce(
            &app.workspace,
            flavite_workspace::LayoutAction::OpenNote {
                expected_revision: app.workspace.revision(),
                locator,
                group,
            },
        ) {
            flavite_workspace::Outcome::Committed(commit) => {
                app.workspace = commit.state;
                app.adapter.reconcile(&app.workspace);
                app.refresh_semantics();
            }
            flavite_workspace::Outcome::Rejected(error) => panic!("{error}"),
        }
        app
    }

    #[test]
    fn tree_exposes_tablists_tabs_and_the_routing_map_resolves_them() {
        let app = app_with_note();
        let update = shell_tree(&app);
        assert_eq!(update.tree.expect("tree").root, ROOT);
        let roles: Vec<Role> = update.nodes.iter().map(|(_, n)| n.role()).collect();
        assert!(
            roles.contains(&Role::TabList),
            "groups expose tablists: {roles:?}"
        );
        assert!(roles.contains(&Role::Tab));
        let tab = app.workspace.focused_tab();
        assert_eq!(
            app.a11y_nodes.get(&tab_node_id(tab)),
            Some(&NodeTarget::ActivateTab(tab))
        );
    }

    #[test]
    fn accesskit_character_positions_round_trip_unicode_boundaries() {
        let text = "aé🦀z";
        for (byte, _) in text
            .char_indices()
            .chain(std::iter::once((text.len(), '\0')))
        {
            let character = byte_to_char_index(text, byte);
            assert_eq!(char_index_to_byte(text, character), Some(byte));
        }
        assert_eq!(char_index_to_byte(text, text.chars().count() + 1), None);
    }
}
