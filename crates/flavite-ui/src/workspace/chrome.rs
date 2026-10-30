//! Shared compact shell controls. Icons are drawn as vectors, without font glyphs.

use crate::{app::Message, style};
use iced::advanced::{Layout, Widget, layout, mouse, renderer, widget};
use iced::widget::{button, container, tooltip};
use iced::{Background, Border, Element, Length, Rectangle, Renderer, Size, Theme};

#[derive(Debug, Clone, Copy)]
pub enum Icon {
    Note,
    Notes,
    Add,
    More,
    Tabs,
    Close,
    SplitRight,
    SplitDown,
    Maximize,
}

pub fn icon<'a>(kind: Icon) -> Element<'a, Message> {
    Element::new(kind)
}

pub fn icon_button(kind: Icon, label: &str, message: Option<Message>) -> Element<'_, Message> {
    tooltip(
        button(container(icon(kind)).center(Length::Fixed(24.0)))
            .width(32)
            .height(32)
            .padding(4)
            .style(style::toolbar_button)
            .on_press_maybe(message),
        container(iced::widget::text(label).size(style::TEXT_SM))
            .padding(6)
            .style(style::card),
        tooltip::Position::Bottom,
    )
    .into()
}

impl Widget<Message, Theme, Renderer> for Icon {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(16.0), Length::Fixed(16.0))
    }
    fn layout(
        &mut self,
        _tree: &mut widget::Tree,
        _renderer: &Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(Size::new(16.0, 16.0))
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
        let mut line = |x: f32, y: f32, w: f32, h: f32| {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: p.x + x,
                        y: p.y + y,
                        width: w,
                        height: h,
                    },
                    ..renderer::Quad::default()
                },
                theme.palette().text,
            );
        };
        match self {
            Self::Add => {
                line(7.25, 2.0, 1.5, 12.0);
                line(2.0, 7.25, 12.0, 1.5);
            }
            Self::More => {
                for x in [2.0, 7.0, 12.0] {
                    line(x, 7.0, 2.0, 2.0);
                }
            }
            Self::Close => {
                for i in 0_u16..10 {
                    let x = f32::from(i);
                    line(3.0 + x, 3.0 + x, 1.5, 1.5);
                    line(12.0 - x, 3.0 + x, 1.5, 1.5);
                }
            }
            Self::Note => {
                line(3.0, 1.0, 9.0, 1.0);
                line(3.0, 1.0, 1.0, 14.0);
                line(12.0, 1.0, 1.0, 14.0);
                line(3.0, 14.0, 10.0, 1.0);
                line(6.0, 6.0, 4.0, 1.0);
                line(6.0, 9.0, 4.0, 1.0);
            }
            Self::Notes => {
                line(1.0, 4.0, 14.0, 1.0);
                line(1.0, 4.0, 1.0, 10.0);
                line(14.0, 4.0, 1.0, 10.0);
                line(1.0, 13.0, 14.0, 1.0);
                line(2.0, 2.0, 5.0, 1.0);
            }
            kind => {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle {
                            x: p.x + 2.0,
                            y: p.y + 2.0,
                            width: 12.0,
                            height: 12.0,
                        },
                        border: Border {
                            color: theme.palette().text,
                            width: 1.0,
                            ..Border::default()
                        },
                        ..renderer::Quad::default()
                    },
                    Background::Color(iced::Color::TRANSPARENT),
                );
                match kind {
                    Self::SplitRight | Self::Tabs => renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: p.x + 7.0,
                                y: p.y + 3.0,
                                width: 1.0,
                                height: 10.0,
                            },
                            ..renderer::Quad::default()
                        },
                        theme.palette().text,
                    ),
                    Self::SplitDown => renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle {
                                x: p.x + 3.0,
                                y: p.y + 7.0,
                                width: 10.0,
                                height: 1.0,
                            },
                            ..renderer::Quad::default()
                        },
                        theme.palette().text,
                    ),
                    _ => {}
                }
            }
        }
    }
}
