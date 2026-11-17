//! Shared lazy vault tree for Notes tabs and the sidebar.
use crate::app::{App, Message};
use flavite_api::{
    ContextQueryResult, DirectoryCursor, DirectoryEntry, QueryOutcome, QueryPayload, RequestToken,
};
use flavite_workspace::{NoteLocator, TabId};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::PathBuf,
};

pub type Owner = Option<TabId>;
const QUERY_OWNER: u128 = u128::MAX - 12;

#[derive(Debug, Clone, Default)]
pub struct BrowserView {
    pub expanded: BTreeSet<PathBuf>,
    pub selected: Option<PathBuf>,
    pub scroll: f32,
    pub matches: BTreeSet<PathBuf>,
}
#[derive(Debug, Clone, Default)]
pub struct Directory {
    pub entries: Vec<DirectoryEntry>,
    pub next: Option<DirectoryCursor>,
    pub error: Option<String>,
    /// Cursors that loaded pages beyond the first, in load order. A refresh
    /// re-enqueues these so a reload restores the same pagination extent
    /// instead of collapsing the folder to its first page.
    continuations: Vec<DirectoryCursor>,
}
#[derive(Debug, Default)]
pub struct FileBrowser {
    pub views: BTreeMap<Owner, BrowserView>,
    pub directories: BTreeMap<PathBuf, Directory>,
    queue: VecDeque<(PathBuf, Option<DirectoryCursor>)>,
    pending: Option<(RequestToken, PathBuf, Option<DirectoryCursor>)>,
    attachment: BTreeMap<RequestToken, Owner>,
    generation: u64,
    initialized: bool,
    /// An implicit reload (catalog index change) was requested while the
    /// browser had a query in flight; it runs when the browser is idle.
    refresh_pending: bool,
    pub focused: Option<Owner>,
    typeahead: String,
    typed_at: Option<std::time::Instant>,
}
#[derive(Debug, Clone)]
pub enum BrowserMessage {
    Toggle(Owner, PathBuf),
    Open(Owner, PathBuf),
    Reveal(Owner),
    More(PathBuf),
    Refresh,
    Scroll(Owner, f32),
    Select(Owner, PathBuf),
    TypeAhead(Owner, String),
}
#[derive(Debug, Clone)]
pub struct TreeRow {
    pub entry: DirectoryEntry,
    pub depth: usize,
    pub expanded: bool,
}

impl FileBrowser {
    pub fn transfer(&mut self, from: Owner, to: Owner) {
        let state = self.views.remove(&from).unwrap_or_default();
        self.views.insert(to, state);
        if self.focused == Some(from) {
            self.focused = Some(to);
        }
    }
    fn enqueue(&mut self, path: PathBuf, cursor: Option<DirectoryCursor>) {
        // A queued initial page must not silently swallow a later explicit
        // "Load more" for the same folder: the requests differ by cursor, so
        // dedupe on the exact request (initial page or the same continuation
        // cursor). Distinct continuation cursors (loaded pages beyond the
        // first) are all queued — a refresh replays them to restore the
        // pagination extent. A refresh clearing the queue keeps this honest
        // — it re-enqueues everything it needs.
        if !self
            .queue
            .iter()
            .any(|(queued, queued_cursor)| queued == &path && queued_cursor == &cursor)
            && !self.pending.as_ref().is_some_and(|(_, queued, pending_cursor)| {
                queued == &path && *pending_cursor == cursor
            })
        {
            self.queue.push_back((path, cursor));
        }
    }
    /// Reload the vault tree, preserving each directory's pagination extent
    /// (cursors that loaded pages beyond the first): the reload re-enqueues
    /// every loaded page instead of collapsing folders to their first page
    /// mid-browse. Used by implicit reloads (catalog index changes) where
    /// an in-flight directory query must not be orphaned.
    pub fn refresh(&mut self) {
        let continuations: Vec<(PathBuf, Vec<DirectoryCursor>)> = self
            .directories
            .iter()
            .filter(|(_, dir)| !dir.continuations.is_empty())
            .map(|(path, dir)| (path.clone(), dir.continuations.clone()))
            .collect();
        self.generation += 1;
        self.pending = None;
        self.queue.clear();
        self.directories.clear();
        self.enqueue(PathBuf::new(), None);
        for (path, cursors) in continuations {
            self.enqueue(path.clone(), None);
            for cursor in cursors {
                self.enqueue(path.clone(), Some(cursor));
            }
        }
        let paths: BTreeSet<_> = self
            .views
            .values()
            .flat_map(|view| view.expanded.iter().cloned())
            .collect();
        for path in paths {
            if !self.directories.contains_key(&path)
                && !self.queue.iter().any(|(queued, _)| queued == &path)
            {
                self.enqueue(path, None);
            }
        }
        self.initialized = true;
    }

