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
//! counts, provenance, and the writing-target line, so an explicit
//! insertion's destination is visible before it happens. D-02: a
//! double-click **opens** a node; only Ctrl+Enter/Insert places a link.
//! A missing target gets an explanation and a repair action, never a
//! hidden fallback.

use std::sync::Arc;

use crate::app::{App, Message};
use crate::graph::canvas::GraphCanvas;
use crate::graph::geometry::{HitNode, RenderGeometry};
use crate::graph::renderer::{GraphMessage, GraphProgram};
use crate::host::graph::{self, GraphCommand, GraphTabState};
use crate::style;
use crate::views::empty_state;
use crate::workspace::{
    bounds as bounds_obs,
    chrome::{self, Icon},
    tab_strip,
};
use flavite_api::GraphServiceStatus;
use flavite_workspace::{TabId, ViewKind};
use iced::widget::{button, column, container, row, scrollable, text};
use iced::{Element, Length, Padding, alignment};

/// Rows rendered in list/accessibility mode before truncation is noted.
const LIST_ROW_BUDGET: usize = 50;

/// Render one graph tab for the active view slot.
///
/// Compact chrome (concept 01): one toolbar row plus an optional selection
/// row, the writing-target banner, a full-bleed canvas, and a single
/// status/hint footer — the controls hug their content and the network
/// gets the space.
pub fn view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let metrics = *app.appearance.metrics();
    let Some(session) = app.workspace.tab(&tab) else {
        return empty_state(metrics, "Empty", "This tab has no content.");
    };
    let ViewKind::Graph(_) = session.kind() else {
        return empty_state(metrics, "Graph", "This tab is not a graph view.");
    };
    let incarnation = session.interaction().incarnation;
    let Some(state) = app.graph.state(tab) else {
        return empty_state(metrics, "Graph", "The graph view is starting…");
    };

    let mut activity = column![].spacing(8);
    if state.selection.is_some() {
        activity = activity.push(
            container(
                column![
                    selection_band(app, tab, state),
                    target_banner(app),
                    scrollable(crate::host::occurrences::view(app, tab)).height(Length::Shrink)
                ]
                .spacing(8),
            )
            .width(Length::Fill)
            .padding(12)
            .style(style::palette_card),
        );
    }
    let body = match &state.geometry {
        None => unavailable(app, tab, state),
        Some(geometry) if state.list_mode => list_body(app, tab, state, geometry),
        Some(geometry) => visual_body(
            app,
            tab,
            incarnation,
            state,
            geometry,
            state.selection.map(|_| Element::from(activity)),
        ),
    };
    column![
        header(app, tab, state),
        container(body).width(Length::Fill).height(Length::Fill),
        container(footer(app, tab, state))
            .width(Length::Fill)
            .height(20.0 * metrics.scale)
            .clip(true)
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

/// The dismissible D-02 gesture-contract explanation: the first upgraded
/// launch shows it once; acknowledging persists the device preference.
fn activation_notice<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    container(bounds_obs::observe(
        container(row![
            text(
                "Double-click a node to open its note. Ctrl+Enter inserts a link into the writing note.",
            )
            .size(metrics.text_sm)
            .width(Length::Fill),
            button(text("Got it").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                .on_press(Message::Settings(
                    crate::host::settings::SettingsMessage::AcknowledgeGraphActivation,
                )),
        ]
        .spacing(metrics.spacing)
        .align_y(alignment::Vertical::Center))
        .width(Length::Fill)
        .padding(Padding {
            top: 4.0,
            bottom: 4.0,
            left: metrics.spacing,
            right: metrics.spacing,
        })
        .style(style::toolbar),
        move |rect| Message::GraphNoticeBounds(tab, rect),
    ))
    .width(Length::Fill)
    .into()
}

/// One overlay row height: a compact action row that never changes with
/// its content (AC-4). Deterministic in the metrics.
fn target_banner<'a>(app: &'a App) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let label = app.valid_writing_target().map_or_else(
        || "Place the caret in a note to insert a link.".to_owned(),
        |(tab, ..)| {
            format!(
                "Insert into: {}",
                app.notes
                    .locator_of(tab)
                    .map_or_else(|| "note".to_owned(), |note| note.title().to_owned())
            )
        },
    );
    text(label)
        .size(metrics.text_sm)
        .style(style::dim_text)
        .into()
}

