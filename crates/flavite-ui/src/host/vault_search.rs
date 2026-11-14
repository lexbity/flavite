//! Presentation ownership for one vault-search workbench in a panel or tab.
use crate::app::{App, Message};
use flavite_workspace::{LayoutAction, NoteLocator, TabId, ViewKind};
use std::collections::BTreeMap;

/// Process-local panel owner, outside persisted workspace tab identities.
pub const SIDEBAR_SEARCH: TabId = TabId::from_raw(u128::MAX - 13);
#[derive(Debug, Default)]
pub struct VaultSearchHost {
    pub sidebar: bool,
    pub indexed: BTreeMap<TabId, bool>,
    pub bounds: BTreeMap<(TabId, String), iced::Rectangle>,
    bound_generations: BTreeMap<TabId, u64>,
    jumps: BTreeMap<TabId, MatchJump>,
}
#[derive(Debug)]
struct MatchJump {
    owner: TabId,
    finder: String,
    incarnation: u64,
    path: NoteLocator,
    hash: String,
    start: usize,
    end: usize,
}
#[derive(Debug, Clone)]
pub enum VaultSearchMessage {
    Sidebar(bool),
    FocusIndexed(TabId),
    FocusField(TabId, super::replace::ReplaceField),
    Mode(TabId, bool),
    Dock(TabId),
    Undock,
    Bounds(TabId, String, u64, iced::Rectangle),
    OpenMatch {
        owner: TabId,
        finder: String,
        path: String,
        hash: String,
        start: u64,
        end: u64,
    },
}
impl VaultSearchHost {
    pub fn indexed(&self, app: &App, tab: TabId) -> bool {
        self.indexed.get(&tab).copied().unwrap_or_else(|| {
            app.workspace
                .tab(&tab)
                .is_some_and(|view| matches!(view.kind(), ViewKind::Search))
        })
    }
    pub fn transfer(&mut self, from: TabId, to: TabId) {
        if let Some(mode) = self.indexed.remove(&from) {
            self.indexed.insert(to, mode);
        }
        self.bounds.retain(|(owner, _), _| *owner != from);
        for jump in self.jumps.values_mut() {
            if jump.owner == from {
                jump.owner = to;
            }
        }
    }
}
fn transfer(app: &mut App, from: TabId, to: TabId) {
    let indexed = app.vault_search.indexed(app, from);
    app.vault_search.indexed.insert(from, indexed);
    app.replace.transfer(from, to);
    app.search.transfer(from, to);
    app.vault_search.transfer(from, to);
}
pub fn handle(app: &mut App, action: VaultSearchMessage) -> iced::Task<Message> {
    match action {
        VaultSearchMessage::FocusIndexed(tab) => {
            return iced::widget::operation::focus(crate::views::search::input_id(tab));
        }
        VaultSearchMessage::FocusField(tab, field) => {
            let suffix = match field {
                super::replace::ReplaceField::Query => "-query",
                super::replace::ReplaceField::Replacement => "-replacement",
                super::replace::ReplaceField::Folder => "-folder",
            };
            return iced::widget::operation::focus(crate::views::replace::input_id(tab, suffix));
        }
        VaultSearchMessage::Bounds(tab, name, generation, rect) => {
            if generation != app.replace.view_state(tab).frame_generation() {
                return iced::Task::none();
            }
            if app.vault_search.bound_generations.insert(tab, generation) != Some(generation) {
                app.vault_search
                    .bounds
                    .retain(|(owner, _), _| *owner != tab);
            }
            app.vault_search.bounds.insert((tab, name), rect);
        }
        VaultSearchMessage::Sidebar(search) => {
            app.vault_search.sidebar = search;
            app.set_sidebar_open(true);
        }
        VaultSearchMessage::Mode(tab, indexed) => {
            let query = if indexed {
                app.replace.view_state(tab).query.clone()
            } else {
                app.search.view_state(tab).query.clone()
            };
            app.vault_search.indexed.insert(tab, indexed);
            if indexed {
                app.search.edited(tab, query);
            } else {
                app.replace
                    .edited(tab, super::replace::ReplaceField::Query, query);
            }
        }
        VaultSearchMessage::Dock(tab) => {
            if app
                .workspace
                .tab(&tab)
                .is_none_or(|view| !matches!(view.kind(), ViewKind::Search | ViewKind::Replace))
            {
                return iced::Task::none();
            }
            // Replace the old panel's live search only when a real tool is docked.
            app.replace.detach(SIDEBAR_SEARCH, app.flavite.as_ref());
            app.search.detach(SIDEBAR_SEARCH);
            transfer(app, tab, SIDEBAR_SEARCH);
            if let Some(group) = app.workspace.group_of(tab) {
                if app
                    .workspace
                    .group(&group)
                    .is_some_and(|group| group.tabs().len() == 1)
                {
                    app.dispatch_layout(|expected_revision| LayoutAction::OpenEmpty {
                        expected_revision,
                        group,
                    });
                }
                app.dispatch_layout(|expected_revision| LayoutAction::CloseTab {
                    expected_revision,
                    tab,
                });
            }
            app.vault_search.sidebar = true;
            app.set_sidebar_open(true);
        }
        VaultSearchMessage::Undock => {
            if let Some(group) = app.navigation_group() {
                app.dispatch_layout(|expected_revision| LayoutAction::OpenReplace {
                    expected_revision,
                    group,
                });
                let tab = app.workspace.focused_tab();
                transfer(app, SIDEBAR_SEARCH, tab);
                app.set_sidebar_open(false);
            }
        }
        VaultSearchMessage::OpenMatch {
            owner,
            finder,
            path,
            hash,
            start,
            end,
        } => {
            if app.replace.view_state(owner).finder.as_ref() != Some(&finder) {
                app.status = "Search results changed; choose the match again".into();
                return iced::Task::none();
            }
            let Ok(path) = NoteLocator::from_relative(std::path::Path::new(&path)) else {
                return iced::Task::none();
            };
            let (Ok(start), Ok(end)) = (usize::try_from(start), usize::try_from(end)) else {
                return iced::Task::none();
            };
            app.open_intent(super::open_intents::OpenIntent::reuse(
                path.clone(),
                (owner != SIDEBAR_SEARCH).then_some(owner),
            ));
            let tab = app.workspace.focused_tab();
            if app.notes.locator_of(tab).as_ref() != Some(&path) {
                return iced::Task::none();
            }
            let Some(incarnation) = app.notes.incarnation(tab) else {
                return iced::Task::none();
            };
            app.vault_search.jumps.insert(
                tab,
                MatchJump {
                    owner,
                    finder,
                    incarnation,
                    path,
                    hash,
                    start,
                    end,
                },
            );
        }
    }
    iced::Task::none()
}

