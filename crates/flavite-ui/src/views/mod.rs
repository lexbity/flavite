//! Workspace view renderers (RFC-0003 W03/W04 view layout).
//!
//! Each view kind renders through a module here; the shell
//! ([`crate::workspace_shell`]) only routes to the active view. Shared
//! building blocks live at the module root.

pub mod backlinks;
pub mod context_header;
pub mod graph;
pub mod history;
pub mod note;
pub mod notes;
pub mod notifications;
pub mod preview;
pub mod search;
pub mod settings;

use crate::app::Message;
use crate::style::{self, Metrics};
use iced::widget::{column, container, text};
use iced::{Element, Length, alignment};

/// A centered placeholder used by every view kind for honest empty,
/// loading, and unavailable states.
pub fn empty_state(metrics: Metrics, title: &str, hint: &str) -> Element<'static, Message> {
    container(
        column![
            text(title.to_owned()).size(metrics.heading),
            text(hint.to_owned()).style(style::dim_text),
        ]
        .spacing(metrics.spacing_sm),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(metrics.spacing_lg)
    .style(style::content)
    .center_x(Length::Fill)
    .align_y(alignment::Vertical::Top)
    .into()
}

pub mod outline;
pub mod properties;
