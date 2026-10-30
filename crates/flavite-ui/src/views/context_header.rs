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
use iced::widget::{button, container, row, text};
use iced::{Element, Length, alignment};

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
            crate::host::context::ToolContent::Preview { provenance, .. } => format!(
                "Saved text · index epoch {} generation {}",
                provenance.epoch, provenance.generation
            ),
            crate::host::context::ToolContent::Backlinks { rows, provenance } => format!(
                "{} link(s) · index epoch {} generation {}",
                rows.len(),
                provenance.epoch,
                provenance.generation
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
            },
            _ => "Unbound".into(),
        },
    }
}

/// One header row: source label, status, and the Follow/Pin binding
/// selector. Rendered above every Preview/Backlinks view.
pub fn view<'a>(app: &'a App, tab: TabId) -> Element<'a, Message> {
    let label = source_label(app, tab);
    let status = status_label(app, tab);
    let is_pinned = matches!(binding_of(app, tab), Some(ContextBinding::Pinned(_)));
    let mut controls = row![].spacing(style::SPACING_SM);
    if is_pinned {
        controls = controls.push(
            button(text("Follow document").size(style::TEXT_SM))
                .padding(tab_strip::chip_padding())
                .style(style::ghost())
                .on_press(Message::SetContextBinding {
                    tab,
                    binding: ContextBinding::FollowDocument,
                }),
        );
    } else {
        controls = controls.push(
            button(text("Pin source").size(style::TEXT_SM))
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
    container(
        row![
            text(label).size(style::TEXT_SM).width(Length::Fill),
            text(status).size(style::TEXT_SM).style(style::dim_text),
            controls,
        ]
        .spacing(style::SPACING)
        .align_y(alignment::Vertical::Center),
    )
    .height(32)
    .padding([0, 12])
    .align_y(alignment::Vertical::Center)
    .style(style::toolbar)
    .into()
}

/// Whether a tab is a context tool at all (shared guard for commands).
#[must_use]
pub fn is_context_tab(app: &App, tab: TabId) -> bool {
    app.workspace.tab(&tab).is_some_and(|session| {
        matches!(
            session.kind(),
            ViewKind::Preview(_) | ViewKind::Backlinks(_)
        )
    })
}