    /// Hard reload: drop every loaded directory and restart from the first
    /// page. Used by the explicit user Refresh action.
    pub fn refresh_reset(&mut self) {
        self.generation += 1;
        self.pending = None;
        self.queue.clear();
        self.directories.clear();
        self.enqueue(PathBuf::new(), None);
        let paths: BTreeSet<_> = self
            .views
            .values()
            .flat_map(|view| view.expanded.iter().cloned())
            .collect();
        for path in paths {
            self.enqueue(path, None);
        }
        self.initialized = true;
    }

    /// Request an implicit reload (e.g. a catalog index change). The reload
    /// is deferred until no directory query is in flight or queued so it
    /// never orphans a pending continuation mid-paging.
    pub fn request_refresh(&mut self) {
        self.refresh_pending = true;
    }

    /// Run the deferred implicit reload when the browser is idle. Returns
    /// whether a reload ran. Called at the start of a pump turn, before any
    /// results are drained, so a freshly applied result is never erased by
    /// the reload in the same turn.
    pub fn refresh_if_idle(&mut self) -> bool {
        if self.refresh_pending && self.pending.is_none() && self.queue.is_empty() {
            self.refresh_pending = false;
            self.refresh();
            true
        } else {
            false
        }
    }
    pub fn rows(&self, owner: Owner, filter: &str) -> Vec<TreeRow> {
        let view = self.views.get(&owner);
        let needle = filter.to_lowercase();
        let mut admitted = BTreeSet::new();
        if !needle.is_empty() {
            let matches = self
                .directories
                .values()
                .flat_map(|dir| &dir.entries)
                .filter(|entry| {
                    entry
                        .path
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&needle)
                })
                .map(|entry| &entry.path)
                .chain(view.into_iter().flat_map(|view| &view.matches));
            for path in matches {
                admitted.extend(path.ancestors().map(std::path::Path::to_path_buf));
            }
        }
        fn walk(
            browser: &FileBrowser,
            path: &std::path::Path,
            depth: usize,
            view: Option<&BrowserView>,
            admitted: Option<&BTreeSet<PathBuf>>,
            out: &mut Vec<TreeRow>,
        ) {
            if depth > 128 {
                return;
            }
            let Some(directory) = browser.directories.get(path) else {
                return;
            };
            for entry in &directory.entries {
                let open = entry.directory
                    && (admitted.is_some_and(|paths| paths.contains(&entry.path))
                        || view.is_some_and(|view| view.expanded.contains(&entry.path)));
                if admitted.is_none_or(|paths| paths.contains(&entry.path)) {
                    out.push(TreeRow {
                        entry: entry.clone(),
                        depth,
                        expanded: open,
                    });
                    if open {
                        walk(browser, &entry.path, depth + 1, view, admitted, out);
                    }
                }
            }
        }
        let mut out = Vec::new();
        walk(
            self,
            std::path::Path::new(""),
            0,
            view,
            (!needle.is_empty()).then_some(&admitted),
            &mut out,
        );
        out
    }
    pub fn reveal_path(&mut self, owner: Owner, path: PathBuf) {
        let parents: Vec<_> = path
            .ancestors()
            .skip(1)
            .filter(|p| !p.as_os_str().is_empty())
            .map(std::path::Path::to_path_buf)
            .collect();
        let view = self.views.entry(owner).or_default();
        view.selected = Some(path);
        view.expanded.extend(parents.iter().cloned());
        for parent in parents.into_iter().rev() {
            if !self.directories.contains_key(&parent) {
                self.enqueue(parent, None);
            }
        }
    }
}

