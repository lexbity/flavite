//! Graph scope controls and bounded draft admission. No query or filesystem IO.
use crate::app::{App, Message};
use crate::host::graph::{self, GraphCommand};
use crate::style;
use flavite_workspace::{
    Direction, GraphScope, LayoutAction, Neighborhood, NotePredicate, PropertyValue, TabId,
    ViewKind,
};
use iced::widget::{button, checkbox, column, pick_list, row, text, text_input};
use iced::{Element, Length};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FilterField {
    Tag,
    Path,
    PropertyKey,
    PropertyValue,
    Query,
}

#[derive(Debug, Clone, Default)]
pub struct FilterDraft {
    pub advanced: bool,
    pub tag: String,
    pub path: String,
    pub property_key: String,
    pub property_value: String,
    pub query: String,
    pub attachments: bool,
    pub embeds: bool,
    pub unresolved: bool,
    pub any: bool,
    pub negate: bool,
    pub expanded: bool,
}
impl FilterDraft {
    pub fn from_scope(scope: &GraphScope) -> Self {
        let mut draft = Self {
            attachments: scope.include_attachments,
            embeds: scope.include_embeds,
            unresolved: scope.show_unresolved,
            ..Self::default()
        };
        let mut work = vec![&scope.predicate];
        while let Some(predicate) = work.pop() {
            match predicate {
                NotePredicate::Tag { name } => draft.tag.clone_from(name),
                NotePredicate::PathPrefix { native } => {
                    draft.path = String::from_utf8(native.clone()).unwrap_or_default()
                }
                NotePredicate::Property { key, value } => {
                    draft.property_key.clone_from(key);
                    draft.property_value = match value {
                        PropertyValue::Text(s)
                        | PropertyValue::Date(s)
                        | PropertyValue::DateTime(s) => format!("{s:?}"),
                        PropertyValue::Number(s) => s.clone(),
                        PropertyValue::Bool(v) => v.to_string(),
                        PropertyValue::Null => "null".into(),
                    };
                }
                NotePredicate::SavedQuery { query } => draft.query.clone_from(query),
                NotePredicate::And { clauses } => work.extend(clauses),
                NotePredicate::Or { clauses } => {
                    draft.any = true;
                    work.extend(clauses);
                }
                NotePredicate::Not { clause } => {
                    draft.negate = true;
                    work.push(clause);
                }
                NotePredicate::All => {}
            }
        }
        // The compact editor cannot faithfully flatten arbitrary nested ASTs.
        // Keep those scopes active until an explicit Clear replaces them.
        draft.advanced = draft.compile(scope.neighborhood.clone()).as_ref() != Ok(scope);
        draft
    }
    pub fn compile(&self, neighborhood: Option<Neighborhood>) -> Result<GraphScope, String> {
        if self.advanced {
            return Err(
                "This filter has nested conditions; Clear filters before building a replacement"
                    .into(),
            );
        }
        let mut clauses = Vec::new();
        if !self.tag.trim().is_empty() {
            clauses.push(NotePredicate::Tag {
                name: self.tag.trim().trim_start_matches('#').to_lowercase(),
            });
        }
        if !self.path.is_empty() {
            clauses.push(NotePredicate::PathPrefix {
                native: self.path.as_bytes().to_vec(),
            });
        }
        if !self.property_key.is_empty() {
            let value = match serde_json::from_str::<serde_json::Value>(&self.property_value) {
                Ok(serde_json::Value::String(s)) => PropertyValue::Text(s),
                Ok(serde_json::Value::Number(n)) => PropertyValue::Number(n.to_string()),
                Ok(serde_json::Value::Bool(b)) => PropertyValue::Bool(b),
                Ok(serde_json::Value::Null) => PropertyValue::Null,
                _ => {
                    return Err(
                        "Property value: use quoted text, a number, true, false or null".into(),
                    );
                }
            };
            clauses.push(NotePredicate::Property {
                key: self.property_key.clone(),
                value,
            });
        }
        if !self.query.trim().is_empty() {
            flavite_api::Query::parse(&self.query)
                .map_err(|_| "Invalid saved-content query; the last valid scope is retained")?;
            clauses.push(NotePredicate::SavedQuery {
                query: self.query.clone(),
            });
        }
        let predicate = match clauses.len() {
            0 => NotePredicate::All,
            1 => clauses.remove(0),
            _ if self.any => NotePredicate::Or { clauses },
            _ => NotePredicate::And { clauses },
        };
        let scope = GraphScope {
            predicate: if self.negate {
                NotePredicate::Not {
                    clause: Box::new(predicate),
                }
            } else {
                predicate
            },
            neighborhood,
            include_attachments: self.attachments,
            include_embeds: self.embeds,
            show_unresolved: self.unresolved,
        };
        scope.validate().map_err(str::to_owned)?;
        Ok(scope)
    }
}

