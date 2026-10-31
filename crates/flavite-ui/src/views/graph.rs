//! The graph tab view (RFC-0003 §23, W07).
//!
//! A first-class workspace view of canonical note/link data: selectable,
//! movable, and operable by pointer, keyboard, and assistive technology.
//! Both renderers (instanced wgpu and the software canvas) mount the same
//! [`RenderGeometry`](crate::graph::geometry::RenderGeometry) and publish
//! the same typed [`GraphMessage`](crate::graph::renderer::GraphMessage)
//! set, so a capture on either backend proves the same behavior.
//!
//! Persistent affordances (§13.1/D-05): the header always shows the base
//! counts, provenance, and the writing-target line, so a double-click's
//! destination is visible before it happens. A missing target gets an
//! explanation and a repair action, never a hidden fallback.

use std::sync::Arc;

use crate::app::{App, Message};
use crate::graph::canvas::GraphCanvas;
use crate::graph::geometry::{HitNode, RenderGeometry};
use crate::graph::renderer::{GraphMessage, GraphProgram};
use crate::host::graph::{self, GraphCommand, GraphTabState};
use crate::style;
use crate::views::empty_state;
use crate::workspace::{bounds as bounds_obs, tab_strip};
use flavite_api::GraphServiceStatus;
use flavite_workspace::{TabId, ViewKind};
use iced::widget::{button, column, container, row, rule, scrollable, text};
use iced::{Element, Length, Padding, alignment};

/// Rows rendered in list/accessibility mode before truncation is noted.
const LIST_ROW_BUDGET: usize = 50;

