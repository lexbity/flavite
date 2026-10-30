//! `iced_winit::Program` adapter for the actual Flavite application shell.
//! It delegates UI behavior to `App`; only native semantics are added here.

use crate::{
    accessibility,
    app::{App, Message},
    style, workspace_shell,
};
use accesskit::{ActionRequest, TreeUpdate};
use iced::{Element, Settings, Size, Theme, window};
use iced_winit::program::{self, core, runtime};

pub struct FlaviteProgram;

impl program::Program for FlaviteProgram {
    type State = App;
    type Message = Message;
    type Theme = Theme;
    type Renderer = iced::Renderer;
    type Executor = iced::executor::Default;

    fn name() -> &'static str {
        "flavite"
    }

    fn settings(&self) -> Settings {
        Settings::default()
    }

    fn window(&self) -> Option<window::Settings> {
        Some(window::Settings {
            size: Size::new(1180.0, 760.0),
            // D-13: the supported default minimum window is 640×480.
            min_size: Some(Size::new(640.0, 480.0)),
            ..window::Settings::default()
        })
    }

    fn boot(&self) -> (App, runtime::Task<Message>) {
        (App::boot(), runtime::Task::none())
    }

    fn update(&self, state: &mut App, message: Message) -> runtime::Task<Message> {
        state.update(message)
    }

    fn view<'a>(
        &self,
        state: &'a App,
        _window: core::window::Id,
    ) -> Element<'a, Message, Theme, iced::Renderer> {
        workspace_shell::view(state)
    }

    fn accessibility_tree(&self, state: &App, _window: core::window::Id) -> Option<TreeUpdate> {
        Some(accessibility::tree::shell_tree(state))
    }

    fn accessibility_action(
        &self,
        state: &App,
        _window: core::window::Id,
        request: ActionRequest,
    ) -> Option<Message> {
        accessibility::route_action(state, request)
    }

    fn title(&self, _state: &App, _window: core::window::Id) -> String {
        "Flavite".into()
    }

    fn subscription(&self, state: &App) -> iced::Subscription<Message> {
        App::subscription(state)
    }

    fn theme(&self, _state: &App, _window: core::window::Id) -> Option<Theme> {
        Some(style::theme())
    }
}