pub fn input(app: &mut App, tab: TabId, incarnation: u64, field: FilterField, value: String) {
    if value.len() > 8192
        || app.workspace.tab(&tab).is_none_or(|s| {
            s.interaction().incarnation != incarnation || !matches!(s.kind(), ViewKind::Graph(_))
        })
    {
        return;
    }
    if let Some(state) = app.graph.state_mut(tab) {
        let destination = match field {
            FilterField::Tag => &mut state.filter.tag,
            FilterField::Path => &mut state.filter.path,
            FilterField::PropertyKey => &mut state.filter.property_key,
            FilterField::PropertyValue => &mut state.filter.property_value,
            FilterField::Query => &mut state.filter.query,
        };
        *destination = value;
        state.filter_error = None;
    }
}

pub fn commit(app: &mut App, tab: TabId, scope: GraphScope) {
    let previous = app.graph.state(tab).map(|s| s.scope.clone());
    if previous.as_ref() == Some(&scope) {
        return;
    }
    app.dispatch_layout(|revision| LayoutAction::SetGraphScope {
        expected_revision: revision,
        tab,
        scope: scope.clone(),
    });
    let committed = app
        .workspace
        .tab(&tab)
        .is_some_and(|s| matches!(s.kind(), ViewKind::Graph(v) if v.scope() == &scope));
    if committed && let Some(state) = app.graph.state_mut(tab) {
        state.previous_scope = previous;
        state.scope = scope;
        state.scope_sequence = state.scope_sequence.saturating_add(1);
        state.updating = true;
        state.list_offset = 0;
        state.box_preview = None;
        state.arbiter.cancel();
        state.filter_error = None;
    }
}

pub fn command(app: &mut App, tab: TabId, command: GraphCommand) {
    let Some(state) = app.graph.state(tab) else {
        return;
    };
    let scope = match command {
        GraphCommand::ToggleFilters => {
            if let Some(state) = app.graph.state_mut(tab) {
                state.filter.expanded = !state.filter.expanded;
            }
            return;
        }
        GraphCommand::ApplyFilters => {
            match state.filter.compile(state.scope.neighborhood.clone()) {
                Ok(scope) => scope,
                Err(error) => {
                    if let Some(state) = app.graph.state_mut(tab) {
                        state.filter_error = Some(error);
                    }
                    return;
                }
            }
        }
        GraphCommand::ClearFilters | GraphCommand::RevealSelected => GraphScope::default(),
        GraphCommand::UndoFilters => match state.previous_scope.clone() {
            Some(scope) => scope,
            None => return,
        },
        GraphCommand::ExploreSelected => {
            let Some(record) = state
                .selection
                .and_then(|hit| graph::record_for(app, hit.id))
            else {
                return;
            };
            let mut scope = state.scope.clone();
            scope.neighborhood = Some(Neighborhood {
                root_native: record.path_native,
                depth: 2,
                direction: Direction::Both,
            });
            scope
        }
        GraphCommand::ClearSelection => {
            if let Some(state) = app.graph.state_mut(tab) {
                state.selection = None;
                state.selected_nodes.clear();
            }
            graph::publish_empty(app, tab);
            return;
        }
        GraphCommand::Depth(depth) => {
            let mut scope = state.scope.clone();
            if let Some(n) = &mut scope.neighborhood {
                n.depth = depth;
            }
            scope
        }
        GraphCommand::Direction(direction) => {
            let mut scope = state.scope.clone();
            if let Some(n) = &mut scope.neighborhood {
                n.direction = direction;
            }
            scope
        }
        GraphCommand::Unresolved(value)
        | GraphCommand::Attachments(value)
        | GraphCommand::Embeds(value)
        | GraphCommand::Any(value)
        | GraphCommand::Negate(value) => {
            if let Some(state) = app.graph.state_mut(tab) {
                match command {
                    GraphCommand::Unresolved(_) => state.filter.unresolved = value,
                    GraphCommand::Attachments(_) => state.filter.attachments = value,
                    GraphCommand::Embeds(_) => state.filter.embeds = value,
                    GraphCommand::Any(_) => state.filter.any = value,
                    _ => state.filter.negate = value,
                }
            }
            return;
        }
        _ => return,
    };
    let expanded = state.filter.expanded && command != GraphCommand::ApplyFilters;
    commit(app, tab, scope.clone());
    if let Some(state) = app.graph.state_mut(tab) {
        state.filter = FilterDraft::from_scope(&scope);
        state.filter.expanded = expanded;
    }
}

