//! Custom iced editor widget over a revisioned, immutable API view.

use crate::keymap::KeyAction;
use crate::surface::{EditorAction, EditorSurface, EditorView};
use crate::viewport::{
    Viewport, clamp_char_boundary, next_grapheme_boundary, previous_grapheme_boundary,
};
use cosmic_text::{Attrs, Buffer as ShapingBuffer, FontSystem, Metrics, Shaping};
use flavite_api::BufferView;
use iced::advanced::{
    Clipboard, Layout, Renderer as AdvancedRenderer, Shell, Widget,
    layout::{Limits, Node},
    mouse, renderer, text,
    text::Renderer as TextRenderer,
    widget::Tree,
};
use iced::keyboard::{self, Key, Modifiers, key::Named};
use iced::{Background, Border, Color, Length, Point, Rectangle, Size, Theme};
use std::cell::RefCell;
use std::marker::PhantomData;

const FONT_SIZE: f32 = 16.0;
const LINE_HEIGHT: f32 = 22.0;
const PAD_X: f32 = 12.0;
const PAD_Y: f32 = 10.0;

thread_local! {
    static FONT_SYSTEM: RefCell<FontSystem> = RefCell::new(FontSystem::new());
}

/// Custom editor surface. Mutations are emitted as revision-guarded actions;
/// this widget never mutates a buffer or accesses the filesystem itself.
pub struct EditorWidget<'a, Message> {
    view: EditorView,
    on_action: Box<dyn Fn(EditorAction) -> Message + 'a>,
    width: Length,
    height: Length,
    _message: PhantomData<Message>,
}

impl<'a, Message> EditorWidget<'a, Message> {
    /// Create an editor from the API's immutable revisioned view.
    pub fn new(view: &'a BufferView, on_action: impl Fn(EditorAction) -> Message + 'a) -> Self {
        Self {
            view: EditorView::from(view),
            on_action: Box::new(on_action),
            width: Length::Fill,
            height: Length::Fill,
            _message: PhantomData,
        }
    }

    /// Set the requested widget width.
    #[must_use]
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Set the requested widget height.
    #[must_use]
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }

    /// Build a headless state for embedding and deterministic tests.
    #[must_use]
    pub fn state_for_test() -> EditorState {
        EditorState::default()
    }

    fn emit(&self, shell: &mut Shell<'_, Message>, action: EditorAction) {
        shell.publish((self.on_action)(action));
    }

    fn execute_key(
        &self,
        state: &mut EditorState,
        key: &Key,
        modifiers: Modifiers,
        text: Option<&str>,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) -> bool {
        let action = key_action(key, modifiers, text);
        let range = state.selection();
        let (start, end) = range;
        let mut emitted = false;
        match action {
            KeyAction::Insert | KeyAction::Newline => {
                let replacement = if action == KeyAction::Newline {
                    "\n"
                } else {
                    text.unwrap_or("")
                };
                if !replacement.is_empty() {
                    let edit =
                        EditorAction::replace(&self.view, start, end, replacement.to_owned());
                    state.caret = start + replacement.len();
                    state.anchor = state.caret;
                    self.emit(shell, edit);
                    emitted = true;
                }
            }
            KeyAction::Previous | KeyAction::Next | KeyAction::Home | KeyAction::End => {
                let selected = start != end;
                let target = match action {
                    KeyAction::Previous if selected && !modifiers.shift() => start,
                    KeyAction::Next if selected && !modifiers.shift() => end,
                    KeyAction::Previous => previous_grapheme_boundary(&self.view.text, state.caret),
                    KeyAction::Next => next_grapheme_boundary(&self.view.text, state.caret),
                    KeyAction::Home => 0,
                    KeyAction::End => self.view.text.len(),
                    _ => state.caret,
                };
                state.move_to(target, modifiers.shift());
                emitted = true;
            }
            KeyAction::Backspace | KeyAction::Delete => {
                emitted = true;
                let (from, to) = if start != end {
                    (start, end)
                } else if action == KeyAction::Backspace {
                    (
                        previous_grapheme_boundary(&self.view.text, state.caret),
                        state.caret,
                    )
                } else {
                    (
                        state.caret,
                        next_grapheme_boundary(&self.view.text, state.caret),
                    )
                };
                if from < to {
                    self.emit(
                        shell,
                        EditorAction::replace(&self.view, from, to, String::new()),
                    );
                    state.caret = from;
                    state.anchor = from;
                    emitted = true;
                }
            }
            KeyAction::Undo | KeyAction::Redo => {
                // Undo and redo remain Session commands. The action variant is
                // represented through the same host callback in A7; the caller
                // can map it to the corresponding typed Session command.
                self.emit(
                    shell,
                    EditorAction::history(&self.view, action == KeyAction::Redo),
                );
                emitted = true;
            }
            KeyAction::Copy | KeyAction::Cut => {
                emitted = true;
                if start < end {
                    clipboard.write(
                        iced::advanced::clipboard::Kind::Standard,
                        self.view.text[start..end].to_owned(),
                    );
                    if action == KeyAction::Cut {
                        self.emit(
                            shell,
                            EditorAction::replace(&self.view, start, end, String::new()),
                        );
                        state.caret = start;
                        state.anchor = start;
                    }
                }
            }
            KeyAction::Paste => {
                emitted = true;
                if let Some(plain_text) = clipboard.read(iced::advanced::clipboard::Kind::Standard)
                    && !plain_text.is_empty()
                {
                    self.emit(
                        shell,
                        EditorAction::replace(&self.view, start, end, plain_text.clone()),
                    );
                    state.caret = start + plain_text.len();
                    state.anchor = state.caret;
                }
            }
            KeyAction::SelectAll => {
                state.anchor = 0;
                state.caret = self.view.text.len();
                emitted = true;
            }
            KeyAction::Ignore => {}
        }
        emitted
    }
}

