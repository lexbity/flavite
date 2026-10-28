//! Custom iced editor widget over a revisioned, immutable API view.

use crate::keymap::KeyAction;
use crate::surface::{EditorAction, EditorSurface, EditorView, LinkTarget};
use crate::viewport::{
    Viewport, clamp_char_boundary, next_grapheme_boundary, previous_grapheme_boundary,
};
use cosmic_text::{Attrs, Buffer as ShapingBuffer, FontSystem, Metrics, Shaping};
use flavite_api::BufferView;
use flavite_syntax::TextByte;
use flavite_syntax::pulldown_glue::Link;
use flavite_syntax::text::Span;
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
    on_open_link: Option<Box<dyn Fn(String) -> Message + 'a>>,
    link_targets: Vec<LinkTarget>,
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
            on_open_link: None,
            link_targets: Vec::new(),
            width: Length::Fill,
            height: Length::Fill,
            _message: PhantomData,
        }
    }

    /// Route ctrl/cmd-click on a recognized wikilink to the host's note-open command.
    #[must_use]
    pub fn on_open_link(mut self, callback: impl Fn(String) -> Message + 'a) -> Self {
        self.on_open_link = Some(Box::new(callback));
        self
    }

    /// Provide indexed note candidates for wikilink autocomplete.
    #[must_use]
    pub fn with_link_targets(mut self, targets: Vec<LinkTarget>) -> Self {
        self.link_targets = targets;
        self
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
                    KeyAction::Home => line_bounds(&self.view.text, state.caret).0,
                    KeyAction::End => line_bounds(&self.view.text, state.caret).1,
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
        if emitted {
            state.ensure_caret_visible(&self.view.text);
        }
        emitted
    }
}

/// Widget-local interaction state. Text and mutations remain Session-owned.
#[derive(Debug, Default)]
pub struct EditorState {
    buffer_id: Option<u64>,
    caret: usize,
    anchor: usize,
    focused: bool,
    preserve_focus_on_reconcile: bool,
    selecting: bool,
    viewport: Viewport,
    links: Vec<Link>,
    tags: Vec<Span<TextByte>>,
    modifiers: Modifiers,
    selected_completion: usize,
    parsed_revision: Option<u64>,
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

    fn reconcile(&mut self, view: &EditorView) {
        if self
            .buffer_id
            .is_some_and(|buffer_id| buffer_id != view.buffer_id)
        {
            let focused = self.preserve_focus_on_reconcile && self.focused;
            *self = Self::default();
            self.focused = focused;
        }
        self.buffer_id = Some(view.buffer_id);
        if self.parsed_revision != Some(view.revision) {
            if let Ok(parsed) = flavite_syntax::ts_ranges::parse_note_ts(view.text.as_bytes()) {
                self.links = parsed.links;
                self.tags = parsed.tags;
            } else {
                self.links.clear();
                self.tags.clear();
            }
            self.parsed_revision = Some(view.revision);
            self.selected_completion = 0;
        }
        self.caret = clamp_char_boundary(&view.text, self.caret);
        self.anchor = clamp_char_boundary(&view.text, self.anchor);
        self.viewport.line_height = LINE_HEIGHT;
        self.viewport.first_line = self
            .viewport
            .first_line
            .min(logical_line_count(&view.text).saturating_sub(1));
        self.ensure_caret_visible(&view.text);
    }

    fn ensure_caret_visible(&mut self, text: &str) {
        let caret = self.caret.min(text.len());
        let line = text[..caret].bytes().filter(|byte| *byte == b'\n').count();
        if line < self.viewport.first_line {
            self.viewport.first_line = line;
        } else if line
            >= self
                .viewport
                .first_line
                .saturating_add(self.viewport.visible_lines.max(1))
        {
            self.viewport.first_line = line + 1 - self.viewport.visible_lines.max(1);
        }
    }

