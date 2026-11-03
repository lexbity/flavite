//! Correlated read/choose/open/prepare/commit workflow for one directed occurrence.
use crate::app::{App, Message};
use crate::host::graph::GraphCommand;
use flavite_api::{
    ContextQueryResult, ExecutionResult, Invocation, LocalResult, OccurrenceDetail,
    OccurrenceSource, QueryOutcome, QueryPayload, RequestToken,
};
use flavite_workspace::{NoteLocator, TabId};
use iced::Element;
use iced::widget::{button, column, row, text};

#[derive(Debug, Clone)]
pub enum RemovalStage {
    Inspecting(RequestToken),
    Choosing(OccurrenceDetail),
    Opening(OccurrenceDetail),
    Preparing {
        request_id: u64,
        detail: OccurrenceDetail,
        buffer: u64,
        revision: u64,
    },
    Failed(String),
}
#[derive(Debug, Clone)]
pub struct RemovalWorkflow {
    pub tab: TabId,
    pub incarnation: u64,
    pub sequence: u64,
    pub stage: RemovalStage,
}

pub fn cancel(app: &mut App) {
    if let Some(workflow) = app.graph.removal.take()
        && let Some(client) = app.graph.removal_client
        && let Some(api) = &app.flavite
    {
        api.cancel_link_intent(client, workflow.sequence);
    }
}

pub fn inspect(
    app: &mut App,
    tab: TabId,
    incarnation: u64,
    key: flavite_graph::RenderKey,
    index: usize,
) {
    cancel(app);
    if !app.is_tab_visible(tab) || app.has_composition() {
        return;
    }
    if app
        .workspace
        .tab(&tab)
        .is_none_or(|s| s.interaction().incarnation != incarnation)
    {
        return;
    }
    let Some(state) = app
        .graph
        .state(tab)
        .filter(|s| !s.updating && !s.stale && s.geometry_key == Some(key))
    else {
        return;
    };
    let Some(occurrence) = state.occurrences.get(index).cloned() else {
        return;
    };
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    app.graph.removal_sequence = app.graph.removal_sequence.saturating_add(1);
    let sequence = app.graph.removal_sequence;
    match api.query_service().submit(
        tab.raw(),
        incarnation,
        sequence,
        QueryPayload::Occurrence {
            epoch: key.epoch,
            occurrence,
        },
    ) {
        Ok(token) => {
            app.graph.removal = Some(RemovalWorkflow {
                tab,
                incarnation,
                sequence,
                stage: RemovalStage::Inspecting(token),
            })
        }
        Err(_) => app.status = "Occurrence inspector is busy; try again".into(),
    }
}

pub fn observe(app: &mut App, outcome: &QueryOutcome) -> bool {
    if !matches!(outcome.payload, QueryPayload::Occurrence { .. }) {
        return false;
    }
    let Some(workflow) = app.graph.removal.as_mut() else {
        return true;
    };
    if !matches!(&workflow.stage, RemovalStage::Inspecting(token) if *token == outcome.token) {
        return true;
    }
    if app
        .workspace
        .tab(&workflow.tab)
        .is_none_or(|s| s.interaction().incarnation != workflow.incarnation)
    {
        cancel(app);
        return true;
    }
    workflow.stage = match &outcome.result {
        ContextQueryResult::Occurrence(detail) => RemovalStage::Choosing(detail.clone()),
        _ => RemovalStage::Failed(
            "The connection changed or could not be read. Refresh the chooser.".into(),
        ),
    };
    true
}

pub fn confirm(app: &mut App, tab: TabId, sequence: u64) {
    if app.has_composition() {
        app.status = "Finish composition before removing a connection".into();
        return;
    }
    let Some(workflow) = app
        .graph
        .removal
        .as_mut()
        .filter(|w| w.tab == tab && w.sequence == sequence)
    else {
        return;
    };
    let RemovalStage::Choosing(detail) = &workflow.stage else {
        return;
    };
    let detail = detail.clone();
    let Ok(locator) = NoteLocator::from_relative(&detail.occurrence.source_path) else {
        workflow.stage =
            RemovalStage::Failed("Source path cannot be represented; edit source".into());
        return;
    };
    workflow.stage = RemovalStage::Opening(detail);
    // Explicit removal opens its source so the user can inspect/undo the edit
    // and the document has a normal retained view lease.
    app.request_open(locator, None);
}