/// Widget-local interaction state. Text and mutations remain Session-owned.
#[derive(Debug, Default)]
pub struct EditorState {
    caret: usize,
    anchor: usize,
    focused: bool,
    selecting: bool,
    viewport: Viewport,
}

impl EditorState {
    /// Caret byte offset.
    #[must_use]
    pub fn caret(&self) -> usize {
        self.caret
    }

    /// Current selection in normalized TextByte coordinates.
    #[must_use]
    pub fn selection(&self) -> (usize, usize) {
        (self.caret.min(self.anchor), self.caret.max(self.anchor))
    }

    fn move_to(&mut self, byte: usize, extend: bool) {
        self.caret = byte;
        if !extend {
            self.anchor = byte;
        }
    }
}

impl<Message: 'static> Widget<Message, Theme, iced::Renderer> for EditorWidget<'_, Message> {
    fn size(&self) -> Size<Length> {
        Size::new(self.width, self.height)
    }

    fn layout(&mut self, tree: &mut Tree, _renderer: &iced::Renderer, limits: &Limits) -> Node {
        let size = limits.resolve(self.width, self.height, Size::new(640.0, 480.0));
        tree.state
            .downcast_mut::<EditorState>()
            .viewport
            .set_height(size.height - PAD_Y * 2.0);
        Node::new(size)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<EditorState>();
        let bounds = layout.bounds();
        let clip = bounds
            .intersection(viewport)
            .unwrap_or(Rectangle::with_size(Size::ZERO));
        let text_layout = shape_text(&self.view.text, (bounds.width - PAD_X * 2.0).max(1.0));
        let runs = text_layout.layout_runs().collect::<Vec<_>>();
        let line_count = runs.len();
        let mut viewport_state = state.viewport;
        viewport_state.line_height = LINE_HEIGHT;
        viewport_state.set_height(bounds.height - PAD_Y * 2.0);
        let visible = viewport_state.visible_range(line_count);
        let (selection_start, selection_end) = state.selection();
        if selection_start < selection_end {
            for run in runs.iter().skip(visible.start).take(visible.len()) {
                let line_start = line_start_byte(&self.view.text, run.line_i);
                for glyph in run.glyphs {
                    let glyph_start = line_start + glyph.start;
                    let glyph_end = line_start + glyph.end;
                    if glyph_end > selection_start && glyph_start < selection_end {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: Rectangle::new(
                                    Point::new(
                                        bounds.x + PAD_X + glyph.x,
                                        bounds.y + PAD_Y + run.line_y - run.line_top,
                                    ),
                                    Size::new(glyph.w.max(1.0), LINE_HEIGHT),
                                ),
                                border: Border::default(),
                                ..renderer::Quad::default()
                            },
                            Background::Color(Color::from_rgba(0.24, 0.42, 0.67, 0.55)),
                        );
                    }
                }
            }
        }
        let displayed = runs
            .iter()
            .enumerate()
            .filter(|(index, _)| visible.contains(index))
            .map(|(_, run)| run.text.to_owned())
            .collect::<Vec<_>>()
            .join("\n");
        renderer.fill_text(
            text::Text {
                content: displayed,
                bounds: Size::new(bounds.width - PAD_X * 2.0, f32::INFINITY),
                size: iced::Pixels(FONT_SIZE),
                line_height: text::LineHeight::Absolute(iced::Pixels(LINE_HEIGHT)),
                font: iced::Font::MONOSPACE,
                align_x: text::Alignment::Left,
                align_y: iced::alignment::Vertical::Top,
                shaping: text::Shaping::Advanced,
                wrapping: text::Wrapping::WordOrGlyph,
            },
            Point::new(bounds.x + PAD_X, bounds.y + PAD_Y),
            Color::from_rgb(0.88, 0.90, 0.93),
            clip,
        );
        if state.focused {
            let (x, y) = caret_position(&runs, &self.view.text, state.caret);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(
                            bounds.x + PAD_X + x,
                            bounds.y + PAD_Y + y - state.viewport.first_line as f32 * LINE_HEIGHT,
                        ),
                        Size::new(1.5, LINE_HEIGHT),
                    ),
                    border: Border::default(),
                    ..renderer::Quad::default()
                },
                Background::Color(Color::from_rgb(0.45, 0.72, 1.0)),
            );
        }
    }

    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        iced::advanced::widget::tree::Tag::of::<EditorState>()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        iced::advanced::widget::tree::State::new(EditorState::default())
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &iced::Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_mut::<EditorState>();
        match event {
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(position) = cursor.position_in(layout.bounds()) {
                    state.focused = true;
                    state.selecting = true;
                    state.caret = hit_test(
                        &self.view.text,
                        position.x - PAD_X,
                        position.y - PAD_Y + state.viewport.first_line as f32 * LINE_HEIGHT,
                        (layout.bounds().width - PAD_X * 2.0).max(1.0),
                    );
                    state.anchor = state.caret;
                    shell.capture_event();
                }
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.selecting => {
                if let Some(position) = cursor.position_in(layout.bounds()) {
                    state.caret = hit_test(
                        &self.view.text,
                        position.x - PAD_X,
                        position.y - PAD_Y + state.viewport.first_line as f32 * LINE_HEIGHT,
                        (layout.bounds().width - PAD_X * 2.0).max(1.0),
                    );
                    shell.capture_event();
                }
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                state.selecting = false;
            }
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / LINE_HEIGHT,
                };
                let last_line = self.view.text.lines().count().saturating_sub(1);
                if lines > 0.0 {
                    state.viewport.first_line = state
                        .viewport
                        .first_line
                        .saturating_add(lines.ceil() as usize)
                        .min(last_line);
                } else {
                    state.viewport.first_line = state
                        .viewport
                        .first_line
                        .saturating_sub(lines.abs().ceil() as usize);
                }
                shell.capture_event();
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(_)) => {}
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                modifiers,
                text,
                ..
            }) if state.focused
                && self.execute_key(state, key, *modifiers, text.as_deref(), clipboard, shell) =>
            {
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn mouse_interaction(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        if cursor.is_over(layout.bounds()) {
            mouse::Interaction::Text
        } else {
            mouse::Interaction::default()
        }
    }
}