pub fn pump(app: &mut App) {
    let ready: Vec<_> = app
        .vault_search
        .jumps
        .keys()
        .copied()
        .filter(|tab| app.note_editor_view(*tab).is_some() || app.workspace.tab(tab).is_none())
        .collect();
    for tab in ready {
        let Some(jump) = app.vault_search.jumps.remove(&tab) else {
            continue;
        };
        let Some(view) = app.note_editor_view(tab) else {
            continue;
        };
        if app.notes.incarnation(tab) != Some(jump.incarnation)
            || app.notes.locator_of(tab).as_ref() != Some(&jump.path)
            || app.workspace.focused_tab() != tab
        {
            continue;
        }
        if app.replace.view_state(jump.owner).finder.as_ref() != Some(&jump.finder)
            || !flavite_api::replace::source_match_current(
                &view.text, &jump.hash, jump.start, jump.end,
            )
        {
            app.status = "Search match is stale; refresh vault search".into();
            continue;
        }
        if !flavite_buffer::selection::is_grapheme_boundary(&view.text, jump.start)
            || !flavite_buffer::selection::is_grapheme_boundary(&view.text, jump.end)
        {
            app.status = "Match falls inside a grapheme; inspect it with note Find".into();
            continue;
        }
        app.accessible_selection(tab, jump.incarnation, view.revision, jump.start, jump.end);
        app.status = "Opened exact source match".into();
    }
}
