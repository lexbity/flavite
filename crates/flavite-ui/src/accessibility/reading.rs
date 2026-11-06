//! Semantic reading/structure tree from the same mounted canonical read model.
use super::{NodeTarget, tree::node_id};
use crate::{
    app::{App, Message},
    host::reading,
};
use accesskit::{Action, Node, NodeId, Role};
use flavite_api::{ReadKind, ReadNode, ReadSnapshot};
use flavite_workspace::{ContextBinding, TabId, ViewKind};
use std::{collections::BTreeMap, sync::Arc};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadingAction {
    OpenSource,
    InsertLink,
    ToggleSource,
    Link(usize),
    Task { start: usize, checked: bool },
    Outline(usize),
    PropertyInput(String),
    PropertyApply(String),
}
#[derive(Default)]
pub struct Projection {
    pub nodes: Vec<(NodeId, Node)>,
    pub roots: Vec<NodeId>,
    pub actions: BTreeMap<NodeId, NodeTarget>,
}
fn read_id(tab: TabId, doc: &ReadSnapshot, role: &str, at: usize) -> NodeId {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    doc.path.hash(&mut h);
    doc.content_hash.hash(&mut h);
    node_id(&format!("read-{role}-{}-{at}", h.finish()), tab.raw())
}
impl Projection {
    fn action(&mut self, id: NodeId, tab: TabId, doc: &Arc<ReadSnapshot>, action: ReadingAction) {
        self.actions.insert(
            id,
            NodeTarget::Reading {
                tab,
                document: doc.clone(),
                action,
            },
        );
    }
    fn push(&mut self, id: NodeId, node: Node) {
        self.nodes.push((id, node));
    }
}
pub fn project(app: &App) -> Projection {
    let mut out = Projection::default();
    if app.palette_open || app.picker.open || app.strip_menu.is_some() {
        return out;
    }
    for (tab, session) in app
        .workspace
        .tabs()
        .filter(|(t, _)| app.is_tab_visible(**t))
    {
        if let Some(find) = app.note_search.finds.get(tab) {
            let id = node_id("find-query", tab.raw());
            let mut node = Node::new(Role::MultilineTextInput);
            node.set_label("Find in current note, including unsaved changes");
            node.set_value(find.query.clone());
            node.add_action(Action::Focus);
            node.add_action(Action::SetValue);
            out.actions.insert(id, NodeTarget::FindQuery(*tab));
            out.push(id, node);
            out.roots.push(id);
            for (command, label) in [
                ("note.find_regex", "Toggle regex matching"),
                ("note.find_case", "Toggle case sensitive matching"),
                ("note.find_word", "Toggle whole word matching"),
                ("note.find_selection", "Toggle selected source scope"),
            ] {
                let id = node_id(command, tab.raw());
                let mut node = Node::new(Role::Button);
                node.set_label(label);
                let active = match command {
                    "note.find_regex" => find.mode == "regex",
                    "note.find_case" => find.case_sensitive,
                    "note.find_word" => find.whole_word,
                    "note.find_selection" => find.use_selection_scope,
                    _ => false,
                };
                node.set_toggled(accesskit::Toggled::from(active));
                node.add_action(Action::Click);
                out.actions
                    .insert(id, NodeTarget::NoteSearchCommand(*tab, command));
                out.push(id, node);
                out.roots.push(id);
            }
            let status_id = node_id("find-status", tab.raw());
            let mut status = Node::new(Role::Label);
            status.set_label(if let Some(error) = &find.last_error {
                format!("Find error: {error}")
            } else if find.pending.is_some() {
                "Find results: searching".into()
            } else {
                format!(
                    "Find results: {} matches",
                    crate::host::note_search::matches(app, *tab)
                        .map_or(0, |page| page.total_matches)
                )
            });
            out.push(status_id, status);
            out.roots.push(status_id);
            for (role, label, target) in [
                (
                    "find-prev",
                    "Previous match",
                    NodeTarget::FindNext {
                        tab: *tab,
                        forward: false,
                    },
                ),
                (
                    "find-next",
                    "Next match",
                    NodeTarget::FindNext {
                        tab: *tab,
                        forward: true,
                    },
                ),
                ("find-close", "Close find", NodeTarget::FindClose(*tab)),
            ] {
                let id = node_id(role, tab.raw());
                let mut node = Node::new(Role::Button);
                node.set_label(label);
                node.add_action(Action::Click);
                out.actions.insert(id, target);
                out.push(id, node);
                out.roots.push(id);
            }
        }
        if let Some(replace) = app.note_search.replaces.get(tab) {
            // RFC-0006 S01: replacement controls ride the same catalog, so
            // native input and AT act on exactly the mounted rows.
            let id = node_id("replace-input", tab.raw());
            let mut node = Node::new(Role::MultilineTextInput);
            node.set_label("Replace with");
            node.set_value(replace.replacement.clone());
            node.add_action(Action::Focus);
            node.add_action(Action::SetValue);
            out.actions.insert(id, NodeTarget::ReplaceInput(*tab));
            out.push(id, node);
            out.roots.push(id);
            for (role, label, target) in [
                (
                    "replace-next",
                    "Replace next match",
                    NodeTarget::ReplaceNext(*tab),
                ),
                (
                    "replace-all",
                    "Replace all matches",
                    NodeTarget::ReplaceAll(*tab),
                ),
            ] {
                let id = node_id(role, tab.raw());
                let mut node = Node::new(Role::Button);
                node.set_label(label);
                node.add_action(Action::Click);
                out.actions.insert(id, target);
                out.push(id, node);
                out.roots.push(id);
            }
        }
        if let ViewKind::Note(note) = session.kind() {
            let id = node_id("note-mode", tab.raw());
            let mut node = Node::new(Role::Button);
            node.set_label(format!(
                "Switch to {}",
                if note.mode() == flavite_workspace::NoteMode::Edit {
                    "Read"
                } else {
                    "Edit"
                }
            ));
            if !app.has_composition() {
                node.add_action(Action::Click);
                out.actions.insert(id, NodeTarget::NoteMode(*tab));
            }
            out.push(id, node);
            out.roots.push(id);
        }
        if app.workspace.binding_of(tab).is_some() {
            let id = node_id("binding-status", tab.raw());
            let mut node = Node::new(Role::Label);
            node.set_value(format!(
                "{} · {}",
                crate::views::context_header::source_label(app, *tab),
                crate::views::context_header::status_label(app, *tab)
            ));
            out.push(id, node);
            out.roots.push(id);
            let mut bindings = vec![
                ("Follow document".to_owned(), ContextBinding::FollowDocument),
                (
                    "Follow selection or document".into(),
                    ContextBinding::FollowExplorationOrDocument,
                ),
            ];
            for (source, s) in app
                .workspace
                .tabs()
                .filter(|(_, s)| flavite_workspace::is_source_kind(s.kind()))
            {
                bindings.push((
                    format!("Follow source: {}", s.title()),
                    ContextBinding::FollowSource { tab: *source },
                ));
            }
            if let Some(source) = app.contexts.observed(tab) {
                bindings.push((
                    "Pin context to this note".into(),
                    ContextBinding::Pinned(flavite_workspace::EntityLocator::new(
                        source.locator.clone(),
                    )),
                ));
            }
            for (label, binding) in bindings {
                let id = node_id(&format!("binding-{binding:?}"), tab.raw());
                let mut node = Node::new(Role::Button);
                node.set_label(label);
                node.add_action(Action::Click);
                out.actions
                    .insert(id, NodeTarget::ContextBinding { tab: *tab, binding });
                out.push(id, node);
                out.roots.push(id);
            }
        }
        let Some(doc) = reading::document(app, *tab) else {
            continue;
        };
        let root = read_id(*tab, doc, "document", 0);
        let mut document = Node::new(Role::Document);
        document.set_label(format!(
            "{} · {}",
            session.title(),
            if reading::unsaved(app, doc) {
                "unsaved"
            } else {
                "saved"
            }
        ));
        document.set_read_only();
        let mut children = Vec::new();
        if matches!(
            session.kind(),
            ViewKind::Preview(_) | ViewKind::Properties(_)
        ) {
            let mut controls = vec![("Open source note", ReadingAction::OpenSource, true)];
            if matches!(session.kind(), ViewKind::Preview(_)) {
                controls.push((
                    "Insert link to source note",
                    ReadingAction::InsertLink,
                    app.valid_writing_target().is_some(),
                ));
                controls.push((
                    if app.reading.source_previews.contains(tab) {
                        "Rendered preview"
                    } else {
                        "Source preview"
                    },
                    ReadingAction::ToggleSource,
                    true,
                ));
            }
            for (label, action, available) in controls {
                let id = read_id(*tab, doc, label, 0);
                let mut node = Node::new(Role::Button);
                node.set_label(label);
                if available && reading::current(app, doc) {
                    node.add_action(Action::Click);
                    out.action(id, *tab, doc, action);
                } else {
                    node.set_disabled();
                }
                out.push(id, node);
                children.push(id);
            }
        }
        match session.kind() {
            ViewKind::Preview(_) if app.reading.source_previews.contains(tab) => {
                let id = read_id(*tab, doc, "source-preview", 0);
                let mut node = Node::new(Role::Label);
                node.set_value(doc.text.to_string());
                out.push(id, node);
                children.push(id);
            }
            ViewKind::Outline(_) => {
                for h in &doc.document.outline {
                    let id = read_id(*tab, doc, "outline", h.byte);
                    let mut node = Node::new(Role::Button);
                    node.set_label(h.title.clone());
                    if reading::current(app, doc) {
                        node.add_action(Action::Click);
                        out.action(id, *tab, doc, ReadingAction::Outline(h.byte));
                    }
                    out.push(id, node);
                    children.push(id);
                }
            }
            ViewKind::Properties(_) => {
                if let Ok(props) = flavite_api::inspect_properties(&doc.text) {
                    for p in props {
                        let id = read_id(*tab, doc, &format!("property-{}", p.key), 0);
                        let mut node = Node::new(Role::TextInput);
                        node.set_label(p.key.clone());
                        let value = app
                            .reading
                            .property_drafts
                            .get(&(*tab, p.key.clone()))
                            .cloned()
                            .unwrap_or_else(|| match p.value {
                                flavite_api::ScalarValue::Text(v)
                                | flavite_api::ScalarValue::Number(v) => v,
                                flavite_api::ScalarValue::Bool(v) => v.to_string(),
                                flavite_api::ScalarValue::Null => "null".into(),
                            });
                        node.set_value(value);
                        let enabled = reading::can_edit(app, doc);
                        if enabled {
                            node.add_action(Action::SetValue);
                            out.action(id, *tab, doc, ReadingAction::PropertyInput(p.key.clone()));
                        } else {
                            node.set_read_only();
                        }
                        out.push(id, node);
                        children.push(id);
                        let apply = read_id(*tab, doc, &format!("apply-{}", p.key), 0);
                        let mut node = Node::new(Role::Button);
                        node.set_label(format!("Apply {}", p.key));
                        if enabled {
                            node.add_action(Action::Click);
                            out.action(apply, *tab, doc, ReadingAction::PropertyApply(p.key));
                        } else {
                            node.set_disabled();
                        }
                        out.push(apply, node);
                        children.push(apply);
                    }
                } else {
                    let id = read_id(*tab, doc, "yaml-source", 0);
                    let mut node = Node::new(Role::Label);
                    node.set_value(
                        doc.document
                            .properties
                            .as_ref()
                            .map_or("No frontmatter".into(), |fm| fm.yaml_text.clone()),
                    );
                    out.push(id, node);
                    children.push(id);
                }
            }
            ViewKind::Note(n) if n.mode() == flavite_workspace::NoteMode::Edit => continue,
            _ => {
                for n in &doc.document.nodes {
                    children.push(project_node(app, &mut out, *tab, doc, n, 0));
                }
            }
        }
        document.set_children(children);
        out.push(root, document);
        out.roots.push(root);
    }
    out
}
fn project_node(
    app: &App,
    out: &mut Projection,
    tab: TabId,
    doc: &Arc<ReadSnapshot>,
    n: &ReadNode,
    instance: u64,
) -> NodeId {
    let role = match n.kind {
        ReadKind::Heading(_) => Role::Heading,
        ReadKind::Paragraph => Role::Paragraph,
        ReadKind::List(_) => Role::List,
        ReadKind::Item => Role::ListItem,
        ReadKind::Table => Role::Table,
        ReadKind::TableHead | ReadKind::TableRow => Role::Row,
        ReadKind::TableCell => Role::Cell,
        ReadKind::Task(_) => Role::CheckBox,
        ReadKind::Link { .. } => Role::Link,
        _ => Role::Label,
    };
    let id = read_id(
        tab,
        doc,
        &format!("{role:?}-{:?}-{instance}", std::mem::discriminant(&n.kind)),
        n.range.start,
    );
    let mut node = Node::new(role);
    if let ReadKind::Heading(level) = n.kind {
        node.set_level(usize::from(level));
    }
    let text = flavite_api::plain_text(n);
    if !text.is_empty() {
        node.set_label(text);
    }
    let mut children = Vec::new();
    for child in &n.children {
        children.push(project_node(app, out, tab, doc, child, instance));
    }
    match n.kind {
        ReadKind::Task(checked) => {
            node.set_toggled(if checked {
                accesskit::Toggled::True
            } else {
                accesskit::Toggled::False
            });
            node.set_label(if checked { "Done" } else { "To do" });
            if reading::can_edit(app, doc) {
                node.add_action(Action::Click);
                out.action(
                    id,
                    tab,
                    doc,
                    ReadingAction::Task {
                        start: n.range.start,
                        checked,
                    },
                );
            } else {
                node.set_disabled();
            }
        }
        ReadKind::Link { embed, .. } => {
            if reading::current(app, doc) {
                node.add_action(Action::Click);
                out.action(id, tab, doc, ReadingAction::Link(n.range.start));
            }
            if embed {
                match doc.embeds.get(&n.range.start) {
                    Some(flavite_api::EmbedContent::Note(child)) => {
                        let child_instance = instance
                            .wrapping_mul(16777619)
                            .wrapping_add(n.range.start as u64)
                            .wrapping_add(1);
                        for n in &child.document.nodes {
                            children.push(project_node(app, out, tab, child, n, child_instance));
                        }
                    }
                    Some(flavite_api::EmbedContent::Unavailable(s)) => {
                        node.set_description(s.clone())
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    node.set_children(children);
    out.push(id, node);
    id
}
pub fn route(
    app: &App,
    tab: TabId,
    doc: Arc<ReadSnapshot>,
    action: ReadingAction,
    request: accesskit::ActionRequest,
) -> Option<Message> {
    let edit = matches!(
        action,
        ReadingAction::Task { .. }
            | ReadingAction::PropertyInput(_)
            | ReadingAction::PropertyApply(_)
    );
    if !(if edit {
        reading::bound_edit(app, tab, &doc)
    } else {
        reading::bound(app, tab, &doc)
    }) {
        return None;
    }
    match action {
        ReadingAction::OpenSource if request.action == Action::Click => {
            let locator = flavite_workspace::NoteLocator::from_relative(&doc.path).ok()?;
            Some(
                if matches!(app.workspace.tab(&tab)?.kind(), ViewKind::Properties(_)) {
                    Message::EditSourceFromContext { locator }
                } else {
                    Message::OpenNoteFromContext { locator }
                },
            )
        }
        ReadingAction::InsertLink
            if request.action == Action::Click && app.valid_writing_target().is_some() =>
        {
            Some(Message::InsertLink {
                locator: flavite_workspace::NoteLocator::from_relative(&doc.path).ok()?,
            })
        }
        ReadingAction::ToggleSource if request.action == Action::Click => {
            Some(Message::ToggleSourcePreview(tab))
        }
        ReadingAction::Link(start) if request.action == Action::Click => {
            Some(Message::ReadActivate {
                tab,
                document: doc,
                start,
            })
        }
        ReadingAction::Outline(byte) if request.action == Action::Click => {
            Some(Message::OutlineNavigate {
                tab,
                document: doc,
                byte,
            })
        }
        ReadingAction::PropertyInput(key) if request.action == Action::SetValue => {
            if let Some(accesskit::ActionData::Value(value)) = request.data {
                Some(Message::PropertyDraft {
                    tab,
                    document: doc,
                    key,
                    value: value.into(),
                })
            } else {
                None
            }
        }
        ReadingAction::PropertyApply(key) if request.action == Action::Click => {
            Some(Message::PropertyApply {
                tab,
                document: doc,
                key,
            })
        }
        ReadingAction::Task { start, checked } if request.action == Action::Click => {
            let (buffer, revision) = doc.buffer?;
            Some(Message::StructureEdit {
                tab,
                document: doc,
                command: "task.toggle",
                args: serde_json::json!({"buffer_id":buffer,"revision":revision,"start":start,"checked":checked}),
            })
        }
        _ => None,
    }
}