impl<Message> EditorSurface for EditorWidget<'_, Message> {
    fn view(&self) -> &EditorView {
        &self.view
    }
}

impl<'a, Message: 'static> From<EditorWidget<'a, Message>>
    for iced::Element<'a, Message, Theme, iced::Renderer>
{
    fn from(widget: EditorWidget<'a, Message>) -> Self {
        Self::new(widget)
    }
}

fn key_action(key: &Key, modifiers: Modifiers, text: Option<&str>) -> KeyAction {
    if modifiers.command() || modifiers.control() {
        return match key {
            Key::Character(c) if c.eq_ignore_ascii_case("a") => KeyAction::SelectAll,
            Key::Character(c) if c.eq_ignore_ascii_case("c") => KeyAction::Copy,
            Key::Character(c) if c.eq_ignore_ascii_case("x") => KeyAction::Cut,
            Key::Character(c) if c.eq_ignore_ascii_case("v") => KeyAction::Paste,
            Key::Character(c) if c.eq_ignore_ascii_case("z") && modifiers.shift() => {
                KeyAction::Redo
            }
            Key::Character(c) if c.eq_ignore_ascii_case("z") => KeyAction::Undo,
            Key::Character(c) if c.eq_ignore_ascii_case("y") => KeyAction::Redo,
            _ => KeyAction::Ignore,
        };
    }
    match key {
        Key::Named(Named::ArrowLeft) => KeyAction::Previous,
        Key::Named(Named::ArrowRight) => KeyAction::Next,
        Key::Named(Named::Home) => KeyAction::Home,
        Key::Named(Named::End) => KeyAction::End,
        Key::Named(Named::Backspace) => KeyAction::Backspace,
        Key::Named(Named::Delete) => KeyAction::Delete,
        Key::Named(Named::Enter) => KeyAction::Newline,
        _ if text
            .is_some_and(|value| !value.is_empty() && !value.chars().all(char::is_control)) =>
        {
            KeyAction::Insert
        }
        _ => KeyAction::Ignore,
    }
}