pub fn filter_changed(app: &mut App, owner: Owner, query: &str) {
    app.file_browser.views.entry(owner).or_default().scroll = 0.0;
    app.file_browser
        .views
        .entry(owner)
        .or_default()
        .matches
        .clear();
    if query.is_empty() {
        return;
    }
    let query = query.to_lowercase();
    let paths: Vec<_> = app
        .catalog()
        .iter()
        .filter(|entry| {
            entry.path.to_string_lossy().to_lowercase().contains(&query)
                || entry.title.to_lowercase().contains(&query)
        })
        .map(|entry| entry.path.clone())
        .collect();
    for path in paths {
        app.file_browser
            .views
            .entry(owner)
            .or_default()
            .matches
            .insert(path.clone());
        app.file_browser.reveal_path(owner, path);
    }
}

pub fn pump(app: &mut App) {
    let Some(api) = &app.flavite else {
        return;
    };
    if !app.file_browser.initialized {
        app.file_browser.refresh();
    }
    if app.file_browser.pending.is_none()
        && let Some((path, cursor)) = app.file_browser.queue.front().cloned()
        && let Ok(token) = api.query_service().submit(
            QUERY_OWNER,
            1,
            app.file_browser.generation,
            QueryPayload::Directory {
                path: path.clone(),
                cursor: cursor.clone(),
                limit: 200,
            },
        )
    {
        app.file_browser.queue.pop_front();
        app.file_browser.pending = Some((token, path, cursor));
    }
}

pub fn observe(app: &mut App, outcome: &QueryOutcome) -> bool {
    if let Some(owner) = app.file_browser.attachment.remove(&outcome.token) {
        // A result bound to a replaced view is stale: its incarnation no
        // longer matches, so the opener must not fire (a replaced tab drops
        // silently, like the graph inspector).
        let live = owner.is_none_or(|tab| {
            app.workspace
                .tab(&tab)
                .is_some_and(|view| view.interaction().incarnation == outcome.token.incarnation)
        });
        if live {
            app.status = match &outcome.result {
                ContextQueryResult::AttachmentOpen(Ok(path)) => {
                    if super::reading::spawn_os(path.as_os_str()).is_ok() {
                        "Opened local file".into()
                    } else {
                        "Could not open local file".into()
                    }
                }
                ContextQueryResult::AttachmentOpen(Err(reason)) => reason.clone(),
                _ => "This file cannot be opened; use a supported local document".into(),
            };
        }
        return true;
    }
    if outcome.token.context_tab != QUERY_OWNER
        || outcome.token.purpose != flavite_api::QueryPurpose::Directory
    {
        return false;
    }
    let Some((token, path, cursor)) = app.file_browser.pending.clone() else {
        return true;
    };
    if token != outcome.token {
        return true;
    }
    let append = cursor.is_some();
    app.file_browser.pending = None;
    let retained: usize = app
        .file_browser
        .directories
        .values()
        .map(|dir| dir.entries.len())
        .sum();
    if let ContextQueryResult::Directory(Ok(page)) = &outcome.result
        && retained.saturating_add(page.entries.len()) > 100_000
    {
        app.file_browser.directories.entry(path).or_default().error = Some(
            "File browser retains at most 100,000 entries; collapse folders and refresh".into(),
        );
        return true;
    }
    let directory = app.file_browser.directories.entry(path).or_default();
    match &outcome.result {
        ContextQueryResult::Directory(Ok(page)) => {
            if !append {
                directory.entries.clear();
                directory.continuations.clear();
            }
            directory.entries.extend(page.entries.iter().cloned());
            directory.next.clone_from(&page.next);
            directory.error = None;
            // Remember the cursor that loaded this page so a later refresh
            // can restore the same pagination extent.
            if append
                && let Some(cursor) = cursor
                && !directory.continuations.contains(&cursor)
            {
                directory.continuations.push(cursor);
            }
        }
        ContextQueryResult::Directory(Err(error))
        | ContextQueryResult::Failed { message: error } => directory.error = Some(error.clone()),
        _ => directory.error = Some("Directory unavailable; refresh to retry".into()),
    }
    true
}

