//! Shared compact shell controls. Icons are drawn as vectors, without font glyphs.

use crate::style::Metrics;
use crate::{app::Message, style};
use iced::advanced::{Layout, Widget, layout, mouse, renderer, widget};
use iced::widget::{button, container, tooltip};
use iced::{Background, Border, Element, Length, Rectangle, Renderer, Size, Theme};

#[derive(Debug, Clone, Copy)]
pub enum Icon {
    Menu,
    Sidebar,
    Search,
    Commands,
    Back,
    Forward,
    Graph,
    Read,
    Edit,
    Note,
    Notes,
    Add,
    More,
    Tabs,
    Close,
    SplitRight,
    SplitDown,
    Maximize,
    Settings,
    Cut,
    Copy,
    Paste,
    Undo,
    Redo,
}

/// A vector icon drawn at half the scaled icon-button target (16 px at 100%).
#[derive(Debug, Clone, Copy)]
pub struct Glyph {
    kind: Icon,
    size: f32,
    /// An explicit ink color for themed surfaces (xpi themes); `None`
    /// follows the active palette text.
    ink: Option<iced::Color>,
}

pub fn icon<'a>(metrics: Metrics, kind: Icon) -> Element<'a, Message> {
    Element::new(Glyph {
        kind,
        size: metrics.icon_target * 0.5,
        ink: None,
    })
}

pub fn icon_button(
    metrics: Metrics,
    kind: Icon,
    label: &str,
    message: Option<Message>,
) -> Element<'_, Message> {
    themed_icon_button(metrics, kind, label, message, None)
}

/// An icon button for the themed top bar: the glyph and its hover wash
/// carry the theme's frame text color when the package supplies one.
pub fn themed_icon_button(
    metrics: Metrics,
    kind: Icon,
    label: &str,
    message: Option<Message>,
    ink: Option<iced::Color>,
) -> Element<'_, Message> {
    tooltip(
        button(
            container(Element::new(Glyph {
                kind,
                size: metrics.icon_target * 0.5,
                ink,
            }))
            .center(Length::Fixed(metrics.icon_target * 0.5)),
        )
        .width(metrics.icon_target)
        .height(metrics.icon_target)
        .padding(4)
        .style(move |theme, status| match ink {
            Some(_) => style::top_bar_button(ink)(theme, status),
            None => style::toolbar_button(theme, status),
        })
        .on_press_maybe(message),
        container(iced::widget::text(label).size(metrics.text_sm))
            .padding(6)
            .style(style::card),
        tooltip::Position::Bottom,
    )
    .into()
}