fn shape_text(text: &str, width: f32) -> ShapingBuffer {
    FONT_SYSTEM.with(|font_system| {
        let mut font_system = font_system.borrow_mut();
        let mut buffer = ShapingBuffer::new(&mut font_system, Metrics::new(FONT_SIZE, LINE_HEIGHT));
        buffer.set_size(&mut font_system, Some(width), None);
        buffer.set_text(
            &mut font_system,
            text,
            &Attrs::new().family(cosmic_text::Family::Monospace),
            Shaping::Advanced,
            None,
        );
        buffer.shape_until_scroll(&mut font_system, true);
        buffer
    })
}

fn hit_test(text: &str, x: f32, y: f32, width: f32) -> usize {
    let buffer = shape_text(text, width);
    let Some(cursor) = buffer.hit(x.max(0.0), y.max(0.0)) else {
        return text.len();
    };
    let line_start = line_start_byte(text, cursor.line);
    clamp_char_boundary(text, line_start + cursor.index).min(text.len())
}

fn line_start_byte(text: &str, line: usize) -> usize {
    text.split_inclusive('\n').take(line).map(str::len).sum()
}

fn caret_position(runs: &[cosmic_text::LayoutRun<'_>], text: &str, byte: usize) -> (f32, f32) {
    for run in runs {
        let line_start = line_start_byte(text, run.line_i);
        let line_end = line_start + run.text.len();
        if byte < line_start || byte > line_end {
            continue;
        }
        for glyph in run.glyphs {
            let glyph_start = line_start + glyph.start;
            let glyph_end = line_start + glyph.end;
            if byte <= glyph_start {
                return (glyph.x, run.line_y - run.line_top);
            }
            if byte < glyph_end {
                return (glyph.x, run.line_y - run.line_top);
            }
        }
        if byte <= line_end {
            let x = run.glyphs.last().map_or(0.0, |glyph| glyph.x + glyph.w);
            return (x, run.line_y - run.line_top);
        }
    }
    (0.0, runs.last().map_or(0.0, |run| run.line_y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keymap_respects_platform_command_modifiers_and_text() {
        assert_eq!(
            key_action(&Key::Character("z".into()), Modifiers::CTRL, None),
            KeyAction::Undo
        );
        assert_eq!(
            key_action(&Key::Named(Named::Enter), Modifiers::empty(), None),
            KeyAction::Newline
        );
        assert_eq!(
            key_action(&Key::Character("e".into()), Modifiers::empty(), Some("é")),
            KeyAction::Insert
        );
    }

    #[test]
    fn cosmic_text_hit_test_returns_utf8_boundary() {
        let text = "a🧑🏽‍💻e\u{301}";
        let byte = hit_test(text, 45.0, 4.0, 200.0);
        assert!(text.is_char_boundary(byte));
    }

    #[test]
    fn caret_moves_by_grapheme_clusters() {
        let text = "a🧑🏽‍💻e\u{301}";
        let end = text.len();
        assert_eq!(previous_grapheme_boundary(text, end), "a🧑🏽‍💻".len());
        assert_eq!(next_grapheme_boundary(text, 1), "a🧑🏽‍💻".len());
    }
}