pub fn handle(app: &mut App, message: BrowserMessage) -> iced::Task<Message> {
    match message {
        BrowserMessage::Select(owner, path) => {
            app.file_browser.focused = Some(owner);
            let rows = app.file_browser.rows(
                owner,
                owner.map_or(app.sidebar_filter.as_str(), |tab| app.notes_filter(tab)),
            );
            let index = rows
                .iter()
                .position(|row| row.entry.path == path)
                .unwrap_or(0);
            let state = app.file_browser.views.entry(owner).or_default();
            state.selected = Some(path);
            let row_height = 30.0 * app.appearance.metrics().scale;
            state.scroll = index as f32 * row_height;
            return iced::widget::operation::scroll_to(
                crate::views::notes::scroll_id(owner),
                iced::widget::scrollable::AbsoluteOffset {
                    x: 0.0,
                    y: state.scroll,
                },
            );
        }
        BrowserMessage::TypeAhead(owner, produced) => {
            let now = std::time::Instant::now();
            if app
                .file_browser
                .typed_at
                .is_none_or(|at| now.duration_since(at) > std::time::Duration::from_secs(1))
            {
                app.file_browser.typeahead.clear();
            }
            app.file_browser.typed_at = Some(now);
            if app.file_browser.typeahead.len() + produced.len() <= 256 {
                app.file_browser
                    .typeahead
                    .push_str(&produced.to_lowercase());
            }
            let rows = app.file_browser.rows(
                owner,
                owner.map_or(app.sidebar_filter.as_str(), |tab| app.notes_filter(tab)),
            );
            if let Some(row) = rows.iter().find(|row| {
                row.entry
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase()
                    .starts_with(&app.file_browser.typeahead)
            }) {
                return handle(app, BrowserMessage::Select(owner, row.entry.path.clone()));
            }
        }
        BrowserMessage::Refresh => {
            app.file_browser.refresh_reset();
            app.request_catalog_refresh();
        }
        BrowserMessage::Scroll(owner, offset) => {
            app.file_browser.views.entry(owner).or_default().scroll = offset
        }
        BrowserMessage::More(path) => {
            if let Some(cursor) = app
                .file_browser
                .directories
                .get(&path)
                .and_then(|dir| dir.next.clone())
            {
                app.file_browser.enqueue(path, Some(cursor));
            }
        }
        BrowserMessage::Reveal(owner) => {
            if let Some(path) = app
                .workspace
                .document_anchor()
                .and_then(|tab| app.notes.locator_of(tab))
                .map(|locator| locator.components().iter().collect::<PathBuf>())
            {
                app.file_browser.reveal_path(owner, path);
            }
        }
        BrowserMessage::Toggle(owner, path) => {
            app.file_browser.focused = Some(owner);
            if app
                .file_browser
                .directories
                .values()
                .flat_map(|dir| &dir.entries)
                .any(|entry| entry.path == path && entry.symlink)
            {
                app.status = "Symbolic links are not expanded from the vault browser".into();
                return iced::Task::none();
            }
            let state = app.file_browser.views.entry(owner).or_default();
            state.selected = Some(path.clone());
            if !state.expanded.remove(&path) {
                state.expanded.insert(path.clone());
                if !app.file_browser.directories.contains_key(&path) {
                    app.file_browser.enqueue(path, None);
                }
            }
        }
        BrowserMessage::Open(owner, path) => {
            app.file_browser.focused = Some(owner);
            app.file_browser.views.entry(owner).or_default().selected = Some(path.clone());
            let entry = app
                .file_browser
                .directories
                .values()
                .flat_map(|dir| &dir.entries)
                .find(|entry| entry.path == path)
                .cloned();
            match entry {
                Some(entry) if entry.directory => {
                    return handle(app, BrowserMessage::Toggle(owner, path));
                }
                Some(entry) if entry.symlink => {
                    app.status = "Symbolic links are not opened from the vault browser".into()
                }
                Some(_) if flavite_api::attachment_kind(&path).is_none() => {
                    // Anything the vault does not classify as a binary
                    // attachment opens as a text note, regardless of
                    // extension. Genuinely undecodable bytes surface the
                    // typed encoding-choice flow when the buffer installs.
                    match NoteLocator::from_relative(&path) {
                        Ok(locator) => {
                            app.open_intent(super::open_intents::OpenIntent::reuse(locator, owner));
                        }
                        Err(error) => app.status = error.to_string(),
                    }
                }
                Some(_) => {
                    // One attachment activation per owner at a time, and none
                    // colliding with a reading/graph open for the same view:
                    // same-identity results supersede each other in the
                    // query service and would strand a pending token.
                    let already_opening = app
                        .file_browser
                        .attachment
                        .values()
                        .any(|held| *held == owner)
                        || owner.is_some_and(|tab| {
                            app.reading
                                .attachment_opens
                                .values()
                                .any(|(held, _)| *held == tab)
                                || app.graph.attachment_open_pending(tab)
                        });
                    if already_opening {
                        app.status = "An attachment is already opening".into();
                    } else if let Some(api) = &app.flavite {
                        // Bind the view's live incarnation so a result from a
                        // replaced view is recognized as stale in observe.
                        let incarnation = owner.map_or(1, |tab| {
                            app.workspace
                                .tab(&tab)
                                .map_or(0, |view| view.interaction().incarnation)
                        });
                        match api.query_service().submit(
                            owner.map_or(QUERY_OWNER, TabId::raw),
                            incarnation,
                            0,
                            QueryPayload::AttachmentOpen { path },
                        ) {
                            Ok(token) => {
                                app.file_browser.attachment.insert(token, owner);
                                app.status = "Opening local file…".into();
                            }
                            Err(_) => app.status = "File service busy; try again".into(),
                        }
                    }
                }
                None => app.status = "File changed; refresh the browser".into(),
            }
        }
    }
    pump(app);
    iced::Task::none()
}