fn footer<'a>(app: &'a App, _tab: TabId, state: &GraphTabState) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let label = if state.stale {
        "Graph is out of date · refresh before inserting".to_owned()
    } else if state.updating {
        "Updating…".to_owned()
    } else {
        state
            .geometry
            .as_ref()
            .map_or_else(String::new, |geometry| {
                format!("{} nodes", geometry.node_count())
            })
    };
    container(text(label).size(metrics.text_sm).style(style::dim_text))
        .width(Length::Fill)
        .padding([0, 12])
        .align_x(alignment::Horizontal::Right)
        .into()
}

fn header<'a>(app: &'a App, tab: TabId, _state: &GraphTabState) -> Element<'a, Message> {
    let metrics = *app.appearance.metrics();
    container(
        row![
            text("Graph view").size(metrics.text).style(style::dim_text),
            iced::widget::Space::new().width(Length::Fill),
            chrome::icon_button(
                metrics,
                Icon::More,
                "Graph controls",
                Some(Message::GraphCommand {
                    tab,
                    command: GraphCommand::ToggleControls
                })
            )
        ]
        .align_y(alignment::Vertical::Center),
    )
    .height(38.0 * metrics.scale)
    .padding([0, 12])
    .width(Length::Fill)
    .into()
}

fn controls_panel<'a>(app: &'a App, tab: TabId, state: &GraphTabState) -> Element<'a, Message> {
    let metrics = *app.appearance.metrics();
    let section = |title: &'static str, open: bool, command| {
        button(
            row![
                text(if open { "⌄" } else { "›" }).size(metrics.text),
                text(title).size(metrics.text)
            ]
            .spacing(12)
            .align_y(alignment::Vertical::Center),
        )
        .width(Length::Fill)
        .padding([12, 14])
        .style(style::ghost())
        .on_press(Message::GraphCommand { tab, command })
    };
    let action = |title: &'static str, command| {
        button(text(title).size(metrics.text_sm))
            .padding([6, 10])
            .style(style::ghost())
            .on_press(Message::GraphCommand { tab, command })
    };
    let mut card = column![
        row![
            iced::widget::Space::new().width(Length::Fill),
            chrome::icon_button(
                metrics,
                Icon::Close,
                "Hide graph controls",
                Some(Message::GraphCommand {
                    tab,
                    command: GraphCommand::ToggleControls
                })
            )
        ],
        section(
            "Filters",
            state.filter.expanded,
            GraphCommand::ToggleFilters
        ),
    ];
    if state.filter.expanded {
        card = card.push(container(crate::graph::filters::view(app, tab)).padding(12));
    }
    card = card.push(iced::widget::rule::horizontal(1));
    card = card.push(section(
        "Display",
        state.display_controls_open,
        GraphCommand::ToggleDisplayControls,
    ));
    if state.display_controls_open {
        card = card.push(
            container(
                column![
                    action(
                        if state.list_mode {
                            "Show graph"
                        } else {
                            "Show list"
                        },
                        GraphCommand::ToggleList
                    ),
                    text(crate::graph::provenance::label(state))
                        .size(metrics.text_sm)
                        .style(style::dim_text),
                ]
                .spacing(8),
            )
            .padding(12),
        );
        if app.appearance.graph_activation_notice_pending() {
            card = card.push(activation_notice(app, tab));
        }
    }
    card = card.push(iced::widget::rule::horizontal(1));
    card = card.push(section(
        "Layout",
        state.layout_controls_open,
        GraphCommand::ToggleLayoutControls,
    ));
    if state.layout_controls_open {
        card = card.push(
            container(
                column![
                    action(
                        if state.frozen {
                            "Resume layout"
                        } else {
                            "Freeze layout"
                        },
                        GraphCommand::ToggleFreeze
                    ),
                    action("Fit to view", GraphCommand::FitView),
                ]
                .spacing(6),
            )
            .padding(12),
        );
    }
    container(scrollable(card).height(Length::Shrink))
        .width(290.0 * metrics.scale)
        .max_height(540.0 * metrics.scale)
        .style(style::palette_card)
        .into()
}