/// Render one graph tab for the active view slot.
pub fn view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let Some(session) = app.workspace.tab(&tab) else {
        return empty_state("Empty", "This tab has no content.");
    };
    let ViewKind::Graph(_) = session.kind() else {
        return empty_state("Graph", "This tab is not a graph view.");
    };
    let incarnation = session.interaction().incarnation;
    let Some(state) = app.graph.state(tab) else {
        return empty_state("Graph", "The graph view is starting…");
    };

    let header = header(app, tab, state);
    let body: Element<'a, Message> = if state.filter.expanded {
        scrollable(crate::graph::filters::view(app, tab))
            .height(Length::Fill)
            .into()
    } else {
        match &state.geometry {
            None => unavailable(app, tab, state),
            Some(geometry) if state.list_mode => list_body(app, tab, state, geometry),
            Some(geometry) => visual_body(app, tab, incarnation, state, geometry),
        }
    };
    column![
        text(app.writing_target_label()).size(style::TEXT_SM),
        scrollable(header).height(Length::FillPortion(2)),
        rule::horizontal(1),
        container(body)
            .width(Length::Fill)
            .height(Length::FillPortion(5))
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// Toolbar: counts + provenance, explicit selection label, writing-target
/// affordance, and the view's real controls.
fn header<'a>(app: &'a App, tab: TabId, state: &GraphTabState) -> Element<'a, Message> {
    let counts = state
        .scope_result
        .as_ref()
        .map(|scope| {
            format!(
                "{} base notes · {} matching notes · {} rendered · {} omitted",
                scope.total_base_notes,
                scope.matching_notes,
                state.geometry.as_ref().map_or(0, |g| g.node_count()),
                scope
                    .nodes
                    .len()
                    .saturating_sub(state.geometry.as_ref().map_or(0, |g| g.node_count()))
            )
        })
        .unwrap_or_else(|| "Graph: updating scope".into());
    let provenance = crate::graph::provenance::label(state);
    let selection = state
        .selection
        .and_then(|hit| graph::record_for(app, hit.id).map(|record| (hit, record)))
        .map(|(hit, record)| {
            format!(
                "Selected: {} [{}] {}",
                record.title,
                if hit.kind == flavite_graph::NodeKind::Attachment {
                    "attachment"
                } else {
                    "note"
                },
                if state
                    .scope_result
                    .as_ref()
                    .is_some_and(|scope| !scope.nodes.contains(&hit.id))
                {
                    format!("{} · selected outside current scope", record.path)
                } else {
                    record.path
                }
            )
        })
        .unwrap_or_else(|| "Selected: none — click a node".into());
    let stale = if state.stale || state.updating {
        text("frame stale — insertion disabled").size(style::TEXT_SM)
    } else {
        text("").size(style::TEXT_SM)
    };
    let chip = |label: &'static str, command: GraphCommand, enabled: bool| {
        button(text(label).size(style::TEXT_SM))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press_maybe(enabled.then_some(Message::GraphCommand { tab, command }))
    };
    container(
        column![
            row![
                text(counts).size(style::TEXT).width(Length::Fill),
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fixed(0.0)),
                chip(
                    if state.frozen {
                        "Resume layout"
                    } else {
                        "Freeze layout"
                    },
                    GraphCommand::ToggleFreeze,
                    true
                ),
                chip(
                    if state.list_mode {
                        "Show graph"
                    } else {
                        "Show list"
                    },
                    GraphCommand::ToggleList,
                    true
                ),
                chip("Fit", GraphCommand::FitView, state.geometry.is_some()),
            ]
            .spacing(style::SPACING)
            .align_y(alignment::Vertical::Center),
            row![
                text(selection)
                    .size(style::TEXT_SM)
                    .style(style::dim_text)
                    .width(Length::Fill),
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fixed(0.0)),
                chip(
                    "Open (Enter)",
                    GraphCommand::OpenSelected,
                    state.selection.is_some()
                ),
                chip(
                    "Insert link (Ctrl+Enter)",
                    GraphCommand::InsertSelected,
                    state.selection.is_some()
                ),
                chip(
                    "Unpin",
                    GraphCommand::UnpinSelected,
                    state.selection.is_some()
                ),
            ]
            .spacing(style::SPACING)
            .align_y(alignment::Vertical::Center),
            row![
                text(app.writing_target_label()).size(style::TEXT_SM),
                text(if app.valid_writing_target().is_some() {
                    ""
                } else {
                    "— double-click inserts here; otherwise choose an editor and place the cursor"
                })
                .size(style::TEXT_SM)
                .style(style::dim_text),
                stale,
                iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fixed(0.0)),
                text(provenance).size(style::TEXT_SM).style(style::dim_text),
                chip(
                    "Retry",
                    GraphCommand::RetryService,
                    matches!(app.graph.status, Some(GraphServiceStatus::Failed { .. }))
                ),
            ]
            .spacing(style::SPACING_SM)
            .align_y(alignment::Vertical::Center),
            row![
                chip(
                    if state.filter.expanded {
                        "Close filters"
                    } else {
                        "Filters"
                    },
                    GraphCommand::ToggleFilters,
                    true
                ),
                chip("Clear filters", GraphCommand::ClearFilters, true),
                chip(
                    "Undo filters",
                    GraphCommand::UndoFilters,
                    state.previous_scope.is_some()
                ),
                chip(
                    "Neighborhood",
                    GraphCommand::ExploreSelected,
                    state.selection.is_some()
                )
            ]
            .spacing(4),
            crate::host::occurrences::view(app, tab),
            row![
                chip(
                    "Reveal selection",
                    GraphCommand::RevealSelected,
                    state.selection.is_some()
                ),
                chip(
                    "Clear selection",
                    GraphCommand::ClearSelection,
                    state.selection.is_some()
                ),
                text(if state.selection_summary.is_empty() {
                    format!("{} selected", state.selected_nodes.len())
                } else {
                    state.selection_summary.clone()
                })
                .size(style::TEXT_SM),
                text(
                    if state.scope_result.as_ref().is_some_and(|s| s.excluded_root) {
                        "Root excluded — clear filters"
                    } else if state.scope_result.as_ref().is_some_and(|s| s.missing_root) {
                        "Neighborhood root is missing"
                    } else if state
                        .scope_result
                        .as_ref()
                        .is_some_and(|s| s.nodes.is_empty())
                    {
                        "No matching notes — clear filters"
                    } else {
                        ""
                    }
                )
                .size(style::TEXT_SM),
            ]
            .spacing(4),
            row![
                // D-04: a *source* view shows what it publishes, not a
                // binding label (the graph has no follow-binding of its own).
                text(match app.workspace.exploration() {
                    Some(selection) if selection.source_tab == tab => match &selection.target {
                        Some(target) => format!("Publishing to context: {}", target.title()),
                        None => "Publishing to context: no selection".into(),
                    },
                    _ => "Publishing to context: not publishing".into(),
                })
                .size(style::TEXT_SM)
                .style(style::dim_text),
            ],
        ]
        .spacing(2),
    )
    .width(Length::Fill)
    .padding(Padding {
        top: 4.0,
        bottom: 4.0,
        left: style::SPACING,
        right: style::SPACING,
    })
    .style(style::toolbar)
    .into()
}