pub fn pump(app: &mut App) {
    let Some(workflow) = app.graph.removal.clone() else {
        return;
    };
    if app
        .workspace
        .tab(&workflow.tab)
        .is_none_or(|s| s.interaction().incarnation != workflow.incarnation)
    {
        cancel(app);
        return;
    }
    let RemovalStage::Opening(detail) = workflow.stage else {
        return;
    };
    if app.has_composition() {
        fail(app, "Finish composition and choose the occurrence again");
        return;
    }
    let Some(api) = app.flavite.as_ref() else {
        return;
    };
    let Some(view) = api.open_source_view(&detail.occurrence.source_path) else {
        if !app.notes.any_pending() {
            fail(app, "The source could not be opened; retry or locate it");
        }
        return;
    };
    let valid = match &detail.occurrence.source {
        OccurrenceSource::Buffer {
            buffer_id,
            revision,
        } => *buffer_id == view.buffer_id && *revision == view.revision,
        OccurrenceSource::Saved { content_hash } => {
            view.state == flavite_api::BufferStatus::Clean && &view.base_hash == content_hash
        }
    };
    if !valid || view.text.get(detail.start..detail.end) != Some(detail.exact_source.as_str()) {
        fail(app, "The source changed; refresh the occurrence chooser");
        return;
    }
    let Some(frame) = api
        .graph_frame()
        .filter(|f| f.render_key().epoch == detail.epoch)
    else {
        fail(app, "The index changed; refresh the occurrence chooser");
        return;
    };
    let Some(target) = frame
        .publication()
        .node_index(detail.occurrence.edge.to)
        .and_then(|i| frame.publication().record(i as usize))
    else {
        fail(app, "The destination is missing; refresh the chooser");
        return;
    };
    let client = *app
        .graph
        .removal_client
        .get_or_insert_with(|| api.new_link_client());
    let request_id = api.command_registry().next_request_id();
    let result = api.execute(Invocation::new(request_id, "link.prepare_remove", serde_json::json!({
        "intent": { "client": client, "sequence": workflow.sequence },
        "source": { "buffer_id": view.buffer_id, "revision": view.revision, "caret": detail.start },
        "start": detail.start, "end": detail.end, "expected_source": detail.exact_source,
        "target_path": target.path, "epoch": detail.epoch,
        "target_entity_id": detail.occurrence.edge.to.id,
        "target_is_attachment": detail.occurrence.edge.to.kind == flavite_graph::NodeKind::Attachment,
        "target_is_unresolved": detail.occurrence.edge.to.kind == flavite_graph::NodeKind::Unresolved,
    }), flavite_api::Origin::Gui));
    match result {
        ExecutionResult::Pending { request_id } => {
            app.operations
                .accept(request_id, "Prepare connection removal");
            if let Some(w) = &mut app.graph.removal {
                w.stage = RemovalStage::Preparing {
                    request_id,
                    detail,
                    buffer: view.buffer_id,
                    revision: view.revision,
                };
            }
        }
        _ => fail(
            app,
            "Connection removal was refused; refresh the chooser and check source permissions",
        ),
    }
}

pub fn completion(app: &mut App, completion: &flavite_api::OperationCompletion) -> bool {
    let Some(workflow) = app.graph.removal.clone() else {
        return false;
    };
    let RemovalStage::Preparing {
        request_id,
        detail: _,
        buffer,
        revision,
    } = workflow.stage
    else {
        return false;
    };
    if request_id != completion.request_id {
        return false;
    }
    let valid_view = app
        .workspace
        .tab(&workflow.tab)
        .is_some_and(|s| s.interaction().incarnation == workflow.incarnation);
    if !valid_view || app.has_composition() {
        fail(app, "Connection removal cancelled; the source view changed");
        return true;
    }
    let ExecutionResult::Local(LocalResult::Success(success)) = &completion.result else {
        fail(
            app,
            "Connection removal became stale or was refused; refresh the chooser",
        );
        return true;
    };
    let Some(plan) = success.data["plan_id"]
        .as_str()
        .and_then(|s| s.parse::<u128>().ok())
    else {
        fail(app, "Connection removal plan unavailable");
        return true;
    };
    let Some(api) = app.flavite.as_ref() else {
        return true;
    };
    if api
        .get_shared_buffer_view(buffer.into(), Some(flavite_buffer::Revision::new(revision)))
        .is_err()
    {
        fail(app, "The source changed; refresh the chooser");
        return true;
    }
    let result = api.execute(Invocation::new(api.command_registry().next_request_id(), "link.remove", serde_json::json!({ "intent": { "client": app.graph.removal_client, "sequence": workflow.sequence }, "plan_id": plan }), flavite_api::Origin::Gui));
    if result.is_success() {
        app.workspace.invalidate_writing_target(buffer);
        app.contexts.invalidate_all();
        app.status = "One connection occurrence removed · unsaved · Undo restores it".into();
        app.graph.removal = None;
    } else {
        fail(app, "Connection removal was refused; refresh the chooser");
    }
    true
}

