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
    Editor {
        tab: flavite_workspace::TabId,
        incarnation: u64,
        revision: u64,
    },
    MenuItem(usize),
    MenuClose,
    GroupMenu(flavite_workspace::GroupId, crate::workspace::StripMenu),
    FocusGroup(flavite_workspace::GroupId),
    OverlayQuery,
    OverlayItem(usize),
    DismissOverlay,
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
    if let Some((group, menu)) = app.strip_menu {
        for (i, entry) in crate::workspace::tab_strip::menu_entries(app, group, menu)
            .iter()
            .enumerate()
        {
            if entry.action.is_some() {
                map.insert(menu_item_node(i), NodeTarget::MenuItem(i));
            }
        }
        map.insert(node_id("dismiss-menu", 0), NodeTarget::MenuClose);
        return map;
    }
    if app.palette_open || app.picker.open {
        map.insert(node_id("overlay-query", 0), NodeTarget::OverlayQuery);
        map.insert(node_id("overlay-dismiss", 0), NodeTarget::DismissOverlay);
        let count = if app.picker.open {
            app.picker_matches().len()
        } else {
            app.palette_snapshot().len()
        };
        for i in 0..count.min(50) {
            map.insert(
                node_id("overlay-item", i as u128),
                NodeTarget::OverlayItem(i),
            );
        }
        return map;
    }
    if collapsed(app) {
        for group in app.workspace.group_order() {
            map.insert(
                node_id("show-group", group.raw()),
                NodeTarget::FocusGroup(group),
            );
        }
    }
    map.insert(node_id("quick-switch", 0), NodeTarget::OpenPicker);
    map.insert(node_id("command-palette", 0), NodeTarget::OpenPalette);
    for group in app.workspace.group_order() {
        if let Some(g) = app.workspace.group(&group) {
            if !app.is_tab_visible(g.active()) {
                continue;
            }
            for tab in g.tabs() {
                map.insert(tab_node_id(*tab), NodeTarget::ActivateTab(*tab));
                map.insert(close_node_id(*tab), NodeTarget::CloseTab(*tab));
            }
            for (role, menu) in [
                ("tabs", crate::workspace::StripMenu::Tabs),
                ("new-view", crate::workspace::StripMenu::NewView),
                ("more", crate::workspace::StripMenu::Actions),
            ] {
                map.insert(
                    node_id(role, group.raw()),
                    NodeTarget::GroupMenu(group, menu),
                );
            }
            if let Some(view) = app.note_editor_view(g.active()) {
                let incarnation = app.notes.incarnation(g.active()).unwrap_or(0);
                map.insert(
                    editor_node(app, g.active()),
                    NodeTarget::Editor {
                        tab: g.active(),
                        incarnation,
                        revision: view.revision,
                    },
                );
            }
        }
    }
    if app.maximized_group().is_none()
        && flavite_workspace::fits(
            &app.workspace,
            flavite_workspace::Size {
                width: app.workspace_bounds.width,
                height: app.workspace_bounds.height,
            },
            1.0,
        )
    {
        for split in app.workspace.tree().splits() {
            map.insert(
                split_node_id(split),
                NodeTarget::ResizeSplit {
                    split,
                    direction: 0,
                },
            );
        }
    }
    for (path, id) in visible_catalog(app) {
        map.insert(id, NodeTarget::OpenNote(path));
    }
    map
}

fn collapsed(app: &App) -> bool {
    app.maximized_group().is_some()
        || !flavite_workspace::fits(
            &app.workspace,
            flavite_workspace::Size {
                width: app.workspace_bounds.width,
                height: app.workspace_bounds.height,
            },
            1.0,
        )
}

fn visible_catalog(app: &App) -> Vec<(std::path::PathBuf, NodeId)> {
    if !app.workspace.tabs().any(|(tab, v)| {
        app.is_tab_visible(*tab) && matches!(v.kind(), flavite_workspace::ViewKind::Notes)
    }) {
        return vec![];
    }
    app.catalog()
        .iter()
        .take(500)
        .map(|e| (e.path.clone(), note_node_id(&e.path)))
        .collect()
}

#[must_use]
pub fn editor_node(app: &App, tab: flavite_workspace::TabId) -> NodeId {
    node_id(
        "editor",
        tab.raw() ^ (u128::from(app.notes.incarnation(tab).unwrap_or(0)) << 64),
    )
}

#[must_use]
pub fn run_node(app: &App, tab: flavite_workspace::TabId, line: usize) -> NodeId {
    let revision = app.note_editor_view(tab).map_or(0, |v| v.revision);
    run_id(editor_node(app, tab), revision, line)
}

fn run_id(editor: NodeId, revision: u64, line: usize) -> NodeId {
    node_id(
        "text-run",
        u128::from(editor.0) ^ (u128::from(revision) << 64) ^ line as u128,
    )
}

pub fn menu_item_node(index: usize) -> NodeId {
    node_id("menu-item", index as u128)
}