/// The reserved selection band (AC-4): identity, summary, and the typed
/// actions. Empty with a stable height when nothing is selected — the
/// canvas geometry never notices a selection change (D-02). Chip
/// availability comes from the same predicates the commands enforce
/// (FR-15), so no surface can offer what the core would refuse.
fn selection_band<'a>(app: &'a App, tab: TabId, state: &GraphTabState) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let Some(hit) = state.selection else {
        return iced::widget::Space::new()
            .width(Length::Fill)
            .height(Length::Fixed(0.0))
            .into();
    };
    let record = graph::record_for(app, hit.id);
    let selection = record
        .as_ref()
        .map_or_else(|| "Selection".to_owned(), |record| record.title.clone());
    let chip = |label: &'static str, command: GraphCommand, enabled: bool| {
        button(text(label).size(metrics.text_sm))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press_maybe(enabled.then_some(Message::GraphCommand { tab, command }))
    };
    let action_chip = |label: &'static str, message: Message, enabled: bool| {
        button(text(label).size(metrics.text_sm))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press_maybe(enabled.then_some(message))
    };
    let locator = record.as_ref().and_then(graph::locator_for);
    let mut actions = row![].spacing(6);
    // D-02: every node kind gets its own explicit intent and nothing else
    // is offered — a resolved note opens (and can be inserted into), an
    // attachment activates the existing local inspector, and an unresolved
    // target offers Create-or-Locate without ever creating implicitly.
    match record.as_ref().map(|record| record.id.kind) {
        Some(flavite_graph::NodeKind::Note) if locator.is_some() => {
            actions = actions
                .push(chip("Open", GraphCommand::OpenSelected, true))
                .push(chip(
                    "Insert link",
                    GraphCommand::InsertSelected,
                    graph::insert_available(app, tab),
                ));
        }
        Some(flavite_graph::NodeKind::Attachment) => {
            actions = actions.push(chip("Open attachment", GraphCommand::OpenSelected, true));
        }
        Some(flavite_graph::NodeKind::Unresolved) => {
            let representable = record
                .as_ref()
                .is_some_and(graph::unresolved_is_representable);
            actions = actions
                .push(action_chip(
                    "Create note…",
                    Message::CreateFromNode(tab),
                    representable,
                ))
                .push(action_chip(
                    "Locate note",
                    Message::LocateFromNode(tab),
                    true,
                ));
        }
        _ => {}
    }
    if state.selected_nodes.len() > 1 {
        actions = actions.push(chip("Open in tabs", GraphCommand::OpenSelectedInTabs, true));
    }
    column![
        row![
            text(selection).size(metrics.text),
            iced::widget::Space::new().width(Length::Fill),
            chip("Clear", GraphCommand::ClearSelection, true)
        ],
        actions,
        row![
            chip(
                "Neighborhood",
                GraphCommand::ExploreSelected,
                record.is_some()
            ),
            chip("Unpin", GraphCommand::UnpinSelected, record.is_some())
        ]
        .spacing(6)
    ]
    .spacing(8)
    .into()
}

