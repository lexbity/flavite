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

use crate::app::Message;
use crate::style;
use iced::widget::{column, container, text};
use iced::{Element, Length, alignment};

/// A centered placeholder used by every view kind for honest empty,
/// loading, and unavailable states.
pub fn empty_state(title: &str, hint: &str) -> Element<'static, Message> {
    container(
        column![
            text(title.to_owned()).size(style::HEADING),
            text(hint.to_owned()).style(style::dim_text),
        ]
        .spacing(style::SPACING_SM),
    )
    .width(Length::Fill)
    .height(Length::Fill)
    .padding(style::SPACING_LG)
    .style(style::content)
    .center_x(Length::Fill)
    .align_y(alignment::Vertical::Top)
    .into()
}

pub mod outline;
pub mod properties;
