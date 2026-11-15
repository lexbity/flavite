//! Client-side window chrome (W12): the shell draws its own title bar, so
//! the minimize/maximize/close controls, the drag region, and the maximize
//! state all resolve through these tasks. One window exists (the shell), so
//! every control targets the oldest window.

use crate::app::Message;
use iced::Task;

/// The only window of the application.
fn oldest_window() -> Task<Option<iced::window::Id>> {
    iced::window::oldest()
}

/// Minimize the window from its shell control.
pub fn minimize() -> Task<Message> {
    oldest_window().and_then(|id| iced::window::minimize::<Message>(id, true))
}

/// Toggle maximize from the shell control or a double-click on the bar, then
/// re-poll the state so the control's icon follows.
pub fn toggle_maximize() -> Task<Message> {
    oldest_window()
        .and_then(iced::window::toggle_maximize::<Message>)
        .chain(poll_maximized())
}

/// Begin an interactive move from the shell's drag region.
pub fn drag() -> Task<Message> {
    oldest_window().and_then(iced::window::drag::<Message>)
}

/// Query whether the window is currently maximized, so the shell control can
/// render the restore variant.
pub fn poll_maximized() -> Task<Message> {
    oldest_window().and_then(|id| iced::window::is_maximized(id).map(Message::WindowMaximized))
}