/// Only unconsumed keys from the focused browser participate; text fields
/// and editors retain their native keyboard handling.
pub fn keyboard_message(app: &App, event: &iced::keyboard::Event) -> Option<Message> {
    use iced::keyboard::{Event, Key, key::Named};
    if app.palette_open || app.picker.open || app.dialog.is_some() || app.strip_menu.is_some() {
        return None;
    }
    if app.context_mode.session.is_some() {
        return None;
    }
    let focused_tab = app.workspace.focused_tab();
    let owner = if app
        .workspace
        .tab(&focused_tab)
        .is_some_and(|view| matches!(view.kind(), flavite_workspace::ViewKind::Notes))
    {
        Some(focused_tab)
    } else {
        app.file_browser.focused?
    };
    if owner.is_some_and(|tab| app.workspace.focused_tab() != tab || !app.is_tab_visible(tab))
        || (owner.is_none() && !app.sidebar_open())
    {
        return None;
    }
    let Event::KeyPressed {
        key,
        modifiers,
        text,
        ..
    } = event
    else {
        return None;
    };
    if modifiers.control() || modifiers.alt() || modifiers.logo() {
        return None;
    }
    let rows = app.file_browser.rows(
        owner,
        owner.map_or(app.sidebar_filter.as_str(), |tab| app.notes_filter(tab)),
    );
    let selected = app
        .file_browser
        .views
        .get(&owner)
        .and_then(|view| view.selected.as_ref());
    let current = rows
        .iter()
        .position(|row| Some(&row.entry.path) == selected)
        .unwrap_or(0);
    let row = rows.get(current)?;
    let action = match key {
        Key::Named(Named::ArrowDown) => BrowserMessage::Select(
            owner,
            rows[(current + 1).min(rows.len() - 1)].entry.path.clone(),
        ),
        Key::Named(Named::ArrowUp) => {
            BrowserMessage::Select(owner, rows[current.saturating_sub(1)].entry.path.clone())
        }
        Key::Named(Named::Home) => BrowserMessage::Select(owner, rows[0].entry.path.clone()),
        Key::Named(Named::End) => BrowserMessage::Select(owner, rows.last()?.entry.path.clone()),
        Key::Named(Named::ArrowRight) if row.entry.directory && !row.expanded => {
            BrowserMessage::Toggle(owner, row.entry.path.clone())
        }
        Key::Named(Named::ArrowLeft) if row.expanded => {
            BrowserMessage::Toggle(owner, row.entry.path.clone())
        }
        Key::Named(Named::ArrowLeft) => {
            BrowserMessage::Select(owner, row.entry.path.parent()?.to_path_buf())
        }
        Key::Named(Named::Enter) => BrowserMessage::Open(owner, row.entry.path.clone()),
        _ => BrowserMessage::TypeAhead(owner, text.as_ref()?.to_string()),
    };
    Some(Message::FileBrowser(action))
}