impl Widget<Message, Theme, Renderer> for Glyph {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.size), Length::Fixed(self.size))
    }
    fn layout(
        &mut self,
        _tree: &mut widget::Tree,
        _renderer: &Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(Size::new(self.size, self.size))
    }
    fn draw(
        &self,
        _tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        use iced::advanced::Renderer as _;
        let p = layout.bounds();
        // Glyph geometry is authored on a 16 px grid and scales with the
        // laid-out size.
        let s = p.width / 16.0;
        let ink = self.ink.unwrap_or_else(|| theme.palette().text);
        let mut line = |x: f32, y: f32, w: f32, h: f32| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: p.x + x * s,
                        y: p.y + y * s,
                        width: w * s,
                        height: h * s,
                    },
                    ..renderer::Quad::default()
                },
                ink,
            );
        };
        match self.kind {
            Icon::Cut => {
                for (x, y, w, h) in [
                    (2.0, 10.0, 4.0, 1.0),
                    (2.0, 14.0, 4.0, 1.0),
                    (2.0, 10.0, 1.0, 5.0),
                    (6.0, 10.0, 1.0, 5.0),
                    (10.0, 10.0, 4.0, 1.0),
                    (10.0, 14.0, 4.0, 1.0),
                    (10.0, 10.0, 1.0, 5.0),
                    (14.0, 10.0, 1.0, 5.0),
                ] {
                    line(x, y, w, h);
                }
                for i in 0..9_u16 {
                    let t = f32::from(i);
                    line(3.0 + t, 2.0 + t, 1.0, 1.0);
                    line(12.0 - t, 2.0 + t, 1.0, 1.0);
                }
            }
            Icon::Copy | Icon::Paste => {
                for (x, y, w, h) in [
                    (5.0, 4.0, 9.0, 1.0),
                    (5.0, 14.0, 9.0, 1.0),
                    (5.0, 4.0, 1.0, 11.0),
                    (13.0, 4.0, 1.0, 11.0),
                ] {
                    line(x, y, w, h);
                }
                if matches!(self.kind, Icon::Copy) {
                    line(2.0, 1.0, 9.0, 1.0);
                    line(2.0, 1.0, 1.0, 11.0);
                } else {
                    line(7.0, 2.0, 5.0, 3.0);
                    line(7.0, 8.0, 5.0, 1.0);
                    line(7.0, 11.0, 5.0, 1.0);
                }
            }
            Icon::Undo | Icon::Redo => {
                let flipped = matches!(self.kind, Icon::Redo);
                for (x, y, w, h) in [
                    (2.0, 5.0, 10.0, 1.0),
                    (12.0, 5.0, 1.0, 7.0),
                    (6.0, 12.0, 7.0, 1.0),
                ] {
                    line(if flipped { 16.0 - x - w } else { x }, y, w, h);
                }
                for i in 0..4_u16 {
                    let t = f32::from(i);
                    let x = 2.0 + t;
                    line(if flipped { 15.0 - x } else { x }, 5.0 - t, 1.0, 1.0);
                    line(if flipped { 15.0 - x } else { x }, 5.0 + t, 1.0, 1.0);
                }
            }
            Icon::Menu => {
                for y in [3.0, 7.5, 12.0] {
                    line(2.0, y, 12.0, 1.0);
                }
            }
            Icon::Sidebar => {
                line(1.0, 2.0, 14.0, 1.0);
                line(1.0, 13.0, 14.0, 1.0);
                for x in [1.0, 5.0, 14.0] {
                    line(x, 2.0, 1.0, 12.0);
                }
            }
            Icon::Search => {
                for (x, y, w, h) in [
                    (3.0, 2.0, 7.0, 1.0),
                    (2.0, 3.0, 1.0, 6.0),
                    (10.0, 3.0, 1.0, 6.0),
                    (3.0, 9.0, 7.0, 1.0),
                ] {
                    line(x, y, w, h);
                }
                for i in 0_u16..5 {
                    let at = f32::from(i);
                    line(9.0 + at, 9.0 + at, 1.5, 1.5);
                }
            }
            Icon::Commands => {
                for i in 0_u16..5 {
                    let at = f32::from(i);
                    line(2.0 + at, 3.0 + at, 1.0, 1.0);
                    line(6.0 - at, 7.0 + at, 1.0, 1.0);
                }
                line(9.0, 11.0, 5.0, 1.0);
            }
            Icon::Back | Icon::Forward => {
                line(2.0, 7.0, 12.0, 1.0);
                for i in 0_u16..6 {
                    let at = f32::from(i);
                    let x = if matches!(self.kind, Icon::Back) {
                        2.0 + at
                    } else {
                        13.0 - at
                    };
                    line(x, 7.0 - at, 1.0, 1.0);
                    line(x, 7.0 + at, 1.0, 1.0);
                }
            }
            Icon::Graph => {
                line(3.0, 2.0, 3.0, 3.0);
                line(10.0, 10.0, 3.0, 3.0);
                line(1.0, 11.0, 3.0, 3.0);
                line(4.0, 4.0, 1.0, 8.0);
                line(4.0, 10.0, 7.0, 1.0);
            }
            Icon::Read => {
                for x in [1.0, 7.5, 14.0] {
                    line(x, 3.0, 1.0, 10.0);
                }
                line(1.0, 3.0, 14.0, 1.0);
                line(1.0, 12.0, 14.0, 1.0);
            }
            Icon::Edit => {
                for i in 0_u16..11 {
                    let at = f32::from(i);
                    line(2.0 + at, 13.0 - at, 2.0, 2.0);
                }
                line(1.0, 14.0, 4.0, 1.0);
            }
            Icon::Add => {
                line(7.25, 2.0, 1.5, 12.0);
                line(2.0, 7.25, 12.0, 1.5);
            }
            Icon::More => {
                for x in [2.0, 7.0, 12.0] {
                    line(x, 7.0, 2.0, 2.0);
                }
            }
            Icon::Close => {
                for i in 0_u16..10 {
                    let x = f32::from(i);
                    line(3.0 + x, 3.0 + x, 1.5, 1.5);
                    line(12.0 - x, 3.0 + x, 1.5, 1.5);
                }
            }
            Icon::Note => {
                line(3.0, 1.0, 9.0, 1.0);
                line(3.0, 1.0, 1.0, 14.0);
                line(12.0, 1.0, 1.0, 14.0);
                line(3.0, 14.0, 10.0, 1.0);
                line(6.0, 6.0, 4.0, 1.0);
                line(6.0, 9.0, 4.0, 1.0);
            }
            Icon::Notes => {
                line(1.0, 4.0, 14.0, 1.0);
                line(1.0, 4.0, 1.0, 10.0);
                line(14.0, 4.0, 1.0, 10.0);
                line(1.0, 13.0, 14.0, 1.0);
                line(2.0, 2.0, 5.0, 1.0);
            }
            // Three slider rails with staggered knobs (settings).
            Icon::Settings => {
                line(2.0, 4.0, 12.0, 1.0);
                line(2.0, 8.0, 12.0, 1.0);
                line(2.0, 12.0, 12.0, 1.0);
                line(9.0, 2.5, 2.0, 4.0);
                line(4.0, 6.5, 2.0, 4.0);
                line(10.0, 10.5, 2.0, 4.0);
            }
            kind => {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: p.x + 2.0 * s,
                            y: p.y + 2.0 * s,
                            width: 12.0 * s,
                            height: 12.0 * s,
                        },
                        border: Border {
                            color: ink,
                            width: 1.0,
                            ..Border::default()
                        },
                        ..renderer::Quad::default()
                    },
                    Background::Color(iced::Color::TRANSPARENT),
                );
                match kind {
                    Icon::SplitRight | Icon::Tabs => renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: p.x + 7.0 * s,
                                y: p.y + 3.0 * s,
                                width: 1.0 * s,
                                height: 10.0 * s,
                            },
                            ..renderer::Quad::default()
                        },
                        ink,
                    ),
                    Icon::SplitDown => renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: p.x + 3.0 * s,
                                y: p.y + 7.0 * s,
                                width: 10.0 * s,
                                height: 1.0 * s,
                            },
                            ..renderer::Quad::default()
                        },
                        ink,
                    ),
                    _ => {}
                }
            }
        }
    }
}