/// Honest loading/error states before any frame exists (§13.4). The graph
/// never shows an empty network because the index is merely behind.
fn unavailable<'a>(app: &'a App, tab: TabId, state: &GraphTabState) -> Element<'a, Message> {
    let pins_failure = app.graph.pins.load_failure().map(str::to_owned);
    let (title, detail, retry) = match app.graph.status.as_ref() {
        Some(GraphServiceStatus::Indexing { reason }) => (
            "Graph: indexing",
            format!("The knowledge network appears once the saved index exists ({reason}). Notes remain fully usable."),
            false,
        ),
        Some(GraphServiceStatus::Failed { error }) => (
            "Graph service failed",
            error.clone(),
            true,
        ),
        Some(GraphServiceStatus::Live) | None => (
            "Graph: waiting for layout",
            "The base index is live; positions are being computed. This frame appears as soon as the pair agrees.".into(),
            state.stale,
        ),
    };
    let mut body = column![
        text(title).size(style::HEADING),
        text(detail).style(style::dim_text).width(Length::Fill),
    ]
    .spacing(style::SPACING_SM);
    if let Some(error) = pins_failure {
        body =
            body.push(text(format!("Graph pins were not loaded: {error}")).style(style::dim_text));
    }
    if let Some(error) = app.graph.pins.failure.clone() {
        body = body.push(
            row![
                text(format!("Pin storage error (retry scheduled): {error}"))
                    .size(style::TEXT_SM)
                    .style(style::dim_text),
                button(text("Retry pins now").size(style::TEXT_SM))
                    .padding(tab_strip::chip_padding())
                    .style(style::action())
                    .on_press(Message::GraphCommand {
                        tab,
                        command: GraphCommand::RetryPins,
                    }),
            ]
            .spacing(style::SPACING_SM),
        );
    }
    container(
        body.push(
            button(text("Retry service").size(style::TEXT_SM))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press_maybe(retry.then_some(Message::GraphCommand {
                    tab,
                    command: GraphCommand::RetryService,
                })),
        ),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(style::SPACING_LG)
    .style(style::content)
    .into()
}

/// The visual body: exactly one mounted frame, one geometry, one camera —
/// both backends consume the same values.
fn visual_body<'a>(
    app: &'a App,
    tab: TabId,
    incarnation: u64,
    state: &GraphTabState,
    geometry: &Arc<RenderGeometry>,
) -> Element<'a, Message> {
    let key = state.geometry_key.expect("mounted geometry has a version");
    let publish = move |message: GraphMessage| Message::GraphAction {
        tab,
        incarnation,
        message,
        key,
    };
    let canvas: Element<'a, GraphMessage> = match app.graph.backend {
        crate::host::graph::GraphBackend::Gpu => {
            GraphProgram::new(Arc::clone(geometry), state.camera)
                .widget()
                .into()
        }
        crate::host::graph::GraphBackend::Canvas => {
            GraphCanvas::new(Arc::clone(geometry), state.camera)
                .element()
                .into()
        }
    };
    let surface: Element<'a, Message> = canvas.map(publish);
    let target = graph::writing_target_node(app);
    let frame = app.flavite.as_ref().and_then(|api| api.graph_frame());
    let mut labels = std::collections::BTreeMap::new();
    if let (Some(bounds), Some(frame)) = (state.bounds, frame) {
        let publication = frame.publication();
        for (index, node) in geometry.nodes().iter().enumerate() {
            let Some(id) = geometry.node_id(index) else {
                continue;
            };
            let screen = state.camera.world_to_screen(
                node.center,
                [0.0, 0.0],
                [bounds.width, bounds.height],
            );
            if screen[0] < 0.0
                || screen[1] < 0.0
                || screen[0] > bounds.width
                || screen[1] > bounds.height
            {
                continue;
            }
            let priority = if state.selected_nodes.contains(&id) {
                0
            } else if Some(id) == target {
                1
            } else {
                2
            };
            let key = (priority, id);
            if labels.len() >= 200
                && labels
                    .last_key_value()
                    .is_some_and(|(last, _)| *last <= key)
            {
                continue;
            }
            let Some(record) = publication
                .node_index(id)
                .and_then(|index| publication.record(index as usize))
            else {
                continue;
            };
            // Never allocate a title for every visible entity before truncating.
            labels.insert(
                key,
                (
                    node.center,
                    node.size,
                    record.title.chars().take(160).collect::<String>(),
                ),
            );
            if labels.len() > 200 {
                labels.pop_last();
            }
        }
    }
    let labels = labels.into_values().collect();
    let surface: Element<'a, Message> = iced::widget::stack![
        surface,
        crate::graph::canvas::label_overlay(state.camera, labels)
    ]
    .into();
    let surface: Element<'a, Message> = if let Some((first, second)) = state.box_preview {
        iced::widget::stack![
            surface,
            crate::graph::canvas::selection_overlay(state.camera, first, second)
        ]
        .into()
    } else {
        surface
    };
    bounds_obs::observe(
        container(surface).width(Length::Fill).height(Length::Fill),
        move |rect| Message::GraphBounds(tab, rect),
    )
}

