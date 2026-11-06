//! Observe actual mounted rectangles, including their window coordinates.
//! Observations are geometry only: they never change workspace identity.

use crate::app::App;
use crate::app::Message;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer, widget};
use iced::{Element, Event, Length, Rectangle, Renderer, Theme, Vector};

pub fn observe<'a>(
    content: impl Into<Element<'a, Message>>,
    on_bounds: impl Fn(Rectangle) -> Message + 'a,
) -> Element<'a, Message> {
    Element::new(Bounds {
        content: content.into(),
        on_bounds: Box::new(on_bounds),
        split_hits: Vec::new(),
        resizing: false,
        identity: None,
    })
}

/// Dynamic rows must acknowledge geometry again when their identity changes,
/// even when their new rectangle happens to equal the previous row's.
pub fn observe_named<'a>(
    content: impl Into<Element<'a, Message>>,
    identity: String,
    on_bounds: impl Fn(Rectangle) -> Message + 'a,
) -> Element<'a, Message> {
    Element::new(Bounds {
        content: content.into(),
        on_bounds: Box::new(on_bounds),
        split_hits: Vec::new(),
        resizing: false,
        identity: Some(identity),
    })
}

/// Splitter capture belongs to the host, using the projected geometry. It
/// prevents editor input underneath the full 8px splitter hit corridor.
pub fn workspace<'a>(content: impl Into<Element<'a, Message>>, app: &App) -> Element<'a, Message> {
    Element::new(Bounds {
        content: content.into(),
        on_bounds: Box::new(Message::WorkspaceBounds),
        split_hits: app.split_hits(),
        resizing: app.resizing_split.is_some(),
        identity: None,
    })
}

struct Bounds<'a> {
    content: Element<'a, Message>,
    on_bounds: Box<dyn Fn(Rectangle) -> Message + 'a>,
    split_hits: Vec<(
        flavite_workspace::SplitId,
        Rectangle,
        iced::widget::pane_grid::Axis,
    )>,
    resizing: bool,
    identity: Option<String>,
}

#[derive(Default)]
struct BoundsState {
    identity: Option<String>,
    rect: Option<Rectangle>,
}

impl Widget<Message, Theme, Renderer> for Bounds<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<BoundsState>()
    }
    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(BoundsState {
            identity: self.identity.clone(),
            rect: None,
        })
    }
    fn children(&self) -> Vec<widget::Tree> {
        vec![widget::Tree::new(&self.content)]
    }
    fn diff(&self, tree: &mut widget::Tree) {
        let state = tree.state.downcast_mut::<BoundsState>();
        if state.identity != self.identity {
            state.identity.clone_from(&self.identity);
            state.rect = None;
        }
        tree.diff_children(&[&self.content]);
    }
    fn size(&self) -> iced::Size<Length> {
        self.content.as_widget().size()
    }
    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if let Event::Mouse(mouse_event) = event {
            if matches!(
                mouse_event,
                mouse::Event::ButtonPressed(mouse::Button::Left)
            ) {
                if let Some((split, _, _)) = self
                    .split_hits
                    .iter()
                    .find(|(_, rect, _)| cursor.position().is_some_and(|p| rect.contains(p)))
                {
                    shell.publish(Message::ResizeBegin(*split));
                    shell.capture_event();
                    return;
                }
            } else if self.resizing
                && matches!(
                    mouse_event,
                    mouse::Event::CursorMoved { .. }
                        | mouse::Event::ButtonReleased(mouse::Button::Left)
                )
            {
                shell.capture_event();
                return;
            }
        }
        if matches!(
            event,
            Event::Window(iced::window::Event::RedrawRequested(_))
        ) {
            let state = tree.state.downcast_mut::<BoundsState>();
            if state.rect != Some(layout.bounds()) {
                state.rect = Some(layout.bounds());
                shell.publish((self.on_bounds)(layout.bounds()));
            }
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }
    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        if let Some((_, _, axis)) = self
            .split_hits
            .iter()
            .find(|(_, rect, _)| cursor.position().is_some_and(|p| rect.contains(p)))
        {
            return match axis {
                iced::widget::pane_grid::Axis::Vertical => mouse::Interaction::ResizingHorizontally,
                iced::widget::pane_grid::Axis::Horizontal => mouse::Interaction::ResizingVertically,
            };
        }
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut widget::Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}