/// Byte ranges of actual text lines. A trailing newline has an empty final run.
pub fn text_lines(text: &str) -> Vec<(usize, &str)> {
    let mut start = 0;
    let mut lines = Vec::new();
    for line in text.split_inclusive('\n') {
        lines.push((start, line));
        start += line.len();
    }
    if text.is_empty() || text.ends_with('\n') {
        lines.push((start, ""));
    }
    lines
}

pub fn text_position(
    app: &App,
    tab: flavite_workspace::TabId,
    text: &str,
    byte: usize,
) -> accesskit::TextPosition {
    let lines = text_lines(text);
    let index = lines
        .iter()
        .rposition(|(start, _)| *start <= byte)
        .unwrap_or(0);
    accesskit::TextPosition {
        node: run_node(app, tab, index),
        character_index: byte_to_char_index(lines[index].1, byte.saturating_sub(lines[index].0)),
    }
}

pub fn position_byte(
    app: &App,
    tab: flavite_workspace::TabId,
    text: &str,
    position: accesskit::TextPosition,
) -> Option<usize> {
    let revision = app.note_editor_view(tab)?.revision;
    let editor = editor_node(app, tab);
    for (index, (start, line)) in text_lines(text).into_iter().enumerate() {
        if position.node == run_id(editor, revision, index) {
            return char_index_to_byte(line, position.character_index).map(|byte| start + byte);
        }
    }
    None
}