fn fail(app: &mut App, message: &str) {
    app.status = message.into();
    if let Some(workflow) = &app.graph.removal
        && let Some(client) = app.graph.removal_client
        && let Some(api) = &app.flavite
    {
        api.cancel_link_intent(client, workflow.sequence);
    }
    if let Some(workflow) = &mut app.graph.removal {
        workflow.stage = RemovalStage::Failed(message.into());
    }
}

pub fn view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let mut content = column![].spacing(4);
    let Some(state) = app.graph.state(tab) else {
        return content.into();
    };
    if let Some(workflow) = app.graph.removal.as_ref().filter(|w| w.tab == tab) {
        match &workflow.stage {
            RemovalStage::Choosing(detail) => {
                content = content.push(
                    text(format!(
                        "{} → {} · {}",
                        detail.occurrence.source_path.display(),
                        detail.occurrence.target_raw,
                        if matches!(detail.occurrence.source, OccurrenceSource::Buffer { .. }) {
                            "Unsaved"
                        } else {
                            "Saved"
                        }
                    ))
                    .size(metrics.text_sm),
                );
                content = content.push(text(&detail.excerpt).size(metrics.text_sm));
                content = content.push(button("Remove this occurrence").on_press(
                    Message::GraphCommand {
                        tab,
                        command: GraphCommand::ConfirmRemoval(workflow.sequence),
                    },
                ));
            }
            RemovalStage::Failed(message) => {
                content = content.push(text(message).size(metrics.text_sm));
            }
            _ => {
                content =
                    content.push(text("Preparing connection occurrence…").size(metrics.text_sm));
            }
        }
        return content
            .push(button("Cancel removal").on_press(Message::GraphCommand {
                tab,
                command: GraphCommand::CancelRemoval,
            }))
            .into();
    }
    let Some(selected) = state.selection else {
        return content.into();
    };
    let Some(key) = state.geometry_key else {
        return content.into();
    };
    let incarnation = app
        .workspace
        .tab(&tab)
        .map_or(0, |s| s.interaction().incarnation);
    let related: Vec<_> = state
        .occurrences
        .iter()
        .enumerate()
        .filter(|(_, occurrence)| {
            occurrence.edge.from == selected.id || occurrence.edge.to == selected.id
        })
        .collect();
    content = content.push(
        text(format!(
            "{} directed occurrence(s) · choose a source span",
            related.len()
        ))
        .size(metrics.text_sm),
    );
    for (index, occurrence) in related.iter().skip(state.occurrence_offset).take(12) {
        content = content.push(
            row![
                text(format!(
                    "{} → {} · {}",
                    occurrence.source_path.display(),
                    occurrence.target_raw,
                    if matches!(occurrence.source, OccurrenceSource::Buffer { .. }) {
                        "Unsaved"
                    } else {
                        "Saved"
                    }
                ))
                .size(metrics.text_sm),
                button("Inspect / remove").on_press_maybe(
                    (!state.updating && !state.stale).then_some(Message::InspectGraphOccurrence {
                        tab,
                        incarnation,
                        key,
                        index: *index
                    })
                )
            ]
            .spacing(4),
        );
    }
    if related.len() > 12 {
        content = content.push(row![
            button("Previous occurrences").on_press_maybe((state.occurrence_offset > 0).then_some(
                Message::GraphCommand {
                    tab,
                    command: GraphCommand::PreviousOccurrences
                }
            )),
            button("Next occurrences").on_press_maybe(
                (state.occurrence_offset + 12 < related.len()).then_some(Message::GraphCommand {
                    tab,
                    command: GraphCommand::NextOccurrences
                })
            )
        ]);
    }
    content.into()
}