pub fn view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let state = app.graph.state(tab).expect("mounted graph");
    if state.filter.advanced {
        return column![
            text("This saved scope uses nested conditions. It remains active."),
            text("Clear filters to build a replacement with the compact editor."),
            button("Clear filters").on_press(Message::GraphCommand {
                tab,
                command: GraphCommand::ClearFilters
            }),
        ]
        .spacing(8)
        .into();
    }
    let incarnation = app
        .workspace
        .tab(&tab)
        .expect("mounted graph")
        .interaction()
        .incarnation;
    let command = move |command| Message::GraphCommand { tab, command };
    let action = |title, cmd| {
        button(text(title).size(metrics.text_sm))
            .on_press(command(cmd))
            .style(style::ghost())
    };
    let mut content = column![
        row![
            action("Reset", GraphCommand::ClearFilters),
            action("Undo", GraphCommand::UndoFilters),
            action("Neighborhood", GraphCommand::ExploreSelected),
        ]
        .spacing(4)
    ]
    .spacing(8);
    if state.filter.expanded {
        let input =
            move |placeholder: &'static str, value: &'a str, field| {
                let widget = text_input(placeholder, value)
                    .size(metrics.text_sm)
                    .on_input(move |value| Message::GraphFilterInput {
                        tab,
                        incarnation,
                        field,
                        value,
                    })
                    .on_submit(Message::GraphCommand {
                        tab,
                        command: GraphCommand::ApplyFilters,
                    });
                crate::workspace::bounds::observe(widget, move |bounds| {
                    Message::GraphFilterBounds { tab, field, bounds }
                })
            };
        content = content.push(
            row![
                input("Tag", &state.filter.tag, FilterField::Tag),
                input("Path prefix", &state.filter.path, FilterField::Path),
                input(
                    "Property",
                    &state.filter.property_key,
                    FilterField::PropertyKey
                ),
                input(
                    "Value (e.g. true)",
                    &state.filter.property_value,
                    FilterField::PropertyValue
                )
            ]
            .spacing(4),
        );
        content = content.push(
            row![
                input(
                    "Saved query (e.g. text:idea)",
                    &state.filter.query,
                    FilterField::Query
                ),
                action("Apply", GraphCommand::ApplyFilters)
            ]
            .spacing(4),
        );
        content = content.push(
            row![
                checkbox(state.filter.attachments)
                    .label("Attachments")
                    .on_toggle(move |value| command(GraphCommand::Attachments(value))),
                checkbox(state.filter.unresolved)
                    .label("Unresolved")
                    .on_toggle(move |value| command(GraphCommand::Unresolved(value))),
                checkbox(state.filter.embeds)
                    .label("Embeds")
                    .on_toggle(move |value| command(GraphCommand::Embeds(value))),
                checkbox(state.filter.any)
                    .label("Match any")
                    .on_toggle(move |value| command(GraphCommand::Any(value))),
                checkbox(state.filter.negate)
                    .label("Not")
                    .on_toggle(move |value| command(GraphCommand::Negate(value)))
            ]
            .spacing(8),
        );
    }
    if let Some(neighborhood) = &state.scope.neighborhood {
        let direction = match neighborhood.direction {
            Direction::In => "In",
            Direction::Out => "Out",
            Direction::Both => "Both",
        };
        content = content.push(
            row![
                text("Depth"),
                pick_list(
                    vec![0u8, 1, 2, 3, 4, 5, 6],
                    Some(neighborhood.depth),
                    move |depth| command(GraphCommand::Depth(depth))
                ),
                pick_list(
                    vec!["In", "Out", "Both"],
                    Some(direction),
                    move |direction| command(GraphCommand::Direction(match direction {
                        "In" => Direction::In,
                        "Out" => Direction::Out,
                        _ => Direction::Both,
                    }))
                )
            ]
            .spacing(4),
        );
    }
    if let Some(error) = &state.filter_error {
        content = content.push(text(error).size(metrics.text_sm));
    }
    content.width(Length::Fill).into()
}