/// Honest loading/error states before any frame exists (§13.4). The graph
/// never shows an empty network because the index is merely behind.
fn unavailable<'a>(app: &'a App, tab: TabId, state: &GraphTabState) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
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
        text(title).size(metrics.heading),
        text(detail).style(style::dim_text).width(Length::Fill),
    ]
    .spacing(metrics.spacing_sm);
    if let Some(error) = pins_failure {
        body =
            body.push(text(format!("Graph pins were not loaded: {error}")).style(style::dim_text));
    }
    if let Some(error) = app.graph.pins.failure.clone() {
        body = body.push(
            row![
                text(format!("Pin storage error (retry scheduled): {error}"))
                    .size(metrics.text_sm)
                    .style(style::dim_text),
                button(text("Retry pins now").size(metrics.text_sm))
                    .padding(tab_strip::chip_padding())
                    .style(style::action())
                    .on_press(Message::GraphCommand {
                        tab,
                        command: GraphCommand::RetryPins,
                    }),
            ]
            .spacing(metrics.spacing_sm),
        );
    }
    container(
        body.push(
            button(text("Retry service").size(metrics.text_sm))
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
    .padding(metrics.spacing_lg)
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
    activity_overlay: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let key = state.geometry_key.expect("mounted geometry has a version");
    let publish = move |message: GraphMessage| Message::GraphAction {
        tab,
        incarnation,
        message,
        key,
    };
    let scheme = app.appearance.scheme();
    let canvas: Element<'a, GraphMessage> = match app.graph.backend {
        crate::host::graph::GraphBackend::Gpu => {
            GraphProgram::new(Arc::clone(geometry), state.camera, scheme)
                .widget()
                .into()
        }
        crate::host::graph::GraphBackend::Canvas => {
            GraphCanvas::new(Arc::clone(geometry), state.camera, scheme)
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
        crate::graph::canvas::label_overlay(state.camera, labels, scheme)
    ]
    .into();
    let surface: Element<'a, Message> = if let Some((first, second)) = state.box_preview {
        iced::widget::stack![
            surface,
            crate::graph::canvas::selection_overlay(state.camera, first, second, scheme)
        ]
        .into()
    } else {
        surface
    };
    // Corner fit control (concept 01): the canvas keeps every pixel; the
    // button floats over it in the bottom-right.
    let fit_control = container(
        button(text("Fit to view").size(metrics.text_sm))
            .padding(tab_strip::chip_padding())
            .style(style::ghost())
            .on_press_maybe(state.geometry.is_some().then_some(Message::GraphCommand {
                tab,
                command: GraphCommand::FitView,
            })),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(6)
    .align_x(alignment::Horizontal::Right)
    .align_y(alignment::Vertical::Bottom);
    let surface: Element<'a, Message> = iced::widget::stack![surface, fit_control].into();
    // Selection actions, occurrence details, and the D-02 notice float
    // over the top of the canvas: their content changes never touch the
    // canvas bounds (AC-4).
    let surface: Element<'a, Message> = if let Some(overlay) = activity_overlay {
        let selected_y = state
            .selection
            .and_then(|selected| {
                geometry
                    .nodes()
                    .iter()
                    .enumerate()
                    .find(|(index, _)| geometry.node_id(*index) == Some(selected.id))
            })
            .and_then(|(_, node)| {
                state.bounds.map(|bounds| {
                    (
                        state.camera.world_to_screen(
                            node.center,
                            [0.0, 0.0],
                            [bounds.width, bounds.height],
                        )[1],
                        bounds.height,
                    )
                })
            });
        let pinned = container(iced::widget::opaque(
            container(overlay).max_width(360.0 * metrics.scale),
        ))
        .width(Length::Fill)
        .height(Length::Fill)
        .padding(12)
        .align_x(alignment::Horizontal::Left)
        .align_y(if selected_y.is_some_and(|(y, height)| y < height * 0.5) {
            alignment::Vertical::Bottom
        } else {
            alignment::Vertical::Top
        });
        iced::widget::stack![surface, pinned].into()
    } else {
        surface
    };
    let surface: Element<'a, Message> = if state.controls_open {
        let panel = container(iced::widget::opaque(controls_panel(app, tab, state)))
            .width(Length::Fill)
            .height(Length::Fill)
            .padding(16)
            .align_x(alignment::Horizontal::Right)
            .align_y(alignment::Vertical::Top);
        iced::widget::stack![surface, panel].into()
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
    let metrics = app.appearance.metrics();
    let Some(publication) = app
        .flavite
        .as_ref()
        .and_then(|flavite| flavite.graph_frame())
        .map(|frame| Arc::clone(frame.publication()))
    else {
        return empty_state(*metrics, "Graph", "The base publication disappeared.");
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
                        .size(metrics.text_sm)
                        .style(style::dim_text),
                ]
                .spacing(metrics.spacing)
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
            button(text("Open").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::action())
                // D-02: every node kind's Open does something honest — a
                // resolved note navigates, an attachment opens the local
                // inspector, an unresolved target presents Create/Locate.
                // A blocked or unrepresentable path gets no dead control.
                .on_press_maybe(
                    (locator.is_some()
                        || record.id.kind == flavite_graph::NodeKind::Attachment
                        || (record.id.kind == flavite_graph::NodeKind::Unresolved
                            && graph::unresolved_is_representable(record)))
                    .then_some(Message::GraphNodeCommand {
                        tab,
                        incarnation,
                        key,
                        command: GraphCommand::Open(id),
                    }),
                ),
            button(text("Insert").size(metrics.text_sm))
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
        .spacing(metrics.spacing_sm);
        rows = rows.push(row_view);
    }
    let omitted = geometry.node_count().saturating_sub(LIST_ROW_BUDGET);
    let footer = if omitted > 0 {
        format!(
            "+ {omitted} more nodes (list budget {LIST_ROW_BUDGET}) — the visual graph shows all"
        )
    } else {
        format!(
            "{} current nodes · Enter/double-click opens · Ctrl+Enter or Insert places a link at the writing target",
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
            text(footer).size(metrics.text_sm).style(style::dim_text),
        ]
        .spacing(metrics.spacing_sm),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(metrics.spacing)
    .style(style::content)
    .into()
}
