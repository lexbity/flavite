//! Search actions share the presentation's guarded preview and job identities.
use crate::{
    app::{App, Message},
    host::{
        replace::{ReplaceField, ReplaceFlag, ReplacePanelMode, SelectAction},
        vault_search::{SIDEBAR_SEARCH, VaultSearchMessage},
    },
};
use accesskit::{Action, ActionData, ActionRequest, Node, NodeId, Role};
use flavite_workspace::{TabId, ViewKind};

pub(crate) struct Control {
    pub id: NodeId,
    pub node: Node,
    message: Message,
    field: Option<(TabId, ReplaceField)>,
}

pub(crate) fn controls(app: &App) -> Vec<Control> {
    let mut owners: Vec<_> =
        app.workspace
            .group_order()
            .into_iter()
            .filter_map(|group| app.workspace.group(&group).map(|group| group.active()))
            .filter(|tab| {
                app.is_tab_visible(*tab)
                    && app.workspace.tab(tab).is_some_and(|view| {
                        matches!(view.kind(), ViewKind::Search | ViewKind::Replace)
                    })
            })
            .collect();
    if app.sidebar_open() && app.vault_search.sidebar {
        owners.push(SIDEBAR_SEARCH);
    }
    let mut controls = Vec::new();
    for tab in owners {
        let state = app.replace.view_state(tab);
        let mut add = |name: String,
                       label: String,
                       message: Message,
                       input: Option<(ReplaceField, String)>,
                       checked: Option<bool>| {
            // Include captured authority in the node identity. An old assistive
            // action must never acquire a newer preview, selection or job.
            let id = super::tree::node_id(
                &format!(
                    "vault-search-{}-{name}-{:?}",
                    state.frame_generation(),
                    message
                ),
                tab.raw(),
            );
            let mut node = Node::new(if input.is_some() {
                Role::TextInput
            } else if checked.is_some() {
                Role::CheckBox
            } else {
                Role::Button
            });
            node.set_label(label);
            let field = input.map(|(field, value)| {
                node.set_value(value);
                node.add_action(Action::SetValue);
                node.add_action(Action::Focus);
                (tab, field)
            });
            if field.is_none() {
                node.add_action(Action::Click);
            }
            if let Some(checked) = checked {
                node.set_toggled(if checked {
                    accesskit::Toggled::True
                } else {
                    accesskit::Toggled::False
                });
            }
            if let Some(rect) = app.vault_search.bounds.get(&(tab, name)) {
                node.set_bounds(super::tree::semantic_bounds(*rect));
            }
            controls.push(Control {
                id,
                node,
                message,
                field,
            });
        };
        for (name, label, message) in [
            (
                "source",
                "Source find",
                Message::VaultSearch(VaultSearchMessage::Mode(tab, false)),
            ),
            (
                "indexed",
                "Indexed saved-content search",
                Message::VaultSearch(VaultSearchMessage::Mode(tab, true)),
            ),
            (
                "dock",
                if tab == SIDEBAR_SEARCH {
                    "Open search as tab"
                } else {
                    "Move search to sidebar"
                },
                Message::VaultSearch(if tab == SIDEBAR_SEARCH {
                    VaultSearchMessage::Undock
                } else {
                    VaultSearchMessage::Dock(tab)
                }),
            ),
        ] {
            add(name.into(), label.into(), message, None, None);
        }
        if app.vault_search.indexed(app, tab) {
            let indexed = app.search.view_state(tab);
            let mut query = Node::new(Role::TextInput);
            query.set_label("Search indexed saved content");
            query.set_value(indexed.query.clone());
            query.add_action(Action::SetValue);
            query.add_action(Action::Focus);
            let id = super::tree::node_id("vault-search-indexed-query", tab.raw());
            controls.push(Control {
                id,
                node: query,
                message: Message::VaultSearch(VaultSearchMessage::FocusIndexed(tab)),
                field: None,
            });
            for (name, label, mode) in [
                (
                    "text",
                    "Full-text search",
                    crate::host::search::SearchPanelMode::FullText,
                ),
                (
                    "expression",
                    "Indexed expression search",
                    crate::host::search::SearchPanelMode::Expression,
                ),
            ] {
                let message = Message::SearchSetMode { tab, mode };
                let id = super::tree::node_id(&format!("vault-search-indexed-{name}"), tab.raw());
                let mut node = Node::new(Role::Button);
                node.set_label(label);
                node.add_action(Action::Click);
                controls.push(Control {
                    id,
                    node,
                    message,
                    field: None,
                });
            }
            for row in &indexed.rows {
                if let Ok(locator) =
                    flavite_workspace::NoteLocator::from_relative(std::path::Path::new(&row.path))
                {
                    let id = super::tree::node_id(
                        &format!(
                            "vault-search-indexed-result-{}-{:?}",
                            row.path, indexed.snapshot
                        ),
                        tab.raw(),
                    );
                    let mut node = Node::new(Role::Button);
                    node.set_label(format!("{}; {}", row.title, row.excerpt));
                    node.add_action(Action::Click);
                    controls.push(Control {
                        id,
                        node,
                        message: Message::OpenIntent(crate::host::open_intents::OpenIntent::reuse(
                            locator, None,
                        )),
                        field: None,
                    });
                }
            }
            continue;
        }
        for (name, label, field, value) in [
            (
                "query",
                "Find in Markdown source",
                ReplaceField::Query,
                state.query.clone(),
            ),
            (
                "folder",
                "Search folder; empty means whole vault",
                ReplaceField::Folder,
                state.folder.clone(),
            ),
            (
                "replacement",
                "Replacement text",
                ReplaceField::Replacement,
                state.replacement.clone(),
            ),
        ] {
            if field == ReplaceField::Replacement && state.mode != ReplacePanelMode::Preview {
                continue;
            }
            add(
                name.into(),
                label.into(),
                Message::VaultSearch(VaultSearchMessage::FocusField(tab, field)),
                Some((field, value)),
                None,
            );
        }
        for (name, label, flag, checked) in [
            (
                "regex",
                "Regular expression",
                ReplaceFlag::Regex,
                state.regex,
            ),
            (
                "case",
                "Case sensitive",
                ReplaceFlag::CaseSensitive,
                state.case_sensitive,
            ),
            (
                "word",
                "Whole word",
                ReplaceFlag::WholeWord,
                state.whole_word,
            ),
            (
                "drafts",
                "Include unsaved notes",
                ReplaceFlag::IncludeDrafts,
                state.include_drafts,
            ),
        ] {
            add(
                name.into(),
                label.into(),
                Message::ReplaceToggle { tab, flag },
                None,
                Some(checked),
            );
        }
        for (name, label, mode) in [
            ("mode-find", "Find", ReplacePanelMode::Find),
            ("mode-replace", "Replace", ReplacePanelMode::Preview),
        ] {
            add(
                name.into(),
                label.into(),
                Message::ReplaceSetMode { tab, mode },
                None,
                None,
            );
        }
        if state.mode == ReplacePanelMode::Find {
            add(
                "find".into(),
                "Find in vault".into(),
                Message::ReplaceSubmitNow { tab },
                None,
                None,
            );
            if state.has_next() {
                add(
                    "next".into(),
                    "Next matches".into(),
                    Message::ReplaceNextPage { tab },
                    None,
                    None,
                );
            }
            for row in &state.rows {
                add(
                    format!("group:{}", row.ordinal),
                    format!("{}; {} matches", row.path, row.note_matches),
                    Message::ReplaceCollapse {
                        tab,
                        path: row.path.clone(),
                    },
                    None,
                    None,
                );
                if state.collapsed.contains(&row.path) {
                    continue;
                }
                for hit in &row.contexts {
                    add(
                        format!("match:{}:{}", row.ordinal, hit.start),
                        format!(
                            "{} line {} column {}: {}{}{}",
                            row.path, hit.line, hit.column, hit.before, hit.matched, hit.after
                        ),
                        Message::VaultSearch(VaultSearchMessage::OpenMatch {
                            owner: tab,
                            finder: state.finder.clone().unwrap_or_default(),
                            path: row.path.clone(),
                            hash: row.content_hash.clone(),
                            start: hit.start,
                            end: hit.end,
                        }),
                        None,
                        None,
                    );
                }
            }
        } else {
            add(
                "run-preview".into(),
                "Run replacement preview".into(),
                Message::ReplaceRunPreview { tab },
                None,
                None,
            );
            if let Some(target) = state.preview_target() {
                for (name, label, selection) in [
                    ("select-all", "Select all notes", SelectAction::All),
                    ("select-none", "Select no notes", SelectAction::None),
                ] {
                    add(
                        name.into(),
                        label.into(),
                        Message::ReplaceSelect {
                            tab,
                            target: target.clone(),
                            selection,
                        },
                        None,
                        None,
                    );
                }
                if state.can_apply() {
                    add(
                        "apply".into(),
                        format!(
                            "Apply replacement to {} selected notes",
                            state.selected_count()
                        ),
                        Message::ReplaceApply {
                            tab,
                            target: target.clone(),
                        },
                        None,
                        None,
                    );
                }
                for candidate in &state.candidates {
                    let checked = state
                        .selected
                        .get(&candidate.path_native)
                        .copied()
                        .unwrap_or(false);
                    let mut keys: Vec<_> = state
                        .selected
                        .iter()
                        .filter(|(path, selected)| **selected && **path != candidate.path_native)
                        .map(|(path, _)| path.clone())
                        .collect();
                    if !checked {
                        keys.push(candidate.path_native.clone());
                    }
                    add(
                        format!("candidate:{}", candidate.item_id),
                        format!(
                            "{}; {} replacement matches",
                            candidate.path, candidate.match_count
                        ),
                        Message::ReplaceSelect {
                            tab,
                            target: target.clone(),
                            selection: SelectAction::Keys(keys),
                        },
                        None,
                        Some(checked),
                    );
                    let diff = state
                        .diffs
                        .get(&candidate.item_id)
                        .and_then(|diff| diff.as_ref().ok())
                        .map_or_else(String::new, |diff| {
                            format!(
                                "; before: {}; after: {}{}",
                                diff.before,
                                diff.after,
                                if diff.truncated { "; excerpt" } else { "" }
                            )
                        });
                    add(
                        format!("inspect:{}", candidate.item_id),
                        format!("Inspect changes in {}{diff}", candidate.path),
                        Message::ReplaceInspect {
                            tab,
                            item_id: candidate.item_id,
                        },
                        None,
                        None,
                    );
                }
            }
        }
        if let Some(job) = &state.job {
            for (name, label, enabled, message) in [
                (
                    "cancel",
                    "Cancel replacement",
                    state.can_cancel(),
                    Message::ReplaceCancel {
                        tab,
                        job_id: job.job_id.clone(),
                    },
                ),
                (
                    "resume",
                    "Resume replacement",
                    state.can_resume(),
                    Message::ReplaceResume {
                        tab,
                        job_id: job.job_id.clone(),
                    },
                ),
                (
                    "revert",
                    "Revert replacement",
                    state.can_revert(),
                    Message::ReplaceRevert {
                        tab,
                        job_id: job.job_id.clone(),
                    },
                ),
            ] {
                if enabled {
                    add(name.into(), label.into(), message, None, None);
                }
            }
        }
    }
    controls
}

pub(crate) fn route(app: &App, id: NodeId, request: ActionRequest) -> Option<Message> {
    let control = controls(app).into_iter().find(|control| control.id == id)?;
    if let Message::VaultSearch(VaultSearchMessage::FocusIndexed(tab)) = control.message {
        return match (request.action, request.data) {
            (Action::SetValue, Some(ActionData::Value(value))) => Some(Message::SearchQuery {
                tab,
                value: value.into(),
            }),
            (Action::Focus, _) => Some(control.message),
            _ => None,
        };
    }
    match (request.action, request.data, control.field) {
        (Action::SetValue, Some(ActionData::Value(value)), Some((tab, field))) => {
            Some(Message::ReplaceEdit {
                tab,
                field,
                value: value.into(),
            })
        }
        (Action::Focus, _, Some(_)) | (Action::Click, _, None) => Some(control.message),
        _ => None,
    }
}
