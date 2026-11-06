//! Correlated read resources; hidden tabs retain logical state, not read caches.
use crate::app::App;
use flavite_api::{ContextQueryResult, QueryOutcome, QueryPayload, ReadSnapshot, RequestToken};
use flavite_workspace::{NoteMode, TabId, ViewKind};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};
#[derive(Default)]
pub struct ReadingHost {
    pub notes: BTreeMap<TabId, ReadEntry>,
    pub navigation: BTreeMap<flavite_workspace::NoteLocator, String>,
    pub anchor_requests: BTreeMap<RequestToken, TabId>,
    pub attachment_opens: BTreeMap<RequestToken, (TabId, Arc<ReadSnapshot>)>,
    pub images: BTreeMap<[u8; 32], iced::widget::image::Handle>,
    pub source_previews: BTreeSet<TabId>,
    pub property_drafts: BTreeMap<(TabId, String), String>,
    /// Per-rendered-block offsets in content coordinates, for content anchors.
    pub blocks: BTreeMap<(TabId, usize), iced::Rectangle>,
    pub controls: BTreeMap<(TabId, usize, &'static str, usize), iced::Rectangle>,
    pub scroll: BTreeMap<TabId, f32>,
    pub pending_anchor: BTreeMap<TabId, usize>,
}
pub struct ReadEntry {
    pub stamp: (u64, u64, u64),
    pub pending: Option<RequestToken>,
    pub document: Option<Arc<ReadSnapshot>>,
    pub error: Option<String>,
}
pub fn is_edit(app: &App, tab: TabId) -> bool {
    matches!(app.workspace.tab(&tab).map(|s| s.kind()), Some(ViewKind::Note(n)) if n.mode() == NoteMode::Edit)
}
pub fn current(app: &App, doc: &ReadSnapshot) -> bool {
    !refresh_needed(app, doc)
        && app
            .flavite
            .as_ref()
            .and_then(|api| api.read_catalog_version())
            == Some(doc.catalog_version)
}
/// An in-progress catalog publication is not evidence that cached source
/// bytes changed. Retain the reading surface; identity actions still use the
/// stricter `current` check until a complete publication is available.
fn refresh_needed(app: &App, doc: &ReadSnapshot) -> bool {
    let Some(api) = &app.flavite else {
        return true;
    };
    if api
        .read_catalog_version()
        .is_some_and(|version| version != doc.catalog_version)
    {
        return true;
    }
    let open = api.open_source_view(&doc.path);
    let matches = match (doc.buffer, open) {
        (Some((id, revision)), Some(view)) => view.buffer_id == id && view.revision == revision,
        (None, None) => true,
        _ => false,
    };
    !matches
        || doc.embeds.values().any(|e| match e {
            flavite_api::EmbedContent::Note(doc) => refresh_needed(app, doc),
            _ => false,
        })
}
/// Shared availability predicate for pointer and native structure actions.
pub fn can_edit(app: &App, doc: &ReadSnapshot) -> bool {
    !app.has_composition()
        && source_current(app, doc)
        && app.flavite.as_ref().is_some_and(|api| {
            !api.is_read_only()
                && api.open_source_view(&doc.path).is_some_and(|view| {
                    matches!(
                        view.state,
                        flavite_buffer::state::BufferState::Clean
                            | flavite_buffer::state::BufferState::Dirty
                    )
                })
        })
}
fn source_current(app: &App, doc: &ReadSnapshot) -> bool {
    app.flavite
        .as_ref()
        .and_then(|api| api.open_source_view(&doc.path))
        .is_some_and(|view| doc.buffer == Some((view.buffer_id, view.revision)))
}
pub fn unsaved(app: &App, doc: &ReadSnapshot) -> bool {
    app.flavite
        .as_ref()
        .and_then(|api| api.open_source_view(&doc.path))
        .map_or(doc.unsaved, |view| {
            view.state != flavite_buffer::state::BufferState::Clean
        })
}
/// A save/index publication may replace a rendering without changing source
/// bytes. Textual actions use exact buffer/revision/hash and the root source,
/// rather than rejecting a valid task click during a durability receipt.
pub fn bound_edit(app: &App, tab: TabId, doc: &ReadSnapshot) -> bool {
    if !app.is_tab_visible(tab)
        || app.picker.open
        || app.palette_open
        || app.strip_menu.is_some()
        || !source_current(app, doc)
    {
        return false;
    }
    fn contains(root: &ReadSnapshot, doc: &ReadSnapshot) -> bool {
        (root.root_path==doc.root_path && root.path==doc.path && root.buffer==doc.buffer && root.content_hash==doc.content_hash) || root.embeds.values().any(|embed|matches!(embed,flavite_api::EmbedContent::Note(child) if contains(child,doc)))
    }
    document(app, tab).is_some_and(|root| contains(root, doc))
}
pub fn document(app: &App, tab: TabId) -> Option<&Arc<ReadSnapshot>> {
    if let Some(crate::host::context::ToolState::Ready(
        crate::host::context::ToolContent::Document(doc),
    )) = app.contexts.state(&tab)
    {
        return Some(doc);
    }
    app.reading.notes.get(&tab)?.document.as_ref()
}
pub fn sync(app: &mut App) {
    let navigation = app.reading.navigation.clone();
    for (locator, anchor) in navigation {
        let target = app.workspace.tabs().find_map(|(tab, session)| matches!(session.kind(), ViewKind::Note(n) if *n.locator() == locator && n.load() == flavite_workspace::NoteLoad::Ready).then_some(*tab));
        if let Some(tab) = target
            && let Some(view) = app.note_editor_view(tab)
            && let Some(api) = &app.flavite
            && let Ok(token) = api.query_service().submit(
                tab.raw(),
                app.notes.incarnation(tab).unwrap_or(0),
                view.revision,
                QueryPayload::Anchor {
                    buffer_id: view.buffer_id,
                    revision: view.revision,
                    anchor,
                },
            )
        {
            app.reading.anchor_requests.insert(token, tab);
            app.reading.navigation.remove(&locator);
        }
    }

    let tabs: Vec<_> = app
        .workspace
        .tabs()
        .filter_map(|(tab, session)| {
            if matches!(session.kind(), ViewKind::Note(n) if n.mode() == NoteMode::Read)
                && app.is_tab_visible(*tab)
            {
                app.note_editor_view(*tab).map(|v| {
                    (
                        *tab,
                        session.interaction().incarnation,
                        v.buffer_id,
                        v.revision,
                    )
                })
            } else {
                None
            }
        })
        .collect();
    app.reading
        .notes
        .retain(|tab, _| tabs.iter().any(|(t, ..)| tab == t));
    app.reading
        .source_previews
        .retain(|tab| app.workspace.tab(tab).is_some());
    let mut hashes = BTreeSet::new();
    fn image_hashes(doc: &ReadSnapshot, out: &mut BTreeSet<[u8; 32]>) {
        for embed in doc.embeds.values() {
            match embed {
                flavite_api::EmbedContent::Note(child) => image_hashes(child, out),
                flavite_api::EmbedContent::Image(image) => {
                    out.insert(image.hash);
                }
                _ => {}
            }
        }
    }
    for (tab, _) in app.workspace.tabs() {
        if let Some(doc) = document(app, *tab) {
            image_hashes(doc, &mut hashes);
        }
    }
    app.reading.images.retain(|hash, _| hashes.contains(hash));
    let mut live_sources = BTreeSet::new();
    fn sources(doc: &Arc<ReadSnapshot>, set: &mut BTreeSet<usize>) {
        set.insert(Arc::as_ptr(doc) as usize);
        for embed in doc.embeds.values() {
            if let flavite_api::EmbedContent::Note(child) = embed {
                sources(child, set);
            }
        }
    }
    for (tab, _) in app.workspace.tabs() {
        if app.is_tab_visible(*tab)
            && let Some(doc) = document(app, *tab)
        {
            sources(doc, &mut live_sources);
        }
    }
    app.reading
        .controls
        .retain(|(_, source, _, _), _| live_sources.contains(source));
    app.reading
        .blocks
        .retain(|(tab, _), _| app.workspace.tab(tab).is_some());
    app.reading
        .property_drafts
        .retain(|(tab, _), _| app.workspace.tab(tab).is_some());
    app.reading
        .pending_anchor
        .retain(|tab, _| app.workspace.tab(tab).is_some());

    let Some(api) = &app.flavite else {
        return;
    };
    for (tab, incarnation, buffer, revision) in tabs {
        let stamp = (incarnation, buffer, revision);
        if app.reading.notes.get(&tab).is_some_and(|entry| {
            entry.stamp == stamp
                && entry
                    .document
                    .as_ref()
                    .is_none_or(|doc| !refresh_needed(app, doc))
        }) {
            continue;
        }
        let Some(locator) = app.notes.locator_of(tab) else {
            continue;
        };
        match api.query_service().submit(
            tab.raw(),
            incarnation,
            revision,
            QueryPayload::Read {
                path: locator.components().to_vec(),
                expected_buffer: Some((buffer, revision)),
            },
        ) {
            Ok(token) => {
                app.reading.blocks.retain(|(t, _), _| *t != tab);
                app.reading.pending_anchor.entry(tab).or_insert_with(|| {
                    app.workspace
                        .tab(&tab)
                        .map_or(0, |s| s.interaction().viewport_anchor)
                });
                app.reading.notes.insert(
                    tab,
                    ReadEntry {
                        stamp,
                        pending: Some(token),
                        document: None,
                        error: None,
                    },
                );
            }
            Err(_) => {
                app.status = "Reading queued; query service busy".into();
            }
        }
    }
}
pub fn observe(app: &mut App, outcome: &QueryOutcome) -> bool {
    if matches!(outcome.payload, QueryPayload::Anchor { .. }) {
        if let Some(tab) = app.reading.anchor_requests.remove(&outcome.token)
            && app.notes.incarnation(tab) == Some(outcome.token.incarnation)
        {
            if let ContextQueryResult::Anchor {
                buffer_id,
                revision,
                byte: Some(byte),
            } = outcome.result
            {
                if let Some(view) = app
                    .note_editor_view(tab)
                    .filter(|v| v.buffer_id == buffer_id && v.revision == revision)
                {
                    let _ = app.workspace.set_content_anchor(tab, byte, &view.text);
                    app.reading.pending_anchor.insert(tab, byte);
                } else {
                    app.status = "Anchor source changed; open the link again".into();
                }
            } else {
                app.status = "Anchor unavailable in current note".into();
            }
        }
        return true;
    }

    if let QueryPayload::AttachmentOpen { .. } = &outcome.payload {
        if let Some((tab, doc)) = app.reading.attachment_opens.remove(&outcome.token) {
            if bound(app, tab, &doc) {
                match &outcome.result {
                    ContextQueryResult::AttachmentOpen(Ok(path)) => {
                        app.status = if spawn_os(path.as_os_str()).is_ok() {
                            "Opened local attachment".into()
                        } else {
                            "Local attachment could not be opened".into()
                        };
                    }
                    _ => {
                        app.status = "Attachment unavailable; locate a supported local file".into()
                    }
                }
            }
            return true;
        }
        // D-02: a graph-activated attachment inspector reports on its
        // owning graph tab; a replaced/removed tab drops silently.
        if let Some(tab) = app.graph.take_attachment_open(&outcome.token)
            && app.workspace.tab(&tab).is_some_and(|session| {
                session.interaction().incarnation == outcome.token.incarnation
            })
        {
            match &outcome.result {
                ContextQueryResult::AttachmentOpen(Ok(path)) => {
                    app.status = if spawn_os(path.as_os_str()).is_ok() {
                        "Opened local attachment".into()
                    } else {
                        "Local attachment could not be opened".into()
                    };
                }
                _ => app.status = "Attachment unavailable; locate a supported local file".into(),
            }
        }
        return true;
    }
    if !matches!(
        outcome.payload,
        QueryPayload::Read {
            expected_buffer: Some(_),
            ..
        }
    ) {
        return false;
    }
    let tab = TabId::from_raw(outcome.token.context_tab);
    let doc = match &outcome.result {
        ContextQueryResult::Document(doc) if current(app, doc) => Some(doc.clone()),
        _ => None,
    };
    if matches!(outcome.result, ContextQueryResult::Updating)
        || (matches!(outcome.result, ContextQueryResult::Document(_)) && doc.is_none())
    {
        if app
            .reading
            .notes
            .get(&tab)
            .is_some_and(|entry| entry.pending == Some(outcome.token))
        {
            app.reading.notes.remove(&tab);
        }
        return true;
    }
    if let Some(doc) = &doc {
        cache_images(app, doc);
    }
    if let Some(entry) = app.reading.notes.get_mut(&tab) {
        if entry.pending != Some(outcome.token) {
            return true;
        }
        entry.pending = None;
        match &outcome.result {
            ContextQueryResult::Document(_) if doc.is_some() => entry.document = doc,
            ContextQueryResult::Failed { message } => entry.error = Some(message.clone()),
            _ => entry.error = Some("Source changed; retry reading".into()),
        }
    }
    true
}

/// A callback must still belong to the tab's exact mounted reading resource.
/// Is `doc` the (possibly embedded) document this tab is actually showing?
fn snapshot_in_tab(app: &App, tab: TabId, doc: &Arc<ReadSnapshot>) -> bool {
    if !app.is_tab_visible(tab) || app.picker.open || app.palette_open || app.strip_menu.is_some() {
        return false;
    }
    fn contains(root: &Arc<ReadSnapshot>, doc: &Arc<ReadSnapshot>) -> bool {
        Arc::ptr_eq(root, doc)
            || root.embeds.values().any(
                |e| matches!(e, flavite_api::EmbedContent::Note(child) if contains(child, doc)),
            )
    }
    document(app, tab).is_some_and(|root| contains(root, doc))
}

/// Strict admission for guarded edits, bounds, and presentation: the
/// snapshot must be this tab's document *and* match the current catalog
/// version, so an in-progress reindex never accepts revision-guarded
/// edits against stale source.
pub fn bound(app: &App, tab: TabId, doc: &Arc<ReadSnapshot>) -> bool {
    snapshot_in_tab(app, tab, doc) && current(app, doc)
}

/// Navigation admission (`ReadActivate`/`OutlineNavigate`): pointer identity
/// and visibility only. Following a link whose target path is recorded in
/// the displayed snapshot is safe regardless of catalog freshness (D-08:
/// navigation routes canonically); guarded edits keep the strict `bound`.
pub fn bound_navigation(app: &App, tab: TabId, doc: &Arc<ReadSnapshot>) -> bool {
    snapshot_in_tab(app, tab, doc)
}
pub fn handle(app: &mut App, message: crate::app::Message) -> iced::Task<crate::app::Message> {
    use crate::app::Message;
    match message {
        Message::ToggleNoteMode(tab) => {
            if app.has_composition() || !app.is_tab_visible(tab) {
                return iced::Task::none();
            }
            let Some(ViewKind::Note(note)) = app.workspace.tab(&tab).map(|s| s.kind()) else {
                return iced::Task::none();
            };
            let mode = if note.mode() == NoteMode::Edit {
                NoteMode::Read
            } else {
                NoteMode::Edit
            };
            if mode == NoteMode::Read {
                let anchor = app
                    .workspace
                    .tab(&tab)
                    .map_or(0, |s| s.interaction().viewport_anchor);
                app.reading.pending_anchor.insert(tab, anchor);
                app.reading.blocks.retain(|(t, _), _| *t != tab);
            }
            app.dispatch_layout(|revision| flavite_workspace::LayoutAction::SetNoteMode {
                expected_revision: revision,
                tab,
                mode,
            });
            sync(app);
        }
        Message::RetryReading(tab) => {
            app.reading.notes.remove(&tab);
            sync(app);
        }
        Message::ToggleSourcePreview(tab) => {
            if !app.reading.source_previews.remove(&tab) {
                app.reading.source_previews.insert(tab);
            }
        }
        Message::ReadControlBounds {
            tab,
            document: doc,
            role,
            start,
            rect,
        } => {
            if bound(app, tab, &doc) {
                app.reading
                    .controls
                    .insert((tab, Arc::as_ptr(&doc) as usize, role, start), rect);
            }
        }
        Message::ReadBlockBounds {
            tab,
            hash,
            start,
            rect,
        } => {
            if document(app, tab).is_none_or(|d| d.content_hash != hash) {
                return iced::Task::none();
            }
            app.reading.blocks.insert((tab, start), rect);
            if let Some(anchor) = app.reading.pending_anchor.get(&tab).copied() {
                let target = document(app, tab)
                    .and_then(|doc| {
                        doc.document
                            .nodes
                            .iter()
                            .find(|n| n.range.start <= anchor && n.range.end > anchor)
                            .or_else(|| {
                                doc.document
                                    .nodes
                                    .iter()
                                    .rev()
                                    .find(|n| n.range.start <= anchor)
                            })
                    })
                    .map(|n| n.range.start);
                if target == Some(start) {
                    // Layout bounds are in content space; subtract the first
                    // mounted block, preserving the semantic content anchor.
                    if let Some(origin) = app
                        .reading
                        .blocks
                        .iter()
                        .filter(|((t, _), _)| *t == tab)
                        .min_by_key(|((_, byte), _)| *byte)
                        .map(|(_, r)| r.y)
                    {
                        let y = (rect.y - origin).max(0.0);
                        app.reading.pending_anchor.remove(&tab);
                        return iced::widget::operation::scroll_to(
                            crate::read::renderer::scroll_id(tab),
                            iced::widget::scrollable::AbsoluteOffset {
                                x: None,
                                y: Some(y),
                            },
                        );
                    }
                }
            }
        }
        Message::ReadScrolled { tab, offset } => {
            app.reading.scroll.insert(tab, offset);
            let Some(doc) = document(app, tab).cloned() else {
                return iced::Task::none();
            };
            let origin = app
                .reading
                .blocks
                .iter()
                .filter(|((t, _), _)| *t == tab)
                .min_by_key(|((_, byte), _)| *byte)
                .map(|(_, r)| r.y)
                .unwrap_or(0.0);
            if let Some(byte) = app
                .reading
                .blocks
                .iter()
                .filter(|((t, _), r)| *t == tab && r.y + r.height > origin + offset)
                .min_by_key(|((_, byte), _)| *byte)
                .map(|((_, byte), _)| *byte)
            {
                let _ = app.workspace.set_content_anchor(tab, byte, &doc.text);
            }
        }
        Message::ReadActivate {
            tab,
            document: doc,
            start,
        } => {
            if !bound_navigation(app, tab, &doc) {
                app.status = "Read link: the source changed; refresh before opening".into();
                return iced::Task::none();
            }
            match doc.links.get(&start).cloned() {
                Some(flavite_api::ReadLink::Note { path, anchor }) => {
                    if let Ok(locator) = flavite_workspace::NoteLocator::from_relative(&path) {
                        app.open_intent(crate::host::open_intents::OpenIntent::reuse(
                            locator.clone(),
                            Some(tab),
                        ));
                        if let Some(anchor) = anchor {
                            app.reading.navigation.insert(locator, anchor);
                            sync(app);
                        }
                    }
                }
                Some(flavite_api::ReadLink::Anchor(anchor)) => {
                    if let Some(byte) = anchor_byte(&doc, &anchor) {
                        app.reading.pending_anchor.insert(tab, byte);
                    } else {
                        app.status = "Read link: this anchor is missing from the note".into();
                    }
                }
                Some(flavite_api::ReadLink::External(url))
                    if flavite_api::allowed_external_url(&url) =>
                {
                    return open_external(url);
                }
                Some(flavite_api::ReadLink::Attachment(path)) => {
                    if app
                        .reading
                        .attachment_opens
                        .values()
                        .any(|(owner, _)| *owner == tab)
                    {
                        app.status = "An attachment is already opening".into();
                        return iced::Task::none();
                    }
                    if let Some(api) = &app.flavite {
                        match api.query_service().submit(
                            tab.raw(),
                            app.workspace
                                .tab(&tab)
                                .map_or(0, |s| s.interaction().incarnation),
                            0,
                            QueryPayload::AttachmentOpen { path },
                        ) {
                            Ok(token) => {
                                app.reading.attachment_opens.insert(token, (tab, doc));
                                app.status = "Opening local attachment…".into();
                            }
                            Err(_) => app.status = "Attachment service busy; try again".into(),
                        }
                    }
                }
                Some(flavite_api::ReadLink::Unavailable(reason)) => app.status = reason,
                _ => app.status = "Unsupported link; edit source".into(),
            }
        }
        Message::OutlineNavigate {
            tab,
            document: doc,
            byte,
        } => {
            if !bound_navigation(app, tab, &doc)
                || !doc.document.outline.iter().any(|h| h.byte == byte)
            {
                app.status = "Outline changed; refresh before navigating".into();
                return iced::Task::none();
            }
            // FollowSource targets that exact document view; other bindings
            // explicitly open/activate a source using canonical routing.
            let target = match app.workspace.binding_of(&tab) {
                Some(flavite_workspace::ContextBinding::FollowSource { tab: source }) => Some(*source),
                _ => app.workspace.tabs().find_map(|(tab, s)| matches!(s.kind(), ViewKind::Note(n) if n.locator().components().iter().collect::<PathBuf>() == doc.path).then_some(*tab)),
            };
            if let Some(target) = target {
                if app.group_of(target).is_some() {
                    app.dispatch_layout(|revision| flavite_workspace::LayoutAction::ActivateTab {
                        expected_revision: revision,
                        tab: target,
                    });
                    let _ = app.workspace.set_content_anchor(target, byte, &doc.text);
                    app.reading.pending_anchor.insert(target, byte);
                }
            } else if let Ok(locator) = flavite_workspace::NoteLocator::from_relative(&doc.path) {
                app.open_intent(crate::host::open_intents::OpenIntent::reuse(
                    locator,
                    Some(tab),
                ));
            }
        }
        Message::StructureEdit {
            tab,
            document: doc,
            command,
            args,
        } => {
            if !bound_edit(app, tab, &doc)
                || app.has_composition()
                || !matches!(command, "task.toggle" | "property.set")
            {
                app.status = "Source changed; refresh before editing".into();
                return iced::Task::none();
            }
            let Some((buffer, revision)) = doc.buffer else {
                app.status = "Open source before editing".into();
                return iced::Task::none();
            };
            if args["buffer_id"].as_u64() != Some(buffer)
                || args["revision"].as_u64() != Some(revision)
            {
                return iced::Task::none();
            }
            let Some(api) = &app.flavite else {
                return iced::Task::none();
            };
            let request_id = api.command_registry().next_request_id();
            let result = api.execute(flavite_api::Invocation::new(
                request_id,
                command,
                args,
                flavite_api::Origin::Gui,
            ));
            let success = result.is_success();
            app.status = match result {
                flavite_api::ExecutionResult::Local(flavite_api::LocalResult::Success(_)) => {
                    "Source changed · unsaved".into()
                }
                flavite_api::ExecutionResult::ActionRequired { code, data, .. } => format!(
                    "{code}: {}",
                    data.map_or_else(|| "Edit source".into(), |v| v.to_string())
                ),
                flavite_api::ExecutionResult::Rejected(rejected) => {
                    format!("Edit refused: {:?}", rejected.reason)
                }
                flavite_api::ExecutionResult::Pending { request_id } => {
                    app.operations.accept(request_id, command);
                    "Edit pending".into()
                }
                _ => "Edit failed; refresh or edit source".into(),
            };
            if success {
                app.workspace.invalidate_writing_target(buffer);
                app.contexts.invalidate_all();
                app.reading.notes.remove(&tab);
                app.reading.property_drafts.retain(|(t, _), _| *t != tab);
                app.reconcile_contexts();
                sync(app);
            }
        }
        Message::PropertyDraft {
            tab,
            document: doc,
            key,
            value,
        } => {
            if bound_edit(app, tab, &doc)
                && value.len() <= 8192
                && flavite_api::inspect_properties(&doc.text)
                    .is_ok_and(|props| props.iter().any(|p| p.key == key))
            {
                app.reading.property_drafts.insert((tab, key), value);
            }
        }
        Message::PropertyApply {
            tab,
            document: doc,
            key,
        } => {
            if !bound_edit(app, tab, &doc) {
                return iced::Task::none();
            }
            let Some((buffer, revision)) = doc.buffer else {
                return iced::Task::none();
            };
            let Some(property) = flavite_api::inspect_properties(&doc.text)
                .ok()
                .and_then(|props| props.into_iter().find(|p| p.key == key))
            else {
                return iced::Task::none();
            };
            let Some(draft) = app.reading.property_drafts.get(&(tab, key.clone())) else {
                return iced::Task::none();
            };
            let value = match property.value {
                flavite_api::ScalarValue::Text(_) => flavite_api::ScalarValue::Text(draft.clone()),
                flavite_api::ScalarValue::Number(_) => {
                    flavite_api::ScalarValue::Number(draft.clone())
                }
                flavite_api::ScalarValue::Bool(_) => match draft.as_str() {
                    "true" => flavite_api::ScalarValue::Bool(true),
                    "false" => flavite_api::ScalarValue::Bool(false),
                    _ => {
                        app.status = "Boolean values must be true or false".into();
                        return iced::Task::none();
                    }
                },
                flavite_api::ScalarValue::Null => flavite_api::ScalarValue::Null,
            };
            return handle(
                app,
                Message::StructureEdit {
                    tab,
                    document: doc,
                    command: "property.set",
                    args: serde_json::json!({ "buffer_id": buffer, "revision": revision, "key": key, "value": value }),
                },
            );
        }
        Message::ExternalOpened(result) => {
            app.status = if result.is_ok() {
                "Opened external link".into()
            } else {
                "External link could not be opened".into()
            };
        }
        _ => {}
    }
    app.refresh_semantics();
    scroll_pending(app)
}
fn anchor_byte(doc: &ReadSnapshot, anchor: &str) -> Option<usize> {
    doc.document
        .outline
        .iter()
        .find(|h| h.title.eq_ignore_ascii_case(anchor))
        .map(|h| h.byte)
}
fn spawn_os(target: &std::ffi::OsStr) -> Result<(), String> {
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(target).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open")
        .arg("--")
        .arg(target)
        .spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("rundll32.exe")
        .arg("url.dll,FileProtocolHandler")
        .arg(target)
        .spawn();
    result.map(|_| ()).map_err(|_| "OS open failed".into())
}
fn open_external(url: String) -> iced::Task<crate::app::Message> {
    iced::Task::perform(
        async move { spawn_os(std::ffi::OsStr::new(&url)) },
        crate::app::Message::ExternalOpened,
    )
}
struct ImageOwner {
    pixels: Arc<[u8]>,
    _document: Arc<ReadSnapshot>,
}
impl AsRef<[u8]> for ImageOwner {
    fn as_ref(&self) -> &[u8] {
        &self.pixels
    }
}
pub fn cache_images(app: &mut App, doc: &Arc<ReadSnapshot>) {
    for embedded in doc.embeds.values() {
        match embedded {
            flavite_api::EmbedContent::Note(child) => cache_images(app, child),
            flavite_api::EmbedContent::Image(pixels) => {
                app.reading.images.entry(pixels.hash).or_insert_with(|| {
                    iced::widget::image::Handle::from_rgba(
                        pixels.width,
                        pixels.height,
                        iced::advanced::graphics::core::Bytes::from_owner(ImageOwner {
                            pixels: pixels.rgba.clone(),
                            _document: doc.clone(),
                        }),
                    )
                });
            }
            _ => {}
        }
    }
}

pub fn scroll_pending(app: &mut App) -> iced::Task<crate::app::Message> {
    let mut tasks = Vec::new();
    for (tab, anchor) in app.reading.pending_anchor.clone() {
        let Some(doc) = document(app, tab) else {
            continue;
        };
        let target = doc
            .document
            .nodes
            .iter()
            .find(|n| n.range.start <= anchor && anchor < n.range.end)
            .or_else(|| {
                doc.document
                    .nodes
                    .iter()
                    .rev()
                    .find(|n| n.range.start <= anchor)
            })
            .map(|n| n.range.start);
        if let Some(target) = target
            && let Some(rect) = app.reading.blocks.get(&(tab, target))
        {
            let origin = app
                .reading
                .blocks
                .iter()
                .filter(|((t, _), _)| *t == tab)
                .min_by_key(|((_, byte), _)| *byte)
                .map(|(_, rect)| rect.y)
                .unwrap_or(rect.y);
            tasks.push(iced::widget::operation::scroll_to(
                crate::read::renderer::scroll_id(tab),
                iced::widget::scrollable::AbsoluteOffset {
                    x: None,
                    y: Some((rect.y - origin).max(0.0)),
                },
            ));
            app.reading.pending_anchor.remove(&tab);
        }
    }
    iced::Task::batch(tasks)
}
pub fn observe_control<'a>(
    tab: TabId,
    doc: &Arc<ReadSnapshot>,
    role: &'static str,
    start: usize,
    element: impl Into<iced::Element<'a, crate::app::Message>>,
) -> iced::Element<'a, crate::app::Message> {
    let doc = doc.clone();
    crate::workspace::bounds::observe(element, move |rect| {
        crate::app::Message::ReadControlBounds {
            tab,
            document: doc.clone(),
            role,
            start,
            rect,
        }
    })
}