    fn place_caret_from_point(&mut self, text: &str, x: f32, y: f32, width: f32, extend: bool) {
        let byte = hit_test_visible(text, self.viewport, x, y, width);
        self.move_to(byte, extend);
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
        renderer.fill_quad(
            renderer::Quad {
                bounds,
                border: Border::default(),
                ..renderer::Quad::default()
            },
            Background::Color(Color::from_rgb(0.10, 0.11, 0.13)),
        );
        let clip = bounds
            .intersection(viewport)
            .unwrap_or(Rectangle::with_size(Size::ZERO));
        let (visible_text, source_start) = visible_text(&self.view.text, state.viewport);
        let text_layout = shape_text(&visible_text, (bounds.width - PAD_X * 2.0).max(1.0));
        let runs = text_layout.layout_runs().collect::<Vec<_>>();
        let (selection_start, selection_end) = state.selection();
        if selection_start < selection_end {
            for run in &runs {
                let line_start = source_start + line_start_byte(&visible_text, run.line_i);
                for glyph in run.glyphs {
                    let glyph_start = line_start + glyph.start;
                    let glyph_end = line_start + glyph.end;
                    if glyph_end > selection_start && glyph_start < selection_end {
                        renderer.fill_quad(
                            renderer::Quad {
                                bounds: Rectangle::new(
                                    Point::new(
                                        bounds.x + PAD_X + glyph.x,
                                        bounds.y + PAD_Y + run.line_top,
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
        renderer.fill_text(
            text::Text {
                content: visible_text.clone(),
                bounds: Size::new(
                    (bounds.width - PAD_X * 2.0).max(1.0),
                    (bounds.height - PAD_Y * 2.0).max(1.0),
                ),
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
        // Scanner spans are the same semantic links/tags consumed by the
        // indexer. Their visible underline is drawn from glyph geometry, so
        // wrapped and Unicode text remains aligned with the shaped buffer.
        for link in &state.links {
            let color = if link.is_embed {
                Color::from_rgb(0.78, 0.62, 1.0)
            } else {
                Color::from_rgb(0.40, 0.72, 1.0)
            };
            draw_span_underline(
                renderer,
                &runs,
                &visible_text,
                source_start,
                link.span,
                bounds,
                color,
            );
        }
        for span in &state.tags {
            draw_span_underline(
                renderer,
                &runs,
                &visible_text,
                source_start,
                *span,
                bounds,
                Color::from_rgb(0.45, 0.82, 0.62),
            );
        }
        if state.focused
            && state.caret >= source_start
            && state.caret <= source_start + visible_text.len()
        {
            let local_caret = state.caret.saturating_sub(source_start);
            let (x, y) = caret_position(&runs, &visible_text, local_caret);
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle::new(
                        Point::new(bounds.x + PAD_X + x, bounds.y + PAD_Y + y),
                        Size::new(1.5, LINE_HEIGHT),
                    ),
                    border: Border::default(),
                    ..renderer::Quad::default()
                },
                Background::Color(Color::from_rgb(0.45, 0.72, 1.0)),
            );
        }
        if state.focused {
            let completions = link_completions(&self.view.text, state.caret, &self.link_targets);
            if !completions.is_empty() {
                let local_caret = state.caret.saturating_sub(source_start);
                let (caret_x, caret_y) = caret_position(&runs, &visible_text, local_caret);
                let popup_x = (bounds.x + PAD_X + caret_x).min(bounds.x + bounds.width - 260.0);
                let popup_y =
                    (bounds.y + PAD_Y + caret_y + LINE_HEIGHT).min(bounds.y + bounds.height - 24.0);
                let visible = completions.iter().take(5).enumerate();
                for (index, candidate) in visible {
                    let row_y = popup_y + index as f32 * 24.0;
                    let selected = index == state.selected_completion;
                    renderer.fill_quad(
                        renderer::Quad {
                            bounds: Rectangle::new(
                                Point::new(popup_x, row_y),
                                Size::new(250.0, 24.0),
                            ),
                            border: Border::default(),
                            ..renderer::Quad::default()
                        },
                        Background::Color(if selected {
                            Color::from_rgb(0.20, 0.31, 0.45)
                        } else {
                            Color::from_rgb(0.16, 0.18, 0.21)
                        }),
                    );
                    renderer.fill_text(
                        text::Text {
                            content: candidate.title.clone(),
                            bounds: Size::new(242.0, 22.0),
                            size: iced::Pixels(13.0),
                            line_height: text::LineHeight::Absolute(iced::Pixels(18.0)),
                            font: iced::Font::MONOSPACE,
                            align_x: text::Alignment::Left,
                            align_y: iced::alignment::Vertical::Center,
                            shaping: text::Shaping::Advanced,
                            wrapping: text::Wrapping::None,
                        },
                        Point::new(popup_x + 5.0, row_y + 2.0),
                        Color::WHITE,
                        clip,
                    );
                }
            }
        }
    }

    fn tag(&self) -> iced::advanced::widget::tree::Tag {
        iced::advanced::widget::tree::Tag::of::<EditorState>()
    }

    fn state(&self) -> iced::advanced::widget::tree::State {
        iced::advanced::widget::tree::State::new(EditorState::default())
    }

    fn diff(&self, tree: &mut Tree) {
        tree.state
            .downcast_mut::<EditorState>()
            .reconcile(&self.view);
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
            iced::Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) => {
                state.modifiers = *modifiers;
            }
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if let Some(position) = cursor.position_in(layout.bounds()) {
                    let clicked = hit_test_visible(
                        &self.view.text,
                        state.viewport,
                        position.x - layout.bounds().x - PAD_X,
                        position.y - layout.bounds().y - PAD_Y,
                        (layout.bounds().width - PAD_X * 2.0).max(1.0),
                    );
                    if (state.modifiers.command() || state.modifiers.control())
                        && let Some(link) = state
                            .links
                            .iter()
                            .find(|link| link.span.start.0 <= clicked && clicked < link.span.end.0)
                        && let Some(callback) = &self.on_open_link
                    {
                        state.focused = true;
                        state.preserve_focus_on_reconcile = true;
                        shell.publish(callback(link.target_raw.clone()));
                        shell.capture_event();
                        return;
                    }
                    state.focused = true;
                    state.selecting = true;
                    state.place_caret_from_point(
                        &self.view.text,
                        position.x - PAD_X,
                        position.y - PAD_Y,
                        (layout.bounds().width - PAD_X * 2.0).max(1.0),
                        false,
                    );
                    shell.capture_event();
                } else {
                    state.focused = false;
                    state.selecting = false;
                }
            }
            iced::Event::Mouse(mouse::Event::CursorMoved { .. }) if state.selecting => {
                if let Some(position) = cursor.position_in(layout.bounds()) {
                    state.place_caret_from_point(
                        &self.view.text,
                        position.x - PAD_X,
                        position.y - PAD_Y,
                        (layout.bounds().width - PAD_X * 2.0).max(1.0),
                        true,
                    );
                    shell.capture_event();
                }
            }
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                if state.selecting {
                    shell.capture_event();
                }
                state.selecting = false;
            }
            iced::Event::Mouse(mouse::Event::WheelScrolled { delta })
                if cursor.position_in(layout.bounds()).is_some() =>
            {
                let lines = match delta {
                    mouse::ScrollDelta::Lines { y, .. } => *y,
                    mouse::ScrollDelta::Pixels { y, .. } => *y / LINE_HEIGHT,
                };
                let last_line = logical_line_count(&self.view.text).saturating_sub(1);
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
            }) if state.focused => {
                let completions =
                    link_completions(&self.view.text, state.caret, &self.link_targets);
                if !completions.is_empty()
                    && matches!(
                        key,
                        Key::Named(Named::ArrowDown) | Key::Named(Named::ArrowUp)
                    )
                {
                    let len = completions.len();
                    state.selected_completion = if matches!(key, Key::Named(Named::ArrowUp)) {
                        state.selected_completion.checked_sub(1).unwrap_or(len - 1)
                    } else {
                        (state.selected_completion + 1) % len
                    };
                    shell.capture_event();
                } else if !completions.is_empty()
                    && matches!(key, Key::Named(Named::Enter) | Key::Named(Named::Tab))
                {
                    if let Some(target) =
                        completions.get(state.selected_completion.min(completions.len() - 1))
                    {
                        let (start, end) = link_completion_range(&self.view.text, state.caret);
                        self.emit(
                            shell,
                            EditorAction::replace(
                                &self.view,
                                start,
                                end,
                                format!("{}]]", target.target),
                            ),
                        );
                        state.caret = start + target.target.len() + 2;
                        state.anchor = state.caret;
                    }
                    shell.capture_event();
                } else if self.execute_key(
                    state,
                    key,
                    *modifiers,
                    text.as_deref(),
                    clipboard,
                    shell,
                ) {
                    shell.capture_event();
                }
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

/// Shape and hit-test only the logical lines currently in the viewport.
fn hit_test_visible(text: &str, viewport: Viewport, x: f32, y: f32, width: f32) -> usize {
    let (visible, source_start) = visible_text(text, viewport);
    source_start + hit_test(&visible, x, y, width)
}

fn visible_text(text: &str, viewport: Viewport) -> (String, usize) {
    let line_count = logical_line_count(text);
    let start_line = viewport.first_line.min(line_count.saturating_sub(1));
    let end_line = start_line
        .saturating_add(viewport.visible_lines.max(1))
        .min(line_count);
    let start = line_start_byte(text, start_line);
    let end = if end_line == line_count {
        text.len()
    } else {
        line_start_byte(text, end_line)
    };
    (text[start..end].to_owned(), start)
}

fn logical_line_count(text: &str) -> usize {
    text.split('\n').count().max(1)
}

fn line_bounds(text: &str, byte: usize) -> (usize, usize) {
    let byte = clamp_char_boundary(text, byte);
    let start = text[..byte].rfind('\n').map_or(0, |newline| newline + 1);
    let end = text[byte..]
        .find('\n')
        .map_or(text.len(), |newline| byte + newline);
    (start, end)
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
                return (glyph.x, run.line_top);
            }
            if byte < glyph_end {
                return (glyph.x, run.line_top);
            }
        }
        if byte <= line_end {
            let x = run.glyphs.last().map_or(0.0, |glyph| glyph.x + glyph.w);
            return (x, run.line_top);
        }
    }
    (0.0, runs.last().map_or(0.0, |run| run.line_top))
}

fn link_completion_range(text: &str, caret: usize) -> (usize, usize) {
    let caret = clamp_char_boundary(text, caret.min(text.len()));
    let before = &text[..caret];
    let Some(open) = before.rfind("[[") else {
        return (caret, caret);
    };
    if before[open + 2..].contains("]]") {
        return (caret, caret);
    }
    let end = text[caret..]
        .find("]]")
        .map_or(caret, |offset| caret + offset + 2);
    (open + 2, end)
}

fn link_completions(text: &str, caret: usize, targets: &[LinkTarget]) -> Vec<LinkTarget> {
    let (start, end) = link_completion_range(text, caret);
    if start >= end || start == caret {
        return Vec::new();
    }
    let query = text[start..caret.min(end)].trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut matches = targets
        .iter()
        .filter(|target| {
            target.target.to_lowercase().contains(&query)
                || target.title.to_lowercase().contains(&query)
        })
        .cloned()
        .collect::<Vec<_>>();
    matches.sort_by_key(|target| {
        (
            !target.target.to_lowercase().starts_with(&query),
            target.title.to_lowercase(),
        )
    });
    matches.truncate(5);
    matches
}

fn draw_span_underline(
    renderer: &mut iced::Renderer,
    runs: &[cosmic_text::LayoutRun<'_>],
    visible_text: &str,
    source_start: usize,
    span: Span<TextByte>,
    bounds: Rectangle,
    color: Color,
) {
    let start = span.start.0.max(source_start);
    let end = span.end.0.min(source_start + visible_text.len());
    if start >= end {
        return;
    }
    for run in runs {
        let line_start = source_start + line_start_byte(visible_text, run.line_i);
        for glyph in run.glyphs {
            let glyph_start = line_start + glyph.start;
            let glyph_end = line_start + glyph.end;
            if glyph_end > start && glyph_start < end {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: Rectangle::new(
                            Point::new(
                                bounds.x + PAD_X + glyph.x,
                                bounds.y + PAD_Y + run.line_top + LINE_HEIGHT - 2.0,
                            ),
                            Size::new(glyph.w.max(1.0), 1.0),
                        ),
                        border: Border::default(),
                        ..renderer::Quad::default()
                    },
                    Background::Color(color),
                );
            }
        }
    }
}

#[cfg(test)]
mod link_aware_tests {
    use super::*;

    #[test]
    fn autocomplete_matches_open_wikilink_prefix_and_completes_existing_closer() {
        let targets = vec![
            LinkTarget {
                target: "Concepts.md".into(),
                title: "Concepts".into(),
            },
            LinkTarget {
                target: "Inbox.md".into(),
                title: "Inbox".into(),
            },
        ];
        let text = "See [[Con]] now";
        let matches = link_completions(text, 9, &targets);
        assert_eq!(matches[0].title, "Concepts");
        assert_eq!(link_completion_range(text, 9), (6, 11));
    }

    #[test]
    fn autocomplete_is_inactive_outside_an_open_wikilink() {
        let targets = vec![LinkTarget {
            target: "Concepts.md".into(),
            title: "Concepts".into(),
        }];
        assert!(link_completions("ordinary Con text", 10, &targets).is_empty());
        assert!(link_completions("[[Concepts]]", 12, &targets).is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flavite_syntax::text::{Span, TextByte};

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
    fn cosmic_text_layout_contains_the_editor_text() {
        let text = "Heading\n\nbody\n\n- item\n- final visual editor input";
        let shaped = shape_text(text, 400.0);
        let runs = shaped.layout_runs().collect::<Vec<_>>();
        assert!(!runs.is_empty());
        assert!(runs.iter().any(|run| run.text == "Heading"));
        let (_, y) = caret_position(&runs, text, text.len());
        assert_eq!(y, runs.last().expect("non-empty layout").line_top);
    }

    #[test]
    fn cosmic_text_layout_wraps_long_lines() {
        let shaped = shape_text("abcdefghij", 24.0);
        assert!(shaped.layout_runs().count() > 1);
    }

    #[test]
    fn caret_moves_by_grapheme_clusters() {
        let text = "a🧑🏽‍💻e\u{301}";
        let end = text.len();
        assert_eq!(previous_grapheme_boundary(text, end), "a🧑🏽‍💻".len());
        assert_eq!(next_grapheme_boundary(text, 1), "a🧑🏽‍💻".len());
    }

    fn test_view(text: &str, buffer_id: u64) -> BufferView {
        BufferView {
            buffer_id,
            revision: 11,
            text: text.to_owned(),
            base_hash: Vec::new(),
            state: "Clean".into(),
            path: std::path::PathBuf::from("note.md"),
        }
    }

    #[test]
    fn headless_key_dispatch_emits_one_revision_guarded_edit() {
        let view = test_view("hello", 91);
        let widget = EditorWidget::new(&view, |action| action);
        let mut state = EditorWidget::<EditorAction>::state_for_test();
        state.focused = true;
        state.caret = 2;
        state.anchor = 2;
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);

        assert!(widget.execute_key(
            &mut state,
            &Key::Character("x".into()),
            Modifiers::empty(),
            Some("é"),
            &mut clipboard,
            &mut shell,
        ));
        drop(shell);

        assert_eq!(state.caret(), 4);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].buffer_id, 91);
        assert_eq!(messages[0].expected_revision, 11);
        assert_eq!(
            messages[0].edits[0].range,
            Span::new(TextByte(2), TextByte(2))
        );
        assert_eq!(messages[0].edits[0].replacement, "é");
    }

    #[test]
    fn consecutive_input_before_view_refresh_keeps_optimistic_caret() {
        let view = test_view("x", 92);
        let widget = EditorWidget::new(&view, |action| action);
        let mut state = EditorWidget::<EditorAction>::state_for_test();
        state.focused = true;
        state.caret = 1;
        state.anchor = 1;
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);

        for (key, text) in [("a", "a"), ("b", "b")] {
            widget.execute_key(
                &mut state,
                &Key::Character(key.into()),
                Modifiers::empty(),
                Some(text),
                &mut clipboard,
                &mut shell,
            );
        }
        drop(shell);

        assert_eq!(messages.len(), 2);
        assert_eq!(
            messages[0].edits[0].range,
            Span::new(TextByte(1), TextByte(1))
        );
        assert_eq!(
            messages[1].edits[0].range,
            Span::new(TextByte(2), TextByte(2))
        );
        assert_eq!(state.caret(), 3);
        assert_eq!(state.selection(), (3, 3));
    }

    #[test]
    fn headless_selection_and_history_use_session_actions() {
        let view = test_view("one\ntwo", 18);
        let widget = EditorWidget::new(&view, |action| action);
        let mut state = EditorWidget::<EditorAction>::state_for_test();
        state.focused = true;
        let mut clipboard = iced::advanced::clipboard::Null;
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);

        widget.execute_key(
            &mut state,
            &Key::Character("a".into()),
            Modifiers::CTRL,
            None,
            &mut clipboard,
            &mut shell,
        );
        drop(shell);
        assert_eq!(state.selection(), (0, view.text.len()));

        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        widget.execute_key(
            &mut state,
            &Key::Character("z".into()),
            Modifiers::CTRL,
            None,
            &mut clipboard,
            &mut shell,
        );
        drop(shell);
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].history, Some(false));
        assert_eq!(messages[0].buffer_id, 18);
    }

    #[test]
    fn viewport_shapes_only_requested_source_lines_and_maps_offsets() {
        let source = "zero\none\ntwo\nthree\nfour";
        let viewport = Viewport {
            first_line: 2,
            visible_lines: 2,
            ..Viewport::default()
        };
        let (visible, source_start) = visible_text(source, viewport);
        assert_eq!(visible, "two\nthree\n");
        assert_eq!(source_start, "zero\none\n".len());
        assert_eq!(
            hit_test_visible(source, viewport, 0.0, 1.0, 300.0),
            source_start
        );
    }

    #[test]
    fn pointer_hit_test_places_and_extends_selection_at_glyph_boundary() {
        let text = "abcdef";
        let shaped = shape_text(text, 300.0);
        let first_run = shaped.layout_runs().next().expect("one shaped line");
        let target = &first_run.glyphs[3];
        let expected_byte = target.start;
        let mut state = EditorState {
            viewport: Viewport {
                visible_lines: 1,
                ..Viewport::default()
            },
            ..EditorState::default()
        };

        state.place_caret_from_point(text, target.x + 0.1, 1.0, 300.0, false);
        assert_eq!(state.caret(), expected_byte);
        assert_eq!(state.selection(), (expected_byte, expected_byte));
        state.place_caret_from_point(text, first_run.glyphs[1].x + 0.1, 1.0, 300.0, true);
        assert_eq!(
            state.selection(),
            (first_run.glyphs[1].start, expected_byte)
        );
    }

    #[test]
    fn widget_state_resets_on_reopen_and_clamps_after_reload() {
        let old = EditorView {
            buffer_id: 1,
            revision: 2,
            text: "long text".into(),
            path: std::path::PathBuf::from("one.md"),
            state: "Dirty".into(),
        };
        let mut state = EditorState {
            buffer_id: Some(old.buffer_id),
            caret: 8,
            anchor: 4,
            ..EditorState::default()
        };

        let shorter_same_buffer = EditorView {
            revision: 3,
            text: "x".into(),
            ..old.clone()
        };
        state.reconcile(&shorter_same_buffer);
        assert_eq!(state.selection(), (1, 1));

        let reopened = EditorView {
            buffer_id: 2,
            text: "new".into(),
            ..shorter_same_buffer
        };
        state.focused = true;
        state.reconcile(&reopened);
        assert_eq!(state.caret(), 0);
        assert_eq!(state.selection(), (0, 0));
        assert!(!state.focused);
    }

    #[test]
    fn home_and_end_are_line_local() {
        let text = "first\nsecond\nthird";
        assert_eq!(line_bounds(text, 8), (6, 12));
    }

    #[test]
    fn caret_motion_scrolls_the_viewport_to_keep_the_caret_visible() {
        let mut state = EditorState {
            caret: 0,
            viewport: Viewport {
                visible_lines: 3,
                ..Viewport::default()
            },
            ..EditorState::default()
        };
        state.caret = "one\ntwo\nthree\nfour\nfive".len();
        state.ensure_caret_visible("one\ntwo\nthree\nfour\nfive");
        assert_eq!(state.viewport.first_line, 2);

        state.caret = 0;
        state.ensure_caret_visible("one\ntwo\nthree\nfour\nfive");
        assert_eq!(state.viewport.first_line, 0);
    }
}