/// The accessible list mode: the same saved-base nodes as the visual
/// graph, as real controls, with per-row Open/Insert actions and honest
/// disabled reasons. This is also what the a11y tree mirrors, so the
/// network is operable with no pointer at all (§13.3).
fn list_body<'a>(
    app: &'a App,
    tab: TabId,
    state: &GraphTabState,
    geometry: &Arc<RenderGeometry>,
) -> Element<'a, Message> {
    let Some(publication) = app
        .flavite
        .as_ref()
        .and_then(|flavite| flavite.graph_frame())
        .map(|frame| Arc::clone(frame.publication()))
    else {
        return empty_state("Graph", "The base publication disappeared.");
    };
    let incarnation = app
        .workspace
        .tab(&tab)
        .expect("mounted graph")
        .interaction()
        .incarnation;
    let key = state.geometry_key.expect("mounted graph");
    let has_target = app.valid_writing_target().is_some();
    let mut rows = column![].spacing(2);
    for dense in state.list_offset
        ..geometry
            .node_count()
            .min(state.list_offset + LIST_ROW_BUDGET)
    {
        let Some(id) = geometry.node_id(dense) else {
            continue;
        };
        let Some(record) = publication
            .node_index(id)
            .and_then(|index| publication.record(index as usize))
        else {
            continue;
        };
        let selected = state.selection == Some(HitNode::new(id, record.id.kind));
        let locator = graph::locator_for(record);
        let row_view = row![
            iced::widget::checkbox(state.selected_nodes.contains(&id)).on_toggle(move |_| {
                Message::GraphNodeCommand {
                    tab,
                    incarnation,
                    key,
                    command: GraphCommand::ToggleSelected(id),
                }
            }),
            button(
                row![
                    text(record.title.clone()).width(Length::Fill),
                    text(record.path.clone())
                        .size(style::TEXT_SM)
                        .style(style::dim_text),
                ]
                .spacing(style::SPACING)
            )
            .width(Length::Fill)
            .padding(Padding {
                top: 6.0,
                bottom: 6.0,
                left: 10.0,
                right: 10.0,
            })
            .style(style::list_row(selected))
            .on_press(Message::GraphNodeCommand {
                tab,
                incarnation,
                key,
                command: GraphCommand::Select(id),
            }),
            button(text("Open").size(style::TEXT_SM))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press_maybe((record.id.kind == flavite_graph::NodeKind::Note).then_some(
                    Message::GraphNodeCommand {
                        tab,
                        incarnation,
                        key,
                        command: GraphCommand::Open(id),
                    }
                ),),
            button(text("Insert").size(style::TEXT_SM))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press_maybe((has_target && locator.is_some()).then_some(
                    Message::GraphNodeCommand {
                        tab,
                        incarnation,
                        key,
                        command: GraphCommand::Insert(id),
                    }
                ),),
        ]
        .spacing(style::SPACING_SM);
        rows = rows.push(row_view);
    }
    let omitted = geometry.node_count().saturating_sub(LIST_ROW_BUDGET);
    let footer = if omitted > 0 {
        format!(
            "+ {omitted} more nodes (list budget {LIST_ROW_BUDGET}) — the visual graph shows all"
        )
    } else {
        format!(
            "{} current nodes · double-click or Insert places a link at the writing target",
            geometry.node_count()
        )
    };
    container(
        column![
            row![
                button("Previous").on_press_maybe((state.list_offset > 0).then_some(
                    Message::GraphCommand {
                        tab,
                        command: GraphCommand::PreviousListPage
                    }
                )),
                text(format!(
                    "{}–{} of {}",
                    state.list_offset + 1,
                    (state.list_offset + LIST_ROW_BUDGET).min(geometry.node_count()),
                    geometry.node_count()
                )),
                button("Next").on_press_maybe(
                    (state.list_offset + LIST_ROW_BUDGET < geometry.node_count()).then_some(
                        Message::GraphCommand {
                            tab,
                            command: GraphCommand::NextListPage
                        }
                    )
                )
            ]
            .spacing(4),
            scrollable(rows).width(Length::Fill).height(Length::Fill),
            text(footer).size(style::TEXT_SM).style(style::dim_text),
        ]
        .spacing(style::SPACING_SM),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(style::SPACING)
    .style(style::content)
    .into()
}