fn semantic_bounds(rect: iced::Rectangle) -> Rect {
    Rect {
        x0: f64::from(rect.x),
        y0: f64::from(rect.y),
        x1: f64::from(rect.x + rect.width),
        y1: f64::from(rect.y + rect.height),
    }
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

    let mut focus = ROOT;
    if let Some((group, menu)) = app.strip_menu {
        let id = node_id("menu", group.raw());
        let mut modal = Node::new(Role::Menu);
        modal.set_label("Group menu");
        modal.set_modal();
        let mut children = Vec::new();
        for (i, entry) in crate::workspace::tab_strip::menu_entries(app, group, menu)
            .into_iter()
            .enumerate()
        {
            let child = menu_item_node(i);
            let mut item = Node::new(Role::MenuItem);
            item.set_label(entry.label);
            if entry.action.is_some() {
                item.add_action(Action::Click);
                item.add_action(Action::Focus);
            } else {
                item.set_disabled();
            }
            if i == app.menu_selected {
                focus = child;
            }
            nodes.push((child, item));
            children.push(child);
        }
        let dismiss = node_id("dismiss-menu", 0);
        let mut button = Node::new(Role::Button);
        button.set_label("Dismiss menu");
        button.add_action(Action::Click);
        nodes.push((dismiss, button));
        children.push(dismiss);
        modal.set_children(children);
        nodes.push((id, modal));
        window_children.push(id);
    } else if app.palette_open || app.picker.open {
        let id = node_id("dialog", 0);
        let mut dialog = Node::new(Role::Dialog);
        dialog.set_modal();
        dialog.set_label(if app.palette_open {
            "Command palette"
        } else {
            "Quick switch"
        });
        let query_id = node_id("overlay-query", 0);
        let mut query = Node::new(Role::TextInput);
        query.set_label("Search");
        query.set_value(if app.picker.open {
            app.picker.query.clone()
        } else {
            app.palette_query.clone()
        });
        query.add_action(Action::SetValue);
        nodes.push((query_id, query));
        let mut children = vec![query_id];
        let labels: Vec<String> = if app.picker.open {
            app.picker_matches().into_iter().map(|e| e.title).collect()
        } else {
            app.palette_snapshot()
                .into_iter()
                .map(|s| s.title.to_owned())
                .collect()
        };
        for (i, label) in labels.into_iter().take(50).enumerate() {
            let item_id = node_id("overlay-item", i as u128);
            let mut item = Node::new(Role::ListItem);
            item.set_label(label);
            item.add_action(Action::Click);
            nodes.push((item_id, item));
            children.push(item_id);
        }
        let dismiss = node_id("overlay-dismiss", 0);
        let mut button = Node::new(Role::Button);
        button.set_label("Dismiss");
        button.add_action(Action::Click);
        nodes.push((dismiss, button));
        children.push(dismiss);
        dialog.set_children(children);
        nodes.push((id, dialog));
        window_children.push(id);
        focus = query_id;
    } else {
        if collapsed(app) {
            for group in app.workspace.group_order() {
                let id = node_id("show-group", group.raw());
                let mut button = Node::new(Role::Button);
                let title = app
                    .workspace
                    .group(&group)
                    .and_then(|g| app.workspace.tab(&g.active()))
                    .map_or_else(|| "Group".into(), |v| v.title());
                button.set_label(format!("Show group: {title}"));
                button.add_action(Action::Click);
                nodes.push((id, button));
                window_children.push(id);
            }
        }
        for (id, label) in [
            (node_id("quick-switch", 0), "Quick switch"),
            (node_id("command-palette", 0), "Commands"),
        ] {
            let mut button = Node::new(Role::Button);
            button.set_label(label);
            button.add_action(Action::Click);
            nodes.push((id, button));
            window_children.push(id);
        }
        for group in app.workspace.group_order() {
            let Some(g) = app.workspace.group(&group) else {
                continue;
            };
            if !app.is_tab_visible(g.active()) {
                continue;
            }
            let group_id = group_node_id(group);
            let mut tablist = Node::new(Role::TabList);
            tablist.set_label("Tab group");
            if let Some(rect) = app.group_bounds.get(&group) {
                tablist.set_bounds(semantic_bounds(*rect));
            }
            let mut children = Vec::new();
            for tab in g.tabs() {
                let Some(session) = app.workspace.tab(tab) else {
                    continue;
                };
                let id = tab_node_id(*tab);
                let mut node = Node::new(Role::Tab);
                node.set_label(session.title());
                node.set_selected(*tab == g.active());
                node.add_action(Action::Click);
                node.add_action(Action::Focus);
                nodes.push((id, node));
                children.push(id);
                let close = close_node_id(*tab);
                let mut node = Node::new(Role::Button);
                node.set_label(format!("Close {}", session.title()));
                node.add_action(Action::Click);
                nodes.push((close, node));
                children.push(close);
            }
            for (role, label) in [
                ("tabs", "All tabs"),
                ("new-view", "New view"),
                ("more", "Group actions"),
            ] {
                let id = node_id(role, group.raw());
                let mut button = Node::new(Role::Button);
                button.set_label(label);
                button.add_action(Action::Click);
                nodes.push((id, button));
                children.push(id);
            }
            tablist.set_children(children);
            nodes.push((group_id, tablist));
            window_children.push(group_id);
            if let Some(view) = app.note_editor_view(g.active()) {
                let tab = g.active();
                let id = editor_node(app, tab);
                let mut editor = Node::new(Role::MultilineTextInput);
                editor.set_label(view.path.display().to_string());
                editor.set_value(view.text.to_string());
                editor.add_action(Action::Focus);
                editor.add_action(Action::SetTextSelection);
                if let Some(rect) = app
                    .editor_bounds
                    .get(&tab)
                    .or_else(|| app.group_bounds.get(&group))
                {
                    editor.set_bounds(semantic_bounds(*rect));
                }
                let mut runs = Vec::new();
                for (i, (_, line)) in text_lines(&view.text).into_iter().enumerate() {
                    let run = run_id(id, view.revision, i);
                    let mut node = Node::new(Role::TextRun);
                    node.set_value(line);
                    node.set_character_lengths(
                        flavite_editor::viewport::accessible_characters(line)
                            .into_iter()
                            .map(|range| u8::try_from(range.len()).unwrap_or(4))
                            .collect::<Vec<_>>(),
                    );
                    nodes.push((run, node));
                    runs.push(run);
                }
                editor.set_children(runs);
                if let Some(snapshot) = app.editor_interaction_snapshot(tab) {
                    editor.set_text_selection(accesskit::TextSelection {
                        anchor: text_position(app, tab, &view.text, snapshot.selection.0),
                        focus: text_position(app, tab, &view.text, snapshot.selection.1),
                    });
                    if snapshot.focused {
                        focus = id;
                    }
                }
                nodes.push((id, editor));
                window_children.push(id);
            }
        }
        for split in app.workspace.tree().splits() {
            let id = split_node_id(split);
            if !app.a11y_nodes.contains_key(&id) {
                continue;
            }
            let mut node = Node::new(Role::Splitter);
            node.set_label("Split divider");
            node.add_action(Action::Increment);
            node.add_action(Action::Decrement);
            node.set_numeric_value(f64::from(split_ratio(app, split)) / 1_000_000.0);
            node.set_min_numeric_value(0.05);
            node.set_max_numeric_value(0.95);
            nodes.push((id, node));
            window_children.push(id);
        }
        for (path, id) in visible_catalog(app) {
            let mut node = Node::new(Role::ListItem);
            node.set_label(path.display().to_string());
            node.add_action(Action::Click);
            nodes.push((id, node));
            window_children.push(id);
        }
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

/// Convert an already validated UTF-8 byte boundary to AccessKit's
/// character index for a text run.
#[must_use]
pub fn byte_to_char_index(text: &str, byte: usize) -> usize {
    flavite_editor::viewport::accessible_characters(text)
        .iter()
        .take_while(|r| r.end <= byte)
        .count()
}

/// Convert a native selectable-character index to a UTF-8 byte boundary.
#[must_use]
pub fn char_index_to_byte(text: &str, character_index: usize) -> Option<usize> {
    let characters = flavite_editor::viewport::accessible_characters(text);
    if character_index == characters.len() {
        Some(text.len())
    } else {
        characters.get(character_index).map(|r| r.start)
    }
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
    fn native_characters_preserve_combining_emoji_and_crlf_boundaries() {
        let text = "e\u{301}🧑🏽‍💻\r\n";
        let characters = flavite_editor::viewport::accessible_characters(text);
        assert_eq!(characters.len(), 3);
        for (i, range) in characters.iter().enumerate() {
            assert_eq!(char_index_to_byte(text, i), Some(range.start));
            assert_eq!(byte_to_char_index(text, range.start), i);
        }
        assert_eq!(char_index_to_byte(text, 3), Some(text.len()));
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
