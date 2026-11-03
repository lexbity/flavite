//! Context tool header: explicit source label and binding selector
//! (RFC-0003 §20, W04).
//!
//! The source label always reveals the effective choice (D-04); a tool can
//! be moved to any group and keeps following or pinning correctly because
//! bindings reference identities, not widget positions.

use crate::app::{App, Message};
use crate::host::context::ToolState;
use crate::style;
use crate::workspace::tab_strip;
use flavite_workspace::{ContextBinding, ContextRouter, ContextSource, TabId, ViewKind};
use iced::widget::{button, column, container, pick_list, row, text};
use iced::{Element, Length};

/// The binding a context tool tab currently carries.
#[must_use]
pub fn binding_of(app: &App, tab: TabId) -> Option<ContextBinding> {
    app.workspace.binding_of(&tab).cloned()
}

/// Human label for the effective source (D-04: the label always reveals
/// the effective choice, including unbound states).
#[must_use]
pub fn source_label(app: &App, tab: TabId) -> String {
    let Some(binding) = binding_of(app, tab) else {
        return "Context".into();
    };
    match &binding {
        ContextBinding::FollowDocument => match ContextRouter::resolve(&app.workspace, &binding) {
            ContextSource::Document { locator, .. } => {
                format!("Following: {}", locator.title())
            }
            _ => "Following: Document (none focused)".into(),
        },
        ContextBinding::FollowExplorationOrDocument => {
            match ContextRouter::resolve(&app.workspace, &binding) {
                ContextSource::Exploration { locator, .. } => {
                    format!("Following: Selection ({})", locator.title())
                }
                ContextSource::Document { locator, .. } => {
                    format!("Following: {}", locator.title())
                }
                ContextSource::Unbound { .. } => "Following: Selection or document".into(),
                ContextSource::Pinned { locator } => format!("Pinned: {}", locator.title()),
            }
        }
        ContextBinding::FollowSource { tab } => {
            let title = app
                .workspace
                .tab(tab)
                .map(|session| session.title())
                .unwrap_or_else(|| "closed source".into());
            format!("Following: {title}")
        }
        ContextBinding::Pinned(locator) => format!("Pinned: {}", locator.path().title()),
    }
}

/// Status line for the tool's query state machine.
#[must_use]
pub fn status_label(app: &App, tab: TabId) -> String {
    let Some(state) = app.contexts.state(&tab) else {
        return String::new();
    };
    match state {
        ToolState::Idle => "Loading…".into(),
        ToolState::Querying => "Updating…".into(),
        ToolState::Busy => "Query queued; the service is busy".into(),
        ToolState::Ready(content) => match content {
            crate::host::context::ToolContent::Document(doc) => {
                if app.contexts.is_updating(&tab) {
                    "Updating…".into()
                } else if crate::host::reading::unsaved(app, doc) {
                    "Current note · unsaved changes".into()
                } else {
                    "Saved content".into()
                }
            }
            crate::host::context::ToolContent::BufferPreview { unsaved, .. } => {
                if *unsaved {
                    "Current note · unsaved changes".into()
                } else {
                    "Current note · saved".into()
                }
            }
            crate::host::context::ToolContent::Preview { .. } => "Saved text".into(),
            crate::host::context::ToolContent::Backlinks { rows, .. } => format!(
                "{} linking notes · {} unsaved",
                rows.len(),
                rows.iter().filter(|r| r.unsaved).count()
            ),
        },
        ToolState::Missing => "Missing note".into(),
        ToolState::NoBacklinks => "No backlinks".into(),
        ToolState::Failed { message } => format!("Query failed: {message}"),
        ToolState::Unbound { .. } => match ContextRouter::resolve(
            &app.workspace,
            &binding_of(app, tab).unwrap_or(ContextBinding::FollowDocument),
        ) {
            ContextSource::Unbound { reason } => match reason {
                flavite_workspace::UnboundReason::NoDocumentAnchor => {
                    "Unbound: focus a note to follow it".into()
                }
                flavite_workspace::UnboundReason::SourceClosed(_) => {
                    "Unbound: the followed source is closed".into()
                }
                flavite_workspace::UnboundReason::NoExploration => {
                    "Unbound: no selection published".into()
                }
                flavite_workspace::UnboundReason::EmptySelection => {
                    "No selection — the source published an empty selection".into()
                }
            },
            _ => "Unbound".into(),
        },
    }
}

/// One header row: source label, status, and the Follow/Pin binding
/// selector. Rendered above every Preview/Backlinks view.
pub fn view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let metrics = app.appearance.metrics();
    let label = source_label(app, tab);
    let status = status_label(app, tab);
    let status = if app.contexts.is_updating(&tab) {
        format!("Updating · {status}")
    } else {
        status
    };
    let is_pinned = matches!(binding_of(app, tab), Some(ContextBinding::Pinned(_)));
    let mut controls = row![].spacing(metrics.spacing_sm);
    if is_pinned {
        controls = controls.push(
            button(text("Follow document").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::SetContextBinding {
                    tab,
                    binding: ContextBinding::FollowDocument,
                }),
        );
    } else {
        controls = controls.push(
            button(text("Pin context").size(metrics.text_sm))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press_maybe(
                    app.contexts
                        .observed(&tab)
                        .map(|source| flavite_workspace::EntityLocator::new(source.locator.clone()))
                        .map(|locator| Message::SetContextBinding {
                            tab,
                            binding: ContextBinding::Pinned(locator),
                        }),
                ),
        );
    }
    let mut choices = vec![
        BindingChoice {
            label: "Follow document".into(),
            binding: ContextBinding::FollowDocument,
        },
        BindingChoice {
            label: "Follow selection or document".into(),
            binding: ContextBinding::FollowExplorationOrDocument,
        },
    ];
    for (index, (source, session)) in app
        .workspace
        .tabs()
        .filter(|(_, session)| flavite_workspace::is_source_kind(session.kind()))
        .enumerate()
    {
        choices.push(BindingChoice {
            label: format!("{} · view {}", session.title(), index + 1),
            binding: ContextBinding::FollowSource { tab: *source },
        });
    }
    let selected = binding_of(app, tab).map(|binding| {
        choices
            .iter()
            .find(|choice| choice.binding == binding)
            .cloned()
            .unwrap_or(BindingChoice {
                label: label.clone(),
                binding,
            })
    });
    if let Some(selected) = &selected
        && !choices.contains(selected)
    {
        choices.push(selected.clone());
    }
    let selector = pick_list(choices, selected, move |choice| {
        Message::SetContextBinding {
            tab,
            binding: choice.binding,
        }
    })
    .width(Length::Fill)
    .text_size(metrics.text_sm);
    container(
        column![
            row![selector, controls].spacing(8),
            text(label).size(metrics.text_sm),
            text(status).size(metrics.text_sm).style(style::dim_text)
        ]
        .spacing(4),
    )
    .padding(8)
    .style(style::toolbar)
    .into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BindingChoice {
    label: String,
    binding: ContextBinding,
}
impl std::fmt::Display for BindingChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.label)
    }
}

/// Whether a tab is a context tool at all (shared guard for commands).
#[must_use]
pub fn is_context_tab(app: &App, tab: TabId) -> bool {
    app.workspace.tab(&tab).is_some_and(|session| {
        matches!(
            session.kind(),
            ViewKind::Preview(_)
                | ViewKind::Backlinks(_)
                | ViewKind::Outline(_)
                | ViewKind::Properties(_)
        )
    })
}
